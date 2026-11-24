# 合同草案 v0（评审用）

日期：2026-11-07（v0.1 G1 回填；**v0.2 G3 上行音频回填**；**v0.6 2026-11-13：音量 device 归因、播放兜底/残留清理、HA 两种接入、播放看门狗**；**v0.7 2026-11-13：房间相对端点、ACK 终态分类与回合身份、语音回答长度、工具新鲜度/失败呈现（协议不变，管理 API 与行为变更）**；**v0.8 2026-11-13：交付形状三档＋失败短答、跟随窗提示音不压媒体（协议不变）**；**v0.9 2026-11-13：天气国内备用源＋城市代码、带命令的唤醒不播 ACK、音乐流不再受总时限（协议不变）**；**v0.10 2026-11-13：通用搜索服务与 `web.search`、媒体会话与 `music.control`、唤醒＝暂停（协议不变）**；**v0.11 2026-11-13：播放/采集看门狗改为「有界非零退出（75）由监督器重启」、静态时钟写纳入写闸门、写集合取写入时刻（协议不变）**；**v0.12 2026-11-13：终态回执区分 consumed/written（`consumed_ms`/`progress_basis`）、flush 失败不再确认、断线即隔代（协议字段新增、向后兼容）**；**v0.13 2026-11-13：端点检测器进 `/mode.mic.vad.endpoint`、stop_receipt 结算该 play 的等待者、逐句续播按消费终态推进、回合等待预算 15/45 s、ASR 输入身份（协议语义新增，字段全部加法）**；**v0.14 2026-11-13：语音事件可导出（`/voice/events`，按天/大小滚动、总量上限）、每轮时间线（`turn_end.timeline` + `turn_audio_end`）、`aec.owner` 归设备且缺省不猜、KWS 加载证明 `loaded_hash`、无效能量记 null；M7：动作闸门单字词、新闻多源默认合并、记忆自动提取候选（后台确认后生效）**；**v0.15 2026-11-14：应用发布与回滚（M8a）——`/api/v0/release*`、`/status.build`、`db.schema`、Ed25519 清单、唯一激活者＋健康确认、DB 快照/开机守卫；设备协议不变**；**v0.16 2026-11-14：语音转写的唤醒兜底成为用户设置（`asr_wake_fallback`，默认开；关＝空闲说话不转写，`mic.counters.asr_skipped` 计数；设备协议不变）**；**v0.17 2026-11-14：声音风格——`/api/v0/config/audio` 与 `audio_eq` 帧，固定喇叭校正＋用户口味 EQ（EchoMuse 对齐；设备协议新增字段，向后兼容）**；**v0.18 2026-11-14：审计收口——OpenAPI 关闭时不读文档即可保存（地址变更才清白名单）、本地关键词识别零唤醒词时给部署前警告、发布状态 `last.db.snapshot_ms`（断电守卫改读 sidecar `ms`，不再依赖文件 mtime）；设备协议不变**；**v0.19 2026-11-14：OpenAPI 附加认证头（`auth_header`，值复用令牌）＋设备 id 唯一（重复 id 的 hello 整条拒绝）**；**v0.20 2026-11-14：琥珀只点"人的暂停"——`Program.system_cut`（预算截断书签不点琥珀、跟随窗关闭即清）、`spoken.resume_lead` 续播开头语（设备协议不变）**；**v0.21 2026-11-14：交付形状交给模型（三档同一段提示词、关键词词表退役、运行时只留失控兜底 400 句/2000 字、续播按快照读整段）、定时提醒（`/api/v0/reminders` + `reminder.*` 三工具、schema 6）、工具权威句在回执时外发**；**v0.22 2026-11-14：交互审计修复——续播 epoch 随书签绑定（退场后不复播）、`resumed` 如实报告、出声期丢块计数 `suppressed_during_voice`、提醒避让未返回的转写与刚关段的 2.5 s、转写唤醒同样接管媒体（保留队列）**；**v0.23 2026-11-14：家庭平面鉴权——`/api/v0/auth` 三个入口（状态／首配／轮换）、其余 `/api/v0/*` 需 `Authorization: Bearer`（loopback 信任保留给 `hm-release`，控制台静态文件不鉴权）、令牌只存 SHA-256、`/status.auth` 只报状态；设备协议不变**）；**v0.24 2026-11-14：静音下 `reflex.deploy` 语义成立——固件 0.0.46 的驱动有界等待 + 监督者 SIGKILL 升级，回执 `detail` 只说 "restart signalled" 不说 "restarting"，`loaded_hash` 仍是唯一加载证明；设备协议字段不变**）；**v0.25 2026-11-15：关窗播报不再续期自己——按 play id 忽略那一次 `PlayFinished`（发布 33 曾每 21 s 说一次「拜拜啦」），`follow_window_tone` 默认改 `false`（窗口与追问额度不变；设备协议不变）**）；**v0.26 2026-11-15：凭证库与互联下发——密钥在库内加密（`data/secret.key` + ChaCha20-Poly1305，值 `enc:v1:`，旧明文行可读并在启动时封装、env 独有密钥一次性回填）、对端与令牌（`config_kv.fleet`，令牌同样加密）、`/api/v0/fleet*` 与 `POST /fleet/push`（`include_secrets` 默认 false、`dry_run` 只出计划）、导出补齐 `reflex`/`audio_eq`、唤醒词永不下发（发送端剥 + 接收端拒）；设备协议不变**）；**v0.27 2026-11-15：预合成应答随配置变化重建——导入/历史恢复/关窗缺素材都触发
`spawn_ack_synthesis`（关窗事件新增 `synth:"requested"` 字段），种子里的应答 WAV 降级为缓存；
设备协议不变**）；**v0.28 2026-11-15：KWS 驱动成为发布组件——清单新增可选 `components.kws`
（dir/entry/files，缺省=旧发布照常安装）、包内该目录一律 0755、激活时逐文件硬链接进
`/data/local/tmp/hm-kws` 且永不改写 `kw.txt`、回执新增 `kws_bundle` 判据；`run.sh` 改为从
`hm-direct/device-id` 读节点名（与节点无关）；设备协议不变**）；**v0.29 2026-11-15：结束会话为最高优先级——空闲时命中 `退下` 类也执行
（停一切＋关窗＋播结束语），不再静默丢弃 `reflex_precondition:no_session`；`precondition` 增加
`idle` 字段；设备协议不变**）；**v0.30 2026-11-15：唤醒仲裁——`keyword` 帧新增 `rms_dbfs`（1.6 s 窗内后 AEC 峰值，与喂给 spotter 的同一批样本）与 `lag_ms`（驱动自报的新鲜度，`ts - lag_ms` 即该句最后一段音频到达的时刻）；新增 `POST /api/v0/fleet/wake`（对端声明，与现有 fleet bearer 同一鉴权，回执带上本机的同名声明）与 `/mode.wake_arbitration`（`off`/`observe`/`enforce`，全局，同一条管理路径与变更记录）；决定规则为纯函数（先到者胜，1.6 s 内视为同一句并按响度差 3 dB 改判，毫秒级并列按固定顺序兜底并记 `arbitration_tiebreak`），事件 `wake_claim`/`arbitration_won`/`arbitration_lost`/`arbitration_unavailable`/`arbitration_observed`；退下/停止不走仲裁；设备协议字段全部加法、旧固件不报即为 `null`**）；**v0.31 2026-11-16：KWS 本地退出（交互合同 v2 #5）——`kws_deploy` 新增 `retreat_words`（本份词表里意图为 `retreat` 的词）与 `intents_hash`（词→意图映射的 FNV-1a）；`kws_deploy_receipt` 新增 `local_exit`（设备实际装载了几个退出词）与 `intents_hash` 回显；新增网关→核心帧 `local_exit_flushed`（`keyword`/`ts`/`latency_ms`/`channels`/`detail`）。网关命中未被回声守卫判为自听的退出词时，**先本地 flush 两个声道并报事实**，核心 `close_session` 仍是会话唯一权威；设备的词表意图未装载时，`hm-release` 在激活阶段补推一次。设备协议字段全部加法，旧核心/旧网关互相仍可解析**）；**v0.32 2026-11-16：桌面实测三修（交互合同 v2 #2/#3/#4）——跨机 cue 回声不再被当请求（`mic_foreign_cue_echo`，同核心内的家庭事实）、停播类工具不再取消自己那一轮（`stop_program_for_tool`，人说的停止语义不变）、识别灵敏度 `kws_threshold`/`kws_score` 成为户设置并随 `kws_deploy` 下发（`kw.tuning.json` → `KWS_THRESHOLD`/`KWS_SCORE`，回执与报告回显模型自报的在用值）；个人服务「故事」以 `openapi` 块接入搜索（实测可用），播放待 symphonia `aac`/`isomp4` 与故事源适配器**）。状态：**v0/v0.2 已实现并经合同测试**（主仓 `crates/hm-core/src/gateway.rs` ↔ 网关仓 `crates/gateway/src/protocol.rs`，两侧镜像、无共享代码，以本文为权威）。三份合同：设备协议（核心↔网关）、管理API（控制台/语音管理↔核心）、工具合同（大脑↔工具层）。版本号与软件版本分开。

约定通用语义：

- **证据等级**：`proposed`（模型建议）→ `accepted`（执行层受理）→ `submitted`（已提交网关/服务）→ `sent`（已发送，有传输证据）→ `device_confirmed`（设备回执确认）→ `unknown`（断线/超时后保留未知，不冒充成功）。
- **任务身份**：每个有副作用或长动作的请求有 `task_id`（单调/ULID）；迟到结果只补记原任务；取消后旧任务不得提交新动作。
- **连接代**：`generation` 随重连递增；旧代事件到达时仅记账，不触发新动作。

## 一、设备协议 v0（核心 ↔ 设备网关）

范围：先**事件级**（唤醒、文本输入、播放回执、停止、音量），PCM/音频帧布局随后续里程碑扩展。

**G1 定稿（2026-11-07）**：

1. **传输**：TCP＋换行分隔 JSON（LDJSON）；核心监听（默认 `127.0.0.1:8932`，`HM_GATEWAY_BIND` 可改），网关拨出、重连递增代。单网关连接为 v0 形态：新连接取代旧连接（旧代待回执按代释放，等待方得到 unknown）。
2. **帧上限**：单帧 8 MiB；握手 hello 10s 超时；超限/超时断开。
3. **音频传递（v0）**：play 帧内 `audio_b64`（base64 WAV，s16le）内联；PCM 流式为 v1 议题。`content_ref` 前缀区分来源（`tts:*`＝内联语音，`mocklib:*`＝模拟曲库，仅回执与时长，无采样）。
4. **输出通道 `channel`**（v0.1 新增）：`voice`（TTS/应答）与 `media`（音乐）可并存（真实产品为闪避混音；虚拟终端并行计时）。唤醒打断（半双工生效时）与 stop 同时作用于两通道——核心以停止命令实现，网关无需理解 `barge_in` 字段（保留为声明性字段）。
5. **`mode_set`**（v0.1 新增，核心→网关）：请求双工模式（half/full 均发送）；网关必须以 `mode_report` 回应协商/生效事实，能力不足时如实保持 half。
6. **`play_finished`**（v0.1 新增，网关→核心）：设备自身观测的消费完成（played_ms），不承诺用户听到；stop 之后不再补发。
7. **回执诚实性**：`play_receipt.stage=unknown` 在核心侧映射为证据 `unknown`（工具层报 `ToolStatus::Unknown`），绝不转为失败或成功。

### 握手与能力

```jsonc
// gateway → core（v0.8 起新增 min_protocol / gateway_version，serde default 可选）
{ "type": "hello", "protocol": 0, "min_protocol": 0, "gateway": "home-mate-device-gateway 0.0.1",
  "gateway_version": "0.0.53",
  "devices": [ { "stable_id": "dot2-d", "model": "AEOBC",
                 "capabilities": ["wake_event","text_in","audio_out","volume","stop"],
                 "audio_profile": "fireos_builtin_speaker" } ] }
// core → gateway（v0.8 起新增 min_protocol / core_version）
{ "type": "hello_ack", "protocol": 0, "min_protocol": 0, "core_version": "0.0.1", "generation": 12 }
```

- 能力按需声明（`text_in`、`audio_in`、`audio_out`、`wake_event`、`volume`、`stop`、`wifi`、`play_stream`、`remote_mute`、`local_exit`、`interrupt_wake_word`、`interrupt_retreat_local_exit`…）；没有的能力不得伪造回执。
- `audio_profile`：输出/输入路径标识（来自设备基线证据，如 dot2-d=FireOS＋内置扬声器）；核心不写设备型号分支，仅透传展示。
- v0.8 的兼容判定是**区间求交**（见 v0.8 节），不是严格相等；省略 `min_protocol` 的端按"只接受 0"处理。

### 事件（gateway → core）

```jsonc
{ "type": "wake",        "device": "dot2-d", "ts": 1762500001123, "seq": 481 }
{ "type": "utterance",   "device": "dot2-d", "ts": ..., "source": "text", "text": "讲个睡前故事" }
{ "type": "button",      "device": "dot2-d", "action": "action_press" }
{ "type": "mute_state",  "device": "dot2-d", "muted": true, "source": "hardware" }
```

- M1/M2 虚拟终端用 `source:"text"` 模拟识别结果；真实 ASR 音频帧（`source:"asr"`）字段后续扩展。
- `seq` 单调递增，用于迟到/乱序诊断。

### D0 来源身份合同（2026-11-15，本地接收端实现；设备发送端未接线）

本节为协议 0 的加法扩展。Wake / Utterance / Keyword / AudioChunk 均有可选 `origin`；
三种身份分属不同所有者，不能互相代用：

| 身份 | 签发者与时点 | D0 表达 |
|---|---|---|
| 传输 | hub 收到帧时，使用该 socket 的连接代和本次入站序号 | `provenance.transport={generation,inbound_seq}`；不是发送端 `seq` |
| 采集 / 原始输入 | 源开始采集上下文、采样或接收文本时，异步识别前 | `origin` 如下；hub/voice 不补当前票据 |
| 请求 | 将来 D1 的串行会话所有者在准入时 | session generation / request_id / 输出取消 epoch；**D0 不产生** |

```jsonc
// 同一源输入的 PCM 与 KWS 使用共同坐标；结果发送 ts/seq 不参与此身份。
{"type":"audio_chunk","device":"dot2-d","ts":123,"seq":10,
 "origin":{"kind":"audio","capture_epoch":"source-boot-A","ticket":"capture-17",
           "input_id":"input-17","start_sample":0,"end_sample":1280},
 "pcm_b64":"...","format":"s16le","sample_rate":16000,"channels":1,"layout":"mono"}
{"type":"keyword","device":"dot2-d","ts":999,"seq":40,"keyword":"高达",
 "origin":{"kind":"audio","capture_epoch":"source-boot-A","ticket":"capture-17",
           "input_id":"input-17","start_sample":0,"end_sample":1280}}
// 文本输入有自己的不可变 ID，没有音频区间；不能用文本/到达时间猜音频归属。
{"type":"utterance","device":"dot2-d","ts":123,"seq":41,"source":"text","text":"你好",
 "origin":{"kind":"text","input_id":"console-input-93"}}
```

**签发与切换边界**：采集源拥有 `capture_epoch`，在源重启、采样坐标重置、格式/采样率变化或
采集上下文撤销后开始新 epoch；同一 epoch 的坐标不得回退复用。`ticket` 是采集上下文票据，
在该上下文开始、撤销/切换时由源签发/轮换，不从 hub 连接代推算。源在将输入交给任何异步 KWS
任务前冻结 `input_id`、票据和源区间；任务完成、重新发送、换连接时原样带回。
连续流中的输入标识可覆盖一个源采集区间，并供 PCM 与 KWS 共享；不能分别按 KWS 结果和
ASR 文本临时造 ID。区间为源采样时钟上的 `[start_sample,end_sample)`，单位是每通道采样帧
（mic_ref 的一组 mic/ref 算一帧）；设备 ID + epoch + ticket + input_id 是来源作用域，区间
才表达该输入中的实际音频。匹配用共同作用域与区间交叠，不用相同文本或相近到达时间。
该合同要求采集源真实提供共同坐标，D0 不替外部网关伪造语音分段归属。

**VAD 与 ASR**：每块被保留的 PCM 同时保留其源 envelope。preroll、开段、关段、半 hangover
裁剪与 barge head 切片遵循现有音频行为；reset 丢弃对应 PCM 及元数据。段的来源是有序
`ticket.spans[]`（内部工作票据），每项包含 `provenance={origin,transport}`、`offset_samples`、
`samples`。offset 相对该原始输入帧的音频起点，samples 是本段实际保留的 mono 帧数；原始
origin 不改写。跨上下文的段明确保留多个来源，禁止把整段标成关段时的“当前来源”。ASR 在
派发前冻结该列表，成功、失败、空结果均原样返回；不在结果返回时读当前连接或新票据。
`mic_speech_started.provenance`、`mic_segment_closed.ticket`、`mic_transcript_source.ticket`
和 `keyword_source.provenance` 供现有 voice 日志/导出观察。M5 原有 input_id 数字水位、关段时
stop_epoch 与块计数保留原行为，不能当本节源采集身份或 D1 请求身份。

**能力与兼容**：发送端可在每设备 hello/DeviceStatus descriptor 的 capabilities 声明
`input_origin_v1`，表示它能履行上述四类输入来源冻结及共同坐标合同；源端应在开启这项能力前
完成 KWS/PCM 接线。D0 核心接收并保留该能力描述，解析 additive origin，hello_ack 仍仅有
protocol/generation，**尚无严格能力选择或票据签发握手**；未声明但带 origin 的帧同样透传，
不据此推断协商成功或身份真实性。缺整个 origin（包括旧端）反序列化为 `{"kind":"legacy"}`；
本地测试注入或旧 BargeIn 前滚缺 transport 时明确 null。即使传输代新鲜也保持“采集身份未证明”，
绝不由 ts/seq/fed_ms/当前会话代补成 Audio。字段存在但形状缺必填项按普通坏 JSON 帧处理。
本片不增加采集新鲜性拒绝、不按源 ID 改去重，也不改变旧端的开窗/退出/额度行为。

