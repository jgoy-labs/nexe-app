// Regression guard for the production Content-Security-Policy (I-002).
//
// `tauri.conf.json` `app.security.csp` is the CSP applied to PRODUCTION builds.
// `'unsafe-eval'` had been added "for Vite HMR" (commit 28750f2) but Tauri only
// applies this config CSP to production assets — in dev the app loads from the
// Vite dev server. No frontend code (src/) nor any production dependency
// (@tauri-apps/api, @tauri-apps/plugin-dialog) uses eval()/new Function(), and
// the built dist/ bundle contains zero eval call-sites. So `'unsafe-eval'` only
// weakened production with no runtime benefit.
//
// This test fails with the pre-fix config (which contained `'unsafe-eval'`) and
// passes with the fix. If anyone re-introduces `'unsafe-eval'`, CI catches it
// and forces an explicit justification.

import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

const conf = JSON.parse(
  readFileSync(resolve(process.cwd(), "src-tauri/tauri.conf.json"), "utf8"),
);
const csp = conf.app.security.csp;

// Extract a single CSP directive's value (the tokens after its name, up to ';').
// Lets a guard target `script-src` without tripping on `style-src`'s own tokens.
function directive(name) {
  const seg = csp
    .split(";")
    .map((s) => s.trim())
    .find((d) => d === name || d.startsWith(name + " "));
  return seg ? seg.slice(name.length).trim() : null;
}

describe("production CSP (tauri.conf.json)", () => {
  it("does NOT allow 'unsafe-eval' in script-src", () => {
    expect(csp).not.toContain("unsafe-eval");
  });

  it("keeps the strict baseline directives", () => {
    expect(csp).toContain("default-src 'self'");
    expect(csp).toContain("object-src 'none'");
    expect(csp).toContain("frame-ancestors 'none'");
    expect(csp).toContain("base-uri 'self'");
  });
});

describe("CSP 15/06 tripwire — do not repeat the reverted hardening (B236/B245)", () => {
  // On 2026-06-15 (commit 526e666) `'unsafe-inline'` was removed from script-src
  // while `dangerousDisableAssetCspModification` was true. With asset CSP
  // modification disabled Tauri cannot inject nonces/hashes for its IPC bootstrap
  // scripts, and the loopback web UI carries an inline <script>, so the webview
  // broke and it was reverted 24 min later (a7ac749). The OLD test was a static
  // string check that passed green on that broken config — pure test-theatre.
  // This guard encodes the load-bearing invariant so a naive removal fails CI
  // instead of shipping a blank webview. Real hardening (flip the flag + nonces,
  // or externalise every inline across both CSP layers) is a separate session
  // and must add a render-time gate; until then, removing 'unsafe-inline' here
  // alone is a known footgun.
  it("requires 'unsafe-inline' (or nonces/hashes) in script-src while asset CSP modification is disabled", () => {
    if (conf.app.security.dangerousDisableAssetCspModification === true) {
      const scriptSrc = directive("script-src");
      expect(scriptSrc, "script-src directive must be present").not.toBeNull();
      const allowsInlineScripts =
        scriptSrc.includes("'unsafe-inline'") ||
        scriptSrc.includes("'nonce-") ||
        /'sha(256|384|512)-/.test(scriptSrc);
      expect(
        allowsInlineScripts,
        "with dangerousDisableAssetCspModification=true, script-src must keep " +
          "'unsafe-inline' (or move to nonces/hashes by flipping the flag). " +
          "Removing it without changing the architecture broke the webview on " +
          "2026-06-15 (526e666 -> reverted a7ac749). See B236/B245 + ADR-0008.",
      ).toBe(true);
    }
  });
});
