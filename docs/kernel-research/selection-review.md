# M3.5 内核选型结论独立评审报告

- 评审对象：`docs/kernel-selection.md`（M3.5 选型结论，2026-11-07）
- 评审智能体：独立评审（未参与调研与实现）
- 评审日期：2026-11-07
- 评审范围：kernel-selection.md §8 五项；证据链＝六份调研报告（docs/kernel-research/）、spike 代码与实测证据（spikes/）、§5 场景定义（docs/agent-kernel-research.md §5）、现有合同（crates/hm-core/src/llm/）、红线（AGENTS.md、docs/HANDOFF.md §0.0）
- 评审结论：**REQUEST_CHANGES**（1×P1，3×P2，6×P3）。整体决策方向（保留自写客户端＋自有循环；genai 为指定替换路径；langchain-rust 拒绝；Rig/ADK/graph-flow 只借方法）**有充分证据支撑、无红线违反**；但 genai 行的一处证据表述超出实测所能证明的范围（且 spike 适配器在该方向存在真实映射 bug），必须修正文档并补入替换边界后方可生效。

---

## 一、五项评审请求逐项结论

### ① 决策与证据是否匹配 —— 大体匹配，一处过度声明（P1，详见发现 1）

- **"保留自写 419 行客户端"**：成立。deepseek.rs 实测 419 行；对照实验 12/12 解码全绿（我方复现，见②）；"唯一实证缺口＝无 per-chunk 读超时"与代码事实一致（deepseek.rs 仅有连接 10s＋总 180s 客户端超时，首事件 35s 超时在循环层 agent/mod.rs:53）。
- **"genai 为已验证替换路径"**：主体成立、一处声明不实。恰一次 End（live.rs:59 `assert_eq!(ends, 1)`）、End 方向点名映射（live.rs:71）、工具历史被接受（live.rs:104-131）均有 live 断言；但"工具名**双向**映射"中的"历史→wire"方向（lib.rs:107）用反了函数，发给供应商的是内部点名（见发现 1），live 通过靠的是 bigmodel 的宽容＋断言只查"收到 End"。该 bug 不推翻"替换可行"结论（一行修复），但 §1 的"全合同 live 通过"表述必须修正，且 §5 边界未记录该接缝。
- **"拒绝 langchain-rust"**：有充分报告依据。§1 行内每一项事实（停更两年后爆发、mockito 进生产依赖、native-tls 硬编码、reqwest 0.13 双栈、docs.rs 30.51%、token 为 executor 级）均可回溯到 langchain-rust.md §1/§2/§3/§4/§7，且与该报告 §8"拒绝引入为运行时依赖"一致。仅"14 天"与报告"13 天"差一天（P3-3）。
- **"graph-flow 只挂观察名单"**：与 graph-flow.md §7 裁决逐点一致（只借方法＋观察名单、触发条件、=0.8.1 精确 pin、自写 rusqlite 存储 ~100–150 行、禁开 rig feature）。"四候选中唯一天然不抢任务状态"引自该报告 §5 红线核查，有源码级依据。
- **Rig/ADK 行**：与 rig.md §8、adk-rust.md §8 裁决一致；spike A 把 rig.md 的两个 unverified 项（bigmodel 兼容、drop 关闭）落定为实测（live-rig-bigmodel.txt、replay-rig.txt）。
- **循环层行**："无候选可平移替代"的实质依据是六份报告的接缝分析＋M2/M3 已钉死的自有回归测试，而非 §5 对照实验本身（对照只测了客户端层，见发现 4）。

### ② §2 数据是否可从 spikes/ 复现 —— 已实际复现，成立

按 spikes/README.md 路径执行（wire-mock 监听 127.0.0.1:8977；compare 同夹具三客户端）。我的复现输出与 `spikes/results/compare-run.txt` 同量级且结论一致：

| 项 | committed 结果 | 我方复现（2026-11-07） |
|---|---|---|
| 工具夹具解码 own/genai/rig | 16.7/18.0/88.3 ms 全绿 | 19.8/18.4/91.6 ms 全绿 |
| 文本夹具 14 增量 ×3 | ✓ | ✓ |
| 规范分片/缺 index 怪癖 ×3×2 | 全绿 args_ok=true | 全绿 args_ok=true |
| 首事件后空转 | own 20.005s End（等服务端放弃）；genai 2.007s Failed；rig 2.010s Failed | own 20.005s End；genai 2.008s Failed；rig 2.011s Failed |
| drop 关闭延迟 | 6/7/5 ms（连接寿命） | 7/6/6 ms |