**将来严格拒绝的前提（尚未实现）**：双方明确协商选中严格版本、源端完成采集/KWS/PCM 接线、
采集票据的签发/撤销/切换可核验，并由 D1/D2 串行所有者绑定请求与关闭边界后，才可按该版本
拒绝缺票/Legacy、无效/空 ID、无效区间、与 PCM 长度/格式不符、已撤销票据或旧采集代输入。
兼容退出及迁移必须显式可观察，不可默认把旧端升级为严格身份。D0 只证明身份未被洗新，不证明
来源可信、旧事件全部拒绝、零重开、播放重叠成立或真实设备声学验收。外部网关仓、真实设备
及 BargeIn 自身的源票据扩展不在本片；其缺票前滚保留 Legacy，不能把它洗成后续 AudioChunk 的来源。

### D2 采集撤销租约与关闭边界（2026-11-15；主仓核心/假网关实现；2026-11-16 已移除未实现的 `capture_lease_v1` / `capture_grant` 线框）

> 注：外部网关从未实现 `capture_lease_v1` / `capture_grant` 线框，核心侧已于 2026-11-16 清理 `CoreFrame::CaptureGrant` 死代码；`Participation` 内部租约逻辑保留。

D0 上文保留为该轮证据；以下是 D2 新增的接收合同。外部网关仓和真实设备尚未接线，不能
因为这份核心实现而宣称现网设备已经支持。没有改变既有协议编号；以设备能力
`capture_lease_v1` 选择这项加法扩展，未声明的旧端不会收到新帧。

```jsonc
// core → gateway：上线/重连，以及任何实际会话关闭后发放。
{"type":"capture_grant","device":"dot2-d","lease":"capture-opaque-core-issued-id"}
// gateway 在采集/接收文本时冻结 lease，PCM、KWS、协议文本的异步结果原样携回。
{"type":"keyword","device":"dot2-d","ts":999,"seq":40,"keyword":"停止播放","self_heard":true,
 "origin":{"kind":"leased","lease":"capture-opaque-core-issued-id",
   "origin":{"kind":"audio","capture_epoch":"source-boot-A","ticket":"capture-17",
             "input_id":"input-17","start_sample":100,"end_sample":200},
   "overlap":{"play_id":"P-original","capture_epoch":"source-boot-A","ticket":"capture-17",
              "start_sample":150,"end_sample":250}}}
```

- 租约由 `Participation` 持有；激活的 session generation 与采集撤销租约职责不同。
  同一口音频的 KWS 激活不会撤掉其 ASR 原票；实际关闭（显式退出、timeout、SingleRequest）
  generation +1、额度清零、撤采集票/reset，重连也更换票。不可在识别完成/重发时读新 grant
  给旧音频补票；未知但第一次迟到的旧输入也必须按它原来的 lease 拒绝。
- 非空 Text input_id 或完整 Audio 身份/正向半开区间才可包在 Leased 内，禁止嵌套包票或
  包 Legacy。hub 固定传输代，核心另核当前连接归属；发送 ts/seq/fed_ms 不能替代采集 lease。
- **旧端兼容边界**：新核心起始仅兼容未证明来源的 Legacy/未包票输入；**任何一次实际关闭后**
  都 fail closed，普通 timeout/关闭跟随窗的单次请求结束也包括在内，激活/重连不会重新放宽。
  因此旧发送端在首次关闭后不能继续语音激活。真实部署前必须完成发送端能力声明、实际 grant
  接收与采集时冻结接线；本轮只在假网关验证，不把它写成可直接部署的兼容升级。
- `overlap` 可选，表示发送端在与命中相同的 capture_epoch/ticket 采样时钟上测得的真实播放
  重叠区间；play_id 必须指向本代仍在播放的 answer，核心记录的实际提交文本也必须含该词。
  两证同时满足才窄拒绝 self_heard retreat；缺证、错 play/源、无交叠或回答不含词均接受退出。
  不用播放提交时钟/合成长度/fed_ms/KWS发送时间伪造重叠。本轮证据是可信票注入，非真实声学。
- 播放中 PCM 进入既有 VAD/ASR 控制识别。保留的 `ticket.spans[]` 加内部
  `control_only:true`（false 不序列化），preroll/裁剪照实传播；普通自声/电视声不能借播放结束
  后的状态成为请求。退出、唤醒与普通准入分开；识别出的退出先于 cue stripping、自声、media、
  额度和普通意图去重。旧 `retreat_intent`/普通 `keyword_interrupt` 开关不再禁用已识别退出。
- 普通 Utterance 永不自开窗；显式 Wake/KWS/ASR 唤醒才激活。首问免费，后续最多 N 次；
  额度只花不补。只有本次有效请求全部 answer 正常播完，且生产结束，才重给完整
  follow_window_ms；任一 answer 失败/unknown/停止、无音频、ack/filler/prompt/提醒/诊断没有续窗权。
- 原始 `(generation,request_id)` 与播放 epoch 从准入贯穿整段 TTS/stream/回执。本地 clock、
  提醒、截断提示及异步 ack 也不得在排队/合成结束后取当前代补票。Request 的共享取消 scope
  在 agent spawn 前绑定，关闭撤销全部在途请求（含尚未宣布 task_id 的任务），session 写入与
  关闭清历史通过该 scope 的 commit gate 排序。媒体等待取流/解码与最终注册也核原取消票。

管理 close API、新 session 管理快照/UI、0 次配置与模型 session.close 仍属 D3，本节不新增它们。

### 播放与停止（core → gateway，回执 gateway → core）

```jsonc
{ "type": "play", "task_id": "T01", "device": "dot2-d", "content_ref": "tts:abc123",
  "audio": { "format": "wav", "sample_rate": 24000, "channels": 1 }, "barge_in": "stop_current" }
{ "type": "play_receipt", "task_id": "T01", "stage": "accepted|submitted|sent|device_confirmed|failed|unknown",
  "detail": "...", "ts": ... }
{ "type": "stop", "task_id": "T01", "flush": true }
{ "type": "stop_receipt", "task_id": "T01", "stage": "sent|device_confirmed|unknown" }
```

- `content_ref` 引用音频缓冲（核心先合成、网关拉取或推送，G1 定）；`barge_in` 声明播放中新唤醒的打断语义。
- 停止后：核心停发旧任务数据；网关区分"停止请求已发送 / 设备已确认清空 / 未知"。重连后旧代回执只记账。
- 断线注入是 G1 合同测试的必测项。
- **播放看门狗（2026-11-13 定；v0.11 改契约，M4）**：网关在"一次写（数据周期**或静音时钟**）超过
  `--play-watchdog <s>`（默认 10 s，0=关）没有完成"时，按名回报
  `play_receipt{stage:"failed", detail:"play_watchdog: no DAC progress …"}`、拆流清槽，
  然后**在有界窗口（1.5 s 排空）后以退出码 75 退出**，由监督器重启做全新 codec bring-up。
  旧合同"不退出进程"**作废**：进程留着只证明"线还活着"，音频回不来。
  - 写集合取**发起该次写那一刻**的活跃通道（首写阻塞也可见——旧实现读循环顶部那份、
    比命令排空早一拍，会漏掉一次播放的第一个周期）；`detail` 仍带 `played_ms/pushed_ms`。
  - 只是"等下一块"的饥饿（没有待写数据在途）**不算停滞**，不退出。
  - 数据写**返回错误**＝DAC 通路已死：该通道立即回 `failed` 收据，之后到达的 `stop` 走立即回执路径
    （不挂死 `pending_stop`），同样退出 75。
  - 采集侧：连续 `--capture-watchdog <s>`（默认 10 s）**没有一次成功采集块**＝采集线程已死
    （panic 或读永不返回），同一退出路径——听不见的盒子即使还能说话也算故障。
  - 退出码 **0 只留给 init 的 TERM**（主动停止：监督器不再拉起）；75 由监督器**记名**重启
    （`audio fault (code 75)`），并按既有快退策略计数，重复出现仍可回退。
  - **不覆盖**：写入卡在内核不可中断等待（D 态）时进程根本退不出去——那是镜像级故障，
    监督器救不了；不许把它算成"已兜住"。
  - 仓内测试钩子 `--fault-inject-ms <ms>`（默认 0）走**真实**的"回执→有界退出→监督器重启"路径，
    用于真机验收；它不模拟内核卡死。
- **停止即终态（v0.13，M5，2026-11-13）**：`stop_receipt` 是**该 play 的终态**，不只是停止请求的回执——
  核心在收到它时同时结算 `play()` 的等待者（此前只有 `play_finished` 会结算，逐句续播因此只能等到超时）。
  字段与 v0.12 相同（`stage`/`played_ms`/`consumed_ms`/`progress_basis`/`detail`），语义新增。
- **消费与代（v0.12，M6a，2026-11-13）**：
  - **终态回执区分"写了多少"与"DAC 吃了多少"**：`play_finished` 与 `stop_receipt` 可选带
    `consumed_ms` 与 `progress_basis`（`hw_delay`＝读 `SNDRV_PCM_IOCTL_DELAY` 的积压后算出的消费量；
    `written`＝只能报写入量）。**缺失 `consumed_ms` 时不许把 `played_ms` 当作"听到"**。真机实测：
    1408 ms 的播放 `consumed_ms=1252`（积压 156 ms）；1109 ms 处停止 `consumed_ms=940`（170 ms 被 drop）。
  - **flush 语义**：`stop_receipt` 的 `device_confirmed` 只在 DROP 成功后发出；DROP 失败＝
    `stage:"unknown"` ＋ `detail:"flush_failed: …"`（该通道可能还在响）。DROP 之后的 PREPARE/START 只是
    重新起钟：本机 vendor 驱动对空 buffer 的 START 回 EPIPE，那是 underrun 不是"没停住"，计入
    `pcm_underruns` 而不降级回执。
  - **连接代**：网关每次**链路结束**（EOF 或错误）都清槽、清流、flush 两个通道，再重连——旧播放既不能
    续到新一代，也不能在新一代上报旧任务的终态。核心侧对应：回执只结算**提交它的那一代**；
    跨代（或已被 teardown 退役的 play_id）的事实只记 `stale_receipt`，不结算等待者、不产生事件。

### 音量与模式

```jsonc
{ "type": "volume_set", "task_id": "T02", "device": "dot2-d", "value": 0.6, "unit": "linear" }
{ "type": "volume_receipt", "task_id": "T02", "effective": 0.6, "unit": "linear",
  "stage": "device_confirmed", "device": "dot2-d" }   // device 可缺（旧固件），见下
{ "type": "mode_report", "device": "dot2-d",
  "duplex": { "capability": "half", "requested": "half", "negotiated": "half", "effective": "half" },
  "aec": { "owner": "core", "reference": "available" } }
```

- 音量语义核心侧用统一单位（线性 0..1 起步），设备刻度映射归网关。
- **R2 维护（2026-11-13）**：`volume_receipt` 增加可选 `device`（固件自报）。有它时回执自归属——
  两台设备各按音量键，各自快照分别更新；缺省时核心仍按 `volume_pending` 归属，旧帧行为不变。
  未知的 device 名被忽略（不凭空建档）。
- `mode_report` 把能力/请求/协商/生效分开（对应全双工设计要求）；AEC 所有者唯一。
  **v0.14**：`aec.owner` 是设备事实——`"device"`（dot2-d 的 AEC 在设备音频线程里，核心只消费）、
  `"none"`（纯文本虚拟终端，没有音频路径）、`"core"`（核心自己消回声；当前无此路径）。
  `aec` 字段现在**可缺省**（旧网关/虚拟终端），核心把空 owner 显示为 `null`（未知），绝不猜成 `core`。

### v0.2 上行音频与设备状态（G3 回填，2026-11-07）

范围：闭环缺口 G1（上行音频）＋G6（设备在线状态）。帧新增四条，全部**加法**（未知类型仍拒收）；两侧仓同步实现，`hello.protocol` 仍为 0。

```jsonc
// gateway → core：上行 PCM 块（dot2-d＝EchoMuse 80ms 块，16k s16le）
{ "type": "audio_chunk", "device": "dot2-d", "ts": ..., "seq": 1001,
  "pcm_b64": "…", "format": "s16le", "sample_rate": 16000, "channels": 1,
  "layout": "mono" }          // "mic_ref"＝2ch 交错 [mic|ref]，ref 供核心 AEC 用
// gateway → core：mic 会话达成状态（mic_session 的应答＋设备侧诚实变迁）
{ "type": "mic_report", "device": "dot2-d", "active": true,
  "layout": "mono", "sample_rate": 16000, "channels": 1, "detail": null }
// gateway → core：网关连接内的设备在线状态（南向设备掉线不拆网关连接）
{ "type": "device_status", "device": "dot2-d", "online": true,
  "reason": "registered",     // registered|control_lost|data_lost|…
  "descriptor": { …HelloDevice，首次引入设备时必带… } }
// core → gateway：mic 会话控制（v0.2 只有 continuous）
{ "type": "mic_session", "device": "dot2-d", "action": "start", "mode": "continuous" }
```

定稿语义：

1. **`audio_chunk`**：`seq` 与该设备其他事件共用单调序列（核心按设备做新鲜度检查，乱序/回退只记账不动作，seq 跳号记 gap）；每设备单活动流（无会话 id）；无逐块确认；不进诊断 ring（稳态 ~12.5 帧/s，逐条记录会冲掉 ring——只记异常与 gap）。
2. **`mic_session`**：核心在 audio_in 设备上线时发 start（半双工门控不靠它——见 4）；网关必须以 `mic_report` 回应**达成的事实**（设备侧 mic 起不来＝`active:false`＋detail，不得谎报）。
3. **`device_status`**：南向设备状态变化的事实帧。首次引入带 `descriptor`（能力按厂商注册表如实映射，不伪造）；核心 upsert 设备槽位 online 标志并发布事件，**不**拆网关↔核心连接（网关连接代语义不变——这纠正 v0 只有 hello/断线两态、设备 60s 重拨周期核心全盲的缺口）。
4. **半双工门控在核心**（映射草案 #17）：传输层天然全双工时，"播放中不收"由核心 mic 处理器丢块实现（v0.2 无 AEC，诚实门控优于假装抗回声）；全双工协商生效后同一管线放开。核心侧唤醒＝VAD 分段＋批量 ASR＋拼音匹配（音节级、不限声调；≤2 音节唤醒词不容错——"告诉"距"高达"仅一次替换，必须不命中）。设备侧 oww 由网关以保守 config 关闭（缺口 G2 首版策略）。

### Luna additive northbound diagnostics schema 1 (2026-11-17)

Core owns this optional `DeviceStatus.northbound` contract; Gateway mirrors it without importing Core types. Actual negotiated protocol/minimum remains **0/0**. This revision grants no input provenance, applied-profile, owner or lease capability. Canonical fixture: [northbound-status-v1.json](fixtures/northbound-status-v1.json), SHA-256 `820efbe8bb6e6183a4d8d0584db47a359226d2edf378c7b5d98df6ebc6522ca2`. Both repositories preserve identical fixture bytes, including final LF.

`northbound` is omitted by legacy senders. Unknown schema or malformed diagnostic content must not prevent decoding the enclosing compatible status frame. Core retains only validated schema1 facts or an unavailable state; it does not retain raw unknown content. Every schema1 field below is required, including explicit `last_fault:null` when no fault is known. Unknown schema1 fields are invalid rather than implicitly supported.

| Fields | JSON type and semantics |
|---|---|
| `schema` | Integer `1`; independent of negotiated wire version. |
| `boot_id` | 1–64 ASCII letters/digits/underscore/hyphen, unique process boot instance; diagnostic identity only. |
| `link_generation`, `sampled_uptime_ms` | Unsigned64 integers: local connection attempt identity and monotonic process uptime at sampling. Never compare this generation to Core's independent connection generation. |
| `critical_current_count`, `critical_high_water_count`, `critical_current_bytes`, `critical_high_water_bytes` | Saturating unsigned64 integers. Count includes reserved encoder slots plus encoded queued and in-flight critical envelopes, bounded to64; bytes count retained encoded queued and in-flight envelopes including delimiters, independently bounded to262144. Concurrent reservation starts do not define FIFO: FIFO is completed encoded queue admission, and admission success is returned only after enqueue. These counters do not represent total RSS. |
| `health_coalesced`, `presence_coalesced`, `critical_full`, `invalid_frames`, `audio_dropped` | Saturating unsigned64 process counters; replacement/loss/fatal facts, not delivery receipts. |
| `audio_current_count`, `audio_high_water_count` | Unsigned64 counts for the existing25-block queued audio lane; a writer's separate in-flight allowance is documented in the transport memory report. |
| `writer_timeouts`, `writer_errors`, `transport_disconnects`, `successful_write_bytes` | Saturating unsigned64 process counters. A successful socket write is not proof of Core consumption. |
| `oldest_pending_ms`, `oldest_pending_high_water_ms` | Unsigned64 monotonic queued age/current and process high-water, measured at sampling. |
| `last_fault` | Null or `{code,uptime_ms}`; `uptime_ms` is unsigned64 boot-relative time no greater than sampled uptime. The first terminal reason for a connection wins; a later connection may update this process-level last fault. |

Fixed `last_fault.code` values are `critical_count_full`, `critical_bytes_full`, `invalid_source`, `oversized_frame`, `encode_failed`, `invalid_frame`, `writer_timeout`, `writer_error`, `writer_panic`, `reader_error`, `peer_closed`, `decode_error`, `protocol_mismatch`. No arbitrary error string, PCM, transcript or secret enters this record.

Gateway attaches a fresh snapshot to post-handshake/ordinary DeviceStatus only; no periodic critical diagnostics are added. Core accepts it only from the current connection and matching registered device/descriptor. `/api/v0/diagnostics` and gateway snapshots expose each device's `northbound:{state,schema,sample,received_ms,connection_generation,stale}`. States are `available`, `absent`, `unsupported` or `invalid`; unavailable samples are null. Receive time is Core wall time, and `connection_generation` is Core's source connection, distinct from sample.link_generation. A sample remains explicitly **last reported**, not live queue telemetry. Disconnect/offline/new connection generation marks retained evidence stale; fresh Hello alone cannot refresh it. A legacy ordinary status without diagnostics replaces it with absent. The UI also marks retained data stale after a management refresh error.

Music/filler disconnect interpretation in this slice only confirms a received successful waiter result. Closed receiver/send failure/cancellation/timeout cannot prove a flush. No-live-play is a local no-op without a device-confirmed fact. Existing `Receiver<()>` terminal-receipt stage limitations remain #118; this schema does not close typed-stop, owner/lease or field acceptance.

