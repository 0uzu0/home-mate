# Gander（腾讯混元 Multimodal Duplex Interaction Agent）架构评估

日期：2026-11-10。性质：**调研报告，不含实现、不改任何现有文件**。
评估对象：Gander（arXiv 2609.08977；`Omni-Interaction-Gander/Omni-Interaction-Agent`；`Gander-Omni/Gander`）。
评估基准：本仓 `main@0ff7529` 的代码事实（引用行号为该提交）；设备 dot2-d（482 MB / 4×A53 / 固件 0.0.12）。

> **拓扑更正（评估前提）**：用户口述为「局域网 Mac 上的 core」；实际部署（C 阶段起）是**核心在音箱上自持**
> （`/data/hm`，aarch64 musl），Mac 只在开发期承担 ASR（oMLX Qwen3-ASR-0.6B）与工具链。
> 本报告按「**端侧小设备自持 + 云端 LLM/TTS/ASR**」评估，这比「Mac 当 core」更保守，结论不受影响。

---

## 0. 结论摘要（先看这张表）

| Gander 设计 | 判定 | 一句话理由 |
|---|---|---|
| **① 控制 token 先行**（1 s 单元：先判 listen/speak/interrupt/tool，再生成内容） | **形态可借，实现不可借** | 「先决策后内容」的两段式正是我们缺的；但它是 9B 端到端模型的一个输出位，我们只能用**规则/小模型做 200–300 ms 判定**，且**不能照搬 1 s 节拍**（打断要求 ≤300 ms） |
| **② 小脑-大脑职责划分**（小脑即时反应，复杂任务 `task_start` 异步交大脑） | **原则直接借，能力边界要重画** | 我们已有「小脑雏形」（唤醒应答/退下：预合成 WAV、本地播、不走云）；缺的是**「处理中应答」**与**任务/对话解耦**。但小脑不能是模型，只能是确定性反射层 |
| **③ 任务词汇 + execution generation fence** | **语义借 2/3，工程只借 1 个概念** | `task_send(main)`（注入改口）与 `task_resolve`（取消/授权）值得借；`fork` 在**半双工播放**下收益有限（见 §4.3）。工程上只借 **turn generation（轮次代际）** 一个概念；五实体/worker registry/跨进程恢复明确不借 |

**如果只做一件事**：做 **②的「处理中应答」**（§5）。它把用户干等的 1.7–2.4 s 变成 300 ms 内有回应，
不依赖 T4/T5、不动协议、不改设备固件，是投入产出比最高的一步。

---

## 1. 调研来源与可靠度（先说清楚哪些是读到的、哪些是推断）

| 来源 | 状态 | 用途 |
|---|---|---|
| `arxiv.org/abs/2609.08977` | ✅ 读到摘要 | 确认：基于 MiniCPM-o 4.5、Cerebellum–Brain 拆分、streaming Thinker–Talker、异步 agent loop、通过 tool calling + orchestration runtime 通信 |
| `arxiv.org/html/2609.08977v1` | ✅ 读到系统设计章节 | 1 秒单元、控制 token、任务三操作、execution generation/fencing、Gateway 校验、worker provider 能力声明 |
| `github.com/.../Omni-Interaction-Agent` README（raw） | ✅ 读到 | 四控制值（含 `tool`）、chunk 格式、四任务操作、部署（3×GPU/CUDA 12.4） |
| 项目页 `omni-interaction-gander.github.io/...` | ❌ TLS 连接被重置 | 未取到 |
| HF `Gander-Omni/Gander` | ⬜ 未取（不影响架构评估） | 模型细节不在本次范围 |
| 源码实现（`gander_runtime`） | ❌ 未读 | **凡涉及 generation/fence 的具体实现，本文只按文档描述评估，标注为「文档描述」** |

两处来源冲突已记录：arXiv HTML 把工具调用写成「特殊 token 包裹的独立区段」（控制值只有 listen/speak/interrupt 三个），
README 明确写 **四个控制值含 `tool`**。本报告按 README（发布版口径）记录，并保留该差异。

---

## 2. Gander 的架构事实（只记架构与协议，不碰训练细节）

### 2.1 三块结构

1. **Streaming Cerebellum**（`minicpm_ft`）：MiniCPM-o 4.5 微调，Thinker/Talker 拆分——
   Thinker 负责感知与交互控制，Talker 独立出声、不阻塞感知。输入输出被「拍平」成按 chunk 排序的 token 流。
