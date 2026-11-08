'use strict';
const $ = selector => document.querySelector(selector);
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
const percent = value => value == null ? 'unknown' : `${value.toFixed(1)}% used · ${Math.max(0, 100 - value).toFixed(1)}% left`;
function setText(element, text) { if (element.textContent !== text) element.textContent = text; }
const notice = error => { $('#notice').textContent = error.message || error; };
function actionState(element, busy) {
  element.disabled = busy || element.dataset.unavailable === 'true';
  element.classList.toggle('busy', busy);
  element.setAttribute('aria-busy', String(busy));
}
function button(parent, label, action, key, unavailable = false) {
  const element = node('button', label);
  element.type = 'button';
  element.dataset.action = key || '';
  element.dataset.unavailable = String(unavailable);
  actionState(element, pendingActions.has(key));
  element.onclick = async () => {
    if (key && pendingActions.has(key)) return;
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
function notStarted(limit, observed) {
  if (limit.used_percent !== 0 || !limit.window_minutes) return false;
  return !limit.resets_at || limit.resets_at - observed >= limit.window_minutes * 60 - 120;
}
const playIcon = '<svg viewBox="0 0 10 10" width="10" height="10" aria-hidden="true"><path d="M2 1l7 4-7 4z" fill="currentColor"/></svg>';
function resetLine(profile, limit) {
  const line = node('div', undefined, 'reset-line');
  const canStart = providerCapabilities.get(profile.provider)?.includes('start_window');
  if (canStart && notStarted(limit, profile.snapshot.observed_at)) {
    line.append(node('span', 'Not started', 'meta'));
    const name = label(limit, profile.provider);
    const start = button(line, '', () => backgroundOperation(`/profiles/${profile.id}/window`, {}, profile.id), `window:${profile.id}`, !profile.enabled);
    const run = start.onclick;
    start.onclick = async () => { if (await confirmStart(name, limit.window_minutes)) await run(); };
    start.classList.add('icon');
    start.insertAdjacentHTML('afterbegin', playIcon);
    start.title = `Start ${name} window`;
    start.setAttribute('aria-label', start.title);
  } else line.append(node('span', limit.resets_at ? 'Resets ' + time(limit.resets_at) : 'Reset time unknown', 'meta'));
  return line;
}
function confirmStart(name, minutes) {
  const dialog = $('#start-dialog');
  setText($('#start-title'), `Start ${name} window?`);
  setText($('#start-reset'), time(Math.floor(Date.now() / 1000) + minutes * 60));
  dialog.returnValue = '';
  dialog.showModal();
  return new Promise(resolve => dialog.addEventListener('close', () => resolve(dialog.returnValue === 'start'), { once: true }));
}
async function details(profile) {
  const content = $('#detail-content');
  content.replaceChildren();
  const facts = node('dl', undefined, 'facts');
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
  content.append(node('h3', 'Recent successful checks'));
  if (!history.length) content.append(node('p', 'No successful measurements yet.', 'muted'));
  const recent = history.slice(0, 20);
  if (recent.length) {
    const columns = [...new Set(recent.flatMap(snapshot => snapshot.limits.map(l => label(l, profile.provider))))];
    const table = node('table', undefined, 'history'), head = node('tr');
    for (const title of ['Checked', ...columns]) head.append(node('th', title));
    table.append(head);
    for (const snapshot of recent) {
      const row = node('tr');
      row.append(node('td', time(snapshot.observed_at)));
      for (const column of columns) {
        const limit = snapshot.limits.find(l => label(l, profile.provider) === column);
        row.append(node('td', limit?.used_percent == null ? '—' : `${limit.used_percent.toFixed(1)}% used`));
      }
      if (snapshot.account.email && snapshot.account.email !== profile.account?.email) row.append(node('td', snapshot.account.email, 'error'));
      table.append(row);
    }
    content.append(table);
  }
  if (profile.error) content.append(node('p', profile.error.message, 'error'));
  $('#details').showModal();
}
async function refresh() {
  const profiles = await api('/profiles');
  $('#profiles').replaceChildren();
  if (!profiles.length) $('#profiles').append(node('p', 'No profiles yet. Add an account to begin.'));
  for (const profile of profiles) {
    const card = node('article', undefined, `card provider-${profile.provider}`);
    const identity = node('div', undefined, 'identity');
    identity.append(node('span', profile.provider, 'eyebrow agent'), node('h2', profile.display_name));
    const email = profile.account?.email;
    identity.append(node('span', email && email !== profile.display_name ? email : '', 'sub slot'));
    const pills = node('div', undefined, 'pills');
    pills.append(node('span', profile.enabled ? 'Enabled' : 'Disabled', profile.enabled ? 'pill' : 'pill off'));
    pills.append(node('span', profile.auth_status.replaceAll('_', ' '), 'pill'));
    if (profile.account?.plan) pills.append(node('span', profile.account.plan, 'pill'));
    const header = node('div', undefined, 'profile-head');
    header.append(identity);
    const groups = profile.provider === 'antigravity' ? [...new Set((profile.snapshot?.limits || []).map(l => l.model).filter(Boolean))] : [];
    let selectedGroup = quotaGroups.get(profile.id);
    if (!groups.includes(selectedGroup)) selectedGroup = groups.find(g => g === 'Gemini 4 Argon') || groups[0];
    if (groups.length) {
      const select = node('select');
      select.setAttribute('aria-label', 'Quota group');
      for (const group of groups) { const option = node('option', group === 'Claude and GPT models' ? 'Claude & GPT' : group === 'Gemini Models' ? 'Gemini' : group); option.value = group; select.append(option); }
      select.value = selectedGroup;
      select.onchange = () => { quotaGroups.set(profile.id, select.value); refresh().catch(notice); };
      header.append(select);
    }
    card.append(header, pills);
    if (profile.snapshot) {
      if (profile.snapshot.account.email && profile.snapshot.account.email !== profile.account?.email) card.append(node('p', `Previous measurement account: ${profile.snapshot.account.email}`, 'error'));
      const quotas = node('div', undefined, 'quotas');
      const visibleLimits = profile.snapshot.limits.filter(l => !groups.length || l.model === selectedGroup);
      if (profile.provider === 'antigravity') visibleLimits.sort((a, b) => (a.window_minutes ?? Infinity) - (b.window_minutes ?? Infinity));
      for (const limit of visibleLimits) {
        const quota = node('div', undefined, 'quota'), line = node('div', undefined, 'quota-line');
        line.append(node('span', profile.provider === 'antigravity' ? label({...limit, label: ''}, profile.provider) : label(limit, profile.provider)), node('span', percent(limit.used_percent), 'num'));
        quota.append(line);
        if (limit.used_percent != null) {
          const bar = node('div', undefined, 'bar'), fill = node('span');
          fill.style.width = `${Math.min(100, limit.used_percent)}%`; bar.append(fill); quota.append(bar);
        }
        quota.append(resetLine(profile, limit));
        quota.append(node('span', limit.exhausted ? 'Provider reports an exhausted limit' : '', 'warn slot'));
        quotas.append(quota);
      }
      card.append(quotas);
    } else card.append(node('p', 'Usage unavailable', 'sub'));
    const foot = node('div', undefined, 'card-foot');
    const lastCheck = node('span', 'Last successful check ' + time(profile.last_success), 'meta');
    const elapsed = Date.now() - (successfulChecks.get(profile.id) || 0);
    if (elapsed < 2400) {
      lastCheck.classList.add('check-success');
      lastCheck.style.animationDelay = `-${elapsed}ms`;
    } else successfulChecks.delete(profile.id);
    foot.append(lastCheck);
    if (profile.stale && profile.enabled) foot.append(node('p', 'Stale — awaiting a successful check', 'warn'));
    if (profile.error && profile.enabled) foot.append(node('p', profile.error.message, 'error'));
    const actions = node('div', undefined, 'actions'), group = node('div', undefined, 'seg');
    button(group, 'Check now', () => backgroundOperation(`/profiles/${profile.id}/limits`, {}, profile.id), `probe:${profile.id}`, !profile.enabled);
    button(group, 'Details', () => details(profile));
    if (providerCapabilities.get(profile.provider)?.includes('account_statistics')) button(group, 'Statistics', async () => showOperation(await api(`/profiles/${profile.id}/statistics`, 'POST', {}))).disabled = !profile.enabled;
    group.firstChild.classList.add('primary');
    const more = node('div', undefined, 'more');
    button(more, profile.account ? 'Reauthenticate' : 'Authenticate', async () => showOperation(await api(`/profiles/${profile.id}/authentication`, 'POST', {})));
    button(more, profile.enabled ? 'Disable' : 'Enable', async () => { await api(`/profiles/${profile.id}`, 'PATCH', { enabled: !profile.enabled }); await refresh(); });
    actions.append(group, more);
    button(more, 'Delete', async () => {
      if (confirm(`Permanently delete "${profile.display_name}"? This removes credentials, configuration and all local history.`)) {
        await api(`/profiles/${profile.id}`, 'DELETE', { confirm: true }); await refresh();
      }
    }).classList.add('danger');
    foot.append(actions); card.append(foot); $('#profiles').append(card);
  }
  const runtimes = await api('/runtimes');
  $('#runtimes').replaceChildren();
  for (const runtime of runtimes) {
    const row = node('article', undefined, `runtime provider-${runtime.provider}`), head = node('div', undefined, 'card-head');
    head.append(node('h3', runtimeName(runtime.provider)), node('span', `${runtime.version} pinned · latest ${runtime.latest_version || 'not checked'}`, 'num sub'));
    row.append(head, node('span', `Version checked ${time(runtime.checked_at)} · usage verified ${time(runtime.verified_at)}`, 'meta'));
    if (runtime.previous_version) row.append(node('span', `Retained version: ${runtime.previous_version}`, 'meta'));
    if (runtime.last_result) row.append(node('p', runtime.last_result, runtime.last_error ? 'error' : 'meta'));
    const actions = node('div', undefined, 'actions'), group = node('div', undefined, 'seg');
    button(group, 'Check available version', () => backgroundOperation(`/runtimes/${runtime.provider}/check`, {}), `version:${runtime.provider}`);
    button(group, 'Upgrade', () => backgroundOperation(`/runtimes/${runtime.provider}/upgrade`, { version: runtime.latest_version }), `upgrade:${runtime.provider}`, !runtime.latest_version || compareVersions(runtime.latest_version, runtime.version) <= 0);
    if (runtime.previous_version && compareVersions(runtime.previous_version, runtime.version) < 0) {
      button(group, 'Rollback', async () => {
        if (confirm(`Roll back ${runtimeName(runtime.provider)} from ${runtime.version} to ${runtime.previous_version}? The retained version will be checked before activation. Account credentials and history are preserved.`)) {
          await backgroundOperation(`/runtimes/${runtime.provider}/rollback`, { version: runtime.previous_version, confirm: true });
        }
      }, `rollback:${runtime.provider}`);
    }
    actions.append(group); row.append(actions); $('#runtimes').append(row);
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
let currentTheme = matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
try { currentTheme = localStorage.getItem('theme') || currentTheme; } catch {}
function theme() {
  document.documentElement.dataset.theme = currentTheme;
  $('#theme').textContent = currentTheme === 'dark' ? '☀ Light' : '☾ Dark';
}
$('#theme').onclick = () => {
  currentTheme = currentTheme === 'dark' ? 'light' : 'dark';
  try { localStorage.setItem('theme', currentTheme); } catch {}
  theme();
};
theme();
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
  el.title = telegramLabels[status] ?? '';
  el.setAttribute('aria-label', el.title);
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
    if (disconnected) { disconnected = false; $('#notice').textContent = ''; }
    retryDelay = 2000;
    pollTimer = setTimeout(poll, 15000);
  } catch {
    showTelegram('pending');
    disconnected = true;
    $('#notice').textContent = 'Connection to the service lost. Reconnecting…';
    pollTimer = setTimeout(poll, retryDelay);
    retryDelay = Math.min(retryDelay * 2, 15000);
  } finally { polling = false; }
}
document.addEventListener('visibilitychange', () => { if (!document.hidden) poll(); });
addEventListener('online', poll);
poll();
