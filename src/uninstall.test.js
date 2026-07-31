// Uninstall modal — findings 830 / 836.
//
// 830: the modal was ONE flat list of data checkboxes, and "Erase everything
// and quit" left the application installed. The modal now asks two separate
// questions ("erase my data" / "uninstall nexe"), and the select-all button
// must honour BOTH — that is the regression this file locks down.
//
// Node vitest environment with manual DOM shims (the repo convention — no
// jsdom; see onboarding.test.js / blocb-frontend.test.js).

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";

const invoke = vi.fn(async () => ({ failures: [], exited: true }));
const close = vi.fn(async () => {});
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a) => invoke(...a) }));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ close: () => close() }),
}));

// Minimal element shim with a working `querySelector("#id")` — the modal reads
// its own checkboxes back through it.
function mkEl(tag) {
  const el = {
    tagName: String(tag).toUpperCase(),
    id: "",
    className: "",
    type: "",
    textContent: "",
    checked: false,
    disabled: false,
    children: [],
    _listeners: {},
    appendChild(c) {
      el.children.push(c);
      return c;
    },
    addEventListener(ev, fn) {
      el._listeners[ev] = fn;
    },
    click() {
      if (el._listeners.click) el._listeners.click();
    },
    querySelector(sel) {
      const id = sel.startsWith("#") ? sel.slice(1) : null;
      if (!id) return null;
      const walk = (node) => {
        for (const c of node.children) {
          if (c.id === id) return c;
          const found = walk(c);
          if (found) return found;
        }
        return null;
      };
      return walk(el);
    },
  };
  return el;
}

const doc = { createElement: (t) => mkEl(t), body: mkEl("body") };

/** Depth-first list of every text rendered in the overlay. */
function textsOf(node, acc = []) {
  if (node.textContent) acc.push(node.textContent);
  for (const c of node.children) textsOf(c, acc);
  return acc;
}

let mod;
beforeEach(async () => {
  vi.resetModules();
  invoke.mockClear();
  close.mockClear();
  // `navigator` is a getter-only global in Node — stub it, never assign.
  vi.stubGlobal("navigator", { language: "ca-ES" });
  vi.stubGlobal("localStorage", { getItem: () => null, setItem: () => {} });
  mod = await import("./uninstall.js");
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.clearAllMocks();
});

function build() {
  const onConfirm = vi.fn();
  const onCancel = vi.fn();
  const overlay = mod.buildUninstallModal(doc, "ca", { onCancel, onConfirm });
  return { overlay, onConfirm, onCancel };
}

/** Find a rendered button by its label text. */
function buttonWithText(overlay, text) {
  const walk = (node) => {
    for (const c of node.children) {
      if (c.tagName === "BUTTON" && c.textContent === text) return c;
      const found = walk(c);
      if (found) return found;
    }
    return null;
  };
  return walk(overlay);
}