2. **Agent Orchestration Runtime**（`gander_runtime`）：可信轮次、任务状态、多模态上下文、worker 事件、权限、投递。
   Gateway 负责结构化校验（任务状态、provider 能力、权限边界、部署配置）、工作区隔离与并发控制；
   跟踪五类实体：Project / Task / Run / WorkerEvent / Delivery。
3. **Pluggable Back Brain**：统一 Worker Provider 接口，registry 方式接入；默认 Codex app-server（评测用 GPT-5.6 驱动）。
   provider 需声明能力：native steering、只读旁问、用户交互、跨进程恢复、输入模态、上下文获取、并行执行。

### 2.2 控制 token 先行（①的来源）

- 交互被切成**固定 1 秒窗口**，每窗一个 chunk；chunk 结构：
  `[video] [audio] [optional task context] → [control] [text or tool content]`
- **control 在内容之前预测**，四个值：`listen`（静默观察）/ `speak`（本窗出声）/ `interrupt`（打断当前话语）/
  `tool`（发出结构化任务操作）。内容 token 数可以为 0。
- 语音侧：每单元「8 个 text token 对 50 个 S3 token」，因果 flow-matching 增量合成。
- 上下文：保留最近 **128 个单元（约 2 分钟）**，先淘汰最旧；音频约 10 token/s，视频最长边 448。

### 2.3 任务词汇与 fence（③的来源）

| 操作 | 语义（文档） |
|---|---|
| `task_start` | 从「当前可信轮次」创建一个后台任务；任务标识用描述或名字 |
| `task_send(main)` | 把后续输入**折进活动执行上下文**（steering：改需求、加约束） |
| `task_send(fork)` | 只读旁问，**不改主任务状态** |
| `task_resolve` | 生命周期控制：`cancel` / `allow_once` / `allow_session` / `deny` |

- **execution generation / fencing**：工具调用被绑定到「**transport 层确认的最终用户轮次**」——
  目的之一是**防止模型自己生成的参数去设定目标**；Gateway 再做结构化校验与隔离。
- 运行时两种模式：`lean` / `coordinator`。

### 2.4 部署（我们不做的部分）

Linux + Conda + CUDA 12.4（PyTorch 栈），**三块物理 GPU**：GPU0 Thinker、GPU1 Talker、GPU2 ASR；
权重＝MiniCPM-o 4.5 + 匹配的 Gander Thinker/Talker + faster-whisper large-v3；另需可 `app-server --stdio` 的 Codex CLI。
模型规模 9B 级。→ 与 dot2-d（482 MB、无 NPU、单核 A53 基线）差 3 个数量级，**端侧不可能**。

---

## 3. 我们的现状基线（代码事实，供对照）

| 能力 | 位置 | 现状 |
|---|---|---|
| 唤醒应答（预合成 WAV，本地播，5 词轮换） | `voice.rs:134`（`WakeAckSettings`）、`:679`（`play_local_ack`）、`:711`（`synthesize_wake_acks`） | 已有 manifest + TTS 身份失效重建；**与「处理中应答」同构**，可直接扩 |
| 退下词白名单 / 退下应答 | `voice.rs:111`（默认 9 词）、`:1265`（`retreat_session`） | 确定性执行：停播＋关窗＋本地应答 |
| 双工协商（capability/requested/negotiated/effective 四层） | `voice.rs` duplex 状态、`ModeSet`/`ModeReport` | 半双工默认；全双工要设备能力 |
| barge-in（唤醒/话语即停） | `voice.rs:1303`（`barge_in_if_half_duplex`） | 停全部播放 + `request_cancel` + `stop_epoch += 1` |
| 停止纪元 | `voice.rs:289`、`:1322` | 只作用于「合成/播放」：旧答案合成完也不播 |
| 任务注册表 | `task.rs:63`（`TaskRegistry`）、`:93`（`request_cancel`）、`:103`（`finish`） | create/get/request_cancel/finish/list；cancel=AtomicBool；迟到结果只记录 |
| 工具执行 | `agent/mod.rs:24`（`MAX_TOOLS_PER_CALL=4`）、`:367`（串行 `for … await`） | **串行**，每轮最多 4 个；`ToolCtx` 带 cancel flag |
| TTS 流式 + 背压 | `voice.rs` `speak_stream_segment`、`TTS_AHEAD_LIMIT_S=30` | 边合成边播；首块 374–441 ms（近期实测） |
| 主动说话开关 | `VoiceSettings.proactive_speech`（`voice.rs:65`） | **存了但从未实现**（M2 起就是占位） |
| 会话窗 | `PARTICIPATION_TIMEOUT_MS = 30_000`（`voice.rs:42`） | 30 s 无活动关窗 + 提示音 |

