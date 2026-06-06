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
