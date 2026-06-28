// Bloc B (frontend) regression tests — MC-036, MC-037, MC-038.
// Node vitest environment with manual DOM shims (matches the repo's
// onboarding.test.js / main.test.js convention — no jsdom).

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";

// Global (hoisted) Tauri mocks shared by every test in this file.
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => null) }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn(async () => "/tmp") }));

// Minimal element shim: enough for the splash/dropdown DOM these fixes touch.
function mkEl(tag) {
  const el = {
    tagName: String(tag).toUpperCase(),
    className: "",
    id: "",
    textContent: "",
    value: 0,
    max: 100,
    style: { cssText: "", display: "" },
    children: [],
    _listeners: {},
    appendChild(c) { el.children.push(c); return c; },
    replaceChildren(...a) { el.children = [...a]; },
    setAttribute(k, v) { el[k] = v; },
    getAttribute(k) { return el[k] ?? null; },
    addEventListener(ev, fn) { el._listeners[ev] = fn; },
    contains() { return false; },
    remove() {},
    appendTo() {},
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
    addEventListener: () => {},
    head: { appendChild: () => {} },
    body: { appendChild: () => {}, replaceChildren: () => {}, children: [] },
  };
  globalThis.AbortController = AbortController; // native (Node 16+)
}

afterEach(() => {
  vi.resetModules();
  vi.useRealTimers();
  vi.clearAllMocks();
});

// ---------------------------------------------------------------------------
// MC-038 — step0 splash: absolute timeout + idempotent settle
// ---------------------------------------------------------------------------
describe("MC-038 — step0 splash", () => {
  let goToStep, listenCb, unlisten;

  async function loadStep0() {
    vi.resetModules();
    goToStep = vi.fn();
    unlisten = vi.fn();
    listenCb = null;
    vi.doMock("./main.js", () => ({ goToStep, state: { lang: "ca" } }));
    vi.doMock("./i18n.js", () => ({ t: () => "extracting" }));
    vi.doMock("@tauri-apps/api/event", () => ({
      listen: vi.fn(async (_ev, cb) => { listenCb = cb; return unlisten; }),
    }));
    installDocShim();
    return (await import("./step0-splash.js")).step0;
  }

  it("advances via the absolute ceiling when progress stalls below 100%", async () => {
    vi.useFakeTimers();
    const step0 = await loadStep0();
    await step0();

    // progress starts but stalls at 50% → received=true, never reaches 100
    listenCb({ payload: { percent: 50 } });

    // the 600ms quick timer must NOT fire (received === true)
    vi.advanceTimersByTime(600);
    expect(goToStep).not.toHaveBeenCalled();

    // the absolute ceiling rescues the hung splash
    vi.advanceTimersByTime(120000);
    expect(goToStep).toHaveBeenCalledWith(1);
    expect(unlisten).toHaveBeenCalled();
  });

  it("advances after 600ms when no progress event ever arrives", async () => {
    vi.useFakeTimers();
    const step0 = await loadStep0();
    await step0();
    vi.advanceTimersByTime(600);
    expect(goToStep).toHaveBeenCalledWith(1);
  });

  it("settle is idempotent: 100% then the timers cannot re-fire goToStep", async () => {
    vi.useFakeTimers();
    const step0 = await loadStep0();
    await step0();
    listenCb({ payload: { percent: 100 } });
    expect(goToStep).toHaveBeenCalledTimes(1);
    vi.advanceTimersByTime(120000);
    expect(goToStep).toHaveBeenCalledTimes(1);
  });
});

// ---------------------------------------------------------------------------
// MC-036 — dropdown: outside-click listener must not accumulate across renders
// ---------------------------------------------------------------------------
describe("MC-036 — dropdown outside-click listener", () => {
  async function loadStep2() {
    vi.resetModules();
    vi.doMock("./main.js", () => ({
      goToStep: vi.fn(), saveState: vi.fn(), state: { lang: "ca", selectedModel: null },
    }));
    vi.doMock("./i18n.js", () => ({ t: (k) => k }));
    vi.doMock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
    installDocShim();
    return import("./step2-hardware.js");
  }

  it("installs the outside-click listener once across many renders (no stacking)", async () => {
    const { _buildCustomDropdown, _resetOutsideClickForTest } = await loadStep2();
    // installDocShim ran inside loadStep2 — now spy on the global document.
    let docClicks = 0;
    globalThis.document.addEventListener = (ev) => { if (ev === "click") docClicks += 1; };
    globalThis.document.querySelectorAll = () => [];
    _resetOutsideClickForTest();

    _buildCustomDropdown([]);
    _buildCustomDropdown([]);
    _buildCustomDropdown([]);

    // a single delegated listener, regardless of how many dropdowns rendered
    expect(docClicks).toBe(1);
  });
});

// ---------------------------------------------------------------------------
// MC-037 — showError must surface even when the splash body was cleared
// ---------------------------------------------------------------------------
describe("MC-037 — showError robustness", () => {
  // showError lives in the app shell src/main.js (i.e. ../main.js from here),
  // NOT the onboarding wizard src/onboarding/main.js that the suites above mock.
  beforeEach(() => { vi.resetModules(); });

  it("rebuilds #splash-error when it no longer exists", async () => {
    const appended = [];
    globalThis.window = { addEventListener: () => {} };
    globalThis.document = {
      querySelector: () => null, // #splash-error and .spinner both absent
      createElement: (t) => mkEl(t),
      body: { appendChild: (el) => { appended.push(el); return el; } },
    };
    const mod = await import("../main.js");
    mod.showError("boom");

    expect(appended).toHaveLength(1);
    expect(appended[0].id).toBe("splash-error");
    expect(appended[0].textContent).toBe("boom");
    expect(appended[0].style.display).toBe("block");
  });

  it("reuses an existing #splash-error without creating a duplicate", async () => {
    const existing = mkEl("div");
    existing.id = "splash-error";
    const appended = [];
    globalThis.window = { addEventListener: () => {} };
    globalThis.document = {
      querySelector: (sel) => (sel === "#splash-error" ? existing : null),
      createElement: (t) => mkEl(t),
      body: { appendChild: (el) => { appended.push(el); return el; } },
    };
    const mod = await import("../main.js");
    mod.showError("again");

    expect(appended).toHaveLength(0);
    expect(existing.textContent).toBe("again");
  });
});
