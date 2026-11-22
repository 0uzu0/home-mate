# ASR 本地 API 服务接入方案

日期：2026-11-07（M3.5 产出方案；**M4 当日实现并接入**，见文末「M4 落地记录」）。状态：**已实现**——核心侧 `openai-asr` provider 上线（提交 `1039ed4`），接入的引擎为开发机 oMLX 的 Qwen3-ASR（用户 2026-11-07 晚下载）。

## 0. 决定与边界（用户 2026-11-07 决定）

- LLM/ASR/TTS **三服务全部以 API 形态接入**：当前 LLM=GLM 云端、TTS=火山云端；**ASR 在开发机（Mac）上起本地服务暴露 API**，核心经服务页 asr 配置接入。
- 核心无论部署到 NAS 还是音箱，都只依赖这三个服务接口；ASR 服务与核心的物理分离是既定拓扑，不是临时安排。
- 本方案只覆盖 ASR 的**转写**职责（utterance→文本）。唤醒词（KWS）属于网关侧设备能力，VAD/分帧由语音管线管理，均不在 ASR API 合同内。

## 1. 现状接缝（已存在，无需改动合同）

| 接缝 | 现状 |
|---|---|
| trait | `crates/hm-core/src/speech.rs` `AsrClient { async fn transcribe(&self, wav: &[u8]) -> Result<String>; fn provider(&self) -> &str }`（M2 留位，当前只有 `MockAsr`；`asr_from_config` 直接返回 mock） |
| 配置 | `config.asr: ServiceConfig`（provider/model/base_url/api_key 字段齐全，密钥入 secrets 表只写不读）；服务页 asr 区块已可配置 |
| 连接测试 | `POST /api/v0/config/services/test` 的 asr 分支当前返回"随实机音频输入提供"占位 |

接入＝新增一个真实 `AsrClient` 实现＋`asr_from_config` 按 provider 分派＋asr 测试分支改为真实探针（发送一小段内置 WAV，断言返回非空文本）。**合同不变。**

## 2. 引擎候选（公开资料，未实测）

| 候选 | 形态 | API | 判断 |
|---|---|---|---|
| **whisper.cpp `whisper-server`**（ggml-org，MIT） | 官方自带 HTTP server | OpenAI 兼容 `POST /v1/audio/transcriptions`（multipart `file` 字段＋可选 `model/language`，响应 `{"text": "..."}`）＋ `GET /load` 运行时换模型 | **主候选**：官方维护、MIT、协议即 OpenAI 形态；Apple Silicon 可用 Core ML 加速。注意个别构建的响应形状差异需实测核对（`{"text"}`） |
| MLX-whisper 封装服务（如 mlx-api、whisper-server-apple-silicon 等） | 社区项目，MLX 加速 | 多为 OpenAI 兼容 | Mac 上更快，但均为小型社区项目（bus factor 1），作为性能不达标时的备选，不首选 |
| **oMLX 本机服务**（127.0.0.1:9981，已在运行） | 本机推理服务 | OpenAI 兼容 `/v1`；`/v1/audio/transcriptions` **已于 2026-11-07 探测确认支持**（用户下载 ASR 模型后） | **已接入（M4）**：`Qwen3-ASR-0.6B-bf16` 与 `Qwen3-ASR-0.6B-4bit` 均实测可用（10/10 口语句式全对；预热后延迟 ~75-105ms/短句）；whisper.cpp 不再需要安装。第三变体 moona3k/mlx-qwen3-asr-0.6b-4bit 因缺 `preprocessor_config.json` 加载失败（已补文件，待 oMLX 重启清缓存后复测），不用它 |
| 云端 ASR（SiliconFlow，`SILICONFLOW_API_KEY` 在 hermes env） | 在线 API | OpenAI 兼容 | **已切换（C 阶段 2026-11-08，主用）**：`FunAudioLLM/SenseVoiceSmall`，配置＝provider `openai-asr`＋`https://api.siliconflow.cn/v1`，经管理 API 写入（控制台服务页可见可改可探针）。实测（`scripts/asr/asr-colloquial-10.py`，TTS 合成刺激的无人值守代理）：口语 10/10（归一化口径＝去 SenseVoice 情绪 emoji＋数字映射，原始严格匹配 6/10，差异均为格式非内容）；延迟 p50 ~530ms / p90 3.6s / 偶发服务端多秒尖刺（冷实例排队特征，如实记账）；真人抽查留用户。本地 oMLX 降为备选（Mac 退场后不可用） |

