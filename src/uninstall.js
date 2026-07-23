// Uninstall modal (Finding B) — selective wipe with checkboxes.
//
// This renders inside a DEDICATED Tauri window (label "uninstall", page
// uninstall.html), opened from the tray "Uninstall…" item. A dedicated window
// is used instead of an event to the main webview because, after onboarding,
// the main webview navigates to the sidecar HTTP origin (main.js
// `window.location.replace(...)`), where our JS no longer runs — an emitted
// event would be dead. This window IS the dialog: it builds the modal directly
// at load (see uninstall-page.js), Cancel closes the window, and Uninstall
// calls the gated `uninstall_with_options` command (which shows its OWN native
// confirmation before touching disk, then wipes + exits the app).
//
// CSP: no inline <script> — everything is built via createElement + wired with
// addEventListener from this module (script-src 'self'). The scoped <style> is
// allowed by style-src 'unsafe-inline'.

import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { t } from "./onboarding/i18n.js";

const MODAL_ID = "uninstall-modal-overlay";

// Ids of the option checkboxes — single source of truth for build + collect.
const OPT_IDS = {
  models: "uninstall-opt-models",
  conversations: "uninstall-opt-conversations",
  library: "uninstall-opt-library",
  ollama: "uninstall-opt-ollama",
  embeddingsCache: "uninstall-opt-embeddings",
};

const MODAL_CSS = `
#${MODAL_ID} {
  position: fixed; inset: 0; z-index: 99999;
  display: flex; align-items: center; justify-content: center;
  background: #0a0a0a;
  font-family: Inter, system-ui, -apple-system, sans-serif;
}
#${MODAL_ID} .uninstall-card {
  background: #141414; color: #eaeaea;
  border: 1px solid #2a2a2a; border-radius: 12px;
  max-width: 470px; width: calc(100% - 2rem);
  padding: 1.4rem 1.4rem 1.15rem;
  box-shadow: 0 12px 44px rgba(0,0,0,.55);
  max-height: calc(100vh - 1.5rem); overflow-y: auto;
}
#${MODAL_ID} h2 { font-size: 1.12rem; margin: 0 0 .5rem; }
#${MODAL_ID} .uninstall-intro { font-size: .84rem; opacity: .75; margin: 0 0 .9rem; line-height: 1.45; }
#${MODAL_ID} label {
  display: flex; gap: .55rem; align-items: flex-start;
  padding: .5rem .55rem; border-radius: 8px; cursor: pointer;
  font-size: .88rem; line-height: 1.3;
}
#${MODAL_ID} label:hover { background: #1e1e1e; }
#${MODAL_ID} input[type="checkbox"] {
  margin-top: .15rem; width: 16px; height: 16px; flex: 0 0 auto; accent-color: #e62020; cursor: pointer;
}
#${MODAL_ID} .opt-desc { display: block; font-size: .72rem; opacity: .58; margin-top: .1rem; }
#${MODAL_ID} .uninstall-warn { color: #e6a020; font-size: .76rem; margin: .7rem 0 0; }
#${MODAL_ID} .uninstall-status { font-size: .76rem; opacity: .8; margin: .45rem 0 0; min-height: 1em; }
#${MODAL_ID} .uninstall-actions {
  display: flex; gap: .5rem; justify-content: flex-end; align-items: center;
  margin-top: 1.05rem; flex-wrap: wrap;
}
#${MODAL_ID} button {
  font: inherit; font-size: .83rem; padding: .48rem .95rem; border-radius: 8px;
  border: 1px solid #333; background: #1e1e1e; color: #eaeaea; cursor: pointer;
}
#${MODAL_ID} button:hover { border-color: #555; }
#${MODAL_ID} .btn-all { margin-right: auto; background: transparent; border-color: #444; }
#${MODAL_ID} .btn-danger { background: #7a1414; border-color: #a01c1c; color: #fff; }
#${MODAL_ID} .btn-danger:hover { background: #941818; border-color: #b02020; }
#${MODAL_ID} button[disabled] { opacity: .5; cursor: default; }
`;

