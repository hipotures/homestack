/*! The Mechanics 0.1.11 — behaviors (optional companion to the-mechanics.css) — CC0-1.0 — https://github.com/hipotures/the-mechanics */
/*
  CSS cannot restyle what the browser draws outside the page: the date and
  time popups, Firefox's select list, the title tooltip, confirm(). This
  module replaces those with The Mechanics versions and adds the small
  behaviors the design rules assume. No dependencies, no framework.

  Use:
    <script type="module" src="/vendor/the-mechanics/the-mechanics.js"></script>
  It runs init(document) once the DOM is ready and keeps enhancing elements
  added later (re-rendered views, dialogs). Opt out of the automatic start
  with <html data-tm-manual> and call init(root) yourself:
    import { init } from "/vendor/the-mechanics/the-mechanics.js";

  init(root) does: input modality hook (data-tm-input on <html>), select list
  fallback (browsers without appearance: base-select), date/time picker for
  .tm-input[type=date|time], clear ✕ in text fields, data-tm-tip tooltips,
  [popovertarget] menus (anchoring, arrow keys, focus return, follow on
  scroll), .tm-stepper buttons.
  APIs to call: toast(), confirmDialog(), dismissibleAlert(), setFieldError(),
  validateInline(), anchorPopover(), followAnchor().
*/

const esc = (v) =>
  String(v ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);

// The few 16px stroke icons the behaviors draw.
const ICONS = {
  x: "M3.5 3.5l9 9M12.5 3.5l-9 9",
  chevronLeft: "M10 3.5L5.5 8l4.5 4.5",
  chevronRight: "M6 3.5l4.5 4.5L6 12.5",
  calendar: "M2 3.5h12v10H2zM2 6.5h12M5 2v3M11 2v3",
  clock: "M8 14.5a6.5 6.5 0 1 0 0-13 6.5 6.5 0 0 0 0 13zM8 4.5V8l2.5 1.5",
  alert: "M8 1.5l6.5 12h-13zM8 6v3.5M8 11.5v.01",
  info: "M8 14.5a6.5 6.5 0 1 0 0-13 6.5 6.5 0 0 0 0 13zM8 7v4.5M8 4.5v.01",
  success: "M8 14.5a6.5 6.5 0 1 0 0-13 6.5 6.5 0 0 0 0 13zM5 8l2 2 4-4",
  danger: "M8 14.5a6.5 6.5 0 1 0 0-13 6.5 6.5 0 0 0 0 13zM5.5 5.5l5 5M10.5 5.5l-5 5",
};
const icon = (name, cls = "") =>
  `<svg class="tm-icon ${cls}" viewBox="0 0 16 16" aria-hidden="true" focusable="false"><path d="${ICONS[name] || ICONS.info}"/></svg>`;
const MARK_STATE = { success: "complete", complete: "complete", warning: "warning", danger: "error", error: "error", info: "info", pending: "pending", review: "review", active: "active" };

/* ---------- Input modality ------------------------------------------------
   Keeps data-tm-input="pointer" | "key" on <html>. The CSS uses it so that a
   popup opened with the mouse does not paint its scripted focus as keyboard
   focus (see "Pointer sessions" in components.css). Call once at start-up. */
export function trackInputModality() {
  if (trackInputModality.done) return;
  trackInputModality.done = true;
  const root = document.documentElement;
  const set = (v) => { if (root.dataset.tmInput !== v) root.dataset.tmInput = v; };
  document.addEventListener("pointerdown", () => set("pointer"), true);
  document.addEventListener("keydown", (e) => { if (!["Shift", "Control", "Alt", "Meta"].includes(e.key)) set("key"); }, true);
}


/* Dismissible alert: closing never loses the message — a small status
   button appears in restoreInto and brings it back. */
export function dismissibleAlert(alertEl, restoreInto, { tone = "warning", label = "1 warning" } = {}) {
  const close = document.createElement("button");
  close.className = "tm-btn tm-btn--ghost tm-btn--icon tm-btn--sm";
  close.setAttribute("aria-label", "Hide this message");
  close.innerHTML = icon("x");
  alertEl.append(close);
  const restore = document.createElement("button");
  restore.className = "tm-status tm-status--sm";
  restore.hidden = true;
  restore.setAttribute("aria-label", `Show hidden message: ${alertEl.querySelector(".tm-alert__title")?.textContent || label}`);
  restore.innerHTML = `<span class="tm-mark" data-state="${MARK_STATE[tone] || "warning"}" aria-hidden="true"></span>${esc(label)}`;
  restoreInto.prepend(restore);
  close.addEventListener("click", () => {
    alertEl.hidden = true;
    restore.hidden = false;
    restore.focus();
  });
  restore.addEventListener("click", () => {
    alertEl.hidden = false;
    restore.hidden = true;
    close.focus();
  });
}