The same Core gateway management snapshot exposes counts from its existing collections: `pending_plays`, `active_plays`, `pending_stop_sent`, `pending_stop_plays`, `pending_stop_waiters`, `pending_wifi`, `pending_volume`. Waiter counts are retained entries, including senders whose receiver has timed out until terminal cleanup; they are not delivered confirmations. `transport_tasks:{connection_tasks,reader_futures,writer_tasks}` are O(1) RAII lifetime gauges for connection handlers, inline reader futures and spawned writer tasks. Reader futures are explicitly not independent Tokio tasks. They count actual owned lifetimes, including a writer until it has ended/been joined, and do not describe every Core task or OS thread. Supersession actively cancels the old connection even if its writer is blocked and its reader remains open. Core aborts and joins its own blocked writer on reader termination or supersession before publishing teardown; retiring cleanup removes only that generation’s play/stop entries and cannot clear fresh stop/volume/WiFi command state; dropping a handle alone would detach it. These management observations add no wire capability or second state store.

### v0.2.1 助手呈现状态（G4 补篇，2026-11-08）

范围：映射缺口 G8（LED）。**加法一条**，`hello.protocol` 仍为 0；两侧仓已同步实现（主仓 `gateway.rs` ↔ 网关仓 `protocol.rs`/`emuse`）。

```jsonc
// core → gateway：助手交互呈现状态（语义帧，设备呈现归网关）
{ "type": "assistant_state", "device": "dot2-d", "state": "listening" }
```

定稿语义：

1. **`state` 枚举**：`idle|listening|wake|thinking|speaking|music|muted|error`。语义归核心（唤醒/参与窗/轮次只有核心知道），**呈现归网关**（dot2-d：12 灯环调色板、保留绿 `#00A050` 仅 listening 可达、未知状态变暗不变旧、`listening:true` 标志仅随 listening 态）。
2. **推送时机**：状态变迁即推＋每秒对账 ticker（退休超时类状态：唤醒闪 1.5s、错误闪 3s、参与窗 30s 关闭）；设备上线/网关重连强制重推（force）。发送失败不阻塞交互（呈现永不反压语义），失败时标记未达以便下次重推。
3. **优先级**（核心侧计算）：muted > error 闪 > speaking > music > thinking > wake 闪 > listening > idle。hub 播放注册表里的语音道播放（诊断探针/未来主动发言）同样计入 speaking——不经 voice 管线提交的播放不得哑灯。
4. **seq 纪元修正**（随 G5-1 实机发现）：设备经 `device_status(descriptor)` 在**新连接代**再引入时，核心重置该设备的 seq 水位——适配器进程重启后 seq 从 0 起，旧水位会把整条新流丢成 `audio_chunk_stale`。旧代迟到帧仍由连接代检查拦截。

### v0.2.2 主机时钟投影（G6 补篇，2026-11-08）

范围：被替换能力 #2（emOS 无时间服务，设备时钟停在 2010；C 阶段 TLS 的硬前置）。**加法一条**，`hello.protocol` 仍为 0；两侧仓已同步实现（主仓 `gateway.rs` ↔ 网关仓 `protocol.rs`/固件）。

```jsonc
// core → gateway：hello_ack 之后立即下发的主机时钟投影
{ "type": "time_set", "ts_ms": 1799000000000 }
```

定稿语义：

1. **投影不是协商**：fire-and-forget、无应答帧。固件以 root `settimeofday` 应用并记日志（失败仅记日志、绝不断链）；共存适配器/虚拟终端忽略（Android 有时间服务/无时钟语义）。
2. **下发时机**：每次连接建立（`hello_ack` 后一条）。设备重启/固件换槽即重新对时；连接保持期内不重发（24h 漂移 ~2s，TLS 容忍）。
3. `ts_ms`＝核心主机墙钟 Unix 毫秒；核心自身时钟错误会如实传导（未来 NTP 归 C 阶段设备侧兜底）。

### v0.2.3 在线换网（C 阶段补篇，2026-11-08）

范围：被替换能力 #1-②（在线换网主径；向导 WiFi 步的产品出口）。`hello.protocol` 仍为 0；同 ship 门禁＝固件版本标记 `hm-firmware 0.0.2+` 且 descriptor 带能力 `wifi`（旧固件收到该帧会忽略/hello 严格校验不变；核心在 API 层拒绝向无能力固件发起）。

```jsonc
// core → gateway：在线换网（管理 API POST /api/v0/devices/{id}/wifi）
{ "type": "wifi_config", "ssid": "NewHome", "psk": "********" }
// gateway → core：看门狗结论（一次变更恰有一条）
{ "type": "wifi_state", "device": "dot2-d", "ts": 1794100000000, "ssid": "NewHome", "state": "applied", "detail": "joined" }
```

定稿语义：

1. **看门狗保证不失联**：固件备份当前 wpa 配置→应用新网→`wpa_cli reconfigure`→20s 内未关联即恢复备份并重连旧网，`state` 如实报 `applied|reverted`。核心上音箱后北向链路是回环，换网不断控制链——回执必达（实测假 SSID 场景全链回滚、核心全程在线）。
2. **换网期间 HTTP 应答可能丢失**（发起方自己就在旧网络上）：向导/API 按"应答丢失≠失败"处理，以 `wifi_state` 事件（voice log 可见）与 descriptor 的当前 SSID 为准。
3. descriptor 扩展：`HelloDevice.wifi_ssid`（已关联 SSID，未关联/无射频为缺省）；能力表新增 `wifi`（仅直占形态固件）。
4. 共存适配器/虚拟终端如实忽略（无射频/Android 栈归属）。

### v0.2.7 交互矩阵（R1g，2026-11-12；真机通过）

半双工＝唤醒词是唯一命令入口。本补篇只加**字段**（`hello.protocol` 仍为 0，两仓镜像枚举）：
部署顺序＝固件先（旧核心忽略未知字段、`#[serde(default)]` 兜住旧固件）。

- `keyword` 增加 `self_heard: bool`（默认 false）。设备的回声守卫（播放中 mic 与 DAC 参考相关
  ≥ 阈值）**不再吞掉命中**，只把这个判断作为提示随事实送北；是否动作由核心决定——
  核心看自己**正在读的文本**是否含唤醒词：含→`keyword_ignored reason=self_heard_phrase_in_our_own_answer`，
  不含→照矩阵执行（人声盖过播放）。
  为什么：半双工下 near-end 本地停播已关闭，KWS 是唯一的下沉停播路径；守卫吞掉命中＝用户无法打断
  自己的故事（真机 2026-11-12）。
- `kws_report` 增加 `guarded`（v0.2.5 只写进了控制台 JSON，帧里没有 → 核心恒 0）与
  `epoch`（本进程启动 ms；网关的终身计数跨核心重启存活，只有同一 epoch 内可比）。
- `barge_in` 的语义收紧（字段不变）：半双工下核心对它**只记账不动作**（`direct_barge_suppressed_half_duplex`），
  固件也不得本地停播；全双工才恢复本地停播与接受判定。

### v0.2.5 语义打断（D/T5 补篇，2026-11-09；**协议已实现，功能真机未通过**）

> **状态警告**：本补篇的帧与字段已在两仓实现、单测全绿并随 fw 0.0.17 / core_b 上机，
> 但 **T5 真机验收失败**（自激循环，用户真人「一直卡」）。用户 2026-11-09 夜指示
> 「本回合不处理」。**不得把本补篇当作功能验收记录**；失败详录＝网关仓
> `docs/d/t5-semantic-bargein-20261109.md`。`hello.protocol` 仍为 0（加帧不加版，
> 新帧只在 gateway→core 方向，部署顺序＝固件先；两仓镜像枚举，无共享代码）。

新增 gateway→core 帧（设备只报**事实**，停不停由核心规则决定）：

```jsonc
{ "type": "barge_in", "device": "dot2-d", "ts": 1794235595905, "seq": 42,
  "pcm_b64": "<400ms/16k/mono s16le onset pre-roll, base64>",
  "sample_rate": 16000, "channels": 1,
  "corr": 0.214, "energy_db": -43.7 }
```

- `corr`＝近端 mic 与 DAC 参考在 ±40ms 滞后搜索下的最佳归一化互相关；
  设备侧守卫要求 `corr < corr_max`（当前默认 0.50，`--barge-in-corr` 可调），
  回声相关窗口不出帧。`energy_db` 为 onset 切片 RMS（dBFS）。
- `pcm_b64` 是**前滚**，不是当前块：核心在播放期间一直丢弃上行，靠它把首个音节
  送回 VAD（400ms = 6400 samples @16k）。`channels` 必须为 1；base64 非法或长度
  非偶时核心记 `barge_in_bad_b64` / `barge_in_bad_payload` 并丢弃，不动作。
- 每个播放 episode 最多一帧（设备侧锁存，voice 通道转静后复位）。
- `kws_report` 增加 `guarded`（默认 0）：设备回声守卫抑制掉的自听命中数，
  供控制台显示「自听拦截」。旧核心忽略未知字段。

管理 API：`GET /api/v0/mode` 顶层 `barge_in`（全局开关）；每台设备
`barge{stops,ignored,last_barge_in_ms,preroll_held,window_open}`；
`PUT /api/v0/mode/{device}` 接受 `barge_in: bool`，进配置历史/恢复链并记 `mode_set`。
核心如实忽略理由：`disabled` / `muted` / `no_voice_playback`（R1g 后：半双工下改为
`direct_barge_suppressed_half_duplex`（有播放）/ `direct_barge_ignored_idle`（无播放），
media 不再单独排除——见 v0.2.7 补篇）。

**真机暴露的语义缺口（下轮补，不在本补篇内解决）**：帧里的「低相关」是
「有人在说话」的**必要非充分**条件——AEC 残余、TTS 起始瞬态同样低相关；
核心在 `barge_in_window` 内豁免半双工门控并清尾盲窗，会把设备自己的应答
放行给检测器造成自激。协议层需要额外的播放状态/保护期语义，或核心单方面
在开窗期间屏蔽新播放，不能只靠固件阈值。

### v0.3 流式播放（D 阶段 T3 补篇，2026-11-10）

范围：全双工关键路径的第一块——**边合成边下发、边收边播、随时可掐**（full-duplex-plan T3）。现状＝整段 WAV base64 内联单帧 `play`（长答案首声＝整段合成完才开始播）。补篇新增三个核心→网关帧，复用既有回执；`hello.protocol` 仍为 0（**加帧不加版**，同 time_set 先例：新增帧只在 core→gateway 方向、部署顺序＝固件先）。

**能力门禁**：descriptor 能力表新增 `play_stream`（固件实现即声明，诚实规则）。核心**必须**先查能力再流式；无能力设备回退整段 `play`（老路径永在）。

```jsonc
// 1) 声明一条流式播放（不含音频数据）
{ "type": "play_begin", "task_id": "T31", "device": "dot2-d",
  "content_ref": "tts:stream-abc", "channel": "voice",
  "audio": { "format": "pcm_s16le", "sample_rate": 24000, "channels": 1 },
  "text": "答案首句…", "barge_in": "stop_current" }
// 2) 增量载荷（raw PCM，无 WAV 头；seq 从 0 起、每块 +1；建议 100–400ms/块）
{ "type": "play_chunk", "task_id": "T31", "seq": 0, "audio_b64": "…" }
// 3) 结束（最后一块已随 chunk 下发；此后网关按既有 EOS 语义收尾）
{ "type": "play_end", "task_id": "T31" }
```

定稿语义：

1. **载荷**：`play_chunk.audio_b64`＝`audio` 元数据声明的裸 PCM（S16LE 单声道起步）；每 task 的 `seq` 严格递增，**缺号/重号＝协议违规＝断连**（与 time_set 同等严格，乱序即毒流）。单帧上限沿用既有 MAX_FRAME。
2. **回执链扩展**：`play_receipt.stage` 新增 `streaming`（首块被播放线程消费）——`accepted`(begin 校验过) → `streaming` → `device_confirmed`(末块写穿硬件且 drain，语义与整段播放一致)。`play_finished.played_ms` 如实累计实际播出时长；流中途 stop 的已播部分照报。
3. **可掐＝既有语义**：`stop {task_id, flush}` 对流式播放同样生效（pending 清空、flush、`stop_receipt`）；唤醒打断两条通道同停。核心在 LLM 仍在生成时即可 begin+前几块，用户打断即 stop——「此块不做，T1/T2 白做」的落点。
4. **有界缓冲**：固件对每条流的 pending 上限＝**60s 超前**（播放位置起算；**2026-11-10 修订：原 10s**——火山合成 ~2× 实时，「讲个故事」类长答案在 ~12s 处必然撞顶，被句中硬切＝无淡变 click（取证 run3：−13dBFS→数字零 <5ms）；60s @48k mono i16 峰值 ≈5.8MB pending，482MB 设备余量充分。**配速已实现（2026-11-10 晚）**：60s 仍不够真长故事（用户实测复现）→核心侧发送配速上线——TtsPiece 本地队列＋ahead 预算 30s（`TTS_AHEAD_LIMIT_S`），ahead 数学上恒 <60s 上限，任意长度答案不再可能 overflow；60s 上限保留为内存后盾）；超限＝该 task `play_receipt.stage=failed, detail="stream_overflow"` ＋流拆除，不断连。
5. **重采样连续性归固件**：声明采样率→48k 的重采样由固件承担，**跨块携带上一采样点**（线性插值状态）；合同测试钉死「同一斜坡素材，流式播放与整段播放样本级等价」。
6. **混音/通道不变**：voice 与 media 并发混音、灯语、音量弧对流式播放一视同仁（同一播放线程同一通道抽象）。
7. **两端合同测试义务**：①样本等价（流 vs 整段）；②中途 stop＝零残留＋回执链完整；③seq 缺号＝断连；④overflow＝failed 不断连；⑤能力门禁＝无 `play_stream` 的设备收到 begin（核心侧拒发＋网关侧未知帧如实违规断连）。

**核心侧配套（实现批，不入设备合同）**：`TtsClient` 增流式变体（火山 NDJSON 逐行转发而非整包缓冲）；LLM 首句切分（首块尽快下发）。

### v0.4 反射词表部署（R1j/P1，2026-11-13；真机通过）

范围：设备端关键词词表从「手写 `kw.txt`」变成「核心按配置编译、网关校验安装」。
一条新帧 + 一条回执；`hello.protocol` 仍为 0（加帧不加版，部署顺序＝固件先）。

```jsonc
// core → gateway: 整份关键字文件（UTF-8，一行一个词：`<token>… @<词>`）
{ "type": "kws_deploy", "device": "dot2-d",
  "keywords": "n ǐ h ǎo g āo d á @你好高达\nz àn t íng b ō f àng @暂停播放\n" }
// gateway → core: 设备事实（不是「帧发出去了」）
{ "type": "kws_deploy_receipt", "device": "dot2-d", "accepted": true,
  "hash": "d76ce9d4d7900a2c", "keywords": 3,
  "detail": "3 keywords installed at /data/local/tmp/hm-kws/kw.txt; spotter restarting" }
```

定稿语义：

1. **校验在设备侧**：只有设备有模型的 `model/tokens.txt`。网关逐行解析，**任一 token
   不在表里＝整份拒收**并指名（`unknown token 'x' in '词'`）——2026-11-12 实测：一个
   坏 token 会让 sherpa `Encode keywords failed` 而 driver 仍报 ready，整份词表静默失效。
2. **安装是原子的**：备份上一份到 `kw.txt.bak` → 写 `kw.txt.tmp` → `rename`。拒收时
   旧词表原样保留、备份不写。
3. **重启是有意的**：写完后 `SIGTERM` 当前旁路进程，监督器立即重拉（无退避）；
   这次重启**不计入** `kws_report.restarts`（RJ-4 的「0 重启」是失败预算）。
4. **hash 对齐，且分三段**（v0.14）：FNV-1a 64 hex，两端同一实现。
   - `sent_hash`＝核心编译出的那份；回执 `hash`＝设备**安装**的那份；两者相等才算"装机一致"。
   - **加载证明**：spotter 进程启动时网关对 `kw.txt` 取 hash，只有 driver 的 `ready` 行到达后才把
     它写进 `kws_report.loaded_hash`（带 `loaded_ms`）；进程退出即清空。`/mode` 的
     `reflex.deploy.loaded_matches` 只有在加载 hash 与安装 hash 相等时为 true，
     `state` 也只在三者一致时才是 `loaded`。没加载过的词表不许说"已生效"。
5. **词表是配置**：`reflex` 块（词 + token 拼写 + 意图 + 开关）经 `PUT /api/v0/reflex`
   保存、`POST /api/v0/reflex/deploy` 部署；`/mode` 每设备回报 `reflex.words[*].hits/
   ignored` 与 `reflex.deploy{state,hash,keywords,detail}`。
6. **策略只在一处**：`interaction::decide` 的 `Trigger::Keyword(KeywordIntent)` 逐词给
   「停/开窗/执行意图/不动作」；文本层（`retreat_match`/`pause_match`/`continue_match`）
   保留为 ASR 兜底，KWS 已执行的意图在 ~1.5 s 内不再由转写重复执行（`intent_deduped`）。
7. **自听**：`退下` 类的 `self_heard=true` 一律拒（无「我方文本不含」豁免）；其它词沿用
   R1g 提示语义。

### v0.5 KWS 本地退出（交互合同 v2 #5，2026-11-16；主机合同测试通过，真机待验收）

范围：**退出词不再往返核心才停音频**。半双工下 KWS 是唯一的打断路径，核心忙/队列满时
往返就是几百毫秒到几秒的延迟；现在设备自己先静音，再把两种事实报北。**会话状态的唯一权威
仍是核心的 `close_session`**——本地 flush 不是第二套状态机，也不补应答。

```jsonc
// core → gateway: v0.4 的帧多两个字段（旧网关忽略）
{ "type": "kws_deploy", "device": "dot2-d",
  "keywords": "n ǐ h ǎo g āo d á @你好高达\nt íng zh ǐ b ō f àng @停止播放\n",
  "retreat_words": ["停止播放"],     // 意图为 retreat 的词（只含本份词表里有的）
  "intents_hash": "9f2c…" }          // 词→意图映射的 FNV-1a（"词\t意图\n" 逐行）

// gateway → core: 回执多两个字段
{ "type": "kws_deploy_receipt", "device": "dot2-d", "accepted": true,
  "hash": "…", "keywords": 5, "local_exit": 1, "intents_hash": "9f2c…",
  "detail": "5 keywords installed at …; 1 local exit word(s)" }

// gateway → core: 设备自己已经静音（事实，不是决定）
{ "type": "local_exit_flushed", "device": "dot2-d", "keyword": "停止播放",
  "ts": 1794712345678, "latency_ms": 84, "channels": ["voice", "music"],
  "detail": null }                   // detail 非空＝某声道 flush 失败，音频可能仍在响
```

