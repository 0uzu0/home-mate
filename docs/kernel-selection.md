# Agent 内核选型结论（M3.5，2026-11-07）

状态：**选型结论；独立评审已完成（REQUEST_CHANGES：1×P1＋3×P2＋6×P3），全部发现已修复，达成评审给出的通过路径**——按 M3 先例（REQUEST_CHANGES 全修后视为通过）生效为正式技术选择。仍未引入任何新依赖（本文档与修复只动文档、trait 文档注释与 spike 隔离代码）。评审报告全文归档 [selection-review](kernel-research/selection-review.md)。

证据基础：六份独立深研/扫描报告（[rig](kernel-research/rig.md)、[rust-genai](kernel-research/rust-genai.md)、[adk-rust](kernel-research/adk-rust.md)、[全景扫描](kernel-research/landscape-scan.md)、[langchain-rust 补研](kernel-research/langchain-rust.md)、[graph-flow 补研](kernel-research/graph-flow.md)）＋三项隔离 spike 实测（代码与原始输出在 [`spikes/`](../spikes/README.md)，证据文件 `spikes/results/`）＋ §5 最小场景确定性对照。调研智能体与实现者分离；spike 依赖全部隔离在 `spikes/` 独立工程（空 `[workspace]` 表），主仓 `crates/hm-core` 与主工作区零改动、零新依赖。

## 1. 决策表

| 层 | 决定 | 依据摘要 |
|---|---|---|
| **模型客户端（现役）** | **保留自写 419 行客户端**，本次不替换 | 单供应商（bigmodel/GLM）场景下已满足全部需求且零 churn 暴露；对照实验 12/12 解码全绿（见 §3）；唯一实证缺口（无 per-chunk 读超时）在自己代码内修复，成本远低于引入依赖 |
| **模型客户端（替换路径）** | **genai 0.7.3 为已验证的指定替换项**（非"将来再说"） | Spike B 实测：`GenaiLlmClient`（~250 行，`spikes/genai-swap/src/lib.rs`）过真 `LlmClient` trait 合同 live 通过 bigmodel——恰一次 End、工具调用解码（End 方向点名映射断言验证）、纯文本、工具历史回传（真实接受）。**独立评审发现历史方向 wire 名映射 bug（fn_name 须 `to_wire_tool_name`，初版误用 `from_wire`，bigmodel 宽容未暴露）——已修复＋单测 `history_tool_calls_carry_wire_names` 钉死＋live 复跑通过**：两个映射方向的正确性现在都有断言或测试证据。触发条件＝出现第二供应商/wire 格式（音箱内 Ollama 研究是天然触发点）；届时适配器迁入 `crates/hm-core/src/llm/genai.rs`，pin `=0.7.3` 起步 |
| **Rig** | 只借方法，不引入（含模型客户端层） | 深研结论不变；spike A 虽实测其客户端层过 bigmodel 可用（含 idle timeout 2.007s 精确触发、drop 即时关闭），但当前无自写客户端不能满足的需求；月度 breaking＋ARC 代币资金背景进商用评审。方法清单见 §4 |
| **ADK-Rust** | 只借方法，不引入（不变） | 取消语义全场最成熟，但 Runner 共掌会话/运行/在途状态＝红线冲突；作未来持久编排后备记录 |
| **langchain-rust** | **拒绝**（连"只借方法"仅取少量，见 §4） | 补研：停更两年后单人 13 天三 major（v5→v8，2026-10-21~11-03）；`mockito` 硬编进生产依赖；native-tls/OpenSSL 硬编码；reqwest 0.13 双栈；docs.rs 覆盖 30.5%；取消语义虽方向正确但 token 为 executor 级 |
| **graph-flow** | 只借方法＋**观察名单**（四候选中唯一天然不抢任务状态） | 补研：引擎只持 current_task_id+context+version；SQLite 缺口小（自写 ~100–150 行）；但 bus factor 1、0.6.0 主体 AI 生成、曾 6.5 个月沉寂。真实多步可恢复工作流需求出现时以精确 pin＋自写 rusqlite 适配小步组合引入 |
| **循环/任务层** | **自有循环保留并吸收方法清单（§4）** | 依据＝六份报告的代码级接缝分析（任何整框架引入都以状态共掌或 churn 税换取已自有的能力）＋M2/M3 已钉死自有循环语义的回归测试集（喊停/迟到回执，`tests/gateway_voice.rs`/`tests/scenarios.rs`）。注意：§5 对照只测了模型客户端层，**无候选循环参赛**——循环层结论不来自对照实验 |