**关键时序事实**（HANDOFF §0.12）：首声 p50 ≈2.4 s；关思考后命令轮 1.7–2.4 s；流式 TTS 短答案 wake→首声 ≈1.2 s。
用户说的「一思考就干等 2.4 秒」就是**同一个模型既做「要不要说」又做「说什么」**的直接后果。

---

## 4. 问题 1：三个设计逐条评估

### 4.1 控制 token 先行 → **形态可借，实现不可借；节拍必须改**

**可以直接借的（1 条）**：**「要不要开口」与「说什么」是两次决策，且第一次先于第二次**。
我们其实已经有这个形态的雏形——唤醒时先播预合成 ACK（决定开口，内容是固定的），再决定是否跑 LLM（决定内容）。
缺的是把它**显式化成一个仲裁器**：谁在什么时候有权占用 voice 通道。

**需要改的（3 条）**：
1. **判定源**：Gander 的 control 是 9B 模型在 token 流里学出来的；我们的判定器只能是
   **规则 + 轻量特征**（VAD 能量/时长、KWS 命中、播放状态、任务状态、是否处于会话窗）。
   这不是妥协，是分工：**规则层只做反射（200–300 ms、离线），语义判断留给 T5/T1**。
2. **节拍**：Gander 1 s 一个单元；我们的打断验收是 **P50 ≤300 ms 停播**（T4），1 s 粒度直接不达标。
   我们已有 80 ms 块节拍 + 事件驱动（VAD 开关、KWS 命中、任务事件），应当**事件驱动**而不是定时器驱动。
3. **四个值的映射**：
   | Gander | 我们的对应物 | 现状 |
   |---|---|---|
   | listen | 会话窗内静默等待（mic 开、不播） | 有（参与窗） |
   | speak | 本地 ACK / 答案 | 有（但「处理中应答」缺） |
   | interrupt | 唤醒/关键词打断（T4）、动作键 | 只有唤醒/按键；播放中说话被门控丢弃 |
   | tool | 任务派发（`run_device_turn`） | 有（同步阻塞） |

**不适用的（2 条）**：1 秒单元与 128 单元上下文（模型 KV 管理，我们没有端到端模型）；
音视频 token 流（无摄像头/多模态需求）。**不要在设备上引入任何形式的小模型推理来「模拟」control token**——
482 MB 内存 + 4×A53 连 KWS 3M 都要单独占一核（T4 已立项），再塞一个判定模型会挤掉音频链。

**风险（必须写进设计）**：Gander 的 control 是**学出来的**，边界样本（backchannel「嗯」vs 真打断）由数据决定；
我们的规则判定在边界上会退化。所以规则**只允许命中固定短语/能量阈值**，不允许做「这句话是不是在纠正我」这种语义判断。

### 4.2 小脑-大脑职责划分 → **原则直接借，能力边界要重画**

**可以直接借的（1 条）**：**「秒回」不归大脑管**。Gander 的小脑只做即时反应与接话判断，复杂任务异步交大脑；
我们现在是**一个 DeepSeek 同时干两件事**，所以一思考就干等。

**需要改的（2 条）**：
1. **小脑能力边界**：Gander 小脑是 9B 多模态模型，能「应一声 + 判断该不该接话」；我们的「小脑」只能是
   **确定性反射层**：预合成短语（已有 5+1 条 ACK 链）、关键词命中（KWS）、固定动作（停播/退下/关窗）。
   **它能解决的只是「不让用户干等」，不能解决「该不该接话」**——后者是 T1/T5 的活。
2. **异步任务的载体**：Gander 的 back brain 是独立进程（Codex CLI，registry 可换）；我们**自己就是大脑**，
   不需要第二个 agent，只需要**把「先说一句」与「后台算」在调度上解耦**（同进程 tokio 任务即可）。

**不适用的（3 条）**：9B 端侧小脑、Codex/worker provider registry、跨进程恢复。
理由：我们是单设备、单进程、单大脑；引入第二套 worker 抽象只会增加状态归属的歧义（本项目最贵的教训就是状态归属）。

### 4.3 任务词汇 + generation fence → **语义借 2/3，工程只借 1 个概念**

**可以直接借的（2 条）**：
- `task_send(main)` 的**语义**：用户改口应当**注入正在跑的任务**，而不是「取消 + 重开」。
  我们现在「纠正换歌」是靠**下一轮 LLM 读历史**（跨轮），不是任务内 steer——所以改口一次就要重来一次。