定稿语义：

1. **词表带意图下楼**：网关把 `retreat_words` 写进 `kw.exit.json`（与 `kw.txt` 同目录，
   部署时原子替换），旁路进程按它判断"这个词是否退出"。词表文件仍只有 `<token> @<词>`，
   driver 不解析意图；**拒收的部署不动退出集合**，没有退出词时删掉旧集合（残留的旧词
   不许再砍音频）。`local_exit < retreat_words_sent` 说明有词没进词表（回执 `detail` 指名）。
2. **两个否决**：只有「意图是 retreat」且「回声守卫没判为自听」的命中才本地执行。唤醒词
   永不本地执行；我方回答里说出「停止播放」被守卫判为自听时只上报、不自杀。
   **范围措辞（v0.8 修正）**：退出词是 **KWS 的唯一本地音频动作**（物理静音、音量、断线
   清场本来就是本地行为，不经 spotter）。播放中退出词被自听守卫拦住、核心兜底对
   Legacy 输入无效的缺口，已挂账另案（见 v0.8 节 non-goals）。
3. **一次一层、幂等**：一个退出片段（episode）同时只允许一个；重复命中不产生第二次 flush
   或第二条事实。**帧序（v0.8 措辞修正）**：命中即排队 flush，`keyword` 与
   `local_exit_flushed` 按各自事实就绪先后上报（合同测试接受任意先到）——不是"先静音后
   上报"。`latency_ms` 是设备自己的命中→静音时间（在**最后一个声道的硬件 flush 完成**
   时刻测得，慢链路无法放大它）；≤300 ms 的判据口径是**命中→flush 完成**，合同测试用桩
   sidecar＋假硬件、只盖 voice 通道，双通道与真人声学验收待用户安排（如实，不写通过）。
4. **槽位归属**：本地砍掉的是命中时该声道正持有的 task；flush 完成即清槽，核心随后为同一
   task 发来的 `stop` 立刻得到 `device_confirmed`（`played_ms: null`），不会被重新 flush
   成一次假的 0 ms 播放。
5. **核心仍然关会话**：`keyword` 事实照旧上送，`close_session` 清额度/票据/媒体并播结束语；
   因为事实与命令分开，重复或迟到的 `local_exit_flushed` 永远不会产生第二声应答。
6. **`/mode` 可查，且以设备事实为准**：`kws.local_exit` 是设备自己报的「当前武装了几个
   退出词」（`kws_report.local_exit`，**跨核心重启存活**——核心的 `reflex.deploy` 是内存记录，
   重启就没了，只看它会误报「未装载」）；`reflex.deploy.local_exit(_matches)` 只描述**最近一次
   部署**；`intents_hash`/`intents_hash_installed` 给映射版本，`local_exits` 与
   `last_local_exit{keyword,ts,latency_ms,channels}` 记事实。控制台命令词表卡按
   **`kws.local_exit`** 显示「本地退出：n 词」。
7. **激活时补装含义**：`hm-release` 激活前若发现「spotter running 且设备 `kws.local_exit=0`，
   而配置里确有退出词」，补推一次 `POST /reflex/deploy`（代价是一次约 7 s 的识别进程重启）；
   设备已武装就绝不打扰（否则**每次激活都会多一次重启**，因为核心的内存部署记录每次重启都清零）。

### v0.7 仲裁修正：最响层的候选才用时间（2026-11-16，桌面实测）

2026-11-16 桌面实测（Mac 播放唤醒词，b/d 同桌面、e 在客厅）暴露旧口径的漏洞：
e（客厅，−39.5 dBFS，**最早** 91 ms）拿走了整句话，而 d（儿童房，−30.5，晚 753 ms）比它响 9 dB、
b（书房，−32.8）比它响 6.7 dB。原因＝"最响的两个差 <3 dB → 无法分 → 用时间"这条只看**最响两个**
（d 与 b 差 2.3 dB），落回时间后按全体最早选，于是**最轻的那台**赢了。

修正（纯函数，一个筛选）：声学分不出高下时，**时间只在"最响层"里排序**——即在最响测量值
`LOUDER_DB` 以内的候选；没有测量值的节点不参与最响层（"不能证明响度"就按原样不靠它赢）。
实测数据已钉成单测 `the_quietest_claim_cannot_win_the_whole_utterance`；同层最早仍是 `why=first`，
完全同刻仍是 `tiebreak`。**规则仍是响度优先**，只是不允许"安静但早"赢得整句。

### v0.6 三条现场修复（2026-11-16，交互合同 v2 的 #2/#3/#4）

范围：桌面实测（两台音箱放在同一张桌子上）暴露的三个问题，都不是阈值问题。

1. **#2 邻居的提示语不是用户的问题**：**每台设备跑自己的核心**，所以"另一台刚说了「来了」"
   不是本机事实，必须过家庭平面。核心在 `play_local_ack_at`/`play_filler` 记下
   `(短语, 设备, 时刻)` 并 `POST /api/v0/fleet/cue {device, phrase, at_ms}` 广播给对端
   （fire-and-forget，1.5 s 预算，失败只丢消息）；接收端 `fleet/cue` 记进同一张表。
   转写到达时若**与另一台设备在 5 s 内说过的 cue 完全相同**（忽略标点与空格）就丢弃并记
   `mic_foreign_cue_echo`。判据刻意窄：自己说的归 `strip_cue_echo`；包含该短语的长句不碰；
   没有邻居刚说话时，人自己说同一个词照常处理（事件 `mic_peer_cue` 记收到的对端 cue）。
   现场依据：dot2-d 把 dot2-b 的唤醒应答「来了」当成用户一句话，起了一轮并答了 1.58 s。
2. **#3 停播类工具不杀自己这一轮**：`music.control stop`（program 目标）原先走
   `stop_program_with_cancel` → `stop_device_locked`，后者 `stop_epoch += 1` 并取消全部请求，
   于是**调用它的那一轮被自己判成"被打断"**：dot2-b 2026-11-16 08:31:10「睡觉吧。」→ 模型关了
   书房灯、停了内容，回答只播 853 ms 就断，最后那句「当前内容已停止。」没说出来。
   现在工具路径用 `stop_program_for_tool(device, keep_task)`：丢弃书签、按名停止**不是本轮的**
   播放、取消节目原本的 task，但**不动 stop epoch、不取消本轮请求**——本轮还能把事实说出来。
   人自己说「停止播放」仍走完整 stop（epoch 递增 + 取消一切），语义不变。
   没有任何东西可停时 `music.control stop` 现在如实失败，而不是回「已停止」。
3. **#4 识别灵敏度可调**：`VoiceSettings.kws_threshold`（0.05–0.90，默认 0.25）与
   `kws_score`（0.5–6.0，默认 3.0），走同一条 `PUT /api/v0/mode/{device}` 校验/历史/恢复/导入链，
   `/mode` 暴露配置值；`kws_deploy` 新增 `threshold`/`score`，网关写 `kw.tuning.json` 并作为
   `KWS_THRESHOLD`/`KWS_SCORE` 传给 run.sh（**部署才生效**，重启识别进程约 7 s）；
   `kws_report`/`kws_deploy_receipt` 回显，`/mode.kws.threshold|score` 是**模型自报的在用值**。
   控制台新增「识别灵敏度（设备端）」卡：只保存配置 / 保存并部署 两个动作，并显示"设备在用"。

**个人服务「故事」接入现状（2026-11-16 实测）**：`http://192.168.1.168:18083` 有 OpenAPI 3.1 文档，
`/v1/search`、`/v1/album/{id}` 是 JSON GET，`/v1/stream/{id}` 是 `audio/mp4`（支持 Range）。
**搜索/列集已可用**：`openapi` 服务块（spec_url + `auth_header: X-Internal-Token` + 同一枚家庭令牌 +
白名单 `search_v1_search_get`/`album_v1_album__album_id__get`）在 dot2-c 上配置成功，
模型真的调用并念出了三条标题（文本 `/chat`，无音频）。**播放还不行**：需要
(a) symphonia 增加 `aac`+`isomp4` 特性（当前只有 mp3/flac/wav/pcm），(b) 一个"故事源"适配器
（搜索映射 + 带头的流 URL + 复用音乐会话的队列/暂停/继续/下一首）——另立一片。

### v0.8 协议机制：握手协商、拒绝帧、特权帧闸门、远程静音、打断能力声明（#8／网关 #11–#14，2026-11-16；两仓合同测试通过，**线上协议版本保持 0**）

**发布切分（硬约束）**：本节全部机制随"发布 A"上线，线上协议值**保持 0**；`protocol` 升 1 是**另一次两仓成对的发布 B**。原因＝激活顺序是"固件先重启，`hm-release` 经**尚未重启的老核心**轮询固件版本，然后才重启核心"——老核心是严格相等判定，同发布升版本会在固件确认阶段必败、每次发版必回滚（Codex P0，`activate.rs` 实证）。此后任何一端升级都不再破坏窗口。

**兼容判定＝区间求交**（两侧同一纯函数）：

```
compatible ⇔ max(两端 min_protocol) ≤ min(两端 protocol)
运行版本   = min(两端 protocol)      // 交集非空时必然 ≥ 两端 min
```

- `hello`/`hello_ack` 各带 `min_protocol`（serde default 0＝"只接受 0"，与旧端严格相等语义一致）与机器可读版本串（`gateway_version`/`core_version`）。`min_protocol` 用 `u32+default` 会丢"缺失 vs 显式 0"的区别——可接受：省略字段的端必然是 A 时代或更早的端。`/status.gateway` 暴露 `gateway_version` 与 `protocol_operating`，并在当前连接断开时清回 `null`。
- 运行版本只用于判定与日志；**帧行为一律由能力 token 与可选字段驱动，不分支在版本号上**——这保证 A（0）与 B（1）窗口互操作。
- 新增字段一律 serde default 可选（老端照常解析）；**新增 variant 不受 default 保护**——解码策略见下。

**拒绝就明说**（否则对面只能看见断线）：

```jsonc
// core → gateway：核心拒绝握手（在断开前、经握手 socket 直发，有界写入）
{ "type": "hello_reject", "protocol": 0, "min_protocol": 0, "core_version": "0.0.1",
  "reason": "protocol_no_intersection: peer accepts [7,7], this side accepts [0,0]" }
// gateway → core：网关/固件拒绝链接（尽力而为——旧核心运行期记日志跳过，不致命）
{ "type": "protocol_error", "protocol": 0, "min_protocol": 0, "gateway_version": "0.0.53",
  "reason": "protocol_no_intersection | capability_missing:<token>" }
```

`reason` 枚举：`protocol_no_intersection` / `capability_missing:<token>` / `duplicate_device_id`（重复 id 并入同一拒绝路径）。**时限常量**：固件 TCP dial 3 s（`CONNECT_TIMEOUT_MS`）、hello_ack 等待 5 s（`HELLO_TIMEOUT_MS`，此前无超时）、判定失败→亮灯→断开预算 3 s（`PROTOCOL_FAIL_DISCONNECT_MS`）、协议错误后重连周期 30 s（`PROTOCOL_ERROR_RETRY_MS`；**粘性灯**——只有完整握手成功才清除，普通断线不覆盖它）。核心侧 hello 等待 10 s（既有）。

**特权帧闸门**（两侧：固件未声明能力 → 协议错误灯＋断开；核心只向声明者发送）：

| 帧 | 能力 token | 违规处置 |
|---|---|---|
| `wifi_config` | `wifi` | 协议错误（灯＋断开） |
| `mute_set`（新） | `remote_mute` | **操作级 409**（不断连接——无此能力的设备不失其它功能） |
| `time_set` | **不设闸，永久无条件收发** | —（token 化会切断旧端时钟投影，TLS 依赖它） |

既有 `play_stream`／采集租约闸门保留不动。能力检查按**当前连接代＋在线状态**的 descriptor，不查陈旧缓存。

**远程静音（#13）**：

```jsonc
// core → gateway：写闩锁＝等效按键（同一闩锁、同一持久化、无超时自动开麦）
{ "type": "mute_set", "task_id": "M-1", "device": "dot2-d", "muted": true }
// gateway → core：回执（事实），echo task_id；按键/开机报告不带 task_id
{ "type": "mute_state", "device": "dot2-d", "ts": …, "seq": …, "muted": true,
  "source": "core_set", "task_id": "M-1" }
```

- **闩锁写入者从此只有两个**：物理键与显式 `mute_set`；其余任何帧（含 `assistant_state`）永不改它。这是对「设备主权态永不被网络覆盖」的产品级修订（#13 所有者批准）：**"任何帧不能动"收窄为"只有这一条显式帧能动"**。
- **命令解析只认关联事实**：核心侧 pending 表按连接代持有；`mute_state` 带匹配 `task_id` 且**同一连接代**才结算命令——按键、开机报告、跨代迟到事实都不结算；超时如实回 `unknown`（发出 ≠ 生效）。若设备回传的 `device` 或 `muted` 与请求不一致，返回 `applied: false, stage: "mismatch"`。命令**不跨重连存活**（不盲重放解除命令）。
- `MuteState.source` 取值：`"button"`（固件物理静音键）、`"hardware"`（虚拟终端物理键等价源）、`"device_boot"`（设备启动上报）、`"core_set"`（核心下发 `mute_set` 后设备回执）。
- 持久化：tmp+rename 原子写；写盘失败**闩锁照常生效**（隐私优先）＋日志如实报错；坏文件启动默认未静音＝现状保留。上行闸门：静音时丢弃在途块，发送前复检（静音不晚于当前块边界）。
- 管理 API：`PUT /api/v0/devices/{id}/mute`（请求 `{ "muted": bool }`；成功回执 `{ "applied": true, "stage": "device_confirmed", "task_id": "...", "device": "...", "muted": bool, "source": "..." }`；未收到回执回 `{ "applied": false, "stage": "unknown", ... }`，设备回执与请求不一致回 `{ "applied": false, "stage": "mismatch", ... }`；无 `remote_mute` 能力或设备离线返回 `409`；鉴权＋变更记录；`/mode.devices[]` 增加 `mute_source`，控制台设备卡显示来源徽标）。

**打断能力声明（#14）**：descriptor 新增 `local_exit`、`interrupt_wake_word`、`interrupt_retreat_local_exit`——**本版打断只认唤醒词与退出词两条路**（半双工矩阵不变：half＋近端语音永不执行）。`retreat_words` 字段照发不闸（serde default，老固件无害）。播放中退出词的 `self_heard` 缺口＝**挂账另案**：核心兜底 `retreat_is_own_overlap` 要求 `Leased+Audio+overlap`，真实固件 Keyword 无 origin（Legacy）恒 false；且 fleet/cue 过滤不在 Keyword 路径、仲裁只管 Wake——另案需定义设备侧执行前证据与跨机作用域，做独立音频验证（D8 自播负样本）。

**机器健康灯（#12，网关本机事实，不进协议）**：`protocol_error`（粘性）/`core_offline`/`wifi_down`（三态探测：未知不谎报断开；连续 2 次未关联才点亮）/`reboot`（Starting 态，首次判定后不再进入）。优先级：`muted > protocol_error > 音量弧 > core_offline > wifi_down > reboot > 语义态`（`error` 在语义态首位的旧表不变）。**灯的全部绘制收敛到唯一入口**（`leds::ring_frame` 纯函数）——弧、动画 tick、重连重绘此前各自直写，任何阶梯都无法成立（审计结论）。核心收到 `protocol_error` 记 `link_protocol_error` 事件并经 `/status.gateway.last_link_error` 暴露，下一次成功握手清除。

**解码策略（各阶段、各端，如实）**：握手期失败＝断开（两侧 fail-closed）；运行期未知帧＝核心/真实固件**记日志跳过**（现状），虚拟终端**退出**（现状，保留）；拒绝帧对旧端＝尽力发送，EOF 降级可接受。`ProtocolError`/`MuteSet` 等新 variant 对旧核心运行期是"被跳过"——`remote_mute` 闸门保证的是**正确性**（核心不把静默忽略当成功），不是连接安全。

**Non-goals（如实）**：版本机制不提供防重放/防降级（回环信任域）；回滚还原 core/audio/web/model 指针，但**持久状态不回滚**（静音闩锁、KWS 文件、DB 写入保留——静音闩锁跨回滚存留＝与物理键同语义）；双语与全双工不在本批。

## 二、管理 API v0（控制台/语音管理 ↔ 核心）

HTTP＋JSON（SSE 用于对话流）；绑定 `127.0.0.1`，端口默认 `8930`（可配置，避开占用）。本地单用户起步：首轮用启动时生成的 token（开发期打印一次/写本地文件），授权模型后续扩展。

### 服务配置（ASR/TTS/LLM 各自独立）

```jsonc
GET  /api/v0/config/services        // 读：provider、endpoint、model、key_ref（仅名字，绝不回显值）、生效状态
PUT  /api/v0/config/services/llm    // 写：校验→保存→应用；返回 { state: "saved|pending_apply|applied|failed", reason? }
POST /api/v0/config/services/llm/test   // 连接验证（真实调用一次最小请求），返回耗时与错误详情
```

- 三项服务独立配置/验证/生效；替换其中一项不影响另外两项（验收场景）。
- 变更记录：`GET /api/v0/config/history`，含时间、来源（页面/语音/系统/恢复）、摘要与**可恢复快照 payload**（M3）。payload 只含配置值与「密钥是否已设置」，**永不包含密钥值**。模式变更（PUT /mode）同样入史（scope `voice`，payload 为完整设置快照）。

### 服务块：音乐与天气（R1e/R1h 定稿）

```jsonc
GET  /api/v0/config/services/music          // enabled/prefer_local/library/online + secret_set + revision
PUT  /api/v0/config/services/music          // 校验→保存→入史（scope music）；带 revision，过期 409
POST /api/v0/config/services/music/test     // 真探针：每个已配置来源各发一次真实请求

GET  /api/v0/config/services/weather        // enabled/provider/endpoint/places/default_alias/max_days/state/revision
PUT  /api/v0/config/services/weather        // 同上 + api_key（只写，""＝清除）；校验→保存→入史（scope weather）
POST /api/v0/config/services/weather/test   // 真探针：向默认地点发一次真实请求，返回 location/温度/现象/延迟/状态词
GET  /api/v0/config/services/weather/geocode?q=杭州   // 控制台"搜索城市"：Open-Meteo geocoding，返回 ≤5 个候选（name/admin/country/经纬度/时区）
```

