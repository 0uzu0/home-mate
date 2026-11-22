# 商业化目标、自有许可与第三方许可初筛

日期：2026-11-07。用户已明确Home Mate计划商用。商业许可与分发方式是依赖选型和发布设计的前置考虑，不等实现完成才检查。

本轮只核对候选官方仓库的当前许可声明，未选定依赖、审计传递依赖或形成可发行产品的完整许可结论。本文记录事实和后续任务，不替用户决定开源、闭源或具体商业授权文本。

## 1. “协议”分成四类

| 类别 | 管什么 | 当前安排 |
|---|---|---|
| 自有代码许可证 | Home Mate核心、桥、SDK、示例的使用/修改/分发条件 | 权利归属和开源/闭源策略待定；不擅自加MIT、Apache或其他授权 |
| 技术接口契约 | API、设备能力、工具、事件、版本和兼容规则 | 按[通用接口与工具层](interfaces-and-tool-layer.md)设计，可独立于核心实现公开 |
| 商业软件/服务条款 | 安装分发或托管服务的授权范围、支持、更新、终止等 | 先确定本地软件、硬件套装、托管服务或组合的实际销售方式，再拟条款 |
| 数据与隐私规则 | 音频/图像/记忆的采集、用途、保存、删除及第三方处理 | 根据实际产品和部署地区制定；页面和接口需能落实，不只写声明 |

接口规范可以开放，核心代码仍可采用不同授权；采用开源依赖也不自动决定自有代码许可证。仓库private只说明访问范围，不代表已经完成对客户的商业授权设计。

## 2. 候选顶层许可初筛

核对时间为2026-11-07，下列链接指main/dev，会变化；落地时必须固定实际版本、包及相应许可文件。这里只记录对应项目声明，不覆盖全部可选模块和依赖。

