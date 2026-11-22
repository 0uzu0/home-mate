# EchoMuse 最新 AEC/KWS 对照与可借点（2026-11-12，只读）

> 来源：EchoMuse upstream `9ff985d`（2026-11-11，克隆到 `/tmp/echomuse-latest`）、dot2 fork
> `178ba54`、我们的 B（网关仓＋主仓）、RUI-local（原始设计）与 A（冻结 Rust 实现）。
> 方法：Codex CLI 只读宽口径首轮读了约 200 KB 代码但未产出最终报告（上下文用尽）；本页由
> 同一组文件聚焦复核，每条给 file:line。**只作路线图输入，不改代码。**
> 许可：EchoMuse 为 MIT；SpeexDSP 为 BSD；Silero VAD / ONNX Runtime 需单独过
> `commercial-licensing.md`。**只借设计，不搬 Go/C 源码。**

## 0. 结论

1. **EchoMuse 的“每麦 AEC”是生产路径**：`device/internal/client/data.go:1327-1337` 在硬件参考
   可用时调用 `ProcessMicsWithRef`；`device/internal/aec/aec.go:93-109, 667-735` 给**每个麦**
   维护独立 Speex state，每周期全部自适应，只**返回当前选中麦**的抗噪输出。注释实测：单个
   filter 在上一轮那个麦上消 24–26 dB，换到另一个麦只有 23 dB 到 **−7 dB**；七个 state 全跑
   成本 6.04 ms/32 ms（19% 单核）。
2. **KWS 不是每麦**：唤醒模型吃的是**选中麦**（beamformer 的 active channel）AEC 后的单路
   （`data.go:1298` 先 `beam.Process`，`:1337` 再 AEC）。不是七路各跑一个唤醒模型。
3. **EM 的自触发守卫不是相关守卫**：用 `VoiceAudible(ScoreSpan)` / `MusicAudible` 判断
   “音箱还在响”，播放时把唤醒门限降到 barge 档，并在**设备本地** `CutReplyOnBarge` 直接切
   回答（`device/cmd/server.go:1391-1396, 1535-1548`）。`wakelevel` 记唤醒电平，
   `speechgate` 用 Silero VAD 判“这一段是不是人声”；`echoref` 只用于 Dot 3 独立 loopback
   的对齐（`device/internal/echoref/echoref.go:1-60`）。
4. **EM 已有 AEC 状态持久化**：`device/internal/aec/persist.go:12-41` 在收敛 ≥12 dB 后把每麦
   echo path 存盘（`.mics`），重启加载；注释写“冷启动 0–2 dB，收敛约 20 dB”。这正好打我们
   冷启动 3–5 s 的窗口。
5. **尾长/静音停放**：`aec.go:62-77` 的 `hwTailMs=64`、`quietHold=8`。他们实测 16 ms 尾在
   硬件参考上能消到和 300 ms 一样深，64 ms 约 4 s 收敛 vs 300 ms 约 10 s。我们的 160 ms 是
   dot2-d 房间混响选的，**不能直接照搬**，但“参考对齐准了以后尾可以更短”值得测。
6. **3 s 停播的结构差**：EM 在设备本地 cut；B 是 fact→core→Stop，Stop 排在 TTS 分片后面
   （实测 fact 3.078 s、Stop 确认再晚 1.9 s）。结构解＝**立即停播下沉到 gateway 本地反射**，
   core 只决定“这句话算不算一轮”。
7. **暂停/续播/时间/音乐**：RUI-local `SpokenProgram` 有界队列、句边界书签、暂停/续播、聊天与
   应用内容区分；A 有 Rust 落地（`src/runtime.rs`、`src/tools/music.rs`、`src/tools/clock.rs`）。
   B 的 `continue_intent` 只计数（`resumed:false`），音乐是 mock，无 time 工具。
