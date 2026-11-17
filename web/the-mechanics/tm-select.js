/* Select option list fallback (Firefox and other browsers without appearance: base-select),
   ported from The Mechanics playground (src/playground/lib.js). Shared by Agent Accounts and AI Usage. */
'use strict';
(function () {
function anchorPopover(pop, anchor) {
  const r = anchor.getBoundingClientRect();
  const pw = pop.offsetWidth;
  const ph = pop.offsetHeight;
  let left = Math.min(r.left, window.innerWidth - pw - 8);
  if (r.right - pw > 8 && r.left + pw > window.innerWidth - 8) left = r.right - pw;
  let top = r.bottom + 4;
  if (top + ph > window.innerHeight - 8) top = Math.max(8, r.top - ph - 4);
  pop.style.left = `${Math.max(8, left)}px`;
  pop.style.top = `${top}px`;
}
function enhanceSelects(root = document) {
  const forced = new URLSearchParams(location.search).get("select") === "fallback";
  if (!forced && CSS.supports("appearance", "base-select")) return;
  document.documentElement.classList.add("tm-select-fallback");
  // Selects rendered later (re-rendered forms, dialogs) are enhanced too.
  if (!enhanceSelects.observer) {
    let queued = false;
    enhanceSelects.observer = new MutationObserver(() => {
      if (queued) return;
      queued = true;
      queueMicrotask(() => { queued = false; enhanceSelects(document); });
    });
    enhanceSelects.observer.observe(document.body, { childList: true, subtree: true });
  }
  root.querySelectorAll("select.tm-select:not([data-tm-enhanced])").forEach((sel) => {
    sel.dataset.tmEnhanced = "1";
    let list = null;
    let dismiss = null;
    const close = (focus = true) => {
      if (!list) return;
      dismiss?.(); dismiss = null;
      list.remove();
      list = null;
      sel.setAttribute("aria-expanded", "false");
      if (focus) sel.focus();
    };
    const choose = (i) => {
      if (sel.selectedIndex !== i) {
        sel.selectedIndex = i;
        sel.dispatchEvent(new Event("input", { bubbles: true }));
        sel.dispatchEvent(new Event("change", { bubbles: true }));
      }
      close();
    };
    const open = () => {
      if (list || sel.disabled) return;
      list = document.createElement("div");
      list.className = "tm-menu tm-listbox";
      list.setAttribute("role", "listbox");
      list.setAttribute("popover", "manual");
      list.setAttribute("aria-label", sel.labels?.[0]?.textContent?.trim() || sel.getAttribute("aria-label") || "Options");
      [...sel.options].forEach((o, i) => {
        const b = document.createElement("button");
        b.type = "button";
        b.className = "tm-menu__item tm-listbox__option";
        b.setAttribute("role", "option");
        b.setAttribute("aria-selected", String(i === sel.selectedIndex));
        b.disabled = o.disabled;
        b.textContent = o.textContent;
        b.addEventListener("click", () => choose(i));
        list.append(b);
      });
      document.body.append(list);
      list.showPopover();
      list.style.minWidth = `${sel.offsetWidth}px`;
      anchorPopover(list, sel);
      sel.setAttribute("aria-expanded", "true");
      list.querySelectorAll('[role="option"]')[Math.max(0, sel.selectedIndex)]?.focus();
      list.addEventListener("keydown", (e) => {
        const items = [...list.querySelectorAll('[role="option"]:not(:disabled)')];
        const i = items.indexOf(document.activeElement);
        if (e.key === "ArrowDown") { e.preventDefault(); items[Math.min(items.length - 1, i + 1)].focus(); }
        else if (e.key === "ArrowUp") { e.preventDefault(); items[Math.max(0, i - 1)].focus(); }
        else if (e.key === "Home") { e.preventDefault(); items[0].focus(); }
        else if (e.key === "End") { e.preventDefault(); items.at(-1).focus(); }
        else if (e.key === "Escape") { e.preventDefault(); close(); }
        else if (e.key === "Tab") close(false);
        else if (e.key.length === 1) {
          const k = e.key.toLowerCase();
          const next = items.find((b, j) => j > i && b.textContent.toLowerCase().startsWith(k)) || items.find((b) => b.textContent.toLowerCase().startsWith(k));
          next?.focus();
        }
      });
      // Light dismiss: a pointer outside the list or a page scroll closes it; scrolling the list itself does not.
      setTimeout(() => {
        if (!list) return;
        const away = (e) => { if (list && !list.contains(e.target) && e.target !== sel) close(false); };
        const scrolled = (e) => { if (list && !list.contains(e.target)) close(false); };
        document.addEventListener("pointerdown", away, true);
        window.addEventListener("scroll", scrolled, { capture: true, passive: true });
        dismiss = () => {
          document.removeEventListener("pointerdown", away, true);
          window.removeEventListener("scroll", scrolled, { capture: true });
        };
      });
    };
    sel.setAttribute("aria-haspopup", "listbox");
    sel.setAttribute("aria-expanded", "false");
    sel.addEventListener("mousedown", (e) => {
      if (e.button !== 0) return;
      e.preventDefault(); // suppress the OS popup
      sel.focus();
      list ? close() : open();
    });
    sel.addEventListener("keydown", (e) => {
      if (e.key === "Enter" || e.key === " " || (e.altKey && e.key === "ArrowDown")) {
        e.preventDefault();
        open();
      }
    });
  });
}
// Input modality (The Mechanics v0.1.5): lists opened by mouse show only the selection fill;
// the first navigation key brings the keyboard focus frame back.
(function trackInputModality() {
  const root = document.documentElement;
  const set = (v) => { if (root.dataset.tmInput !== v) root.dataset.tmInput = v; };
  document.addEventListener("pointerdown", () => set("pointer"), true);
  document.addEventListener("keydown", (e) => { if (!["Shift", "Control", "Alt", "Meta"].includes(e.key)) set("key"); }, true);
})();
document.addEventListener('DOMContentLoaded', () => enhanceSelects(document));
})();
