// Step 5 — finalize + restart sidecar + navigate to chat UI.
//
// Flow:
//   1. POST /installer/finalize with {engine, model_id} so the sidecar
//      persists the onboarding state to disk (engine + model + path).
//   2. invoke("restart_sidecar") — Rust kills the running sidecar, spawns a
//      new one on a fresh ephemeral port, polls /admin/system/health until OK,
//      emits "sidecar-restarted" and returns the new port.
//   3. invoke("mark_onboarding_complete") — Rust writes the first-run flag so
//      this wizard does not show on the next launch.
//   4. window.location.replace("http://127.0.0.1:<newPort>/?nexe_api_key=K")
//      — replace (not href) so the Back button cannot return to the dead old
//      port. After the revert, navigates directly to the sidecar HTTP origin,
//      passing the api_key as a query param (cross-origin handoff — see
//      app.js constructor that reads nexe_api_key from URLSearchParams).
//
// On Advanced mode the API key is shown to the user before the Start button.

import { invoke } from "@tauri-apps/api/core";
import { state, saveState } from "./main.js";
import { t } from "./i18n.js";

export async function step5() {
  const app = document.getElementById("onboarding-app");
  app.replaceChildren();

  const wrapper = document.createElement("div");
  wrapper.className = "step step5";

  const title = document.createElement("h2");
  title.textContent = t("step5_title", state.lang);
  wrapper.appendChild(title);

  // Show API key only in Advanced mode (rendered after we obtain it).
  const advancedContainer = document.createElement("div");
  advancedContainer.className = "step5-advanced";
  wrapper.appendChild(advancedContainer);

  // Low-RAM reminder shown right before the user starts server-nexe, while
  // the machine still has memory headroom to close other apps.
  if (state.hardware?.ram_gb && state.hardware.ram_gb < 12) {
    const warn = document.createElement("p");
    warn.className = "step5-low-ram-warning";
    warn.textContent = t("step5_low_ram_warning", state.lang);
    wrapper.appendChild(warn);
  }

  // Start button — disabled until we have the API key from finalize.
  const startBtn = document.createElement("button");
  startBtn.className = "btn-primary";
  startBtn.textContent = t("btn_start_nexe", state.lang);
  startBtn.disabled = true;
  wrapper.appendChild(startBtn);

  // Status text (replaced as the flow progresses).
  const statusEl = document.createElement("p");
  statusEl.className = "step5-status";
  statusEl.textContent = t("step5_finalizing", state.lang);
  wrapper.appendChild(statusEl);

  app.appendChild(wrapper);

  // ── Phase A — finalize on the CURRENT sidecar port ────────────────────────
  //
  // Always POST /installer/finalize so the backend persists OnboardingState
  // and the next sidecar restart leaves minimal mode (web_ui_module gets
  // initialised). Engine paths:
  //   - "mlx"|"ollama"|"gguf": catalog model downloaded in step 3.
  //   - "local": user pointed at a local models folder; model_id carries the
  //     folder path. The backend sets NEXE_STORAGE_PATH and the chat UI
  //     selector picks the concrete model at runtime.
  // (Pre-2026-05-29 the local-folder path did a GET that never persisted state
  //  and skipped the restart, leaving the sidecar in minimal mode → the UI
  //  crashed with "SessionManager accessed before WebUIModule.initialize()".)
  const port = await invoke("get_sidecar_port");
  const engine = state.selectedModel?.engine;
  try {
    // Pass the optional HF token if present (stored in the macOS
    // Keychain; never persisted client-side). Propagate the wizard language so
    // the sidecar serves the UI in the right locale at next restart.
    const body = {
      engine,
      model_id: state.selectedModel?.model_id,
      lang: state.lang,
    };
    if (state.hfToken) body.hf_token = state.hfToken;
    const resp = await fetch(`http://127.0.0.1:${port}/installer/finalize`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    if (!resp.ok) throw new Error(`finalize HTTP ${resp.status}`);
    const data = await resp.json();
    // Gate: only proceed if the backend confirmed it persisted state and is
    // ready. Prevents marking onboarding complete / navigating to /ui/ on top
    // of a sidecar that stayed in minimal mode.
    if (data.status !== "ready") throw new Error(`finalize status ${data.status}`);
    state.apiKey = data.api_key || "";
    saveState();
  } catch (_err) {
    const errEl = document.createElement("p");
    errEl.className = "error-msg";
    errEl.textContent = t("step5_error", state.lang);
    wrapper.replaceChild(errEl, statusEl);
    return;
  }

  // Always show the API key (not only in advanced mode).
  // The user may need to copy it for external integrations even when in
  // normal mode. Confirmed during an AI-assisted review (aligned with
  // the original installer CLI which always shows it in show_final_summary).
  if (state.apiKey) {
    const keyLabel = document.createElement("p");
    keyLabel.textContent = t("step5_apikey_label", state.lang);
    advancedContainer.appendChild(keyLabel);

    const keyEl = document.createElement("code");
    keyEl.className = "api-key";
    keyEl.textContent = state.apiKey;
    advancedContainer.appendChild(keyEl);

    const hint = document.createElement("p");
    hint.className = "hint";
    hint.textContent = t("step5_apikey_hint", state.lang);
    advancedContainer.appendChild(hint);
  }

  statusEl.textContent = t("step5_restarting", state.lang);
  startBtn.disabled = false;

  startBtn.addEventListener("click", async () => {
    startBtn.disabled = true;
    statusEl.textContent = t("step5_restarting", state.lang);

    // ── Phase B — restart sidecar (new ephemeral port) ───────────────────────
    // Always restart so the new sidecar reads the freshly-persisted
    // OnboardingState (engine + model, or NEXE_STORAGE_PATH for "local") and
    // leaves minimal mode — only then is the chat UI under /ui/ fully wired.
    let nextPort;
    try {
      nextPort = await invoke("restart_sidecar");
    } catch (err) {
      startBtn.disabled = false;
      const errEl = document.createElement("p");
      errEl.className = "error-msg";
      errEl.textContent = `${t("step5_error", state.lang)}: ${err}`;
      wrapper.replaceChild(errEl, statusEl);
      return;
    }

    // ── Phase C — persist api_key + mark wizard complete ─────────────────────
    localStorage.setItem("nexe_api_key", state.apiKey);
    await invoke("mark_onboarding_complete");
    localStorage.removeItem("nexe_onboarding_state");

    // ── Phase D — navigate to chat UI on the (possibly new) port ────────────
    // `replace` (not `href`) so the Back button cannot return to the dead old
    // sidecar port (decided during an AI-assisted review). After the revert, target the
    // sidecar HTTP origin directly with the api_key as a percent-encoded
    // query param — app.js picks it up on first load, persists it to the
    // sidecar-origin localStorage, and scrubs the URL. Path is `/ui/`: the
    // web_ui_module router mounts under that prefix (routes.py:106); `/`
    // returns the framework identity JSON which the webview would render
    // as plain text.
    const encodedKey = encodeURIComponent(state.apiKey);
    window.location.replace(`http://127.0.0.1:${nextPort}/ui/?nexe_api_key=${encodedKey}`);
  });
}
