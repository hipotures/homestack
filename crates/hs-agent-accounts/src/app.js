'use strict';
const $ = selector => document.querySelector(selector);
let selectedOperation = null;
let source = null;
const providerCapabilities = new Map();
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
function button(parent, label, action) {
  const element = node('button', label);
  element.type = 'button';
  element.onclick = async () => {
    element.disabled = true;
    try { await action(); } catch (error) { notice(error); }
    finally { element.disabled = false; }
  };
  parent.append(element);
  return element;
}
function label(limit) {
  const duration = limit.window_minutes === 300 ? '5-hour' : limit.window_minutes === 10080 ? '7-day' : limit.window_minutes ? `${limit.window_minutes} min` : limit.id;
  return `${limit.label} · ${duration}`;
}
async function details(profile) {
  const content = $('#detail-content');
  content.replaceChildren();
  for (const [title, value] of [
    ['Profile', profile.display_name], ['Provider', profile.provider],
    ['Account', profile.account?.email || 'Not identified'],
    ['Plan', profile.account?.plan || 'Unknown'], ['Authentication', profile.auth_status.replaceAll('_', ' ')],
    ['Monitoring', profile.enabled ? 'Enabled' : 'Disabled'],
    ['Measurement scope', 'Account'], ['Profile ID', profile.id],
  ]) content.append(node('p', `${title}: ${value}`));
  if (profile.snapshot?.reset_credits) content.append(node('p', `Available earned resets: ${profile.snapshot.reset_credits.availableCount ?? 'Unknown'}`));
  const history = await api(`/profiles/${profile.id}/history`);
  content.append(node('h3', 'Recent successful checks'));
  if (!history.length) content.append(node('p', 'No successful measurements yet.', 'muted'));
  const recent = history.slice(0, 20);
  if (recent.length) {
    const columns = [...new Set(recent.flatMap(snapshot => snapshot.limits.map(label)))];
    const table = node('table', undefined, 'history'), head = node('tr');
    for (const title of ['Checked', ...columns]) head.append(node('th', title));
    table.append(head);
    for (const snapshot of recent) {
      const row = node('tr');
      row.append(node('td', time(snapshot.observed_at)));
      for (const column of columns) {
        const limit = snapshot.limits.find(l => label(l) === column);
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
    const card = node('article', undefined, 'card');
    card.append(node('span', profile.provider.toUpperCase(), 'muted'), node('h2', profile.display_name));
    card.append(node('span', profile.enabled ? 'Enabled' : 'Disabled', 'badge'));
    card.append(node('p', profile.account?.email || 'Account not identified', 'muted'));
    card.append(node('p', `${profile.auth_status.replaceAll('_', ' ')}${profile.account?.plan ? ' · ' + profile.account.plan : ''}`));
    if (profile.snapshot) {
      if (profile.snapshot.account.email && profile.snapshot.account.email !== profile.account?.email) card.append(node('p', `Previous measurement account: ${profile.snapshot.account.email}`, 'error'));
      for (const limit of profile.snapshot.limits) {
        const quota = node('div', undefined, 'quota');
        quota.append(node('div', `${label(limit)}: ${percent(limit.used_percent)}`));
        if (limit.used_percent != null) {
          const bar = node('progress'); bar.max = 100; bar.value = Math.min(100, limit.used_percent); quota.append(bar);
        }
        quota.append(node('div', 'Reset: ' + time(limit.resets_at), 'muted'));
        if (limit.exhausted) quota.append(node('p', 'Provider reports an exhausted limit', 'error'));
        card.append(quota);
      }
    } else card.append(node('p', 'Usage unavailable', 'muted'));
    card.append(node('p', 'Last successful check: ' + time(profile.last_success), 'muted'));
    if (profile.stale && profile.enabled) card.append(node('p', 'Stale — awaiting a successful check', 'error'));
    if (profile.error && profile.enabled) card.append(node('p', profile.error.message, 'error'));
    const actions = node('div', undefined, 'actions');
    button(actions, profile.account ? 'Reauthenticate' : 'Authenticate', async () => showOperation(await api(`/profiles/${profile.id}/authentication`, 'POST', {})));
    button(actions, profile.enabled ? 'Disable' : 'Enable', async () => { await api(`/profiles/${profile.id}`, 'PATCH', { enabled: !profile.enabled }); await refresh(); });
    button(actions, 'Check now', async () => showOperation(await api(`/profiles/${profile.id}/limits`, 'POST', {}))).disabled = !profile.enabled;
    button(actions, 'Details', () => details(profile));
    if (providerCapabilities.get(profile.provider)?.includes('account_statistics')) button(actions, 'Account statistics', async () => showOperation(await api(`/profiles/${profile.id}/statistics`, 'POST', {}))).disabled = !profile.enabled;
    button(actions, 'Delete', async () => {
      if (confirm(`Permanently delete "${profile.display_name}"? This removes credentials, configuration and all local history.`)) {
        await api(`/profiles/${profile.id}`, 'DELETE', { confirm: true }); await refresh();
      }
    });
    card.append(actions); $('#profiles').append(card);
  }
  const runtimes = await api('/runtimes');
  $('#runtimes').replaceChildren();
  for (const runtime of runtimes) {
    const row = node('article', undefined, 'runtime');
    row.append(node('h3', runtime.provider), node('p', `Pinned: ${runtime.version} · latest: ${runtime.latest_version || 'not checked'}`));
    row.append(node('p', `Last version check: ${time(runtime.checked_at)} · Usage verification: ${time(runtime.verified_at)}`, 'muted'));
    if (runtime.previous_version) row.append(node('p', `Previous working version retained: ${runtime.previous_version}`, 'muted'));
    if (runtime.last_result) row.append(node('p', runtime.last_result, runtime.last_error ? 'error' : 'muted'));
    const actions = node('div', undefined, 'actions');
    button(actions, 'Check available version', async () => showOperation(await api(`/runtimes/${runtime.provider}/check`, 'POST', {})));
    button(actions, 'Upgrade', async () => showOperation(await api(`/runtimes/${runtime.provider}/upgrade`, 'POST', { version: runtime.latest_version }))).disabled = !runtime.latest_version || compareVersions(runtime.latest_version, runtime.version) <= 0;
    row.append(actions); $('#runtimes').append(row);
  }
}
function compareVersions(a, b) {
  const left = a.split('.').map(Number), right = b.split('.').map(Number);
  for (let i = 0; i < 3; i++) if (left[i] !== right[i]) return left[i] - right[i];
  return 0;
}
function showOperation(operation) {
  selectedOperation = operation;
  $('#operation h2').textContent = ({ authentication: 'Authentication', probe: 'Usage check', upgrade: 'Runtime upgrade', update_check: 'Available runtime version', statistics: 'Account statistics' })[operation.kind] || 'Operation';
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
  if (operation.finished_at) { source?.close(); refresh().catch(notice); }
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
(async () => {
  for (const provider of await api('/providers')) {
    providerCapabilities.set(provider.id, provider.capabilities);
    const option = node('option', provider.id); option.value = provider.id; $('#providers').append(option);
  }
  await refresh();
})().catch(notice);
setInterval(() => refresh().catch(notice), 15000);
