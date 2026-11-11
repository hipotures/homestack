import { query } from '@anthropic-ai/claude-agent-sdk';
import { createInterface } from 'node:readline';

const controller = new AbortController();
let q;
const timer = setTimeout(() => controller.abort(), 45000);
process.on('SIGTERM', () => controller.abort());
process.on('SIGINT', () => controller.abort());
function cancelled() {
  if (controller.signal.aborted) return Promise.resolve();
  return new Promise(resolve => controller.signal.addEventListener('abort', resolve, { once: true }));
}
function isolated(binary) {
  return {
    pathToClaudeCodeExecutable: binary,
    abortController: controller,
    persistSession: false,
    settingSources: [],
    settings: { disableAllHooks: true },
    tools: [],
    allowedTools: [],
    mcpServers: {},
    strictMcpConfig: true,
    env: { ...process.env, ENABLE_CLAUDEAI_MCP_SERVERS: 'false', CLAUDE_CODE_AUTO_CONNECT_IDE: '0', CLAUDE_CODE_IDE_SKIP_AUTO_INSTALL: '1' },
    stderr: () => {},
  };
}
async function usage(binary) {
  // No user message is ever yielded; initialization and control requests only.
  q = query({ prompt: (async function* () { await cancelled(); })(), options: isolated(binary) });
  const init = await q.initializationResult();
  if (typeof q.usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET !== 'function') throw Error('unsupported');
  const usage = await q.usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET({ skipBehaviors: true });
  const account = init.account || {};
  return {
    account: { email: account.email ?? null, plan: usage.subscription_type ?? account.subscriptionType ?? null },
    rate_limits_available: usage.rate_limits_available,
    rate_limits: usage.rate_limits,
  };
}
// One minimal turn: the default system prompt, tools, settings and hooks are replaced or
// disabled so the request stays as small as possible.
async function turn(binary) {
  q = query({
    prompt: 'hi',
    options: { ...isolated(binary), systemPrompt: 'Reply with one word.', model: 'haiku', thinking: { type: 'disabled' }, maxTurns: 1 },
  });
  let done = null;
  const rejected = new Set();
  try {
    for await (const message of q) {
      if (message.type === 'result') done = message;
      const info = message.type === 'rate_limit_event' ? message.rate_limit_info : null;
      if (info?.status === 'rejected' && info.rateLimitType) rejected.add(info.rateLimitType);
    }
  } catch (error) {
    if (!rejected.size) throw error;
  }
  const u = done?.usage || {};
  return {
    succeeded: !!done && done.subtype === 'success' && !done.is_error,
    error: String(done?.result || 'turn failed'),
    rejected: [...rejected],
    tokens: {
      input: u.input_tokens ?? null, cache_creation_input: u.cache_creation_input_tokens ?? null,
      cache_read_input: u.cache_read_input_tokens ?? null, output: u.output_tokens ?? null,
    },
  };
}
// Opens the subscription usage window.
async function start(binary) {
  const result = await turn(binary);
  if (!result.succeeded) throw Error(result.error);
  return { tokens: result.tokens };
}
// Tells whether Claude still serves requests: a completed turn, or the limits that rejected it.
async function check(binary) {
  const result = await turn(binary);
  if (!result.succeeded && !result.rejected.length) throw Error(result.error);
  return { rejected: result.rejected, tokens: result.tokens };
}
let id = null;
let mode = 'usage';
let result;
try {
  const input = createInterface({ input: process.stdin });
  const line = await new Promise(resolve => input.once('line', resolve));
  input.close();
  const request = JSON.parse(line);
  mode = request.mode ?? 'usage';
  if (request.version !== 1 || typeof request.id !== 'string' || typeof request.binary !== 'string' || !request.binary.startsWith('/') || !['usage', 'start', 'check'].includes(mode)) throw Error('protocol');
  id = request.id;
  const data = await { usage, start, check }[mode](request.binary);
  result = { version: 1, id, ok: true, observed_at: Math.floor(Date.now() / 1000), data };
} catch (error) {
  const message = String(error?.message || '').toLowerCase();
  const code = controller.signal.aborted ? 'probe_timeout' : message.includes('unsupported') ? 'unsupported_capability' : /auth|login|401|credential/.test(message) ? 'reauthentication_required' : /network|fetch|connect|dns/.test(message) ? 'network_failure' : 'provider_api_failure';
  result = { version: 1, id, ok: false, error: { code, message: mode === 'start' ? 'Claude could not start the usage window' : 'Claude SDK usage probe could not complete' } };
} finally {
  controller.abort();
  q?.close();
  clearTimeout(timer);
}
process.stdout.write(JSON.stringify(result) + '\n');
