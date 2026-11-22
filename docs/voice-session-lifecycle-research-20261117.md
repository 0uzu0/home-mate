# 主流语音助手「多轮会话生命周期」公开技术做法调研

> 调研时间：2026-11-17。对象：Amazon Alexa、Google Assistant / Gemini for Home、小米小爱同学、Apple Siri。
> 标注约定：**【官方】**＝厂商帮助页/开发者文档/官方博客/官方申请专利；**【第三方】**＝媒体、评测、开发者实测、社区复现。查不到确切数字的，写「未找到公开数据」，不做推测。

---

## 0. 一句话结论

四家的「跟随窗」都在 **5–15 秒** 量级，主流落点 **8–10 秒**；**没有任何一家设置「免唤醒追问次数」额度**，全部靠时间窗；**没有任何一家在静默超时时播报结束语**，只靠灯效/提示音，或用「谢谢/停止/我完了」这类词显式结束；多设备仲裁全部是「就近优先」，但实现路径分两派（云端仲裁 vs 设备间蓝牙协商），且**同时响应至今没有被完全解决**。

更关键的趋势：Alexa+、Siri AI、Gemini Live 都在把「会话」从「一段会超时的麦克风开启期」改成「**持久的上下文线程 + 短暂的语音窗口**」。这对你的「会话总寿命」设计有直接影响（见第 6 节）。

---

## 1. Amazon Alexa

### 1.1 Follow Up Mode（免唤醒追问）

| 项目 | 内容 | 性质 |
|---|---|---|
| 机制名 | Follow Up Mode | 官方 |
| 静默窗口 | **约 8 秒**（Alexa+：「up to about 8 seconds after Alexa's response」） | **官方** |
| 经典 Alexa 窗口 | 官方只写「a short period of time after Alexa responds」；**未给出秒数** | 官方（无数字） |
| 2018 发布时窗口 | **5 秒**（The Verge、TechCrunch、MacRumors 均引 CNET 报道） | 第三方（媒体） |
| 触发条件 | 一次**语音发起的唤醒词**之后；每次 Alexa 回答结束即开窗 | 官方 |
| 重置条件 | 用户说话 → 进入新一轮 → 回答结束再开新窗（滚动窗口） | 官方（行为描述） |
| 关闭条件 | 静默超时；说「thank you」「stop」可强制结束（The Verge 引 Amazon 说法） | 第三方 |
| 次数额度 | **未找到公开数据**（无「几次」概念；时间窗内可连续多轮） | — |
| 结束语 | **未找到官方结束语**；超时静默关闭，只有灯环/屏幕指示消失 | — |
| 可配置 | 可按设备/App 开关；窗口长度**不可配置** | 官方 |
| 不生效场景 | Alexa skills、智能家居命令、Routines、按键触发、上轮未被响应、蓝牙配对时；Alexa+ 还要求唤醒词是语音发起 | **官方** |

- 官方 Alexa+ 帮助页（含 8 秒与不生效场景）：https://www.amazon.com/gp/help/customer/display.html?nodeId=TtQI6drnxX9fpAYrc1
- 官方 Follow Up Mode 说明（无秒数，含隐私说明「响应后会短暂上云」）：https://www.amazon.com/gp/help/customer/display.html?nodeId=T9HzhoGwqM0QwYOe64
- 官方「什么是 Follow Up Mode」：https://www.amazon.com/gp/help/customer/display.html?nodeId=GX7EJ9WHEPYBV94J
- 5 秒（第三方，2018 发布报道）：https://www.theverge.com/2018/3/9/17101330/amazon-alexa-follow-up-mode-back-to-back-requests

### 1.2 Skill Session（开发者视角的会话生命周期）

