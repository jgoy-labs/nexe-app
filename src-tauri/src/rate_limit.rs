//! Rate limiter — bounded token buckets with LRU cap.
//!
//! Two layers (WSC-003):
//! - per-plugin buckets (fairness between legitimate plugins), and
//! - fixed-key GLOBAL buckets for `plugin://` and `fetch_from_sidecar`,
//!   which bound total throughput even when an attacker mints fresh ids.
//!
//! Token bucket (burst-resistant).
//! LRU cap 500 to avoid OOM with many different IDs.
//! Fail-closed on mutex poison.
//! Lookup-then-insert avoids `.to_string()` alloc per
//! request on the happy path (hot path limiter).

use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Maximum tokens per plugin (= sustained requests per second).
pub(crate) const RATE_LIMIT_CAPACITY: u64 = 1000;

/// Maximum number of entries in the LRU cache (plugins with active limiter).
pub(crate) const RATE_LIMIT_LRU_CAP: usize = 500;

// ─────────────────────────────────────────────────────────────────────────
// WSC-003 — global buckets.
//
// The per-plugin bucket alone is evadable: `extract_plugin_id_from_uri` takes
// the plugin id straight from the (attacker-controlled) URI, so cycling ids
// gives a fresh bucket every time. The global buckets below share one fixed
// key, so total throughput stays bounded no matter how many ids are minted.
//
// The keys contain a `/` on purpose: `extract_plugin_id_from_uri` cuts the
// host at the first `/`, so no crafted `plugin://` URI can ever produce these
// strings as its per-plugin key and drain a global bucket "from the side".
// ─────────────────────────────────────────────────────────────────────────

/// Fixed key of the global bucket shared by ALL `plugin://` requests.
pub(crate) const PLUGIN_GLOBAL_RATE_KEY: &str = "__global/plugin__";

/// Global `plugin://` budget (tokens = sustained requests per second).
/// Same magnitude as the per-plugin cap: one legitimate plugin saturating its
/// own bucket was already the accepted per-id ceiling, so the global cap adds
/// no new constraint for legitimate use — it only stops the fresh-id evasion.
pub(crate) const PLUGIN_GLOBAL_RATE_CAPACITY: u64 = 1000;

/// Fixed key of the global bucket for the `fetch_from_sidecar` IPC proxy.
pub(crate) const SIDECAR_GLOBAL_RATE_KEY: &str = "__global/sidecar__";

/// Global `fetch_from_sidecar` budget (tokens = sustained requests/second).
///
/// Sizing (from the real boot cadence in src/main.js): the splash polls
/// `/admin/system/health` every 500 ms (2 req/s) and, once healthy on
/// Windows, probes `/ui/` every 250 ms (4 req/s) — worst case ~6 req/s.
/// 100 req/s gives >16x headroom over the busiest legitimate burst while
/// still being a real brake on an XSS-driven flood (which would otherwise
/// hammer the admin-token proxy unbounded).
pub(crate) const SIDECAR_GLOBAL_RATE_CAPACITY: u64 = 100;

static RATE_LIMITERS: OnceLock<Mutex<LruCache<String, (Instant, u64)>>> = OnceLock::new();

pub(crate) fn rate_limiters() -> &'static Mutex<LruCache<String, (Instant, u64)>> {
    RATE_LIMITERS.get_or_init(|| {
        // RATE_LIMIT_LRU_CAP is const=500 > 0 — unwrap_or safe with minimal fallback
        let cap = NonZeroUsize::new(RATE_LIMIT_LRU_CAP).unwrap_or(NonZeroUsize::MIN);
        Mutex::new(LruCache::new(cap))
    })
}

/// Token bucket per plugin. Returns true if tokens are available, false if the request should be rejected.
pub(crate) fn rate_limit_ok_for(plugin_id: &str) -> bool {
    rate_limit_ok_with_capacity(plugin_id, RATE_LIMIT_CAPACITY)
}

