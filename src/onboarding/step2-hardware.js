// Step 2 — Model selection.
// Normal mode: custom dropdown of compatible models + download.
// Advanced mode: local models folder + selector.

import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { goToStep, state, saveState } from "./main.js";
import { t } from "./i18n.js";

// HF tokens page. The wizard does NOT auto-open the browser
// (Tauri 2 needs a plugin or custom Rust command for that, and we want to
// avoid new dependencies here — see ADR-pending). Instead, the "Get
// token" button copies the URL to the clipboard so the user can paste it
// into their browser. Lower-friction than asking the user to type it.
const HF_TOKENS_URL = "https://huggingface.co/settings/tokens";

// MC-036: the outside-click handler was attached to `document` on EVERY dropdown
// render and never removed, so re-rendering Step 2 (theme/lang/back-forward)
// stacked N global click listeners, each closing over a stale dropdown. We instead
// install ONE delegated listener at module scope (idempotent) that closes any open
// `.custom-dropdown` when clicking outside it. Event delegation means zero
// accumulation across renders AND correct behaviour with several simultaneous
// dropdowns (a per-render AbortController would detach sibling dropdowns).
let _outsideClickInstalled = false;

// Test-only reset of the install latch (see blocb-frontend.test.js).
export function _resetOutsideClickForTest() {
  _outsideClickInstalled = false;
}

function _ensureOutsideClickListener() {
  if (_outsideClickInstalled) return;
  _outsideClickInstalled = true;
  document.addEventListener("click", (e) => {
    document.querySelectorAll(".custom-dropdown").forEach((wrap) => {
      if (wrap.contains(e.target)) return;
      const list = wrap.querySelector(".custom-dropdown-list");
      const arrow = wrap.querySelector(".dropdown-arrow");
      if (list) list.style.display = "none";
      if (arrow) arrow.textContent = "▾";
    });
  });
}

// ── Helpers ───────────────────────────────────────────────────

// Origin → flag emoji. Lookup table instead of chained includes so the
// classification stays flat (low CCN) and new origins are a one-line edit.
// Exported for unit testing.
const ORIGIN_FLAGS = [
  { keywords: ["alibaba", "deepseek", "xina", "china"], flag: "🇨🇳" },
  { keywords: ["bsc", "aina", "mistral", "catalunya", "europa"], flag: "🇪🇺" },
];
const DEFAULT_ORIGIN_FLAG = "🇺🇸";

export function originFlag(origin) {
  const o = (origin || "").toLowerCase();
  for (const { keywords, flag } of ORIGIN_FLAGS) {
    if (keywords.some((kw) => o.includes(kw))) return flag;
  }
  return DEFAULT_ORIGIN_FLAG;
}

// Model name → capability flags. Same lookup-table pattern. Exported for tests.
const FLAG_RULES = [
  { keywords: ["-vl", "vision"], flag: "vision" },
  { keywords: ["r1", "qwen3", "thinking"], flag: "thinking" },
  { keywords: ["e4b", "a3b", "moe", "distill"], flag: "moe" },
];

export function inferFlags(model) {
  const n = (model.name || "").toLowerCase();
  return FLAG_RULES
    .filter(({ keywords }) => keywords.some((kw) => n.includes(kw)))
    .map(({ flag }) => flag);
}

// ── Main ──────────────────────────────────────────────────────

export async function step2() {
  const app = document.getElementById("onboarding-app");
  app.replaceChildren();

  if (!state.hardware.ram_gb) state.hardware = await invoke("get_hardware");
  if (!state.catalog.length)  state.catalog  = await invoke("fetch_catalog");

  const wrapper = document.createElement("div");
  wrapper.className = "step step2";

  const logo = document.createElement("img");
  logo.src = "/onboarding-logo.png";
  logo.alt = "server-nexe";
  logo.className = "nexe-logo";
  wrapper.appendChild(logo);

  wrapper.appendChild(_buildHwBadges());

  const expl = document.createElement("p");
  expl.className = "step2-explainer";
  expl.textContent = t("step2_explainer", state.lang);
  wrapper.appendChild(expl);

  if (state.hardware.ram_gb < 12) {
    const warn = document.createElement("p");
    warn.className = "step2-tier-warning";
    warn.textContent = t("step2_8gb_warning", state.lang);
    wrapper.appendChild(warn);
  }

  const zone = document.createElement("div");
  zone.id = "step2-zone";
  wrapper.appendChild(zone);

  _renderZone(zone);
  app.appendChild(wrapper);
}