| 项目 | 本轮查看到的声明 | 后续要核对 |
|---|---|---|
| Rig | 根[Cargo.toml](https://github.com/0xPlaygrounds/rig/blob/main/Cargo.toml)标记MIT；[LICENSE](https://github.com/0xPlaygrounds/rig/blob/main/LICENSE)包含相应授权文本 | 实际crate/feature、分发文件及依赖的许可与声明 |
| rust-genai | [Cargo.toml](https://github.com/jeremychone/rust-genai/blob/main/Cargo.toml)标记`MIT OR Apache-2.0`；仓库有[MIT](https://github.com/jeremychone/rust-genai/blob/main/LICENSE-MIT)和[Apache](https://github.com/jeremychone/rust-genai/blob/main/LICENSE-APACHE)文本 | 所用发布包的双许可声明是否一致，采用哪一分支及依赖义务 |

M3.5 选型新增的商用评审遗留项（引入时需结论，当前均未引入，见 [kernel-selection](kernel-selection.md) §3-4）：Rig 默认 rustls 路径的 `webpki-root-certs`（CDLA-Permissive-2.0，数据许可需单列）；`sqlite-vec` 许可未查；Rig 的 ARC 代币资金背景记入审查；genai 若按 Apache-2.0 分支走需 NOTICE/归因义务。langchain-rust 已拒绝引入，其依赖许可问题不再进入清单。
| ADK-Rust | 根[LICENSE](https://github.com/zavora-ai/adk-rust/blob/main/LICENSE)声明Apache-2.0 | 实际选取组件、NOTICE/修改记录、可选模块和依赖 |
| Pi | 根[LICENSE](https://github.com/earendil-works/pi/blob/main/LICENSE)声明MIT | 如复制/改写具体实现仍需履行相应许可；本项目暂以方法学习为主 |
| MCP规范与Rust SDK | [规范仓库LICENSE](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/LICENSE)和[Rust SDK LICENSE](https://github.com/modelcontextprotocol/rust-sdk/blob/main/LICENSE)载明MIT向Apache-2.0过渡，并说明文档贡献与规范的许可区别 | 具体版本/文件仍适用何种许可、包声明、遗留MIT贡献与文档署名，不能只填“所有MCP都是MIT” |
| Home Assistant Core | [Core LICENSE.md](https://github.com/home-assistant/core/blob/dev/LICENSE.md)为Apache-2.0 | 仅API对接、修改其源码、捆绑其分发是不同情况；附加组件与品牌使用另核对 |

**已引入（2026-11-12，R1e 音乐；2026-11-22，#252 故事 `audio/mp4` 解码补齐 `isomp4,aac`）**：本机 `~/.cargo/registry` 实际版本的 `Cargo.toml` 声明——
`symphonia =0.5.5`（含 `symphonia-core`/`symphonia-bundle-mp3`/`symphonia-bundle-flac`/
`symphonia-metadata`/`symphonia-format-riff`/`symphonia-format-isomp4`/`symphonia-codec-aac` 等同版本）为 **MPL-2.0**；`md-5 0.10.6` 为
**MIT OR Apache-2.0**。用途：Subsonic/Personal Services 下发的是原始文件（mp3/flac/m4a/aac），设备只吃
PCM，缺了它只能把整首歌先拉进内存或改成 16 kHz 丢高频。MPL-2.0 是文件级弱著佐权：链接/分发时
需要保留许可文本、声明使用，**修改过的 MPL 文件**须公开其修改；未修改的依赖不要求开源自有代码。
发布前要做的：把 symphonia 系列的 LICENSE 与版本号放进发行物的第三方声明，并复查是否有
`pcm`/`wav` 之外新开的 feature（当前固定 `mp3,flac,wav,pcm,isomp4,aac`）。本项目**不据此给自有代码定许可**。

MIT和Apache-2.0允许相应商业使用，但附有条件。MIT文本要求在有关副本/实质部分中保留版权和许可；Apache-2.0还规定再分发、修改说明和适用NOTICE等要求，专利及商标范围亦有具体条款。应按实际分发内容和方式落实，而非只记录“能商用”。参见[MIT文本](https://github.com/jeremychone/rust-genai/blob/main/LICENSE-MIT)与[Apache官方全文](https://www.apache.org/licenses/LICENSE-2.0)。

GPL、LGPL、AGPL或其他许可也不能简单写成“禁止商用”；若候选依赖出现此类条款，应结合链接、修改、分发或服务方式评估义务，再决定是否符合产品策略。

**品牌资产（2026-11-12，用户提供）**：`web/public/home-mate-logo.svg` 与 `home-mate-icon.svg` 由项目
所有者设计并提供，不是第三方素材，不引入额外许可；发布前仍需完成名称/图形商标查询（与 `Home Mate`
及唤醒词「高达」的商标事项一起，属发布门，未完成）。

**已引入（2026-11-13，R2-BM6.3 HA 暴露模式）**：`tokio-tungstenite 0.30.0`
（`~/.cargo/registry` 的包声明 **MIT**；传递 `tungstenite 0.30.0` 为 **MIT OR Apache-2.0**，
`http`/`httparse`/`utf-8`/`sha1` 等为 MIT/Apache-2.0 系）。用途：①网关仓自 G5 起用它做 EchoMuse
WebSocket 服务端（`emuse/mod.rs`，此前未登记，本次一并补记）；②核心从 2026-11-13 起用它作为
Home Assistant 客户端，读 `homeassistant/expose_entity/list`（Assist 暴露列表，无 REST 等价物）。
TLS 走 rustls（Apache-2.0/ISC/MIT），`webpki-roots` 的数据许可问题见 §2 的 CDLA-Permissive-2.0 备注。
发布前：把版本与 LICENSE 文本放进第三方声明；只按现用 feature（客户端 + rustls 根证书）核对。

**已引入（2026-11-12，R1i 修订）**：`lucide-vue-next 1.0.0`（npm 包声明 **ISC**；peer
`vue >=3.0.1`）。用途：控制台的统一图标库（导航、卡片标题、动作按钮、空/错误态）。图标只作为
SVG 组件按名引用，随控制台构建产物分发；发布前把版本与 ISC 文本放进第三方声明。ISC 允许商业
使用，要求保留版权与许可声明。

**已引入（2026-11-12，R1i 控制台重写）**：`vue-router 4.6.4`（npm 包声明 **MIT**；`package.json` 的
peerDependencies 为 `vue ^3.2.0`）。用途：控制台六页的 history 路由与懒加载分节（R1i 起取代单页
手工页签）。RUI v2 用的是同款 `^4.5.0`。发布前要做：把版本与 LICENSE 文本放进第三方声明；控制台
构建产物只含 `vue`＋`vue-router`＋项目代码，无 UI 组件库、无新字体/图标资产。

## 3. 商用前的依赖资产清单

在N1选型时登记直接依赖，按实际构建补齐传递依赖与系统/原生库，发布前针对实际产物复核：

- 名称、版本/提交、来源、license表达式、LICENSE/NOTICE、所用features、修改情况。
- 是代码链接、独立进程、远程API、模型权重还是示例/文档；“外部进程”不代表无许可义务。
- 是否随二进制、容器、SDK或硬件一起交付，需附带什么材料。
- 评估结论、未决项、第三方声明位置和负责确认的人。

模型权重、ASR/KWS/TTS资源、音色、音乐/有声内容、固件和OS镜像分别核对。库的许可证不能替代模型/音色/服务条款；家庭自用的缓存或录音授权也不能直接扩展成商业内容分发权。

旧RUI与固件仓的许可也须在任何实际代码复用前核对；迁移可验证事实和经验不等于复制源码的许可已经处理。实际引用代码保留来源与应有声明。

## 4. 自有许可与条款的拟定顺序

1. 明确权利主体、贡献来源与外部代码边界；确定产品销售与部署形式。
2. 分别决定核心、外部桥、接口规范、SDK、示例和文档的开放范围。SDK/规范许可需支持希望实现的第三方接入与分发方式。
3. 依据决定使用合适的代码许可和商业合同文本，不混写技术协议与软件授权，也不擅自重许可他人代码。
4. 对安装交付与托管服务分别明确适用条款；支持、更新、遥测、第三方调用、责任与数据处理需对应真实产品行为。
5. 在商业发布前按销售地区、实际产物与数据流完成相应审核，并把需要的通知/授权/保留/删除机制落实到页面和接口。

具体许可和商业条款最终由用户确认。本轮不生成宣称已生效的EULA、不开放仓库、不增加未经决定的LICENSE。

## 5. 阶段闸口

- **N0.1**：商业发行形式、代码/SDK/规范开放意向列为明确待定项；权限与家庭/设备数据归属进入接口设计。
- **N1**：Agent、MCP SDK、音频/视觉组件选型同时给出固定版本许可与预期分发影响；不只比较功能、性能和star数。
- **N2/N3**：累积实际使用的依赖清单、素材/模型来源与第三方声明；接口实现不绕过接入方授权和数据范围。
- **商业发布前**：针对实际产物、销售与数据处理方式复核；未决许可不能在发布报告中写成已解决。

本轮完成顶层资料初筛及任务安排。未完成依赖审计、权重/内容权利核对或商业条款确认。

**已引入（2026-11-13，M1b Silero 端点检测）**：`tract-onnx 0.21.19` 及其同版本子 crate
`tract-core`/`tract-data`/`tract-hir`/`tract-linalg`/`tract-nnef`/`tract-onnx-opl` 在
`~/.cargo/registry` 的包声明均为 **MIT OR Apache-2.0**；传递依赖 `prost-derive 0.11.9` 为
**Apache-2.0**，`memmap2 0.9.12` 为 **MIT OR Apache-2.0**。用途：在核心进程内跑 Silero VAD v4
（16 kHz 导出）做端点检测，替换绝对 dBFS 能量阈值。**没有引入 onnxruntime**（C++ 运行时，体积与
交叉编译成本更高）。发布前：把上述版本与 LICENSE 文本放进第三方声明；本项目按 Apache-2.0 分支
履行 NOTICE/再分发义务（或按 MIT 分支，二选一，需在有具体分发形态时定）。

**模型资产（2026-11-13，M1b）**：`silero_vad.onnx`（Silero VAD v4，MIT，
https://github.com/snakers4/silero-vad）**不进 Git、不嵌入二进制**，随部署放在
`<data_dir>/models/silero_vad.onnx`；缺失即回退能量 VAD。当前实际部署文件为 sherpa-onnx 的
16 kHz 导出（643,854 B，md5 `d486e9c504c9034316d53a9f4e0eee2e`），已验证与设备上 EchoMuse 自带的
1,806,511 B 版本逐帧输出完全相同（max|Δprob| = 0.0000）。发布前：把模型许可文本与来源写进发行物声明，
并确认所分发的那份导出（含导出脚本/来源）满足 MIT 的版权与许可保留要求。

**已引入（2026-11-14，M8a 发布包验签/安装器 `hm-release`）**：`ed25519-dalek 2.2.0` 与
`curve25519-dalek 4.1.3` 在 `~/.cargo/registry` 的包声明均为 **BSD-3-Clause**；`sha2 0.10.9`、
`tar 0.4.46` 为 **MIT OR Apache-2.0**；传递依赖 `signature 2.2.0`（Apache-2.0/MIT）、
`ed25519 2.2.3`（Apache-2.0/MIT）、`subtle 2.6.1`（BSD-3-Clause）、`zeroize 1.9.1`
（Apache-2.0/MIT）、`digest 0.10.7`/`block-buffer 0.10.4`/`cpufeatures 0.2.17`（MIT/Apache-2.0）、
`filetime 0.2.29`/`xattr 1.7.0`（MIT/Apache-2.0，tar 的可选特性）、`libc 0.2.191`（MIT/Apache-2.0）。
用途：独立的发布清单验签/安装/激活二进制（Ed25519 验签 + 真 SHA-256；公钥编译进该二进制，
私钥只在开发机、不入库不上设备）。**核心不引入这些依赖**（16 MiB 更新体闸门），发布包也不含
`hm-release` 自身。**2026-11-14（M9 家庭平面鉴权）核心只新增 `sha2 0.10`**（MIT OR Apache-2.0，
本段已列）用于把管理 API 令牌存成 SHA-256；ed25519/curve25519/tar 仍只在安装器里。发布前：把上述
版本与 LICENSE 文本放进第三方声明；BSD-3-Clause 需保留版权、
许可文本与免责声明，不要求开源自有代码。

**已引入（2026-11-15，M12a-1 凭证库）**：核心新增 `chacha20poly1305 0.10.1`，本机
`~/.cargo/registry` 的包声明为 **Apache-2.0 OR MIT**；传递链同版本声明一并核对：
`aead 0.5.2`、`chacha20 0.9.1`、`cipher 0.4.4`、`poly1305 0.8.0`、`universal-hash 0.5.1`、
`inout 0.1.4`、`opaque-debug 0.3.1` 全部为 **MIT OR Apache-2.0**（前三者写作
"Apache-2.0 OR MIT"，同一组双许可，二选一即可）。用途：`secrets` 表的值在落盘前做 AEAD
加密（`enc:v1:`），密钥是本机 `data/secret.key`。全部纯 Rust、无 C 依赖、无运行期代码生成。
**双许可按 MIT 分支使用**即可满足分发义务（保留版权与许可文本）；若按 Apache-2.0 分支则需
NOTICE/归因——**这一选择随自有代码许可一并由 owner 决定**，本文不代选。发布前：把上述版本与
LICENSE 文本加入第三方声明。

## 内容来源（音乐/笑话）与商业化边界（2026-11-14 用户拍板）

1. **Home Mate 镜像只做通用调用方**：不含任何平台特定解析、不伪装请求头、不内置任何在线端点；
   出厂 `MusicConfig` 为空（`enabled:false`、`library:None`、`online:None`），未配置＝0 外部请求。
2. **灰区能力不随产品分发**：平台特定解析/解析器地址/伪装 UA 只存在于本机、独立的
   **Personal Service** 项目（private，README 明令不得公开、不进 release）；
   不打包进 Home Mate 发布包、不做"一键安装附赠"。
3. **对外话术**：只说"可接入你自己配置的来源"，不说"全网音乐免费听"，不指向任何公共解析器。
4. **本地库优先**：`prefer_local` 默认 true；本地库（Navidrome）是默认可用的音乐能力。
5. 审批前复核：`Personal Service` 若要商业化，必须先完成"可发布/不可发布分层"
   （见 [personal-service](d/personal-service-20261114.md) §3）。

**已引入（2026-11-18，L6 浏览器验收，仅开发/CI）**：精确锁定 `@playwright/test 1.64.1` → `playwright 1.64.1` → `playwright-core 1.64.1`，实际 npm 包均声明 **Apache-2.0**、Node >=20；依赖链与 lock integrity 已逐包核对，不升级既有依赖。实际包内 `LICENSE`、`NOTICE` 及打包 Babel/expect/utils/codecs 等 `*.LICENSE` 必须保留；只新增这三个 npm 包不表示只有三个版权主体。来源：[固定版本包声明](https://raw.githubusercontent.com/microsoft/playwright/v1.64.1/packages/playwright-test/package.json)、[LICENSE](https://raw.githubusercontent.com/microsoft/playwright/v1.64.1/LICENSE)、[NOTICE](https://raw.githubusercontent.com/microsoft/playwright/v1.64.1/NOTICE)。

浏览器由该固定版本 manifest 选择 Chrome Headless Shell **156.0.8078.4 / revision1248**，安装还包含 Playwright FFmpeg **1013**。本机已读实际安装的 `LICENSE.headless_shell`（Chromium 及第三方声明）；浏览器/FFmpeg 是独立测试资产，不能从 npm 的 Apache-2.0 推导其全部许可。runner 与浏览器仅用于本地/CI真实控制台验收，**不进入 `src`、`web/dist`、设备、容器或产品发行包**；安装目录与证据目录分离，CI不上传浏览器或 node_modules。日后若分发这些独立资产，须另核对实际平台包的许可证/credits与再分发义务；本条不构成产品完整商业许可结论，也不替所有者选择自有许可证。

**已引入（2026-11-19，Phase C 个人学习服务 `learning-svc` 及 Core HTTP 客户端边界，`#103` `START-05`）**：
- **NAS `learning-svc`（独立 Python 进程，不进 `hm-core` 二进制或音箱固件）**：
  - `fsrs 6.3.2`（PyPI `fsrs==6.3.2`，https://github.com/open-spaced-repetition/py-fsrs，声明 **MIT**）：FSRS 间隔重复调度算法核心。
  - `typing-extensions 4.12+`（PyPI，声明 **PSF-2.0**）：`fsrs` / `pydantic` 运行期类型兼容依赖。
  - `pinyin 0.4.0`（PyPI，声明 **MIT**）：中文词义与发音辅助归一化。
  - HTTP 服务栈 `fastapi`（**MIT**）、`uvicorn`（**BSD-3-Clause**）、`pydantic`（**MIT**）、`httpx`（**BSD-3-Clause**）。
  - **Cairn 参考实现与教材数据隔离**：`learning-svc` 为独立重写实现，不复制 Cairn 源码；内置或导入的教材词表（如沪教版五年级 124 词）属于用户自有或本地私有学习数据，**不作为商业公开发行包内置语料分发**。
- **`hm-core` 侧（`crates/hm-core/src/tool/{learning,personal_media}.rs`）**：仅复用既有的 `reqwest 0.12`（**MIT OR Apache-2.0**）与 `serde_json`（**MIT OR Apache-2.0**）通过标准 HTTP JSON + `Idempotency-Key` 调用 `learning-svc` / `story-svc` / `joke-svc` / `radio-svc`，在 Core 侧**零新增第三方 crate 依赖**。


---

## 开源与硬件商业化策略草案（2026-11-19，提案 — 待终审）

> 本节记录所有者于 2026-11-19 确认的阶段性方向提案（详见追踪单 [#201](https://github.com/yes999zc/home-mate-b/issues/201)）。在完成全量第三方依赖许可核查、历史脱敏与所有者正式签发前，**不向任何仓库添加开源 `LICENSE` 文件**。

- **阶段目标**：先做高口碑的社区开源项目，通过「出厂声学标定 + 官方签名一键 OTA」的**定制硬件 / 预装盒子**实现初期变现；后期视社区起量再演化为独立软硬件商业产品。
- **拟定分仓分层授权**：
  - `hm-core` + Web 控制台：`AGPL-3.0` + 商业双授权（配轻量 CLA，防商业闭源白嫖套壳，保留 OEM / 商业闭源扩展权）；
  - Gateway 通信协议（LDJSON Spec / SDK）：`Apache-2.0`（鼓励社区适配更多二手/DIY 硬件）；
  - 已调校的 Dot2 / LX06 网关固件：`AGPL-3.0`；
  - 品牌、Logo、Ed25519 签名密钥与官方 OTA 频道：专有保留（不开源）；
  - NAS 个人服务：仅开放干净自研模块（如 `learning-svc`），含第三方数据源/版权敏感的模块永久私有。
- **公开前硬门禁**：跑通 Dot2 + LX06 双机型、新建干净公开仓脱敏（严禁直接公开现有私有仓历史）、补齐本文件及 `THIRD_PARTY_NOTICES.md` 全部依赖声明（含 `fsrs`、`sherpa-onnx`、`onnxruntime`、`tract-onnx`、`speexdsp` 及 `hm-kws` 随包 Debian glibc/libstdc++）。
