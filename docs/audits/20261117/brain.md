# 大脑、记忆与工具链审计

审计日期：2026-11-17。Core 基线 `c3ed6c6`，运行时代码与现场 `05da212` 相同。本报告综合专项 Agent 的隔离复现与主 Agent 对调用链的复核。仅检查本地源码和合成数据；未读取现场数据库、凭据、私人录音，未执行真实外部工具动作。

## 范围与结论

覆盖 Agent prompt/history 组装、工具目录与动态注册、模型工具名称往返、工作记忆读写、NAS 上传与缓存、OpenAPI 执行确认。长期记忆有结构和来源记录，但作用域、普通观察入队、跨设备共享及删除传播没有形成完整合同。模型工具目录存在不代表每个注册名称都能执行；副作用元数据存在也不代表执行层强制授权。

确认 6 项：3 个 P1、3 个 P2。P1 表示在依赖相关边界或扩大使用前优先修复，并不表示已观测到现场损害。未证明 P0。

## B-01 — P1 — 工作记忆工具没有执行调用者作用域检查

证据：`crates/hm-core/src/working_memory.rs` 的 prompt 渲染按 `ScopeContext` 过滤；`tool/mod.rs:235` 的 `ToolCtx` 只有 device/trace provenance，没有可信 person/permission。`tool/memory.rs:189` 的 get 忽略 ctx；无过滤参数时调用 `list(None,None)`，而 `working_memory.rs:347` 明确表示 None 为不限范围。set 在 `memory.rs:126` 接受模型参数指定 person/device；delete 在 `:250` 同样忽略 ctx。`Registry.call`（`tool/mod.rs:483`）仅查找、检查取消后执行，没有补充作用域权限。

触发与影响：匿名设备回合的 prompt 看不到另一人、另一设备的条目，但同一回合通过工作记忆工具可以读取它，指定作用域还可以覆盖或删除。这里讨论已进入助手的工具调用权限，不声称存在匿名公网 API 入口。另有 `workmem.delete` 在 `memory.rs:232` 错标 `SideEffect::Query`；元数据本身不提供保护，合并到本项，不重复计数。

验证：专项 Agent 在临时 SQLite、虚构 Alice/设备数据和真实 `Registry.call` 上复现读、写、删的差异；零外部 HTTP、无现场数据。主 Agent复核 ToolCtx→Registry→memory→store 路径。

修复与验收：把可信身份、允许的读写范围放入执行上下文，由工具统一裁决；模型参数只能缩小请求范围。管理端的全库读取需独立授权。覆盖匿名、同设备、跨设备、不同成员、全家共享条目，保证 prompt 和工具结果具有一致可见性，纠正 delete 的副作用分类。

## B-02 — P1 — 破坏性 OpenAPI 操作把模型传入的布尔值当作用户确认

证据：`tool/openapi.rs:1306`、`:1334` 将 `confirmed=true` 暴露给模型；`:1353` 直接读模型参数并传给执行层。`tool/openapi_verify.rs:40` 对 Destructive 只判断该布尔值；`agent/mod.rs:964–982` 不存在与用户确认记录绑定的额外授权步骤。

触发与影响：管理员已配置并加入白名单的破坏性操作，模型只要给出 `confirmed=true` 就满足所谓确认条件。白名单/参数校验仍有效；问题是不能据此声称每次执行都获得了用户本轮确认。未触发真实删除、支付或设备动作。

修复与验收：由服务端创建绑定 task、operation、参数摘要、有效期的待确认动作；可信用户交互批准后才消费该授权。模型不得自行签发。覆盖模型自填 true、参数变化、过期、重放、取消及真实确认路径。

## B-03 — P2 — 普通对话/新记忆没有接入 NAS 观察上传队列

证据：`shared_memory.rs:96` 定义 `enqueue_observation`，生产源码无调用；除该定义外，唯一 `INSERT INTO shared_memory_outbox` 位于 `working_memory.rs:245` 的容量淘汰路径（500 条上限）。普通 voice turn、工作记忆新增及 USER.md 提议未入该队列。同步 worker 只能上传已有 outbox。

触发与影响：启用 NAS 共享并正常对话、保存少量工作记忆时，后台可能正常探活/同步却没有普通观察可综合；达到容量后淘汰的条目反而成为主要自动输入。不能把服务健康等同家庭记忆持续更新。外部系统直接提交 NAS observations 的能力不受本项影响。