选型规则：**协议优先于引擎**——所有候选都走 OpenAI `/v1/audio/transcriptions` 形态，核心侧只实现一个 `openai-asr` provider，引擎在服务端可替换（whisper.cpp ↔ MLX ↔ 云端），不给任何引擎写专用分支。

## 3. 核心侧设计（实现清单，随下个里程碑）

1. **provider `openai-asr`**（`speech.rs`）：`transcribe(wav)` → `POST {base_url}/v1/audio/transcriptions`，multipart `file`（wav 字节）＋`language=zh`（可配）；响应 `{"text"}` trim 后返回；空文本如实返回空串由上层决定处理；HTTP/网络错误映射 `SpeechError`（与 TTS 适配器同风格；无重试——转写是一次性输入，失败如实上浮）。
2. **`asr_from_config`**：`provider=="openai-asr"` 且 base_url 非空 → 真实适配器；否则 mock（现行为）。
3. **服务页 asr 测试**：内置 1–2 秒样例 WAV（仓库内测试资产，非私人录音）→ 真实转写 → 返回 `ok + 转写文本 + 耗时`；服务不可达如实失败。
4. **语音管线消费**：设备/虚拟终端的 `audio_in` 到达后按轮聚合 WAV → `AsrClient::transcribe` → 文本进对话轮（半双工路径与现有文本入口共用后续链路）。mock 时期已验证的轮次归属/停止语义不因 ASR 接入改变。
5. **配置示例**：`provider=openai-asr, base_url=http://127.0.0.1:<port>/v1, model=whisper-1（或服务端模型名）, api_key=<可空，本地服务>`。

## 4. 部署拓扑与端口

- **开发机（Mac）**：ASR 服务监听本机端口，建议 `127.0.0.1:8978`（避开 8930 核心 / 8931 控制台 / 8932 网关 / 9981 oMLX / 8977 调研用 wire-mock）。
- **NAS 核心调用 Mac**：核心部署 NAS 后走 LAN `http://<mac-ip>:8978/v1`。whisper.cpp server 的鉴权能力（api-key header 等）**需在部署时核实**；家庭 LAN 内若服务无鉴权，绑定具体内网地址＋不对外映射端口为最低要求，正式方案在 M4 部署文档一并定。
- 模型文件（如 ggml large-v3-turbo / distil 版本）与服务进程由开发机侧管理，不进本仓；启动脚本与版本在部署时记录。

## 5. 验收标准（实现里程碑时执行）

1. 服务页配置 `openai-asr` 后连接测试返回真实转写文本与耗时（真实入口验收）。
2. 虚拟 dot2-d 注入样例 WAV → 文本进入对话轮 → LLM 应答（全链路，无 mock ASR）。
3. 中文短句（含唤醒后口语句式）转写抽样正确率可接受（人工核对 10 句）。
4. 服务停止时：连接测试如实失败、对话轮明确报 ASR 不可用，不静默挂死（对齐 lessons 横切 4）。
5. 凭证/样例音频不入 Git；服务端模型文件路径记录在部署文档。

## 6. 未决项（不阻塞方案）

- ~~oMLX 是否已支持 `/v1/audio/transcriptions`~~ **已确认支持（2026-11-07）**，见 §2 与文末记录。
- oMLX 的内网暴露范围（当前 Mac 上监听可内网访问、有 key 鉴权）：家庭 LAN 内可用，最终暴露策略（绑定地址限定等）属用户决策项，正式结论随下一轮部署文档定。
- 流式转写（分片上传/WS）：当前按整段 utterance 转写设计；流式留给真实声学验证后再评估，不在首版合同。
- whisper.cpp server 具体构建的响应形状与鉴权字段：**不再需要**（oMLX 已覆盖本地路径），留作 oMLX 不可用时的备选，届时再实测。
- 多说话人/声道合并、参考通道处理：dot2-d 实机（G2 只读准备）阶段的事实，不影响本 API 合同。

## 7. 参考

- whisper.cpp server 示例（官方）：https://github.com/ggml-org/whisper.cpp/tree/master/examples/server
- Apple Silicon 本地 Whisper 指南（Core ML 加速）：https://www.onresonant.com/resources/whisper-for-mac
- MLX OpenAI 兼容封装示例：https://github.com/duytechie/mlx-api 、https://github.com/chaosslabs/whisper-server-apple-silicon

