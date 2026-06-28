// INST-002-FE — the onboarding wizard surfaces the backend's non-blocking
// SHA256_NOT_PINNED warning (server-nexe c71736a) instead of silencing it.
// Node vitest environment with manual DOM shims (matches b054-hf-token.test.js
// / onboarding.test.js — no jsdom).
//
// What this pins (mutation notes inline):
//  - a `warning` SSE frame invokes the onWarning callback and is NON-terminal
//    (the stream keeps reading until `done`)
//  - end-to-end: a stream progress→warning→done still completes (goToStep(4))
//    AND records the warning in state.shaWarnings
//  - step4 recaps the warning as a banner only when there is one

import { describe, it, expect, afterEach, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => 1234) }));

function mkEl(tag) {
  const el = {
    tagName: String(tag).toUpperCase(),
    className: "", id: "", textContent: "", value: 0, max: 100,
    style: { cssText: "", display: "" },
    classList: { add() {}, remove() {}, toggle() {}, contains() { return false; } },
    children: [], _listeners: {}, disabled: false,
    appendChild(c) { el.children.push(c); return c; },
    replaceChildren(...a) { el.children = [...a]; },
    setAttribute(k, v) { el[k] = v; },
    getAttribute(k) { return el[k] ?? null; },
    addEventListener(ev, fn) { el._listeners[ev] = fn; },
    querySelectorAll() { return []; },
    querySelector() { return null; },
    contains() { return false; },
    remove() {}, focus() {},
  };
  return el;
}

let appEl;
function installDocShim() {
  appEl = mkEl("div");
  globalThis.window = { addEventListener: () => {}, location: { replace: vi.fn() } };
  globalThis.document = {
    createElement: (t) => mkEl(t),
    createTextNode: (text) => ({ nodeType: 3, textContent: text }),
    getElementById: () => appEl,
    querySelector: () => null,
    querySelectorAll: () => [],
    addEventListener: () => {},
    head: { appendChild: () => {} },
    body: { appendChild: () => {}, replaceChildren: () => {}, children: [] },
  };
  globalThis.AbortController = AbortController;
}

/** Depth-first search for the first node carrying `cls` in its className. */
function findByClass(node, cls) {
  if (!node) return null;
  if (node.className === cls) return node;
  for (const c of node.children || []) {
    const found = findByClass(c, cls);
    if (found) return found;
  }
  return null;
}

/** A ReadableStream-ish reader that emits each SSE frame as its own chunk. */
function frameReader(frames) {
  const enc = new TextEncoder();
  const chunks = frames.map((f) => enc.encode(f + "\n\n"));
  let i = 0;
  return {
    read: async () => (i < chunks.length ? { done: false, value: chunks[i++] } : { done: true }),
    cancel() {},
  };
}

afterEach(() => {
  vi.resetModules();
  vi.useRealTimers();
  vi.clearAllMocks();
  delete globalThis.fetch;
});

