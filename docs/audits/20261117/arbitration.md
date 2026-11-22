# Home Mate 多房间仲裁专项审计

审计基线：Core `c3ed6c6c8b5d367a44f10ee103dd0c22865f3b4d`（运行代码 `05da212`），Gateway 以 serial 69 已部署事实为既有证据。审计只读，没有读取凭证、连接设备、改变配置、发布或修改 GitHub。日期 2026-11-17。

## 目的与已存在的良好设计

产品应当让同一句家庭唤醒只由合适的一台音箱响应，同时允许不同房间的独立请求并发，停止/退下不等待跨房选举。当前实现已有无中心的对称 claim 交换、200 ms 交换预算、独立网络任务、明确 unavailable 降级、健康/SNR/clipping/profile 参与证据筛选、声学参数逐台来源、部分真实管理 API 集成测试。这些值得保留。

本次完整阅读了 `arbitration.rs`、`sovereignty.rs`，并沿生产调用追踪 `voice/reflex.rs`、`voice/pump_turn.rs`、`voice/pump.rs`、`voice/turn.rs`、`voice/mod.rs`、`voice/participation.rs`、`fleet.rs`、`api/services/system.rs`、`gateway/frames.rs`/`mod.rs`、相关 Gateway near-end 发送部分；核对 `tests/wake_arbitration.rs` 和 `voice/v2_tests.rs`/`tests.rs`/`d2_tests.rs`。未以旧审计文档替代代码审查。

## 结论

W2 的数据结构与 UI 已交付，但“跨核心一致的房间所有权”和“完整候选集上的单一决策”尚未闭环。几个单测在同一 Core 里手动创建多个设备/所有权记录，与现场“一台一个 Core”的拓扑不同，掩盖了关键缺口。以下结论不是 W0 校准误差推测，而是能够定位到代码分支的逻辑或接线问题。

## ARB-01 — P1：不同房间的独立请求仍被固定时间窗合并

- 证据：`crates/hm-core/src/arbitration.rs:232–244` 的 `decide_around` 仅比较 `heard_ms` 差值是否 ≤2400 ms，完全不使用 room、独立事件身份或语音包络；`voice/reflex.rs:395–399,471–505` 据此只有一个胜者获得激活。
- 触发：客厅用户 t=10000 ms、卧室用户 t=11000 ms 各自说唤醒词，均有正常近端证据。两条请求都被归入同组；其中一间失去自己的请求。
- 已执行实际源码复现：两个不同 room 的 `Claim`，相差 1000 ms，分别以自身为 anchor 调用 `decide_around`，结果均为 `a`；卧室 `b` 被吞。
- 测试缺口：`voice/v2_tests.rs:245–276` 和 `sovereignty.rs` 的 concurrent 测试直接在一个 `RoomSessionDomain` 插入两个 owner，且使用 3000 ms 间隔，并未通过两个 Core 的真实唤醒/仲裁链。
- 修复方向：先明确跨节点的 utterance/domain 身份与独立房间请求判据，再在分组阶段区分；room 标签不能简单作为“每屋都赢”，否则同一句串音会多答。W0 测量用于选择相关性/时间容差，但单纯调大/调小 2400 无法同时满足这两个目标。
- 置信度：高；真实源码确定性复现。

## ARB-02 — P1：纯 ASR 唤醒没有加入对等选举，多台 KWS 同时漏检时均可响应

- 证据：`voice/pump_turn.rs:827–866` 从空闲转写调用 `admit_asr_wake` 后直接激活；`voice/reflex.rs:233–286` 仅检查已存在的本机 verdict、recent foreign claims 和本机 reflex 去重，没有 `arbiter.begin/exchange` 或广播。只有 Keyword wake 的 `reflex.rs:672–693` 启动选举。
- 触发：两个音箱 KWS 都漏检，但 proactive ASR 都正确转写出相同唤醒词。没有任何 KWS claim，双方 foreign 集合为空，双方都返回 true、各自激活并回答。即使 `wake_arbitration=Enforce` 也如此。
- 附带时间语义错误：当有 foreign claim 时，`reflex.rs:256` 将 ASR 完成时的 `now_ms()` 当 `heard_ms`，没有使用音段采集时间。ASR 慢于 2.4 秒时，同一句已被 peer 赢得的输入会被过滤成另一句。
- 测试缺口：已测“ASR 被已有 KWS verdict 阻止”，未测“全体只有 ASR 的两个真实 Core”。
- 修复方向：所有唤醒入口共用相同 claim/round/commit 入口，携带采集时间和输入身份；单纯查询旧 verdict 不能取代参与选举。
- 置信度：高；完整生产静态路径，无现场重放。

## ARB-03 — P1：被动近端候选采用两两比较并自行宣告获胜，可产生多个 winner