- **位置存坐标**：地点在配置期解析为经纬度，工具调用**不再 geocode**（语音里的城市名不能触发任意外部查询）。别名唯一，默认地点用于"不说地点时"。
- **状态词只有一个来源**：`unconfigured`（没有地点）/`disabled`（配了但关着）/`ready`。提示词、控制台、探针用同一套词。
- 天气密钥不进 `GET`；`secrets=1` 的导出包含它，导入经同一校验路径。
- 二者都没有"mock 播放/mock 天气"：未配置＝如实报 `music_unconfigured`/`weather_unconfigured`，0 次外部请求。

### 家庭自动化服务块（R1j/P0-b → R2-BM5 定稿）

```jsonc
GET  /api/v0/config/services/home    // backend/availability/state/detail/tools/claim
                                     // + base_url/token_set/entities[{name,entity}]/timeout_ms/retries
PUT  /api/v0/config/services/home    // { backend?, base_url?, token?, entities?, timeout_ms?, retries? }
                                     // 校验→保存→入史（scope home）；未知 backend/坏地址/重复实体＝400
POST /api/v0/config/services/home/test   // 真探针：GET /api/ ＋ 读白名单第一个实体
```

- **没有后端就没有确认**：`backend=none`（默认）时服务是**已注册但 unconfigured**——提示词把它列进「已注册但当前不能用」，`home.light` 在任何调用上返回命名失败 `HOME_UNCONFIGURED`（零副作用、不回 `DeviceConfirmed`、不接受模型自造的实体名）。
- **配置不完整也是 unconfigured**：`home_assistant` 已选但缺地址/令牌/白名单时，`detail` 指名缺什么；**未配置＝0 次 HTTP**（工具与探针都在发请求前返回）。
- **实体白名单**：`entities[{name,entity}]` 是模型唯一的词汇——模型说 `name`，核心映射到 `entity`（`light.*`）；自造名字回 `HOME_ENTITY_NOT_ALLOWED`，零请求。
- **写后回读**：`POST /api/services/light/turn_on|off` 的 200 只算 accepted；随后 `GET /api/states/{entity}` 与目标一致才是 `DeviceConfirmed`。读回不一致＝`HOME_READBACK_MISMATCH`（`Unknown`），超时＝`HOME_TIMEOUT`（`Unknown`），鉴权＝`HOME_AUTH`（`Failed`）。**`service_accepted ≠ confirmed`**。
- **令牌只写**：`home.token` 存 secrets 表；GET/PUT/`/mode` 与**普通**导出只回 `token_set`，值不出现；只有显式
  `GET /config/export?secrets=1` 的本地备份包含值（与其他 secret 同一规则，R1f 的「secret 显式才含」）。
  `PUT` 里 `token` 缺省＝保持、`""`＝清除。
- `GET /mode` 顶层回 `home{backend,availability,state,detail,tools,claim,base_url,token_set,entities,timeout_ms,retries}`；提示词与卡片用同一个状态词。
- **答案事实由核心渲染**（BM5-A）：Mutate 工具回执带 `fact{subject,action,basis,confirmed,detail}`；核心在工具返回后立即播报事实句（`answer_rendered{source:"tool"}`），模型不得自证动作。未确认时只说「没能确认…」；模型无回执却断言动作→整句被拒（`answer_rendered{source:"refusal"}`）。
- **回执类别**：`/mode` 每设备 `outputs[*]` 带 `kind`（`answer|prompt|filler|ack`；媒体播放为 `media`），回执不再靠猜。

### 家庭自动化与实体发现（R2-BM6.1 定稿，取代 `home.light` 的工具名）

```jsonc
POST /api/v0/config/services/home/discover
// → { ok, state, detail, entities:[{entity,name,state,domain,supported}],
//     domains:{light:1,...}, supported, http_requests, latency_ms }
```

- **工具 `home.set`**（服务 `home`）：`{entity, state}`，`state ∈ on|off|open|close`。
  `light/switch/fan/input_boolean` → `turn_on|turn_off`（读回 `on|off`）；
  `cover` → `open_cover|close_cover`（读回 `open|closed`）。模型只用白名单里的 `name`。
- **可控域是保存时的硬边界**：`light`/`switch`/`fan`/`input_boolean`/`cover`；
  `lock`/`alarm`/`valve`/`script`/`automation`/`shell_command`/`python_script`/`hassio` 连白名单
  都进不去（不依赖模型自觉）。
- **发现是只读管理动作**：读 HA `GET /api/states`，响应 ≤1 MiB、列表 ≤500、名字去标签/控制字符/
  折叠空白/≤60 字；`supported` 标出可控域；传感器/锁/场景仍列出但不标可用。
  **发现只需要连接**（backend+地址+令牌），不需要已有白名单；未配置/连接不完整 = 0 HTTP。
- 发现结果只是选择器：勾选后保存才进入白名单；已有手改白名单不被覆盖；手动添加保留。
- 事实合同不变：`home.set` 仍以写后回读为唯一 `DeviceConfirmed` 来源；`open`/`close` 由核心渲染
  为「已打开/已关闭」；读回不一致/超时仍是 `Unknown`，`service_accepted ≠ confirmed`。
- **两种接入模式（R2-BM6.3，2026-11-13 用户指令）**：`PUT /config/services/home` 增加
  `entity_source: "api" | "exposed"`（默认 `api`）。
  - `api`＝BM6.1 现状：`GET /api/states` + 控制台搜索/域筛选/勾选/删除/手动添加。
  - `exposed`＝HA 的 Assist 暴露：WebSocket `homeassistant/expose_entity/list`，只取
    `conversation: true`；实体集来自 WS，名字/状态用一次有界 `GET /api/states` 补全（补不上就用
    object id，状态留空，不编造）。控制台只读，改动回 HA。
  - 失败**按名**返回：`state:"unavailable"`、`code:"HOME_EXPOSURE_UNAVAILABLE"`（连接/鉴权/命令/超时
    各自带 detail），**不回退到 `/api/states`**，`http_requests:0`、`ws_commands:1`。
  - **A/B 切换清空旧目录**：`entity_source` 变化时保存会清空 `entities`，重新发现并保存后才 `ready`
    （不产生半配置）。

### 反射词表（R1j/P1 定稿）

```jsonc
GET  /api/v0/reflex   // { words:[{text,tokens,intent,enabled}], compiled:"<kw.txt 全文>", hash, error, warnings:[…] }
PUT  /api/v0/reflex   // { words, deploy?:bool, device? }：校验→入史（scope reflex）→（可选）立即部署
POST /api/v0/reflex/deploy   // { device? } → { state:"sent", hash, words, keywords_file }
GET  /api/v0/reflex/tokens?text=暂停一下   // → { text, tokens:"z àn t íng y í x ià", error:null }
```

- **汉字→拼写**：`tokens` 留空时保存会**自动生成**（同一个函数支撑上面的 GET）。规则：`pinyin`
  带调音节 → 声母＋带调韵母（`zh/ch/sh` 整体；`uì/ià/ǎo` 等整体韵母不拆）→ `一/不` 变调；
  非汉字指名报错。控制台输入汉字即自动填充，手改后可用「生成拼写」重新生成。

- **一份配置，一处策略**：`intent` ∈ wake|retreat|pause|continue；逐词执行条件（有会话/有书签）在核心，判定在 `interaction::decide`，文本层只做兜底。
- **编译前置校验**（核心）：至少一个启用词、词 1–12 字、无空白/@、不重复、token 1–16 个。
  **token 数 <6 只给警告、不拒绝**：实测这个模型对 4-token 短词 KWS 0/5，但同一张表会经转写
  兜底执行（约 1 s），所以短词仍可保存/部署，`warnings` 里说明这个代价。
- **部署后回执**：`/mode` 每设备 `reflex.deploy{state,hash,keywords,detail,sent_ms,acked_ms}`；`state=sent` 是「帧已发」，`accepted/rejected` 才是设备事实。
- **每词计数**：`/mode` `reflex.words[*].hits/ignored/text_hits`（每个词单独判误触发预算；`hits`＝
  设备 spotter 命中，`text_hits`＝同一张表由转写执行——控制台据此显示该词实际走哪条路）。
- 手工改设备上的 `kw.txt` 会被下一次部署覆盖；网关校验会指名坏 token。

**M3（v0.10）**：`intent` 增加 `next|stop`；默认表 7 词（你好高达／暂停播放／继续播放／下一首／停止播放，
另加家庭已有的拜拜/再见）。约束与判据：
- **唤醒带控制词不做 barge**：转写里唤醒词后面跟的是反射控制词（暂停/继续/下一首/停止）时，唤醒路径把这句话
  交给同一个意图执行器，而不是先停音频——否则「暂停」会丢掉会话、「继续」会把刚续上的播放停掉。
- **同意图去重窗 3 s**：spotter 比整句转写快约 1–2.5 s（实测 2.5 s），窗口太短会让同一条「下一首」执行两次。
- **可命中性是词自己的事实**：受控离线测（单文件单词＋唤醒对照）里 `下一首`/`停止播放` 在 0.07/0.05/0.03、
  tb0/1 全部 0 命中；它们仍保留在表里，由转写兜底执行。控制台看 `hits`（spotter）与 `text_hits`（转写）分开记。

### 通用接入（R2-BM6 定稿）：新闻、OpenAPI 查询、音量/模式管理工具

**工具合同扩展（加法）**

- `FactBasis` 新增 `device_receipt`：设备自己的回执携带结果值（`volume_receipt`、`mode_report`）；
  只有 `Evidence::DeviceConfirmed` 能与之组合。`ToolFact` 新增可选 `value`（"80%"/"全双工"），
  动作新增 `set`/`switch`；核心渲染「{subject}已调到{value}。」/「已把{subject}切换到{value}。」，
  未确认仍是「没能确认…是否调节/切换。」。
- 管理工具（服务 `system`）：`system.volume{percent 0–100, device?}`、
  `system.duplex{mode half|full, device?}`。二者直接调用 `VoicePipeline::set_volume`/`set_mode`
  ——与 `POST /devices/{id}/volume`、`PUT /mode/{device}` 同一校验、同一配置历史（actor=voice）、
  同一设备下发。`device` 可用 `device_meta` 的名字/房间。事实只认命令之后到达的设备回执
  （hub 到达序号，不是墙钟）：无新回执＝`Unknown`；设备新报告的值不是目标＝`Failed`（如
  capability=half 请求 full）。

**服务块：新闻**

```jsonc
GET  /api/v0/config/services/news   // enabled/sources[{alias,url,kind:rss|atom|json,max_items}]
                                    // /default_alias/timeout_ms/state/revision
PUT  /api/v0/config/services/news   // 校验→保存→入史（scope news）；带 revision，过期 409
POST /api/v0/config/services/news/test   // 真探针：逐源真实请求，返回每源 state/items/latency
```

- 状态词 `ready|unconfigured|disabled`（与天气/Home 同口径）；未配置/关闭 = **0 HTTP**。
- 工具 `news.headlines{source?,limit?}`：只读标题；标题去标签/CDATA/实体、≤80 字；链接只留
  http(s)；每次 ≤10 条。空源=`NEWS_EMPTY`、非 feed/截断=`NEWS_PARSE`、超时/离线/HTTP 命名；
  重定向不跟随；响应 ≤256 KiB。**没有 mock 新闻数据**。
- **v0.14 不点名源时的默认**（M7-3）：配了 `default_alias` 就用它；否则**合并全部源**
  （并发读取、逐源截断、条目按源轮转交错、每条带 `source`，`merged:true`）；
  一个源坏/空不影响其余，失败源在 `failed_sources` 里点名；全部失败才报错。
  `NEWS_SOURCE_UNKNOWN` 仍用于点名了不存在的源；`NEWS_SOURCE_REQUIRED` **不再返回**。

**服务块：OpenAPI 查询适配器**

```jsonc
GET  /api/v0/config/services/openapi   // enabled/spec_url/base_url/token_set/operations[]/
                                       // timeout_ms/retries/state/revision
PUT  /api/v0/config/services/openapi   // {enabled?,spec_url?,base_url?,token?,operations?[id],
                                       //  timeout_ms?,retries?,revision?}
POST /api/v0/config/services/openapi/discover // {spec_url,token?} → 文档里的 GET 操作（不保存）
POST /api/v0/config/services/openapi/test   // 真探针：重取文档＋核对白名单＋（无必填参数时）调一次
```

- **v1 只接 GET**：保存与导入都校验 method；执行固定 GET。写操作没有回读合同，不接入。
- **文档只在保存/发现/探针时读取**；解析出的 method/path/参数子集存进配置，工具调用不再取文档。
- **参数子集在 HTTP 之前**：未知参数/缺必填/类型/枚举/路径参数 `.`/`..`/斜杠/反斜杠/最终路径编码段
  本地拒绝，0 HTTP；查询串 ≤1024 字符、参数 ≤8 KiB、响应 ≤64 KiB。
- **令牌只写且绑定来源**：`openapi.token` 在 secrets 表；普通 GET/`/mode`/普通导出只回 `token_set`，
  显式 `?secrets=1` 备份才含值。换 spec URL 且未给新令牌 → 清除旧令牌；显式空串即清除。
  上游回显的令牌在进入结果/trace/历史前脱敏（含 JSON 键、操作元数据）。
