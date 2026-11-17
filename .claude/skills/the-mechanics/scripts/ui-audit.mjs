#!/usr/bin/env node
// Rendered-page audit: finds what still looks like the browser, not like The
// Mechanics. Opens each URL in every theme (Playwright/Chromium), walks the
// visible elements and reports leaks. Findings are leads, not verdicts.
//
//   node ui-audit.mjs <url> [<url>…] [--themes=light,dark,coffee] [--width=1440]
//                     [--json=report.json] [--theme-attr=data-tm-theme]
//
// Needs the `playwright` package resolvable from where you run it (and its
// Chromium). Set the theme by attribute on <html>; pages that keep the theme
// elsewhere can pass --theme-attr. Exit code is always 0.
import { chromium } from "playwright";
import { writeFileSync } from "node:fs";

const args = process.argv.slice(2);
const opt = (k, d) => args.find((a) => a.startsWith(`--${k}=`))?.split("=").slice(1).join("=") ?? d;
const urls = args.filter((a) => !a.startsWith("--"));
if (!urls.length) { console.error("usage: node ui-audit.mjs <url> [<url>…] [--themes=light,dark,coffee] [--width=1440] [--json=out.json]"); process.exit(0); }
const themes = opt("themes", "light,dark,coffee").split(",");
const width = +opt("width", "1440");
const themeAttr = opt("theme-attr", "data-tm-theme");

