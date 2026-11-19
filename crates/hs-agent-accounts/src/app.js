'use strict';
const $ = selector => document.querySelector(selector);
const { confirmDialog, setFieldError, validateInline } = window.TheMechanics;
const clearIcon = '<svg class="tm-icon tm-icon--sm" viewBox="0 0 16 16" aria-hidden="true"><path d="M4 4l8 8M12 4l-8 8" stroke="currentColor" stroke-width="1.6"/></svg>';
let selectedOperation = null;
let source = null;
const providerCapabilities = new Map();
const quotaGroups = new Map();
const pendingActions = new Set();
const successfulChecks = new Map();
async function api(path, method = 'GET', body) {
  const response = await fetch('/api/v1' + path, {
    method, headers: { 'Content-Type': 'application/json' },
    ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
  });
  if (!response.ok) {
    const value = await response.json().catch(() => ({}));
    throw Error(value.error?.message || `Request failed (${response.status})`);
  }
  const text = await response.text();
  return text ? JSON.parse(text) : null;
}
/** Wraps an element in a The Mechanics tooltip (one tooltip look; never the browser title popup). */
function withTip(el, text, align = 'start') {
  const tip = node('span', undefined, `tm-tip tm-tip--below aa-tipwrap aa-tipwrap--${align}`);
  tip.append(el, node('span', text, 'tm-tip__bubble aa-info__bubble'));
  return tip;
}
function node(tag, text, className) {
  const element = document.createElement(tag);
  if (text !== undefined) element.textContent = text;
  if (className) element.className = className;
  return element;
}
const pad = number => String(number).padStart(2, '0');
function time(value) {
  if (!value) return 'Unknown';
  const d = new Date(value * 1000);
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}
function periodLeft(end) {
  const minutes = Math.floor((end - Date.now() / 1000) / 60);
  if (minutes <= 0) return 'Period ended';
  const days = Math.floor(minutes / 1440), hours = Math.floor(minutes % 1440 / 60);
  return 'Ends in ' + (days ? `${days}d ${hours}h` : `${hours}h ${minutes % 60}m`);
}
/** Adds calendar months the way subscription billing does: a missing day falls back to the month's last day. */
function addMonths(seconds, months) {
  const d = new Date(seconds * 1000), day = d.getDate();
  d.setDate(1); d.setMonth(d.getMonth() + months);
  d.setDate(Math.min(day, new Date(d.getFullYear(), d.getMonth() + 1, 0).getDate()));
  return Math.floor(d.getTime() / 1000);
}
function localInput(seconds) {
  const d = new Date(seconds * 1000);
  return `${pad(d.getDate())}.${pad(d.getMonth() + 1)}.${d.getFullYear()} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}
/** Parses local DD.MM.YYYY HH:MM as Unix seconds; null for a malformed or nonexistent time. */
function parseInput(value) {
  const m = value.trim().match(/^(\d{1,2})\.(\d{1,2})\.(\d{4}) (\d{1,2}):(\d{2})$/);
  if (!m) return null;
  const [day, month, year, hour, minute] = m.slice(1).map(Number);
  const d = new Date(year, month - 1, day, hour, minute);
  return d.getDate() === day && d.getMonth() === month - 1 && d.getHours() === hour ? Math.floor(d.getTime() / 1000) : null;
}
let paymentProfile = null;
function editPayment(profile) {
  paymentProfile = profile;
  $('#payment-at').value = profile.last_payment ? localInput(profile.last_payment) : '';
  $('#payment-dialog').showModal();
}
async function savePayment(value) {
  try {
    await api(`/profiles/${paymentProfile.id}`, 'PATCH', { last_payment: value });
    $('#payment-dialog').close(); await refresh();
    done(paymentProfile.display_name, value == null ? 'Payment date cleared.' : 'Payment date saved.', `card-${paymentProfile.id}`);
  } catch (error) { notice(error); }
}
const percent = value => value == null ? 'unknown' : `${value.toFixed(1)}% used · ${Math.max(0, 100 - value).toFixed(1)}% left`;
function setText(element, text) { if (element.textContent !== text) element.textContent = text; }
/* Errors are toasts too, never an alert above the lists (owner decision).
   A failed action is a danger toast that stays until closed; the same message
   is not stacked twice. A lost connection is a condition toast: it leaves by
   itself on reconnect, and ✕ hides it until the connection drops again. */
const errorToasts = new Map();
function notice(error) {
  const message = String(error?.message || error);
  if (errorToasts.has(message)) return;
  const el = makeToast({ tone: 'danger', title: 'Action failed', body: message, timeout: 0, onClose: () => errorToasts.delete(message) });
  document.querySelector('.tm-toast-region').append(el); errorToasts.set(message, el);
}
let connectionToast = null, connectionDismissed = false;
function setConnectionLost(lost) {
  if (!lost) { connectionToast?.remove(); connectionToast = null; connectionDismissed = false; return; }
  if (connectionToast || connectionDismissed) return;
  connectionToast = makeToast({ tone: 'danger', title: 'Connection lost', body: 'Reconnecting to the service…', timeout: 0, onClose: () => { connectionToast = null; connectionDismissed = true; } });
  document.querySelector('.tm-toast-region').append(connectionToast);
}
function actionState(element, busy) {
  element.disabled = busy || element.dataset.unavailable === 'true';
  element.classList.toggle('busy', busy);
  element.setAttribute('aria-busy', String(busy));
}
// `ask` (optional) runs before the button turns busy — a confirmation must not
// show a spinner on the button while the user is still deciding.
function button(parent, label, action, key, unavailable = false, ask = null) {
  const element = node('button', label, 'tm-btn tm-btn--sm tm-btn--ghost');
  element.type = 'button';
  element.dataset.action = key || '';
  element.dataset.unavailable = String(unavailable);
  actionState(element, pendingActions.has(key));
  element.onclick = async () => {
    if (key && pendingActions.has(key)) return;
    if (ask && !(await ask())) return;
    if (key) pendingActions.add(key);
    actionState(element, true);
    try { await action(); } catch (error) { notice(error); }
    finally {
      if (key) pendingActions.delete(key);
      actionState(element, false);
      if (key) document.querySelectorAll('button[data-action]').forEach(button => {
        if (button.dataset.action === key) actionState(button, false);
      });
    }
  };
  parent.append(element);
  return element;
}
function label(limit, provider) {
  const m = limit.window_minutes;
  const duration = !m ? '' : m % 1440 === 0 ? `${m / 1440}-day` : m % 60 === 0 ? `${m / 60}-hour` : `${m}-minute`;
  const name = limit.label && limit.label !== duration && limit.label.toLowerCase() !== provider ? limit.label : '';
  return [duration, name].filter(Boolean).join(' · ') || limit.id;
}
// A window whose reset is unknown, or always a full window away, has not started: usage begins the clock.
// A reported reset that already passed has ended. Codex reports a started window only on a later check,
// so a completed start request counts as started until its window would end.
function startedReset(limit, started) {
  const span = (limit.window_minutes || 0) * 60;
  return span && started && started + span > Date.now() / 1000 ? started + span : null;
}
function notStarted(limit, observed, started) {
  if (!limit.window_minutes || startedReset(limit, started)) return false;
  if (limit.resets_at && limit.resets_at <= Date.now() / 1000) return true;
  return limit.used_percent === 0 && (!limit.resets_at || limit.resets_at - observed >= limit.window_minutes * 60 - 120);
}
// While Codex still reports the pre-start state, the reset follows from the start request.
function resetAt(profile, limit) {
  const fresh = limit.used_percent === 0 && startedReset(limit, profile.window_started_at);
  return fresh && (!limit.resets_at || limit.resets_at - profile.snapshot.observed_at >= limit.window_minutes * 60 - 120) ? fresh : limit.resets_at;
}
// The session window is the shortest one; the server's daily auto-start uses the same choice.
function sessionLimit(profile) {
  const limits = (profile.snapshot?.limits || []).filter(l => l.window_minutes > 0);
  return limits.reduce((best, l) => !best || l.window_minutes < best.window_minutes ? l : best, null);
}
const playIcon = '<svg viewBox="0 0 10 10" width="10" height="10" aria-hidden="true"><path d="M2 1l7 4-7 4z" fill="currentColor"/></svg>';
function resetLine(profile, limit) {
  const line = node('div', undefined, 'aa-reset');
  if (notStarted(limit, profile.snapshot.observed_at, profile.window_started_at)) line.append(node('span', 'Not started', 'tm-meta'));
  else {
    const reset = resetAt(profile, limit);
    line.append(node('span', reset ? 'Resets ' + time(reset) : 'Reset time unknown', 'tm-meta'));
  }
  if (profile.enabled && providerCapabilities.get(profile.provider)?.includes('start_window') && limit === sessionLimit(profile)) {
    const name = label(limit, profile.provider);
    const start = button(line, '', () => backgroundOperation(`/profiles/${profile.id}/window`, {}, profile.id), `window:${profile.id}`);
    const run = start.onclick;
    start.onclick = async () => { if (await windowDialog(profile, limit)) await run(); };
    start.className = 'tm-btn tm-btn--ghost tm-btn--icon tm-btn--sm aa-start';
    start.insertAdjacentHTML('afterbegin', playIcon);
    const startLabel = `${name} window: start or schedule`;
    start.setAttribute('aria-label', startLabel);
    line.append(withTip(start, startLabel));
    if (profile.auto_start) line.append(node('span', `Auto-start ${profile.auto_start.time}`, 'tm-meta'));
  }
  return line;
}
const browserZone = Intl.DateTimeFormat().resolvedOptions().timeZone;
let windowProfile = null;
/** Resolves true when the owner chose to start the window now. */
function windowDialog(profile, limit) {
  windowProfile = profile;
  const dialog = $('#window-dialog'), name = label(limit, profile.provider);
  const idle = notStarted(limit, profile.snapshot.observed_at, profile.window_started_at), reset = resetAt(profile, limit);
  setText($('#window-title'), `${name[0].toUpperCase()}${name.slice(1)} window`);
  setText($('#window-status'), !profile.enabled ? 'Profile disabled' : idle ? 'Not started' : reset ? `Running · resets ${time(reset)}` : 'Running');
  setText($('#window-reset'), time(Math.floor(Date.now() / 1000) + limit.window_minutes * 60));
  $('#window-start').disabled = !idle || !profile.enabled;
  $('#auto-time').value = profile.auto_start?.time || '';
  $('#auto-tolerance').value = profile.auto_start?.tolerance_minutes ?? 60;
  $('#auto-tolerance').dispatchEvent(new Event('input', { bubbles: true }));
  $('#auto-clear').disabled = !profile.auto_start;
  setText($('#auto-zone'), `Time zone: ${browserZone}`);
  dialog.returnValue = '';
  dialog.showModal();
  return new Promise(resolve => dialog.addEventListener('close', () => resolve(dialog.returnValue === 'start'), { once: true }));
}
async function saveAutoStart(value) {
  try {
    await api(`/profiles/${windowProfile.id}`, 'PATCH', { auto_start: value });
    $('#window-dialog').close(); await refresh();
  } catch (error) { notice(error); }
}
async function details(profile) {
  const content = $('#detail-content');
  content.replaceChildren();
  const facts = node('dl', undefined, 'aa-facts');
  for (const [title, value] of [
    ['Profile', profile.display_name], ['Provider', profile.provider],
    ['Account', profile.account?.email || 'Not identified'],
    ['Plan', profile.account?.plan || 'Unknown'], ['Authentication', profile.auth_status.replaceAll('_', ' ')],
    ['Monitoring', profile.enabled ? 'Enabled' : 'Disabled'],
    ['Measurement scope', 'Account'], ['Profile ID', profile.id],
  ]) facts.append(node('dt', title), node('dd', value));
  content.append(facts);
  if (profile.snapshot?.reset_credits) content.append(node('p', `Available earned resets: ${profile.snapshot.reset_credits.availableCount ?? 'Unknown'}`));
  const history = await api(`/profiles/${profile.id}/history`);
  if (maskIdentities) for (const snapshot of history) if (snapshot.account.email) snapshot.account.email = maskEmail(snapshot.account.email);
  content.append(node('h3', 'Recent successful checks', 'aa-h3'));
  if (!history.length) content.append(node('p', 'No successful measurements yet.', 'tm-meta'));
  const recent = history.slice(0, 20);
  if (recent.length) {
    const columns = [...new Set(recent.flatMap(snapshot => snapshot.limits.map(l => label(l, profile.provider))))];
    const wrap = node('div', undefined, 'tm-table-wrap aa-history');
    const table = node('table', undefined, 'tm-table'), thead = node('thead'), head = node('tr'), body = node('tbody');
    for (const title of ['Checked', ...columns]) { const th = node('th', title, title === 'Checked' ? undefined : 'tm-col-num'); th.scope = 'col'; head.append(th); }
    thead.append(head); table.append(thead, body);
    for (const snapshot of recent) {
      const row = node('tr');
      row.append(node('td', time(snapshot.observed_at), 'aa-mono'));
      for (const column of columns) {
        const limit = snapshot.limits.find(l => label(l, profile.provider) === column);
        row.append(node('td', limit?.used_percent == null ? '—' : `${limit.used_percent.toFixed(1)}% used`, 'tm-col-num tm-num'));
      }
      if (snapshot.account.email && snapshot.account.email !== profile.account?.email) row.append(node('td', snapshot.account.email, 'tm-text-danger'));
      body.append(row);
    }
    wrap.append(table); content.append(wrap);
  }
  if (profile.error) content.append(message(profile.error.message, 'error'));
  $('#details').showModal();
}
/** A short attention line (warning or error) with a shape marker, so state is never color alone. */
function message(text, state, tag = 'p') {
  // Inline problem text is the size of the text it annotates: no icon twice its height.
  return node(tag, text, `aa-msg aa-msg--${state}`);
}
/** Routine facts as quiet text; only deviations (disabled, auth problem) become badges. */
// One account line of fixed height: email · plan (· auth problem), and on the
// right the quota-group switch where a provider has groups. Enabled/disabled is
// shown by the card itself; "authenticated" is the normal case and not repeated.
function accountLine(profile, email) {
  const line = node('div', undefined, 'aa-account-line');
  const facts = node('p', undefined, 'aa-status');
  const parts = [];
  if (email) parts.push(node('span', email, 'aa-email'));
  if (profile.auth_status !== 'authenticated') parts.push(node('span', profile.auth_status.replaceAll('_', ' '), 'aa-auth-problem'));
  parts.forEach((part, index) => { if (index) facts.append(node('span', '·', 'aa-sep')); facts.append(part); });
  line.append(facts);
  return line;
}
async function refresh() {
  const all = maskIdentities ? masked(await api('/profiles')) : await api('/profiles');
  const profiles = hideFree ? all.filter(p => p.account?.plan?.toLowerCase() !== 'free') : all;
  $('#profiles').replaceChildren();
  if (!all.length) $('#profiles').append(node('p', 'No profiles yet. Add an account to begin.', 'aa-empty'));
  else if (!profiles.length) $('#profiles').append(node('p', 'All accounts are on the free plan and hidden.', 'aa-empty'));
  for (const profile of profiles) {
    const card = node('article', undefined, `tm-panel aa-account provider-${profile.provider}${profile.enabled ? '' : ' is-inactive'}`);
    card.setAttribute('aria-labelledby', `title-${profile.id}`); card.id = `card-${profile.id}`;
    const header = node('header', undefined, 'tm-panel__head tm-panel__head--rich aa-account__head');
    const identity = node('div', undefined, 'tm-panel__heading');
    const title = node('h3', profile.display_name, 'tm-panel__title aa-account__title');
    title.id = `title-${profile.id}`;
    // Plan changes rarely, so it sits with the name: "Lab account · max", thin.
    if (profile.account?.plan) title.append(' ', node('span', `· ${profile.account.plan}`, 'aa-plan'));
    identity.append(node('span', profile.provider, 'tm-eyebrow aa-provider'), title);
    header.append(identity);
    // Every account can take a payment date; a provider-reported period is used
    // until one is entered.
    const manual = !!profile.last_payment || !profile.account?.period_end;
    const start = manual ? profile.last_payment : profile.account?.period_start;
    const end = manual ? start && addMonths(start, 1) : profile.account?.period_end;
    {
      const text = end ? periodLeft(end) : 'Set last payment';
      const left = node('button', text, 'tm-btn tm-btn--ghost tm-btn--sm aa-period aa-period--manual');
      left.type = 'button';
      const info = end ? `Period start: ${time(start)}\nPeriod end: ${time(end)}${manual ? '' : '\nReported by the provider; click to enter a payment date.'}` : 'Enter the time of the latest receipt';
      left.setAttribute('aria-description', info);
      if (!end || end <= Date.now() / 1000) left.classList.add('aa-period--unset');
      left.onclick = () => editPayment(profile);
      header.append(withTip(left, info, 'end'));
    }
    card.append(header);
    const body = node('div', undefined, 'tm-panel__body aa-account__body');
    const email = profile.account?.email;
    const accLine = accountLine(profile, email && email !== profile.display_name ? email : '');
    body.append(accLine);
    const groups = profile.provider === 'antigravity' ? [...new Set((profile.snapshot?.limits || []).map(l => l.model).filter(Boolean))] : [];
    let selectedGroup = quotaGroups.get(profile.id);
    if (!groups.includes(selectedGroup)) selectedGroup = groups.find(g => g === 'Gemini 4 Argon') || groups[0];
    if (groups.length) {
      const select = node('select', undefined, 'tm-select aa-group');
      select.setAttribute('aria-label', 'Quota group');
      for (const group of groups) { const option = node('option', group === 'Claude and GPT models' ? 'Claude & GPT' : group === 'Gemini Models' ? 'Gemini' : group); option.value = group; select.append(option); }
      select.value = selectedGroup;
      select.onchange = () => { quotaGroups.set(profile.id, select.value); refresh().catch(notice); };
      select.classList.add('tm-input--sm');
      accLine.append(select);
    }
    if (profile.snapshot) {
      if (profile.snapshot.account.email && profile.snapshot.account.email !== profile.account?.email) body.append(message(`Previous measurement account: ${profile.snapshot.account.email}`, 'error'));
      const quotas = node('div', undefined, 'aa-quotas');
      const visibleLimits = profile.snapshot.limits.filter(l => !groups.length || l.model === selectedGroup);
      if (profile.provider === 'antigravity') visibleLimits.sort((a, b) => (a.window_minutes ?? Infinity) - (b.window_minutes ?? Infinity));
      for (const limit of visibleLimits) {
        const quota = node('div', undefined, 'aa-quota'), line = node('div', undefined, 'aa-quota__line');
        const name = profile.provider === 'antigravity' ? label({...limit, label: ''}, profile.provider) : label(limit, profile.provider);
        const exhausted = limit.exhausted ?? (limit.used_percent != null && limit.used_percent >= 100);
        const head = node('span', undefined, 'aa-quota__head');
        head.append(node('span', name, 'aa-quota__name'));
        if (exhausted) head.append(node('span', '· limit reached', 'aa-quota__flag'));
        line.append(head, node('span', percent(limit.used_percent), 'tm-num aa-quota__num'));
        if (exhausted) quota.classList.add('is-exhausted');
        quota.append(line);
        if (limit.used_percent != null) {
          const bar = node('progress', undefined, 'tm-progress aa-bar');
          bar.max = 100; bar.value = Math.min(100, limit.used_percent);
          bar.setAttribute('aria-label', `${name}: ${percent(limit.used_percent)}`);
          quota.append(bar);
        }
        quota.append(resetLine(profile, limit));
        quotas.append(quota);
      }
      body.append(quotas);
    } else body.append(node('p', 'Usage unavailable', 'tm-meta'));
    card.append(body);
    const foot = node('div', undefined, 'tm-panel__foot aa-account__foot');
    const lastCheck = node('span', 'Last successful check ' + time(profile.last_success), 'tm-meta');
    if (profile.stale && profile.enabled) lastCheck.append(node('span', ' · stale', 'aa-stale'));
    const elapsed = Date.now() - (successfulChecks.get(profile.id) || 0);
    if (elapsed < 2400) {
      lastCheck.classList.add('aa-check-success');
      lastCheck.style.animationDelay = `-${elapsed}ms`;
    } else successfulChecks.delete(profile.id);
    foot.append(lastCheck);
    const actions = node('div', undefined, 'aa-actions'), group = node('div', undefined, 'aa-actions__main');
    button(group, 'Check now', async () => { await backgroundOperation(`/profiles/${profile.id}/limits`, {}, profile.id); done(profile.display_name, 'Usage checked.', `card-${profile.id}`); }, `probe:${profile.id}`, !profile.enabled);
    button(group, 'Details', () => details(profile));
    if (providerCapabilities.get(profile.provider)?.includes('account_statistics')) button(group, 'Statistics', async () => showOperation(await api(`/profiles/${profile.id}/statistics`, 'POST', {}))).disabled = !profile.enabled;
    group.firstChild.classList.replace('tm-btn--ghost', 'tm-btn--outline');
    const more = node('div', undefined, 'aa-actions__more');
    button(more, profile.account ? 'Reauthenticate' : 'Authenticate', async () => showOperation(await api(`/profiles/${profile.id}/authentication`, 'POST', {})));
    // The action names what it does, but both labels share one width so the
    // buttons after it never move when it flips.
    const toggleBtn = button(more, '', async () => { await api(`/profiles/${profile.id}`, 'PATCH', { enabled: !profile.enabled }); await refresh(); done(profile.display_name, profile.enabled ? 'Monitoring disabled.' : 'Monitoring enabled.', `card-${profile.id}`); });
    toggleBtn.append(swapLabel(profile.enabled ? 'Disable' : 'Enable', profile.enabled ? 'Enable' : 'Disable'));
    actions.append(group, more);
    button(more, 'Delete', async () => {
      await api(`/profiles/${profile.id}`, 'DELETE', { confirm: true }); await refresh();
      done(profile.display_name, 'Account deleted.');
    }, `delete:${profile.id}`, false, () => confirmDialog({
      title: `Delete "${profile.display_name}"?`,
      body: `Permanently delete "${profile.display_name}"? This removes credentials, configuration and all local history.`,
      confirm: 'Delete', danger: true,
    }));
    foot.append(actions); card.append(foot); $('#profiles').append(card);
  }
  // Both sections share one column count, set by the accounts: 2 to 4.
  document.querySelector('.aa-main').style.setProperty('--aa-cols', Math.min(4, Math.max(2, profiles.length)));
  const runtimes = await api('/runtimes');
  syncIssueToasts(all, runtimes);
  $('#runtimes').replaceChildren();
  for (const runtime of runtimes) {
    const row = node('article', undefined, `tm-panel aa-runtime provider-${runtime.provider}`); row.id = `rt-${runtime.provider}`;
    const head = node('header', undefined, 'tm-panel__head tm-panel__head--rich');
    const versions = node('span', `${runtime.version} pinned · latest `, 'tm-panel__meta aa-versions');
    const newer = runtime.latest_version && compareVersions(runtime.latest_version, runtime.version) > 0;
    versions.append(node('span', runtime.latest_version || 'not checked', newer ? 'aa-newer' : undefined));
    const name = node('h3', runtimeName(runtime.provider), 'tm-panel__title');
    // Details on demand: the version line carries them instead of four lines of text.
    const details = [runtime.previous_version && `Retained version: ${runtime.previous_version}`, runtime.verified_at && `Usage verified ${time(runtime.verified_at)}`, !runtime.last_error && runtime.last_result].filter(Boolean).join('\n');
    let versionsEl = versions;
    if (details) {
      // No info icon: the version line itself carries the details (dotted underline, like "Ends in").
      versions.classList.add('aa-has-details'); versions.tabIndex = 0;
      versions.setAttribute('aria-description', details.replaceAll('\n', '; '));
      versionsEl = withTip(versions, details, 'end');
    }
    head.append(name, versionsEl);
    const body = node('div', undefined, 'tm-panel__body aa-runtime__body');
    body.append(node('span', `Last version check ${time(runtime.checked_at)}`, 'tm-meta aa-nowrap'));
    const actions = node('div', undefined, 'aa-actions__main aa-runtime__actions');
    // One strong action: Upgrade when a newer version exists, otherwise the check.
    const check = button(actions, 'Check available version', async () => { await backgroundOperation(`/runtimes/${runtime.provider}/check`, {}); done(runtimeName(runtime.provider), 'Available version checked.', `rt-${runtime.provider}`); }, `version:${runtime.provider}`);
    const upgrade = button(actions, 'Upgrade', async () => { await backgroundOperation(`/runtimes/${runtime.provider}/upgrade`, { version: runtime.latest_version }); done(runtimeName(runtime.provider), `Upgraded to ${runtime.latest_version}.`, `rt-${runtime.provider}`); }, `upgrade:${runtime.provider}`, !runtime.latest_version || compareVersions(runtime.latest_version, runtime.version) <= 0);
    (newer ? upgrade : check).classList.replace('tm-btn--ghost', 'tm-btn--outline');
    if (runtime.previous_version && compareVersions(runtime.previous_version, runtime.version) < 0) {
      button(actions, 'Rollback', async () => {
        await backgroundOperation(`/runtimes/${runtime.provider}/rollback`, { version: runtime.previous_version, confirm: true });
        done(runtimeName(runtime.provider), `Rolled back to ${runtime.previous_version}.`, `rt-${runtime.provider}`);
      }, `rollback:${runtime.provider}`, false, () => confirmDialog({
        title: `Roll back ${runtimeName(runtime.provider)}?`,
        body: `Roll back ${runtimeName(runtime.provider)} from ${runtime.version} to ${runtime.previous_version}? The retained version will be checked before activation. Account credentials and history are preserved.`,
        confirm: 'Rollback',
      }));
    }
    body.append(actions); row.append(head, body); $('#runtimes').append(row);
  }
  setText($('#versions'), [appVersion, ...runtimes.map(r => `${runtimeName(r.provider)} ${r.version}`)].join(' · '));
}
const runtimeName = provider => ({ codex: 'Codex CLI', claude: 'Claude Code' })[provider] || provider;
const appVersion = document.querySelector('#versions').textContent;
function compareVersions(a, b) {
  const left = a.split('.').map(Number), right = b.split('.').map(Number);
  for (let i = 0; i < 3; i++) if (left[i] !== right[i]) return left[i] - right[i];
  return 0;
}
async function backgroundOperation(path, body, profileId) {
  const operation = await api(path, 'POST', body);
  const result = operation.finished_at ? operation : await new Promise((resolve, reject) => {
    const events = new EventSource(`/api/v1/operations/${operation.id}/events`);
    events.addEventListener('operation', event => {
      const value = JSON.parse(event.data);
      if (value.finished_at) { events.close(); resolve(value); }
    });
    events.onerror = () => {
      if (events.readyState === EventSource.CLOSED) {
        events.close(); reject(Error('Progress connection lost. Refresh to check the operation status.'));
      }
    };
  });
  if (result.status === 'succeeded' && profileId) successfulChecks.set(profileId, Date.now());
  await refresh();
  if (result.status === 'failed') throw Error(result.error?.message || result.message);
}
function showOperation(operation) {
  selectedOperation = operation;
  $('#operation h2').textContent = ({ authentication: 'Authentication', probe: 'Usage check', upgrade: 'Runtime upgrade', update_check: 'Available runtime version', statistics: 'Account statistics', start_window: 'Start usage window' })[operation.kind] || 'Operation';
  if (!$('#operation').open) $('#operation').showModal();
  source?.close(); updateOperation(operation);
  if (!operation.finished_at) {
    source = new EventSource(`/api/v1/operations/${operation.id}/events`);
    source.addEventListener('operation', event => updateOperation(JSON.parse(event.data)));
    source.onerror = () => { $('#progress').textContent = 'Progress connection interrupted. Reopen the operation or refresh.'; };
  }
}
function updateOperation(operation) {
  selectedOperation = operation;
  setText($('#progress'), operation.message);
  setText($('#operation-result'), operation.result ? JSON.stringify(operation.result, null, 2) : '');
  $('#auth-url').hidden = !operation.authorization_url;
  if (operation.authorization_url && $('#auth-url').href !== operation.authorization_url) $('#auth-url').href = operation.authorization_url;
  setText($('#device-code'), operation.user_code || '');
  $('#copy-code').hidden = !operation.user_code;
  $('#code-form').hidden = !operation.accepts_code;
  $('#cancel-operation').hidden = !!operation.finished_at;
  if (operation.finished_at) {
    source?.close();
    if (operation.kind === 'authentication') {
      $('#operation').close();
      if (operation.status === 'failed') notice(operation.error?.message || operation.message);
    }
    refresh().catch(notice);
  }
}
$('#copy-code').onclick = async () => {
  const code = $('#device-code');
  try { await navigator.clipboard.writeText(code.textContent); $('#copy-code').textContent = 'Copied'; }
  catch { getSelection().selectAllChildren(code); $('#copy-code').textContent = 'Selected — press Ctrl+C'; }
  setTimeout(() => { $('#copy-code').textContent = 'Copy code'; }, 2000);
};
$('#cancel-operation').onclick = () => selectedOperation && api(`/operations/${selectedOperation.id}`, 'DELETE', {}).catch(notice);
$('#code-form').onsubmit = async event => {
  event.preventDefault(); if (!selectedOperation?.profile_id) return;
  try {
    await api(`/profiles/${selectedOperation.profile_id}/authentication/code`, 'POST', { code: $('#auth-code').value });
    $('#auth-code').value = ''; $('#code-form').hidden = true;
  } catch (error) { notice(error); }
};
$('#payment-form').onsubmit = event => {
  event.preventDefault();
  const value = parseInput($('#payment-at').value);
  if (value == null) { setFieldError($('#payment-at'), 'Use DD.MM.YYYY HH:MM, for example 09.11.2026 16:55'); $('#payment-at').focus(); return; }
  savePayment(value);
};
$('#payment-next').onclick = () => {
  const value = parseInput($('#payment-at').value);
  if (value != null) $('#payment-at').value = localInput(addMonths(value, 1));
};
$('#payment-clear').onclick = () => savePayment(null);
$('#window-start').onclick = () => $('#window-dialog').close('start');
$('#window-form').onsubmit = event => {
  event.preventDefault();
  const m = $('#auto-time').value.trim().match(/^(\d{1,2}):(\d{2})$/);
  if (!m || +m[1] > 23 || +m[2] > 59) { setFieldError($('#auto-time'), 'Use HH:MM, for example 08:00'); $('#auto-time').focus(); return; }
  saveAutoStart({ time: `${pad(+m[1])}:${m[2]}`, tolerance_minutes: +$('#auto-tolerance').value, timezone: browserZone });
};
$('#auto-clear').onclick = () => saveAutoStart(null);
$('#add').onclick = () => $('#add-dialog').showModal();
document.querySelectorAll('[data-close]').forEach(button => { button.onclick = () => button.closest('dialog').close(); });
$('#create').onsubmit = async event => {
  event.preventDefault();
  try {
    const profile = await api('/profiles', 'POST', Object.fromEntries(new FormData(event.target)));
    $('#add-dialog').close(); event.target.reset(); await refresh();
    showOperation(await api(`/profiles/${profile.id}/authentication`, 'POST', {}));
  } catch (error) { notice(error); }
};
// Three themes (The Mechanics), one quiet icon button that cycles
// Light → Dark → Coffee → Light; the icon shows the current theme.
// The head script applies the saved theme before paint.
const themes = ['light', 'dark', 'coffee'];
const themeInfo = {
  light: { label: 'Light', icon: 'M8 11a3 3 0 1 0 0-6 3 3 0 0 0 0 6zM8 1v1.5M8 13.5V15M1 8h1.5M13.5 8H15M3 3l1 1M12 12l1 1M3 13l1-1M12 4l1-1' },
  dark: { label: 'Dark', icon: 'M13.5 9.5A6 6 0 0 1 6.5 2.5a6 6 0 1 0 7 7z' },
  coffee: { label: 'Coffee', icon: 'M2.5 5.5h9v4a4 4 0 0 1-4 4h-1a4 4 0 0 1-4-4zM11.5 6.5h1.5a1.5 1.5 0 0 1 0 3h-1.5M5 1.5v2M8 1.5v2' },
};
function theme(id) {
  const root = document.documentElement;
  if (id && themes.includes(id)) {
    root.classList.add('tm-theme-switching');
    root.dataset.tmTheme = id;
    requestAnimationFrame(() => requestAnimationFrame(() => root.classList.remove('tm-theme-switching')));
    try { localStorage.setItem('tm-theme', id); } catch {}
  }
  const current = themes.includes(root.dataset.tmTheme) ? root.dataset.tmTheme : 'light';
  const next = themes[(themes.indexOf(current) + 1) % themes.length];
  $('#theme-icon').setAttribute('d', themeInfo[current].icon);
  const text = `Theme: ${themeInfo[current].label} · click for ${themeInfo[next].label}`;
  setText($('#theme-tip'), text);
  $('#theme').setAttribute('aria-label', text);
}
$('#theme').onclick = () => {
  const current = document.documentElement.dataset.tmTheme;
  theme(themes[(themes.indexOf(current) + 1) % themes.length]);
};
theme();
let hideFree = true;
try { hideFree = localStorage.getItem('hideFree') !== 'false'; } catch {}
$('#hide-free').checked = hideFree;
$('#hide-free').onchange = () => {
  hideFree = $('#hide-free').checked;
  try { localStorage.setItem('hideFree', hideFree); } catch {}
  refresh().catch(notice);
};
// Masked identities for screenshots: names become account_one, account_two…,
// emails user_one@example.com…; numbering follows the profile order.
let maskIdentities = false;
try { maskIdentities = localStorage.getItem('maskIdentities') === 'true'; } catch {}
const numberWords = ['one', 'two', 'three', 'four', 'five', 'six', 'seven', 'eight', 'nine', 'ten', 'eleven', 'twelve'];
const numberWord = index => numberWords[index] ?? String(index + 1);
const maskedEmails = new Map();
function maskEmail(email) {
  if (!maskedEmails.has(email)) maskedEmails.set(email, `user_${numberWord(maskedEmails.size)}@example.com`);
  return maskedEmails.get(email);
}
function masked(profiles) {
  return profiles.map((p, index) => {
    const email = p.account?.email, snapshotEmail = p.snapshot?.account?.email;
    return {
      ...p,
      display_name: p.display_name.includes('@') ? maskEmail(p.display_name) : `account_${numberWord(index)}`,
      account: p.account && { ...p.account, email: email && maskEmail(email) },
      snapshot: p.snapshot && { ...p.snapshot, account: { ...p.snapshot.account, email: snapshotEmail && maskEmail(snapshotEmail) } },
    };
  });
}
function showMask() {
  const text = maskIdentities ? 'Names and emails hidden · click to show' : 'Hide names and emails';
  $('#mask').setAttribute('aria-pressed', maskIdentities);
  $('#mask').setAttribute('aria-label', text);
  setText($('#mask-tip'), text);
}
$('#mask').onclick = () => {
  maskIdentities = !maskIdentities;
  try { localStorage.setItem('maskIdentities', maskIdentities); } catch {}
  showMask();
  // Problem toasts carry account names: rebuild them in the new form.
  for (const el of issueToasts.values()) el.remove();
  issueToasts.clear();
  $('#details').close();
  refresh().catch(notice);
};
showMask();
const pageVersion = document.querySelector('meta[name="agent-accounts-version"]').content;
let providersLoaded = false, disconnected = false, polling = false, pollTimer = null, retryDelay = 2000;
const telegramLabels = {
  verified: 'Telegram notifications verified by a successful message',
  disabled: 'Telegram notifications are not configured',
  failed: 'Telegram notifications failed: invalid configuration or the last message was not delivered',
};
function showTelegram(status) {
  const el = $('#telegram-status');
  el.hidden = !(status in telegramLabels);
  el.dataset.status = status;
  setText($('#telegram-tip'), telegramLabels[status] ?? '');
  el.setAttribute('aria-label', telegramLabels[status] ?? '');
}
// One poll cycle: reload the page after a server upgrade, otherwise refresh the data.
async function poll() {
  if (polling) return;
  polling = true; clearTimeout(pollTimer);
  try {
    const { version } = await api('/version');
    if (version !== pageVersion) { location.reload(); return; }
    showTelegram((await api('/notifications/telegram')).status);
    if (!providersLoaded) {
      for (const provider of await api('/providers')) {
        providerCapabilities.set(provider.id, provider.capabilities);
        const option = node('option', provider.id); option.value = provider.id; $('#providers').append(option);
      }
      providersLoaded = true;
    }
    await refresh();
    if (disconnected) { disconnected = false; setConnectionLost(false); }
    retryDelay = 2000;
    pollTimer = setTimeout(poll, 15000);
  } catch {
    showTelegram('pending');
    disconnected = true;
    setConnectionLost(true);
    pollTimer = setTimeout(poll, retryDelay);
    retryDelay = Math.min(retryDelay * 2, 15000);
  } finally { polling = false; }
}
document.addEventListener('visibilitychange', () => { if (!document.hidden) poll(); });
addEventListener('online', poll);
poll();

/* Per-account problems (stale data, failed checks) are toasts, not card text:
   one toast per account and problem, shown while the problem lasts, removed
   by itself when it clears; ✕ hides it until a new problem appears. */
const issueToasts = new Map(), dismissedIssues = new Set();
// One problem per account; the key includes the attempt time, so a later
// failed check is a new problem and shows again after a dismissal.
function issueOf(p) {
  if (!p.enabled) return null;
  if (p.error) return { key: `${p.id}:${p.error.code}:${p.last_attempt}`, panel: `card-${p.id}`, tone: 'danger', title: p.display_name, body: p.error.message, short: /timeout/.test(p.error.code) ? 'check timed out' : 'check failed' };
  if (p.stale) return { key: `${p.id}:stale:${p.last_attempt}`, panel: `card-${p.id}`, tone: 'warning', title: p.display_name, body: 'Stale — awaiting a successful check.', short: 'stale' };
  return null;
}
function runtimeIssueOf(r) {
  return r.last_error ? { key: `rt:${r.provider}:${r.checked_at}`, panel: `rt-${r.provider}`, tone: 'danger', title: `Provider runtimes · ${runtimeName(r.provider)}`, body: r.last_result || r.last_error.message, short: 'check failed' } : null;
}
function syncIssueToasts(profiles, runtimes = []) {
  const region = document.querySelector('.tm-toast-region');
  const current = new Map();
  for (const p of profiles) { const i = issueOf(p); if (i) current.set(i.key, i); }
  for (const r of runtimes) { const i = runtimeIssueOf(r); if (i) current.set(i.key, i); }
  for (const [key, el] of issueToasts) if (!current.has(key)) { el.remove(); issueToasts.delete(key); }
  for (const key of dismissedIssues) if (!current.has(key)) dismissedIssues.delete(key);
  for (const [key, t] of current) {
    if (issueToasts.has(key) || dismissedIssues.has(key)) continue;
    const el = makeToast({ ...t, timeout: 0, onClose: () => {
      dismissedIssues.add(key); issueToasts.delete(key);
    } });
    region.append(el); issueToasts.set(key, el);
  }
}

/* Confirmation of an action the user just took: a toast that leaves by itself
   (errors stay until closed — see syncIssueToasts). */
function done(title, body, panel) {
  document.querySelector('.tm-toast-region').prepend(makeToast({ tone: 'success', title, body, panel, timeout: 5000 }));
}

/* One toast for everything (as in The Mechanics gallery): tone icon, close
   button, and — for toasts that leave by themselves — a countdown bar.
   Hover or keyboard focus pauses the countdown, leaving resumes it, a click
   on the toast keeps it (the bar goes). Errors and warnings never leave by
   themselves (timeout 0). Hover/focus also outlines the panel the toast is
   about, in the toast's tone; a click on the title scrolls to that panel. */
const TOAST_ICON = { success: 'M8 14.5a6.5 6.5 0 1 0 0-13 6.5 6.5 0 0 0 0 13zM5 8l2 2 4-4', warning: 'M8 1.5l6.5 12h-13zM8 6v3.5M8 11.5v.01', danger: 'M8 14.5a6.5 6.5 0 1 0 0-13 6.5 6.5 0 0 0 0 13zM5.5 5.5l5 5M10.5 5.5l-5 5', info: 'M8 14.5a6.5 6.5 0 1 0 0-13 6.5 6.5 0 0 0 0 13zM8 7v4.5M8 4.5v.01' };
function makeToast({ tone, title, body, panel, timeout = 5000, onClose }) {
  const el = node('div', undefined, `tm-alert tm-alert--${tone} tm-toast`);
  el.setAttribute('role', tone === 'danger' ? 'alert' : 'status');
  el.insertAdjacentHTML('afterbegin', `<svg class="tm-icon" viewBox="0 0 16 16" aria-hidden="true" focusable="false"><path d="${TOAST_ICON[tone]}"/></svg>`);
  const text = node('div');
  const head = node(panel ? 'button' : 'div', title, `tm-alert__title${panel ? ' aa-toast__to' : ''}`);
  if (panel) { head.type = 'button'; head.onclick = () => document.getElementById(panel)?.scrollIntoView({ block: 'nearest', behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth' }); }
  text.append(head, node('div', body, 'tm-alert__body'));
  const close = node('button', undefined, 'tm-btn tm-btn--ghost tm-btn--icon tm-btn--sm');
  close.type = 'button'; close.setAttribute('aria-label', `Dismiss: ${title}`); close.innerHTML = clearIcon;
  let gone = false;
  const flag = on => { const p = panel && document.getElementById(panel); if (p) { p.classList.toggle('aa-flag', on && !gone); p.dataset.flag = tone; } };
  // A removed toast gets no mouseleave, so it switches its panel off first.
  const remove = () => { gone = true; flag(false); el.remove(); onClose?.(); };
  close.onclick = remove;
  el.append(text, close);
  let over = false, focused = false;
  const hot = () => flag(over || focused);
  el.addEventListener('mouseenter', () => { over = true; hot(); });
  el.addEventListener('mouseleave', () => { over = false; hot(); });
  el.addEventListener('focusin', () => { focused = true; hot(); });
  el.addEventListener('focusout', e => { if (!el.contains(e.relatedTarget)) { focused = false; hot(); } });
  if (timeout) {
    const bar = node('div', undefined, 'tm-toast__progress'); bar.style.setProperty('--tm-toast-ms', `${timeout}ms`);
    el.append(bar);
    let left = timeout, started = Date.now(), timer = setTimeout(remove, left), kept = false;
    const pause = () => { if (kept || !timer) return; clearTimeout(timer); timer = null; left -= Date.now() - started; bar.style.animationPlayState = 'paused'; };
    const resume = () => { if (kept || timer || over || focused) return; started = Date.now(); timer = setTimeout(remove, Math.max(left, 1500)); bar.style.animationPlayState = 'running'; };
    el.addEventListener('mouseenter', pause); el.addEventListener('focusin', pause);
    el.addEventListener('mouseleave', resume); el.addEventListener('focusout', () => setTimeout(resume));
    el.addEventListener('click', e => { if (e.target.closest('button')) return; kept = true; clearTimeout(timer); bar.remove(); });
  }
  return el;
}

/** A label that reserves the width of its alternative (both stacked, one hidden). */
function swapLabel(shown, other) {
  const box = node('span', undefined, 'aa-swap');
  const alt = node('span', other); alt.setAttribute('aria-hidden', 'true'); alt.className = 'aa-swap__alt';
  box.append(node('span', shown), alt);
  return box;
}

['#create', '#code-form', '#window-form', '#payment-form'].forEach(sel => { const f = $(sel); if (f) validateInline(f); });
