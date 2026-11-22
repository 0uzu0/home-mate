# 同类产品研究独立复核

复核日期：2026-11-17。对象：`/tmp/hm-product-benchmark.md`。只读核查官方资料、Core 前端路由/提醒工具、现行里程碑与历史范围裁定；未登录竞品、未测试竞品或现场设备。

## 结论

通过，需两处小幅校准后交付。五个产品系列的主要功能有官方依据；报告已经把厂商说明与实测性能、地区可用性、模型差异和 Home Mate 结构潜力分开。至少八项是清晰的非 bug 产品增量；成员模式另属已规划 W4c 的产品扩展，不宜把声纹能力再次计为新提案。没有发现借对标恢复 #85、MCP、双语语音、全双工/S2S 或故事跨房接续的情况。

## 需要校准

1. 第 6 项手机任务卡的访客隔离验收依赖真正成员身份及服务端权限。当前共享 token/Scope 字段不能直接等同家庭角色体系。首批可限定管理员受控确认；访客与敏感确认隔离应与第 9 项身份基础同时交付。否则 S–M 成本和首批范围偏乐观。
2. 小米 NFC 接力需标注 2023 年 9 月新版 Xiaomi Sound 与兼容小米 NFC 手机/软件；本次直接打开产品页仅获得导航，但官方搜索返回产品正文和限制，可与官方全屋播放手册共同引用。不得泛化至所有小爱型号。

轻微措辞建议：对照表“成熟方向”改为“官方支持或声明的能力”，以免 Alexa 厂商描述被读作独立成熟度评级。标题可称“8 个新增方向与 1 个既有路线扩展”。

## 官方证据复核

| 对象 | 复核结论 | 官方依据 |
|---|---|---|
| Home Assistant | 完全本地管线真实存在；本地与可换模型不是 Home Mate 独占优势。封闭指令/Whisper 的能力、算力和语言边界应保留 | https://www.home-assistant.io/voice_control/voice_remote_local_assistant/ |
| Alexa+ | 官方页面 2026-07 更新，支持语音 routines、偏好记忆、文档、Echo/手机/网页接续。报告正确标为厂商声明；不宜引用该页未来式 agentic 服务预约为已交付能力 | https://www.aboutamazon.com/news/devices/new-alexa-generative-artificial-intelligence 与 https://www.aboutamazon.com/news/devices/new-alexa-top-features |
| Gemini for Home | early access；地区与语言表确实不含中国大陆/中文；基础日程/清单/提醒免费、Live 要兼容硬件及 Premium。报告表述正确 | https://support.google.com/googlehome/answer/16618650 |
| HomePod | Intercom 可按房间/区域与个人设备；成员、声音识别及个人内容有 Apple 账户/设备前提，声音识别存在语言地区限制 | https://support.apple.com/en-in/101606 与 https://support.apple.com/guide/homepod/set-up-siri-apd1841a8f81/1.0/homepod/1.0 |
| Xiaomi Sound | 官方型号页与手册支持家庭传声及同局域网/同账号全屋播放，最多八台支持型号。NFC 接力须注意版本与手机条件 | https://www.mi.com/xiaomi-sound?cfrom=search&product_id=1230800914 与 https://cdn.cnbj1.fds.api.mi-img.com/ics-resources/articles/606677793d15e2f93e95f56e.html |


## 最值得优先讨论的五项

1. 能力中心与接入配方：充分利用已有工具 Registry，让家人知道现在真能做什么。
2. 手机任务卡与回执：直接补齐现有管理控制台之外的家庭日常入口；首批限制身份范围。
3. 按房间广播：最能发挥现有五台音箱价值，定向投递、免打扰和离线状态要明确；先完成仲裁缺陷修复。
4. 带来源家庭资料库：和自有 NAS/记忆基础契合，具有长期产品差异化；必须有来源、更新、删除和权限语义。
5. 可预览/可停用家庭例程：从一次指令扩展到可监督的长期任务，设备规则优先委托 HA。

这五项均不等于现有 bug 修复，也不重复现有单次提醒、persona 编辑、工具白名单或声纹计划。家庭日程、通知中心、本地能力包和成员模式作为后续路线仍成立，但各有明确状态、权限或算力依赖。


本次统一基线、优先级与验证边界见 [审计总报告](README.md)；隔离复现源码/输出见 [证据附录](evidence.md)。