function _renderZone(zone) {
  zone.replaceChildren();
  state.advanced ? _renderAdvancedZone(zone) : _renderNormalZone(zone);
}

// ── Mode normal ───────────────────────────────────────────────

// Helper to derive model_id based on the chosen engine.
// Extracted from the original inline code (_buildCustomDropdown) because the
// user can now switch engine via the clickable badge — re-derivation needed.
function _deriveModelId(model, engine) {
  const e = engine.toLowerCase().replace("llama.cpp", "gguf");
  if (e === "mlx") return model.mlx || model.ollama || "";
  if (e === "gguf") return model.gguf || "";
  return model.ollama || model.mlx || "";
}

// Filter backends by Metal availability.
// If Metal is unavailable (Intel, Linux), hide MLX from the list.
function _filterBackendsByMetal(backends) {
  const metalOK = state.metalAvailable !== false;
  return (backends || []).filter((b) => metalOK || b.toLowerCase() !== "mlx");
}

// B054: does the current selection need an HF token at download time? Only
// HF-hosted gated repos pulled via mlx/gguf — Ollama pulls the same model
// from its own registry with no token. Exported for unit testing.
export function _modelNeedsToken(sel) {
  return !!(sel && sel.gated && sel.engine !== "ollama");
}

// Show/hide the normal-zone HF token block to match the current selection,
// so basic users picking a non-gated model never see it but a gated MLX pick
// surfaces the input. Safe to call when the slot is absent (advanced zone).
function _updateTokenSlotVisibility() {
  const slot = document.getElementById("hf-token-slot");
  if (!slot) return;
  const needs = _modelNeedsToken(state.selectedModel);
  slot.style.display = needs ? "" : "none";
  // Rebuild the block so a gated pick shows an OPEN, "required" token input the
  // user can't overlook; a non-gated pick collapses back to the optional form.
  // _buildHfTokenBlock reads state.hfToken, so the typed value is preserved.
  slot.replaceChildren(_buildHfTokenBlock({ open: needs, required: needs }));
}

function _renderNormalZone(zone) {
  const usable = state.hardware.ram_gb * 0.55;
  // Map instead of filter — show ALL models but flag
  // those that don't fit in available RAM as _disabled.
  // Chosen for clearer UX, aligned with
  // the original CLI which shows fits_tight rather than hiding.
  const shown = state.catalog.map((m) => ({
    ...m,
    _disabled: usable < m.ram_gb,
    _availableBackends: _filterBackendsByMetal(m.backends),
  }));

  const dropLabel = document.createElement("span");
  dropLabel.className = "field-label-prominent";
  dropLabel.textContent = t("select_model_title", state.lang);
  zone.appendChild(dropLabel);

  // Dropdown custom
  const dropdown = _buildCustomDropdown(shown);
  zone.appendChild(dropdown);

  // B054: gated HF models (mlx/gguf) need a token at download time. The block
  // lives here collapsed and only shows when the current selection needs it
  // (toggled by the dropdown selection + engine-cycle handlers). Reuses the
  // same _buildHfTokenBlock as Advanced; the token stays in memory only.
  const tokenSlot = document.createElement("div");
  tokenSlot.id = "hf-token-slot";
  const needsToken = _modelNeedsToken(state.selectedModel);
  tokenSlot.appendChild(_buildHfTokenBlock({ open: needsToken, required: needsToken }));
  tokenSlot.style.display = needsToken ? "" : "none";
  zone.appendChild(tokenSlot);

  const dlBtn = document.createElement("button");
  dlBtn.className = "btn-primary btn-full";
  dlBtn.textContent = t("btn_start_download", state.lang);
  dlBtn.disabled = !state.selectedModel;
  dlBtn.id = "dl-btn";
  dlBtn.addEventListener("click", () => { if (state.selectedModel) goToStep(3); });
  zone.appendChild(dlBtn);

  const advLink = document.createElement("button");
  advLink.className = "btn-subtle";
  advLink.textContent = "⚙  " + t("btn_advanced", state.lang);
  advLink.addEventListener("click", () => {
    state.advanced = true;
    state.selectedModel = null;
    saveState();
    _renderZone(document.getElementById("step2-zone"));
  });
  zone.appendChild(advLink);
}