| 项目 | 内容 | 性质 |
|---|---|---|
| 开 session | 用户调用 skill，返回 `shouldEndSession=false` 即保持 session 并开麦 | 官方 |
| 单轮静默 | 「if a few seconds elapse without a response, Alexa closes the microphone」 | 官方（无秒数） |
| Reprompt | 若返回了 reprompt，Alexa 重问一次并再开麦「a few more seconds」；仍无响应则 session 结束 | 官方 |
| 实测时长 | **约 8 秒 + reprompt 后再约 8 秒**，随后收到 `SessionEndedRequest` | 第三方（开发者实测） |
| 结束原因 | `USER_INITIATED` / `ERROR` / `EXCEEDED_MAX_REPROMPTS` | **官方** |
| 会话总寿命 | **未找到官方上限**；没有「整个 session 最多活 N 分钟」的定义，只有每轮窗口 | — |
| 带屏设备 | 有屏且返回视觉内容时，麦克风关闭状态下 session 可再保持 **约 30 秒**（`shouldEndSession=false` 时 20–30 秒） | **官方** |

- 官方 session 文档：https://developer.amazon.com/en-US/docs/alexa/custom-skills/manage-skill-session-and-session-attributes.html
- 官方 `SessionEndedRequest`（含 `EXCEEDED_MAX_REPROMPTS` 定义）：https://developer.amazon.com/en-US/docs/alexa/custom-skills/request-types-reference.html
- 8 秒×2（第三方实测）：https://github.com/alexa/alexa-skills-kit-sdk-for-nodejs/issues/223 ；https://stackoverflow.com/questions/47973232/alexa-sdk-response-listenreprompt-speech

### 1.3 ESP 多设备仲裁

| 项目 | 内容 | 性质 |
|---|---|---|
| 机制名 | ESP（Echo Spatial Perception），2018 起**全部搬到云端**，第三方 Alexa 设备无需端侧改动 | **官方** |
| 判据 | 判断哪台设备离用户最近，由该设备应答 | 官方 |
| 实现 | 端上算特征 embedding → 云端融合 → 端到端模型输出「最近设备」；训练用房间仿真（2–5 台设备、混响、噪声源） | **官方论文（Amazon Alexa AI）** |
| 用户可配置 | 只能选唤醒词/关闭某设备麦克风；**没有「指定主设备」开关** | 官方（功能面） |
| 已知问题 | 混响、电视噪声下单纯比能量会错；论文即为解决此问题 | 官方论文 |

- 官方博客（云 ESP）：https://developer.amazon.com/en-US/blogs/alexa/post/042be85c-5a62-4c55-a18d-d7a82cf394df/esp-moves-to-the-cloud-for-alexa-enabled-device
- 官方论文：https://arxiv.org/html/2112.04914

### 1.4 会话总寿命：Alexa 的答案是「不做超时，做记忆」

官方 Alexa+ 文档明确：上下文会保留，**可以跨设备、跨时间接着聊**（「Alexa+ remembers the context of conversations so that you can seamlessly pick up where you left off, whether on your Echo device or in the Alexa app」）。同时，Alexa+ 声称能记住用户明确要求记住的细节。

- https://www.amazon.com/gp/help/customer/display.html?nodeId=Tq6RsnUTBpWtdZgVmC

**含义**：Alexa 没有「会话总寿命」这个机制，它把「会话」拆成了两层——**麦克风窗口（8 秒，会关）** 和 **上下文线程（持久，不关）**。

---

## 2. Google Assistant / Google Home / Nest / Gemini for Home

### 2.1 Continued Conversation