8. **我们已经是逐麦 AEC**：B 的 `aec/mod.rs` 有 7 个 Speex canceller 逐麦处理后再 33-tap
   fusion。真正缺的不是“逐麦”，而是**选中麦/质量权重、持久化、尾长/静音停放、本地停播**。

## 1. EchoMuse AEC（file:line）

- **参考**：硬件参考是同一周期 TDM 帧里的 ch8（`data.go:1309-1316`）；软件 tap（扬声器 ALSA
  写）只是 ch8 不成立时的 fallback。硬件路径上 `hwTailMs=64`，软件 tap 才用配置的
  `aecTailMs`（`aec.go:62-70, 210-245`）。`echoref` 是 Dot 3 的独立 loopback 对齐，不适用于
  dot2-d（我们的 ch8 在 mic 帧内，由 dealias 处理）。
- **每麦**：`micStates` 只在硬件参考下构建；离开硬件参考或关闭 AEC 就销毁
  （`mics_test.go:211-236`）。`ProcessMicsWithRef` 对每个麦跑
  `speex_echo_cancellation`，返回 `active` 麦；非 active 麦在参考静音超过 `quietHold` 后
  暂停（`aec.go:667-735`；`mics_test.go:240-270` 断言“播放时每帧七个都在跑，静音后停”）。
- **为什么**：单个 filter 收敛在上一轮那个麦的声学路径上，换麦会退化到 −7 dB；七个都学，
  轮到哪个麦都已经是收敛的（`aec.go:93-101`）。
- **持久化**：`persist.go:12-41` 在衰减 ≥12 dB 后保存，最多一天一次；每麦一个 `.mics`
  文件（`micsMagic`），加载失败就冷启动；软件 tap 不保存（对齐依赖运行时 delay）。
- **代价**：6.04 ms/32 ms（19% 单核，`aec.go:99-101`）；`quietHold` 是为省这部分。

## 2. EchoMuse KWS / 自触发（file:line）

- **输入**：唤醒/ASR 用 `mono`＝active mic post-AEC（`data.go:1298-1337`）。不是每麦 KWS。
- **门限**：播放时 `speakerPlaying` 为真（`VoiceAudible(ScoreSpan)` 或 `MusicAudible`），
  唤醒门限降到 barge 档（`server.go:1391-1396`）。
- **本地 cut**：`barge := spk.VoiceAudible(wakeword.ScoreSpan)`，`CutReplyOnBarge` 只在
  “回答仍可听见”时切（`server.go:1535-1548`）；不是等识别出词再切，也不是回合 core 再发 Stop。
- **speech gate**：Silero VAD（`client/speechgate.go`），模型缺失时退回 RMS 阈值；用于
  no-speech timeout / end-of-speech / AGC release，不用于唤醒匹配。
- **wakelevel**：唤醒窗口 2.0 s、按 80 ms 帧 RMS 去数字增益，记录 level/peak；用于控制台
  记录与 barge 阈值，不是匹配器（`client/wakelevel.go`）。
- **echoref**：Dot 3 的独立 loopback 与 mic 对齐（±250 ms 粗搜、±40 ms 锁定、两次一致才移动、
  未锁不使用）；dot2-d 不适用（`echoref.go:1-120`）。
- **自触发**：EM 没有我们这种“残差相对参考＋收敛门＋相关守卫”；它靠“音箱是否可听”的
  设备本地状态和 barge 门限，加上本地 cut。两者不冲突：我们的相对判据解决“残余像人”，
  EM 的本地状态解决“停得慢”。

## 3. 我们的 B 与差距（file:line）

- `crates/gateway/src/firmware/aec/mod.rs`：7 个 `speex::Canceller` 逐麦、`dealias` 清理
  参考、`fusion::ArrayMixer` 33-tap 合并成单路上行。
- `aec/dealias.rs`：失锁不再喂 raw ch8（coast 输出环/零），按 20 ms 帧报 `LockState`。
- `bargein.rs`：近端判据相对参考 15 dB；5 个回声块收敛门；pre-AEC fusion 守卫；`BargeIn`
  带 `ref_db/residual_db/ref_locked`。
