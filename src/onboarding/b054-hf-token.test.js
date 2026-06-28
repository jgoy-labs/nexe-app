// B054 — gated HF model token delivery in the normal onboarding flow.
// Node vitest environment with manual DOM shims (matches the repo's
// onboarding.test.js / blocb-frontend.test.js convention — no jsdom).
//
// What the fix guarantees and these tests pin (mutation notes inline):
//  - a gated model selected in the NORMAL zone marks state.selectedModel.gated
//  - step 3 hands the token to POST /installer/hf-token BEFORE the download,
//    and only for HF-hosted gated engines (mlx/gguf), never for Ollama
//  - the token travels in a POST body, never a query param (no access-log leak)

import { describe, it, expect, afterEach, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => 1234) }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn(async () => "/tmp") }));

// Minimal element shim — adds querySelectorAll (the dropdown selection handler
// calls it) on top of the blocb-frontend shim.
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

function installDocShim() {
  globalThis.window = { addEventListener: () => {}, location: { replace: vi.fn() } };
  globalThis.document = {
    createElement: (t) => mkEl(t),
    createTextNode: (text) => ({ nodeType: 3, textContent: text }),
    getElementById: () => mkEl("div"),
    querySelector: () => null,
    querySelectorAll: () => [],
    addEventListener: () => {},
    head: { appendChild: () => {} },
    body: { appendChild: () => {}, replaceChildren: () => {}, children: [] },
  };
  globalThis.AbortController = AbortController;
}

afterEach(() => {
  vi.resetModules();
  vi.useRealTimers();
  vi.clearAllMocks();
  delete globalThis.fetch;
});