// Engine badge. Shows the active engine (e.g. "MLX").
// If availableBackends.length > 1, it's clickable with cycle behavior
// (1 click = switch to next available engine). Re-derives model_id if this
// model is selected.
function _buildEngineBadge(model, availableBackends, primaryEngine, isDisabled) {
  const engineBadge = document.createElement("span");
  engineBadge.className = "badge badge-engine";
  engineBadge.textContent = primaryEngine.toUpperCase();
  if (availableBackends.length <= 1 || isDisabled) {
    engineBadge.title = `Engine: ${primaryEngine}`;
    return engineBadge;
  }
  engineBadge.classList.add("badge-clickable");
  engineBadge.title = `Click: cycle engine (${availableBackends.join(" / ")})`;
  engineBadge.addEventListener("click", (e) => {
    e.stopPropagation();  // don't trigger model selection
    const norm = (b) => b.toLowerCase().replace("llama.cpp", "gguf");
    const idx = availableBackends.findIndex((b) => norm(b) === primaryEngine);
    const nextRaw = availableBackends[(idx + 1) % availableBackends.length];
    model._currentEngine = nextRaw;
    const newEngine = norm(nextRaw);
    engineBadge.textContent = newEngine.toUpperCase();
    // If this model was selected, re-derive state.
    if (state.selectedModel?.name === model.name) {
      state.selectedModel = {
        name: model.name,
        engine: newEngine,
        model_id: _deriveModelId(model, newEngine),
        disk_gb: model.disk_gb,
        gated: !!model.gated,
      };
      saveState();
      // Cycling a gated model MLX↔Ollama flips whether a token is needed.
      _updateTokenSlotVisibility();
    }
  });
  return engineBadge;
}

// Capability badges driven by inferFlags() — table instead of an if per flag.
const CAPABILITY_BADGES = [
  { flag: "thinking", cls: "badge-thinking", text: "🤔", title: "Thinking" },
  { flag: "vision", cls: "badge-vision", text: "👁", title: "Vision" },
  { flag: "moe", cls: "badge-moe", text: "⚡", title: "MoE" },
];

// Right-side badge row: RAM + engine + capability flags + gated lock.
function _buildModelMeta(model, { availableBackends, primaryEngine, isDisabled }) {
  const meta = document.createElement("div");
  meta.className = "dropdown-meta";

  const ramBadge = document.createElement("span");
  ramBadge.className = "badge badge-ram";
  ramBadge.textContent = model.ram_gb + " GB";
  meta.appendChild(ramBadge);

  meta.appendChild(_buildEngineBadge(model, availableBackends, primaryEngine, isDisabled));

  const flags = inferFlags(model);
  for (const { flag, cls, text, title } of CAPABILITY_BADGES) {
    if (!flags.includes(flag)) continue;
    const b = document.createElement("span");
    b.className = "badge " + cls;
    b.textContent = text;
    b.title = title;
    meta.appendChild(b);
  }
  if (model.gated) {
    const b = document.createElement("span");
    b.className = "badge badge-gated";
    b.textContent = "🔒";
    b.title = "Requires Hugging Face token";
    meta.appendChild(b);
  }
  return meta;
}