## 2. Spike 实测证据（全部 2026-11-07，GLM `glm-4-flash` @ `open.bigmodel.cn/api/paas/v4`）

### Spike A：Rig 模型客户端层过 bigmodel（`spikes/rig-bigmodel/`）

- 深研报告两大 unverified 项全部落定：
  1. **bigmodel 兼容性**：`Dialect::gateway`＋`with_base_url` 指向 bigmodel，工具调用流式解码成功（`music_search`＋完整 JSON 参数，1.35s 首事件）。
  2. **drop 取消**：`Streamed` 文档承诺"drop the stream to cancel"——wire-mock 连接日志确定性证明**连接存活 8ms 即关闭**；丢弃后 1.18s 内下次调用正常（无锁/连接泄漏）。
- idle timeout 语义实测：`with_idle_timeout(2s)` 在空转流上 2.007s 精确触发，错误信息含空转时长。
- 实测确认 GLM wire 怪癖：`finish_reason:"tool_calls"` 与完整工具调用**同 chunk**到达（夹具 `spikes/fixtures/glm-tool-normal.sse` 原始留档）。

### Spike B：genai 替换可行性（`spikes/genai-swap/`）

- `GenaiLlmClient` 实现**真 `LlmClient` trait**（path-dep hm-core），live 全绿：工具调用（finish=ToolCalls、`music.search` 点名映射、参数解析）、纯文本（9 增量、Stop）、**工具历史回传**（assistant tool_calls＋tool result 反向映射被 genai 接受）。
- **spike 抓到并修复一个真实适配器 bug**：genai 回传的 `fn_name` 已是 wire 名，回程须 `from_wire_tool_name`（初版误用 `to_wire_tool_name` 双重转换，live 断言当场抓出）——正是替换可行性与适配成本要用实测回答的部分。
- **独立评审再抓一个映射 bug（已修）**：历史方向 genai 把 `fn_name` 原样上 wire，初版误用 `from_wire_tool_name` 导致供应商收到带点的 `music.search`（广告名却是 `music_search`）；bigmodel 宽容故 live 未暴露。修复＝`to_wire_tool_name`＋单测钉死＋live 复跑通过——两个映射方向各被一次实测/评审/测试循环覆盖。
- **发现 genai 自定义端点脚枪**：`util_get_service_url` 用 `Url::join("chat/completions")`，base 无尾斜杠时**末段被替换**（`…/v1` → POST `…/chat/completions`，404）。bigmodel 默认端点自带尾斜杠故 live 正常；未来任意自定义端点必须尾斜杠结尾——已写入替换边界。

### §5 确定性对照（`spikes/compare/`，wire-mock 同夹具三客户端）

| 场景 | own(419) | genai 0.7.3 | rig 0.46.0 |
|---|---|---|---|
| 真 GLM 工具夹具解码 | ✅（16–18ms 量级） | ✅（16–18ms 量级） | ✅（~88ms 量级） |
| 真 GLM 文本夹具（14 增量） | ✅ | ✅ | ✅ |
| 规范分片工具参数（合成怪癖） | ✅ | ✅ | ✅ |
| **缺 index 工具增量**（合成怪癖） | ✅ | ✅ | ✅ |
| 首事件后空转 20s 恢复 | ⚠️ **20.0s**（实验中等 mock 服务端 20s 放弃才结束；生产中最长挂到 reqwest 180s 总超时兜底） | ✅ 2.006s 读超时自愈 | ✅ 2.007s idle 超时自愈 |
| drop 取消→连接关闭 | ✅ ≤8ms | ✅ ≤8ms | ✅ ≤8ms |