- `kws.rs`：旁路 sidecar（当前 `off`，`/data/local/tmp/hm-kws` 缺失），消费**融合后**的
  post-AEC 单路。
- `voice.rs`：`on_barge_in` 对 media 是 `media_playback_out_of_scope`（音乐靠唤醒路径）；
  `continue_intent` 只计数、`resumed:false`；无节目/书签模型。
- **差距**：
  1. **选中麦**：EM 每轮选 active mic 给 KWS/ASR；我们融合后才给 detector/KWS。坏麦残差会进
     fusion；是否改 active mic/质量加权，先测唤醒 CER 再定。
  2. **持久化**：没有；冷启动 3–5 s 的自听窗口本可避免。
  3. **尾长/quiet hold**：没有；160 ms 是为房间混响选的，不能直接缩短。
  4. **本地停播**：没有；Stop 走 core 回程，长流式回答会排在 TTS 分片后。
  5. **节目/暂停/续播**：没有；RUI-local/A 有现成语义与 Rust 实现。

## 4. 3 s 停播：证据与结构解

- 2026-11-12 夜间日志：`barge_in_stop latency_ms=3078`，`stop_stage` 再晚 1.9 s 才确认；
  同一分钟 `mic_speech_started` 的 `ts`/`ts_ms` 只差 1 ms → **不是时钟偏移**，是真实延迟。
- 共同点：长流式回答（57 s 整段重试）；短回答 35 ms。Stop 帧排在 TTS 分片后，设备确认也被
  拖到 1.9 s。
- 结构解：**gateway 本地停播**。固件在 near-end fire 的同一线程/同一命令通道直接停 voice
  播放（它已经有 playback thread 和 flush 能力），然后照常上报 fact；core 只做“停播已发生，
  这句话是否成一轮”的决定。Stop 不再依赖 core 回程，也不排在 TTS 分片后。
- 先测归属（不改代码）：在 gateway 收包点记录 `device_ts`/`gateway_rx_ms`/`core_rx_ms`/
  `downlink_tx_ms`，确认 3 s 是在设备出站、核心事件泵还是下行队列。即使 fact 晚，本地停播
  也能先把体感修好。

## 5. RUI-local/A 的暂停/续播/时间/音乐（file:line）

- RUI-local `docs/continuous-conversation-design.md:22,36`：聊天打断、应用暂停、结束会话分开；
  普通旧回答丢弃；音乐/故事按应用归属保留书签；持续聊天不自行恢复；**手动暂停不能变成自动
  续播**；`rui_gate_state.SpokenPlayback` 有句边界书签。
- RUI-local `docs/agent-brain-research.md:36`：`SpokenProgram` 已有有界队列、暂停/续播、取消、
  generated/submitted/current 内容；不要另造故事播放器。
- RUI-local `overlay/runtime/rui_spoken_program.py`：`sentences()` 句边界切分（`:70-80`）、
  `SpokenProgram`（`:99`）、`current_text`/`submitted_text`/`generated_text`（`:121-123`）。
- A `src/runtime.rs`：`Paused`/`Bookmark`（`:197, 525, 1052-1078, 1181-1200`）；
  `src/tools/music.rs` 是 RUI-local 音乐协议的 Rust 适配器；`src/tools/clock.rs` 是 43 行
  本地时间工具（`现在是%H点%M分`）。
- B：`continue_intent` 只计数；`music.play` 在 `tool/mocks.rs` 是模拟曲库；无 time 工具。
- **搬法**：不要改 stream loop。core 增加“节目会话”层（聊天 vs 故事/音乐、句边界书签、
  pause/resume 状态、手动暂停不自动恢复）；gateway 增加 Pause/Resume（或 stop+bookmark）
  命令与回执；工具接口把 time/music 作为独立能力。RUI-local 是语义参照，A 是 Rust 代码参照。

