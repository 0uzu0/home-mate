# graph-flow (rs-graph-llm) deep-research (as of 2026-11-07)

> M3.5 分智能体调研报告·其六，归档于 2026-11-07。来源为公开网络资料（GitHub/crates.io API/docs.rs/源码 tag v0.8.1），未做本地构建。调研智能体与实现者分离。

Scope: web-only research on [a-agmon/rs-graph-llm](https://github.com/a-agmon/rs-graph-llm)（crates.io 名 `graph-flow`，"A type-safe, LangGraph-inspired framework for building complex, interactive, resumable agent workflows"），对照 landscape-scan 第 2 节的初步线索（0.8.1 / 2026-10-21 / MIT）逐项核实并下探到源码级。源码级结论全部读自 tag **v0.8.1（commit `419d6cc8ac1b`）**；`main` HEAD（`9bbca193f2`，2026-10-21）与 tag 的唯一差异是 #22 修复（见 §1/§4）。未做本地构建，未修改 Home Mate 仓库任何文件。评估准绳：Rust 模块化单体（tokio/axum/rusqlite/reqwest 0.12）、自有最小 Agent 循环＋自有 GLM 客户端（bigmodel v4, glm-4-flash）已在 `LlmClient` trait 后运行、x86_64 Debian 12 NAS（6.7GB RAM）、商用许可审查、语音即时硬停、单房家庭伙伴、**框架任务状态共掌是红线**。

## 1. 版本固定

- 最新发布：**0.8.1，2026-10-21**（crates.io API），tag `v0.8.1` = `419d6cc8ac1b6ebec4d947a9bb1140153f18982f`。无 yank（0.8.1）。
- **`main` HEAD（`9bbca193f2`，2026-10-21T16:58Z）领先 tag 一个提交**："Fix max chained execution step boundary (#22)" —— 把 `execute_session` 里 max-step 判定从 `steps >= max` 改为 `steps > max`（修复 off-by-one，并补了两条测试）。**该修复不在任何已发布版本里**（截至 2026-11-07 无 0.8.2），即 0.8.1 的 `with_max_execution_steps(n)` 实际只放行 n−1 次链式跳步。
- 完整版本史（crates.io API）：0.1.1/0.1.2（2025-06-29）→ 0.2.0–0.2.3（2025-06/07）→ 0.3.0（2025-09-03）→ 0.4.0（2025-09-11）→ **0.4.1（2026-03-26，已被 yank）** → 0.5.0（2026-03-26）→ 0.5.1（2026-04-26）→ 0.5.2（2026-07-03）→ 0.6.0（2026-07-19）→ 0.7.0（2026-09-04）→ 0.8.0（2026-09-07）→ 0.8.1（2026-10-21）。git tag 只有 v0.6.0/v0.8.0/v0.8.1 三个；GitHub 无 Release 条目，发布全靠 crates.io。
- **节奏不均**：2025-09-11（0.4.0）到 2026-03-26（0.5.0）之间 6.5 个月零提交（仅 2025-12-03 一次 README 更新）；2026-03 起恢复活跃，7 个月 7 个版本。总提交 87 个。
- 破坏性变更史（`graph-flow/ROADMAP.md`，逐条迁移指南在册）：
  - **0.5.2→0.6.0（2026-07-19）**：一次性落地 2026-07 引擎评审的全部 10 项破坏性变更——移除 `Graph::execute`、`NextAction::GoBack`、`ExecutionStatus::Error`；`Context` 全同步且 `set` 变 fallible；`build()` 返回 `Result`；`Graph` 构建后不可变；`ChatHistory` 改 VecDeque 环形缓冲；**新增 session 乐观锁 `version` 字段（serde default 0，兼容旧行）**；Postgres session id 由 UUID 改 TEXT。注意：`NextAction` 可序列化，持久化内含 `GoBack` 的旧数据升级后反序列化会失败（ROADMAP 明示）。
  - **0.7.0（2026-09-04）**：唯一破坏点在 `rig` feature——rig-core 0.35→0.42（随 rig 0.42 拆出 rig-agent，`rig::` 路径变 `rig_core::`，`Chat::chat` 改 `&mut Vec<Message>`）；不开 rig 不受影响。
  - **0.8.0（2026-09-07）**：sqlx/postgres 变 optional（`postgres` feature，**默认开启**）；破坏面仅限 `default-features = false` 用户。
  - **0.8.1（2026-10-21）**：API 无变化，tokio 只启用 `rt`+`time`（曾依赖传递的 tokio `full` 的下游需自行声明 feature）。

**Pin 结论**：若引入，pin `=0.8.1` 并记录两个已知瑕疵（off-by-one、下述 §4 的 dead-end 复跑语义）；对 max-step 敏感的用法等 0.8.2 或以 patch 依赖 main HEAD（`9bbca193f2`）自行承担。禁止浮动 `0.8`——该项目 pre-1.0 且每次 minor 都可能破坏（0.6.0 十项一次）。

## 2. 许可

- **`graph-flow` 本体：MIT**（Cargo.toml `license = "MIT"`，crates.io 全部 16 个版本均为 MIT；作者 Alon Agmon）。
- 最小配置（`default-features = false`，不开任何 feature）强制依赖 10 个，逐个查证（crates.io API 对应版本 license 字段）：`anyhow` 1.0 MIT OR Apache-2.0、`async-trait` 0.1 MIT OR Apache-2.0、`chrono` 0.4 MIT OR Apache-2.0、`dashmap` 6.1 MIT、`serde` 1.0 MIT OR Apache-2.0、`serde_json` 1.0 MIT OR Apache-2.0、`thiserror` 2.0 MIT OR Apache-2.0、`tokio` 1.40+ MIT、`tracing` 0.1 MIT、`uuid` 1.10+ Apache-2.0 OR MIT。**全部常规宽松许可，无 copyleft，无罕见许可**。
- `postgres` feature（默认开启）：`sqlx` 0.8.6 / `sqlx-core` / `sqlx-postgres` 均 MIT OR Apache-2.0；经 `runtime-tokio-rustls` 拉入 rustls 0.23 系（MIT/ISC 等，未逐个复查——unverified）。已核对 sqlx-core 0.8.6 的 mandatory 依赖表：**不含 libsqlite3-sys**（sqlite 驱动是 optional）。ROADMAP 里"sqlx 拖入 native-SQLite 传递依赖"的说法针对的是其写作时点的树，对 0.8.6 + 仅 postgres feature 不成立（数据源：crates.io dependencies API）。
- `rig` feature：`rig-core` ^0.42（default-features=false，MIT）。已读 rig-core 0.42 的 Cargo.toml（tag v0.42.0）：**`reqwest` 0.13（json/stream/multipart）是其 mandatory 依赖**（0.44 才把 HTTP 传输拆出），default-features=false 只是去掉 `derive`/TLS 等 feature。即：**开 graph-flow 的 `rig` feature 会在 Home Mate 的 reqwest 0.12 之外再拖入一套 reqwest 0.13（且默认无 TLS，要 HTTPS 还得自己给 rig-core 开 `rustls` feature）**，外加 schemars/sha2/eventsource-stream/http 1 等十几个直接依赖。许可面干净（rig MIT、eventsource-stream MIT OR Apache-2.0 等），但体积/双栈代价明确。
- 未做 CLA/专利授权条款分析（仓库无 CONTRIBUTING 审查）——unverified；最终以项目自己的商用许可审查为准。

## 3. 工具链/MSRV、features、x86_64 Debian 12

- **MSRV 未声明**（crates.io 元数据 `rust_version: None`，Cargo.toml 无 `rust-version`）；`edition = "2024"` ⇒ 实际下限 **Rust ≥ 1.85**。Debian 12 系统 rustc 1.63 不可用，NAS 上必须 rustup（与 rig/adk-rust 报告同结论）。
- CI（`ci.yml`，随 0.8.1 #23 新增）：**ubuntu-latest + dtolnay/rust-toolchain@stable**，四种组合 `default / --no-default-features / --no-default-features --features rig / --all-features` 各跑 `cargo check --all-targets` + `cargo test -p graph-flow`。**无 MSRV job、无 Windows/macOS matrix**；测试平台恰好就是 x86_64 Linux（Home Mate 目标平台）。库本体零平台特定代码（纯 Rust 依赖，无 C/asm）——实际 `cargo build` 未验证。
- 最小接入：`graph-flow = { version = "=0.8.1", default-features = false }` ⇒ 依赖树就是上表 10 个 crate，tokio 只开 `rt`（JoinSet）+`time`（timeout），与 Home Mate 现有 tokio 完全同栈；无 HTTP、无 sqlx、无 rig。
- **Session store 现状：仅 InMemory + Postgres，无 SQLite**。`SessionStorage` trait 只有 `save/get/delete` 三个 async 方法＋"version 不匹配返回 `SessionConflict`、成功则 +1"的契约；`Session` 是 `Serialize + Deserialize` 纯数据。照 `storage_postgres.rs`（155 行）改写一个 rusqlite 版（`sessions` 表、`UPDATE ... WHERE version = ?`、rows_affected==0 判冲突）约 **100–150 行**——SQLite 缺口小且是 Home Mate 自己的地盘（rusqlite 已在栈内），但属于自维护代码。

## 4. 代码级 API 事实（读自 v0.8.1 源码）

**状态模型**：图 = `Arc<dyn Task>` 节点表 + 边表（`EdgeCondition = Arc<dyn Fn(&Context) -> bool>`），`build()` 校验边端点/起始任务后产出**不可变** `Graph`。session 状态全部在 `Session{id, graph_id, current_task_id, status_message, context, version}` 里；`Context = Arc<DashMap<String, serde_json::Value>>`（克隆是廉价句柄、跨任务共享同一状态）+ `Arc<RwLock<ChatHistory>>`（VecDeque 环形，默认 1000 条，只存 `{role, String content, timestamp}` 纯文本消息）。注意：`GraphStorage` trait 存在但 `FlowRunner` 不用它（直接持有 `Arc<Graph>`），`graph_id` 仅是记录字段。

**`NextAction` 确切语义**（`graph.rs::execute_session`）：
- `Continue`：沿边/条件边选出下一任务，**写入 `session.current_task_id` 并立即返回** `ExecutionStatus::Paused{next_task_id, reason}` —— "暂停"= 下一任务已选定并持久化，下次 `run()` 执行它。
- `ContinueAndExecute`：同上选边后 `continue` 内层循环，**同一调用内连跑**直到 `Continue`/`WaitForInput`/`End`/出错/触顶。
- `WaitForInput`：**停在当前任务**（`current_task_id` 回写为本任务），返回 `WaitingForInput`；恢复 = 再次 `run()` **重新执行同一任务**，任务作者自己用 context key 区分首入/续跑（insurance 示例 `smart_claim_validator.rs` 用 `approval_state == "pending"` 分支实证此模式）。框架没有独立的 resume-payload/中断点机制——"恢复到哪"的粒度是任务级，不是边级。
- `GoTo(id)`：校验目标存在后设 `current_task_id`，返回 `Paused{next_task_id: target}`（跳转本身也暂停，下次 run 才执行目标）。
- `End`：返回 `Completed`。**Completed 不是粘性的**：session 无终态标记，之后再 `run()` 会把 `current_task_id` 指向的任务再跑一遍——停不停由调用方循环负责（示例均在 caller 侧 break）。
- **dead-end 复跑陷阱**：`Continue` 但找不到出边时，`current_task_id` 回写为**刚执行完的任务**并返回 `Paused{next_task_id: 本任务, reason: "No outgoing edge found"}` ⇒ 之后每次 `run()` 都会重跑该任务。

**暂停/恢复如何持久化**：没有 checkpoint/事件溯源——**整份 session（含全量 Context JSON + 聊天历史）每次 save 一把快照**。`FlowRunner::run` = load → `execute_session` → save；下层手动 API（`graph.execute_session(&mut session)` + 自管 save）与之等价。恢复语义因此极简：重放 = 从 `current_task_id` 再执行。

**每任务超时与 max-step**：`timeout(self.task_timeout, task.run(context))`，默认 300s，超时/出错 → `GraphError::TaskExecutionFailed(String)` 且**整次 execute_session 即刻 Err**（future 被 drop ⇒ 任务内 in-flight 的 LLM/工具调用随之 drop——这就是它唯一的"中止"机制）。`max_execution_steps` 只计**单次 execute_session 内** `ContinueAndExecute` 的跳步数（默认 unlimited），触顶报错；0.8.1 有 `>=` off-by-one（见 §1）。

**乐观锁**：内存版靠 DashMap entry 锁做 check-and-swap；Postgres 版一条 `INSERT ... ON CONFLICT (id) DO UPDATE SET ... version = sessions.version + 1 WHERE sessions.version = $6`，rows_affected==0 ⇒ `GraphError::SessionConflict`。README/文档明示：**输家的任务副作用不回滚，并发可能时任务须幂等**。

**取消/崩溃恢复边界**：**框架没有任何外部取消 API**（无 CancellationToken、无 abort、无 interrupt）。执行中途要停只有三条路：(a) 任务超时（drop future）；(b) 调用方 drop `run()` 的 future（select! 分支丢弃 ⇒ 任务 future 一并 drop ⇒ 内存中 session 变更**不落盘**，持久态停在 run 前）；(c) 崩溃。崩溃/中断后的恢复边界 = **上一次成功 save 的 session 快照**；`ContinueAndExecute` 链中途崩溃 ⇒ 未保存 ⇒ **从链首任务整链重跑**（README "Durability of chained steps" 明示，副作用同样不补偿）。即：任务边界 at-least-once、副作用不补偿，幂等责任在任务侧。

**FanOut**：`JoinSet` 并发跑子任务、全等待、按 `{prefix}.{child_id}.response/status/next_action` 聚合进 context；子任务 `NextAction` 被忽略；任一子失败 ⇒ FanOut 以首个错误失败（其余子任务仍跑完，context 留部分结果）。整个 FanOut 受同一个 per-task timeout 约束。

**错误面**：`GraphError` 8 个变体全部 `String` 载荷（`TaskExecutionFailed/GraphNotFound/InvalidEdge/TaskNotFound/ContextError/StorageError/SessionNotFound/SessionConflict`）+ `Other(anyhow)`。无重试分类、无 Cancelled 类别、无超时专属变体（超时折叠进 `TaskExecutionFailed` 字符串）。

**LLM 桥接（`rig` feature）实际暴露的只有**：`Context::get_rig_messages()` / `get_last_rig_messages(n)`（历史转 `Vec<rig_core::completion::Message>`）+ `From<SerializableMessage> for Message`（user/assistant/system 纯文本三射）。**没有**工具调用消息、多模态内容、agent 循环、provider 客户端（那些在用户自加的 rig-agent 里）。对 Home Mate：它"不抢模型访问"属实——模型访问完全在 Task::run 内部由使用者掌握；但这个桥本身价值近零（Home Mate 的会话记录含工具调用/结构化部分，塞不进 text-only ChatHistory，只能当普通 context value 存）。

## 5. 接缝分析（对照 Home Mate 约束）

- **红线核查——框架不与 TaskRegistry 共掌任务有效性：通过。** 引擎持有的"编排状态"仅三样：`current_task_id`（一个字符串）、typed context map、乐观锁 version。它没有任务有效性概念、没有 in-flight 调用台账、没有工具注册表、不触碰模型访问。"任务还能不能做事"的判断完全留在 Home Mate 的 TaskRegistry——graph-flow 只回答"下一个跑哪个节点"。这与 Rig（AgentRun 状态机决定下一步）和 ADK-Rust（Runner 拥有 session/event/在途调用台账）是本质不同的归属深度。
- **组合形态**：把 graph-flow 当"可恢复编排层"叠在自有循环之上是自然的：Home Mate 的 agent 回合（一次 GLM 调用+工具批+落账）整体包进一个 `Task::run`；TaskRegistry 的取消判断写在 Task 入口（若任务已取消 → `WaitForInput`/错误返回），语音 stop 用调用方 `select!` drop `run()` future——drop ⇒ in-flight HTTP/工具流即断（tokio drop 语义，与 Home Mate 现有 stop-epoch 同类）、持久 session 停在上次快照、恢复=重跑当前任务（由 Registry 的幂等/迟到结果纪律兜住）。**语音硬停的"in-flight 立即中止"在框架层没有专门 API，但 drop-future 路径恰好给出正确语义**——代价是 Home Mate 要自己保证"重跑当前任务"时已产生副作用的任务可识别（这正是现有 evidence/late-results 设计做的事）。
- **接缝代价清单**（若采纳为编排层）：(1) rusqlite 版 `SessionStorage`（~100–150 行，一次性）；(2) Task 包装层把 Home Mate 回合适配成节点（每节点一个薄壳）；(3) 接受 stringly-typed JSON context（Home Mate 的结构化会话记录序列化后存 key，类型安全由自家 serde 类型自行负责）；(4) 避开 dead-end 复跑与 Completed 非粘性两个语义陷阱（编排图设计时显式收边到 End）；(5) 不开 `rig`/`postgres` feature（否则引入 reqwest 0.13 双栈或 sqlx，均与 Home Mate 栈冲突/冗余）。适配代价总体**小**（与 rust-genai 的 LlmClient 实现同量级，1–2 天）。
- **不匹配处**：(a) 框架的暂停/恢复粒度是任务级复跑，不是断点续传——长任务内部进度（如流式 TTS 位置）不在此层持久化，Home Mate 仍需自己的进度记录；(b) 无事件流/token 流抽象（issue #18 "graphflow-stream" 仍在 open，方向是独立伴生 crate）——token 流继续留在 LlmClient 层，可接受；(c) 条件边只有二元 yes/no（issue #17 open），多路分支要堆叠多条 conditional edge 或用 GoTo；(d) pre-1.0 的 session serde 格式可能再变（0.6.0 曾一次性改十项）。
- **替代视角**：Home Mate 当前单房切片的对话流是否真需要"图"？若只有一个 LLM 任务节点＋少量固定后处理，graph-flow 是杀鸡用牛刀，自有循环已够；它开始划算的触发点是多步、可中断、跨重启的家庭例程/编排（如"睡前流程"＝多任务图 + 人工确认点 WaitForInput）。

## 6. 维护风险

- **Bus factor = 1**：87 提交中 a-agmon 74（85%），BigtoC 8，其余 3 人各 1，另有 2 个 Claude（AI）提交——**0.6.0 那次十项破坏性变更的主体实现由 Claude 完成**（commit 7cfcb2fe29 "graph-flow 0.6.0: implement all planned breaking changes from ROADMAP"）。作者 Alon Agmon（41 followers，62 公开仓库，无组织背书）。
- **Churn**：pre-1.0；0.6.0 单版本十项破坏 + session serde 格式变更史；0.7 又动 rig 桥类型；但 0.6 起有清晰的 ROADMAP/迁移指南文化，破坏是有记录的。
- **活跃度**：曾有 6.5 个月（2025-09→2026-03）完全沉寂，恢复后 7 个月 7 版；最后一次提交 2026-10-21（=发版日），此后 2.5 周无动静。community PR（postgres 可选化、tokio 裁剪、step 边界修复）均被合并——对贡献者友好。
- **采用量**：19,523 总下载 / 最近 90 天 10,448（过半下载集中在近三个月，仍在爬坡）；386 stars / 43 forks / 5 watchers。
- **社区/文档**：docs.rs 覆盖 **72.46%**；README 质量高（含 Production Considerations 一节直陈并发/崩溃语义）；ROADMAP.md 迁移指南完备；无独立文档站。社区存在感弱：一次 r/rust 发布帖、一篇作者 Medium 设计文，**未找到第三方生产采用案例**（搜索仅命中作者自述与仓库）。
- 已知开放缺口即两个 open issue：#17 多路条件边、#18 token 流伴生 crate。

## 7. 裁决

**建议：现阶段"只借方法 + 挂观察名单"；暂不引入为运行时编排层；明确排除 `rig`/`postgres` feature 的任何使用。触发条件满足时（出现真实的多步、可跨重启中断/恢复的家庭工作流），以 `=0.8.x` 精确 pin + 自写 rusqlite `SessionStorage` 的方式组合引入——那是一个小而干净的适配，不是迁移。**

理由（均带来源）：
1. **归属边界干净是它最大的优点**：引擎只持有 `current_task_id` + context + version，无任务有效性/工具台账/模型访问共掌（§4 源码事实），是四个已深研框架中唯一天然满足 Home Mate 红线的循环类候选；扫描报告"不抢模型访问"的判断经源码核实成立。
2. **但当前切片用不上**：单房 NAS 里程碑的对话流是"自有循环 + 单 LLM 节点"，graph-flow 的价值点（WaitForInput 人工确认、跨进程恢复、条件路由、步骤预算）此刻没有对应需求；提前引入 = 为一个 pre-1.0、bus factor 1 的依赖白付 churn 税（§1/§6）。
3. **可借的方法**（写入自有循环设计，不引代码）：(a) **session 整体快照 + 乐观锁 version** 的极简可恢复模型——load→execute→save 三段式与 `SessionConflict` 语义可直接抄进 Home Mate 的 rusqlite 会话表；(b) **"暂停 = 已选定下一任务并持久化"**（`Paused{next_task_id}` 携带 reason）的状态表达；(c) per-task `tokio::time::timeout` 包 future-drop 的中止模型；(d) README "Production Considerations" 对**并发冲突副作用不回滚、链式执行中崩恢复到链首、任务幂等责任**三条的直白陈述——与 Home Mate 的 stop-epoch/迟到结果纪律互相印证。
4. **若引入必须接受的成本**：自维护 SQLite 存储适配；task 级复跑语义（非断点续传）；dead-end/Completed 两个语义陷阱；stringly context；0.8.1 的 max-step off-by-one（修复未发版）；reqwest 0.12 栈上绝不能开 `rig` feature（rig-core 0.42 mandatory 依赖 reqwest 0.13，§2）。
5. 许可面干净（MIT + 10 个宽松传递依赖，逐个查证），商用审查应无障碍；MSRV 未声明 + edition 2024 ⇒ Rust ≥1.85 ⇒ NAS 需 rustup（同 rig/adk-rust 结论）。

### Unverified / could not confirm from public materials

- 实际 `cargo build`/`cargo test` 于 x86_64 Debian 12（CI 只覆盖 ubuntu-latest stable，库无平台特定代码，推断可行）。
- rusqlite `SessionStorage` 适配的实际工时（100–150 行为按 storage_postgres.rs 的估算）。
- rustls 0.23 系逐 crate 许可（仅 `postgres` feature 路径，Home Mate 计划不开）；CLA/专利条款。
- 0.8.2 的发布时间与是否包含 #22 修复（main HEAD 已含，未发版）。
- 社区真实生产采用情况（未找到第三方案例，只有作者自述渠道）。
- Claude 参与提交的工程占比之外的代码质量影响（仅从 commit 记录可见两笔）。
