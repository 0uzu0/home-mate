# 重点审计发现的独立证据复核

日期：2026-11-17。复核对象：Core `c3ed6c6c8b5d367a44f10ee103dd0c22865f3b4d`。角色：独立只读复核；未改仓库源码、设备、配置或凭据，未调用线上服务。读取项目边界后，复核 architecture、arbitration、interaction 三份审计报告及下述真实生产调用链；复用已有隔离构建，未重新跑全量构建。

`CONFIRMED` 表示所述代码行为可确认，不表示五台现场均已发生。`NEEDS_QUALIFICATION` 表示核心问题成立，但必须保留限定语，避免把局部政策复现写成完整物理验收。六项中没有发现应判为 `NOT_SUPPORTED` 的核心机制。

| 发现 | 结论 | 复核后的准确说法 | 必须保留的条件 |
|---|---|---|---|
| ARB-01 两个房间的独立请求相差 1 秒被合并 | CONFIRMED | 当前聚类只按 `heard_ms` 的 2400ms 距离，没有独立话语的分离证据；两份不同房间请求可以选出同一赢家，另一请求被压制。 | 需要启用 enforce、节点可交换 claim，并有两份时间足够接近的 claim。不是所有两房同时讲话必然丢失，也不能直接用 room 不同来分割：同一个人的串音本来就会出现在不同 room。 |
| ARB-03 两个 passive 节点自选赢家 | NEEDS_QUALIFICATION | 两个无本地 KWS round 的 passive 节点分别胜过同一较弱主动节点，都会写入 `last_won_at`，获得被动 ASR 资格；缺少共享候选集合或最终 commit。 | 已运行复现证明的是两个 `won_recently=true`，不是两台扬声器实际出声。双响应还要求两台各自产生能进入请求的 ASR 文本、满足后续唤醒/时序等条件。 |
| I-1 跟随窗到期丢掉已经开始的输入 | CONFIRMED | 在窗内开始、但窗后结束或返回 ASR 的无新唤醒词请求，可能因空闲超时撤销 lease，或在最终准入按完成时刻检查而被丢弃。 | 跟随窗启用、有剩余资格，且没有仍在运行的请求/有效输出把窗口保持打开。默认 `follow_window_enabled=false`；未读取现场设置。9 秒只是当前示例，问题适用于任意配置的有限窗口。 |
| I-2 音乐暂停/停止虚假确认 | CONFIRMED | 断连导致 oneshot sender 被丢弃、发送错误变成 None、或 stop stage 为失败/未知时，音乐控制路径仍可以生成 `DeviceConfirmed`。 | 已运行的是嵌套 Result 表达式复现与真实代码调用链追踪；没有在设备上制造断连，也没有证明错误触发后一定继续出声。缺陷在“确认事实”强于证据。 |
| ARCH-01 factory reset 留下新记忆表 | CONFIRMED | 清理列表遗漏 Agent.md、USER.md、共享 profile cache 和共享 outbox；前几项仍是后续提示词输入，outbox 在共享功能再启用时仍有处理资格。 | 用户/设备 scope 仍需匹配才进入相应提示词；不应说所有私有记录会无条件注入所有人。没有把明确可能保留的 config_history 计入这项。 |
| ARCH-03 fleet 复制每机声学参数 | NEEDS_QUALIFICATION | 全量 `mode` 导出/导入带走源机 `vad` 和 `media_trim_db`；VAD 会对现有 MicProcessor 立即应用，trim 被持久化并在 Core 下次启动时投影到 hub。 | 不要把两者都描述为立即生效；不要把本项和 UnitAcousticProfile 字段未接线的问题合并。前者影响实际控制，后者是另一个接口与生效状态缺陷。 |

## 逐项调用链

### ARB-01：时间聚类无法识别两个独立请求

- `crates/hm-core/src/arbitration.rs:232` 的 `decide_around` 仅按 `abs(c.heard_ms - local.heard_ms) <= COALESCE_MS` 收集；`COALESCE_MS` 为 2400（第 62 行）。
- `arbitration.rs:675` 的 inbound 接收也只有同样的时间边界；`arbitration.rs:738` 的 exchange 过滤亦如此。
- `crates/hm-core/src/voice/reflex.rs:395` 真实 KWS 路径消费该政策；enforce 下 loser 在第 498 行分支保持静默。
- 独立重跑现成 harness：两份 `t=10000/-20dB/room=living` 与 `t=11000/-25dB/room=bedroom` claim，双方决定均为 a。
- 此处根因是“是否同一话语”和“谁离声源近”用了同一时间候选集合。需要独立话语证据设计；简单按 room 排除会破坏跨房串音仲裁。

### ARB-03：passive 资格不是全局选举结果

- `voice/pump.rs:1494` 接收 `NearEndPresence`，更新新鲜 MicLevel，并在第 1532 行记录 passive claim。
- `arbitration.rs:654` 在没有本地 round 时，只比较本机 passive 与 incoming claim；第 661 行直接设置本机 `last_won_at`，回复仅携带本机 claim。
- 若 B/C 各自比 A 强，B/C 都能写入 win；B 没有得到 C 的 claim，C 也没有得到 B 的 claim。
- `voice/pump_turn.rs:337` 把 recent win 用于 proactive ASR 关闭时的转写资格。后续 `voice/reflex.rs:233` 仍会执行 ASR 唤醒准入，但它不发起全体 passive 的最终选举，只看本机已有 verdict/foreign claims；因此不存在可证明唯一性的最终提交边界。
- 独立重跑得到 `b_won=true c_won=true`。应表述为“唯一赢家约束缺失，可形成多个候选同时准入”，并把实际双出声列为待完成的集成/现场验证，不能声称该 harness 已完成后者。