- 12/12 解码格全绿：三客户端对真实与合成 GLM wire 怪癖的解码能力**等效**——替换不损失正确性。
- **唯一实质差距**：自有客户端无 per-chunk 读超时（首事件超时 35s 只保护"一个字都没来"的情形；首事件后挂死既不受读超时保护——最终由 reqwest 180s 总超时或服务端放弃兜底，期间该轮任务不可用）。这是本次对照发现的自有代码缺口，修复方案见 §4-7。
- **对照覆盖层次的诚实边界**：本对照覆盖 §5 场景中作用于**模型客户端层**的行（简短聊天流式、查询后执行的工具调用解码、取消与迟到的客户端侧取消传播）。循环层的场景行（执行中纠正、工具失败重试策略、双工/两房/配置恢复）**没有做候选循环的对照实验**——没有任何候选循环被采纳为对照组；循环层结论（§1 决策表末行）依据六份报告的代码级事实推理得出，其现有语义的证据是 M2/M3 已有的回归测试集（`tests/gateway_voice.rs`、`tests/scenarios.rs`），不是本次新增实验。

## 3. 结论依据的完整推理

1. **正确性已等效，差距只在韧性语义**——既然三客户端解码 12/12 等效，引入外部客户端换不来正确性收益；genai 的真实优势（多供应商适配器＋回放测试过的 wire 怪癖集）只在第二 wire 格式出现时兑现，这正是"保留现役＋指定替换路径"的理由。
2. **循环层无可平移候选**：四个深研框架的取消/状态语义各有亮点（Rig canonical-closed-history、ADK OutcomeUnknown、langchain run_until_cancelled、graph-flow session 快照），但都以接管会话/运行/在途状态为前提＝与 TaskRegistry 单一所有权红线冲突；自有循环的喊停纪元＋迟到回执语义（M2/M3 已用回归测试钉死）反而是候选们没有的能力组合。
3. **churn 是税不是功能**：Rig 月度 breaking、langchain 13 天三 major、ADK 三个月两大版本、genai 4–5 个月 breaking minor、graph-flow pre-1.0——对所有候选都要求精确 pin＋迁移预算；唯一 pin 稳定收益为正的场景是 genai 作为"替换路径"（触发时一次性付 1–2 天）。
4. **商用许可**：全部候选本体宽松（MIT/Apache），但 Rig 的 `webpki-root-certs`（CDLA-Permissive-2.0）与 `sqlite-vec`（未查）、ARC 代币资金背景须在正式商用评审记录；引入任一候选前过 [commercial-licensing](commercial-licensing.md) 流程。

## 4. 方法清单（进自有循环 backlog，非依赖）

三深研共同清单（[agent-kernel-research §6](agent-kernel-research.md)）＋本轮新增：

1. （Rig）流分片序列模型：Part 编号＋Start→grow→End 校验＋部分参数可解析；取消时输出规范闭合历史；精确 max_turns；retry-after 解析与 Stalled 空转分类。
2. （ADK-Rust）OutcomeUnknown 在途回答合成（与迟到回执补记同构，可为下一轮模型上下文提供更诚实的取消叙事）；RunBudget 多轴预算＋耗尽持久事件；ErrorCategory 分类；分层状态键；工具效果分类驱动并行/重试。
3. （rust-genai）header/read 双超时语义＝自有客户端缺口的修复蓝图（见 7）；bigmodel 适配器怪癖回放测试集。
4. **（langchain-rust，新）**`run_until_cancelled` 取消模型（在当前 await 点丢弃在途 future，中止不排空）；mpsc 容量 1 背压＝自然停止下游；解析错误回喂模型自纠（ParserErrorHandler）；max_iterations 软着陆。
5. **（graph-flow，新）**session 整快照＋乐观锁 version 并发控制；`Paused{next_task_id}` 显式恢复点表达；per-task timeout 用 future-drop 实现；幂等责任显式声明（Production Considerations 三条）。

## 5. 版本固定与替换边界

