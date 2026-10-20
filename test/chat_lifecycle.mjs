// Real sidecar JSON-RPC + local HTTP only. No school account or external endpoint.
// cargo build -p onethu-harness && node test/chat_lifecycle.mjs target/debug/onethu-harness
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import http from "node:http";
import { once } from "node:events";

const bin = process.argv[2] ?? "target/debug/onethu-harness";
const storage = new Map();
const requests = [];
const events = [];
const pending = new Map();
let child;
let serial = 0;
let checks = 0;
let holdSettings = false;
let heldSettings;
let stderr = "";
const check = (value, message) => { assert.ok(value, message); checks++; console.log(`✓ ${message}`); };
const deltaFor = (runId) => events.filter((e) => e.runId === runId && e.kind === "delta").map((e) => e.text).join("");
const waitFor = async (predicate, label, timeout = 3000) => {
  const end = Date.now() + timeout;
  while (!predicate()) {
    if (Date.now() > end) throw new Error(`Timed out: ${label}`);
    await new Promise((r) => setTimeout(r, 10));
  }
};
const server = http.createServer(async (req, res) => {
  assert.equal(req.method, "POST");
  assert.equal(req.url, "/v1/chat/completions");
  let body = "";
  for await (const chunk of req) body += chunk;
  const input = JSON.parse(body).messages.findLast((m) => m.role === "user").content;
  const item = { input, res, closed: false };
  requests.push(item);
  res.on("close", () => { item.closed = true; });
  item.finish = () => {
    if (!res.headersSent) res.writeHead(200, { "Content-Type": "text/event-stream" });
    res.end(`data: ${JSON.stringify({ choices: [{ delta: { content: `ok:${input}` }, finish_reason: "stop" }], usage: { prompt_tokens: 1, completion_tokens: 1 } })}\n\ndata: [DONE]\n\n`);
  };
  if (input === "cancel-stream") {
    res.writeHead(200, { "Content-Type": "text/event-stream" });
    res.write('data: {"choices":[{"delta":{"content":"partial"}}]}\n\n');
  } else if (!["slow-busy", "cancel-headers"].includes(input)) item.finish();
});