describe("uninstall modal — two independent blocks (830)", () => {
  it("renders both section headings and the app checkbox", () => {
    const { overlay } = build();
    const texts = textsOf(overlay);
    expect(texts).toContain("Esborra les meves dades");
    expect(texts).toContain("Desinstal·la nexe");
    expect(overlay.querySelector("#uninstall-opt-app")).not.toBeNull();
    // …alongside every data checkbox, untouched.
    for (const id of [
      "uninstall-opt-models",
      "uninstall-opt-conversations",
      "uninstall-opt-library",
      "uninstall-opt-ollama",
      "uninstall-opt-embeddings",
    ]) {
      expect(overlay.querySelector(`#${id}`)).not.toBeNull();
    }
  });

  it("says the data wipe leaves the app installed", () => {
    const { overlay } = build();
    const texts = textsOf(overlay).join(" ");
    expect(texts).toContain("L'app segueix instal·lada i utilitzable");
    expect(texts).toContain("NO desinstal·la l'aplicació");
  });

  it("collects the app flag in the payload Rust expects", () => {
    const { overlay } = build();
    expect(mod.collectOpts(overlay)).toEqual({
      models: false,
      conversations: false,
      library: false,
      ollama: false,
      embeddings_cache: false,
      uninstall_app: false,
    });
    overlay.querySelector("#uninstall-opt-app").checked = true;
    overlay.querySelector("#uninstall-opt-conversations").checked = true;
    const opts = mod.collectOpts(overlay);
    expect(opts.uninstall_app).toBe(true);
    expect(opts.conversations).toBe(true);
    expect(opts.library).toBe(false);
  });

  it("separates 'any data' from 'anything at all'", () => {
    const appOnly = { uninstall_app: true };
    expect(mod.hasDataSelection(appOnly)).toBe(false);
    expect(mod.hasSelection(appOnly)).toBe(true);
    const dataOnly = { models: true };
    expect(mod.hasDataSelection(dataOnly)).toBe(true);
    expect(mod.hasSelection(dataOnly)).toBe(true);
    expect(mod.hasSelection({})).toBe(false);
  });

  it("'erase everything' ticks the app box too, then submits", () => {
    // The exact 830 failure: the button promised everything and left the
    // application installed because it only ticked the data boxes.
    const { overlay, onConfirm } = build();
    buttonWithText(overlay, "Esborra-ho tot i desinstal·la").click();
    expect(mod.collectOpts(overlay)).toEqual({
      models: true,
      conversations: true,
      library: true,
      ollama: true,
      embeddings_cache: true,
      uninstall_app: true,
    });
    expect(onConfirm).toHaveBeenCalledTimes(1);
  });

  it("cancel closes the dedicated window", () => {
    const { overlay, onCancel } = build();
    buttonWithText(overlay, "Cancel·la").click();
    expect(onCancel).toHaveBeenCalledTimes(1);
  });
});

describe("submitUninstall", () => {
  function refs() {
    return {
      status: mkEl("p"),
      confirmBtn: mkEl("button"),
      cancelBtn: mkEl("button"),
    };
  }

  it("refuses an empty selection without calling Rust", async () => {
    const { overlay } = build();
    const r = refs();
    await mod.submitUninstall(overlay, r, "ca");
    expect(invoke).not.toHaveBeenCalled();
    expect(r.status.textContent).toBe("Selecciona almenys una opció.");
  });

  it("passes the app-only selection through to the command", async () => {
    const { overlay } = build();
    overlay.querySelector("#uninstall-opt-app").checked = true;
    const r = refs();
    await mod.submitUninstall(overlay, r, "ca");
    expect(invoke).toHaveBeenCalledWith("uninstall_with_options", {
      opts: {
        models: false,
        conversations: false,
        library: false,
        ollama: false,
        embeddings_cache: false,
        uninstall_app: true,
      },
    });
    expect(r.confirmBtn.disabled).toBe(true);
  });

  it("re-enables the buttons when the native gates are cancelled", async () => {
    invoke.mockResolvedValueOnce({ failures: [], exited: false });
    const { overlay } = build();
    overlay.querySelector("#uninstall-opt-models").checked = true;
    const r = refs();
    await mod.submitUninstall(overlay, r, "ca");
    expect(r.confirmBtn.disabled).toBe(false);
    expect(r.cancelBtn.disabled).toBe(false);
    expect(r.status.textContent).toBe("");
  });

  it("surfaces a command error instead of hanging on 'closing…'", async () => {
    invoke.mockRejectedValueOnce(new Error("ipc down"));
    const { overlay } = build();
    overlay.querySelector("#uninstall-opt-library").checked = true;
    const r = refs();
    await mod.submitUninstall(overlay, r, "ca");
    expect(r.status.textContent).toContain("ipc down");
    expect(r.confirmBtn.disabled).toBe(false);
  });
});

describe("pickLang", () => {
  it("prefers the saved onboarding language", () => {
    vi.stubGlobal("localStorage", { getItem: () => JSON.stringify({ lang: "es" }) });
    expect(mod.pickLang()).toBe("es");
  });

  it("falls back to the navigator language", () => {
    vi.stubGlobal("localStorage", { getItem: () => null });
    vi.stubGlobal("navigator", { language: "en-GB" });
    expect(mod.pickLang()).toBe("en");
  });

  it("survives an unusable localStorage", () => {
    vi.stubGlobal("localStorage", {
      getItem: () => {
        throw new Error("blocked");
      },
    });
    vi.stubGlobal("navigator", { language: "ca-ES" });
    expect(mod.pickLang()).toBe("ca");
  });
});
