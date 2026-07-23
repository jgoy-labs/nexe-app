import { defineConfig } from "vite";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

// ESM has no __dirname; derive it from import.meta.url. Points at the project
// root (where this config lives), so the multi-page HTML inputs below resolve
// regardless of the process cwd.
const __dirname = dirname(fileURLToPath(import.meta.url));

const host = process.env.TAURI_DEV_HOST;

// Configurable port via VITE_PORT to prevent collision in multi-fork dev
const port = parseInt(process.env.VITE_PORT || "1420", 10);

// Vite config for nexe-app — scaffold with `src/` as root, `dist/` as output.
// Tauri integration: https://v2.tauri.app/start/frontend/vite/
export default defineConfig(() => ({
  root: "src",
  // Phase 1: public/ holds the server-nexe web UI (static assets copied as-is).
  // Vite copies publicDir into dist/ without processing — UI assets stay verbatim.
  publicDir: "../public",

  build: {
    outDir: "../dist",
    emptyOutDir: true,
    // Targets compatible with WKWebView (macOS Safari 15+) and WebKitGTK.
    // Tauri 2.10 requires macOS 13+ (Safari 16) and WebKitGTK 4.1 (~Safari 15).
    target: process.env.TAURI_ENV_PLATFORM === "windows" ? "chrome105" : "safari15",
    // Force esbuild to avoid @rolldown/* prerelease (unaudited CI binary).
    minify: process.env.TAURI_ENV_DEBUG ? false : "esbuild",
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
    // C65: SRI via post-build script (scripts/add-sri-to-dist.js).
    // Integrated in package.json "build": "vite build && node scripts/add-sri-to-dist.js"

    // Multi-page (Finding B): the splash/onboarding app (index.html) PLUS the
    // dedicated uninstall dialog window (uninstall.html, opened by the tray).
    // Both are HTML entries so Vite emits dist/index.html + dist/uninstall.html
    // with a shared assets/ chunk graph. Paths are absolute (root is "src", so a
    // bare "index.html" would still work, but absolute is unambiguous).
    rollupOptions: {
      input: {
        main: resolve(__dirname, "src/index.html"),
        uninstall: resolve(__dirname, "src/uninstall.html"),
      },
    },
  },

  clearScreen: false,

  server: {
    // Port via env VITE_PORT (default 1420) — avoids collision in multi-fork
    port,
    strictPort: true,
    host: host || false,
    hmr: host
      ? { protocol: "ws", host, port: port + 1 }
      : undefined,
    watch: {
      // Do not rebuild when Rust changes.
      ignored: ["**/src-tauri/**"],
    },
  },

  // vitest config (JS unit tests) — absolute root to the project, not Vite's `root: "src"`
  test: {
    environment: "node",
    root: process.cwd(),
    include: ["src/**/*.test.js", "isolation-frame/**/*.test.js"],
  },
}));
