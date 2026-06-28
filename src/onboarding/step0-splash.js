// Step 0 — Splash / extraction progress.
//
// Shown while the sidecar bundle is being extracted on first launch.
// MC-059: NO Rust code emits "extract-progress" today — extraction
// (ensure_sidecar_extracted) runs synchronously in setup_services before the
// webview loads, so it cannot report progress to the listener below with the
// current architecture. The listener is forward-compat scaffolding; in practice
// the splash advances on the 600 ms quick timer (indeterminate bar until then).

import { listen } from "@tauri-apps/api/event";
import { goToStep, state } from "./main.js";
import { t } from "./i18n.js";

export async function step0() {
  const app = document.getElementById("onboarding-app");
  app.replaceChildren();

  const wrapper = document.createElement("div");
  wrapper.className = "step step0";

  const msg = document.createElement("p");
  msg.className = "splash-msg";
  msg.textContent = t("step0_extracting", state.lang);
  wrapper.appendChild(msg);

  const bar = document.createElement("progress");
  bar.className = "splash-bar";
  bar.max = 100;
  // MC-059: no initial value → indeterminate animated bar (nothing emits
  // 'extract-progress' today; the listener below sets bar.value only if a real
  // emitter is ever wired, turning it determinate).
  wrapper.appendChild(bar);

  app.appendChild(wrapper);

  let received = false;
  let settled = false;
  let quickTimer;
  let absoluteTimer;

  // Single transition out of the splash: unlisten, clear both timers, advance.
  // Idempotent so the progress event, the quick timer and the absolute ceiling
  // can never double-fire goToStep or leave the listener orphaned (MC-038).
  const settle = () => {
    if (settled) return;
    settled = true;
    clearTimeout(quickTimer);
    clearTimeout(absoluteTimer);
    unlisten();
    goToStep(1);
  };

  const unlisten = await listen("extract-progress", (event) => {
    received = true;
    const pct = event.payload?.percent ?? 0;
    const stage = event.payload?.stage;
    bar.value = pct;
    if (stage) msg.textContent = stage;
    if (pct >= 100) settle();
  });

  // If the bundle was already extracted no event will arrive — skip after 600 ms.
  quickTimer = setTimeout(() => {
    if (!received) settle();
  }, 600);

  // MC-038: absolute ceiling INDEPENDENT of `received`. If progress events start
  // but stall below 100% (extraction hung/crashed), the 600 ms quick timer never
  // fires (received === true) and the old code left the splash and the listener
  // hanging forever. This hard cap always advances so the user is never stuck.
  absoluteTimer = setTimeout(() => settle(), 120000);
}