| 项目 | 内容 | 性质 |
|---|---|---|
| 机制名 | Continued Conversation | 官方 |
| 音箱/智能屏窗口 | **约 10 秒**；「microphone turns off automatically after 10 seconds of silence」 | **官方（新版帮助页）** |
| 音箱/智能屏窗口（旧页） | **约 8 秒** | **官方（旧版帮助页，与新版数字不一致）** |
| Pixel 手机窗口 | **约 10 秒**；用语音 + 陀螺仪/加速度/旋转传感器判断是否在继续对话 | **官方** |
| Gemini Live（Gemini for Home） | **最长 15 秒**；「stop talking and Gemini for Home will stop listening after 15 seconds」 | **官方** |
| Gemini 版 Continued Conversation | 官方博客只写「keeps the microphone active for a few seconds」，**未给数字** | 官方（无数字） |
| 触发 | 每次回答结束后开窗；追问即重置；静音 10 秒关麦 | 官方 |
| 结束方式 | 停止说话 / 转向别人 / 说「Thank you」「Stop」「I'm done」 | **官方** |
| 结束语 | **无**；官方只描述麦克风关闭，不播报 | 官方（反向确认） |
| 次数额度 | **未找到公开数据**（无次数限制，纯时间窗） | — |
| 可配置 | 只能开/关；**窗口长度不可配置** | 官方 |
| 不生效 | 通话中、闹钟/计时器响时、媒体播放/控制时（这些场景麦克风不会重开） | **官方** |
| 语言 | Google Assistant 版仅 US English；Gemini 版支持全部 Gemini for Home 语言 | 官方 |

- 新版官方页（10 秒）：https://support.google.com/googlehome/answer/7685981?hl=en
- 旧版官方页（8 秒）：https://support.google.com/assistant/answer/9249169?hl=en
- Pixel 手机（10 秒 + 传感器）：https://support.google.com/assistant/answer/10004407?hl=en
- Gemini Live 15 秒 + 「1 小时内可恢复上一段对话」：同上 7685981
- 官方博客（2026-04，无秒数）：https://blog.google/products-and-platforms/devices/how-to-use-gemini-continued-conversation/

> ⚠️ 8 秒 vs 10 秒是**两个官方页面自己打架**，不是第三方误读。这本身说明「窗口长度」在大厂内部也是按产品/版本调参的，不是一条铁律。

### 2.2 多设备仲裁

| 项目 | 内容 | 性质 |
|---|---|---|
| 官方表述 | 「Google Assistant uses the best available device」——按请求能力选（放音乐优先音箱），不纯按距离 | **官方** |
| 用户可控手段 | 设备间距 **≥ 8 英尺**；点名设备（「play music on bedroom speaker」）；Nest Hub Max 看着它说；Pixel Watch 抬腕；手机/平板屏幕朝上 | **官方** |
| 专利实现 | 每台设备算 hotword 置信度 + **loudness score**；按响度决定一个**延迟时间**，最响（最近）的设备延迟最短、最先发「我来处理」信号；其他设备在延迟内收到该信号就退出 | **官方专利（Google LLC）** |
| 失败模式 | 官方自己承认「It's not always easy for Assistant to decide which device should respond」 | 官方 |

- 官方「best available device」+ 8 英尺等：https://support.google.com/assistant/answer/13714067?hl=en
- 专利 US20240233727A1（loudness → delay → 抢答）：https://patents.google.com/patent/US20240233727A1/en

### 2.3 会话寿命

- Google Assistant：**未找到**「会话总寿命」的官方定义；只有每轮 10 秒窗口。
- Gemini Live：会话本身可长聊，**结束后 1 小时内**可说「Hey Google, let's chat again」恢复上一段（**官方**）。这是「上下文恢复」而非「麦克风常开」。

---

## 3. 小米小爱同学

### 3.1 连续对话

| 项目 | 内容 | 性质 |
|---|---|---|
| 机制名 | 连续对话（自然连续语音对话） | 官方 |
| 官方描述 | 小爱 3.0：「一次唤醒，连续对话、随时打断」；**未给秒数** | **官方（无数字）** |
| 窗口时长（机型 A） | **30 秒**（评测：唤醒后持续聆听 30 秒；百度经验教程同样写 30s） | 第三方 |
| 窗口时长（机型 B） | **15 秒**（小米智能音箱 `xiaomi.wifispeaker.oh2` 用户实测；小米官方集成维护者复现「同款音箱现象相同」） | 第三方（含官方集成维护者复现） |
| 触发 | 唤醒并执行完指令后继续聆听；每轮追问重置 | 第三方（行为观察） |
| 结束 | 静默超时退出；用户报告说「**再见**」可退出连续对话模式 | 第三方 |
| 结束语 | **未找到公开数据**；未发现官方「结束语」话术，第三方教程均描述为「超时自动退出」 | — |
| 次数额度 | **未找到公开数据**（无次数限制） | — |
| 可配置 | 小爱音箱 App → 更多 → 对话体验 → 唤醒与对话 → 连续对话（开关）；**未见时长可调** | 官方（路径）/第三方（时长） |
| 平台差异 | 手机端（MIUI/HyperOS）与音箱端不是同一套实现；官方称全双工连续对话在逐步适配更多音箱 | 官方 |