/* ---------- Toasts ---------------------------------------------------- */
let toastRegion;
// Toast in the .tm-toast-region (created if the page has none). Title and
// body are plain text. Success / info / neutral leave after `timeout` ms
// (paused on hover); danger and warning stay until closed (timeout 0).
export function toast({ tone = "info", title, body = "", timeout = tone === "danger" || tone === "warning" ? 0 : 5000 } = {}) {
  if (!toastRegion?.isConnected) {
    toastRegion = document.querySelector(".tm-toast-region");
    if (!toastRegion) {
      toastRegion = document.createElement("div");
      toastRegion.className = "tm-toast-region";
      toastRegion.setAttribute("aria-live", "polite");
      document.body.append(toastRegion);
    }
  }
  const el = document.createElement("div");
  el.className = `tm-alert tm-alert--${tone} tm-toast`;
  el.setAttribute("role", tone === "danger" ? "alert" : "status");
  const iconName = { success: "success", warning: "alert", danger: "danger", info: "info", neutral: "info" }[tone];
  el.innerHTML = `${icon(iconName)}<div><div class="tm-alert__title">${esc(title)}</div>${
    body ? `<div class="tm-alert__body">${esc(body)}</div>` : ""
  }</div><button class="tm-btn tm-btn--ghost tm-btn--icon tm-btn--sm" aria-label="Dismiss notification">${icon("x")}</button>${
    timeout ? `<div class="tm-toast__progress" style="--tm-toast-ms:${timeout}ms"></div>` : ""
  }`;
  const close = () => el.remove();
  el.querySelector("button").addEventListener("click", close);
  if (timeout) {
    let t = setTimeout(close, timeout);
    // Pause on hover/focus so the message can be read (WCAG 2.2.1).
    el.addEventListener("mouseenter", () => { clearTimeout(t); el.querySelector(".tm-toast__progress")?.style.setProperty("animation-play-state", "paused"); });
    el.addEventListener("mouseleave", () => { t = setTimeout(close, 2000); });
  }
  toastRegion.append(el);
  return el;
}

