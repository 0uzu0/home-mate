# langchain-rust deep-research (as of 2026-11-07)

> M3.5 分智能体调研报告·其五，调研智能体与实现者分离。归档于 2026-11-07。来源为公开网络资料（GitHub/crates.io API/docs.rs/源码 tag v8.0.0），未做本地构建。

Scope: web-only research on [Abraxas-365/langchain-rust](https://github.com/Abraxas-365/langchain-rust)（crates.io `langchain-rust`，"LangChain for Rust"），对照 Home Mate B 约束评估（自有最小 Agent 循环＋TaskRegistry 硬停/证据分级、自有 OpenAI 兼容 GLM 客户端 ~419 行、tokio/axum/rusqlite/reqwest 0.12 模块化单体、x86_64 Debian 12 NAS、商用许可审查、框架不得共掌任务状态）。源码级结论全部读自 tag **v8.0.0 = commit `a62e1e9f`**；未修改仓库、未构建、未调用模型。本文同时补全 landscape-scan 第 1 节的初步线索（v8.0.0/2026-11-03/MIT 均经 crates.io API 证实；"v5.0 in Oct 2026" 证实为 2026-10-21）。

## 1. Version pinning

- 最新 release：**8.0.0，crates.io 发布 2026-11-03**，tag `v8.0.0` = `a62e1e9fc82b14f96d2b8cbe81f029351da2b157`。`main` HEAD `5fc3f5ac`（2026-11-03，tag 后仅数个提交，此后 4 天无推送）。截至扫描无更新版本。crates.io 下载 162,140 次 / 近 90 天 11,596 次，共 74 个版本（0.1.0 首发 2024-02-24）。
- **发布节奏是本候选最大的形态事实**：2024-02~2024-10 高频迭代（2024-03 一个月 20 版，大量 yanked）到 4.6.0（2024-10-06）后**停更约两年**；2026-10-21/22 两天连发 5.0.0、5.1.0、5.2.0、6.0.0、7.0.0，11-03 发 8.0.0——**13 天内 3 个 major**。复更窗口的 71 个提交中 58 个来自 Abraxas-365（GitHub commits API，since 2026-09-01，全部落在 10-21~11-03）。
- 各 major 破坏性变更（读自 GitHub Releases 页，仓库无 CHANGELOG.md）：
  - **v5.0.0（10-21）**：两年来的首个 release，整体升级依赖 major（async-openai 0.42、surrealdb 3、sqlx 0.9、qdrant-client 1.19、gix 0.89），`LoaderError::DiscoveryError` 改包 `gix::Error`，fastembed 7 单 ONNX Environment 等连锁；修复 `--all-features` 构建，新增离线 mock 测试套件。
  - **v5.1.0 / v5.2.0（10-22）**：Claude/Ollama 原生工具调用、Bedrock Converse；行为变更（Claude 默认模型改 claude-sonnet-5-5、Ollama 工具结果改 tool role）。
  - **v6.0.0（10-22）**：`BaseMemory` 全面 async 化（方法改返回 `Result<_, MemoryError>`，破坏性）；`GenerateResult.tool_calls` 统一为跨 provider 的 `ToolCall` 类型；修复裸 `[]` 响应导致工具 agent 死循环。
  - **v7.0.0（10-22）**：新增 `AgentExecutor::stream_events` 与取消；所有错误枚举改 `#[non_exhaustive]`（需加通配分支）；`semantic_router::Index` 要求 `Send + Sync`。
  - **v8.0.0（11-03）**：thinking/reasoning、Claude prompt caching、MariaDB/pgvecto.rs；`Document` 改 `#[non_exhaustive]`、多个结构体字面量需 `..Default::default()`、WebScrapper 私有化字段；修复失败流不再把半截对话写入 memory。
- **Pin 结论**：若引入必须 `=8.0.0` 精确固定（tag `v8.0.0`）。任何 minor/major 都可能带破坏（v7 连错误枚举都改了），且该项目的节奏是"长期静默＋短促爆发"，无法预期下个版本何时到来。

## 2. Licenses

- **crate 本体 MIT**（全部 74 个版本均为 MIT，crates.io API）。
- 强制依赖（default features 为空也无法避开）逐一查证（crates.io 各 crate 最新版 license 字段，2026-11-07）：async-openai **MIT**；async-recursion、async-trait、futures/futures-util、glob、regex、reqwest、reqwest-eventsource、secrecy、serde/serde_json、thiserror、url 均 **MIT OR Apache-2.0**；async-stream、mockito、readability、strum_macros、text-splitter、tiktoken-rs、tokio/tokio-stream/tokio-util、tracing、urlencoding 均 **MIT**；csv **Unlicense/MIT**；scraper **ISC**。未发现 copyleft。
- 商用注意点：csv 的 Unlicense（公有领域捐献）与 scraper 的 ISC 属少见许可，需在 Home Mate 许可清单里单列；MIT OR Apache-2.0 双许可若按 Apache-2.0 路径走需保留 NOTICE/归因。readability 0.3（最后更新 2023-12）拖入 html5ever 0.26 / markup5ever_rcdom / lazy_static 等旧依赖、mockito 的传递依赖、以及 sqlx/pgvector 等 optional 路径的许可**本次未逐一复核**（同 rig 报告口径，终裁归商用审查）。仓库无 FUNDING.yml、无 SECURITY.md、未查 CLA/专利条款（unverified）。
- **mockito 1.7 是正式 `[dependencies]` 依赖（非 dev）**——HTTP mock 框架进入每个下游的生产编译图，属打包失误特征（推测为 v5.0 离线测试改造把 `#[cfg(test)]` 模块搬进 src 后需要 mockito 可见于普通依赖；此动机为推断）。MIT 无污染，但商用审查会问、构建时间和供应链面无谓扩大。

## 3. 工具链/MSRV、最小 features、x86_64 Debian 12 可行性

- **无 `rust-version`，edition 2021**（Cargo.toml 与 crates.io metadata 双确认，全部历史版本皆无 MSRV 声明）。有效 MSRV 由强制依赖推高：text-splitter 0.33 声明 **Rust 1.90**（最高项），reqwest 0.13/tokio/tokio-util/tiktoken-rs 声明 1.85，thiserror 2 声明 1.77，async-openai 0.42 声明 1.75——**实际需 rustc ≥1.90，且属未声明状态**。Debian 12 系统 rustc 1.63 完全不可用，必须 rustup 工具链（与 rig/ADK 同等约束）。
- **TLS 被硬编码为 native-tls**：reqwest 0.13 与 async-openai 0.42 都按 `native-tls` feature 引入（Cargo.toml），features 里没有任何 rustls 开关。Debian 12 上需要 OpenSSL 头文件（libssl-dev/pkg-config）；若 Home Mate 现栈是 rustls，则引入双 TLS 栈。
- **sqlx 不是强制依赖**（可选项，仅在 postgres/mariadb/sqlite-vec/sqlite-vss features 后），这点比预想好；但强制集已经够重：`tokio` features=["full"]、async-openai（chat-completion+embedding+audio）、reqwest 0.13（json/stream/charset/http2/system-proxy/native-tls）、scraper+readability（HTML 解析链）、csv、glob、tiktoken-rs（内嵌 BPE 词表）、text-splitter、**mockito**。`default = []` 已是最小组合——**没有更小的裁剪开关**。另注意与 Home Mate 的 reqwest 0.12 共存意味着两套 hyper/TLS 栈（langchain 内部 reqwest 0.13 与 async-openai 0.42 一致，不内耗）。
- CI：ubuntu-latest + stable 工具链，fmt/clippy + `cargo build --all-features`（含测试），x86_64 Linux 无特有障碍；tag 触发的正则只匹配 `v0.*`（v8 tag 实际不触发 tag 校验任务，工程卫生小瑕疵）。**未做实际 `cargo build` 验证（unverified）。**

## 4. 代码级 API 事实（v8.0.0 源码）

**循环组织**（`src/agent/executor.rs`、`agent/agent.rs`）：`AgentExecutor<A: Agent>` 是通用循环：`plan → AgentEvent::Action(Vec<AgentAction>) | Finish`，工具**严格顺序执行**（`for action in actions`，无并行）。三种 agent：`OpenAiToolAgent`（OpenAI 原生 function calling，v5.1 起同构支持 Claude/Ollama/Bedrock）、`ReActAgent`（MRKL 文本协议 `Thought:/Action:/Action Input:/Final Answer:`＋`OBSERVATION_STOP_WORDS`＋`ReActOutputParser`，服务无原生工具调用的模型）、ConversationalAgent。`AgentStreamEvent::{Text, Thinking, ToolStart, ToolEnd, Usage, Finish}` 由 `stream_events` 流出；事件通道 **mpsc 容量 1**——消费慢即拖慢整个 run、至多超前一个事件（源码注释明确以此保证 drop 即停）。

**流式工具参数拼接**（`src/llm/openai/mod.rs` 的 `StreamAccumulator`）：`BTreeMap<u32, ToolCall>` 按 delta 的 `index` 键聚合；首片段带 id/name，后续片段对 `arguments` 做**字符串追加**，收尾整体交给 `GenerateResult.tool_calls`；usage 取末块（`stream_options.include_usage`）；只取 choice index==0；v8 修复了"usage 与 content 同块"（#290）的丢弃问题。**没有缺 `index` 服务器的兜底路由**（对比 genai 的 id-routing slots），也没有部分 JSON 的增量校验（对比 rig 的 `parse_partial_arguments`）；参数 JSON 的真正校验发生在工具侧 `Tool::parse_input`（默认实现宽松：解析失败就把原字符串包成 `Value::String`）。已知利刃：遗留 `LLM::stream()`（`StreamData` 路径）在"既无 usage 又无 content"的块上直接 `Err(ContentNotFound)`——纯工具调用 delta 会踩中；但 agent/链的新路径 `call_stream → generate_stream` 走 StreamAccumulator，不受影响。

**取消语义**（`executor.rs`，本候选最核心的事实）：
- `with_cancellation(CancellationToken)`：整个 run 被 `token.run_until_cancelled(run)` 包裹；**取消即在当前 await 点丢弃内部 future——在途 LLM HTTP 请求或在途工具 future 被丢弃（中止，不排空）**，返回 `Err(ChainError::AgentError("agent run cancelled"))`，靠 `token.is_cancelled()` 区分；**取消的 run 不写 memory**。文档注释原文："the in-flight LLM or tool call is dropped… Dropping the future (or the stream from stream_events) also cancels a run."
- drop `AgentEventStream` 同样立即取消（容量 1 限定了滞后窗口）。这天然满足语音"即时硬停、不得排空"的方向。
- 两个 Home Mate 必须知道的缺口：(a) token 挂在 **executor 实例上，作用于该 executor 的每一次 run**（文档原文 "Applies to every run of this executor"）——按任务取消需要每任务一个 executor 或自行外层 select 包裹；(b) drop 只终止 future 本身——工具内部若 `tokio::spawn` 出去的后台动作会存活，属 Rust 协作式取消的固有边界（同 Home Mate 自有循环的现状）。
- **超时与重试：完全没有**。`CallOptions` 无 timeout 字段（源码核对：仅 max_tokens/temperature/stop_words/functions/thinking/stream_usage 等），executor/LLM 客户端无任何 `tokio::time::timeout`、无重试、无退避。首事件超时/停流检测必须 Home Mate 自带。
- **预算/迭代上限**：仅 `max_iterations`（默认 10；按 steps 计数，一次 plan 产出 3 个 action 记 3 步；parse 重试也计数）。超限**返回 `Ok`**、output 为字符串 `"Max iterations reached"`（软着陆非错误，调用方只能靠输出文本或 `with_return_intermediate_steps` 判别）。解析失败可用 `ParserErrorHandler` 把错误作为 `_Exception` observation 回喂模型重试（MRKL 复原手段）；未知工具名默认硬错，或回喂重试。无 token/费用/时长预算。
- **会话与记忆（谁拥有状态）**：memory 为 `Option<Arc<Mutex<dyn BaseMemory>>>`，**默认 None，完全可选**；`BaseMemory` trait 很小（`messages()/add_message()/clear()`）。run 启动时读一次 memory 注入 `chat_history` 模板变量——**会覆盖调用方传入的同名变量**（`run_inner` 源码），即外部历史必须通过实现 `BaseMemory` 适配器进入；只在 `AgentEvent::Finish` 时写入（human＋带 tool_calls 的 AI 消息＋tool 消息＋最终 AI 消息），取消/失败不写（v8 修复项）。per-run `steps: Vec<(AgentAction, String)>` 是 executor 内存态，**无持久化、无 resume、无 canonical history 校验**——比 rig/ADK 轻得多的状态占用。`SqliteMessageHistory`：**sqlx 0.9** 连接池（非 rusqlite，第二套 SQLite 栈）、每消息一行 JSON、`langchain_message_history` 表自动建（可 `without_migration`）、按 `session_id` 键。

## 5. 接缝分析（对照 Home Mate 约束）

- **红线（框架不得共掌任务有效性/会话状态）——冲突中等偏低，但循环所有权仍在框架**：langchain-rust 没有 TaskRegistry 概念、无持久 run 状态、memory 可选可换——这三点比 rig（`AgentRun` 序列化状态机）和 ADK（Runner 拥有 session/event/在途调用台账）干净。但 `AgentExecutor` 拥有 plan→act 循环本体、steps 与迭代计数；取消要经它的 token/drop 路径才生效。"任务是否还能行动"仍由框架循环在两次模型调用之间隐性持有——Home Mate 的 TaskRegistry 只能通过外层 token 与"每任务一个 executor"来保持唯一裁决权，可行但每任务要构造 executor。
- **消息列表/记忆抽象是否强制接管**：不强制（memory=None 合法），但 `chat_history` 变量覆盖行为意味着不用它的 memory 就拿不到历史注入；接 Home Mate 的 rusqlite 会话需实现 `BaseMemory` 适配器（小，trait 仅 3 方法），并接受"只在 Finish 落库"的写入时机（Home Mate 现设计是逐轮持久化＋晚到结果记录，语义不同，适配器需自行先落库再返回）。
- **好消息：`OpenAiToolAgentBuilder::build<L: LLM + 'static>(self, llm: L)` 接受任意 `LLM` trait 实现**——Home Mate 的自有 GLM 客户端可实现 `LLM`（`generate` + 遗留 `stream` + 覆写 `generate_stream`）直接驱动整个 agent 层，一行 async-openai HTTP 都不必走（尽管它仍会被编译进来）。这是比 rig 干净的替换边界。
- **硬停**：token 取消＝丢弃在途 future（中止不排空）＋memory 不写——方向与 Home Mate 语音 stop-epoch 一致；缺口是 executor 级 token 粒度与零超时（见 §4）。
- **成本面**：reqwest 0.13＋native-tls（双栈＋OpenSSL）、强制 mockito/scraper/readability/tiktoken、tokio-full、MSRV 1.90 未声明、sqlx 第二 SQLite 栈（仅当用它的 SQLite memory）。对 6.7GB RAM NAS 无运行时压力，但构建面和许可审查面显著扩大。

## 6. GLM / bigmodel 接入

- 无 GLM/智谱客户端（有 OpenAI/Claude/Qwen(DashScope)/DeepSeek/Ollama/Bedrock 客户端；Qwen/DeepSeek 为手写 reqwest＋SSE，各带自有错误枚举）。GLM 路径即通用 OpenAI 兼容：`OpenAIConfig::new().with_api_base("https://open.bigmodel.cn/api/paas/v4/").with_api_key(...)` + `with_model("glm-4-flash")`（async-openai `Config` trait 标准 `with_api_base`，v8.0.0 源码 re-export）。
- 怪癖风险：(a) 工具流式聚合按 `index` 键，若 GLM 某版本省略 `index` 无兜底（async-openai 类型上 index 为必填，缺字段整块反序列化失败的风险也未验证）；(b) GLM-4 系"首个 content 块即带完整 tool_call＋finish_reason"的单块形态与 StreamAccumulator 兼容（单 entry 即完成），genai 报告记录的该怪癖在此实现下不构成问题——但**均未实测**；(c) `stream_options.include_usage` 需显式 `CallOptions::with_stream_usage`，GLM 是否接受该字段未验证；(d) 遗留 `LLM::stream()` 路径对纯工具块会 `ContentNotFound`（§4），务必只用 `generate_stream/call_stream` 路径。issue 区未搜到 zhipu/glm/bigmodel 相关讨论（GitHub issue 搜索 total=0；无 OR 的搜索语法不完全可靠，弱证据）。**结论：配置面通畅，行为面必须做一次针对 bigmodel v4＋glm-4-flash 工具流式的 spike（unverified）。**

## 7. 维护风险

- **Bus factor ≈ 1**：Abraxas-365 436/603 commits（72%）；复更爆发期 58/71。第二名人类贡献者 prabirshrestha 50 commits（早期为主），renovate[bot] 63。无组织背书、无 FUNDING、无赞助信息。
- **节奏风险是结构性而非偶发的**：2024 年高频→停更两年→14 天 6 个 release→（截稿时）再度静默。三 major/13 天意味着任何升级都可能破坏（v7 连错误枚举都 non_exhaustive 化）。下游生态极小：crates.io 反向依赖仅 5 个 crate，全部还停在 ^4.x（无人跟进 5~8）。下载量中等（162k 总量/11.6k 每 90 天，远低于 genai 的 581k/282k）。
- 文档：**docs.rs 覆盖率 30.51%**（docs.rs 标注），源码内 doc-comment 质量尚可（executor/tool.rs 注释完整）但整体文档是短板；8 个 open issue 多为 2024-25 年陈旧功能请求（含两条文档投诉），复更期间未见清理。仓库：1,362 stars / 179 forks / 13 watchers；CI（ubuntu stable、fmt/clippy、--all-features 构建＋mock 测试）在爆发期内是认真补过的（v5.0 引入离线 mock 测试套件）。

## 8. 裁决

**建议：拒绝引入为运行时依赖；只借方法。**（与 landscape-scan 的"caveats 将由深研确认或否决"预判一致——本报告确认了风险侧。）

理由：
1. **它给 Home Mate 的净值是负的**。Home Mate 已有的最小循环＋419 行 GLM 客户端已覆盖其 agent 层的全部语义（流式文本/工具参数聚合、顺序工具执行、迭代上限、硬停）；langchain-rust 反而**没有**Home Mate 需要补的任何一块——无超时、无重试、无晚到结果记录、无证据分级、无按任务取消的细粒度 token。引入后这些仍要全部自建，再叠加 reqwest 0.13 双栈、native-tls/OpenSSL、强制 mockito/scraper/readability/tiktoken、未声明 MSRV 1.90、sqlx 第二 SQLite 栈。
2. **维护模型与商用产品不匹配**：停更两年后单人 14 天爆发式重写、三 major/13 天、5 个下游且无人跟进新 major、文档 30%——任何一条单独都可控，合并后是长期维护税与审计负担。
3. **值得借的方法**（模式级，不引类型）：(a) `run_until_cancelled` 包裹整个 run＋取消即丢弃在途 future＋`Err("agent run cancelled")`＋**取消不落 memory**——与 Home Mate stop-epoch 语义逐点对齐的现成参照；(b) **mpsc 容量 1 的事件通道**＝天然的背压＋drop 即停窗口上界；(c) `ParserErrorHandler` 把解析失败/未知工具名作为 `_Exception` observation 回喂模型（MRKL 容错的标准做法）；(d) `max_iterations` 软着陆＋`intermediate_steps` 结构化返回；(e) memory 仅在成功 Finish 时整体写入的取舍（及其与逐轮持久化的利弊对照）。
4. 若未来仍要引（例如想要它的 Claude/Bedrock/Ollama 客户端矩阵或 chains）：走 **`LLM` trait 实现接 Home Mate 自有 GLM 客户端**＋自有 `BaseMemory` 适配器＋每任务一个 executor＋外层自有超时/重试，pin `=8.0.0`，并先完成 §6 的 GLM spike。该路径技术上成立（§5 第 3 点），但届时框架只剩"顺序工具循环"这一个壳，仍不划算。

### Unverified / could not confirm from public materials

- x86_64 Debian 12 上的实际 `cargo build`（含 OpenSSL/native-tls 构建链）；有效 MSRV 1.90 为依赖声明推断。
- GLM（open.bigmodel.cn/api/paas/v4＋glm-4-flash）经其 OpenAI 客户端的工具流式实际行为：index 缺省场景、单块全量 tool_call、`stream_options` 兼容性——需 live spike。
- mockito/readability（html5ever 0.26 等）及 sqlx/pgvector 等 optional 路径的传递依赖许可逐项复核；CLA/专利条款。
- 工具内部 `tokio::spawn` 的后台动作在 drop 后存活的实际影响面（Rust 通用语义，未在其工具实现中逐一核对）。
- GitHub issue 搜索 "zhipu/glm/bigmodel" total=0 的结论受无认证搜索语法限制，属弱证据。