/**
 * Language for the modal: saved onboarding lang (localStorage) → navigator →
 * 'ca'. Kept in sync with the wizard's default so a user who picked ES/EN keeps
 * that choice here too.
 */
export function pickLang() {
  try {
    const raw = localStorage.getItem("nexe_onboarding_state");
    if (raw) {
      const saved = JSON.parse(raw);
      if (saved && typeof saved.lang === "string") return saved.lang.substring(0, 2);
    }
  } catch (_) {
    // localStorage/JSON unavailable — fall through to navigator.
  }
  return ((typeof navigator !== "undefined" && navigator.language) || "ca").substring(0, 2);
}

/** Collect the checkbox state into the `UninstallOptions` shape Rust expects. */
export function collectOpts(root) {
  const checked = (id) => !!root.querySelector(`#${id}`)?.checked;
  return {
    models: checked(OPT_IDS.models),
    conversations: checked(OPT_IDS.conversations),
    library: checked(OPT_IDS.library),
    ollama: checked(OPT_IDS.ollama),
    embeddings_cache: checked(OPT_IDS.embeddingsCache),
  };
}

/** True when at least one category is selected. */
export function hasSelection(opts) {
  return !!(
    opts.models ||
    opts.conversations ||
    opts.library ||
    opts.ollama ||
    opts.embeddings_cache
  );
}

function optionRow(doc, lang, id, labelKey, descKey) {
  const cb = doc.createElement("input");
  cb.type = "checkbox";
  cb.id = id;

  const textWrap = doc.createElement("span");
  const strong = doc.createElement("span");
  strong.textContent = t(labelKey, lang);
  textWrap.appendChild(strong);
  const desc = doc.createElement("span");
  desc.className = "opt-desc";
  desc.textContent = t(descKey, lang);
  textWrap.appendChild(desc);

  const label = doc.createElement("label");
  label.appendChild(cb);
  label.appendChild(textWrap);
  return label;
}

/**
 * Build (but do not mount) the modal overlay. Pure DOM construction so it is
 * unit testable with a DOM shim. `onCancel(overlay)` and
 * `onConfirm(overlay, refs)` are invoked on the respective button clicks.
 */