// ---------------------------------------------------------------------------
// _modelNeedsToken — only HF-hosted gated repos (mlx/gguf) need a token
// ---------------------------------------------------------------------------
describe("B054 — _modelNeedsToken", () => {
  async function loadStep2() {
    vi.resetModules();
    vi.doMock("./main.js", () => ({
      goToStep: vi.fn(), saveState: vi.fn(), state: { lang: "ca", selectedModel: null },
    }));
    vi.doMock("./i18n.js", () => ({ t: (k) => k }));
    installDocShim();
    return import("./step2-hardware.js");
  }

  it("true for a gated MLX selection", async () => {
    const { _modelNeedsToken } = await loadStep2();
    expect(_modelNeedsToken({ gated: true, engine: "mlx" })).toBe(true);
    expect(_modelNeedsToken({ gated: true, engine: "gguf" })).toBe(true);
  });

  it("false for the same gated model pulled via Ollama (no token needed)", async () => {
    const { _modelNeedsToken } = await loadStep2();
    // Mutation: drop the `engine !== 'ollama'` guard and this goes red.
    expect(_modelNeedsToken({ gated: true, engine: "ollama" })).toBe(false);
  });

  it("false for a non-gated model and for an absent selection", async () => {
    const { _modelNeedsToken } = await loadStep2();
    expect(_modelNeedsToken({ gated: false, engine: "mlx" })).toBe(false);
    expect(_modelNeedsToken(null)).toBe(false);
    expect(_modelNeedsToken(undefined)).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// Selecting a gated model in the normal dropdown stamps `gated` onto the
// selection (so step 3 knows to deliver the token).
// ---------------------------------------------------------------------------
describe("B054 — dropdown selection carries `gated`", () => {
  let state;
  async function loadStep2() {
    vi.resetModules();
    state = { lang: "ca", selectedModel: null, metalAvailable: true };
    vi.doMock("./main.js", () => ({ goToStep: vi.fn(), saveState: vi.fn(), state }));
    vi.doMock("./i18n.js", () => ({ t: (k) => k }));
    installDocShim();
    return import("./step2-hardware.js");
  }

  it("a gated MLX pick sets selectedModel.gated = true", async () => {
    const { _buildCustomDropdown, _resetOutsideClickForTest } = await loadStep2();
    _resetOutsideClickForTest();
    const model = {
      name: "Gemma 4 E4B", params: "4B", origin: "google", ram_gb: 8, disk_gb: 5,
      gated: true, backends: ["MLX", "Ollama"], mlx: "google/gemma-4-e4b-mlx", ollama: "gemma4:e4b",
    };
    const wrap = _buildCustomDropdown([model]);
    const listWrap = wrap.children[1];
    const item = listWrap.children[0];
    item._listeners.click();
    // Mutation: remove `gated: !!model.gated` from the selection literal → undefined.
    expect(state.selectedModel.gated).toBe(true);
    expect(state.selectedModel.engine).toBe("mlx");
  });

  it("a non-gated pick sets selectedModel.gated = false", async () => {
    const { _buildCustomDropdown, _resetOutsideClickForTest } = await loadStep2();
    _resetOutsideClickForTest();
    const model = {
      name: "Qwen3.5 4B", params: "4B", origin: "alibaba", ram_gb: 6, disk_gb: 4,
      gated: false, backends: ["MLX"], mlx: "Qwen/qwen3.5-4b-mlx",
    };
    const wrap = _buildCustomDropdown([model]);
    const item = wrap.children[1].children[0];
    item._listeners.click();
    expect(state.selectedModel.gated).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// B054-A/B: the token block opens + is labelled required for a gated pick, so
// the user can't miss it and dead-end at the gated download.
// ---------------------------------------------------------------------------
describe("B054 — _buildHfTokenBlock open/required", () => {
  async function loadStep2() {
    vi.resetModules();
    vi.doMock("./main.js", () => ({
      goToStep: vi.fn(), saveState: vi.fn(), state: { lang: "ca", selectedModel: null, hfToken: "" },
    }));
    vi.doMock("./i18n.js", () => ({ t: (k) => k }));
    installDocShim();
    return import("./step2-hardware.js");
  }

  it("gated → block is open and summary uses the 'required' label", async () => {
    const { _buildHfTokenBlock } = await loadStep2();
    const details = _buildHfTokenBlock({ open: true, required: true });
    // Mutation: drop `details.open = open` → this goes red.
    expect(details.open).toBe(true);
    const summary = details.children[0];
    expect(summary.textContent).toBe("hf_section_label_required");
  });

  it("default (advanced/optional) → collapsed and optional label", async () => {
    const { _buildHfTokenBlock } = await loadStep2();
    const details = _buildHfTokenBlock();
    expect(details.open).toBe(false);
    expect(details.children[0].textContent).toBe("hf_section_label");
  });
});

// ---------------------------------------------------------------------------
// _handOverHfToken — POSTs the token (body, not query) and gates on failure
// ---------------------------------------------------------------------------
describe("B054 — _handOverHfToken", () => {
  let errorEl, cancelBtn, retryBtn;
  async function loadStep3() {
    vi.resetModules();
    vi.doMock("./main.js", () => ({ goToStep: vi.fn(), state: { lang: "ca" } }));
    vi.doMock("./i18n.js", () => ({ t: (k) => k }));
    installDocShim();
    errorEl = mkEl("p"); cancelBtn = mkEl("button"); retryBtn = mkEl("button");
    return import("./step3-download.js");
  }

  it("POSTs the token in the body to /installer/hf-token and returns true", async () => {
    const { _handOverHfToken } = await loadStep3();
    const calls = [];
    globalThis.fetch = vi.fn(async (url, opts) => {
      calls.push({ url: String(url), opts });
      return { ok: true, json: async () => ({ ok: true }) };
    });
    const ok = await _handOverHfToken(7777, "hf_secret", errorEl, cancelBtn, retryBtn);
    expect(ok).toBe(true);
    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe("http://127.0.0.1:7777/installer/hf-token");
    expect(calls[0].opts.method).toBe("POST");
    // Token in the BODY, never in the URL (mutation: move it to a query param
    // → the URL assertion above fails, and this one too).
    expect(calls[0].url).not.toContain("hf_secret");
    expect(JSON.parse(calls[0].opts.body)).toEqual({ token: "hf_secret" });
  });

  it("returns false + surfaces error + shows Retry on HTTP failure", async () => {
    const { _handOverHfToken } = await loadStep3();
    globalThis.fetch = vi.fn(async () => ({ ok: false, status: 500 }));
    const ok = await _handOverHfToken(7777, "hf_x", errorEl, cancelBtn, retryBtn);
    expect(ok).toBe(false);
    expect(errorEl.textContent).toContain("500");
    expect(retryBtn.style.display).toBe("");
    expect(cancelBtn.style.display).toBe("none");
  });

  it("returns false on a network throw", async () => {
    const { _handOverHfToken } = await loadStep3();
    globalThis.fetch = vi.fn(async () => { throw new Error("offline"); });
    const ok = await _handOverHfToken(7777, "hf_x", errorEl, cancelBtn, retryBtn);
    expect(ok).toBe(false);
    expect(errorEl.textContent).toContain("offline");
  });
});

// ---------------------------------------------------------------------------
// B054-C: the structured error code (GATED_NO_TOKEN) is propagated out of the
// SSE frame so step3 can offer a real way out (back to model selection).
// ---------------------------------------------------------------------------
describe("B054 — _handleSseFrame propagates the error code", () => {
  async function loadStep3() {
    vi.resetModules();
    vi.doMock("./main.js", () => ({ goToStep: vi.fn(), state: { lang: "ca" } }));
    vi.doMock("./i18n.js", () => ({ t: (k) => k }));
    installDocShim();
    return import("./step3-download.js");
  }

  it("an error frame surfaces its code (GATED_NO_TOKEN)", async () => {
    const { _handleSseFrame } = await loadStep3();
    const reader = { cancel() {} };
    const frame = 'data: {"type":"error","code":"GATED_NO_TOKEN","message":"gated"}';
    const result = _handleSseFrame(frame, mkEl("progress"), mkEl("p"), reader);
    expect(result.ok).toBe(false);
    // Mutation: drop `code: data.code` from the error branch → undefined.
    expect(result.code).toBe("GATED_NO_TOKEN");
  });
});

// ---------------------------------------------------------------------------
// step3 integration — token handed over BEFORE the download, only when gated
// ---------------------------------------------------------------------------
describe("B054 — step3 delivers the token before the download", () => {
  function fetchSpy(calls) {
    return vi.fn(async (url, opts) => {
      calls.push({ url: String(url), method: opts?.method });
      if (String(url).includes("/installer/hf-token")) {
        return { ok: true, json: async () => ({ ok: true }) };
      }
      // download: a stream that ends immediately so step3 returns fast
      return { ok: true, body: { getReader: () => ({ read: async () => ({ done: true }), cancel() {} }) } };
    });
  }
  async function loadStep3(state) {
    vi.resetModules();
    vi.doMock("./main.js", () => ({ goToStep: vi.fn(), state }));
    vi.doMock("./i18n.js", () => ({ t: (k) => k }));
    installDocShim();
    return import("./step3-download.js");
  }

  it("gated MLX + token → POST /installer/hf-token fires before /installer/download", async () => {
    const state = { lang: "ca", hfToken: "hf_tok", selectedModel: { name: "Gemma", engine: "mlx", model_id: "google/gemma-4-e4b-mlx", gated: true } };
    const { step3 } = await loadStep3(state);
    const calls = [];
    globalThis.fetch = fetchSpy(calls);
    await step3();
    const tokenIdx = calls.findIndex((c) => c.url.includes("/installer/hf-token"));
    const dlIdx = calls.findIndex((c) => c.url.includes("/installer/download"));
    expect(tokenIdx).toBeGreaterThanOrEqual(0);     // it was delivered
    expect(dlIdx).toBeGreaterThan(tokenIdx);         // and strictly before the download
  });

  it("non-gated model → no token call, download goes straight out", async () => {
    const state = { lang: "ca", hfToken: "hf_tok", selectedModel: { name: "Qwen", engine: "mlx", model_id: "Qwen/x", gated: false } };
    const { step3 } = await loadStep3(state);
    const calls = [];
    globalThis.fetch = fetchSpy(calls);
    await step3();
    expect(calls.some((c) => c.url.includes("/installer/hf-token"))).toBe(false);
    expect(calls[0].url).toContain("/installer/download");
  });

  it("gated via Ollama → no token call (Ollama needs none)", async () => {
    const state = { lang: "ca", hfToken: "hf_tok", selectedModel: { name: "Gemma", engine: "ollama", model_id: "gemma4:e4b", gated: true } };
    const { step3 } = await loadStep3(state);
    const calls = [];
    globalThis.fetch = fetchSpy(calls);
    await step3();
    expect(calls.some((c) => c.url.includes("/installer/hf-token"))).toBe(false);
  });
});