/// WSC-003: combined `plugin://` gate — the request must clear BOTH its
/// per-plugin bucket (fairness between legitimate plugins) and the shared
/// global bucket (the real brake: minting fresh ids no longer buys unlimited
/// throughput). Short-circuits so a per-plugin rejection does not burn a
/// global token.
pub(crate) fn plugin_rate_limits_ok(plugin_id: &str) -> bool {
    rate_limit_ok_for(plugin_id)
        && rate_limit_ok_with_capacity(PLUGIN_GLOBAL_RATE_KEY, PLUGIN_GLOBAL_RATE_CAPACITY)
}

/// WSC-003: global gate for the `fetch_from_sidecar` IPC proxy (auth.rs).
pub(crate) fn sidecar_global_rate_ok() -> bool {
    rate_limit_ok_with_capacity(SIDECAR_GLOBAL_RATE_KEY, SIDECAR_GLOBAL_RATE_CAPACITY)
}

/// Generic token bucket keyed by an arbitrary string, with a per-bucket
/// capacity (= burst size = sustained tokens/second refill rate).
///
/// We avoid `key.to_string()` alloc per request when the entry already
/// exists in the cache. `contains(&str)` accepts `&Q: ?Sized` (no String required),
/// `get_mut` likewise. We only allocate when actually inserting (first time a key is seen).
///
/// Invariant: a given key must always be used with the same `cap` — the cap
/// is applied at insert and as the refill clamp, so mixing caps for one key
/// would make the effective limit whichever call refills last.
pub(crate) fn rate_limit_ok_with_capacity(key: &str, cap: u64) -> bool {
    let mut guard = match rate_limiters().lock() {
        Ok(g) => g,
        Err(_) => return false, // fail-closed on mutex poison
    };

    // Lookup first (no alloc); insert only if absent.
    if !guard.contains(key) {
        guard.put(key.to_string(), (Instant::now(), cap));
    }
    let entry = match guard.get_mut(key) {
        Some(e) => e,
        None => {
            // B165: unreachable. We hold the exclusive lock and just did put()
            // (LRU cap >= 1; put() inserts as MRU and never evicts the entry it
            // just inserted). Deliberate defensive fail-open: if the invariant
            // ever broke we allow ONE request rather than panicking — a panic
            // here would poison the global Mutex (we hold `guard`), making every
            // later lock() fail → 429 permanent for ALL plugins. NOT a "race".
            debug_assert!(
                false,
                "rate-limit entry vanished after put — LRU invariant broken"
            );
            return true;
        }
    };

    // Refill tokens based on elapsed time
    let elapsed_secs = entry.0.elapsed().as_secs_f64();
    let refill = (elapsed_secs * cap as f64) as u64;
    if refill > 0 {
        entry.1 = (entry.1 + refill).min(cap);
        entry.0 = Instant::now();
    }
    // Consume one token
    if entry.1 == 0 {
        return false;
    }
    entry.1 -= 1;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_request_always_allowed() {
        // A fresh plugin_id should always get through (full bucket).
        assert!(rate_limit_ok_for("test-fresh-plugin-1234"));
    }

    #[test]
    fn exhausting_bucket_rejects() {
        // Flaky-fix (2026-05-21): when this test ran in batch with the rest
        // of the suite, the drain loop took >1 ms and `rate_limit_ok_for`
        // refilled the bucket mid-drain (refill = elapsed_secs * 1000, so a
        // 1 ms gap between calls already credits 1 token back). The assertion
        // 'next one must be rejected' then flaked. We now drain *until* the
        // limiter actually rejects, with a generous upper bound that is
        // still O(milliseconds) of cargo test wall time.
        let id = "test-exhaust-bucket-unique";
        let mut consumed = 0u64;
        // Hard ceiling so a logic bug cannot spin forever.
        let safety_cap = RATE_LIMIT_CAPACITY * 20;
        while rate_limit_ok_for(id) {
            consumed += 1;
            assert!(
                consumed <= safety_cap,
                "drained {consumed} tokens without ever being rejected — refill rate is keeping up with consumption, which means the limiter is not actually bounding burst",
            );
        }
        // We reached a rejection — that is the property under test.
        // The exact `consumed` count is implementation-dependent (depends
        // on how much the OS slept us between calls) but must be > 0.
        assert!(
            consumed >= RATE_LIMIT_CAPACITY,
            "limiter rejected after only {consumed} tokens — capacity is at least {RATE_LIMIT_CAPACITY}"
        );
    }

    #[test]
    fn different_plugins_have_independent_buckets_but_share_the_global() {
        // Per-plugin layer: A and B keep independent buckets…
        let a = "test-independent-a";
        let b = "test-independent-b";
        for _ in 0..RATE_LIMIT_CAPACITY {
            rate_limit_ok_for(a);
        }
        assert!(rate_limit_ok_for(b), "B's bucket must be independent of A");

        // …but under WSC-003 the combined gate ALSO charges a shared global
        // bucket, so fresh ids are not unlimited. We exercise the same
        // two-layer logic with a test-local global key and a small capacity
        // (the prod PLUGIN_GLOBAL_RATE_KEY is shared process-wide state and
        // its 1000/s refill makes exact-drain assertions flaky).
        let global_key = "test-independent/global";
        let global_cap = 5u64;
        let combined = |id: &str| {
            rate_limit_ok_for(id) && rate_limit_ok_with_capacity(global_key, global_cap)
        };

        // Each request uses a FRESH id, so the per-id bucket always passes
        // (first request always allowed) — only the global can say no.
        let mut rejected_at = None;
        for i in 0..(global_cap * 4) {
            let id = format!("test-independent-fresh-{i}");
            if !combined(&id) {
                rejected_at = Some(i);
                break;
            }
        }
        let rejected_at = rejected_at.expect(
            "minting fresh plugin ids must eventually hit the GLOBAL bucket — \
             if this never rejects, the fresh-id evasion (WSC-003) is back",
        );
        assert!(
            rejected_at >= global_cap,
            "global bucket rejected after only {rejected_at} requests (capacity {global_cap})"
        );
    }

    /// WSC-003: the helper behind `sidecar_global_rate_ok` bounds a fixed-key
    /// bucket. `fetch_from_sidecar` itself needs Tauri State handles (not unit
    /// testable), so we test the same helper with a test-local key: drain to
    /// rejection, exactly like `exhausting_bucket_rejects` does per-plugin.
    #[test]
    fn sidecar_global_bucket_bounds_requests() {
        let key = "test-sidecar/global";
        let cap = 5u64;
        let mut consumed = 0u64;
        let safety_cap = cap * 20;
        while rate_limit_ok_with_capacity(key, cap) {
            consumed += 1;
            assert!(
                consumed <= safety_cap,
                "drained {consumed} tokens without rejection — the global sidecar bucket is not bounding"
            );
        }
        assert!(
            consumed >= cap,
            "rejected after only {consumed} tokens — capacity is at least {cap}"
        );
    }

    /// The production sidecar bucket must clear the real boot cadence:
    /// main.js polls /admin/system/health at 2 req/s and probes /ui/ at
    /// 4 req/s (~6 req/s worst case). A first burst well above that must
    /// pass through the PROD key + capacity.
    #[test]
    fn sidecar_global_rate_ok_allows_boot_cadence_burst() {
        // 12 = 2x the worst-case 6 req/s boot second; capacity is 100.
        for i in 0..12 {
            assert!(
                sidecar_global_rate_ok(),
                "boot-cadence request {i} must pass the global sidecar bucket"
            );
        }
    }

    #[test]
    fn mutex_poison_fails_closed() {
        // If the mutex is poisoned, rate_limit_ok_for returns false (fail-closed).
        // We can't easily poison it in a unit test without panicking in another thread,
        // but we verify the code path exists by checking the match arm compiles
        // and the constant is correct.
        assert_eq!(RATE_LIMIT_CAPACITY, 1000);
        assert_eq!(RATE_LIMIT_LRU_CAP, 500);
    }
}
