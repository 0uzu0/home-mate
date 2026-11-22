# M1 真实模型验证记录

日期：2026-11-07。环境：macOS 开发机，`hm-core` v0.0.1，真实 DeepSeek（deepseek-chat，`~/.hermes/.env` 凭证经 `data/env.local` 注入），**模拟家庭工具**（无真实设备副作用）。确定性合同另由 31 个自动化测试覆盖（`cargo test`，FakeLlm）。

| # | 验收场景（用户 2026-11-07 口述标准） | 结果 | 证据摘要 |
|---|---|---|---|
| 1 | 讲睡前故事 →“换个短一点的”保持语境 | ✅ | 第一版 315 字（小刺猬点点），短版 147 字，主角与情节延续（蜗牛/薄荷丛/月亮），仅说“换个短一点的”无其他提示 |
| 2 | 新说法点热榜歌（“来一首现在榜上最火的歌”），查榜→播放→按实际回执回答 | ✅ | 工具链 hotlist.query → music.play（模型直接用榜单内 track_id，合法）；回执 device_confirmed；回答“正在播放今天榜单第一的《晚风信笺》……”与回执一致 |
| 3 | 临时纠正：“不要这首，换第二首” | ✅ | 换到 hotlist#2《光年之外的光》并播放成功；回答明确“已切换到第二首” |
| 4 | 播放失败如实回应（HM_INJECT_PLAY_FAIL=t01） | ✅ | music.play 回执 failed/submitted（PLAY_FAILED：输出设备忙）；回答“播放失败了，输出设备忙。要我重试一次吗？”——未假装成功 |
| 5 | 中途叫停（HM_INJECT_HOTLIST_DELAY_MS=5000，工具执行中取消） | ✅ | cancel 立即受理；工具迟到结果标记 `late: true` 仅记账；Cancelled 事件 + turn_end status=cancelled；任务 notes：“late result from hotlist.query recorded only”“no further model call” |

补充说明：

- 场景 2 中模型省略 music.search 直接由榜单 track_id 播放，属合同允许的自由组合（提示词只说明能力，不规定流程）。
- 场景 4 的“重试一次吗”是模型基于可重试错误码的主动澄清；是否重试由用户下一句决定。
- 修复过的真实链路 bug（Fake 测不出）：① 工具名带点不符合 OpenAI 函数名规范 → 内部名/线上名双向映射（`llm::to_wire_tool_name`）；② assistant tool_calls 消息需 `{"type":"function","function":{…}}` 嵌套；③ 错误路径未收尾导致 task/trace 停留 running。
- 本记录只覆盖文本路径与模拟工具；“播放 completed”为模拟 sink 回执，**不代表有声输出**；语音、设备网关、真实 dot2-d 均未接入。

复现方式：`./target/debug/hm-core`（注入变量见 `crates/hm-core/src/main.rs` 注释），`python3 scripts/verify_m1.py`。
