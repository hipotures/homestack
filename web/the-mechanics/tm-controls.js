/* The Mechanics v0.1.8 control helpers, ported from the playground (src/playground/lib.js):
   input modality, anchored popups that follow their field, the select list fallback (browsers without
   appearance: base-select, e.g. Firefox) and the date/time picker that replaces the browser popup.
   Shared by Agent Accounts and AI Usage; keep it in sync with the upstream reference on updates. */
'use strict';
(function () {
const ICON = {
  chevronLeft: "M10 3.5L5.5 8l4.5 4.5",
  chevronRight: "M6 3.5l4.5 4.5L6 12.5",
  calendar: "M2 3.5h12v10H2zM2 6.5h12M5 2v3M11 2v3",
  clock: "M8 14.5a6.5 6.5 0 1 0 0-13 6.5 6.5 0 0 0 0 13zM8 4.5V8l2.5 1.5",
};
const icon = (name, cls = "") =>
  `<svg class="tm-icon ${cls}" viewBox="0 0 16 16" aria-hidden="true" focusable="false"><path d="${ICON[name]}"/></svg>`;

function trackInputModality() {
  if (trackInputModality.done) return;
  trackInputModality.done = true;
  const root = document.documentElement;
  const set = (v) => { if (root.dataset.tmInput !== v) root.dataset.tmInput = v; };
  document.addEventListener("pointerdown", () => set("pointer"), true);
  document.addEventListener("keydown", (e) => { if (!["Shift", "Control", "Alt", "Meta"].includes(e.key)) set("key"); }, true);
}

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

/* Keeps an anchored popup attached to its anchor while the page scrolls or
   resizes (popups are fixed in the top layer, so they would otherwise stay
   put while the field moves away). Scrolling inside the popup itself is
   ignored. When the anchor leaves the viewport, onGone() runs (close it).
   Returns a function that removes the listeners. */
function followAnchor(pop, anchor, onGone) {
  let queued = false;
  const update = (e) => {
    if (e?.type === "scroll" && pop.contains(e.target)) return;
    if (queued) return;
    queued = true;
    requestAnimationFrame(() => {
      queued = false;
      if (!pop.isConnected) return;
      const r = anchor.getBoundingClientRect();
      if (r.bottom < 0 || r.top > window.innerHeight || r.right < 0 || r.left > window.innerWidth) onGone?.();
      else anchorPopover(pop, anchor);
    });
  };
  window.addEventListener("scroll", update, { capture: true, passive: true });
  window.addEventListener("resize", update, { passive: true });
  return () => {
    window.removeEventListener("scroll", update, { capture: true });
    window.removeEventListener("resize", update);
  };
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
  root.querySelectorAll("select.tm-select:not([data-tm-enhanced]), .tm-root select:not([class]):not([data-tm-enhanced])").forEach((sel) => {
    sel.dataset.tmEnhanced = "1";
    let list = null;
    let dismiss = null; // removes the light-dismiss listeners
    const close = (focus = true) => {
      if (!list) return;
      dismiss?.();
      dismiss = null;
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
      // Light dismiss: a pointer outside the list closes it. While the page
      // scrolls the list follows its select (followAnchor ignores scrolling
      // inside the list: wheel, touch, dragging or clicking its scrollbar)
      // and closes once the select leaves the viewport.
      setTimeout(() => {
        if (!list) return;
        const away = (e) => { if (list && !list.contains(e.target) && e.target !== sel) close(false); };
        document.addEventListener("pointerdown", away, true);
        const unfollow = followAnchor(list, sel, () => close(false));
        dismiss = () => {
          document.removeEventListener("pointerdown", away, true);
          unfollow();
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

const pad2 = (n) => String(n).padStart(2, "0");
const MONTHS = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
function enhanceDateTime(root = document) {
  root.querySelectorAll('input.tm-input:is([type="date"], [type="time"]):not([data-tm-picker])').forEach((input) => {
    input.dataset.tmPicker = "1";
    const isDate = input.type === "date";
    const group = document.createElement("div");
    group.className = "tm-input-group tm-picker-field";
    input.replaceWith(group);
    group.append(input);
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "tm-picker-field__btn";
    btn.setAttribute("aria-haspopup", "dialog");
    btn.setAttribute("aria-expanded", "false");
    const label = input.labels?.[0]?.textContent?.trim() || (isDate ? "date" : "time");
    btn.setAttribute("aria-label", `Choose ${label}`);
    btn.innerHTML = icon(isDate ? "calendar" : "clock", "tm-icon--sm");
    btn.disabled = input.disabled;
    group.append(btn);

    let pop = null, unfollow = null;
    const set = (v) => {
      input.value = v;
      input.dispatchEvent(new Event("input", { bubbles: true }));
      input.dispatchEvent(new Event("change", { bubbles: true }));
    };
    const close = (focus = true) => {
      if (!pop) return;
      pop.remove();
      pop = null;
      unfollow?.();
      unfollow = null;
      btn.setAttribute("aria-expanded", "false");
      document.removeEventListener("pointerdown", outside, true);
      if (focus) btn.focus();
    };
    const outside = (e) => { if (pop && !pop.contains(e.target) && !group.contains(e.target)) close(false); };

    function drawDate(view, focusDay) {
      const sel = input.value;
      const y = view.getUTCFullYear(), m = view.getUTCMonth();
      const first = new Date(Date.UTC(y, m, 1));
      const start = new Date(first);
      start.setUTCDate(1 - ((first.getUTCDay() + 6) % 7));
      const days = Array.from({ length: 42 }, (_, i) => { const d = new Date(start); d.setUTCDate(start.getUTCDate() + i); return d; });
      const iso = (d) => d.toISOString().slice(0, 10);
      const target = focusDay || sel || iso(first);
      pop.innerHTML = `<div class="tm-picker__head"><button type="button" class="tm-btn tm-btn--ghost tm-btn--icon tm-btn--sm" data-nav="-1" aria-label="Previous month">${icon("chevronLeft", "tm-icon--sm")}</button><span aria-live="polite">${MONTHS[m]} ${y}</span><button type="button" class="tm-btn tm-btn--ghost tm-btn--icon tm-btn--sm" data-nav="1" aria-label="Next month">${icon("chevronRight", "tm-icon--sm")}</button></div>
        <div class="tm-picker__grid" role="grid" aria-label="${MONTHS[m]} ${y}">${["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"].map((d) => `<span class="tm-picker__dow" aria-hidden="true">${d}</span>`).join("")}${days
          .map((d) => { const v = iso(d); return `<button type="button" class="tm-picker__cell" data-v="${v}" ${d.getUTCMonth() !== m ? "data-outside" : ""} aria-selected="${v === sel}" tabindex="${v === target ? 0 : -1}" aria-label="${d.getUTCDate()} ${MONTHS[d.getUTCMonth()]} ${d.getUTCFullYear()}">${d.getUTCDate()}</button>`; })
          .join("")}</div>
        <div class="tm-picker__foot"><button type="button" class="tm-btn tm-btn--ghost tm-btn--sm" data-clear>Clear</button><button type="button" class="tm-btn tm-btn--outline tm-btn--sm" data-done>Close</button></div>`;
      (pop.querySelector(`[data-v="${target}"]`) || pop.querySelector(".tm-picker__cell:not([data-outside])")).focus();
      pop.onclick = (e) => {
        const nav = e.target.closest("[data-nav]");
        if (nav) { drawDate(new Date(Date.UTC(y, m + Number(nav.dataset.nav), 1))); return; }
        if (e.target.closest("[data-clear]")) { set(""); close(); return; }
        if (e.target.closest("[data-done]")) { close(); return; }
        const c = e.target.closest("[data-v]");
        if (c) { set(c.dataset.v); close(); input.focus(); }
      };
      pop.onkeydown = (e) => {
        const c = e.target.closest("[data-v]");
        if (!c) return;
        const step = { ArrowLeft: -1, ArrowRight: 1, ArrowUp: -7, ArrowDown: 7 }[e.key];
        const d = new Date(c.dataset.v + "T00:00:00Z");
        if (step) d.setUTCDate(d.getUTCDate() + step);
        else if (e.key === "PageUp" || e.key === "PageDown") d.setUTCMonth(d.getUTCMonth() + (e.key === "PageUp" ? -1 : 1));
        else return;
        e.preventDefault();
        const v = iso(d);
        const next = pop.querySelector(`[data-v="${v}"]:not([data-outside])`);
        if (next) { c.tabIndex = -1; next.tabIndex = 0; next.focus(); }
        else drawDate(new Date(Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), 1)), v);
      };
    }

    function drawTime() {
      const [h = "00", mi = "00"] = (input.value || "").split(":");
      const col = (name, values, cur) => `<div><div class="tm-picker__dow tm-picker__col-label">${name}</div><div class="tm-picker__col" role="listbox" aria-label="${name}">${values
        .map((v) => `<button type="button" class="tm-picker__cell" role="option" data-part="${name}" data-v="${v}" aria-selected="${v === cur}" tabindex="${v === cur ? 0 : -1}">${v}</button>`)
        .join("")}</div></div>`;
      const minutes = Array.from({ length: 12 }, (_, i) => pad2(i * 5));
      if (!minutes.includes(mi)) minutes.push(mi), minutes.sort();
      pop.innerHTML = `<div class="tm-picker__cols">${col("Hour", Array.from({ length: 24 }, (_, i) => pad2(i)), h)}<span class="tm-picker__dow" aria-hidden="true" style="align-self:center">:</span>${col("Min", minutes, mi)}</div>
        <div class="tm-picker__foot"><button type="button" class="tm-btn tm-btn--ghost tm-btn--sm" data-clear>Clear</button><button type="button" class="tm-btn tm-btn--outline tm-btn--sm" data-done>Done</button></div>`;
      pop.querySelectorAll('[aria-selected="true"]').forEach((c) => c.scrollIntoView({ block: "center" }));
      (pop.querySelector('[data-part="Hour"][aria-selected="true"]') || pop.querySelector("[data-v]")).focus();
      const update = (part, v) => {
        const [ch = "00", cm = "00"] = (input.value || "00:00").split(":");
        set(part === "Hour" ? `${v}:${cm}` : `${ch}:${v}`);
        pop.querySelectorAll(`[data-part="${part}"]`).forEach((c) => { const on = c.dataset.v === v; c.setAttribute("aria-selected", String(on)); c.tabIndex = on ? 0 : -1; });
      };
      pop.onclick = (e) => {
        if (e.target.closest("[data-clear]")) { set(""); close(); return; }
        if (e.target.closest("[data-done]")) { close(); input.focus(); return; }
        const c = e.target.closest("[data-v]");
        if (c) { update(c.dataset.part, c.dataset.v); c.focus(); }
      };
      pop.onkeydown = (e) => {
        const c = e.target.closest("[data-v]");
        if (!c) return;
        const items = [...pop.querySelectorAll(`[data-part="${c.dataset.part}"]`)];
        const i = items.indexOf(c);
        if (e.key === "ArrowDown" || e.key === "ArrowUp") {
          e.preventDefault();
          const n = items[Math.max(0, Math.min(items.length - 1, i + (e.key === "ArrowDown" ? 1 : -1)))];
          update(n.dataset.part, n.dataset.v); n.focus(); n.scrollIntoView({ block: "nearest" });
        } else if (e.key === "ArrowRight" || e.key === "ArrowLeft") {
          e.preventDefault();
          pop.querySelector(`[data-part="${e.key === "ArrowRight" ? "Min" : "Hour"}"][aria-selected="true"]`)?.focus();
        } else if (e.key === "Enter") { e.preventDefault(); close(); input.focus(); }
      };
    }

    const open = () => {
      if (pop || input.disabled || input.readOnly) return;
      pop = document.createElement("div");
      pop.className = "tm-menu tm-picker";
      pop.setAttribute("role", "dialog");
      pop.setAttribute("aria-label", `Choose ${label}`);
      pop.setAttribute("popover", "manual");
      document.body.append(pop);
      pop.showPopover();
      if (isDate) { const v = input.value ? new Date(input.value + "T00:00:00Z") : new Date(Date.UTC(new Date().getFullYear(), new Date().getMonth(), 1)); drawDate(new Date(Date.UTC(v.getUTCFullYear(), v.getUTCMonth(), 1))); }
      else drawTime();
      anchorPopover(pop, group);
      unfollow = followAnchor(pop, group, () => close(false));
      pop.addEventListener("keydown", (e) => { if (e.key === "Escape") { e.preventDefault(); close(); } });
      btn.setAttribute("aria-expanded", "true");
      document.addEventListener("pointerdown", outside, true);
    };
    btn.addEventListener("click", () => (pop ? close() : open()));
    input.addEventListener("keydown", (e) => {
      if ((e.altKey && e.key === "ArrowDown") || e.key === "F4") { e.preventDefault(); open(); }
    });
    // Chromium opens its own popup on click in some builds; keep it ours.
    input.addEventListener("click", (e) => { if (input.showPicker) e.preventDefault(); });
  });
}

trackInputModality();
document.addEventListener("DOMContentLoaded", () => { enhanceSelects(document); enhanceDateTime(document); });
})();