function reply(id, result) {
  child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, result })}\n`);
}
function settings() {
  return {
    provider: "custom", apiKey: "local-test-only",
    baseUrl: `http://127.0.0.1:${server.address().port}/v1`,
    model: "local-test", stream: "true", mcpServer: "false", maxContext: "64000",
  };
}
function facade(msg) {
  const { ns, method, args } = msg.params;
  switch (`${ns}.${method}`) {
    case "settings.get":
      if (holdSettings) { heldSettings = msg.id; return; }
      reply(msg.id, settings()); return;
    case "storage.get": reply(msg.id, storage.get(args[0]) ?? null); return;
    case "storage.set": storage.set(args[0], args[1]); reply(msg.id, null); return;
    default: throw new Error(`Unexpected host call: ${ns}.${method}`);
  }
}
function rpc(method, params = {}, timeout = 5000, cancelInSameBatch = false) {
  const id = ++serial;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => { pending.delete(id); reject(new Error(`RPC timed out: ${method}`)); }, timeout);
    pending.set(id, (message) => {
      clearTimeout(timer);
      if (message.error) reject(new Error(message.error.message));
      else resolve(message.result);
    });
    child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n${cancelInSameBatch ? `${JSON.stringify({ jsonrpc: "2.0", method: "interrupt", params: { runId: params.runId } })}\n` : ""}`);
  });
}
const chat = (input, timeout) => rpc("run", { command: "chat", input, runId: `test-${input}` }, timeout);
// JSON whitespace must be accepted, unlike the previous raw substring fast path.
const cancel = (runId) => child.stdin.write(`{ "jsonrpc": "2.0", "method": "interrupt", "params": ${JSON.stringify(runId ? { runId } : {})} }\n`);

try {
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const env = { ...process.env, OH_MCP_PORT: "0" };
  for (const key of Object.keys(env)) if (/^(https?|all|no)_proxy$/i.test(key)) delete env[key];
  child = spawn(bin, [], { env, stdio: ["pipe", "pipe", "pipe"] });
  child.stderr.on("data", (chunk) => { stderr = (stderr + chunk).slice(-12000); });
  let buffer = "";
  child.stdout.on("data", (chunk) => {
    buffer += chunk;
    for (;;) {
      const at = buffer.indexOf("\n");
      if (at < 0) break;
      const line = buffer.slice(0, at);
      buffer = buffer.slice(at + 1);
      if (!line.trim()) continue;
      const message = JSON.parse(line);
      if (message.method === "onethu.call") facade(message);
      else if (message.method === "progress") events.push(message.params);
      else if (pending.has(message.id)) {
        const callback = pending.get(message.id);
        pending.delete(message.id);
        callback(message);
      }
    }
  });
  check((await rpc("activate")).commands.some((c) => c.id === "chat"), "sidecar handshake");

  const slow = chat("slow-busy", 25000);
  await waitFor(() => requests.some((r) => r.input === "slow-busy"), "first HTTP request");
  const slowRequest = requests.find((r) => r.input === "slow-busy");
  check((await chat("busy-before-15s")).ok === false, "busy request rejected before 15 seconds");
  cancel("test-busy-before-15s");
  const beforeMutation = JSON.stringify([...storage]);
  check((await rpc("run", { command: "new_session", runId: "rejected-session-write" })).ok === false, "session mutation is rejected while chat owns the store");
  check(JSON.stringify([...storage]) === beforeMutation, "rejected session mutation leaves the persisted store unchanged");
  // Real elapsed time specifically covers the removed takeover threshold.
  await new Promise((resolve) => setTimeout(resolve, 15500));
  check((await chat("busy-after-15s")).ok === false, "slow healthy request still exclusive after 15 seconds");
  check(!slowRequest.closed, "rejected run's targeted interrupt leaves the original HTTP request alive");
  check(requests.length === 1, "rejected chats did not issue any HTTP request");
  slowRequest.finish();
  check((await slow).ok === true, "original request finishes after busy rejections");
  check((await chat("after-busy")).answer === "ok:after-busy", "busy rejection did not leave the gate stuck");
  check(deltaFor("test-slow-busy") === "ok:slow-busy", "first run's actual deltas carry only its own answer");
  check(deltaFor("test-after-busy") === "ok:after-busy", "next run's actual deltas carry only its own answer");
  check(!events.some((e) => ["test-busy-before-15s", "rejected-session-write"].includes(e.runId)), "rejected runs generate no replacement progress stream");

  holdSettings = true;
  const early = chat("cancel-before-http");
  await waitFor(() => heldSettings !== undefined, "blocked settings response");
  cancel();
  check((await chat("busy-while-cancelling")).ok === false, "cancel does not release a still-running worker");
  holdSettings = false;
  reply(heldSettings, settings());
  heldSettings = undefined;
  check((await early).interrupted === true, "early cancellation survives worker setup");
  check(!requests.some((r) => r.input === "cancel-before-http"), "early cancellation sends no inference request");
  check((await chat("after-early-cancel")).answer === "ok:after-early-cancel", "new run uses a fresh cancellation token");
  const batched = await rpc("run", { command: "chat", input: "cancel-at-admission", runId: "admission-batch" }, 5000, true);
  check(batched.interrupted === true, "run and interrupt in one stdin batch retain cancellation");
  check(!requests.some((r) => r.input === "cancel-at-admission"), "admission-time cancellation reaches no HTTP endpoint");

  for (const input of ["cancel-headers", "cancel-stream"]) {
    const running = chat(input);
    await waitFor(() => requests.some((r) => r.input === input), `${input} reached HTTP`);
    const request = requests.find((r) => r.input === input);
    const at = Date.now();
    cancel(`test-${input}`);
    check((await running).interrupted === true, `${input}: real run reports cancellation`);
    check(Date.now() - at < 2000, `${input}: cancellation is prompt`);
    await waitFor(() => request.closed, `${input} socket closed`, 2000);
    check(request.closed, `${input}: actual HTTP connection closed`);
    check((await chat(`after-${input}`)).answer === `ok:after-${input}`, `${input}: next request succeeds after shutdown`);
  }
  check(requests.length === 7, "only admitted, uncancelled-before-send turns reached local HTTP");
  check(events.length > 10 && events.every((e) => typeof e.runId === "string"), "every actual dispatch progress event remains attributed");
  const exited = once(child, "exit");
  await rpc("dispose");
  const [code] = await exited;
  check(code === 0, "sidecar exits cleanly");
  child = undefined;
  console.log(`PASS ${checks} lifecycle assertions; local mock only`);
} catch (error) {
  console.error(error);
  console.error("Local mock progress:", JSON.stringify(events));
  if (stderr) console.error(stderr);
  process.exitCode = 1;
} finally {
  child?.kill("SIGKILL");
  for (const item of requests) item.res.destroy();
  server.closeAllConnections();
  await new Promise((resolve) => server.close(resolve));
}
