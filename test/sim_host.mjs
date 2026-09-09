//! OneTHU Harness 宿主模拟器（端到端协议自测，不依赖网络/真实 OneTHU）：
//! - spawn 插件二进制，走完整 JSON-RPC：activate → run(chat) → dispose；
//! - 内嵌一个假 OpenAI 服务器（127.0.0.1 随机端口，SSE 流式），脚本化两轮输出：
//!   ①工具调用 query_card_balance → ②最终回答（用工具结果里的余额）；
//! - 模拟宿主门面：session/user/card/settings/storage/library 等最小实现，
//!   校验插件发来的 onethu.call 参数（尤其对象索引解析是否正确）。
//!
//! 用法：node test/sim_host.mjs [path/to/onethu-harness]
//! 退出码 0 = 全部断言通过。

import { spawn } from "node:child_process";
import http from "node:http";
import assert from "node:assert";

const bin = process.argv[2] ?? "target/release/onethu-harness";
let nextId = 1;
const waiters = new Map();
const events = [];
const storage = new Map();
const hostCalls = [];

/* ── 假 OpenAI SSE 服务器 ── */
const server = http.createServer((req, res) => {
  let body = "";
  req.on("data", (d) => (body += d));
  req.on("end", async () => {
    const reqJson = JSON.parse(body);
    const lastToolMsg = [...reqJson.messages].reverse().find((m) => m.role === "tool");
    hostCalls.push({ toolMsgCount: reqJson.messages.filter((m) => m.role === "tool").length });
    res.writeHead(200, { "Content-Type": "text/event-stream" });
    const send = (obj) => res.write(`data: ${JSON.stringify(obj)}\n\n`);
    const lastUserMsg = [...reqJson.messages].reverse().find((m) => m.role === "user");
    const booking = lastUserMsg && lastUserMsg.content.includes("订");
    if (lastUserMsg && lastUserMsg.content.includes("慢速测试")) {
      // R9 回归触发器：模拟网关慢峰——chat 占住主循环 2s
      await sleep(2000);
    }
    if (booking) {
      // 订座场景：要求调用 book_library_seat（纯索引参数；确认执行不再进 LLM）
      send({
        choices: [{
          delta: {
            tool_calls: [{
              index: 0, id: "call_2", type: "function",
              function: { name: "book_library_seat", arguments: '{"libIdx":0,"floorIdx":0,"sectionIdx":0,"seatIdx":0,"date":"明天"}' },
            }],
          },
        }],
      });
      send({ choices: [{ finish_reason: "tool_calls" }], usage: { prompt_tokens: 401, completion_tokens: 31 } });
    } else if (lastUserMsg && lastUserMsg.content.includes("改日程")) {
      // 改日程场景（日程四件套补齐：查/建/删之外的「改」）：①query_agenda 拿 uid → ②edit_schedule（确认型，停）
      if (!lastToolMsg) {
        send({ choices: [{ delta: { reasoning_content: "先查日程拿 uid。" } }] });
        send({
          choices: [{
            delta: {
              tool_calls: [{ index: 0, id: "call_e1", type: "function", function: { name: "query_agenda", arguments: '{"start":"明天","end":"明天"}' } }],
            },
          }],
        });
        send({ choices: [{ finish_reason: "tool_calls" }], usage: { prompt_tokens: 180, completion_tokens: 14 } });
      } else {
        assert.ok(lastToolMsg.content.includes("onethu-edit-test"), "第 2 轮应拿到含 uid 的日程行");
        send({ choices: [{ delta: { reasoning_content: "改时间并清除地点。" } }] });
        send({
          choices: [{
            delta: {
              tool_calls: [{
                index: 0, id: "call_e2", type: "function",
                function: { name: "edit_schedule", arguments: '{"uid":"onethu-edit-test@onethu","start":"16:00","end":"17:30","location":""}' },
              }],
            },
          }],
        });
        send({ choices: [{ finish_reason: "tool_calls" }], usage: { prompt_tokens: 320, completion_tokens: 22 } });
      }
    } else if (!lastToolMsg) {
      // 卡余额场景第 1 轮：要求调用 query_card_balance（带 reasoning_content——v4 思考模型常态）
      send({ choices: [{ delta: { reasoning_content: "先查余额再回答。" } }] });
      send({ choices: [{ delta: { content: "让我先查一下您的校园卡余额。" } }] });
      send({
        choices: [{
          delta: {
            tool_calls: [{ index: 0, id: "call_1", type: "function", function: { name: "query_card_balance", arguments: "" } }],
          },
        }],
      });
      send({ choices: [{ delta: { tool_calls: [{ index: 0, function: { arguments: "{}" } }] } }] });
      send({ choices: [{ finish_reason: "tool_calls" }], usage: { prompt_tokens: 321, completion_tokens: 17 } });
    } else {
      // 第 2 轮：用工具结果作答
      assert.ok(lastToolMsg.content.includes("123.45"), "工具结果应包含模拟余额: " + JSON.stringify(reqJson.messages).slice(-500));
      // v4 协议断言：assistant(tool_calls) 轮回传 reasoning_content；content 恒为字符串（null 会被 400 砖会话）
      const asstMsg = [...reqJson.messages].reverse().find((m) => m.role === "assistant" && m.tool_calls);
      assert.ok(asstMsg, "第 2 轮请求应含 assistant(tool_calls) 消息");
      assert.equal(asstMsg.reasoning_content, "先查余额再回答。", "tool-call 轮应回传 reasoning_content（官方 thinking_mode 规则）");
      assert.equal(typeof asstMsg.content, "string", "assistant content 必须是字符串（防 null 砖会话）");
      for (const m of reqJson.messages) {
        if (m.role === "assistant") assert.equal(typeof m.content, "string", "所有 assistant 消息 content 必须为字符串");
      }
      // 思考链双格式覆盖：reasoning_content 字段 + GLM 风格 <think> 内联（跨 chunk 撕裂："<thi"+"nk>"）
      send({ choices: [{ delta: { reasoning_content: "先核对工具结果…" } }] });
      send({ choices: [{ delta: { content: "<thi" } }] });
      send({ choices: [{ delta: { content: "nk>内部核对：余额 123.45 无误</thi" } }] });
      send({ choices: [{ delta: { content: "nk>您的校园卡余额是 " } }] });
      send({ choices: [{ delta: { content: "123.45 元，祝用餐愉快。" } }] });
      send({ choices: [{ finish_reason: "stop" }], usage: { prompt_tokens: 512, completion_tokens: 23 } });
    }
    res.write("data: [DONE]\n\n");
    res.end();
  });
});