// Build one dropdown row for a model. Returns the item element, or null when
// the model has no available backend (e.g. Intel host + MLX-only model).
function _renderDropdownItem(model, { listWrap, triggerText, arrow }) {
  // Backends filtered by Metal availability + helper to
  // derive model_id. If no backend is available, skip the model.
  const availableBackends = model._availableBackends || _filterBackendsByMetal(model.backends);
  if (availableBackends.length === 0) return null;
  // Active engine: if the user cycled it for this model, respect _currentEngine
  // (in-memory inside the item, not persisted across models).
  const currentEngineRaw = model._currentEngine || availableBackends[0];
  const primaryEngine = currentEngineRaw.toLowerCase().replace("llama.cpp", "gguf");
  const modelId = _deriveModelId(model, primaryEngine);
  // `disabled` class if the model does not fit in RAM.
  const isDisabled = model._disabled === true;

  const item = document.createElement("div");
  item.className = "dropdown-item"
    + (state.selectedModel?.model_id === modelId ? " selected" : "")
    + (isDisabled ? " disabled" : "");
  if (isDisabled) {
    item.title = `Requires ${model.ram_gb} GB RAM`;
  }

  // Origin flag
  const flagEl = document.createElement("span");
  flagEl.className = "dropdown-flag";
  flagEl.textContent = originFlag(model.origin);
  item.appendChild(flagEl);

  // Name + params
  const nameWrap = document.createElement("div");
  nameWrap.className = "dropdown-name-wrap";
  const nameEl = document.createElement("span");
  nameEl.className = "dropdown-name";
  nameEl.textContent = model.name;
  const paramsEl = document.createElement("span");
  paramsEl.className = "dropdown-params";
  paramsEl.textContent = model.params;
  nameWrap.appendChild(nameEl);
  nameWrap.appendChild(paramsEl);
  item.appendChild(nameWrap);

  item.appendChild(_buildModelMeta(model, { availableBackends, primaryEngine, isDisabled }));

  // Disabled models cannot be selected.
  if (!isDisabled) {
    item.addEventListener("click", () => {
      // MC-063: recompute engine + model_id at click time. Cycling the
      // engine badge mutates `model._currentEngine` without re-rendering the row,
      // so the render-time `primaryEngine`/`modelId` may be stale — reading them
      // here would silently revert the selection to the pre-cycle engine.
      const clickEngine = (model._currentEngine || availableBackends[0])
        .toLowerCase().replace("llama.cpp", "gguf");
      const clickModelId = _deriveModelId(model, clickEngine);
      state.selectedModel = {
        name: model.name,
        engine: clickEngine,
        model_id: clickModelId,
        disk_gb: model.disk_gb,
        gated: !!model.gated,
      };
      saveState();
      _updateTokenSlotVisibility();
      triggerText.textContent = model.name;
      triggerText.className = "dropdown-selected-text";
      listWrap.style.display = "none";
      arrow.textContent = "▾";
      listWrap.querySelectorAll(".dropdown-item").forEach((el) => el.classList.remove("selected"));
      item.classList.add("selected");
      const btn = document.getElementById("dl-btn");
      if (btn) btn.disabled = false;
    });
  }

  return item;
}

export function _buildCustomDropdown(models) {
  const wrap = document.createElement("div");
  wrap.className = "custom-dropdown";
  wrap.setAttribute("tabindex", "0");

  const trigger = document.createElement("div");
  trigger.className = "custom-dropdown-trigger";
  trigger.id = "dropdown-trigger";

  const triggerText = document.createElement("span");
  triggerText.className = "dropdown-placeholder";
  triggerText.textContent = state.selectedModel
    ? state.selectedModel.name
    : t("select_model_placeholder", state.lang);
  trigger.appendChild(triggerText);

  const arrow = document.createElement("span");
  arrow.className = "dropdown-arrow";
  arrow.textContent = "▾";
  trigger.appendChild(arrow);

  const listWrap = document.createElement("div");
  listWrap.className = "custom-dropdown-list";
  listWrap.style.display = "none";

  models.forEach((model) => {
    const item = _renderDropdownItem(model, { listWrap, triggerText, arrow });
    if (item) listWrap.appendChild(item);
  });

  trigger.addEventListener("click", () => {
    const open = listWrap.style.display !== "none";
    listWrap.style.display = open ? "none" : "block";
    arrow.textContent = open ? "▾" : "▴";
  });

  // Close when clicking outside — handled by the single delegated listener
  // installed once at module scope (MC-036), so renders never stack listeners.
  _ensureOutsideClickListener();

  wrap.appendChild(trigger);
  wrap.appendChild(listWrap);
  return wrap;
}

// ── Advanced mode ─────────────────────────────────────────────