- 证据：`arbitration.rs:651–665` 无本地 round 时只用 `[passive, incoming]` 比较，胜出立即写 `last_won_at`，仅回复自己的 claim；不建立完整 round，不主动交换候选，不接收最终胜者。`voice/pump_turn.rs:337–342` 将 `won_recently` 作为允许被动 ASR 的权限依据。
- 触发：A 的 KWS 命中但声压 -35 dB；B/C KWS 漏检、各有 -20/-22 dB 被动近端。A 同时询问 B/C，B/C 分别只跟 A 比，两者均自认获胜；A 的完整决策也不会发送回 B/C。后续 B/C 的 ASR 路径仍只看到 A，双方都可能激活。
- 已执行实际源码复现：两份独立 `Arbiter` 各登记 passive，再各接收同一 KWS claim，输出 `b_won=true c_won=true`。
- 测试缺口：`arbitration.rs:1168–1181` 只测试一个 passive 与一个 KWS，不覆盖三个节点和最终裁决一致性。
- 修复方向：被动证据只能是候选，不能凭两两比较直接授予获胜资格；进入同一轮完整候选集的决策/确认，并处理胜者改变时的撤销。
- 置信度：高；真实源码复现多个 won_recently 资格。未端到端复现两台实际出声；双答仍依赖各自 ASR 文本、后续唤醒准入及时间条件。参见 [独立复核](verification.md)。

## ARB-04 — P1：房间 owner 注册表没有跨 Core 同步，现场非 owner 退下保护不成立

- 证据：每个 `VoicePipeline` 在 `voice/mod.rs:1297–1298` 新建独立 `RoomSessionDomain`；生产写入 owner 的入口仅 `voice/pump_turn.rs:1401–1408`，由本机获胜/Keyword 激活调用。`api/services/system.rs:844–852` 接收 wake 仅进入 Arbiter；`:883–895` 接收 playback 仅写播放缓存，均不更新 owner。`sovereignty.rs:255–266` 在看不到别的 owner 时允许退下。
- 触发：A 是正在对话的真实 owner，B/Core 没有收到/保存 A 的 SessionOwner；B 听到串音“退下吧”，`can_retreat` 不能命中 DeniedNotOwner。初始 idle 也允许 explicit close（`participation.rs:426–446`）；若退下提示音缓存可用，`turn.rs:162–164` 可发声，和预期“只有 owner farewell”相反。
- 已执行实际源码复现：独立空白 B domain 对不属于自己的房间退下返回 `Allowed`；本地 A 注册不会跨实例传播。
- 测试缺口：`voice/v2_tests.rs:252–265` 将 A/B/C 手动放在同一个 pipe 并手工塞入 A/C owner，只有这个测试拓扑能看到别人 owner。
- 修复方向：将所有权公告、续租、释放、断线过期变成实际 fleet 协议，或把退下授权绑定当前输入所属的有效 arbitration winner/session，并保持单机离线行为明确。
- 置信度：高；生产调用与实例隔离均确定。

## ARB-05 — P1：迟到 loser 的撤销过晚且有音频后跳过，单一应答约束仍可破坏

- 证据：`reflex.rs:99–115` 是等待整个 2400 ms 后才检查一次，不是在 late claim 到达时重判；它在 200 ms 初次交换之后才被启动（`:471–483`）。同时本机已经激活，ASR recently_won 路径跳过 foreign 重判（`:244–272`）。到最终发现 loser 时，`:170–187` 仅在 `outputs.is_empty()` 且 `live_program.is_none()` 才撤销；已有播放就只记 lost，不停止。
- 触发：A 初次 200 ms 判赢，B 在 500 ms 报更强 claim 并判赢；A 在约 2.6 秒终审前已经通过快速 ASR/LLM/TTS 提交音频。终审时 A 有 output，不撤销，A/B 都继续答。
- 测试缺口：`tests/wake_arbitration.rs:464` 和 `:757` 测了迟到 claim 后空窗关闭，没有在裁决前推进真实/模拟 turn 到音频或工具执行边界。
- 修复方向：provisional 与 committed 所有权明确区分，音频/副作用提交前验证有效轮次；late claim 到来立即触发重判及与该轮绑定的撤销，不能用“还没任何 output”作为唯一撤销条件。
- 置信度：高，静态可达竞态；未在现场制造双答。

## ARB-06 — P2：正常音乐播放永远不发送 active media notice

- 证据：`voice/mod.rs:818–827` 将 activity `playing` 映射成 presentation `music`；`voice/pump.rs:703–706` 却用 `"playing" => "media"` 匹配 presentation state。正常媒体状态进 `_ => ""`，后续 `:720–721` 的 active media notice 分支不可达。
- 影响：声明已实现的跨房媒体播放 metadata 抑制链没有实际生产者；另一房的音乐串音缺少该证据。
- 测试缺口：`voice/v2_tests.rs:416–455` 仅手工注入 voice 输出测试 speaking，没测 music 输出。
- 修复方向：广播来源用真实声道播放状态，不依赖 LED/presentation 字符串；至少加正常 media 开始/结束的 fleet 接收端行为测试。
- 置信度：高，确定的枚举字符串接线错误。