GLM live 部分（live-rig-bigmodel.txt、live-genai-swap.txt）未重跑（按评审请求允许）；其数字（1.354s 首事件、8ms 连接关闭、9 增量、deltas=9、music.search 映射、round-trip 接受）与源码断言逻辑一致、内部自洽（时间戳 1794052xxx/1794053xxx ≈ 2026-11-07，与"全部 2026-11-07"标注相符）。

代码逻辑静态核对亦成立：wire-mock 按 `model` 字段选夹具并以 CloseGuard 记录断连时刻（spikes/wire-mock/src/main.rs:45-58, 103-122）；compare 对 own/genai 走同一条 LlmStream 消费路径（spikes/compare/src/main.rs:63-186）。注意 §2 表中 own/genai 工具解码毫秒数与结果文件对调、drop 行归因错位（P3-1，不影响结论）。

### ③ 替换边界（§5）是否遗漏破坏合同的接缝 —— 发现两处遗漏（P2，发现 2、3）

- **恰一次 End**：两实现均成立。deepseek：`end_emitted` 保证恰一个终止事件（deepseek.rs:165-198, 221-224）；genai 适配器：`done` 标志＋"stream ended without End"→Failed（genai-swap/lib.rs:190-245）。✓
- **停止纪元/任务归属**：边界干净。voice.rs 不引用 LlmClient（全文无该符号）；取消经 TaskRegistry/`request_cancel`/stop_epoch（voice.rs:293-331），工具侧经 `task.cancel_flag()` 的 ToolCtx（agent/mod.rs:327）；trait 调用点仅 agent/mod.rs:211。✓
- **wire 工具名双向映射**：发现两处接缝问题——
  1. **End.tool_calls 命名约定未在合同中固定**：deepseek 返回 wire 名（deepseek.rs:322 原样存储，无反向映射），genai 适配器返回内部点名（lib.rs:230 `from_wire_tool_name`）。今天能工作纯因循环层的 `from_wire_tool_name` 对点名幂等（agent/mod.rs:311）且 deepseek 的 `to_wire` 对 wire 名幂等（deepseek.rs:81）——这一"双向巧合"是不变量，§5 未记录（发现 2）。
  2. **genai 适配器历史方向映射错误**（发现 1，P1）：lib.rs:107 `fn_name: from_wire_tool_name(&c.name)` 把内部点名发给供应商——genai 对 `fn_name` 原样上 wire（genai-0.7.3 `adapter/adapters/openai/adapter_shared.rs:439` `"name": tool_call.fn_name`）。无论历史采用哪种约定（wire 名或点名），`from_wire` 都产出非 wire 名；正确函数应为 `to_wire`（或按固定约定恒等）。live 场景 3 通过是因为 bigmodel 宽容且断言只查"End 到达"。
  3. **错误分类接缝**（发现 3，P2）：适配器把所有 `exec_chat_stream` 错误映射为 `LlmError::Http { status: None }`（lib.rs:170），而 `Http{status:None}` 被判 `is_transient()=true`（mod.rs:124-128）——替换后 401/403/400 会被循环无谓重试两次（现 deepseek 路径填 status、快速失败）。genai 明确提供 `status()`/`headers()`（rust-genai.md §4），适配器未用。§5 未把"错误状态映射"列为适配器职责。
- **尾斜杠陷阱**：核实为真。genai 0.7.3 `util_get_service_url` 用 `Url::join`（adapter_shared.rs:53-77），无尾斜杠 base 会丢末段；bigmodel 默认端点 `https://open.bigmodel.cn/api/paas/v4/`（all_adapters.rs:94）确带尾斜杠。wire-mock 专门加了 `/chat/completions` 别名路由兼容此陷阱（wire-mock/src/main.rs:82-84）。✓

### ④ 拒绝依据是否充分 —— 充分

- langchain-rust：拒绝理由（净值 为负、维护模型与商用不匹配、可借方法清单）与 langchain-rust.md §8 完整对应，该报告 §2-§7 逐项给出 crates.io/源码级来源；"停更两年→单人 14 天爆发→三 major"与报告 §1/§7 相符（仅天数 13 vs 14，P3-3）。
- graph-flow："观察名单而非引入"与其 §7 裁决一致，理由（当前切片用不上、bus factor 1、0.6.0 主体 AI 生成、6.5 个月沉寂、引入成本小而明确）全部有出处。
- 未发现把"报告未验证项"当作"已验证事实"引用的情况；各报告的 Unverified 清单与 §6 未解决风险基本对应（§6 亦如实保留了 rig ARC/sqlite-vec、genai MSRV 等遗留项）。

### ⑤ 红线符合性 —— 全部通过

