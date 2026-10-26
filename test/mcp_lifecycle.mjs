// Real sidecar + loopback HTTP. No model, campus account, filesystem export, or live app.
// cargo build -p onethu-harness && node test/mcp_lifecycle.mjs target/debug/onethu-harness
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import http from 'node:http';
import net from 'node:net';

const binary = process.argv[2] ?? 'target/debug/onethu-harness';
let checks = 0;
const check = (ok, label) => { assert.ok(ok, label); checks++; console.log(`✓ ${label}`); };
const waitFor = async (fn, label) => {
  const until = Date.now() + 3000;
  while (!fn()) { if (Date.now() > until) throw new Error(`Timeout: ${label}`); await new Promise(r => setTimeout(r, 10)); }
};
async function freePort() {
  const server = net.createServer();
  server.listen(0, '127.0.0.1'); await once(server, 'listening');
  const port = server.address().port;
  await new Promise(r => server.close(r)); return port;
}
function sidecar(port, saved = {}) {
  const child = spawn(binary, [], { env: { ...process.env, OH_MCP_PORT: String(port) }, stdio: ['pipe', 'pipe', 'pipe'] });
  const state = { child, settings: saved, calls: [], held: [], holdSettings: false, holdService: false, settingsError: '', stderr: '' };
  let serial = 0, buffer = '';
  const pending = new Map();
  const reply = (id, result, error) => child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, ...(error ? { error: { code: -32000, message: error } } : { result }) }) + '\n');
  state.reply = reply;
  state.rpc = (method, params = {}) => new Promise((resolve, reject) => {
    const id = ++serial;
    const timer = setTimeout(() => { pending.delete(id); reject(new Error(`Timeout ${method}: ${state.stderr}`)); }, 8000);
    pending.set(id, msg => { clearTimeout(timer); msg.error ? reject(new Error(msg.error.message)) : resolve(msg.result); });
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  });
  child.stderr.on('data', chunk => { state.stderr = (state.stderr + chunk).slice(-8000); });
  child.stdout.on('data', chunk => {
    buffer += chunk;
    while (buffer.includes('\n')) {
      const at = buffer.indexOf('\n'); const line = buffer.slice(0, at); buffer = buffer.slice(at + 1);
      if (!line.trim()) continue;
      const msg = JSON.parse(line);
      if (msg.method === 'onethu.call') {
        const { ns, method, args } = msg.params;
        state.calls.push(msg.params);
        if (ns === 'settings' && method === 'get') {
          if (state.holdSettings) { state.held.push({ id: msg.id, type: 'settings' }); continue; }
          reply(msg.id, state.settings, state.settingsError);
        } else if (ns === 'cli' && method === 'list') {
          reply(msg.id, { services: [{ operation: 'learn.courses', access: 'read' }, { operation: 'library.reserve', access: 'confirm' }] });
        } else if (ns === 'cli' && method === 'call') {
          if (state.holdService) { state.held.push({ id: msg.id, type: 'service' }); continue; }
          if (args[0].operation === 'library.reserve') reply(msg.id, undefined, 'CONFIRMATION_REQUIRED: Application confirmation required');
          else if (args[0].operation === 'learn.courses') reply(msg.id, { semester: 'server-semester', rows: [{ id: 'course-1', name: '数据结构', teacher: '测试教师' }], complete: true });
          else if (args[0].operation === 'libroom.list') reply(msg.id, undefined, 'LIBROOM_INITIALIZATION_REQUIRED: Complete setup in OneTHU');
          else reply(msg.id, { semester: 'server-semester', items: Array.from({length: 250}, (_, i) => ({ id: `file-${i}`, courseId: 'course-1', name: `资料${i}.pdf` })), complete: true });
        } else reply(msg.id, undefined, `Unexpected host call ${ns}.${method}`);
      } else if (pending.has(msg.id)) { const done = pending.get(msg.id); pending.delete(msg.id); done(msg); }
    }
  });
  return state;
}
const request = (port, method, params = {}, { path = '/mcp', headers = {}, raw, id = 1 } = {}) => new Promise((resolve, reject) => {
  const body = raw ?? JSON.stringify({ jsonrpc: '2.0', id, method, params });
  const req = http.request({ hostname: '127.0.0.1', port, path, method: 'POST', headers: { 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(body), ...headers } }, res => {
    let text = ''; res.setEncoding('utf8'); res.on('data', c => { text += c; }); res.on('end', () => resolve({ code: res.statusCode, headers: res.headers, text, json: text ? JSON.parse(text) : null }));
  });
  req.setTimeout(5000, () => req.destroy(new Error('HTTP timeout'))); req.on('error', reject); req.end(body);
});
const enable = { mcpServer: 'true', mcpServerRequested: 'true', ohAuthorized: 'true', provider: 'madmodel', madmodelToken: '' };
let a, b;
try {
  const port = await freePort();
  a = sidecar(port, enable);
  await waitFor(() => a.stderr.includes('等待 activate'), 'sidecar listener');
  check((await request(port, 'initialize')).code === 403, 'default closed before settings handshake');
  const activated = await a.rpc('activate', { enabled: false });
  check(activated.mcpStatus.enabled && activated.mcpStatus.listening, 'activate restores saved authorization without chat');
  check(activated.mcpStatus.cliEndpoint === `http://127.0.0.1:${port}/cli/v1`, 'runtime reports actual CLI endpoint');
  check(a.calls.length === 1 && a.calls[0].ns === 'settings', 'activate calls only settings, no model or credentials');
  check((await request(port, 'initialize')).json.result.serverInfo.name === 'OneTHU CLI', 'real initialize works with MadModel unavailable');
  const list = await request(port, 'tools/list');
  check(list.json.result.tools.some(t => t.name === 'query_learn_courses'), 'existing school tools remain listed');
  check(list.json.result.tools.some(t => t.name === 'onethu_services') && list.json.result.tools.some(t => t.name === 'onethu_call'), 'MCP lists unified service entrypoints');
  const unsafe = ['ext_flow_create', 'ext_flow_run', 'ext_tool_create', 'ext_tool_run', 'run_plugin_cmd', 'skill_run', 'visit_fill', 'memory_refresh'];
  check(unsafe.every(name => !list.json.result.tools.some(t => t.name === name)), 'freeform, plugin, skill, flow and unreviewed legacy tools excluded');
  for (const name of unsafe) {
    const before = a.calls.length;
    const response = await request(port, 'tools/call', { name, arguments: { confirmed: true, confirm: true, submit: true } });
    check(response.json.error.code === -32602 && response.json.error.message.includes('use onethu_call') && a.calls.length === before, `${name} rejected before any host dispatch`);
  }
  const oldCourse = await request(port, 'tools/call', { name: 'query_learn_courses', arguments: {} });
  check(JSON.parse(oldCourse.json.result.content[0].text).rows[0].id === 'course-1', 'legacy course query reaches unified CLI and adapts complete course result');
  check(a.calls.findLast(c => c.ns === 'cli').args[0].operation === 'learn.courses', 'legacy queries share CLI permissions and credential projection');
  const legacyRoom = await request(port, 'tools/call', { name: 'query_librooms', arguments: { date: '2026-10-24' } });
  check(legacyRoom.json.error.message.includes('LIBROOM_INITIALIZATION_REQUIRED'), 'legacy room query cannot silently bind email during initialization');
  const cliList = await request(port, 'service/list', {}, { path: '/cli/v1' });
  check(cliList.json.result.services.some(s => s.operation === 'library.reserve'), 'CLI service registry preserves reservations');
  const payload = { operation: 'learn.files', args: ['course-1', 'server-semester'] };
  const cliFiles = await request(port, 'service/call', payload, { path: '/cli/v1' });
  check(cliFiles.json.result.items.length === 250 && cliFiles.json.result.complete, 'CLI returns full structured result without summary truncation');
  const mcpFiles = await request(port, 'tools/call', { name: 'onethu_call', arguments: payload });
  check(mcpFiles.json.result.structuredContent.items.length === 250, 'MCP and CLI share the complete service result');
  check(a.calls.findLast(c => c.ns === 'cli').args[0].operation === payload.operation, 'service request is forwarded intact to the fixed host facade');
  const denied = await request(port, 'tools/call', { name: 'onethu_call', arguments: { operation: 'library.reserve', args: [], confirm: true } });
  check(denied.json.error.message.includes('CONFIRMATION_REQUIRED'), 'MCP confirm flag cannot bypass application approval');
  check((await request(port, 'settings.set', { mcpServer: true }, { path: '/cli/v1' })).json.error.code === -32601, 'CLI cannot dispatch arbitrary host namespaces');
  check((await request(port, 'tools/list', {}, { headers: { Origin: 'https://evil.example' } })).code === 403, 'browser-origin reads rejected');
  check((await request(port, 'tools/list', {}, { headers: { Host: 'rebind.example' } })).code === 403, 'non-loopback Host rejected');
  check(!list.headers['access-control-allow-origin'], 'private local server sends no wildcard CORS');
  check((await request(port, 'tools/list', {}, { path: '/other' })).code === 405, 'only documented HTTP paths accepted');
  check((await request(port, 'notifications/initialized')).code === 202, 'MCP notifications acknowledged without JSON-RPC body');
  check((await request(port, 'tools/list', {}, { raw: '{' })).json.error.code === -32700, 'malformed JSON returns protocol error');
  a.settings = { ...enable, mcpServer: 'false' };
  check(!(await a.rpc('mcp_sync', { enabled: true, mcpServer: true })).enabled, 'sync ignores client-provided enabled and reads host settings');
  check((await request(port, 'initialize')).code === 403, 'close takes effect immediately without chatting');
  check((await request(port, 'service/list', {}, { path: '/cli/v1' })).code === 403, 'CLI shares the same authorization gate');
  a.settings = { ...enable, ohAuthorized: 'false' };
  const unauthorized = await a.rpc('mcp_sync');
  check(!unauthorized.enabled && unauthorized.requested && unauthorized.errorCode === 'authorization-required', 'requested switch and revoked OH authorization distinguished');
  a.settings = enable; await a.rpc('mcp_sync');
  a.settingsError = 'host unavailable';
  const failed = await a.rpc('mcp_sync');
  check(!failed.enabled && failed.errorCode === 'settings-unavailable', 'failed settings read closes server');
  a.settingsError = ''; a.settings = enable; await a.rpc('mcp_sync');
  a.holdSettings = true;
  const oldSync = a.rpc('mcp_sync');
  await waitFor(() => a.held.some(h => h.type === 'settings'), 'pending control settings');
  const heldControl = a.held.findLast(h => h.type === 'settings');
  a.holdSettings = false; a.settings = { ...enable, mcpServer: 'false' };
  const closeStarted = Date.now();
  check(!(await a.rpc('mcp_sync')).enabled && Date.now() - closeStarted < 1000, 'new close does not queue behind a pending control read');
  a.reply(heldControl.id, enable); await oldSync;
  check((await request(port, 'initialize')).code === 403, 'older control result cannot reopen after concurrent close');
  a.settings = enable; await a.rpc('mcp_sync');
  const settingsHeldBeforeChat = a.held.filter(h => h.type === 'settings').length;
  a.holdSettings = true;
  const staleChat = a.rpc('run', { command: 'chat', input: 'never calls a model' });
  await waitFor(() => a.held.filter(h => h.type === 'settings').length > settingsHeldBeforeChat, 'pending chat settings');
  a.holdSettings = false; a.settings = { ...enable, mcpServer: 'false' }; await a.rpc('mcp_sync');
  const held = a.held.findLast(h => h.type === 'settings'); a.reply(held.id, enable);
  check((await staleChat).ok === false, 'missing model key exits chat without inference');
  check((await request(port, 'initialize')).code === 403, 'old chat settings cannot undo a newer close');
  a.settings = enable; await a.rpc('mcp_sync');
  a.holdService = true;
  const staleService = request(port, 'service/call', payload, { path: '/cli/v1' });
  await waitFor(() => a.held.some(h => h.type === 'service'), 'pending service data');
  a.settings = { ...enable, mcpServer: 'false' }; await a.rpc('mcp_sync');
  a.settings = enable; await a.rpc('mcp_sync');
  const heldService = a.held.find(h => h.type === 'service'); a.reply(heldService.id, { private: 'old account data' });
  const cancelled = await staleService;
  check(cancelled.code === 403 && !cancelled.text.includes('old account data'), 'close/reopen discards in-flight private response');
  b = sidecar(port, enable);
  const collision = (await b.rpc('activate')).mcpStatus;
  check(collision.enabled && !collision.listening && collision.errorCode === 'listener-unavailable', 'another instance reports actual port collision');
  a.settingsError = 'webview stopped';
  await new Promise(resolve => setTimeout(resolve, 11000));
  check((await request(port, 'initialize')).code === 403, 'periodic host validation closes an orphaned legacy MCP gate');
  check(a.calls.every(c => ['settings', 'cli'].includes(c.ns)), 'all successful MCP/CLI reads and syncs used only authorized facade and zero model calls');
  const exited = once(a.child, 'exit'); await a.rpc('dispose'); await exited; a = undefined;
  const bExit = once(b.child, 'exit'); await b.rpc('dispose'); await bExit; b = undefined;
  console.log(`PASS ${checks} OneTHU CLI / MCP runtime assertions; mock host and loopback only`);
} finally {
  a?.child.kill('SIGKILL'); b?.child.kill('SIGKILL');
}
