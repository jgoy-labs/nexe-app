// Post-build: calculate SHA-384 of each JS/CSS asset in dist/assets/ and add
// integrity="sha384-..." crossorigin="anonymous" to the corresponding <script>
// and <link> tags in every emitted HTML page.
//
// Run via: node scripts/add-sri-to-dist.js
// Integrated in package.json "build" script: vite build && node scripts/add-sri-to-dist.js
//
// C65: SRI for dist/assets/*.{js,css} — defense in depth against CDN/proxy tampering.
// Note: SRI on same-origin assets is defense-in-depth (not strictly required), but
// ensures that a compromised build artifact is detectable by the browser.
//
// Finding B: the build is now multi-page (index.html + uninstall.html, see
// vite.config.js). We apply SRI to EVERY page — the dedicated uninstall window
// invokes a DESTRUCTIVE command, so its bundle deserves the same integrity
// guarantee as the main app.

import { readFileSync, writeFileSync, readdirSync } from "node:fs";
import { createHash } from "node:crypto";
import { resolve, join, basename } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = fileURLToPath(new URL(".", import.meta.url));
const projectRoot = resolve(__dirname, "..");
const distDir = join(projectRoot, "dist");
const assetsDir = join(distDir, "assets");

// Pages emitted by the multi-page Vite build. Missing pages are skipped
// gracefully (e.g. if the input list ever changes).
const HTML_PAGES = ["index.html", "uninstall.html"];

// Find JS and CSS assets once.
let assets;
try {
  assets = readdirSync(assetsDir)
    .filter((f) => f.endsWith(".js") || f.endsWith(".css"))
    .map((f) => join(assetsDir, f));
} catch {
  console.warn("[sri] dist/assets/ not found — skipping SRI (run vite build first?)");
  process.exit(0);
}

if (assets.length === 0) {
  console.info("[sri] no assets found in dist/assets/ — nothing to do");
  process.exit(0);
}

// B19: Vite already emits `crossorigin` (empty = anonymous-implicit) on <script type="module">
// tags. If we add crossorigin="anonymous" on top, the element ends up with two crossorigin
// attributes. HTML parsers silently ignore the second one (first-wins), so it's semantically
// equivalent, but it's invalid HTML and confusing in audits.
// Fix: strip any existing crossorigin attribute before we inject our own.
function stripExistingCrossorigin(src) {
  // Matches: crossorigin, crossorigin="", crossorigin="anonymous", crossorigin="use-credentials"
  return src.replace(/\s+crossorigin(="[^"]*")?/g, "");
}

// Apply SRI to a single HTML page. Returns the number of tags updated, or -1 if
// the page file does not exist. Throws (exit 1) if the page references assets
// but ended up without any integrity= (silent no-op — e.g. path-separator
// mismatch on Windows), so a broken build fails loud instead of shipping.
function applySriToPage(pageName) {
  const pagePath = join(distDir, pageName);
  let html;
  try {
    html = readFileSync(pagePath, "utf-8");
  } catch {
    return -1; // page not emitted — skip
  }

  let modified = 0;
  for (const assetPath of assets) {
    const content = readFileSync(assetPath);
    const hash = createHash("sha384").update(content).digest("base64");
    const sri = `integrity="sha384-${hash}" crossorigin="anonymous"`;

    // Relative URL as it appears in the HTML: /assets/filename.js
    // basename() is separator-agnostic: on Windows path.join yields backslashes, so
    // split("/assets/") would fail and corrupt relPath — dropping SRI silently (B-win).
    const relPath = "/assets/" + basename(assetPath);

    // Escape special regex chars in the path
    const escapedPath = relPath.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

    // Match <script src="..."> and <link href="..."> that don't already have integrity=
    const scriptRe = new RegExp(
      `(<script[^>]+src="${escapedPath}"(?![^>]*integrity)[^>]*)(>)`,
      "g"
    );
    const linkRe = new RegExp(
      `(<link[^>]+href="${escapedPath}"(?![^>]*integrity)[^>]*)(>)`,
      "g"
    );

    const before = html;
    // B19: strip existing crossorigin before injecting ours (avoids duplicate attribute)
    html = html.replace(scriptRe, (_, tag, close) => `${stripExistingCrossorigin(tag)} ${sri}${close}`);
    html = html.replace(linkRe, (_, tag, close) => `${stripExistingCrossorigin(tag)} ${sri}${close}`);

    if (html !== before) {
      console.info(`[sri] ${pageName}: added integrity to ${relPath}`);
      modified++;
    }
  }

  writeFileSync(pagePath, html);

  // Fail loud: this page references an asset but carries no SRI at all → the
  // injection silently no-op'd. Better to fail the build than ship a page whose
  // frontend lost its integrity attributes. Outcome-based (checks the HTML) so
  // an idempotent re-run over already-tagged HTML still passes.
  const referencesAsset = /(?:src|href)="\/assets\/[^"]+\.(?:js|css)"/.test(html);
  if (referencesAsset && !/integrity="sha384-/.test(html)) {
    console.error(
      `[sri] ERROR: ${pageName} references assets but has no integrity= — SRI not applied. Failing build.`
    );
    process.exit(1);
  }

  return modified;
}

let processedPages = 0;
let totalModified = 0;
for (const page of HTML_PAGES) {
  const result = applySriToPage(page);
  if (result === -1) {
    console.warn(`[sri] ${page} not found in dist — skipped`);
    continue;
  }
  processedPages++;
  totalModified += result;
}

if (processedPages === 0) {
  console.warn("[sri] no HTML pages found in dist — nothing to do");
  process.exit(0);
}

if (totalModified > 0) {
  console.info(`[sri] SRI added to ${totalModified} tag(s) across ${processedPages} page(s)`);
} else {
  console.info("[sri] no tags updated (assets already have integrity)");
}