- **主仓零新依赖**：`git diff HEAD -- Cargo.toml Cargo.lock crates/` 为空；根 workspace members 仅 `crates/hm-core`（cargo metadata 确认）；工作区改动仅 docs/agent-kernel-research.md（1 行状态更新，属"决策变化更新规划文档"的允许动作）＋新增 docs 与 spikes/ 未跟踪文件。✓
- **spike 全部隔离**：wire-mock/compare/genai-swap/rig-bigmodel 四个 Cargo.toml 均第 1 行空 `[workspace]` 表；根 `cargo test` 不可见。✓（compare 与 genai-swap 以 path-dep 引用 hm-core，属只读使用，不修改主仓。）
- **凭证不泄漏**：对 spikes/ 全部文件做了密钥模式扫描（sk-…、api_key=…、Bearer …、32+16 hex 等）零命中；四个 fixtures 为纯 SSE 响应体（响应体中不含凭证）；results/*.txt 无 key；所有 live/capture 二进制均 `std::env::var` 读 key 且不打印（rig-bigmodel/live.rs:34-38、genai-swap/live.rs 头注、wire-mock/capture.rs:12-15）。✓
- 附注（非红线）：①实现者的 wire-mock 进程评审时仍在 127.0.0.1:8977 运行（pid 20253），属环境残留，建议关闭；②已提交的 HANDOFF §0.0 宣告 M3.5 完成并引用尚未提交（untracked）的 kernel-selection.md 与 spikes/——按 AGENTS.md 应在本评审通过后随结论一并提交。

---

## 二、发现清单

### P1（必须修）

**发现 1｜§1 genai 行"工具名双向映射"超出实测所能证明的范围，且 spike 适配器在该方向存在真实映射 bug**

原文（docs/kernel-selection.md §1 表，"模型客户端（替换路径）"行）：

> "Spike B 实测：`GenaiLlmClient`（~250 行，`spikes/genai-swap/src/lib.rs`）过真 `LlmClient` trait 全合同（恰一次 End、工具名双向映射、工具历史回传）live 通过 bigmodel。"

为什么错：
- "工具名双向映射"包含两个方向。**End 方向**（genai→内部）已断言且正确（spikes/genai-swap/src/live.rs:71 `assert_eq!(call.name, "music.search", …)`）。**历史方向**（内部→wire）未验证名称正确性：live 场景 3 只断言"收到 End"（live.rs:104-131 的 `ok`），未断言 wire 上的函数名。
- 该方向的实际代码是错的：`spikes/genai-swap/src/lib.rs:107` `fn_name: from_wire_tool_name(&c.name)`。genai 0.7.3 对 `fn_name` **原样**序列化进 `function.name` 上 wire（`~/.cargo/registry/src/…/genai-0.7.3/src/adapter/adapters/openai/adapter_shared.rs:436-443`）。生产循环推入历史的是 End 事件原样 tool_calls（`crates/hm-core/src/agent/mod.rs:301`），在 genai 适配器下为点名 "music.search"——`from_wire` 对其幂等，于是供应商收到 assistant tool_calls `name="music.search"`，而工具以 `"music_search"` 广告（lib.rs:126 `to_wire_tool_name`）。无论历史约定取 wire 名还是点名，`from_wire` 在此都产出非 wire 名（正确函数为 `to_wire`，或按固定约定恒等映射）。
- live 能过仅因 bigmodel 对名字不严格＋断言缺失。换更严格的第二供应商（恰恰是替换触发条件，如 Ollama 某些版本）可能直接 400。这正是 §5 替换边界应记录而未记录的接缝。

必须修（三选一组合，建议全做）：
1. 修正 §1/§2 措辞：把"工具名双向映射 live 通过"改为"End 方向已断言；历史方向已被 bigmodel 接受但 wire 名映射有 bug（from_wire 误用），修复＋补断言后视为验证完成"；
2. 在 §5 genai 行补入：适配器须把历史 assistant tool_calls 的 `fn_name` 映射为 wire 名（`to_wire_tool_name`），并固定 End.tool_calls 的命名约定（见发现 2）；
3. （可选但推荐）现在就修 spikes/genai-swap/src/lib.rs:107 并在 live.rs 场景 3 增加 wire 名断言后重跑 live，使"已验证"名副其实。

### P2（应修）

**发现 2｜LlmClient 合同未固定 End.tool_calls 的命名约定，两实现现况不一致，§5"上层零改动"依赖一个未申明的不变量**

`crates/hm-core/src/llm/mod.rs:66-75` 的注释只规定请求方向（"dots become underscores on the wire and are mapped back on receipt"），但 receipt 方向实际由谁映射未定：deepseek.rs:322 收到 wire 名**原样**放入 End 事件（无 from_wire），由循环在 agent/mod.rs:311 反解；genai 适配器却在 lib.rs:230 于 End 事件内反解为点名。两种约定 today 都能工作，仅因 `from_wire` 对点名幂等、`to_wire`（deepseek.rs:81）对 wire 名幂等。应把约定写进 mod.rs 的 trait 文档（建议：End.tool_calls 恒为 wire 名，与 deepseek 现况一致，反解统一在循环），并同步到 kernel-selection §5。

**发现 3｜§5 替换边界遗漏错误分类接缝：替换后非瞬时错误会被重试**

spikes/genai-swap/src/lib.rs:170 把所有 `exec_chat_stream` 错误映射为 `LlmError::Http { status: None }`，而 `Http{status:None}` 被 `is_transient()` 判为 true（crates/hm-core/src/llm/mod.rs:124-128）。替换后 401/403/400 会走 agent/mod.rs:205-241 的重试路径（1s/3s 退避两次）才失败；现役 deepseek 填充 status 故快速失败。genai 提供 `status()`/`headers()`（rust-genai.md §4"Retry policy stays with the caller — this only reports what the provider said"）。§5 应把"错误状态映射（用 genai `status()` 填充 LlmError::Http.status）"列为适配器职责。

**发现 4｜§1 循环层行的依据归因不实：§5 对照未测任何候选循环**

原文（§1 表"循环/任务层"行依据摘要）：

> "§5 场景对照中自有循环语义（含 M2/M3 已钉死的喊停/迟到回执）无候选可平移替代"

§5 确定性对照（spikes/compare/）只驱动了三个**模型客户端**（解码/空转/drop），没有任何候选**循环**参赛；"无候选可平移替代"的真实依据是六份报告的接缝分析（rig.md §5、adk-rust.md §6、langchain-rust.md §5、graph-flow.md §5 的状态归属结论）＋M2/M3 已钉死的自有回归测试。应改为归因到报告与已钉测试，避免读者误以为对照实验覆盖了循环层。

### P3（建议）

**发现 5｜§2 表格数字与 results/ 文件存在转置/错位（同量级，不影响结论）**

- 工具夹具解码：compare-run.txt 为 own 16.7ms / genai 18.0ms，文档表写 own ✅18ms / genai ✅16ms（对调）。
- drop 关闭：committed 连接日志寿命按请求顺序（own/genai/rig）为 6/7/5ms，文档写 7/5/6ms；我方复现为 7/6/6ms。结论（毫秒级即时关闭）不变，建议直接引用 results 文件数字。

**发现 6｜"首事件后挂死要等服务端/代理放弃"忽略了既有 180s 总超时兜底**

deepseek.rs:26 `.timeout(Duration::from_secs(180))`（reqwest 总超时，按其文档覆盖到响应体读完）——真挂死最迟 ~180s 会以 Failed 收场，并非只能等对端放弃。缺口本身（无 per-chunk 读超时、语音场景 180s 不可接受）与 §7 修复方案成立，仅措辞建议修正。

**发现 7｜langchain 行"14 天三 major"与报告"13 天"差一天**

kernel-selection.md §1 vs langchain-rust.md §1/§7（"13 天内 3 个 major"）。日子口径（含/不含端点）差异，实质不变，建议统一为 13 天。

**发现 8｜"已全部完成"表述超前：独立评审尚未通过、产物未提交**

docs/agent-kernel-research.md §6 工作树改动写"M3.5 剩余步骤——已全部完成"，但原步骤含"产出选型结论**经独立评审**"；kernel-selection.md 自身状态也是"待独立评审后生效"。且 kernel-selection.md、两份补研、spikes/ 均未跟踪，而已提交的 HANDOFF §0.0 已宣告 M3.5 完成并引用它们。建议：本评审关闭后修正表述并一次性提交。

**发现 9｜compare 中存在死变量（不影响测量）**

spikes/compare/src/main.rs:50-57 创建的 `genai_client` 从未使用（循环内各自重建 target）。建议删除以免误导复现者。

**发现 10｜环境残留：wire-mock 进程仍在运行**

评审时 127.0.0.1:8977 有实现者遗留的 wire-mock（pid 20253）。无仓库影响，建议关闭。

---

## 三、结论

- **REQUEST_CHANGES**。修复发现 1（表述＋边界＋适配器一行修复/断言）与发现 2、3（边界补两条接缝）、发现 4（归因改写）后，本选型结论即可生效：每一"决定"行的证据链（六报告＋三项 spike＋我方独立复现）完整、无越权、无凭证风险、主仓零污染。
- 决策方向本身未被任何发现动摇：12/12 解码等效、韧性差距定位准确（自有客户端读超时缺口为唯一实质差距）、四候选的去留均与其报告结论一致、红线全部守住。