| 项 | 固定 | 边界 |
|---|---|---|
| rig-core（若未来引入） | `=0.46.0`（commit `6e039cd`，MIT，MSRV 1.97.1） | 仅模型客户端层（`default-features=false, features=["derive","reqwest","rustls"]`）；不接 rig-agent 循环/harness |
| genai（替换路径） | `=0.7.3`（tag `v0.7.3`=`b449516`，MIT OR Apache-2.0，无声明 MSRV/edition2024） | 适配器单文件进 `llm/genai.rs`，`LlmClient` trait 及上层零改动；模型名一律 `bigmodel::<id>` 命名空间（bare `glm-*` 路由到 z.ai）；**自定义 base_url 必须尾斜杠结尾**（Url::join 陷阱，spike 实证）；**历史方向工具名必须 `to_wire_tool_name`**（genai 把 `fn_name` 原样上 wire，评审 P1 实证）；**错误映射必须用 genai `Error::status()` 填充 `LlmError::Http.status`**（否则 4xx 会被 `is_transient()` 误判可重试，评审 P2 实证）；`End.tool_calls` 命名不变量已写入 trait 文档（llm/mod.rs）；接受 reqwest 0.13 与 0.12 双栈或整体升级 0.13 |
| graph-flow（观察名单） | `=0.8.1`（tag `v0.8.1`=`419d6cc8`，MIT；**禁开 rig feature**） | 只作编排层组合（自写 rusqlite SessionStorage ~100–150 行）；不接管任务有效性 |
| 主仓现状 | 零新依赖 | `crates/hm-core` 依赖面不变（spike 全部隔离在 `spikes/`，不进主工作区） |

## 6. 未解决风险

1. 自有客户端读超时缺口未修（§4-3/§4-7 蓝图已有，属自有代码 backlog，非本选型阻塞）。
2. genai 无声明 MSRV：edition 2024 隐含 ≥1.85；NAS 交叉编译与 Debian 12 部署时需在目标机验证（替换触发时做）。
3. Rig ARC 资金背景与 `webpki-root-certs`/`sqlite-vec` 许可为商用评审遗留项（只在引入时需要结论）。
4. 真实声学（打断/AEC/参考信号）不在本次对照范围；模型客户端层的结论不外推到声学层。

## 7. 自有客户端韧性修复项（**已于 M4 修复**，提交 `55eeeba`）

在 `crates/hm-core/src/llm/deepseek.rs` 增加 per-chunk 读超时（tokio `timeout` 包 body chunk 循环，对齐 genai `stream_read_timeout` 语义：心跳/部分帧重置计时、非总时限），默认值与首事件超时（35s）区分（建议 20s 起步可配）；补一个"首事件后空转"回归测试（wire-mock 式假件：首事件后静默）钉死"20s 内 Failed 而非挂死"。这是对照实验发现的唯一自有缺口，修复在自有代码内完成，不构成新依赖。

**M4 落地（2026-11-07，`55eeeba`）**：`ReadTimeoutStream` 包装 SSE 字节流（每收到 chunk 重置窗口；仅在 Pending 时起表；空转超预算→流内 `Failed("read timeout: no SSE bytes for 20s (stalled mid-stream)")`，进入既有 consume_stream 语义——吐字后不重试、如实上浮），默认 20s，`HM_LLM_READ_TIMEOUT_SECS` 可配（非法值告警回退默认）。两个 paused-clock 回归钉死：首事件后静默恰在 20s Failed；5s 间隔慢流在 20s 窗口下正常完成不误杀。对照表中"own(419) 20s 等服务端放弃"行已闭环。

## 8. 评审记录（2026-11-07，独立评审智能体；全文 [selection-review](kernel-research/selection-review.md)）

**结论 REQUEST_CHANGES → 全部修复后按通过路径生效**。五项核对：①决策-证据匹配（除 P1 一处表述超实测外全部可回溯）；②复现通过（评审者实际运行 wire-mock＋compare，数字与 `spikes/results/` 同量级）；③边界（发现两处接缝遗漏＝P2-2/P2-3）；④拒绝依据充分；⑤红线全部通过（根 workspace 仅 hm-core、四 spike 均隔离、密钥扫描零命中）。

修复对应：**P1**（历史方向 wire 名映射错误表述＋适配器真 bug）→ `to_wire_tool_name`＋单测 `history_tool_calls_carry_wire_names`＋live 复跑＋§1/§2 措辞更正＋§5 边界补充；**P2-2**（`End.tool_calls` 命名不变量未申明）→ 写入 `llm/mod.rs` trait 文档＋§5 边界行；**P2-3**（错误 status 映射遗漏）→ 适配器用 `Error::status()` 填充＋§5 边界行；**P2-4**（循环层归因不实）→ §1 决策表与 §2 诚实边界改为报告推理＋M2/M3 回归依据；**P3×6** → 表格数字改量级引用、stall 补 180s 兜底措辞、13 天更正、agent-kernel-research 完成态措辞延后至提交时成立、死变量已删、wire-mock 残留进程已清理。