// ---------------------------------------------------------------------------
// _handleSseFrame — a `warning` frame fires onWarning and keeps reading
// ---------------------------------------------------------------------------
describe("INST-002-FE — _handleSseFrame surfaces warning frames", () => {
  async function loadStep3() {
    vi.resetModules();
    vi.doMock("./main.js", () => ({ goToStep: vi.fn(), state: { lang: "ca", selectedModel: null } }));
    vi.doMock("./i18n.js", () => ({ t: (k) => k }));
    installDocShim();
    return import("./step3-download.js");
  }

  it("invokes onWarning with the frame data and returns null (non-terminal)", async () => {
    const { _handleSseFrame } = await loadStep3();
    const onWarning = vi.fn();
    const frame = 'data: {"type":"warning","code":"SHA256_NOT_PINNED","message":"m: unpinned"}';
    const result = _handleSseFrame(frame, mkEl("progress"), mkEl("p"), { cancel() {} }, onWarning);
    // Mutation: route warning to the default case → onWarning never fires.
    expect(onWarning).toHaveBeenCalledTimes(1);
    expect(onWarning.mock.calls[0][0].code).toBe("SHA256_NOT_PINNED");
    // Mutation: make warning terminal (return {ok}) → this stops being null.
    expect(result).toBeNull();
  });

  it("does not throw when onWarning is absent (4-arg back-compat)", async () => {
    const { _handleSseFrame } = await loadStep3();
    const frame = 'data: {"type":"warning","code":"SHA256_NOT_PINNED"}';
    expect(_handleSseFrame(frame, mkEl("progress"), mkEl("p"), { cancel() {} })).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// step3 end-to-end — progress → warning → done completes AND records the warning
// ---------------------------------------------------------------------------
describe("INST-002-FE — step3 records the warning and still completes", () => {
  async function loadStep3(state, goToStep) {
    vi.resetModules();
    vi.doMock("./main.js", () => ({ goToStep, state }));
    vi.doMock("./i18n.js", () => ({ t: (k) => k }));
    installDocShim();
    return import("./step3-download.js");
  }

  it("a SHA256_NOT_PINNED warning is captured in state.shaWarnings; download reaches step4", async () => {
    const goToStep = vi.fn();
    const state = {
      lang: "ca", hfToken: "", shaWarnings: [],
      selectedModel: { name: "Gemma 4 E4B", engine: "mlx", model_id: "google/gemma", gated: false },
    };
    const { step3 } = await loadStep3(state, goToStep);
    globalThis.fetch = vi.fn(async () => ({
      ok: true,
      body: {
        getReader: () => frameReader([
          'data: {"type":"progress","percent":50}',
          'data: {"type":"warning","code":"SHA256_NOT_PINNED","message":"google/gemma: unpinned"}',
          'data: {"type":"done","model_id":"google/gemma"}',
        ]),
      },
    }));

    await step3();

    // The warning was recorded (onWarning ran), not swallowed.
    expect(state.shaWarnings).toHaveLength(1);
    expect(state.shaWarnings[0].code).toBe("SHA256_NOT_PINNED");
    // It is also rendered inline (not just recorded), with the model name.
    // Mutation: drop `_renderShaWarning(data, warningsEl)` (keep the push) → red.
    const warnBox = findByClass(appEl, "step3-warnings");
    expect(warnBox).not.toBeNull();
    const line = findByClass(warnBox, "download-warning");
    expect(line).not.toBeNull();
    expect(line.textContent).toContain("Gemma 4 E4B");
    // And the download still completed — the warning did NOT abort it.
    // Mutation: make warning terminal/abort → goToStep(4) is never reached.
    expect(goToStep).toHaveBeenCalledWith(4);
  });

  it("renders one inline line per warning frame (N > 1)", async () => {
    const goToStep = vi.fn();
    const state = {
      lang: "ca", hfToken: "", shaWarnings: [],
      selectedModel: { name: "Gemma 4 E4B", engine: "mlx", model_id: "google/gemma", gated: false },
    };
    const { step3 } = await loadStep3(state, goToStep);
    globalThis.fetch = vi.fn(async () => ({
      ok: true,
      body: {
        getReader: () => frameReader([
          'data: {"type":"warning","code":"SHA256_NOT_PINNED","message":"a"}',
          'data: {"type":"warning","code":"SHA256_NOT_PINNED","message":"b"}',
          'data: {"type":"done","model_id":"google/gemma"}',
        ]),
      },
    }));

    await step3();

    expect(state.shaWarnings).toHaveLength(2);
    const warnBox = findByClass(appEl, "step3-warnings");
    const lines = (warnBox.children || []).filter((c) => c.className === "download-warning");
    // The contract allows >1 warning; each must render its own inline line.
    expect(lines).toHaveLength(2);
    expect(goToStep).toHaveBeenCalledWith(4);
  });

  it("no warning frame → shaWarnings stays empty and download still completes", async () => {
    const goToStep = vi.fn();
    const state = {
      lang: "ca", hfToken: "", shaWarnings: [],
      selectedModel: { name: "Qwen", engine: "mlx", model_id: "Qwen/x", gated: false },
    };
    const { step3 } = await loadStep3(state, goToStep);
    globalThis.fetch = vi.fn(async () => ({
      ok: true,
      body: {
        getReader: () => frameReader([
          'data: {"type":"progress","percent":100}',
          'data: {"type":"done","model_id":"Qwen/x"}',
        ]),
      },
    }));

    await step3();

    expect(state.shaWarnings).toHaveLength(0);
    expect(goToStep).toHaveBeenCalledWith(4);
  });
});

// ---------------------------------------------------------------------------
// step4 — recap banner shown only when there is a warning
// ---------------------------------------------------------------------------
describe("INST-002-FE — step4 recaps the warning", () => {
  async function loadStep4(state) {
    vi.resetModules();
    vi.doMock("./main.js", () => ({ goToStep: vi.fn(), state }));
    vi.doMock("./i18n.js", () => ({ t: (k) => k }));
    installDocShim();
    return import("./step4-done.js");
  }

  it("renders the .sha-warning-banner when state.shaWarnings is non-empty", async () => {
    const state = { lang: "ca", shaWarnings: [{ model: "Gemma", code: "SHA256_NOT_PINNED" }] };
    const { step4 } = await loadStep4(state);
    step4();
    const banner = findByClass(appEl, "sha-warning-banner");
    // Mutation: drop the `if (state.shaWarnings...)` block → banner disappears.
    expect(banner).not.toBeNull();
    // The banner carries a single <p> with the localised recap key + ⚠ prefix.
    expect(banner.children).toHaveLength(1);
    expect(banner.children[0].textContent).toContain("step4_sha_warning");
  });

  it("renders no banner when there are no warnings", async () => {
    const state = { lang: "ca", shaWarnings: [] };
    const { step4 } = await loadStep4(state);
    step4();
    expect(findByClass(appEl, "sha-warning-banner")).toBeNull();
  });
});