/* ── 模拟宿主门面（onethu.* 最小实现，含权限） ── */
function facade(ns, method, args) {
  hostCalls.push({ ns, method, args });
  switch (`${ns}.${method}`) {
    case "session.status": return "ready";
    case "session.username": return "2025010001";
    case "settings.get": {
      return {
        apiKey: "sk-test", baseUrl: `http://127.0.0.1:${server.address().port}/v1`,
        model: "deepseek-chat", priceIn: "0.27", priceOut: "1.10", budget: "2",
      };
    }
    case "storage.get": return storage.get(args[0]) ?? null;
    case "storage.set": storage.set(args[0], args[1]); return null;
    case "storage.keys": return [...storage.keys()];
    case "user.info": return { name: "测试同学", studentId: "2025010001", department: "计算机系" };
    case "card.info": return { userName: "测试同学", balance: 123.45, cardStatus: "正常" };
    case "library.list": return [{ id: 392, zhName: "北馆(李文正馆)" }];
    case "library.floors": return [{ id: 1, zhName: "3F", zhNameTrace: "北馆/3F", available: 40, total: 200 }];
    case "library.sections": return [{ id: 11, zhName: "A区", zhNameTrace: "北馆/3F/A区" }];
    case "library.seats": return [
      { id: 1001, zhName: "A001", type: "普通座", hasPower: true },
      { id: 1002, zhName: "A002", type: "电源座", hasPower: true },
    ];
    case "library.book": {
      assert.equal(args[1], 11, "book 的 sectionId 应来自解析后的对象");
      assert.equal(args[0].id, 1001, "book 的 seat 应是 seats() 的真实元素");
      return { status: 1, msg: "预约成功" };
    }
    case "library.records": return [{ id: "R1", pos: "北馆 3F A001", time: "明天 08:00", status: "已预约" }];
    case "cal.agenda": {
      // ±400 天窗口内恒返回一条可编辑的云端日程（date 与请求窗口无关，仅用于摘要展示）
      return [{
        uid: "onethu-edit-test@onethu", title: "班会", date: "2027-06-02", start: "14:00", end: "15:30",
        allDay: false, location: "李文正馆", note: "", source: "cloud",
      }];
    }
    case "cal.edit": {
      // 端到端断言：修改集只含要改的字段；显式空串保留（facade 语义：location 空串=清除）
      assert.equal(args[0], "onethu-edit-test@onethu", "edit 的 uid 应来自 agenda 行");
      const ch = args[1] ?? {};
      assert.equal(ch.start, "16:00", "start 应为解析后的 HH:MM");
      assert.equal(ch.end, "17:30", "end 应为解析后的 HH:MM");
      assert.equal(ch.location, "", "显式空 location 应原样传（清除语义）");
      assert.equal(ch.title, undefined, "未传 title 不得出现在修改集");
      assert.equal(ch.date, undefined, "未传 date 不得出现在修改集（保留原日期）");
      assert.equal(ch.allDay, undefined, "未传 allDay 不得出现在修改集");
      return { uid: args[0], where: "cloud" };
    }
    default: throw new Error(`模拟器未实现：${ns}.${method}`);
  }
}

