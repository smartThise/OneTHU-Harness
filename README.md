<div align="center">

<img src="banner.png" alt="(One / THU) — One THUer should have OneTHU." width="640"/>

<br/>
<br/>

<img src="logo.svg" alt="( OH ) — OneTHU Harness" width="92"/>

## ( OH · OneTHU Harness )

**大模型驱动的清华校园助手** —— OneTHU 官方 Rust 插件<br/>
清华大学计算机科学与技术系 2025-2026 夏季学期课程「程序设计训练（Rust）」Agent 大作业 项目

`One App · One Identity · One Campus`

**← 由 [OneTHU](https://github.com/smartThise/OneTHU) 承载** 

</div>

---

**把整个清华校园，装进一次对话。**

安装 [OneTHU](https://github.com/smartThise/OneTHU) 后，左下角出现常驻对话入口 **OH**。以自然语言下达指令，OH 实时调用真实校园数据完成任务——把过去「打开五个页面、翻找三层菜单」的操作，压缩成一句话。

示例指令：

- 「查询明天下午可用的研讨间」
- 「查询本学期成绩与绩点」
- 「预订周五下午的图书馆座位」
- 「查询宿舍电费余额」

所有结果均来自 OneTHU 已登录的真实会话：**不生成虚构数据，无法获取时明确告知**。

## 为什么是 OH

- **快速** —— 直连已登录会话，真实数据秒级返回；说「打断」，正在进行的回答毫秒级停下。
- **流畅** —— 回答流式逐字呈现，思考、调用工具、返回结果全程实时可见，没有黑盒等待。
- **便捷** —— 课表、成绩、校园卡、电费、网费、图书馆座位与研讨间、网络学堂，一个入口全部直达。
- **安全** —— 预约、取消等写操作先生成结构化确认摘要，经你确认才执行，不存在误操作；密钥仅保存在本机。
- **透明** —— 每次对话的 token 用量与费用实时可见，预算上限可设，到量自动停止，绝无意外支出。

## 功能范围

| 分类 | 支持的自然语言指令 |
|---|---|
| 学业 | 课表、成绩与绩点、考试安排、跨学期课程文件检索 |
| 校园 | 新闻、校历、空教室、教室查询 |
| 生活 | 饭卡流水与余额、宿舍电费、校园网流量与在线设备 |
| 图书馆 | 座位 / 研讨间查询与预约（两段式确认） |
| 网络学堂 | 课程、作业、通知、文件，讨论区一键直达 |

## 快速上手

三步，即刻开始：

1. 安装 [OneTHU](https://github.com/smartThise/OneTHU)。OH 随官方安装包内置，开箱即用；也可在「设置 → 插件」手动安装本插件。
2. 打开左下角对话面板，在插件设置中填入 **API Key**（默认对接 DeepSeek，任何 OpenAI 兼容服务均可，包括本地模型）。
3. 开始对话。

## 使用边界

- **不涉及资金与凭据**：无充值、改密等操作能力；
- **暂不提供体育馆预约**（相关接口稳定性不足）；
- 所有写操作均需**二次确认**，不存在未经确认的执行；
- 校园数据访问经 OneTHU **统一权限门禁**，与应用内其他功能一致。

---

以下内容面向开发者与课程评审。

## 架构（R1：核心逻辑全在 Rust）

```
OneTHU 宿主（Tauri/React）                     本插件（本仓库，独立进程 sidecar）
┌──────────────────────────┐   stdio JSON-RPC   ┌─────────────────────────────┐
│ 设置→插件 安装 manifest   │ ──activate/run──▶ │ main.rs  RPC 主循环          │
│ 插件对话面板（左下角）     │ ◀─progress/log──  │ agent.rs agent 主控循环      │
│ 权限门禁 + onethu.* 门面  │ ◀─onethu.call───  │ tools.rs 校园工具集          │
│ （喂原子数据，不碰内部）   │ ──result───────▶  │ llm.rs  OpenAI 兼容流式客户端 │
└──────────────────────────┘                    │ session.rs 多会话持久化      │
                                                │ usage.rs token/价格/预算     │
                                                │ config.rs 设置+中文日期解析   │
                                                └─────────────────────────────┘
```

- **LLM 调用、工具编排、上下文管理、token 统计全部在 Rust 进程内**（`core/` 库 crate）；
  stdio sidecar（`bin/`，桌面端拉起的独立进程）与 App 内嵌宿主（OneTHU src-tauri，
  Android 无进程执行权限，核心直接编进 App）共用同一核心；
  webview 侧只有宿主胶水（OneTHU 仓库内的通用对话面板，不含业务逻辑）。
- 插件经宿主 `onethu.call` 访问校园数据：**权限门禁与 JS 插件完全一致**，
  会话自愈（失登自动重建）、45s 超时等都由宿主承担。

## 场景定制（≥2 项专门优化）

1. **中文日期语义层**（`config.rs`）：「明天下午」「下周三」「9月8日」「下午2点半」等
   由本地时钟确定性换算成接口要的 `YYYY-MM-DD` / `HH:MM` / `dateChoice`，
   杜绝 LLM 心算日期出错；图书馆座位自动落到 今天/明天 枚举。
2. **对象索引定位层**（`tools.rs`）：图书馆/研讨间的「对象传递」链
   （list→floors→sections→seats→book）对模型暴露为纯索引
   （libIdx/floorIdx/sectionIdx/seatIdx/roomIdx），工具在 Rust 侧按索引取回
   **真实对象**再调宿主 API——模型永远不接触也伪造不了复杂对象。
3. **写操作两段式确认**：订座/取消/研讨间预约第一次调用只生成**确认摘要**
   （含解析后的馆/楼层/区域/座位与日期），用户回复「确认」才真正执行，
   「取消」即放弃；pending 状态随会话持久化。
4. **结果聚合压缩**：座位按区域汇总余位+空位预览、流水汇总收支、新闻裁剪、
   教室按节次下标聚合——控制回灌给模型的体积，小上下文也够用。

## 课程要求对照

| 要求 | 落点 |
|---|---|
| R1 核心 Rust | 本仓库整体（agent 循环 / LLM / 工具 / 统计） |
| R2 界面 | OneTHU 插件对话面板（常驻左下角，可对话可打断可展示轨迹）+ 管理页命令 |
| R3 模型配置 | Endpoint/Key/模型/思考模式/上下文预算/流式开关/价格/预算，全部设置页可改、当次生效 |
| R4 进度+打断 | SSE 流式逐块渲染（delta/think/tool/usage 事件）；interrupt 读线程毫秒级置标志，流内即断 |
| R5 上下文历史 | 多会话（新建/切换/删除/列表），完整上下文可导出/导入 JSON；轨迹随会话保存，非黑盒 |
| R6 token 统计 | 每次调用取 API usage，按设置价格换算；会话/全局两级累计；预算到量自动停 |

## 从源码构建与安装

```bash
cargo build --release
# 产物：target/release/onethu-harness
```

1. 准备目录：
   ```
   my-harness/
   ├── manifest.json          # 本仓库 manifest.json
   └── onethu-harness         # 上面的 release 二进制
   ```
2. OneTHU 桌面端 → 设置 → 插件 → 「Rust 骨干插件（选 manifest.json）」→ 选 manifest.json
   （二进制须同目录同名）。
3. 展开插件卡片，填 **API Key**（默认 DeepSeek，任何 OpenAI 兼容端点均可，含本地模型）。
4. 左下角出现对话气泡——点开即聊。

## 自测（不依赖网络与真实 OneTHU）

```bash
cargo build
node test/sim_host.mjs target/debug/onethu-harness
```

模拟器内嵌假 OpenAI SSE 服务器 + 宿主门面（session/card/library/settings/storage…），
端到端验证：握手→命令清单→自检→agent 工具循环→流式增量→用量换算→会话持久化→
导出→两段式确认（含对象解析断言）→dispose。

## 须知

- 遵守 OneTHU 使用边界：**不含任何体育馆接口**，不含充值/改密等资金与凭据写操作。
- token 估算仅用于上下文裁剪；计费以 API 返回 usage 为准。
- 平台：桌面端（macOS/Windows/Linux）以独立进程运行；Android 由 OneTHU 官方包
  **内嵌承载**（核心直接编进 App，同一套代码），第三方 Rust 插件移动端暂不支持。