- **重定向不跟随**；探针核对 id/path/**参数目录**漂移，返回 `OPENAPI_SPEC_CHANGED`。
- 状态词 `ready|unconfigured|disabled`；未配置/关闭 = 0 HTTP。导入的目录只做本地结构校验
  （受信备份例外，不发 HTTP）。

**一键自检**

```jsonc
POST /api/v0/diagnostics/self_check   // {deep?:bool} → {verdict,verdict_stage,stages[]}
```

- 五阶段 `mic→asr→model→playback→services`；每阶段 `{stage,state,detail,evidence}`；
  状态词 `ready|no_device|inactive|stale|unconfigured|disabled|failed|unknown`。
- 默认**被动**：0 外部请求、0 设备动作；`deep:true` 才调用已有真实探针（LLM ping、ASR 样本、
  TTS 合成、各服务探针），**不出声**。
- `verdict`＝`ready|degraded|failed`；`verdict_stage` 指向第一个非 ready 的必需阶段（服务探针
  失败记 `services`）。

### 语音跟随窗（R2-BM6.2 定稿）

- 跟随窗开＝会话 `Listening` 且（有 live 的 voice 输出 或 `now - max(last_activity, last_voice_end) < 30 s`）。
- **30 s 从最后一段语音播放结束（`PlayFinished`）起算**，不按 answer/prompt/ack/filler 分类；语音正在播时
  窗恒开，扫描器不得在播报中把会话关掉。
- `退下` 仍清零：关闭会话并阻断跟随窗，退下应答本身是语音播放，其结束不重开；下一次唤醒/说话/打断解除阻断。
- `/mode.session.state`、`assistant_state`、LED 与窗口结束提示音共用这个判定（提示音的 30 s 也从最后一段
  语音结束起算）。
- 判据与现场＝[follow-window-20261113](d/follow-window-20261113.md)。

### 记忆与对话清除（2026-11-13）

```jsonc
GET    /api/v0/memories                  // {items,count,max,max_text,kinds}
POST   /api/v0/memories {text,kind?,pinned?}
PUT    /api/v0/memories/{id} {text?,pinned?}
DELETE /api/v0/memories/{id}
POST   /api/v0/memories/clear
POST   /api/v0/session/{id}/clear        // 设备会话 id＝dev:<stable_id>
```

- **记忆内容**＝重要事件＋用户画像（称呼/名字的正确说法、偏好、习惯、家里的大事），单条 ≤200 字、
  总数 200、同文合并；**不记**服务是否配置、工具回执、播放/音量/进度这类会自己变化的状态。
- **注入**：系统提示末尾「关于这个家」块（≤40 条/2400 字，钉住优先），明写"只作背景，不逐条复述，
  也不作配置/工具状态的来源"。工具 `memory.remember`（service=`memory`，Query）是唯一语音侧写入口。
- **对话不是配置的事实源**：任一服务的可用性变化（`ready` 工具集变化）时，核心清空所有 `dev:*`
  会话消息并入配置史；控制台/文本会话不动。设备页/设置页另有手动「清空对话」。
- **v0.14 自动提取候选**（M7-1，默认开）：
  ```jsonc
  GET    /api/v0/memories/candidates?status=pending|confirmed|rejected
  POST   /api/v0/memories/candidates/{id}/confirm   // 人工采纳 → 写入记忆（source=auto）
  POST   /api/v0/memories/candidates/{id}/reject    // 忽略
  DELETE /api/v0/memories/candidates/{id}
  ```
  会话结束（退下或跟随窗超时）后，模型只**提候选**（≤3 条/会话）；**第二条后台审核**逐条确认后
  才写进记忆——审核失败时候选留在 pending，绝不默认写入。候选有界：20 条待确认、7 天过期、
  运行状态/配置文本（已配置/音量/正在播放/固件/密钥…）确定性拒收、同文去重。控制台记忆卡
  显示待确认候选（采纳/忽略）。注入测试环境（injected brain）默认关闭提取，由专门测试开启。
- 设计与取舍＝[memory-and-conversation-20261113](d/memory-and-conversation-20261113.md)。

### 语音端点：房间相对阈值（R2 交互层，2026-11-13）

范围：**共同输入端**的端点判定。协议不变（`hello.protocol` 仍为 0），变化在核心与 `/mode`。

- **配置值 = 灵敏度上限**：`vad.start_db` / `vad.end_db` 仍是用户标定的绝对 dBFS。
  检测器**只允许上调**，绝不在安静房间把它变得更敏感。
- **房间地板**：只从"非语音、且不在段内"的块估计（落后 100 块≈8 s 的 p20）。段内冻结 → 永不学用户的声音；
  自身播放期间核心不喂 VAD → 也永不学我们自己的声音。
- **有效阈值**（`/mode.mic.vad` 的 `effective_start_db` / `effective_end_db`）：
  `end = max(配置 end, floor + 6)`；`start = max(配置 start, end + 5, floor + 10)`。
- **`close_reason`**：`quiet`（静音挂尾到期＝句子结束）或 `cap`（撞 `max_blocks`）。
  cap 关段**不裁尾**；`/mode.mic.counters.close_quiet` / `close_cap` 分开计数。
- `mic_segment_closed` 事件带 `close_reason`、`floor_db`、`start_db`、`end_db`。
- 判据与现场＝[interaction-layer §1](d/interaction-layer-20261113.md)。

### 唤醒应答与回合身份（R2 交互层，2026-11-13；**v0.9 修订**）

- **只在裸唤醒时播 ACK**（v0.9，用户 2026-11-13）：KWS 命中后 ASR 只转出唤醒词 → 播一次 ACK；
  **同一口气带命令（「你好高达，今天周几」）不播 ACK**，直接执行命令——命令本身就是唤醒的应答。
  慢轮次由垫话覆盖。旧行为（先 ACK 再有界等待再执行）已废弃。
- **ACK 的终态是分类的**：`play_finished`（或设备不发 finish 时的 `device_confirmed`）＝`finished`；
  `failed`/`unknown`＝`failed`；waiter 被丢弃＝`dropped`；超时＝`timeout`；没播＝`not_played`。
  `/voice/log` 的 `wake_ack_settled{settled,outcome}` 是唯一判据，`settled:true` 不再等同于"播完了"。
- **回合身份在等待之前取**：`stop_epoch` 在 ACK 之前捕获，ACK 播完复核；不一致则
  `turn_cancelled_during_ack` 并放弃命令（`turn_cancelled_before_start` 是入口侧的同一条规则）。
- **窗口开启者**：KWS 开的窗、且未被回合消费时，ASR 只转出唤醒词 → 播 ACK
  （`wake_ack_after_keyword_window`）；其它情形仍是自家回显，不播。
- KWS 命中本身仍然不出声（矩阵 `ack:false`）。

### 语音回答长度（R2 交互层，2026-11-13）

```jsonc
GET  /api/v0/mode                      // 顶层 spoken{max_sentences,max_chars,detail_*,long_form_words,continue_hint}
PUT  /api/v0/mode/{device} { "spoken": { … } }   // 同一校验/历史/恢复链（scope voice）
// 校验：max_sentences 1–8、max_chars 40–600、long_form_words 1–16 个 1–8 字、
//      detail_max_sentences 1–8、detail_max_chars 40–600、detail_words 1–16 个 1–8 字、
//      continue_hint ≤40 字且不含引号/斜杠
```

- **交付形状（v0.8 三档＋失败短路）**：判据是**用户自己的话**（可配词表），不是模型自声明，也不按服务名。
  - `VoiceLong`（讲个/念/播报/全文/从头…）＝不裁剪；
  - `VoiceDetail`（讲讲/说说/解释/最近/为什么…）＝**6 句/300 字**（可配），超出给「继续」；
  - `VoiceShort`（其余）＝2 句/120 字；
  - **失败短路**：本轮**所有**工具结果失败且无动作事实 → 核心渲染**一句 ≤40 字**，
    模型散文整段丢弃（`answer_rendered{source:"tool_failure"}`）。取句顺序：
    工具的 `rendered_answer` → 短且像人话的 `detail` → 按错误类给一句（不按工具名）。
- **同一份预算两处读**：系统提示的长度条款与交付层硬上限用同一组数字。
- **三个出口都受约束**：流式 token、整段 Answer 回退、Program 续播。句数 **且** 字数同时限制，只切整句。
- **事实豁免**：核心渲染的动作事实/拒答/失败句走 `AgentEvent::Fact`，不受预算约束。
- **续听**：被裁时在同一流里补 `continue_hint`；程序按"听完的位置"入 `paused_program`。
- **停止不复活书签**：普通停止与 `退下` 一律丢弃（`program_interrupted{bookmark_kept:false}`）。
- 设计与判据＝[interaction-layer §3](d/interaction-layer-20261113.md)、[m2-delivery](d/m2-delivery-20261113.md)。

### 跟随窗提示音不压媒体（v0.8，2026-11-13）

- 跟随窗关闭的提示音是 **voice** 通道播放，只在"没有语音输出、也没有媒体播放"时发；
  媒体播放在 hub 注册表里（不在 `st.outputs`），判定必须查 `hub_live(device).media`。
- 设备现场：`follow_window_tone` 曾在音乐开始 34 s 后响（298 ms「滴滴」），修复后不再发。
- 媒体继续遵循 R1g 矩阵：唤醒词是唯一命令入口；提示音缺失不改变窗口/会话语义。

### 天气备用源与城市代码（v0.9，2026-11-13）

- **主源**：`provider=open_meteo`，`endpoint` 为空时用官方主机；`endpoint` 非空＝用户自定义，
  **不启用备用源**（不绕过用户指定的主机）。
- **备用源**：中国天气网（`http://t.weather.itboy.net/api/weather/city/<城市代码>`），
  **按城市代码**查；`WeatherPlace.city_code`（可选，6–12 位数字）为空＝该地点没有备用源。
- **触发条件**：主源 `TIMEOUT|OFFLINE|HTTP` 时尝试一次备用源；备用成功 → `provider_used` 标明来源
  （`weather_com_cn`），失败仍报主源错误。主源失败后 **10 分钟**内直接走备用源（避免每次 8 s 等待）。
- **不变量**：只透传固定的中文天气词表（晴/多云/阴/小雨…），服务商文本永不进入模型上下文；
  `observed_at`＝响应日期＋观测时刻；`age_ms` 仍是“距获取”。
- 管理 API：`/config/services/weather` 的 `places[]` 新增 `city_code`；探针回 `provider_used`。

### 工具结果的新鲜度与失败呈现（R2 交互层，2026-11-13）

- **年龄进权威渲染**：`weather.now` / `weather.forecast` 的 `rendered_answer` 在 stale 时以
  「现在连不上天气服务；这是 N 分钟前获取的记录（观测时间 …）：」开头，且 `replace_answer: true`
  ——旧数据不会被模型改写成「现在」。
- **`freshness`**：`{status: fresh|stale, age_ms, fetched_at_ms, observed_at}`；
  `age_ms` 是**距获取**的时间，不是供商观测时间。缓存策略本身不变（≤10 min 直用、≤1 h 可降级、更老过期）。
- **失败也说真话**：失败结果带 `rendered_answer`（「现在连不上天气服务，暂时没有可靠的天气数据。」）、
  `replace_answer: false`；模型空答时由 `answer_rendered{source:"tool_fallback"}` 念出，**不编造任何数字**。

### 播放兜底与残留清理（R2 维护，2026-11-13）

- **核心绝不无限等设备**：语音流 `play_end` 之后 10 s 内没有任何终态回执（`play_finished`）→
  该输出标 `unknown`、移出 hub 活动表（`activity`/LED/控制台不再说 speaking）、清掉该轮已结束的
  live program、记 `play_stalled{stage_before,waited_ms,bound_ms}`；下一次 1 s tick 按既有跟随窗
  规则回 idle/listening。整段（whole-WAV）播放不发 `play_end`，不参与这条。
- **终态记录会退场**：`stopping`/`failed`/`unknown` 记录 5 s 后从 `/mode.outputs` 清出；
  其中 `stopping` 且 hub 里已没有该 play 的立即清（不可能再有回执）。
- **stop 的账要能对平**：命名 stop 只保留 hub 实际发出 Stop 的记录（这些会等回执），本地停播
  路径立刻清空。判据：一次播放/停止后 10 s 内 `/mode.outputs` 为空，`gateway.pending_plays=0`。
- **音乐流的 HTTP 超时（v0.9，实现约束）**：`reqwest::Client::timeout` 是“连接→读完响应体”的
  总时限，不能用于音频流；流式请求走独立 client（`connect_timeout 5 s`＋`read_timeout 30 s`，无总时限）。
  判据：整首歌 `sent_ms` 等于文件时长（真机 186.066667 s → 186,066 ms）。
- 判据与现场＝[device-hang-20261113](d/device-hang-20261113.md) §6。

### 截断提示（R1h，`PUT /mode`）

```jsonc
PUT /api/v0/mode/{device} { "truncation_notice": "故事到这里被截断了，后面的部分没有生成。" }
```

- 一句话（≤60 字、不含引号/斜杠），空串＝不说；保存后按同一份本地提示音缓存预合成（voice 变即失效）。
- 它是**节目的最后一句**（voice 通道、在节目未完成时提交），不是独立 ack：半双工麦克风门只关一次。
- `GET /mode` 顶层回读 `truncation_notice`；变更入史（scope voice）。
- 恢复（M3）：`POST /api/v0/config/history/{id}/restore` —— 经同一校验路径重放配置值，actor=restore；服务范围**保留当前密钥**（密钥不入史故不可恢复）；voice 范围恢复完整设置快照并重发 mode_set。**恢复不重放设备动作**（播放/音量/停止属于任务史，不是配置）。

### 对话（文本路径，无音箱可用）

```jsonc
POST /api/v0/chat            // { session_id?, text } → SSE 流：token / tool_start / tool_result / turn_end(含 trace_id)
POST /api/v0/session/cancel  // { task_id } 立即取消：停止输出与新动作，回执状态如实
```

### 观测与诊断

```jsonc
GET /api/v0/trace?limit=50        // 请求列表：时间、来源（文本/设备）、目标摘要、状态、耗时
GET /api/v0/trace/:id             // 单次全链路：识别文本→目标摘要→工具调用与实参→各阶段结果与证据等级→阶段耗时
GET /api/v0/tasks                 // 活动任务与状态（含迟到补记）
GET /api/v0/status                // 服务连接状态、网关连接、设备清单与模式、数据目录
```

- trace 是"查一次请求为什么成功/失败"的数据源；缺失证据标 `unknown`，不粉饰。
- 凭证不入 trace/日志；`key_ref` 只显示名字与"已设置/未设置"。

### 语音事件导出与每轮时间线（M6b，2026-11-13）

```jsonc
GET /api/v0/voice/log                 // 控制台视图：内存环最后 100 条（口径不变）
GET /api/v0/voice/events?since=<ts>&since_seq=<n>&since_epoch=<epoch>&limit=<n>
     // {events,count,truncated,epoch,next_seq,next_ts,files,stats}
```

- **为什么要落盘**：验收要逐次归属一次 40 分钟窗口里的每个事件；内存环硬截 100 条，十次唤醒
  就打满。现在每个 `note()` 同时写 `<data_dir>/voice-events/voice-events-YYYYMMDD[-NN].jsonl`
  （一行一个 JSON，含 `seq`/`core_epoch`/`ts_ms`）。**按 UTC 日切＋单文件 4 MiB 滚动，
  目录总量 32 MiB（超了删最旧）**；写入由独立线程负责，管线只入队（队列满＝计数丢弃，
  不静默）；不做逐条 fsync（进程崩溃不丢已写，断电可能丢尾）。
- **游标**：`since`＝排他时间戳；`since_seq`＋`since_epoch`＝精确游标（`seq` 随进程重启归零，
  跨 epoch 的 seq 游标被忽略，改用时间戳）。`next_seq`/`next_ts` 可直接作为下一次的值。
- `/mode.voice_journal`＝`{written,dropped,rotated,pruned,files,bytes,file_cap_bytes,total_cap_bytes,epoch}`
  （控制台与自检直接显示，磁盘上限是数字不是承诺）。
- **每轮时间线**：`turn_end.timeline`＝
  `{turn_ms,first_byte_ms,first_audio_ms,last_audio_ms,audio_live_at_end,played_ms,answer_plays,stop:{why,at_ms}|null}`。
  全部是观测值：`first_byte` 是模型第一个输出事件；音频只数 `kind=answer`（ACK/垫话/提示音另计），
  `played_ms` 来自设备终态回执。流式句子可能比 turn 记录活得久，此时 `audio_live_at_end=true`，
  由 `turn_audio_end{task,ended_ms,played_ms,answer_plays}`（按 task 连接）给出真正的末音频。
  `stop.why` 目前是 `stop_epoch_changed` 或 `turn_budget`。
- **无效能量**：设备上报的 `f32::MIN`（"没有起始切片"哨兵）在核心事件里记为 `null`，
  不是 −3.4e38；设备侧也不再用它喂 AEC 收敛门（见 v0.14 固件说明）。
- **自检**：`POST /diagnostics/self_check`（被动口径）新增 `last_turn`（最近一次 `turn_end`
  的时间线）与 `voice_journal`；仍然 0 外部请求。

### 诊断（M3）：被动查询 / 主动测试 / 修复三种性质分开

```jsonc
GET  /api/v0/diagnostics                      // 被动：纯已观测状态（服务配置态、网关快照、设备模式/活动、
                                               //   语音与网关事件计数、近期异常、近期失败 trace）；
                                               //   该端点本身不发起外部调用、不做设备动作
POST /api/v0/diagnostics/tests/voice_play     // 主动（可闻）：经当前 TTS 合成一句短语音并在已连接 audio_out
                                               //   设备播出，回报回执 stage（device_confirmed/unknown/…）与真实耗时
POST /api/v0/diagnostics/repair/reapply_mode  // 修复：{ device } 向单设备重发已持久化的双工请求；
                                               //   仅配置投影，协商事实以设备 mode_report 为准
```

### 通用搜索服务（M2.5 定稿）

`GET/PUT /api/v0/config/services/search`，`POST /api/v0/config/services/search/test`：

```jsonc
// 读回（GET / /mode.search 同源）
{ "enabled": true, "base_url": "https://api.deepseek.com/v1/tools/search",
  "count": 6, "timeout_ms": 8000, "api_key_set": true,
  "state": "ready|unconfigured|disabled", "detail": "...", "max_results": 8,
  "revision": "…" }
// 写（PUT；api_key 只写：缺省＝保留，""＝清除；revision 不符 → 409）
{ "enabled": true, "base_url": "...", "count": 6, "timeout_ms": 8000,
  "api_key": "…", "revision": "…" }
// 探针（POST /test；一次真实查询）
{ "ok": true, "state": "ready", "http_requests": 1, "latency_ms": 370,
  "count": 2, "samples": [{ "title": "...", "url": "https://…", "date": "..." }] }
```

- **未配置（无 key）或已关闭 = 0 HTTP**（探针与工具都在配置门之后）；`state` 三词与天气/新闻/HA 同口径。
- 密钥存 `search.api_key`（secrets 表），状态只回 `api_key_set`；备份导出/导入带该块，密钥仍只在 `?secrets=1` 的 secrets 里。
- 默认 `base_url` 是 DeepSeek Search API 形状（`POST`，Bearer）；任何兼容端点可替换。

### 媒体会话（M3 定稿，`/mode.media`）

```jsonc
"media": { "dot2-d": { "state": "playing|paused", "title": "…", "artist": "…",
  "source": "library|online", "index": 2, "tracks": 5,
  "position_ms": 5433, "since_ms": 1794554024549, "updated_ms": … } }
```

- `position_ms` 是**听到的位置**（播放中＝墙钟推算，暂停＝冻结值），不是提前 20 s 的推送位置。
- 会话生命周期：新播放替换；`pause` 保留（**10 分钟 TTL**，播放中不过期）；`stop` 删除；自然播完在设备
  `play_finished` 后删除。`position_ms` 仅作续播起点，不是设备消费回执（M6 的 `consumed_ms` 另议）。

### 端点检测器（M1b，`/mode.mic.vad.endpoint`）

```jsonc
"vad": {
  "start_db": -45.0, "end_db": -49.0,            // 能量 VAD 的配置值（回退时才是判据）
  "effective_start_db": -40.1, "effective_end_db": -45.1,
  "noise_db": -51.1,
  "endpoint": {
    "kind": "silero",                            // silero | energy —— 现在是谁在判语音
    "stats": { "frames": 542, "speech_frames": 0,
               "threshold": 0.5, "gain": 3.0, "last_prob": 0.023 },
    "fallback": null,                            // 模型推理失败的原因（一旦失败本进程回能量 VAD）
    "model": "/data/hm/data/models/silero_vad.onnx",
    "model_detail": "loaded in 1168 ms"          // 或 "model file absent: energy VAD in use"
  }
}
```

- `kind` 是**处理器自己**的回答：模型缺失/加载失败/推理失败时如实变 `energy` 并给 `fallback` 原因。
- `stats` 只在 `kind=silero` 时有值；`frames` 是 512 样本帧（32 ms），`speech_frames` 是其中越过阈值的。
- 模型参数（threshold 0.5、gain ×3）取 RUI 生产值，**本版不做成用户可调项**；能量 VAD 的
  `start_db`/`end_db` 仍是回退路径的校准值，控制台照旧可写。

### 逐句续播与回合预算（M5，行为合同）

- **逐句续播**：一个书签被「继续」读出时，**每句都要等它自己的终态**（`play_finished` 或 `stop_receipt`）
  才提交下一句；流式回答不受影响（一整条流，句间不留缝）。书签游标只在句子**走完**时前进
  （`finished`，无论 `hw_delay` 还是 `written`）；被停/链路断/超时的句子把游标留在该句上——
  **重复一句可接受，跳过没人听到的音频不可接受**。
- **消费游标**：续播游标与 `program_delivery.consumed_ms` 只读 `consumed_ms` 且 `progress_basis=hw_delay`；
  `written` 单独记在 `played_ms`，绝不并入"听到"。
- **回合等待预算**：首答 **15 s**、整回合 **45 s**，只累计**没有我方语音在播**的时间——节目/媒体播放
  一秒都不计入（R1 把长流误截的根因就是不区分）。超时记 `turn_budget_exceeded{phase,wait_ms}` 并
  有界结束，`turn_end` 仍然只发一次。
- **ASR 输入身份**：每个分段在关闭时取 `input_id`（单调）、`capture_seq`（块游标）与
  `window_epoch`（当时的停止代）。文本回来时：代已变 → `transcript_stale_after_epoch`（不执行）；
  `input_id` 不大于已处理的最大值 → `transcript_out_of_order`（不执行）。两个窗口的转写永不串身份。

### 应用发布与回滚（M8a，v0.15，2026-11-14）

设备的应用层（核心/固件＋网关/控制台/模型）按**版本化发布包**更新。设备协议不变；新增的是
管理 API 与一套由独立安装器承担的规则。

```jsonc
GET  /api/v0/release                     // {installer, releases_dir, state, live}
POST /api/v0/release/install  {url, activate?}   // 202 {state:"started", pid, action:"install"}
POST /api/v0/release/activate {id?, reason?}     // 202；空 body 合法（= 重激活当前发布）
POST /api/v0/release/rollback {reason?}          // 202；空 body 合法
GET  /api/v0/release/log?lines=200               // installer.log 尾部
POST /api/v0/release/snapshot {tag}              // 内部：激活前 DB 快照 → {path, schema, ms}
GET  /api/v0/status                              // + build {version, id}；db.schema
```

- **清单与验签归 `hm-release`**：包为 `home-mate-release-<ver>.tar`（内含 `manifest.json`
  与 `manifest.sig`，另发同名两个散文件）。清单字段：`manifest_version`、`release{serial,
  version, created_ms, notes}`、`target{model, profile, arch}`、`firmware_range{min,max}`、
  `components{core, firmware(+gateway_version), console(files+index), model}`、`total_size`；
  所有 sha256 是真 SHA-256（**不用** `fwupdate::sha256_prefix()`——那是 FNV-1a）。
  设备先逐构件验 hash、再验 Ed25519 签名（公钥编译进安装器；私钥只在开发 Mac），
  **任何一项失败整包拒绝**：不落槽、不翻指针、旧发布继续运行；`serial <= 已装` 视为回放拒绝。
- **唯一激活者**：只有 `hm-release` 写 `releases/current`、`bin/current`、`hm-direct/current`、
  `web` 与模型符号链接；核心只编排（spawn 子进程）并展示状态，不持有密钥。
- **健康确认**（激活后逐项记录）：核心 `/status.build.id` 等于清单 build id、网关
  `connected` 且设备 online、设备 model 与清单一致、固件版本一致（firmware 与 gateway 两个标记）、
  `kws.state=running`、Silero 端点已加载（非能量回退）、`/` 返回的 console index 属于该发布。
  重启顺序＝固件先、核心后；两者都要**进程真的换代**才进入健康确认。失败自动回上一版；
  结果 `state.last.status = ok|failed|rolled_back|rollback_failed`，逐项 criteria 留证。
- **DB 兼容**：激活前 `VACUUM INTO` 快照（含 WAL 内容）；核心启动后写
  `data/home-mate.schema`（schema + build）。回滚时若 schema 前进过，写
  `releases/restore-on-boot.json`，核心在 `Store::open` **之前**恢复快照并删 `-wal/-shm`；
  schema 未变则保留当前 DB。断电/监督器换槽导致的“旧核心 + 新库”由同一开机守卫兜住
  （`status=="running"` 且目标 build 不同、DB 在快照后被改动）。
- **约束**：同一时刻只允许一个安装/激活作业（安装器 flock 是硬闸；核心对“活着”的 running
  记 409，对断电残留的死 pid 不永久上锁）；核心侧 `/release/*` 的管理鉴权模型与其他
  管理端点相同（当前家庭 LAN 无鉴权，M8b/M9 再定）。

### 语音转写的唤醒兜底（v0.16，2026-11-14）

半双工下唤醒有两条路：设备端关键词（本地匹配唤醒词，不上传音频）与**把空闲时说的话也送去
识别**（ASR 兜底）。后者决定“房间里的说话是否被转写”，是**用户可决定的采集策略**，默认开。

```jsonc
GET  /api/v0/mode                 // + asr_wake_fallback: bool（全局）
PUT  /api/v0/mode/{device}  { "asr_wake_fallback": false }
                                  // 200；全局立即生效（device 只用于变更记录归属）
// 设备视图：mic.counters.asr_skipped  （兜底关闭后被跳过的空闲语音段数）
```

- **开（默认）**：任何关闭的 VAD 段都转写，用于匹配唤醒词（未命中＝`mic_ignored_unwoken`）；
  关键词没听清时仍可能被叫醒。代价＝房间里的说话送到 ASR 服务并写入事件台账。
- **关**：只有会话打开期间（关键词唤醒、30 秒跟随窗、打断后的那一句）才转写；空闲语音段
  **不构造 WAV、不调用 ASR、不留文字**，只记 `mic_asr_skipped` 与计数。段的两端任一处于
  打开状态即转写（跨窗口边界的句子不丢）。
- **不变的部分**：设备端关键词 spotter 照常运行（只匹配词表、不转写）；VAD 照常运行；
  彻底停止采集仍是静音（`muted`）。唤醒路径变成“关键词唯一”，漏检即无响应——这是开关的已知代价。
- 与其它语音设置同一条持久化/历史/恢复链（scope `voice`）；配置备份导出/导入含该字段。

### 声音风格（v0.17，2026-11-14）

输出音色由**固定喇叭校正**与**用户口味 EQ** 两层组成（EchoMuse 同款分层）。

```jsonc
GET  /api/v0/config/audio
// { curve, bands, curves:["echomuse","official","rui","custom"], default, devices }
PUT  /api/v0/config/audio  { "curve":"echomuse", "bands":[{"freq":175,"gain_db":-6,"q":0.6}, …] }
// 200 { state:"applied"|"saved_not_applied", curve, bands, sent:[…], errors:[…] }
// 设备协议（核心→网关）：{ "type":"audio_eq", "device":"dot2-d", "curve":"echomuse", "bands":[…] }
```

- **curve**：`echomuse`（EchoMuse 的 biscuit voicing：80 Hz LR4＋65 Hz 陷波＋喇叭校正＋
  Bass Guard 20:1@−50 dB＋三段压缩＋+10 dB 音量顶＋−1 dB 限幅）、`official`（原厂 EQ_50
  拟合形状）、`rui`（旧阶梯）、`custom`（bands 即曲线，无阶梯）。
- **bands**：≤8 段 peaking；freq 20–16000 Hz、gain ±18 dB、Q 0.3–8；`custom` 至少一段。
  **API 拒绝越界**，设备端只做同样的 clamp 兜底。
- **下发时机**：设备上线（防重启留旧曲线）与每次保存；设备在下一个 42.7 ms 周期边界重建
  滤波链，**不重启**。生效事实在固件日志（`[loudness] audio_eq applied: curve=… bands=…`）。
- 变更记 history（scope `audio`，含快照）；默认值＝2026-11-14 真机试听选定的
  EchoMuse＋(−6/+4/+2 dB @175/3500/8000 Hz)。

### 补丁：OpenAPI 附加认证头＋设备 id 唯一（v0.19，2026-11-14）

```jsonc
GET/PUT /api/v0/config/services/openapi
// + auth_header: string        // 头名，空＝只发 Authorization: Bearer
// 值＝已保存的令牌（不新增第二个密钥）；换 spec_url/base_url 时与令牌一起清空
```

- 头名只允许 HTTP token 字符（字母数字 `- _ .`，≤64），拒绝 CR/LF 与空格；值永不出现在回执/日志。
- 同时发送 `Authorization: Bearer <token>` 与 `auth_header: <token>`；未配置头名时行为不变。
- **设备 id 唯一**：一次 hello 里出现重复 `stable_id` → 整条 hello 拒绝（不再静默覆盖同一 hub 条目，
  否则两台音箱会共享一份语音状态/序号水位/音效/会话）。

### 琥珀只点"人的暂停"＋续播开头语（v0.20，2026-11-14）

```jsonc
PUT /api/v0/mode/{device}
// spoken.resume_lead: string   // 续播系统截断书签时拼在第一句前的话；默认「接着刚才的话题。」，空＝不加
GET /api/v0/mode
// program.paused.system_cut: bool  // true＝预算截断留下的书签（不点琥珀，跟随窗关闭即清）
// 事件：program_cut_dropped（窗口到期沿清书签）、spoken_resume_lead（续播开头语）
```

- LED 语义收紧：琥珀（`assistant_state=paused`）只表示**人的暂停**（显式「暂停」、唤醒词接管长内容）；
  预算截断与故障续点是系统截断书签，不点琥珀，跟随窗两声滴滴关闭时随之清除（人暂停的书签仍跨窗
  存活 TTL 10 min）。设备事实：[d/cap-cut-bookmark-20261114.md](d/cap-cut-bookmark-20261114.md)。
- `resume_lead` 只作用于系统截断书签的续播；人暂停的节目不加（琥珀已说明状态）。校验同
  `continue_hint`（≤40 字，不含引号/斜杠）。设备协议不变。

### 交付形状交给模型＋定时提醒（v0.21，2026-11-14）

```jsonc
PUT /api/v0/mode/{device}
// spoken.backstop_max_sentences / spoken.backstop_max_chars  // 失控兜底（默认 400 句 / 2000 字）
// spoken.detail_words / long_form_words   // 字段保留，运行时不读（旧备份可读）
// 事件：spoken_resume_lead（续播开头语，同一段内容一次）、program_cut_dropped、
//       authoritative_line（工具权威句在回执时外发）、reminder_fired / reminder_speech_failed

GET  /api/v0/reminders            // { now_ms, reminders: [{id, device, due_ms, note, created_ms,
                                  //    fired_ms, cancelled_ms, state: pending|fired|cancelled}] }
POST /api/v0/reminders            // { note, device?, delay_minutes? | (hour+minute, day_offset?) }
DELETE /api/v0/reminders/{id}     // 取消待提醒；已响/已取消返回 REMINDER_NOT_FOUND
// 错误码：REMINDER_BAD_ARGS / REMINDER_TIME_IN_PAST / REMINDER_LIMIT / REMINDER_NOT_FOUND /
//         REMINDER_STORE / REMINDER_UNAVAILABLE
```

- **形状**：交付长度由模型按提示词自选（三档写进同一段），运行时只保留失控兜底；
  续播按 Program 快照的预算读一整段，不再一律 2 句。
- **提醒**：时间由核心用设备时钟换算成 `due_ms`（模型只提取结构）；工具句权威、
  模型不得改写；离线/静音/正在说话时保持 pending 并在下一拍重试，说完才标 fired。
  schema 6（`reminders` 表）。
- 新增工具：`reminder.set` / `reminder.list` / `reminder.cancel`（service=`reminder`，查询类副作用）。

### 交互审计修复（v0.22，2026-11-14）

```jsonc
GET /api/v0/mode
// mic.counters.suppressed_during_voice : 我们出声期间被门控丢弃的块数（此前无任何记录）
// mic.counters.asr_inflight            : 已提交未返回的转写数
// 事件：program_resume_dropped（取书签到启动之间发生过退场/停止，续播丢弃）
```

- 续播的 epoch 在取书签的同一把锁内绑定并传给播报任务；启动前比对不符即丢弃
  （此前 spawn 内部读当前 epoch，退场后故事会继续播）。
- 「继续」报告真实结果：书签被扫描清掉时不再谎报 `resumed=true`。
- 提醒触发在「正在说话」「有转写未返回」「最近一段关闭 <2.5 s」时都不播（保持 pending）。
- **转写唤醒也接管媒体**：`wake_word_stop` 与 `keyword_stop` 一样先暂停媒体（队列与位置保留），
  事件新增 `media_paused` 字段——此前只有 spotter 路径保留队列，转写路径会结束会话，
  窗口内的「下一首」被判 `no_media`（设备 2026-11-14 19:07）。
- 设备协议不变；审计全文见 [d/interaction-audit-20261114.md](d/interaction-audit-20261114.md)。

### 家庭平面鉴权（v0.23，2026-11-14，M9）

```jsonc
GET  /api/v0/auth                 // 开放：{ configured, corrupt, tokens:[{id,label,created_ms,last_used_ms}],
                                  //         loopback_trusted:true }（永不含令牌或哈希）
POST /api/v0/auth/provision       // 仅在“从未配对”时开放：{ token:"hm1_…", created_ms }；已配对 → 409
POST /api/v0/auth/rotate          // 需令牌：返回新令牌（只显示一次），旧令牌全部失效
// 其余 /api/v0/*        Authorization: Bearer hm1_<43 字符 base64url>
// 401 → {"error":"unauthorized","detail":"…"} + WWW-Authenticate: Bearer realm="home-mate"
```

- **真实性**：32 字节内核 CSPRNG 生成，`hm1_` 前缀，核心**只存 SHA-256**（config_kv
  `api_tokens`，列表形状）；比较为常量时间；令牌明文只在签发响应里出现一次，
  `/status.auth` 与 `/auth` 只报配对状态与时间戳。
- **覆盖**：所有 `/api/v0/*` 都要令牌，**除** `GET /auth`、`POST /auth/provision`（引导）与
  **loopback 对端**。loopback 信任是用户 2026-11-14 的决定：`hm-release` 经 `127.0.0.1:8930`
  做快照/重启/健康，安装器不做令牌支持，发布链不因升级顺序中断。控制台静态文件（index/assets）
  不鉴权——浏览器导航无法携带头；SPA 加载后由门禁出示配对页。
- **未配对是显式状态**：从未配对时 API 开放（否则新设备无法配对），控制台强制显示配对页，
  `/status.auth.configured=false`。**空列表（配对后被清空）按失败关闭**，不静默重开。
- **恢复**：令牌丢失＝root/串口动作——在 `<data_dir>/auth-reset` 建文件并重启核心，
  启动时清空令牌并删除哨兵；没有任何 API 能解除配对。
- **模型**：家庭级单令牌（用户二选一中的推荐项；当晚未答复，按默认执行）。存储为列表，
  按客户端配对（单独吊销）是增量扩展。
- 设备协议不变；设计/验收见 [d/m9-auth-20261114.md](d/m9-auth-20261114.md)。

### 静音下 deploy 的语义（v0.24，2026-11-14，M9.5）

```jsonc
// 回执 detail 的措辞（行为合同，不是新字段）
POST /api/v0/reflex/deploy → devices[].reflex.deploy
  { "state": "sent|accepted|loaded", "sent_hash": "...", "loaded_hash": "...|null",
    "loaded_matches": true|false|null,
    "detail": "… installed at …; spotter restart signalled" }
```

- **`reflex.deploy` 的语义在静音下同样成立**：设备静音时不产生 PCM，sidecar 不因此变得"停不下来"。
  固件 0.0.46 起：驱动以 250 ms 有界切片等待 stdin 并检查停止标志；监督者在 SIGTERM 1.5 s 后
  升级 SIGKILL，保证部署写下的词表就是随后实际运行的词表（不依赖 sidecar 配合）。
- **`detail` 只说"信号已投递"**（`spotter restart signalled`），不说"已重启"；没有活 sidecar 时说
  `no live spotter — applies on next start`。**"词表已加载"的唯一证明仍是 `loaded_hash`**
  （v0.14 起的 M6b 事实），`loaded_matches` 为 `null` = 还没拿到证明，不是成功。
- 失败预算不变：故意重启不计入 `kws.restarts`（RJ-4）。
- 设备协议字段不变（无新增/删除字段）。验收见 [d/kws-muted-deploy-20261114.md](d/kws-muted-deploy-20261114.md)。

### 关窗播报不再续期自己（v0.25，2026-11-15）

```jsonc
// 行为合同（无新字段）：关窗播报仍然只写一条事件
follow_window_close { "device": "...", "phrase": "拜拜啦", "stage": "device_confirmed" }
// 设置（管理 API，与 /mode 同一条写路径）
PUT /api/v0/mode/<device> { "follow_window_tone": false }   // 现为默认值
```

- **关窗播报的那一次播放属于"边界"，它的 `PlayFinished` 不得移动跟随窗。** 发布 33 的实现里
  `note_voice_play_end` 对所有语音播放结束都无条件把窗口参考推到"现在"，于是**播报自己把刚关掉的窗
  重新打开**：真机 dot2-e 2026-11-14 23:48:21 / 23:48:43 / 23:49:05 连说三次「拜拜啦」，间隔 21–23 s，
  正是 `PARTICIPATION_TIMEOUT_MS`(20 s) + 播报时长。修法＝按 **play id** 记住这一次播报
  （`boundary_play_id`），只有它的结束被忽略；用 id 而不是布尔闸，是为了**唤醒与播报并发时唤醒方仍保有
  自己的窗口**。标记命中即消费，陈旧 id 不会吞掉之后真正回答的结束。
- **`follow_window_tone` 默认改为 `false`**（用户 2026-11-14 深夜：「后面窗户都改成默认关闭」）。
  关的是**关窗播报**；**跟随窗本身、20 s 时长、追问额度 2 不变**。打开播报是安全的（见上一条修复）。
- 事件与计数不变：关窗仍写 `follow_window_close`（`skipped: muted|busy` 语义照旧），拒绝仍写
  `participation_decision {decision:"deny", reason:"followup_credits_exhausted"}`。
- 验收见 [d/follow-window-loop-20261115.md](d/follow-window-loop-20261115.md)。

### 凭证库与互联下发（v0.26，2026-11-15，M12a-1）

```jsonc
// 密钥在库内加密（无新字段）：值是 enc:v1:<b64(nonce‖ciphertext‖tag)>
// data/secret.key = 32 字节内核 CSPRNG、0600、首启生成、永不重建
GET  /api/v0/fleet                  // { self_id, self_id_source: config|env|device|default,
                                    //   push_actor: "fleet:<self_id>",
                                    //   nodes:[{id, base_url, added_ms, last_ok_ms, last_error, token_set}] }
PUT  /api/v0/fleet                  // { self_id }  — 这台机器的名字（出现在对端历史里）
PUT  /api/v0/fleet/{id}             // { base_url, token?, token_clear? } — token 只写，缺省=保留
DELETE /api/v0/fleet/{id}
POST /api/v0/fleet/ping             // {} 或 { ids:[…] } → { results:[{id, ok, latency_ms,
                                    //   build, version, product, data_dir, auth_configured, error}], ok, failed }
POST /api/v0/fleet/push             // { targets:[…], include_secrets:false, dry_run:false } →
                                    // { state:"pushed"|"planned", self_id, actor, results:[{id, ok, dry_run,
                                    //   base_url, actor, applied[], warnings[], http_status, error}], ok, failed }
GET  /api/v0/config/export[?secrets=1]   // 现在补上 reflex 与 audio_eq
POST /api/v0/config/import          // 头 X-Home-Mate-Source: <node id> → 历史 actor = "fleet:<id>"
```

- **加密落盘**：`secrets` 表的每次写入都经 `SecretVault`（ChaCha20-Poly1305），**密钥名做 AAD**，
  所以密文不能在行间搬运；`enc:v1:` 是唯一可识别的封装前缀。**旧明文行照原样可读**，启动时
  `seal_plaintext_rows()` 一次性封装，之后写入自然是密文。解不开的行（key 被换掉/丢失）**不崩**：
  计为 `undecryptable`，该密钥按"没有"处理，`/status.vault` 如实报数。
- **一次性回填**：启动时对 10 个 `CONFIG_SECRETS` 逐个判断——`api_key_env`（或约定的大写名）有值、
  库里没有 → 加密入库。**库里有行（哪怕是空串）永远优先**：清空过的密钥不会被还在 `env.local` 里的
  变量复活。日志只打名字。
- **`secret.key` 的位置与边界**：放在数据库旁边（`<data_dir>/secret.key`）。它保护"数据库被拷走"
  与"备份/快照"，**不保护已获得 root 的攻击者**——这是本设备可达到的上限，写在此处而不是暗示。
  发布/回滚只换二进制与静态资源，key 与库都不动。
- **互联**：对端元数据在 `config_kv["fleet"]`（**不动 schema**，避开 `hm-release` 的 schema 快照比较），
  对端令牌在 secrets 表 `fleet.<id>.token`（同样加密）。**两者都不进导出/备份**：那是本机的接线，
  不是家庭配置，导入别人的令牌只会指向一台它连不上的机器。
- **下发的形状**：发送端用与 `GET /config/export` **同一个** `build_config_document()` 生成文档，
  剥掉 `mode.wake_words` 与 `reflex.words[intent=wake]`，然后对每个目标
  `POST <peer>/api/v0/config/import` 带 `Authorization: Bearer` 与 `X-Home-Mate-Source`。
  对端**复用既有导入实现**（没有第二套写配置的代码），每块写入都记 `actor=fleet:<来源>`。
  接收端**再拒绝一次**：fleet 来源的 `mode.wake_words` 被本机当前列表替换，reflex 只接受非 wake 词、
  自己的 wake 词保留——每台自己的唤醒词永不下发（用户 2026-11-14 拍板）。
- **dry_run**：**纯本地计划**——不连接对端、不写对端、也不更新本机的 `last_ok/last_error`。
  返回的 `applied` 是"按对端导入的词汇会写哪些块"，便于把计划与真实结果逐项对比。
- **作用域**：只写 `targets` 列出的对端；未列出的第三台零接触（测试钉住）。
- **不在本片**：HTTPS/mTLS、per-peer 凭证轮换、发现（对端手工录入）、新令牌签发端点、
  任何对端"运行状态"（音量/播放/会话/发布指针不受 push 影响）。
- 设备协议字段不变。验收见 [d/m12a-1-vault-fleet-20261115.md](d/m12a-1-vault-fleet-20261115.md)。

### 预合成应答随配置变化重建（v0.27，2026-11-15）

```jsonc
// 行为合同（无新字段）：关窗播报缺素材时不再只是"跳过"
follow_window_close { "device": "...", "phrase": "拜拜啦", "skipped": "no asset", "synth": "requested" }
wake_ack_synth     { "synthesized": 1, "force": false }        // 已有事件，语义补全
```

- 唤醒 ACK、关窗播报、截断提示与垫话都是**本机预合成 WAV**。此前只有 `PUT /mode` 会重建这套 WAV，
  于是通过 `POST /config/import`、`/config/history/{id}/restore` 或对端下发改到的句子，**配置是新的、
  音频不存在**，关窗只写 `skipped:"no asset"` 且永不自愈（用户 2026-11-15 在第四台首装后报告）。
- 现在：**写入者之后必须重建**——导入在**所有块应用完之后**（同一请求可能刚装好 TTS 凭证）、
  `scope=voice` 的历史恢复在恢复之后、关窗播放缺素材时在后台补。三条路都走同一个
  `spawn_ack_synthesis`，事件如实记录（无 TTS 凭证时是 `wake_ack_synth {error:…}`）。
- 种子里的应答 WAV 只是**缓存**：不是"必须有这一句"的契约，缺了会自愈。
- 设备协议字段不变。

### KWS 驱动随发布走（v0.28，2026-11-15）

```jsonc
// 发布清单 v1（hm-release）新增可选组件；字段缺省 = 旧发布，照常安装
"components": {
  "kws": {                       // 可选；缺省 = 本发布不带驱动，节点保持原样
    "dir": "components/kws",
    "entry": "run.sh",           // 固件 spawn 的启动脚本，必须是 files 之一
    "files": [ { "path": "hm-kws", "size": 34080, "sha256": "…" }, … ]
  }
}
```

- **为什么需要**：驱动不是单个文件，而是 27 MB 运行时（driver＋Debian loader＋glibc/libstdc++＋
  sherpa-onnx＋4 个模型文件＋`run.sh`）。此前只存在于开发机的手搓目录，且 `run.sh` 写死
  `--name <设备号>`，所以它无法作为签名发布的一部分。
- **启动脚本与节点无关**：`run.sh` 从 `/data/local/tmp/hm-direct/device-id` 读节点名（部署本来
  就写这个文件）。因此同一份 bundle 可以同时用于种子与发布。
- **安装**：包内 `components/kws/**` 一律 0755；与当前 active 发布逐字节相同的文件用**硬链接**
  （同文件系统），不会为每次发布多存 27 MB。
- **激活**：把 bundle **逐文件硬链接**进 `/data/local/tmp/hm-kws`（固件 spawn 的路径），
  **`kw.txt` 永不改写**（它是节点配置，由网关在 `kws_deploy` 写入；包里出现 `kw.txt` 会被打包器拒绝）。
  目标发布没有该组件 → **原样不动**并记录；缺文件 → 直接失败。激活回执新增判据 `kws_bundle`。
- **顺序约束（重要）**：旧安装器只认清单声明的条目，会把 `components/kws/**` 当成"未声明"而
  **整包拒绝**。所以带 kws 的发布必须在**新安装器就位之后**才装。安装器仍是手工/串口升级（M8a 设计）。
- 设备协议不变。验收见 [d/kws-driver-release-20261115.md](d/kws-driver-release-20261115.md)。

### 结束会话是最高优先级（v0.29，2026-11-15）

```jsonc
// 行为合同（无新字段）：退下类命中在任何状态下都执行
reflex_retreat { "device": "…", "keyword": "停止播放", "intent": "retreat",
                 "precondition": { "listening": false, "bookmark": false, "idle": true } }
```

- 此前 `退下` 类（KWS 词与转写同一条判定）要求**会话开着或存在书签**，否则
  `reflex_precondition:no_session` 被**静默丢弃**：不播报、不改变状态，只有一条日志。
  设备本地 spotter 常听，于是空闲时对它说 `停止播放/再见` 什么也不会发生，人会一直重复
  （用户 2026-11-15 在 dot2a 现场："我说了好几个来回，都无法关闭"）。
- **用户拍板：结束会话是最高优先级，永不丢弃。** 空闲命中同样执行：停一切、关窗、**播结束语**
  （于是"它到底收没收到"是可听的）。代价是误命中会多说一句结束语——比"说了没反应"更小。
- `precondition` 仍如实上报（含 `idle:true`），便于诊断区分"空闲关闭"与"会话中关闭"。
- 设备协议不变。

### 会话与跟随窗合同 v2（v0.30；**P0–P2 已实现（发布 45–48），#4/#5 于 2026-11-16 补齐**）

> 状态：2026-11-15 只完成审计与方案；随后按阶段实现。**#4「只有回答结束能续窗」与
> #5「KWS 退下本地执行」在 2026-11-16 落地**（见上文 v0.5 与本轮发布），
> 真机验收待用户安排。实现与验收计划见
> [d/interaction-contract-v2-20261115.md](d/interaction-contract-v2-20261115.md)。
> 本节中的字段名以实际 `/mode.session` 为准（`state`/`phase`/`credits`/`tail_deadline_ms`/
> `close_reason`/`opened_by`/`last_denied`）。

```jsonc
// GET /api/v0/mode 每台设备新增（计划字段）
"session": {
  "generation": 7,                 // 每次显式激活 +1，每次关闭 +1
  "phase": "closed|active|tail|closing",
  "opened_by": "keyword|asr_wake|console|protocol_wake",
  "credits": 2,                    // 剩余裸追问额度
  "tail_deadline_ms": 1794712345678,
  "close_reason": "exit|timeout|credits|console",
  "last_denied": { "input_id": "…", "reason": "followup_credits_exhausted" }
}

// 新增管理操作（计划）：结束会话＝停一切＋关窗＋清额度；代不匹配 409
POST /api/v0/session/close  { "device": "dot2-d", "generation": 7 }
```

**目标行为（全部是变更点）**：

1. **退出先于一切普通闸门**：已识别的结束词（反射墙 `retreat` 或 `retreat_words`）在额度、
   media、self-echo、播放闸门、去重之前执行；任何状态、任何额度都可关闭。
2. **只有显式激活能开窗**：KWS 唤醒为主、ASR 唤醒兜底；协议 `Utterance` 永不自开窗；
   迟到的 KWS/协议事件按会话代拒绝（`stale_session_event_rejected`）。
3. **租约**：一次激活＝首问＋`followup_credits` 次裸追问；只有"这次有效请求的答复播完"
   才启动 `follow_window_ms`；追问只花额度、不续期；唤醒应答/垫子/关窗播报/诊断/提醒零开窗权。
4. **关闭是一次带代号的撤销**：清零额度、撤销采集票据、取消在途任务与已安排的 ack/播报、
   停止全部声道、丢弃暂停媒体；重复退出幂等。
5. **KWS 本地退出**（2026-11-16 已实现，见上文 v0.5）：网关命中 `retreat` 先本地 flush
   两个声道并记 `local_exit_flushed`，再上报核心；half 模式照常可用；自听命中不本地执行。
6. **新配置字段**（走同一 history/restore）：`follow_window_enabled`(true)、`follow_window_ms`(20000)、
   `followup_credits`(2)；`retreat_intent` 退役（读作 true）；已存表里 `停止播放` 的非 `retreat`
   映射迁移为 `retreat`，旧备份导入同样迁移。
7. **可解释**：每次接纳/拒绝写 `input_admission{session_generation, input_id, decision, reason, credits}`；
   页面能回答"谁开的窗、还剩几次、何时到期、上次为什么被拒"。

## 三、工具合同 v0（大脑 ↔ 工具层）

```jsonc
// 描述符（注册时声明）
{ "name": "music.play", "version": 1, "side_effect": "mutate", "scope": "room",
  "description": "在房间输出设备上播放已解析的曲目", "input_schema": {...}, "output_schema": {...},
  "requires": { "authorization": "default", "device_capability": "audio_out" } }

// 执行请求（任务运行器发起，携带任务身份）
{ "call_id": "C7", "task_id": "T01", "tool": "music.play",
  "args": { "track_ref": "hotlist:2026-11-07#1", "room": "single" },
  "idempotency_key": "T01-2", "deadline_ms": 8000 }

// 结果（证据分级，业务结果与网络成功分开）
{ "call_id": "C7", "status": "completed|failed|partial|unknown",
  "evidence": "submitted",       // submitted/sent/device_confirmed/unknown（针对副作用）
  "result": { "played_track": "...", "duration_ms": 213000 },
  "error": { "code": "SOURCE_UNAVAILABLE", "retryable": true, "detail": "..." } }
```

- **执行前钩子**（工具层统一做）：任务仍有效、范围/授权核对、参数校验、目标设备具备能力、并发约束。**执行后**以真实回执进入下一轮模型上下文。
- 模型只产出"建议调用"；是否执行由执行层决定。工具不得私自替换用户目标（检索无匹配时如实报告，不得用无关结果顶替）。
- 取消传播：任务取消后，未开始的调用拒绝、进行中的调用尽力取消、已完成动作只记账不撤回。
- 失败注入是 mock 工具的一等能力（M1 验收"按实际结果回应"用）。

### 能力清单＝注册表的渲染（R1h）

工具描述符新增 `service`（能力分组）与 `availability()`（`ready|unconfigured|disabled|hidden`）：

```jsonc
{ "name": "weather.now", "service": "weather", "side_effect": "query",
  "description": "...", "parameters": {...} }
```

- 系统提示词里的「可用能力」块由**注册表＋各服务配置态**渲染：已配置服务的工具列出；已注册但未配置的服务单独列出并说明"还不能用"；**没注册的能力＝不存在**。
- **加/删一个服务只改注册与配置，不改 `agent/prompt.rs` 的文本**（判据：`the_capability_list_is_rendered_not_written`，连文件里出现服务名都会红）。
- `hidden` 只给测试/占位工具（如固定热榜数据集），绝不作为家庭能力出现。

### 诊断：麦克风采集（2026-11-13）

```jsonc
POST /api/v0/devices/{id}/mic/capture   // {seconds: 1..30} → {state:"recording", seconds, path}
GET  /api/v0/devices/{id}/mic/capture   // {state:"idle|recording|done", seconds, requested_seconds, rate, path}
GET  /api/v0/devices/{id}/mic/capture.wav // 16 kHz mono s16 WAV（完成后才有；否则 404）
```

- 录的是**核心收到的 post-AEC mono**，也就是设备侧 spotter 的同一串样本（网关把同一 block 北送并喂 spotter）。
- 一次只允许一路；落盘 `<data_dir>/captures/dot-<ms>.wav`；不重启固件、不占用 Mac 麦。用途＝KWS 命中率的离线测量。
- 控制台暂无按钮（API-only 诊断）；加按钮时随诊断页同批。

### 通用信息搜索与媒体控制（M2.5/M3 定稿）

| 工具 | 副作用 | 参数 | 语义 |
|---|---|---|---|
| `web.search` | query | `query`（≤128 字）、`count`（1–50，模型最多读 8） | 联网检索；结果带 title/url/snippet/summary/date/authority；**只留 http(s) 源**；失败码 `SEARCH_UNCONFIGURED/DISABLED/BAD_ARGS/TIMEOUT/OFFLINE/HTTP/RESPONSE_TOO_LARGE/PARSE/EMPTY`（detail 是给用户听的人话）；未配置/关闭 0 HTTP |
| `music.control` | mutate | `action`（pause/resume/next/stop）、`target?`（media/program） | 控制「当前可控对象」：**明确指代 > 前台媒体 > 节目书签**；两个都只是暂停 → `MUSIC_TARGET_AMBIGUOUS`（反问）；`next` 只走会话队列、不重搜；`resume` 从暂停位置（源不支持 seek 时核心按「重读并丢弃到断点」实现，不假装） |

动作事实由核心渲染（`《歌名》已暂停。/已继续播放。/已停止。`；节目侧为「当前内容…」），模型散文不得改写。

### M1 mock 工具清单

| 工具 | 语义 | 可注入行为 |
|---|---|---|
| `hotlist.query` | 查询模拟热榜（固定数据集＋时间种子） | 延迟、空榜 |
| `music.search` | 按歌名/榜单位次在模拟曲库找候选 | 无匹配、多候选 |
| `music.play` | 向虚拟设备提交播放（走设备协议 play） | 播放失败、提交后超时（unknown） |
| `home.light` | 真实工具，**没有后端就拒**：`backend=none` → `HOME_UNCONFIGURED`（R1j/P0-b；模拟确认已删） | — |
| `time.now` | 本机时钟（日期/星期/时间） | — |
| `weather.now` | 当前天气（温度/体感/湿度/风/降水/现象） | 未配置、关闭、地点未知、超时、离线、HTTP、无数据、过期 |
| `weather.forecast` | 未来 1–7 天（有界，`max_days`） | 同上 |

故事生成不设工具：由模型直接产出，验证上下文纠正能力（"换个短一点的"）。

## 待定项（实现中定稿）

1. ~~设备协议传输与编码~~ **已定（G1）**：TCP＋LDJSON，核心监听；见 §一 定稿清单。
2. 管理API鉴权模型（开发期本机 token；NAS 部署时按暴露面复核）。
3. ~~`content_ref` 音频传递方式~~ **已定（G1）**：play 帧内 base64 WAV；PCM 流式为 v1。
4. 工具 schema 的具体字段（M1 已随测试用例定稿；后续工具按批补充）。
5. ~~数据库选型~~ **已定（M1）**：SQLite（rusqlite bundled）。

## D3 管理与模型会话合同（2026-11-15；本地实现，未发布）

本节记录 D2 后的本地实现形状，覆盖上文 v0.30 **目标示例**中的枚举占位，不表示设备发送端已升级。

- `POST /api/v0/session/close {device, generation?}`：已知设备；未知设备 404；generation 不匹配
  409 且零关闭副作用。成功返回 `{session, closed}`（可含 `confirmed`）。关闭使用 D2 同一执行器：
  显式退出撤代/撤票/取消在途工作/停全部输出/清历史、书签与媒体；timeout/SingleRequest 的保留语义不改。
  已显式关闭后不重复撤代；timeout 后的显式关闭仍执行显式退出清理。
- `/mode.devices[].session`：`generation`、`phase: closed|active|tail|closing`、`opened_by`、
  `credits`、`tail_deadline_ms`、`close_reason: explicit_exit|timeout|single_request|null`、`last_denied`。
  兼容保留原 `state: idle|listening|closing` 和 `since_ms`。active 包括首问等待及生产/播放；
  tail 仅由该请求完整有效答复结束产生。首问等待超时不伪装 answer tail；关闭/active 的 tail deadline 为 null。
  开启来源含 `keyword/asr_wake/protocol_wake/text_wake/explicit_activation`；拒绝带 reason、at_ms 和重新唤醒提示。
- `session.close`：mutate，scope=device，参数仅 `{confirm?: bool}`。用 task 的原始 device session
  与**当前 request 的取消 scope**核验；不选默认音箱，不借后来请求/新代。缺省/false 立即退出；true
  首次由核心反问「要结束对话吗？」；仅紧邻下一有效请求再次调用才执行。同轮不能自确认，重新激活或
  下一请求改问后 pending 失效。实际关闭记录 `session_retreat via=model` 和 `session_close_tool {confirmed:true}`；
  待确认记 confirmed=false。关闭执行器在清空后的历史写完整工具调用/回执对，并以关闭代许可播核心事实句
  「会话已结束。」＋配置的 retreat_ack。旧任务保持取消；散文不播、不入历史，也不再向模型索要结束语。
  这里确认的是核心会话已撤销，设备实际停止仍以设备回执为据。
- 提示词共享配置：`persona_prompt: String` 默认空、`system_prompt_override: Option<String>` 默认 null，
  均最多 4000 个 Unicode 字符。PUT `/mode/{device}` 可随其余配置保存；无设备也可用
  `PUT /api/v0/prompts {persona_prompt?, system_prompt_override?}`。读回来自 `/mode`；history/restore/backup/fleet
  使用同一 voice 快照。缺字段保留，persona 空串清空；override 空串/空白/null 清除，完整快照亦同；旧快照默认空/null。
  组装固定为骨架（可被 override 替换）→persona→注册表能力清单→渠道条款→BM5/语义退出职责。
  override 无法从组装结构中删除后三项。设置页提供多行 persona、高级折叠 override、警告、当前生效态与下一轮生效提示。
- `followup_credits` 为 0–5；首问免费。配置 0 时首问完整答复/无音频回合结束按 SingleRequest 关闭，
  普通裸追问拒绝并给出重新唤醒提示；退出仍先于普通准入。控制台「跟随窗」沿用配置入口，
  「会话与追问」只展示状态与关闭操作。共享状态轮询 500 ms，关闭操作后立即刷新。

语义选择准确率需要真实模型另验；FakeLlm 只证明工具执行与提示词组装。页面一秒反映尚未浏览器实测，
当前浏览器受审批策略与本地沙箱 Mach port 权限限制。设备/网关发送端、真实声学与发布不在本轮范围。