- `task_resolve` 的**语义**：取消/授权分离。我们目前只有 cancel；`allow_once`/`allow_session` 现在用不上
  （HA 只读、音乐是模拟），但**将来接 HA 写操作（开灯/关窗帘）一定会需要**，值得在类型上预留。

**需要改的（1 条，也是最重要的一条）**：**execution generation**。
Gander 的 fence 绑「transport 层确认的最终用户轮次」；我们的对应物是 `stop_epoch`（只覆盖播放/合成）
+ `current_task` 守卫（只覆盖任务身份）。**缺一个贯穿「工具结果」的代际**：用户改口后，旧任务迟到返回的
工具回执必须被 fence 掉（只记录、不进答案）。这是我们现在**真正会麻爪**的地方。

**不适用的（4 条）**：`fork` 的完整收益（见下）、五实体（Project/Task/Run/WorkerEvent/Delivery）、
worker provider 能力声明、`lean`/`coordinator` 双模式。
- `fork` 的诚实评估：Gander 的旁问靠**全双工音频**才成立（主任务继续说话，旁问同时回答）。
  我们是**半双工、单喇叭**：任何口头旁问都会打断主任务播放。所以 fork 在我们这里**先只对文本/控制台路径有意义**，
  语音路径要等 T5/全双工。**不建议现在做。**

---

## 5. 问题 2：把「唤醒应答」扩成「处理中应答」——可行，改动不大，但有一个真坑

### 5.1 可复用度：高（同一条链）

现有机制已经覆盖了「预合成 → 缓存 manifest → 本地读盘 → voice 通道直投」全链：

- `WakeAckSettings`（`voice.rs:134`）：短语表 + 开关；
- `synthesize_wake_acks`（`voice.rs:711`）：保存时云端合成、按 TTS 身份失效重建、manifest 落盘；
- `play_local_ack`（`voice.rs:679`）：本地读 WAV → `hub.play(channel="voice")`，**不经过云**；
- `pick_ack_phrase`（`voice.rs:657`）：随机轮换。

「我看看」这类处理中应答**就是第二组短语**，目录/ manifest / 合成链全部复用。

### 5.2 触发点（关键设计）

推荐：**首字超时触发**，而不是「用户一说完就播」。

- 位置：`run_device_turn`（`voice.rs:1571`）里，拿到 `AgentEvent::Task` 之后启动一个
  「距用户语音结束 T ms 仍未出首个 Token/Answer」的定时器；
- 若 T 内首字到达 → 取消 ACK（避免「我看看」+ 秒答叠在一起）；
- 若超时 → 播一句处理中 ACK（`kind="processing_ack"`，不进会话历史、不算答案）；
- 建议 T ≈ **400–600 ms**（短于云端首字 1.7–2.4 s，长于短答案 1.2 s 的常见路径，避免抢拍）。
- 也可在**工具段**（`AgentEvent::ToolStart`）追加一次「稍等，我在办」——但同一轮只播一次，避免啰嗦。

### 5.3 真坑：半双工门控

`feed_chunk`（`voice.rs:995`）在**任何 voice 通道播放存活时整段丢 mic**，并在播放后保留
`tail_blind_ms`（当前 200 ms）盲窗。播一句 ACK 会带来：
1. ACK 自身 200–400 ms 的 mic 丢弃 + 200 ms 盲窗 → **用户在这 ~600 ms 内说「停」听不见**；
2. 这恰好是用户最可能改口的时刻。

两条路，建议 **B**：

| 方案 | 做法 | 代价 |
|---|---|---|
| A. 沿用门控 | ACK 照常走 voice 通道 | 简单，但牺牲 0.6 s 打断能力（与 T4 目标冲突） |
| **B. ACK 豁免门控** | 给 ACK play 打标记（`content_ref: ack:*` 已有天然前缀），`feed_chunk` 对这一段**不整段丢 mic**，只保留极短盲窗（或 0） | 依赖 T8 结论：语音通道盲窗=0 时自听段=0（实测）；风险是 ACK 短句不含唤醒词，被 ASR 误收也只是"ignored_unwoken" |

### 5.4 改动量与验证

