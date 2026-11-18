#!/usr/bin/env node
// Heuristic audit of an app's own source for patterns The Mechanics rejects.
// Usage: node audit.mjs <dir> [<dir>…]
// Prints file:line  [rule]  snippet. Findings are leads to inspect, not
// verdicts (a radius on an avatar image can be fine). Exit code is always 0.
// Skips node_modules, dist/build output, VCS folders and the vendored
// The Mechanics CSS itself.
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, extname, basename } from "node:path";

const EXT = new Set([".css", ".scss", ".sass", ".less", ".html", ".htm", ".js", ".mjs", ".jsx", ".ts", ".tsx", ".svelte", ".vue", ".astro", ".rs", ".jinja", ".j2", ".hbs", ".erb", ".twig"]);
const SKIP_DIR = new Set(["node_modules", ".git", "dist", "build", "target", ".next", ".svelte-kit", "coverage", ".cache", "vendor"]);
const SKIP_FILE = /the-mechanics(\.daisyui)?(\.min)?\.(css|js)$|^fonts\.css$/;

const px = (v, unit) => (unit === "rem" || unit === "em" ? parseFloat(v) * 16 : parseFloat(v));
const RULES = [
  {
    id: "radius>3px",
    why: "radius ≤ 3px; no rounded cards or pills",
    test: (l) => {
      const m = l.match(/border-radius\s*:\s*([^;]+)/i);
      if (m) {
        if (/var\(--tm-/.test(m[1])) return false;
        if (/9999|50%|100%/.test(m[1])) return true;
        return [...m[1].matchAll(/([\d.]+)(px|rem|em)/g)].some(([, v, u]) => px(v, u) > 3);
      }
      return /\brounded-(md|lg|xl|2xl|3xl|full)\b/.test(l) || /\bd?-?rounded-(box|field|selector)\b/.test(l);
    },
  },
  {
    id: "soft-shadow",
    why: "no blurred shadows; only hard offset shadows on floating objects and the focus lift",
    test: (l) => {
      const m = l.match(/box-shadow\s*:\s*([^;]+)/i);
      if (m) {
        if (/var\(--tm-|none|inset/.test(m[1])) return false;
        return [...m[1].matchAll(/(-?[\d.]+)(?:px)?\s+(-?[\d.]+)(?:px)?\s+([\d.]+)px/g)].some(([, , , blur]) => parseFloat(blur) > 0);
      }
      return /\bshadow-(sm|md|lg|xl|2xl)\b/.test(l) || /\bdrop-shadow\b/.test(l);
    },
  },
  {
    id: "side-bar",
    why: "no colored side bars (left/right accent strips)",
    test: (l) => /border-(left|right|inline-start)\s*:\s*([3-9]|\d{2})px\s+solid/i.test(l) || /border-(left|inline-start)-width\s*:\s*([3-9]|\d{2})px/i.test(l) || /\bborder-l-(4|8)\b/.test(l),
  },
  {
    id: "focus-ring",
    why: "focus is the ink lift (--tm-focus-lift), never a colored ring",
    test: (l) => (/:focus/.test(l) && /outline\s*:[^;]*(#|rgb|blue|teal)/i.test(l)) || /\bring-(1|2|4|offset)?\b|\bfocus:ring\b|\bfocus-visible:ring\b/.test(l),
  },
  {
    id: "hard-color",
    why: "colors come from --tm-* tokens so all three themes work",
    test: (l, ext) => [".css", ".scss", ".sass", ".less", ".svelte", ".vue"].includes(ext) && /(^|[\s:,(])(#[0-9a-f]{3,8}\b|rgba?\([\d\s,./%]+\)|hsla?\()/i.test(l) && !/--[\w-]+\s*:/.test(l),
  },
  {
    id: "relabel-toggle",
    why: "toggles keep a fixed label; show state with aria-pressed + fill",
    // aria-label-only changes (icon buttons) do not move anything on screen
    // view toggles only; record actions ("Enable"/"Disable") may name what they do
    test: (l) => !/aria-label/.test(l) && /["'`](Show|Hide|Expand|Collapse)\b[^"'`]*["'`]\s*:\s*["'`](Show|Hide|Expand|Collapse)\b/.test(l),
  },
  {
    id: "browser-dialog",
    why: "use <dialog class=\"tm-dialog\"> (danger confirm for destructive actions), not browser popups",
    test: (l, ext) => [".js", ".mjs", ".jsx", ".ts", ".tsx", ".svelte", ".vue", ".html", ".htm"].includes(ext) && /(^|[^.\w])(window\.)?(confirm|alert|prompt)\s*\(/.test(l) && !/function\s+(confirm|alert|prompt)\b/.test(l),
  },
  {
    id: "native-title",
    why: "tooltips are .tm-tip (one look, keyboard reachable), not the browser title popup",
    test: (l) => !/document\.title|<title>|\.tm-tip/.test(l) && (/(^|[\s<])title\s*=\s*["'`{$]/.test(l) || /\.title\s*=\s*[^=]/.test(l) || /setAttribute\(\s*["']title["']/.test(l)),
  },
  {
    id: "own-helper-copy",
    why: "a copy of a The Mechanics behavior: load the-mechanics.js instead, so fixes arrive with the installer",
    test: (l) => /function\s+(enhanceSelects|enhanceDateTime|followAnchor|trackInputModality|enhanceTips|enhanceClearable|anchorPopover|wireMenus|confirmDialog)\b/.test(l),
  },
  {
    id: "important",
    why: "app CSS outside layers already wins; !important usually hides a structure problem",
    test: (l, ext) => [".css", ".scss", ".less"].includes(ext) && /!important/.test(l),
  },
];

function* walk(dir) {
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    let s;
    try { s = statSync(p); } catch { continue; }
    if (s.isDirectory()) { if (!SKIP_DIR.has(name) && !name.startsWith(".")) yield* walk(p); }
    else if (EXT.has(extname(name)) && !SKIP_FILE.test(basename(name)) && s.size < 2_000_000) yield p;
  }
}

const dirs = process.argv.slice(2);
if (!dirs.length) { console.error("usage: node audit.mjs <dir> [<dir>…]"); process.exit(0); }
const counts = {};
let files = 0;
// Whole-project check: date/time fields or selects but the behaviors module is never loaded.
const needsJs = [];
let loadsJs = false;
for (const d of dirs) {
  for (const f of walk(d)) {
    files++;
    const ext = extname(f);
    readFileSync(f, "utf8").split("\n").forEach((line, i) => {
      if (/the-mechanics(\.min)?\.js/.test(line)) loadsJs = true;
      if (/type\s*=\s*["'](date|time|datetime-local)["']|class\s*=\s*["'][^"']*\btm-select\b/.test(line)) needsJs.push(`${f}:${i + 1}`);
      if (line.length > 400) return; // minified
      if (/^\s*(\/\/|\/?\*|<!--)/.test(line)) return; // comments
      for (const r of RULES) {
        if (r.test(line, ext)) {
          counts[r.id] = (counts[r.id] || 0) + 1;
          console.log(`${f}:${i + 1}  [${r.id}]  ${line.trim().slice(0, 140)}`);
        }
      }
    });
  }
}
if (needsJs.length && !loadsJs) {
  counts["no-behaviors-js"] = needsJs.length;
  for (const at of needsJs.slice(0, 10)) console.log(`${at}  [no-behaviors-js]  date/time field or .tm-select, but the-mechanics.js is never loaded`);
}
console.log(`\n${files} files scanned.`);
for (const r of RULES) if (counts[r.id]) console.log(`  ${r.id}: ${counts[r.id]} — ${r.why}`);
if (counts["no-behaviors-js"]) console.log(`  no-behaviors-js: ${counts["no-behaviors-js"]} — without the-mechanics.js these open the browser's own calendar/clock/list; load it next to the CSS`);
if (!Object.keys(counts).length) console.log("  no findings");