## 8. M4 落地记录（2026-11-07，提交 `1039ed4`）

- **实现**：`OpenaiAsr`（`speech.rs`，multipart file+model+language=zh，`{"text"}` trim，空文本如实返回空串，60s 总超时，无重试）；`asr_from_config` 按 provider 分派（空 provider＝mock 不变）；服务页 asr 测试＝真实探针（内置 39KB 合成样例 WAV `crates/hm-core/assets/asr-sample-zh.wav`＝macOS Tingting「你好高达」，非私人录音）；依赖仅启用 reqwest/axum 的 multipart feature，**零新 crate**。
- **验收对照（§5 五条）**：①服务页探针真实文本＋耗时 ✅（Mac 263ms／NAS 跨机 308ms「你好，高达。」）；②虚拟 dot2-d 注入 WAV 全链路——**未做**（虚拟终端按 M2 约定不伪造 audio_in；语音管线消费 ASR 属真实音频输入接入时点，保持方案 §3-4 设计）；③中文短句抽样 ✅ 10/10（bf16 与 4bit 双模型，含唤醒词＋口令句式，标点正确）；④服务停止如实失败 ✅（探针端点对不可达返回 ok:false＋错误详情，TTS/LLM 同风格）；⑤凭证/样例不入 Git ✅（key 走 secrets/env，样例为合成语音）。
- **双模型实测**（oMLX @Mac，2026-11-07）：`Qwen3-ASR-0.6B-bf16` 与 `Qwen3-ASR-0.6B-4bit` 均 10/10；预热后短句延迟 ~75-105ms，两模型持平。当前生效配置＝bf16（NAS 与开发机一致）。
- **moona3k 4bit 变体**：加载失败（HF 转换仓库缺 `preprocessor_config.json`；已从上游 Qwen/Qwen3-ASR-0.6B 补入该文件——纯新增，上游本就无 tokenizer.json/special_tokens_map.json）；oMLX 对加载失败有粘性缓存，unload/load 均不清除，**需 oMLX 服务重启后复测**（共享服务，重启由用户决定）。

## 9. 火山账号权限实测（2026-11-10，`VOLC_API_KEY`）

探测方式：每个资源只发最小请求（1 字 TTS／605ms 语音流式 ASR），记录原始 code/message，以区分「未授权」与「请求格式错」。

| 资源 ID | 结论 | 证据 |
|---|---|---|
| `seed-tts-2.0`（TTS 单向流式） | ✅ 已开通 | code 0，返回 PCM |
| `volc.seedasr.sauc.duration`（流式 ASR，按音频时长计费） | ✅ 已开通 | 605ms「你好，高达」→ partial「高达」(definite=false) → 最终「高达。」(definite=true)，含 word 时间戳（260–340ms） |
| `volc.bigasr.sauc.duration`（同上，别名） | ✅ 已开通 | 与 seedasr 返回一致 |
| `volc.seedasr.sauc.concurrent`（流式 ASR，按并发计费） | ❌ 未开通 | `quota exceeded for types: concurrency` |
| `volc.bigasr.sauc.concurrent` | ❌ 未开通 | 403 `requested resource not granted` |
| `volc.bigasr.auc`（批量 ASR 提交） | ❌ 未开通 | 403 code 45000030 resource not granted |
| `volc.bigasr.auc_turbo` | ❌ 未开通 | 同上 |
| `seed-tts-1.0` / `volc.service_type.10029`（旧版 TTS） | ❌ 未开通 | 45000030 resource not granted |
| `volc.speech.dialog`（端到端实时对话） | ❌ 未开通 | 403 resource not granted |
| 声音复刻（voice clone） | 未测（REST 训练接口，路径不同；如需另测） | — |

**对 T2 实测矩阵的影响（重要）**：
1. **火山没有批量 ASR 权限** → 「火山 nostream 口」基线做不了；矩阵改为 **火山流式（唯一真流式）vs 硅基批量（WAN）vs 本地 oMLX 批量（LAN）**。
2. 流式返回带 **`definite` 标志与 word 时间戳** → 「partial 只用于 definite 匹配」的规则可直接落地（唤醒/退下判定用 definite=true 的文本）。
3. 计费为**按时长**；并发资源未购 → 若并发受限，多设备场景需另购并发包（本阶段单台不受影响，但需实测并发上限）。
4. 火山 LLM（豆包/方舟）是另一个产品与 key，本 key 不适用（LLM 仍走 DeepSeek 主＋GLM 备）。
