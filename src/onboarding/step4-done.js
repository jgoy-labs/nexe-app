// Step 4 — Download complete confirmation.

import { goToStep, state } from "./main.js";
import { t } from "./i18n.js";

export function step4() {
  const app = document.getElementById("onboarding-app");
  app.replaceChildren();

  const wrapper = document.createElement("div");
  wrapper.className = "step step4";

  const icon = document.createElement("div");
  icon.className = "success-icon";
  icon.textContent = "✓";
  wrapper.appendChild(icon);

  const title = document.createElement("h2");
  title.textContent = t("step4_title", state.lang);
  wrapper.appendChild(title);

  const body = document.createElement("p");
  body.textContent = t("step4_body", state.lang);
  wrapper.appendChild(body);

  const trayHint = document.createElement("p");
  trayHint.className = "step4-tray-hint";
  trayHint.textContent = t("step4_tray_hint", state.lang);
  wrapper.appendChild(trayHint);

  // INST-002-FE: recap any non-blocking install warning (e.g. a model installed
  // without a SHA256 pin) here, where the user actually sees it — step3
  // auto-advances too fast for the inline notice alone to register.
  if (state.shaWarnings && state.shaWarnings.length > 0) {
    const banner = document.createElement("div");
    banner.className = "sha-warning-banner";
    const warn = document.createElement("p");
    warn.textContent = "⚠ " + t("step4_sha_warning", state.lang);
    banner.appendChild(warn);
    wrapper.appendChild(banner);
  }

  const btn = document.createElement("button");
  btn.className = "btn-primary";
  btn.textContent = t("btn_next", state.lang);
  btn.addEventListener("click", () => goToStep(5));
  wrapper.appendChild(btn);

  app.appendChild(wrapper);
}
