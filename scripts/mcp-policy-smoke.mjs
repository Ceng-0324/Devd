// Real official TypeScript client, pinned separately from Inspector's one-call CLI.
import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { delimiter, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const root = process.env.PATH.split(delimiter)
  .map(path => resolve(path, '..', '@modelcontextprotocol/client'))
  .find(path => existsSync(resolve(path, 'package.json'))
    && JSON.parse(readFileSync(resolve(path, 'package.json'))).version === '2.2.0');
assert(root, 'run with npx -y --package @modelcontextprotocol/client@2.2.0 node');
const { Client } = await import(pathToFileURL(resolve(root, 'dist/index.mjs')));
const { StdioClientTransport } = await import(pathToFileURL(resolve(root, 'dist/stdio.mjs')));
const [binary, cwd, config] = process.argv.slice(2);
const client = new Client({ name: 'devd-policy-acceptance', version: '1' });
const transport = new StdioClientTransport({ command: binary, cwd,
  args: ['mcp', '--stdio', '--config', config, '--allow', 'restart,stop',
    '--restart-budget', 'worker=1', '--deduplicate'] });
function decode(result) {
  return JSON.parse(result.content[0].text);
}
async function call(name, args = {}) {
  const result = await client.callTool({ name, arguments: args });
  assert(!result.isError, JSON.stringify(result));
  return decode(result);
}
try {
  await client.connect(transport);
  const identity = await call('devd_identity');
  const target = { instance_id: identity.instance_id, run_id: identity.run_id };
  const before = await call('devd_status');
  const args = { target, service: 'worker', operation_key: 'sdk-restart-one' };
  // Treat the first control reply as lost. Keep the same adapter and key.
  await call('devd_restart', args);
  const firstState = await call('devd_status');
  const retried = await call('devd_restart', args);
  assert.equal(retried.operation_status, 'completed');
  const after = await call('devd_status');
  assert.notEqual(before.services.worker.event_generation, firstState.services.worker.event_generation);
  assert.equal(after.services.worker.event_generation, firstState.services.worker.event_generation);
  assert.equal((await call('devd_identity')).control_policy.remaining_restarts.worker, 0);
  for (const [request, expected] of [
    [{ ...args, service: 'missing' }, 'operation-key-conflict'],
    [{ ...args, operation_key: 'sdk-restart-two' }, 'budget-exhausted'],
  ]) {
    const result = await client.callTool({ name: 'devd_restart', arguments: request });
    assert(result.isError);
    assert.equal(decode(result).code, expected);
  }
  console.log('@modelcontextprotocol/client@2.2.0: same-session lost-reply retry, one restart, conflict and exhausted budget passed');
} finally {
  await client.close();
}