| 文件 | 改动 | 量 |
|---|---|---|
| `crates/hm-core/src/voice.rs` | 新短语组 + 超时定时器 + `play_local_ack` 复用 + 门控豁免标记 | **~120–180 行** |
| `crates/hm-core/src/api/mod.rs` | `PUT /mode` 增加 `processing_ack{enabled,phrases}`；快照字段 | ~20 行 |
| `web/src/views/ModeView.vue` + `i18n.ts` | 一张卡（开关 + 短语 chips + 试听） | ~50 行 |
| `crates/hm-core/tests/gateway_voice.rs` | ①超时播 ACK ②首字快则不播 ③ACK 不进历史 ④ACK 期间说「停」能停 | 3–4 条 |

**工作量：2–3 人日**（不含真机调音）。**不新增 crate、不动 v0 协议、不改固件。**

验证（真机，走用户入口）：
1. 问一个会调工具的问题 → 300 ms 内有 ACK，随后答案正常播出；
2. 问一个短问题（<1 s 首字）→ **不播** ACK（无叠音）；
3. ACK 期间说「退下」→ 停播 + 本地应答（证明门控豁免没破坏打断）；
4. `voice/log` 里 ACK 的 `kind` 可辨、`turn` 归属正确。

---

## 6. 问题 3：任务生命周期差距（含 generation fence）

### 6.1 逐项对照

| 能力 | Gander（文档） | 我们（代码） | 差距判定 |
|---|---|---|---|
| 创建 | `task_start`（从可信轮次） | `TaskRegistry::create`（`task.rs:73`） | ✅ 等价 |
| 取消 | `task_resolve(cancel)` | `request_cancel` + AtomicBool（`task.rs:93`） | ✅ 单轮够用 |
| 任务内改口 | `task_send(main)` 注入活动上下文 | **无**；改口=新 turn + barge-in 取消旧任务 | ❌ 缺 |
| 只读旁问 | `task_send(fork)` | **无**；旁问会取消主任务 | ❌ 缺（但半双工下收益有限） |
| 权限授权 | `allow_once` / `allow_session` / `deny` | **无**（工具直接执行） | ⬜ 暂不需要，接 HA 写操作前必须补 |
| 代际隔离 | 工具调用绑「transport 确认的最终轮次」 | `stop_epoch` 只作用于播放/合成；`current_task` 只守卫身份 | ⚠️ **部分：工具结果没有代际** |
| 迟到结果 | 由 Run/Delivery 状态承载 | 记录 `late` 标志进 trace，不采用 | ⚠️ 有记录，但**没有「旧代结果不得进入新答案」的硬约束** |

### 6.2 `request_cancel + AtomicBool` 够不够？

**单轮内：够。** 事实依据：工具执行前检查 flag（`ToolCtx::cancel_flag`），turn_body 在模型调用/工具批次前后
多次检查 `task.is_cancelled()`；播放侧另有 `stop_epoch`。这条链在本项目的喊停竞态里已经被回归测试钉死过。

**跨轮 / 异步任务：不够。** 具体缺三个东西：

1. **代际（generation）**：cancel 是布尔，无法表达「第 2 轮改口后，第 1 轮的工具回执作废但第 1 轮已产生的副作用要如实记账」。
   现在靠 `current_task` 指针比对，只在 voice 层；**工具结果落到会话/答案的路径上没有代际检查**。
2. **steer**：改口只能「取消 + 重开」，工具侧已完成的工作（如已搜到的候选）全部丢弃。
3. **旁问隔离**：没有只读通道；旁问会取消主任务（`barge_in_if_half_duplex("utterance")`）。

### 6.3 最小补齐方案（不抄五实体）

- `TaskState` 增加 `generation: u64` 与 `parent: Option<String>`；
- **每次用户改口/打断 → 会话级 generation += 1**；新 turn 的 task 继承新 generation；
- 工具结果携带 `generation`，写入会话/答案前比对：**旧代只记录、不采用**（与现有 `late` 语义合并成一个概念）；
- `stop_epoch` 与 generation **必须合并成一个"纪元"概念**，否则两套纪元会在 barge-in 路径上打架
  （这是实现时最容易踩的坑，见 §8）。

---

## 7. 问题 4：分三步实施建议

> 假设：1 名熟悉本仓的开发者；工作量含测试与真机验证，不含排期等待。

### 第一步（建议立刻做）：处理中应答 —— 把「秒回」从大脑手里拿回来

- **做什么**：§5 全部；默认关，控制台可开；短语默认「我看看 / 稍等，我查一下 / 马上」。
- **改哪里**：`voice.rs`（主）、`api/mod.rs`、`web/views/ModeView.vue`、`tests/gateway_voice.rs`。
- **工作量**：**2–3 人日**。
- **怎么验证**：§5.4 四条（真机入口 + 线上断言），外加「ACK 不增加首声总时长」的对照（同一问题开/关各 3 次）。
- **为什么先做**：用户感知最强（1.7–2.4 s 干等 → 300 ms 有回应）；不依赖 T4/T5/T1；不动协议与固件；
  失败可一键关（默认关）。