## ARB-07 — P2：15 秒 notice 只在状态切换时发送，长播放会自行失去保护

- 证据：`pump.rs:708–726` 仅 active_channel 变化时产生 notice；`pump_turn.rs:1313–1314` 固定 15000 ms TTL；`sovereignty.rs:79–82` 超时视作无效。持续 speaking/playing 不更新发送时间。并且 preview 只在首次状态转换选择一条 output、截取前 64 字（`pump_turn.rs:1312`），后续句子不刷新。
- 触发：一条连续故事/语音超过 15 秒，后续句子包含唤醒词/退下词；设备仍在播，peer 却已不认为 notice 有效。
- 已执行实际源码复现：相同 active notice 于 t+15001 ms 不再参与抑制。
- 修复方向：明确播放 session/sequence，并有有界续租/恢复快照；文本证据与当前播放段保持同步。续租频率按秒级状态协议预算设置，不需要音频帧级 HTTP。
- 置信度：高；TTL 复现 + 生产发送条件确认。

## ARB-08 — P1：远端文本包含关键词就无条件封锁本机真实近端命令

- 证据：`sovereignty.rs:300–308` 在 preview 包含 phrase 时直接返回，完全忽略 `self_heard=false`、`has_strong_near_end=true`；这些输入仅用于下一个分支。`voice/reflex.rs:562–565` 在 intent 分类前调用 bleed 检查，所以 Wake、Retreat、Stop 等所有 Keyword 都受影响。
- 触发：A 正在讲解“你可以说你好高达/退下吧”；B 房用户真的近距离说相同唤醒或退下词，即使 B 有强近端证据且不是 self_heard，也被当串音，直到通知停止或过期。
- 已执行实际源码复现：`suppress_bleed_for_phrase("b", "你好高达", false, true, now)` 返回 Some。
- 修复方向：文字相同只能是可疑串音证据，不能覆盖可靠本地近端/当前会话控制；将播放段/声学判据/owner 和本机命令优先级一起定义，明确真实“退下/停”的覆盖行为。
- 置信度：高，真实源码确定性复现。

## ARB-09 — P2：迟到 loser 的 RoomSessionDomain 与 recent win 没有撤销

- 证据：初次 provisional win 在 `reflex.rs:472–473` 先写 `record_win` 和 `claim_sovereignty`。迟到 loser 仅改 `last_arbitration` 并调用 `retreat_session_silent`；`turn.rs:34–62` 没有 `sovereignty.close_voice_session`，没有清 `Arbiter.last_won_at`。`has_active_voice_window` 又把 15 秒 lease 当 voice-active（`sovereignty.rs:38–39`）。
- 影响：已关闭本机实际参与窗的 loser，仍有 room sticky bonus 和被动 ASR 资格的另一份状态；新的很近的请求可能按已丢失的所有权比较，诊断呈现亦可能矛盾。
- 修复方向：将 arbitration 轮次的 owner/participation/recent-win 统一撤销；对迟到消息绑定 generation，避免独立布尔/计时器遗留。
- 置信度：高；相关写入、撤销遗漏和消费点均明确。

## 已执行验证及范围

在 `/tmp/hm-arbitration-repro/` 建立极小 Rust harness，通过 `#[path]` 直接编译当前仓库原始 `arbitration.rs` 和 `sovereignty.rs`；只 stub 了 FleetService/Store/ID 外部依赖，未复制或改写政策算法，没有网络、设备或真实存储访问。

运行命令：

```sh
CARGO_TARGET_DIR=/tmp/hm-arbitration-repro-target cargo run --offline --manifest-path /tmp/hm-arbitration-repro/Cargo.toml --quiet
```

结果：

```text
different_room_requests_1000ms_apart: a_winner=a b_winner=a
one_kws_two_passive_nodes: b_won=true c_won=true
real_near_end_wake_blocked_by_matching_peer_text=true
ongoing_playback_notice_live_after_15s=false
other_core_owner_unseen_retreat=Allowed
```

这些证明局部政策/状态实际行为，不冒充完整物理声学验收。root 负责整体构建/测试，未重复运行。

## 明确保留的未测量项

- W0 的 NTP 时钟偏差、lag_ms 分布、真实房间 SNR/麦克风增益仍待测，不能凭代码推断现场误唤醒率、获胜准确率或“已解决回声”。
- 断网时本地回答是当前明确策略，网络分区产生各自响应是产品降级取舍；本报告没有把该已知策略单独当缺陷。
- Gateway KWS RMS 与 Core 双重 mic offset 问题已交给 audio 审计者负责，避免重计。
- ContextSession 的 240 秒 timer 未驱动真实对话历史过期，由 interaction 审计者负责，避免重计。


本次统一基线、优先级与验证边界见 [审计总报告](README.md)；隔离复现源码/输出见 [证据附录](evidence.md)。