export function buildUninstallModal(doc, lang, { onCancel, onConfirm }) {
  const overlay = doc.createElement("div");
  overlay.id = MODAL_ID;

  const style = doc.createElement("style");
  style.textContent = MODAL_CSS;
  overlay.appendChild(style);

  const card = doc.createElement("div");
  card.className = "uninstall-card";

  const h2 = doc.createElement("h2");
  h2.textContent = t("uninstall_title", lang);
  card.appendChild(h2);

  const intro = doc.createElement("p");
  intro.className = "uninstall-intro";
  intro.textContent = t("uninstall_intro", lang);
  card.appendChild(intro);

  card.appendChild(
    optionRow(doc, lang, OPT_IDS.models, "uninstall_opt_models", "uninstall_opt_models_desc")
  );
  card.appendChild(
    optionRow(
      doc,
      lang,
      OPT_IDS.conversations,
      "uninstall_opt_conversations",
      "uninstall_opt_conversations_desc"
    )
  );
  card.appendChild(
    optionRow(doc, lang, OPT_IDS.library, "uninstall_opt_library", "uninstall_opt_library_desc")
  );
  card.appendChild(
    optionRow(doc, lang, OPT_IDS.ollama, "uninstall_opt_ollama", "uninstall_opt_ollama_desc")
  );
  card.appendChild(
    optionRow(
      doc,
      lang,
      OPT_IDS.embeddingsCache,
      "uninstall_opt_embeddings",
      "uninstall_opt_embeddings_desc"
    )
  );

  const warn = doc.createElement("p");
  warn.className = "uninstall-warn";
  warn.textContent = t("uninstall_warn", lang);
  card.appendChild(warn);

  const status = doc.createElement("p");
  status.className = "uninstall-status";
  card.appendChild(status);

  const actions = doc.createElement("div");
  actions.className = "uninstall-actions";

  const cancelBtn = doc.createElement("button");
  cancelBtn.type = "button";
  cancelBtn.textContent = t("uninstall_btn_cancel", lang);
  cancelBtn.addEventListener("click", () => onCancel(overlay));

  const confirmBtn = doc.createElement("button");
  confirmBtn.type = "button";
  confirmBtn.className = "btn-danger";
  confirmBtn.textContent = t("uninstall_btn_confirm", lang);
  confirmBtn.addEventListener("click", () =>
    onConfirm(overlay, { status, confirmBtn, cancelBtn })
  );

  // UX1: "Erase everything and quit" — tick all four boxes (incl. Ollama) AND
  // submit, matching what the label promises. Ticking alone would be a lie: the
  // user would still have to press Uninstall. Routes through the SAME onConfirm
  // (which hits the native WSA-002 gate + exit), so it is not a shortcut around
  // the confirmation.
  const allBtn = doc.createElement("button");
  allBtn.type = "button";
  allBtn.className = "btn-all";
  allBtn.textContent = t("uninstall_select_all", lang);
  allBtn.addEventListener("click", () => {
    for (const id of Object.values(OPT_IDS)) {
      const cb = overlay.querySelector(`#${id}`);
      if (cb) cb.checked = true;
    }
    onConfirm(overlay, { status, confirmBtn, cancelBtn });
  });

  actions.appendChild(allBtn);
  actions.appendChild(cancelBtn);
  actions.appendChild(confirmBtn);

  card.appendChild(actions);
  overlay.appendChild(card);
  return overlay;
}

/**
 * Wire the confirm flow: validate a selection, then call the gated Rust command.
 * Exported (thin wrapper over `invoke`) so the button behaviour is testable.
 * `refs` are the live button/status nodes from `buildUninstallModal`.
 */
export async function submitUninstall(root, refs, lang) {
  const opts = collectOpts(root);
  if (!hasSelection(opts)) {
    refs.status.textContent = t("uninstall_nothing", lang);
    return;
  }
  refs.confirmBtn.disabled = true;
  refs.cancelBtn.disabled = true;
  refs.status.textContent = t("uninstall_closing", lang);
  try {
    // The Rust command shows its OWN native confirmation before touching disk;
    // on confirm it wipes + exits the app (so this promise usually never
    // resolves). If the user cancels that native gate it returns {exited:false}
    // and we re-enable the buttons.
    const outcome = await invoke("uninstall_with_options", { opts });
    if (outcome && outcome.exited === false) {
      refs.confirmBtn.disabled = false;
      refs.cancelBtn.disabled = false;
      refs.status.textContent = "";
    }
  } catch (err) {
    refs.confirmBtn.disabled = false;
    refs.cancelBtn.disabled = false;
    refs.status.textContent = String(err);
  }
}

/**
 * Entry point for the dedicated uninstall window (uninstall.html). Renders the
 * modal into `doc.body` immediately — no Tauri event needed (the tray opens this
 * window directly). Cancel closes this window; Uninstall runs the gated command.
 */
export function initUninstallWindow(doc = document) {
  const lang = pickLang();
  const overlay = buildUninstallModal(doc, lang, {
    onCancel: () => {
      // Close THIS window (dedicated to the dialog). Best-effort — if the
      // window API is unavailable there is nothing else to do.
      try {
        getCurrentWindow()
          .close()
          .catch(() => {});
      } catch (_) {
        /* no window API — nothing to close */
      }
    },
    onConfirm: (root, refs) => submitUninstall(root, refs, lang),
  });
  doc.body.appendChild(overlay);
}