- 官方 3.0 页面（无秒数）：https://developers.xiaoai.mi.com/xiaoai3
- 官方详解（全双工、回声消除、无关人声拒识；无秒数）：https://www.ithome.com/0/460/729.htm
- 30 秒（第三方评测）：https://post.smzdm.com/p/ax6nlwr2/
- 15 秒（第三方实测 + 维护者复现）：https://github.com/XiaoMi/ha_xiaomi_home/issues/1507
- 机型适配计划（官方社区答疑，第三方转述）：https://www.ithome.com/0/819/433.htm

> 小爱的窗口**随机型/固件变化**，15 秒和 30 秒都真实存在。对外只能说「十几秒到 30 秒」，不能说死。

### 3.2 协同唤醒（多设备仲裁）

| 项目 | 内容 | 性质 |
|---|---|---|
| 机制名 | 协同唤醒（前身「就近唤醒」，2018 推出，2020 MIDC 升级） | **官方** |
| 基本规则 | 同一局域网内多台小爱，**离你最近的一台响应** | **官方帮助页** |
| 开启条件 | 多台音箱绑定**同一账号** + **同一局域网** | **官方** |
| 代价 | 开启后「音箱需要等待一点时间才能响应」 | **官方（明确承认延迟）** |
| 升级后判据 | 距离、**活跃状态**、设备形态/品类综合决策（手机也纳入） | **官方（小爱 5.0 页）** |
| 工程实现 | 同账号设备预先组网 → 唤醒时各设备提取**唤醒词方向 + 距离**特征 → 组内交互 → 收齐全局特征**或判决等待超时** → 分布式决策选一台；算法用**直达声能量 + 方位**（不是单纯能量） | **官方工程博客（小米技术）** |
| 核心权衡 | 「判决等待时间」过长 → 应答变慢；过短 → 同时唤醒率升高 | **官方工程博客** |
| 线上效果 | 同时唤醒率从 3.2% 降到 **1.8%**（一年内） | **官方工程博客** |
| 规模 | 接入 60+ 款设备，在线设备超千万 | 官方工程博客 |
| 未解决 | 同账号组网会跨地域把设备组在一起（尤其手机），每次都要等超时；多账号/未登录设备（如电视）无法协同 | **官方工程博客自述** |

- 官方帮助页（最近一台 + 同账号同局域网 + 需等待）：https://ics-resources.xiaomi.com/ics-resources/articles/6066c943e4801400d6db9cd7.html
- 官方小爱 5.0（距离/活跃状态/形态综合）：https://developers.xiaoai.mi.com/xiaoai5
- 官方工程博客（直达声能量+方位、判决等待、1.8%）：https://blog.csdn.net/pengzhouzhou/article/details/121987099
- 官方商城（「空间感知，根据朝向、距离判断」）：https://www.mi.com/aispeaker-mini

---

## 4. Apple Siri

### 4.1 连续对话

