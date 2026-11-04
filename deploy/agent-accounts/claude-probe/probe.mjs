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
let id = null;
let result;
try {
  const input = createInterface({ input: process.stdin });
  const line = await new Promise(resolve => input.once('line', resolve));
  input.close();
  const request = JSON.parse(line);
  if (request.version !== 1 || typeof request.id !== 'string' || typeof request.binary !== 'string' || !request.binary.startsWith('/')) throw Error('protocol');
  id = request.id;
  q = query({
    // No user message is ever yielded; initialization and control requests only.
    prompt: (async function* () { await cancelled(); })(),
    options: {
      pathToClaudeCodeExecutable: request.binary,
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
    },
  });
  const init = await q.initializationResult();
  if (typeof q.usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET !== 'function') throw Error('unsupported');
  const usage = await q.usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET({ skipBehaviors: true });
  const account = init.account || {};
  result = { version: 1, id, ok: true, observed_at: Math.floor(Date.now() / 1000), data: {
    account: { email: account.email ?? null, plan: usage.subscription_type ?? account.subscriptionType ?? null },
    rate_limits_available: usage.rate_limits_available,
    rate_limits: usage.rate_limits,
  }};
} catch (error) {
  const message = String(error?.message || '').toLowerCase();
  const code = controller.signal.aborted ? 'probe_timeout' : message.includes('unsupported') ? 'unsupported_capability' : /auth|login|401|credential/.test(message) ? 'reauthentication_required' : /network|fetch|connect|dns/.test(message) ? 'network_failure' : 'provider_api_failure';
  result = { version: 1, id, ok: false, error: { code, message: 'Claude SDK usage probe could not complete' } };
} finally {
  controller.abort();
  q?.close();
  clearTimeout(timer);
}
process.stdout.write(JSON.stringify(result) + '\n');