// Runs in the page: returns findings for the current theme.
function scan() {
  const out = [];
  const where = (el) => {
    const id = el.id ? `#${el.id}` : "";
    const cls = [...el.classList].slice(0, 3).map((c) => `.${c}`).join("");
    const text = (el.textContent || "").trim().replace(/\s+/g, " ").slice(0, 40);
    return `${el.tagName.toLowerCase()}${id}${cls}${text ? ` "${text}"` : ""}`;
  };
  const visible = (el) => {
    const r = el.getBoundingClientRect();
    if (!r.width || !r.height) return false;
    const s = getComputedStyle(el);
    return s.visibility !== "hidden" && s.display !== "none" && +s.opacity > 0.05;
  };
  const add = (rule, el, detail) => out.push({ rule, el: where(el), detail });
  // Token palette of the active theme (every --tm-* color on :root, resolved).
  const probe = document.createElement("span");
  document.body.append(probe);
  const rootStyle = getComputedStyle(document.documentElement);
  const palette = [];
  for (const sheet of document.styleSheets) {
    let rules; try { rules = sheet.cssRules; } catch { continue; }
    const walk = (list) => { for (const r of list) { if (r.cssRules) walk(r.cssRules); if (r.style) for (const p of r.style) if (p.startsWith("--tm-") && !palette.includes(p)) palette.push(p); } };
    walk(rules);
  }
  const rgb = (v) => { probe.style.color = ""; probe.style.color = v; const m = getComputedStyle(probe).color.match(/[\d.]+/g); return m ? m.slice(0, 3).map(Number) : null; };
  const tokenColors = palette.map((p) => rootStyle.getPropertyValue(p).trim()).filter((v) => v && !/^[\d.]+(px|rem|em|ms|%)?$/.test(v)).map(rgb).filter(Boolean);
  probe.remove();
  const near = (c) => tokenColors.some((t) => Math.abs(t[0] - c[0]) + Math.abs(t[1] - c[1]) + Math.abs(t[2] - c[2]) <= 24);
  const parse = (v) => { const m = v.match(/rgba?\(([^)]+)\)/); if (!m) return null; const p = m[1].split(/[ ,/]+/).filter(Boolean).map(Number); return p.length === 4 && p[3] === 0 ? null : p.slice(0, 3); };
  const offColors = new Map();

  // Unstyled baseline: the same tag/type in an empty same-origin iframe.
  const frame = document.createElement("iframe");
  frame.style.cssText = "position:absolute;width:0;height:0;border:0;visibility:hidden";
  document.body.append(frame);
  const uaCache = new Map();
  const uaStyle = (el) => {
    const key = `${el.tagName}:${el.type || ""}`;
    if (!uaCache.has(key)) {
      const d = frame.contentDocument;
      const x = d.createElement(el.tagName);
      if (el.type) try { x.type = el.type; } catch {}
      d.body.append(x);
      const cs = frame.contentWindow.getComputedStyle(x);
      uaCache.set(key, Object.fromEntries(["backgroundColor", "borderTopStyle", "borderTopColor", "borderTopWidth", "fontFamily"].map((k) => [k, cs[k]])));
    }
    return uaCache.get(key);
  };
  const NATIVE = new Set(["BUTTON", "INPUT", "SELECT", "TEXTAREA", "PROGRESS", "METER", "DIALOG", "DETAILS", "TABLE", "HR"]);
  for (const el of document.querySelectorAll("body *")) {
    if (el.closest("svg") && el.tagName !== "svg") continue;
    if (!visible(el)) continue;
    const s = getComputedStyle(el);
    const r = el.getBoundingClientRect();
    // 1. radius > 3px (circles — equal sides, ≥ 50 % — are allowed: radios, marks, spinners)
    const rad = Math.max(...["borderTopLeftRadius", "borderTopRightRadius", "borderBottomLeftRadius", "borderBottomRightRadius"].map((k) => parseFloat(s[k]) || 0));
    const circle = Math.abs(r.width - r.height) < 2 && rad >= r.width / 2 - 1;
    if (rad > 3 && !circle) add("radius", el, `${rad}px`);
    // 2. soft (blurred) shadows
    // (each shadow layer: skip fully transparent ones)
    for (const layer of s.boxShadow.split(/,(?![^(]*\))/)) {
      const m = layer.match(/(-?[\d.]+)px (-?[\d.]+)px ([\d.]+)px/);
      const alpha = layer.match(/\/\s*([\d.]+)\)|rgba\([^)]*,\s*([\d.]+)\)/);
      const a = alpha ? +(alpha[1] ?? alpha[2]) : 1;
      if (m && +m[3] > 0 && a > 0.01 && !/inset/.test(layer)) { add("soft-shadow", el, layer.trim()); break; }
    }
    // 3. font family of elements that render their own text
    const ownText = [...el.childNodes].some((n) => n.nodeType === 3 && n.textContent.trim());
    if (ownText && !/IBM Plex/i.test(s.fontFamily)) add("font", el, s.fontFamily.split(",")[0]);
    // 4. colors not from the theme tokens (grouped below)
    for (const [prop, val] of [["color", ownText ? s.color : ""], ["background", s.backgroundColor], ["border", parseFloat(s.borderTopWidth) ? s.borderTopColor : ""]]) {
      const c = val && parse(val);
      if (c && !near(c)) { const k = `${prop} rgb(${c.join(",")})`; if (!offColors.has(k)) offColors.set(k, { n: 0, el: where(el) }); offColors.get(k).n++; }
    }
    // 5. controls that still draw the browser's own look: background, border
    //    and font identical to an unstyled element of the same kind.
    if (NATIVE.has(el.tagName) && !["TABLE", "DETAILS", "HR", "DIALOG"].includes(el.tagName) && !(el.tagName === "INPUT" && (el.type === "hidden" || +s.opacity < 0.05))) {
      const ua = uaStyle(el);
      const same = ["backgroundColor", "borderTopStyle", "borderTopColor", "borderTopWidth", "fontFamily"].every((k) => s[k] === ua[k]);
      if (same) add("native-control", el, `<${el.tagName.toLowerCase()}${el.type ? ` type=${el.type}` : ""}> keeps the browser default look`);
    }
    // OS-drawn widgets: their look ignores colors and borders, only appearance: none frees them.
    if ((["PROGRESS", "METER"].includes(el.tagName) || (el.tagName === "INPUT" && ["checkbox", "radio", "range"].includes(el.type))) && s.appearance !== "none" && +s.opacity > 0.05)
      add("native-control", el, `<${el.tagName.toLowerCase()}${el.type ? ` type=${el.type}` : ""}> is drawn by the OS (appearance: ${s.appearance})`);
    if (el.tagName === "SUMMARY" && s.display === "list-item") add("native-control", el, "summary shows the browser disclosure triangle");
    if (el.tagName === "DIALOG" && !el.className) add("native-control", el, "dialog without a class keeps the browser frame and padding");
    // 6. browser tooltip
    if (el.hasAttribute("title") && el.tagName !== "svg") add("native-title", el, `title="${el.getAttribute("title").slice(0, 50)}"`);
  }
  // 7. controls whose popup is drawn by the system
  for (const el of document.querySelectorAll('input[type="date"], input[type="time"], input[type="datetime-local"], input[type="month"], input[type="week"]')) if (!el.closest(".tm-picker-field")) add("system-popup", el, `<input type=${el.type}> without the .tm-picker replacement`);
  for (const el of document.querySelectorAll('input[type="color"], input[type="file"]')) if (!el.closest(".tm-dropzone")) add("system-popup", el, `<input type=${el.type}> opens a system dialog`);
  const baseSelect = CSS.supports("appearance", "base-select");
  for (const el of document.querySelectorAll("select")) if (!baseSelect && !el.dataset.tmEnhanced) add("system-popup", el, "select without the styled list fallback (Firefox shows its own list)");
  for (const f of document.querySelectorAll("form:not([novalidate])")) if (f.querySelector("[required], [pattern], [min], [max], [minlength], [maxlength], input[type=email], input[type=url]")) add("native-validation", f, "form validates with browser bubbles (add novalidate + inline .tm-field__error)");
  frame.remove();
  for (const [k, v] of offColors) out.push({ rule: "off-token-color", el: v.el, detail: `${k} ×${v.n}` });
  return out;
}