### 第二步（紧随其后）：turn generation + fenced 工具结果

- **做什么**：§6.3 的最小集；先把「代际」建起来，steer 可以第二步的后半段或第三步。
- **改哪里**：`task.rs`（generation/parent/状态）、`agent/mod.rs`（工具结果打代、写会话前比对）、
  `voice.rs`（barge-in 时 bump generation，`stop_epoch` 并入）、`tests/scenarios.rs`（确定性场景）。
- **工作量**：**3–5 人日**。
- **怎么验证**：
  1. 确定性测试：任务进行中收到「改口」→ 旧任务工具结果**不进入**最终答案，但 trace/tasks 里可见；
  2. 真机：讲故事途中说「换个短一点的」→ 只播新故事，旧故事不补播（对照现在会怎样，先复现再修）；
  3. 回归：已有的喊停竞态三条测试必须全绿（`stop_during_pending` / `stop_device_stops` / `barge_in_stops`）。

### 第三步（可选，按 T4/T5 进度决定）：fork 旁问（只做文本/控制台路径）

- **做什么**：只读旁问：不取消主任务、不改主任务上下文；答案走**控制台/文本**，语音路径等 T5/全双工。
- **改哪里**：`voice.rs`（旁问判定入口）、`agent/mod.rs`（只读 turn 不动 session 主上下文）。
- **工作量**：**2–3 人日**。
- **怎么验证**：故事播放中在控制台问「刚才那首叫什么」→ 主任务不中断、trace 显示 fork；
  **明确不承诺**语音旁问（半双工单喇叭做不到）。

### 明确不做（写进 roadmap，不再讨论）

1. 端到端小脑模型 / MiniCPM-o / 9B 任何形态的端侧推理；
2. 1 秒控制 token 的模型实现、音视频 token 流、128 单元上下文管理；
3. Codex/worker provider registry、五实体（Project/Task/Run/WorkerEvent/Delivery）、`lean`/`coordinator`；
4. 跨进程恢复、多 worker 并发；
5. CUDA/3 卡部署；
6. 用「小模型跑 control token」的方式在设备上模拟 Gander——482 MB/4×A53 不允许（KWS 已占一核）。

---

## 8. 风险与待确认

1. **两套纪元打架**（实现风险最高）：`stop_epoch`（播放）与新增 generation（任务）如果各管一半，
   barge-in 路径会出现「答案停了但工具结果还被采用」或反之。**落第二步时必须先画出状态图再写代码**。
2. **ACK 门控豁免的声学风险**：依赖 T8「语音通道盲窗=0 自听段=0」的实测；ACK 短语若与唤醒词/退下词同音，
   可能被自己的 ASR 收进来。验收里必须含「ACK 期间说退下能停」这一条。
3. **Gander 文档与实现可能有差**：本报告未读 `gander_runtime` 源码，generation/fence 的具体字段与状态机
   按文档描述评估；若后续要照抄语义，需再读代码。
4. **`fork` 的产品价值待定**：半双工单喇叭下语音旁问不成立；只做文本路径是否值得 2–3 人日，取决于控制台使用频率。
5. **`allow_once` 现在不做，但接 HA 写操作前必须补**——这是唯一一个「将来一定会欠」的账，建议在 roadmap 里显式标注。

---

## 9. 附：本次调研读到的关键原文（摘要级引用）

- Cerebellum–Brain 拆分：「The Cerebellum manages real-time interaction, while the Brain handles complex reasoning…」
- 1 秒单元与 chunk 结构：「fixed windows of one second and assemble one chunk per window」；
  「[video] [audio] [optional task context] → [control] [text or tool content]」
- 四控制值：`listen` / `speak` / `interrupt` / `tool`（README；arXiv HTML 为三值 + 独立工具段）
- 任务操作：`task_start` / `task_send(main|fork)` / `task_resolve(cancel|allow_once|allow_session|deny)`
- 上下文：128 单元（约 2 分钟）；音频 ~10 token/s；视频 ≤448×448
- 部署：Linux + Conda + CUDA 12.4，GPU0 Thinker / GPU1 Talker / GPU2 ASR，faster-whisper large-v3，Codex `app-server --stdio`