| 项目 | 内容 | 性质 |
|---|---|---|
| 经典 Siri back-to-back | iOS 17 起支持「说完一条，不用再喊 Siri 直接说下一条」；WWDC 2023 Federighi 原话「issue back-to-back commands without needing to say 'Siri' again」 | 官方（WWDC）/第三方（整理） |
| 时长/次数 | **未找到公开数据**；Apple 未公布窗口秒数，也未公布次数上限 | — |
| 限制 | 不能把多条命令叠在一次请求里，必须等上一条执行完；不保留上一轮语义上下文（9to5Mac 指出「doesn't appear that Siri has any knowledge or context of the first request」） | 第三方 |
| HomePod | **HomePod OS 17 未包含 back-to-back**（用户实测：https://www.reddit.com/r/HomePod/comments/16mpidb/backtoback_siri_responses_on_homepod_os_17/ ）；截至 2026-11，初代 HomePod / HomePod 2 / HomePod mini 1 **不支持 Siri AI**；HomePod mini 2「未来软件更新」 | 第三方（MacRumors/Reddit，Apple 已确认不支持） |
| Siri AI（2026-09） | 「Users can extend almost any response from Siri into a rich conversation and ask follow-up questions」；HomeView 官方指南：「You can make follow-up requests」 | **官方（无秒数）** |
| 会话持久化 | 独立 Siri App，对话历史经 iCloud 跨设备同步，可在 iPhone 开始、Mac/iPad/Watch/Vision Pro 继续 | **官方** |
| 结束语 | **未找到公开数据**；无官方结束语说明 | — |
| 免唤醒次数 | **未找到公开数据** | — |

- 官方 Siri AI 发布稿：https://www.apple.com/newsroom/2026/09/siri-ai-a-profoundly-more-capable-and-personal-assistant-is-here/
- 官方 HomeView Siri AI 指南：https://support.apple.com/guide/apple-homeview/get-started-with-siri-ai-dev0f0e2a846/homeview
- 官方 Siri AI 可用设备/要求：https://support.apple.com/en-us/127893
- 设备支持时间表（第三方）：https://www.macrumors.com/2026/10/14/apple-home-devices-siri-ai/
- iOS 17 back-to-back（第三方，含 WWDC 引语）：https://appleinsider.com/inside/ios-17/tips/how-to-use-back-to-back-commands-with-siri-in-ios-17
- 不保留上下文（第三方）：https://9to5mac.com/2023/06/07/siri-trigger-ios-17-changes-how-to/

### 4.2 多设备仲裁

| 项目 | 内容 | 性质 |
|---|---|---|
| 机制 | 设备间通过 **Bluetooth 互相通信**，协商谁响应；**不经过云端** | **官方** |
| 判据 | 「the one that heard you best or was recently raised or used」 | **官方** |
| 默认优先级 | **HomePod / HomeView 优先响应大多数 Siri 请求**，即使旁边有 iPhone | **官方** |
| 用户可控 | 可对某台设备关闭「Hey Siri」；iPhone/iPad 屏幕朝下可不响应；Mac 合盖；家庭影院配对的 HomePod 仅在 Apple TV 关闭时响应 | **官方** |
| 失败模式 | 蓝牙关闭/不在范围/软件版本不一致 → 多台同时响应 | **官方** |
| 已知 bug | 多台 HomePod 同时响应、handoff 失败（用户大面积报告） | 第三方 |

- 官方：https://support.apple.com/en-us/105077
- 同时响应问题（第三方）：https://9to5mac.com/2023/02/20/multiple-homepods-responding/

### 4.3 会话寿命

Apple 没有公开「会话超时」参数。它的路线是**把会话变成 App 里的持久对象**：Siri App 保存并同步对话历史，语音只是入口之一。也就是说，Apple 同样在回避「会话总寿命」这个问题，而不是给一个数字。

---

## 5. 横向对比表