修复与验收：先定义哪些经用户允许、带来源和作用域的观察可入队，再接入已完成回合/显式保存事件，保持幂等与关闭上传策略；不恢复冻结的持续空闲转写。小于容量的普通用例应产生预期的一次入队，取消/禁止上传不应产生上传。

## B-04 — P2 — NAS 把来源设备直接当成可见范围，自动画像无法自然跨房间使用

证据：关联仓库 `rui-personal-services` 的 `memory-svc/app/consolidator.py:212` 按 `(person_id,device_id)` 分组，`:257` 写 `scope_device=device_id`，`:259` 又写 `source_device=device_id`。Core `shared_memory.rs:261–265` 要求设备范围为空或严格匹配当前设备。

触发与影响：设备 B 产生的可共享家庭观察，经默认综合会成为仅 B 可见的画像，D 即使已下载缓存也不注入 prompt。管理员可以手工把范围改为共享，这不否定自动管线中的来源/权限混淆。不得通过删除所有范围过滤来修复，否则会扩大 B-01 的问题。

修复与验收：协议分离观察来源和可见范围；只有明确家庭共享信息生成全家画像，个人/房间信息保持限制。用同一家庭偏好和一条私有事实，验证 B→NAS→D 的共享与隔离同时成立。未访问 NAS 运行实例。

## B-05 — P1 — NAS 删除画像后，Core 缓存仍可无限期进入 prompt

证据：NAS `memory-svc/app/main.py:198` 提供删除画像；Core `shared_memory.rs:411–450` 拉取完整列表后只 upsert，未比较消失 ID、处理 tombstone 或替换快照。`shared_memory.rs:228` 的缓存读取及 prompt 过滤没有删除同步/过期裁决。

触发与影响：画像先同步到 Core，用户随后在 NAS 管理端删除；后续成功同步不删除本地旧行，仍匹配的旧信息继续被使用。本项与 ARCH-01 的本机恢复出厂遗漏是两个独立生命周期问题。

修复与验收：协议明确完整快照/增量语义与版本、删除 tombstone；只有完整成功响应才能原子替换，失败不能误清空。验证删除传播、离线恢复、scope 修改、部分响应和同步失败；保留来源与删除记录而不继续注入被删除内容。

## B-06 — P2 — 含下划线的动态工具可被发现，但模型名称往返后找不到

证据：`llm/mod.rs:69–74` 把点编码成下划线，解码把所有下划线改成点。`agent/mod.rs:964` 对返回名称统一解码。`tool/mod.rs:364–390` 精确查找动态工具后，只为内置 `self.tools` 做 wire 名兜底；动态 map/provider 没有同样处理。

触发与影响：合法动态名称 `family_light.start` 出现在 specs；模型返回 wire 名 `family_light_start`，解码为 `family.light.start`，最终 UNKNOWN_TOOL。工具发现/UI 配置成功不等于助手能调用。未证明所有动态工具失败；无下划线名称不受这个具体例子影响。

验证：专项 Agent 使用真实动态注册、specs、名称变换和 Registry.call 复现，零 HTTP 发送；主 Agent复核名称转换与三个注册域的查找差异。

修复与验收：注册时建立明确、唯一、可逆的 wire→canonical 映射，覆盖内置/动态/provider 全目录；拒绝冲突。验证含下划线、多段点名、规范名与 wire 名、热更新/注销，确保 schema 中出现的名称确实可回调。

## 应保留的基础与未覆盖部分

- 自有 Rust agent 循环、独立 ASR/LLM/TTS 服务选择、统一 Registry、ToolFact、取消身份及持久化记录，具有可演进价值。
- `agent/facts.rs` 对回读事实、服务接受、未知结果作了区分；需要修复特殊路径，继续坚持“执行成功不能靠模型口头宣布”。
- 工作记忆、家庭短注、USER.md、Agent.md、NAS 缓存是不同职责；修复时先统一权限/来源/删除语义，不必合成单一大表或替换整套内核。
- 没有进行模型语义质量基准、真实第三方服务回放、NAS 线上数据库检查、全依赖漏洞扫描或商业许可最终审定。本报告不宣称这些项目通过。