### I-1：输入尚未形成 request 时不保持窗口

- `voice/pump_turn.rs:244` 的 SpeechStarted 仅计数、记录 provenance、推送助手状态；不更新 last_activity，不建立 request，不保留已准入捕获。
- `voice/participation.rs:475` 的 session_open 只认 Listening、voice_live、request_running 和时间差。
- `voice/pump.rs:309` 的定时清扫调用 `voice/mod.rs:779` 的 timeout；除 media_live 和 session_open 外，没有 VAD in-speech / ASR-inflight 保护。
- `voice/participation.rs:437` 超时关闭会增加 revision/generation、撤销 capture；后续 `voice/pump_turn.rs:513` 拒绝旧 origin。
- 即使尚未发生清扫，最终 `voice/pump_turn.rs:639` 也用 ASR 返回时刻判断 in_conversation；`session_covers` 的捕获区间判断只在更早的 privacy dispatch 使用。
- 本项由完整静态链确认，本复核没有构造新的 VAD/延迟 ASR 集成测试。修复需要保护窗内已开始的有界输入，同时保留 explicit stop/退下和连接换代的撤销权。

### I-2：停止证据在 Result 与无类型回执中丢失

- `tool/music/session.rs:517`、`:532` 对 `timeout(..., rx).await.is_ok()` 的判断只检查外层超时；`Ok(Err(RecvError))` 被判成功。
- `gateway/mod.rs:702` 在断连时清理 stop_waiters，正好产生该 inner error。
- `tool/music/session.rs:474` 和 `:532` 吞掉 send error；之后 None 分支返回 true。
- `gateway/outbound.rs:418` 返回 `Receiver<()>`，不能保存 flush stage；`gateway/mod.rs:878` 注释明确承认 unknown/flush_failed 不确认静默，但仍无条件 resolve waiter。
- 最终 `tool/music/session.rs:696`、`:780` 生成“设备已停稳”与 DeviceConfirmed。
- 独立重跑 actual-built-library harness：丢弃 sender 后实际结果为 `Ok(Err(_))`，`.is_ok()` 为 true。验证的是表达式和生产断连路径的连接，不是现场停止时延。

### ARCH-01：reset 遗漏仍在使用的存储

- `store.rs:336` 全部 DELETE 列表未出现 `agent_md_revisions`、`user_md_entries`、`shared_profile_cache`、`shared_memory_outbox`；即使 include_secrets=true 也只增加 secrets 删除。
- `api/services/mod.rs:271` 调用该方法，随后仅重置 voice settings。
- `config/mod.rs:165` 每轮 prompt preferences 仍读取最新 Agent.md。
- `agent/mod.rs:573`、`:580` 仍调用共享缓存与 USER.md 的 scoped prompt renderer。
- `shared_memory.rs:228` 查询本地 cache，不以共享功能 enabled 作为读屏障；`user_md.rs:223` 从 active entries 选择当前 scope。
- 阅读已有 exact-SQL 复现结果，四个遗漏表各保留 1 行；本复核未重跑 SQL 脚本。源码遗漏与后续读取消费均已独立确认。

### ARCH-03：真正生效的参数从 mode 绕过 profile 保护

- `api/services/system.rs:211` 导出 `state.voice.settings_snapshot()`；`:916` 导出后在 `:919` 调 `strip_node_local`。
- `fleet.rs:588` 只删 unit_acoustic_profiles、mode.wake_words 和 reflex wake words。
- `api/services/system.rs:415` 接收 fleet mode 时只补回接收方 wake_words；`:435` restore 整份设置。
- `voice/mode.rs:782` 替换运行配置并持久化；`:789` 把导入的 vad.start_db/end_db 写入现有 processors。
- 同一 restore 方法没有 `hub.set_media_trim_db`；`voice/mod.rs:1287` 在启动时才从持久化设置初始化 trim。
- 本复核未进行任何真实 fleet push，也未读取各设备的校准数值。

## 本轮实际运行

复用先前审计生成、直接引用当前源码或实际构建库的隔离二进制，无网络和设备访问：

```text
/tmp/hm-arbitration-repro-target/debug/hm-arbitration-repro
different_room_requests_1000ms_apart: a_winner=a b_winner=a
one_kws_two_passive_nodes: b_won=true c_won=true
real_near_end_wake_blocked_by_matching_peer_text=true
ongoing_playback_notice_live_after_15s=false
other_core_owner_unseen_retreat=Allowed

/tmp/hm-audit-interaction-repro
PASS context reproduction: expired rsess replaced, old dev session history still replayed
PASS stop receipt expression: dropped sender becomes confirmed=true with await.is_ok()
```

上面后几项属于原审计 harness 一同输出，不扩张本复核的六项范围。执行退出码均为 0。未执行设备验收、现场采音或新的全量测试。本报告不作为关闭 issue、合并功能 PR 或标记里程碑完成的验收结论。


本次统一基线、优先级与验证边界见 [审计总报告](README.md)；隔离复现源码/输出见 [证据附录](evidence.md)。