function _renderAdvancedZone(zone) {
  const expl = document.createElement("p");
  expl.className = "step2-advanced-expl";
  expl.textContent = t("step2_advanced_explainer", state.lang);
  zone.appendChild(expl);

  const folderLabel = document.createElement("span");
  folderLabel.className = "field-label";
  folderLabel.textContent = t("models_folder_label", state.lang);
  zone.appendChild(folderLabel);

  const folderRow = document.createElement("div");
  folderRow.className = "folder-row";

  const pathInput = document.createElement("input");
  pathInput.type = "text";
  pathInput.readOnly = true;
  pathInput.value = state.modelsPath;
  pathInput.placeholder = "~/models";

  const folderBtn = document.createElement("button");
  folderBtn.className = "btn-secondary";
  folderBtn.textContent = t("models_folder_btn", state.lang);
  folderBtn.addEventListener("click", async () => {
    const result = await open({ directory: true, multiple: false });
    if (result && typeof result === "string") {
      state.modelsPath = result;
      pathInput.value = result;
      saveState();
      selBtn.disabled = false;
    }
  });

  folderRow.appendChild(pathInput);
  folderRow.appendChild(folderBtn);
  zone.appendChild(folderRow);

  const hint = document.createElement("p");
  hint.className = "hint";
  hint.textContent = t("models_folder_hint", state.lang);
  zone.appendChild(hint);

  const selBtn = document.createElement("button");
  selBtn.className = "btn-primary btn-full";
  selBtn.textContent = t("btn_select_local", state.lang);
  selBtn.disabled = !state.modelsPath;
  selBtn.addEventListener("click", () => {
    if (state.modelsPath) {
      state.selectedModel = { name: "local", engine: "local", model_id: state.modelsPath, disk_gb: 0 };
      saveState();
      goToStep(5);
    }
  });
  zone.appendChild(selBtn);

  // Optional Hugging Face token block. Only in Advanced —
  // basic users should not see this. Hidden behind a collapsed details
  // element so it does not visually clutter the step.
  zone.appendChild(_buildHfTokenBlock());

  const backLink = document.createElement("button");
  backLink.className = "btn-subtle";
  backLink.textContent = "← " + t("btn_back_normal", state.lang);
  backLink.addEventListener("click", () => {
    state.advanced = false;
    saveState();
    _renderZone(document.getElementById("step2-zone"));
  });
  zone.appendChild(backLink);
}

// ── HF Token block ──────────────────────────────────────

export function _buildHfTokenBlock(opts = {}) {
  const { open = false, required = false } = opts;
  const details = document.createElement("details");
  details.className = "hf-token-block";
  // B054: a gated selection opens the block and labels it required so the user
  // cannot miss the token input and end up stuck at the gated download.
  details.open = open;

  const summary = document.createElement("summary");
  summary.textContent = required
    ? t("hf_section_label_required", state.lang)
    : t("hf_section_label", state.lang);
  details.appendChild(summary);

  const hint = document.createElement("p");
  hint.className = "hint";
  hint.textContent = t("hf_token_hint", state.lang);
  details.appendChild(hint);

  const row = document.createElement("div");
  row.className = "folder-row";  // reuse existing flex row style

  const tokenInput = document.createElement("input");
  tokenInput.type = "password";
  tokenInput.placeholder = t("hf_token_placeholder", state.lang);
  tokenInput.value = state.hfToken || "";
  tokenInput.autocomplete = "off";
  tokenInput.spellcheck = false;
  tokenInput.addEventListener("input", () => {
    // Trim whitespace pasted from token UIs.
    state.hfToken = tokenInput.value.trim();
    // NOTE: we intentionally do NOT call saveState() here — the token
    // stays in memory only (see main.js::saveState filter).
  });
  row.appendChild(tokenInput);

  const copyBtn = document.createElement("button");
  copyBtn.className = "btn-secondary";
  copyBtn.type = "button";
  copyBtn.textContent = t("hf_get_token_btn", state.lang);
  copyBtn.addEventListener("click", async () => {
    let copied = false;
    try {
      if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(HF_TOKENS_URL);
        copied = true;
      }
    } catch (_) {
      copied = false;
    }
    copyBtn.textContent = copied
      ? t("hf_get_token_btn_copied", state.lang)
      : HF_TOKENS_URL;
    setTimeout(() => {
      copyBtn.textContent = t("hf_get_token_btn", state.lang);
    }, 2500);
  });
  row.appendChild(copyBtn);

  details.appendChild(row);
  return details;
}

// ── HW badges ─────────────────────────────────────────────────

function _buildHwBadges() {
  const hw = state.hardware;
  const wrap = document.createElement("div");
  wrap.className = "hw-badges";
  [
    { label: "RAM", value: hw.ram_gb + " GB" },
    { label: t("hw_os", state.lang), value: (hw.os || "—").split(" ").slice(0, 2).join(" ") },
    { label: t("hw_disk", state.lang), value: hw.disk_free_gb + " GB" },
  ].forEach(({ label, value }) => {
    const badge = document.createElement("div");
    badge.className = "hw-badge";
    const l = document.createElement("span");
    l.className = "hw-badge-label";
    l.textContent = label;
    const v = document.createElement("span");
    v.className = "hw-badge-value";
    v.textContent = value;
    badge.appendChild(l);
    badge.appendChild(v);
    wrap.appendChild(badge);
  });
  return wrap;
}
