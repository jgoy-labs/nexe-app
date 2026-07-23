// Bootstrap for the dedicated uninstall window (uninstall.html) — Finding B.
//
// This page IS the uninstall dialog: the tray "Uninstall…" item opens a Tauri
// window (label "uninstall") that loads uninstall.html, and here we render the
// modal immediately at DOMContentLoaded. No Tauri event is involved (the old
// `open-uninstall-dialog` bridge was dead once the main webview navigated to the
// sidecar HTTP origin), so there is nothing to wait for.

import { initUninstallWindow } from "./uninstall.js";

window.addEventListener("DOMContentLoaded", () => {
  initUninstallWindow(document);
});