| 维度 | Amazon Alexa | Google（Assistant / Gemini） | 小米小爱 | Apple Siri |
|---|---|---|---|---|
| **静默超时（跟随窗）** | 经典：官方「a short period」，媒体实测 **5s**；Alexa+：**约 8s**（官方） | 音箱 **10s**（新版官方）/ **8s**（旧版官方）；手机 **10s**；Gemini Live **15s** | **15s**（oh2 实测）～**30s**（评测/教程）；官方无数字 | 未公布；iOS 17 back-to-back 无秒数；Siri AI 无秒数 |
| **窗口可配置** | 只能开关，不能调时长 | 只能开关，不能调时长 | 只能开关，不能调时长 | 无开关（back-to-back 默认行为） |
| **计时起点** | 每轮回答**结束后**开窗；说话即重置 | 每轮回答结束后；追问即重置 | 每轮指令执行完后；追问即重置 | 每条命令**执行完后**才能说下一条 |
| **免唤醒次数额度** | 无（时间窗内可多轮） | 无 | 无 | 无 |
| **结束语** | 无；静默关闭；「thank you/stop」可强制结束 | 无；静默关闭；「Thank you/Stop/I'm done」结束 | 未找到公开数据；「再见」可退出（第三方） | 未找到公开数据 |
| **会话总寿命** | 无超时；Alexa+ 上下文**持久 + 跨设备** | 无超时；Gemini Live 可 **1 小时内恢复** | 未找到公开数据 | 无超时；Siri App 历史**持久 + iCloud 同步** |
| **多设备仲裁** | **云端 ESP**：端上特征 + 云端模型判「最近」 | **能力优先 + 响度抢答**：响度决定延迟，最近者先发信号；官方建议间距 ≥8 ft | **协同唤醒**：同账号同 LAN 组网，直达声能量 + 方位，分布式判决；需「判决等待」 | **蓝牙协商**：谁听得最好/最近拿起；HomePod/HomeView 优先 |
| **仲裁可配置** | 无「主设备」开关 | 无「主设备」开关；可靠「点名设备」/看向设备 | 有协同唤醒开关；无「主设备」开关 | 可关闭某设备监听；无「主设备」开关 |
| **失败率公开数据** | 无 | 无 | **同时唤醒率 1.8%**（官方工程博客） | 无（用户报告多台同时响应） |
| **媒体播放时是否开窗** | 否（持久活动时 Follow Up 不生效） | 否（媒体播放/控制时麦克风不重开） | 部分机型不支持（播放中需重新唤醒） | 不适用（Siri 不做持续媒体会话） |

---

## 6. 哪些是「大厂已验证」，哪些是「大厂也没解决」

### 6.1 已验证、可以照抄的

1. **窗口就是 8–10 秒，不是 20 秒。** Alexa+ 官方 8s、Google 官方 8/10s、Gemini Live 15s。你现在的 20s 明显偏长，属于「陪伴型」而不是「助手型」。
2. **不做次数额度，只做时间窗。** 四家都没有「2 次免唤醒」这种 credits 概念。时间窗天然限制了轮次，额度反而让用户无法预测「还能不能接话」。
3. **静默超时不要说话。** Alexa 和 Google 都是灯效/提示音消失 + 麦克风关闭，没有「我先下啦」。半夜突然来一句结束语是负体验。
4. **媒体播放时必须关窗。** Alexa 和 Google 都明确禁用；这是防误触发的硬规则，不是可选项。
5. **多设备必须有「判决等待」预算。** 小米官方博客把权衡说透了：等待太久应答慢，等待太短同时响应率高。你必须显式设计这个预算（几十到几百 ms），不能假设「谁先检测到谁答」。
6. **仲裁判据不能只用能量。** 小米用「直达声能量 + 方位」，Amazon 用端到端学习，Google 用「响度 → 延迟」。纯能量在混响/电视噪声下必错。

### 6.2 大厂也没解决的（你可以差异化，但别低估成本）