/* ── 被测进程 ── */
const child = spawn(bin, [], { stdio: ["pipe", "pipe", "inherit"] });
let buf = "";
child.stdout.on("data", (d) => {
  buf += d.toString();
  let idx;
  while ((idx = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, idx).trim();
    buf = buf.slice(idx + 1);
    if (!line) continue;
    let msg;
    try { msg = JSON.parse(line); } catch { events.push({ log: line }); continue; }
    if (msg.method === "onethu.call") {
      const { ns, method, args } = msg.params;
      let ok = true, result = null;
      try { result = facade(ns, method, args); } catch (e) { ok = false; result = String(e.message ?? e); }
      child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: msg.id, ...(ok ? { result } : { error: { code: -32000, message: result } }) }) + "\n");
    } else if (msg.id != null && msg.method == null) {
      const w = waiters.get(msg.id);
      if (w) { waiters.delete(msg.id); w(msg); }
    } else {
      events.push(msg); // progress/log 通知
    }
  }
});

function request(method, params, timeoutMs = 15000) {
  const id = nextId++;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => { waiters.delete(id); reject(new Error(`超时等待 ${method} 应答`)); }, timeoutMs);
    waiters.set(id, (msg) => { clearTimeout(timer); resolve(msg); });
    child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
  });
}
const run = (command, input = "") => request("run", { command, input });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function main() {
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  await sleep(100);

  // 1) activate 握手 → 命令清单（含 dock 标记）
  const act = await request("activate", { settings: {}, permissions: [] });
  const cmds = act.result?.commands ?? [];
  assert.ok(cmds.some((c) => c.id === "chat" && c.dock === true), "activate 应返回带 dock 标记的 chat 命令");
  console.log("✓ activate 握手，命令清单：", cmds.map((c) => c.id).join(", "));

  // 2) 自检
  const st = await run("selftest");
  assert.equal(st.result?.ok, true, `selftest 应全过：${JSON.stringify(st.result?.results)}`);
  console.log(`✓ 自检 ${st.result.pass}/${st.result.total}`);

  // 3) 对话：一轮工具调用 + 最终回答（验证 agent 循环、usage、会话持久化）
  const chat = await run("chat", "我卡里还有多少钱");
  assert.equal(chat.result?.ok, true, `chat 应成功：${JSON.stringify(chat.result)}`);
  assert.ok(chat.result.answer.includes("123.45"), "回答应引用工具结果");
  assert.equal(chat.result.usage.prompt, 833, "usage.prompt 应=321+512");
  assert.equal(chat.result.usage.completion, 40, "usage.completion 应=17+23");
  assert.equal(chat.result.totalUsage.calls, 2, "usage.calls 应=2");
  assert.ok(!chat.result.answer.includes("内部核对"), "内联 <think> 思考不得混进回答正文");
  assert.ok(events.some((e) => e.params?.kind === "think" && (e.params.text ?? "").includes("内部核对")), "内联 <think> 应路由为思考链事件");
  console.log("✓ agent 循环：", chat.result.answer);
  console.log("  用量：", JSON.stringify(chat.result.usage), "成本：$", chat.result.usage.costUsd.toFixed(5));

  // 成本换算：833/1M*0.27 + 40/1M*1.10 = 0.00026891
  assert.ok(Math.abs(chat.result.usage.costUsd - 0.00026891) < 1e-9, "成本换算应精确");

  // 4) 会话持久化（R5）：storage 里应已有会话与轨迹
  const saved = storage.get("harness.v1");
  assert.ok(saved?.sessions?.length >= 1, "会话应已持久化");
  assert.ok(saved.sessions[0].messages.some((m) => m.role === "tool"), "工具结果应入库");
  assert.ok(saved.totals.calls === 2, "全局用量应累计");
  console.log("✓ 会话持久化：", saved.sessions.length, "个会话，全局", saved.totals.calls, "次调用");

  // 5) 会话列表 / 导出
  const list = await run("list_sessions");
  assert.equal(list.result?.sessions?.length, 1, "应有 1 个会话");
  const exp = await run("export_session", "");
  assert.ok(exp.result?.json?.includes("我卡里"), "导出应含完整 JSON");
  const expObj = JSON.parse(exp.result.json);
  assert.ok(expObj.messages.length >= 4, "导出的会话应含完整消息链");
  console.log("✓ 会话列表/导出：", list.result.sessions[0].title);

  // 6) 进度通知：应有 delta / tool / usage 事件
  const kinds = new Set(events.filter((e) => e.method === "progress").map((e) => e.params.kind));
  assert.ok(kinds.has("delta"), "应有流式回答增量");
  assert.ok(kinds.has("tool"), "应有工具轨迹");
  assert.ok(kinds.has("usage"), "应有用量事件");
  console.log("✓ 进度通知 kinds：", [...kinds].join(", "));

  // 6.5) 预约两段式确认（场景定制③）：book → pending → 确认 → 真实对象执行
  const bk = await run("chat", "帮我明天在北馆订个座");
  assert.equal(bk.result?.ok, true, `订座首轮应成功：${JSON.stringify(bk.result)}`);
  assert.ok(bk.result.confirm?.summary, "应返回确认卡片摘要");
  assert.ok(bk.result.answer.includes("确认"), "回答应请求用户确认");
  assert.ok(bk.result.confirm.summary.includes("北馆"), "摘要应含解析后的馆名");
  console.log("✓ 预约待确认：", bk.result.confirm.summary);
  const cf = await run("chat", "确认");
  assert.equal(cf.result?.ok, true, `确认执行应成功：${JSON.stringify(cf.result)}`);
  assert.equal(cf.result.executed, true, "确认后应真实执行");
  assert.ok(cf.result.answer.includes("已执行"), "应报告执行成功");
  // facade.library.book 内已断言 seat 是真实元素（id=1001）且 sectionId=11
  console.log("✓ 两段式确认执行：", cf.result.answer.split("\n")[0]);

  // 6.6) 改日程（日程四件套补齐）：agenda 拿 uid → edit 确认卡片（旧值→新值）→ 确认执行
  {
    await request("run", { command: "new_session" });
    const ed = await run("chat", "帮我把明天的班会改日程到下午四点到五点半，别带地点了");
    assert.equal(ed.result?.ok, true, `改日程首轮应成功：${JSON.stringify(ed.result)}`);
    assert.ok(ed.result.confirm?.summary, "edit_schedule 应返回确认卡片摘要");
    const sum = ed.result.confirm.summary;
    assert.ok(sum.includes("班会"), "摘要应含日程标题");
    assert.ok(sum.includes("14:00-15:30"), "摘要应含原时间段（旧值）: " + sum);
    assert.ok(sum.includes("16:00-17:30"), "摘要应含新时间段: " + sum);
    assert.ok(sum.includes("李文正馆") && sum.includes("（无）"), "摘要应展示地点 旧值→清除: " + sum);
    assert.ok(/uid onethu-edit/.test(sum), "摘要应含 uid 片段: " + sum);
    console.log("✓ 改日程待确认：", sum);
    const ecf = await run("chat", "确认");
    assert.equal(ecf.result?.ok, true, `改日程确认执行应成功：${JSON.stringify(ecf.result)}`);
    assert.equal(ecf.result.executed, true, "确认后应真实执行 cal.edit");
    assert.ok(ecf.result.answer.includes("已执行"), "应报告执行成功");
    assert.ok(ecf.result.answer.includes("云日历"), "结果应报告存储位置（云日历）");
    console.log("✓ 改日程确认执行：", ecf.result.answer.split("\n")[0]);
  }

  // R9 关键回归：慢 chat 占用期间，控制命令必须照常秒回（主循环永不阻塞）
  {
    await request("run", { command: "new_session" }); // 干净会话：轮次判断不受确认流历史干扰
    const chatP = request("run", { command: "chat", input: "慢速测试" });
    await sleep(300); // chat 已被 sidecar 接走（工作线程在等 LLM）
    const t0 = Date.now();
    const ns = await request("run", { command: "new_session" }); // chat 进行中重开会话（epoch 守卫应拒绝 chat 的陈旧写回）
    const dt = Date.now() - t0;
    assert.ok(dt < 1500, `chat 进行中 new_session 应秒回，实际 ${dt}ms（主循环被 chat 占死？）`);
    assert.ok(ns.result !== undefined, "new_session 应有应答");
    const ls = await request("run", { command: "list_sessions" });
    assert.ok(Array.isArray(ls.result?.sessions), "chat 进行中 list_sessions 应正常");
    const chatR = await chatP;
    assert.ok(chatR.result?.ok !== false, "慢速 chat 最终应完成");
    // epoch 守卫：chat 的陈旧拷贝不得复活已被重置的会话——当前活跃会话应保持干净
    const ex = await request("run", { command: "export_session", input: "" });
    const sess = ex.result?.ok ? JSON.parse(ex.result.json) : { messages: [] };
    assert.ok((sess.messages ?? []).length === 0, `重置后的会话不得被慢 chat 复活（实际 ${JSON.stringify(sess.messages ?? []).slice(0, 120)}）`);
    console.log(`  ✓ 慢 chat 进行中控制命令秒回（${dt}ms），陈旧写回被拒`);
  }

  // 7) dispose
  const disp = await request("dispose", {});
  assert.ok("result" in disp, "dispose 应应答");
  console.log("✓ dispose 应答");
  await sleep(150);

  console.log("\n全部断言通过（" + hostCalls.length + " 次 onethu.call 记录）");
  child.kill();
  server.close();
  process.exit(0);
}

main().catch((e) => {
  console.error("\n✗ 自测失败：", e.message);
  child.kill();
  server.close();
  process.exit(1);
});