const browser = await chromium.launch();
const report = [];
for (const url of urls) {
  for (const theme of themes) {
    const ctx = await browser.newContext({ viewport: { width, height: 900 } });
    await ctx.addInitScript(([attr, t]) => {
      try { localStorage.setItem("tm-theme", t); localStorage.setItem("theme", t); } catch {}
      // Record system popups and validation bubbles the page triggers.
      window.__tmAudit = [];
      for (const fn of ["alert", "confirm", "prompt"]) { const orig = window[fn]; window[fn] = (...a) => { window.__tmAudit.push(`${fn}(${String(a[0] ?? "").slice(0, 50)})`); return fn === "confirm" ? false : fn === "prompt" ? null : orig ? undefined : undefined; }; }
      for (const C of [HTMLFormElement, HTMLInputElement, HTMLSelectElement, HTMLTextAreaElement]) { const o = C.prototype.reportValidity; C.prototype.reportValidity = function () { window.__tmAudit.push(`reportValidity on <${this.tagName.toLowerCase()}>`); return o.call(this); }; }
      document.addEventListener("DOMContentLoaded", () => document.documentElement.setAttribute(attr, t));
    }, [themeAttr, theme]);
    const page = await ctx.newPage();
    await page.goto(url, { waitUntil: "networkidle" }).catch(() => {});
    await page.evaluate(([attr, t]) => document.documentElement.setAttribute(attr, t), [themeAttr, theme]);
    await page.waitForTimeout(600);
    const found = await page.evaluate(scan);
    const calls = await page.evaluate(() => window.__tmAudit || []);
    for (const c of calls) found.push({ rule: "system-dialog", el: "window", detail: c });
    for (const f of found) report.push({ url, theme, ...f });
    await ctx.close();
  }
}
await browser.close();

// Print grouped: rule → unique (element, detail) with the themes it appears in.
const groups = new Map();
for (const f of report) {
  const key = `${f.rule}\u0000${f.url}\u0000${f.el}\u0000${f.detail}`;
  if (!groups.has(key)) groups.set(key, { ...f, themes: [] });
  groups.get(key).themes.push(f.theme);
}
const byRule = {};
for (const g of groups.values()) (byRule[g.rule] ||= []).push(g);
const WHY = {
  radius: "radius ≤ 3px (circles allowed)",
  "soft-shadow": "only hard offset shadows (floating objects, focus lift)",
  font: "IBM Plex Sans/Mono only",
  "off-token-color": "every color from --tm-* tokens (or the app's documented identity colors)",
  "native-control": "native control without a tm- class keeps the browser look",
  "native-title": "browser title popup — use .tm-tip / data-tm-tip",
  "system-popup": "popup drawn by the system — use the tm replacement",
  "native-validation": "browser validation bubbles — validate inline",
  "system-dialog": "alert/confirm/prompt/reportValidity — use tm-dialog / inline errors",
};
for (const [rule, list] of Object.entries(byRule)) {
  console.log(`\n[${rule}] ${list.length} — ${WHY[rule] || ""}`);
  for (const g of list.slice(0, 25)) console.log(`  ${g.url.replace(/^https?:\/\/[^/]+/, "")}  ${g.el}  ${g.detail}  (${g.themes.join(",")})`);
  if (list.length > 25) console.log(`  … ${list.length - 25} more`);
}
if (!report.length) console.log("no findings");
const json = opt("json");
if (json) writeFileSync(json, JSON.stringify([...groups.values()], null, 2));
console.log(`\n${groups.size} unique findings across ${urls.length} page(s) × ${themes.length} theme(s).`);