## 6. 逐机制对照

| 机制 | EM 最新 | B 现在 | 差距 | 适用性 |
|---|---|---|---|---|
| 每麦 AEC | 七路都跑，返回 active mic（`data.go:1327`） | 七路都跑，fusion 成单路 | 没选麦/没质量权重 | **可测**：active mic vs fusion 的唤醒 CER |
| AEC 持久化 | `.mics` 存盘，重启加载 | 无 | 冷启动 3–5 s | **最高价值**：冷启动窗口直接缩短 |
| 尾长 | `hwTailMs=64` | 160 ms | 可能过长 | 先测，不照搬（dot2-d 混响） |
| 静音停放 | `quietHold=8` | 无 | CPU | 可选优化 |
| 唤醒输入 | active mic post-AEC | fusion post-AEC | 可能损失最佳麦 | 先测 |
| 自触发/停播 | 设备本地 `CutReplyOnBarge` | fact→core→Stop | 3 s 停播 | **结构解**：本地停播 |
| 暂停/续播 | RUI-local `SpokenProgram` | 无 | 功能缺口 | A/RUI-local 搬 |
| 时间 | A `clock.rs` | 无 | 功能缺口 | 小工具搬 |
| 音乐 | A/RUI-local 服务 | mock | 功能缺口 | 复用 RUI-local 服务 |

## 7. 不建议照搬 / 风险

- **Go/C 源码**：只借设计；EchoMuse MIT、Speex BSD，但我们的代码/商业许可需要按
  `commercial-licensing.md` 评审；Silero/ORT 模型单独评审。
- **Dot 3 的 `echoref` 独立 loopback**：dot2-d 的 ch8 在 mic 帧内，已有 dealias；不要引入
  第二套对齐。
- **EM 的 beamformer/硬件几何**：dot2-d 是固定 7 麦 + 33-tap crossover，不是可转向波束；
  “选中麦”要靠质量/能量/相关选，不是照搬 beam angle。
- **64 ms 尾长**：dot2-d 的 T8 用 160 ms 解决混响；缩短必须重跑 ERLE/自听判据。
- **每麦状态存盘的二进制格式**：Speex state 是内部结构，跨版本/跨过滤器形状不可移植；
  先做导出/导入 spike，带 magic/frame/tail/rate 断言，失败就冷启动。

## 8. 待测（不改代码）

1. **收包时间戳**：device_ts / gateway_rx / core_rx / downlink_tx，分清 3 s 归属。
2. **active mic vs fusion**：同一唤醒/打断素材的 CER/触发率对比。
3. **AEC 持久化 spike**：Speex state 能否导出/加载；冷启动 ERLE 曲线前后对比。
4. **尾长 A/B**：64/96/160 ms 的收敛时间与稳态 ERLE；不降 ERLE 才考虑。
5. **RUI-local 音乐服务**：现有 NAS/Personal Services 的 search/play/pause/resume 端点与凭证。

## 9. 证据路径

- `/tmp/echomuse-latest`（9ff985d）；`device/internal/aec/aec.go`、`persist.go`、
  `mics_test.go`；`device/internal/client/data.go`；`device/cmd/server.go`；
  `device/internal/client/{wakelevel,speechgate}.go`；`device/internal/echoref/echoref.go`。
- `/Users/bakeyzhang/AI_Workspace/01_Code/projects/echomuse-dot2-ref`（178ba54）。
- RUI-local：`docs/continuous-conversation-design.md`、`docs/agent-brain-research.md`、
  `overlay/runtime/rui_spoken_program.py`、`config/config.yaml`。
- A：`src/runtime.rs`、`src/tools/music.rs`、`src/tools/clock.rs`、commit `8bf0e35`。
- B：网关仓/主仓当前 HEAD（2026-11-12 日间续）。