1. **同时响应。** 小米官方数据仍有 **1.8%** 的同时唤醒率；Apple 至今有多台 HomePod 同时响的用户报告。你有 5 台设备，这个坑一定在。
2. **多账号/未登录设备无法协同。** 小米官方博客自述：电视这类不登录账号的设备无法参与协同；同账号又会把异地设备组进来，导致每次等超时。你的 5 台设备如果都在一个家、一个账号，这个问题反而小。
3. **窗口长度的体验矛盾。** Google 自己两个官方页面写 8s 和 10s；Alexa 从 5s 走到 8s。说明「刚好够接话、又不误收环境音」没有标准答案。
4. **「会话结束」没有用户可见的明确信号。** 四家都靠灯，用户经常不知道还能不能接话。这是你可以做得更好的地方（比如一句极短的「嗯？」或渐暗灯效），但不要用长结束语。
5. **跨设备上下文的一致性。** Alexa+ 说能跨设备续聊，Siri 用 iCloud 同步历史，但都不是实时的「这台说完那台接」；Google 是 1 小时内「chat again」。真正无缝的多房间上下文接力，没有一家做好。

### 6.3 你的设计 vs 主流：逐项判断

| 你的设计 | 主流做法 | 判断 |
|---|---|---|
| 跟随窗 20s | 8–10s（Alexa+/Google），15s（Gemini Live） | **偏离主流，偏长**。建议默认 8s；「连续任务模式」再给 20s |
| 2 次免唤醒 credits | 无此概念 | **差异化，但不是「大厂验证过的」**。若保留，必须让用户可感知剩余次数（灯效/提示音），否则不可预测 |
| 缺会话总寿命 | 大厂用「持久上下文」替代「总寿命」 | **该补，但要补对**：补「麦克风会话寿命」，不要把「上下文」一起杀掉 |
| 结束语「没事的话，我先下啦」 | 无结束语 | **差异化**，但建议只在「用户显式结束」或「总寿命到期且用户刚说过话」时播；静默超时不要播 |
| 多设备 | 云端/端侧仲裁 + 判决等待 | 需要补「判决等待预算」和「同时响应兜底」（比如后响应者静音自检） |

---

## 7. 给「会话生命周期」的具体参数建议（基于以上证据）

> 以下是**建议**，不是大厂原文；依据列在括号里。

1. **两层会话，分开计时**（依据：Alexa+ / Siri AI 的做法）
   - `voice_session`：麦克风开启期，静默 **8s** 关闭（Google/Alexa+ 主流值）。
   - `context_session`：语义上下文，**2–5 分钟**无活动过期；过期不清历史，只清「指代消解」的短期栈。
2. **免唤醒追问不设次数，设「连续轮次软上限」**：比如 6 轮后强制一次唤醒词（防死循环），但不告诉用户「还剩几次」。
3. **总寿命用「最后活动时间 + 硬上限」双条件**：
   - 软：**120s** 无任何活动 → 结束麦克风会话；
   - 硬：单次会话最长 **10 分钟** → 强制结束（防麦克风长开）。
4. **结束语分级**：
   - 用户说「没事了/谢谢/再见」→ 播短结束语（可关）；
   - 静默超时 → **不播**，只渐暗灯；
   - 硬上限到期 → 播一句极短的「先到这里」。
5. **媒体播放时禁用跟随窗**（Alexa/Google 一致做法），播放结束后的 **2s 内**也不开窗（避免尾音误触发）。
6. **多设备**：判决等待 **150–300ms** 起步（小米未公布具体值，只公布权衡逻辑），并上报「同时响应率」作为核心指标；目标先做到 <5%，再优化到 2%。

---

## 8. 数据缺口清单（明确没查到的）

- Alexa 经典 Follow Up Mode 当前版本的**官方秒数**（只有 2018 媒体 5s 与 Alexa+ 官方 8s）。
- Alexa 是否存在「单次会话最多追问 N 次」的内部限制（未找到任何官方/第三方数据）。
- 小爱连续对话的**官方**窗口秒数（只有 15s/30s 第三方，且随机型）。
- 小爱是否有官方结束语（未找到；第三方只描述静默退出与「再见」退出）。
- Siri back-to-back / Siri AI 的**任何**官方或实测秒数。
- Google 多设备仲裁的**官方**算法说明（只有专利 + 用户可操作建议，没有产品级文档）。
- 四家的「同时响应率」只有小米公开了 1.8%，其余无公开数据。

---

*本报告所有链接均在 2026-11-17 实际抓取可访问。*