/* ---------- Popover menus (native popover API + JS anchoring) -------- */
export function anchorPopover(pop, anchor) {
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
export function followAnchor(pop, anchor, onGone) {
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

// Wires every [popovertarget] invoker inside root: anchoring, focus first
// item, arrow-key roving, and focus return (Escape/light-dismiss are native).
export function wireMenus(root) {
  root.querySelectorAll("[popovertarget]").forEach((btn) => {
    const pop = document.getElementById(btn.getAttribute("popovertarget"));
    if (!pop || pop.dataset.wired) return;
    pop.dataset.wired = "1";
    btn.setAttribute("aria-haspopup", pop.getAttribute("role") === "menu" ? "menu" : "dialog");
    btn.setAttribute("aria-expanded", "false");
    pop.addEventListener("toggle", (e) => {
      const open = e.newState === "open";
      const invoker = root.querySelector(`[popovertarget="${pop.id}"][data-last-invoker]`) || btn;
      invoker.setAttribute("aria-expanded", String(open));
      pop._unfollow?.();
      pop._unfollow = null;
      if (open) {
        anchorPopover(pop, invoker);
        pop._unfollow = followAnchor(pop, invoker, () => pop.hidePopover());
        pop.querySelector('[role="menuitem"]:not(:disabled), button:not(:disabled), input, select')?.focus();
      }
    });
    pop.addEventListener("keydown", (e) => {
      const items = [...pop.querySelectorAll('[role="menuitem"]:not(:disabled)')];
      if (!items.length) return;
      const i = items.indexOf(document.activeElement);
      if (e.key === "ArrowDown") { e.preventDefault(); items[(i + 1) % items.length].focus(); }
      if (e.key === "ArrowUp") { e.preventDefault(); items[(i - 1 + items.length) % items.length].focus(); }
      if (e.key === "Home") { e.preventDefault(); items[0].focus(); }
      if (e.key === "End") { e.preventDefault(); items.at(-1).focus(); }
    });
    pop.addEventListener("click", (e) => {
      if (e.target.closest('[role="menuitem"]')) {
        pop.hidePopover();
        btn.focus();
      }
    });
  });
}

/* ---------- Dialog helpers (native <dialog>) ------------------------- */
// Confirmation in a .tm-dialog instead of window.confirm(). Resolves true on
// confirm. `body` is plain text (escaped); pass trusted markup as `bodyHTML`.
// Cancel is focused first; Escape, backdrop and ✕ cancel; focus returns to
// the element that was focused before.
export function confirmDialog({ title, body = "", bodyHTML, confirm = "Confirm", cancel = "Cancel", danger = false }) {
  const trigger = document.activeElement;
  return new Promise((resolve) => {
    const d = document.createElement("dialog");
    const id = `dlg-${Math.random().toString(36).slice(2, 8)}`;
    d.className = `tm-dialog${danger ? " tm-dialog--danger" : ""}`;
    d.setAttribute("aria-labelledby", `${id}-t`);
    d.setAttribute("aria-describedby", `${id}-b`);
    d.innerHTML = `<form method="dialog">
      <div class="tm-dialog__head"><h2 class="tm-dialog__title" id="${id}-t">${esc(title)}</h2>
        <button class="tm-btn tm-btn--ghost tm-btn--icon tm-btn--sm" value="cancel" aria-label="Close">${icon("x")}</button></div>
      <div class="tm-dialog__body" id="${id}-b">${bodyHTML ?? (body ? `<p>${esc(body)}</p>` : "")}</div>
      <div class="tm-dialog__foot">
        <button class="tm-btn tm-btn--outline" value="cancel" autofocus>${esc(cancel)}</button>
        <button class="tm-btn ${danger ? "tm-btn--danger" : "tm-btn--primary"}" value="confirm">${esc(confirm)}</button>
      </div></form>`;
    document.body.append(d);
    d.addEventListener("close", () => {
      resolve(d.returnValue === "confirm");
      d.remove();
      if (trigger?.isConnected && document.activeElement !== trigger) trigger.focus();
    });
    // Backdrop click closes (Escape is native).
    d.addEventListener("click", (e) => { if (e.target === d) d.close("cancel"); });
    d.showModal();
  });
}

// Number stepper behavior for .tm-stepper (delegated): − / + call
// stepDown()/stepUp() within min/max, fire "input" + "change", and disable
// a button at its bound. Native ArrowUp/ArrowDown on the input still work.
export function wireSteppers(root = document) {
  const sync = (stepper) => {
    const input = stepper.querySelector('input[type="number"]');
    if (!input) return;
    const v = input.valueAsNumber;
    const min = input.min === "" ? -Infinity : +input.min;
    const max = input.max === "" ? Infinity : +input.max;
    stepper.querySelectorAll("[data-tm-step]").forEach((b) => {
      const dir = +b.dataset.tmStep;
      b.disabled = input.disabled || (!Number.isNaN(v) && ((dir < 0 && v <= min) || (dir > 0 && v >= max)));
    });
  };
  root.querySelectorAll(".tm-stepper").forEach(sync);
  if (root.__tmSteppers) return;
  root.__tmSteppers = true;
  root.addEventListener("click", (e) => {
    const b = e.target.closest("[data-tm-step]");
    if (!b) return;
    const stepper = b.closest(".tm-stepper");
    const input = stepper.querySelector('input[type="number"]');
    if (Number.isNaN(input.valueAsNumber)) input.value = input.min || 0;
    else +b.dataset.tmStep > 0 ? input.stepUp() : input.stepDown();
    input.dispatchEvent(new Event("input", { bubbles: true }));
    input.dispatchEvent(new Event("change", { bubbles: true }));
    sync(stepper);
  });
  root.addEventListener("input", (e) => {
    const stepper = e.target.closest?.(".tm-stepper");
    if (stepper) sync(stepper);
  });
}

// Select option list fallback for browsers without `appearance: base-select`
// (e.g. Firefox): the native <select> stays the source of truth (value, form,
// change events, labels), but its OS popup is replaced by a styled listbox
// (.tm-menu .tm-listbox). Mouse: click opens. Keyboard on the closed select:
// Enter / Space / Alt+ArrowDown open it. In the list: ArrowUp/Down, Home/End,
// type-ahead, Enter/Space to choose, Escape/Tab to close (focus returns).
// Test hook: ?select=fallback forces this path in any browser.
export function enhanceSelects(root = document) {
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

/* ---------- Date / time picker ----------------------------------------
   Keeps the native <input type="date|time"> (typing, validation) and
   replaces only the browser popup with a .tm-picker popover. Dates use ISO
   weeks (Monday first) and no weekend colouring; times are 24 h. */
const pad2 = (n) => String(n).padStart(2, "0");
const MONTHS = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
export function enhanceDateTime(root = document) {
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
      if (isDate) { const v = input.value ? new Date(input.value + "T00:00:00Z") : new Date(Date.UTC(2026, 10, 1)); drawDate(new Date(Date.UTC(v.getUTCFullYear(), v.getUTCMonth(), 1))); }
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

/* ---------- Clear button for text fields ------------------------------
   Every editable text/search/email/url/tel .tm-input gets a ✕ on the right
   while it holds text (CSS shows it via :placeholder-shown, so programmatic
   value changes are covered too). Click clears, fires input/change and keeps
   focus in the field; Escape clears search fields. Opt out with
   data-no-clear. New fields are picked up by a MutationObserver. */
const CLEARABLE = 'input.tm-input:is([type="text"], [type="search"], [type="email"], [type="url"], [type="tel"], :not([type])):not([readonly], [disabled], [data-no-clear], .tm-otp__cell, [data-tm-clear])';
export function enhanceClearable(root = document) {
  if (!enhanceClearable.observer) {
    let queued = false;
    enhanceClearable.observer = new MutationObserver(() => {
      if (queued) return;
      queued = true;
      requestAnimationFrame(() => { queued = false; enhanceClearable(document); });
    });
    enhanceClearable.observer.observe(document.body, { childList: true, subtree: true });
  }
  root.querySelectorAll(CLEARABLE).forEach((input) => {
    if (input.closest(".tm-stepper, .tm-multiselect, .tm-palette, .tm-picker-field")) return;
    input.dataset.tmClear = "1";
    if (!input.placeholder) input.placeholder = " "; // :placeholder-shown = empty
    const tip = document.createElement("span");
    tip.className = "tm-tip tm-tip--below tm-input-clear";
    tip.innerHTML = `<button type="button" class="tm-input-clear__btn" tabindex="-1" aria-label="Clear">${icon("x", "tm-icon--sm")}</button><span class="tm-tip__bubble" aria-hidden="true">Clear</span>`;
    const group = input.parentElement.classList.contains("tm-input-group") ? input.parentElement : null;
    if (group) {
      input.after(tip);
    } else {
      const wrap = document.createElement("span");
      wrap.className = "tm-clearable";
      const hadFocus = document.activeElement === input;
      const [a, z] = [input.selectionStart, input.selectionEnd];
      input.replaceWith(wrap);
      wrap.append(input, tip);
      if (hadFocus) { input.focus(); try { input.setSelectionRange(a, z); } catch {} }
    }
    const clear = () => {
      input.value = "";
      input.dispatchEvent(new Event("input", { bubbles: true }));
      input.dispatchEvent(new Event("change", { bubbles: true }));
      input.focus();
    };
    tip.firstElementChild.addEventListener("mousedown", (e) => e.preventDefault()); // keep focus in the field
    tip.firstElementChild.addEventListener("click", clear);
    if (input.type === "search") input.addEventListener("keydown", (e) => { if (e.key === "Escape" && input.value) { e.preventDefault(); e.stopPropagation(); clear(); } });
  });
}

/* ---------- Tooltips from data-tm-tip ----------------------------------
   One tooltip look everywhere (never the browser's title popup). Any element
   with data-tm-tip="text" shows a .tm-tip__bubble on hover and keyboard focus,
   positioned below it (above if there is no room) and kept inside the
   viewport; Escape, scrolling or leaving hides it.
   data-tm-tip-overflow — only when the element's text is truncated;
   data-tm-tip-collapsed — only inside a collapsed sidebar (icon rail). */
export function enhanceTips() {
  if (enhanceTips.ready) return;
  enhanceTips.ready = true;
  const bubble = document.createElement("div");
  bubble.className = "tm-tip__bubble tm-tip--float";
  bubble.id = "tm-float-tip";
  bubble.setAttribute("role", "tooltip");
  bubble.hidden = true;
  document.body.append(bubble);
  let host = null;
  const eligible = (el) =>
    !(el.hasAttribute("data-tm-tip-collapsed") && !el.closest('[data-sidebar="collapsed"]')) &&
    !(el.hasAttribute("data-tm-tip-overflow") && el.scrollWidth <= el.clientWidth);
  const hide = () => {
    if (!host) return;
    const ids = (host.getAttribute("aria-describedby") || "").split(" ").filter((i) => i && i !== bubble.id);
    ids.length ? host.setAttribute("aria-describedby", ids.join(" ")) : host.removeAttribute("aria-describedby");
    host = null;
    bubble.hidden = true;
  };
  const show = (el) => {
    if (el === host) return;
    hide();
    if (!el.dataset.tmTip || !eligible(el)) return;
    host = el;
    bubble.textContent = el.dataset.tmTip;
    bubble.hidden = false;
    const r = el.getBoundingClientRect();
    const b = bubble.getBoundingClientRect();
    let top = r.bottom + 6;
    if (top + b.height > innerHeight - 8) top = Math.max(8, r.top - 6 - b.height);
    bubble.style.top = `${top}px`;
    bubble.style.left = `${Math.max(8, Math.min(r.left, innerWidth - b.width - 8))}px`;
    el.setAttribute("aria-describedby", [el.getAttribute("aria-describedby"), bubble.id].filter(Boolean).join(" "));
  };
  document.addEventListener("pointerover", (e) => {
    const el = e.target.closest?.("[data-tm-tip]");
    el ? show(el) : hide();
  });
  document.addEventListener("focusin", (e) => {
    const el = e.target.closest?.("[data-tm-tip]");
    if (el && e.target.matches(":focus-visible")) show(el);
  });
  document.addEventListener("focusout", hide);
  document.addEventListener("keydown", (e) => { if (e.key === "Escape") hide(); });
  window.addEventListener("scroll", hide, { capture: true, passive: true });
  window.addEventListener("hashchange", hide);
}

/* ---------- Inline validation ----------------------------------------
   Browser validation bubbles cannot be styled. Forms get novalidate and this
   helper: on submit every invalid control gets aria-invalid and its message
   in a .tm-field__error below it (the control's own validationMessage, or
   data-error), focus goes to the first one, and submit is stopped. Editing a
   field clears its error. Custom checks: setFieldError(input, "message"). */
export function setFieldError(input, message) {
  const field = input.closest(".tm-field") || input.parentElement;
  let err = field.querySelector(":scope > .tm-field__error");
  if (!message) {
    input.removeAttribute("aria-invalid");
    input.setCustomValidity("");
    err?.remove();
    return;
  }
  input.setCustomValidity(message);
  input.setAttribute("aria-invalid", "true");
  if (!err) {
    err = document.createElement("span");
    err.className = "tm-field__error";
    err.id = `${input.id || `f${Math.random().toString(36).slice(2, 7)}`}-err`;
    field.append(err);
  }
  err.textContent = message;
  if (!(input.getAttribute("aria-describedby") || "").includes(err.id)) input.setAttribute("aria-describedby", [input.getAttribute("aria-describedby"), err.id].filter(Boolean).join(" "));
}
export function validateInline(form, check = () => {}) {
  form.noValidate = true;
  form.addEventListener("input", (e) => { if (e.target.getAttribute?.("aria-invalid") === "true") setFieldError(e.target, ""); });
  form.addEventListener("submit", (e) => {
    form.querySelectorAll("[aria-invalid='true']").forEach((el) => setFieldError(el, ""));
    check(form); // custom rules call setFieldError
    const invalid = [...form.elements].filter((el) => el.willValidate && !el.checkValidity());
    invalid.forEach((el) => setFieldError(el, el.dataset.error || el.validationMessage));
    if (invalid.length) { e.preventDefault(); e.stopImmediatePropagation(); invalid[0].focus(); }
  }, true);
}

/* ---------- Start-up ----------------------------------------------------
   init(root) is idempotent: every enhancer marks what it has done, so it is
   safe to call again after rendering new markup. */
export function init(root = document) {
  trackInputModality();
  enhanceSelects(root);
  enhanceDateTime(root);
  enhanceClearable(root);
  enhanceTips();
  wireMenus(root);
  wireSteppers(root);
}

function autoStart() {
  init(document);
  // Elements added later (client-rendered views, dialogs) are enhanced too.
  let queued = false;
  new MutationObserver(() => {
    if (queued) return;
    queued = true;
    queueMicrotask(() => { queued = false; init(document); });
  }).observe(document.body, { childList: true, subtree: true });
}

// Classic (non-module) app scripts reach the same functions through a global.
if (typeof window !== "undefined") {
  window.TheMechanics = { init, toast, confirmDialog, dismissibleAlert, setFieldError, validateInline, anchorPopover, followAnchor, enhanceSelects, enhanceDateTime, enhanceClearable, enhanceTips, wireMenus, wireSteppers, trackInputModality };
}

if (typeof document !== "undefined" && !document.documentElement.hasAttribute("data-tm-manual")) {
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", autoStart, { once: true });
  else autoStart();
}
