# 音乐服务：可配置的曲库 + 在线源（2026-11-12 拍板）

> 用户 2026-11-12 拍板：**NAS 音乐与个人音乐服务都必须由普通用户在后台手动配置**，
> 不允许通过代码嵌入地址/账号；为此要有管理 API ＋ 控制台卡片 ＋ 连接探针，属"底座＋工具"。
> 参考 A（`home-mate` 冻结仓 `src/tools/music.rs`，287 行，两种连接都接入了）与 RUI-local
> 的点歌优先级。**本轮只写路线与判据，不写实现**（R1 剩余工作下一会话收尾）。

## 1. 现状（B）

| 项 | 状态 |
|---|---|
| `music.search` / `music.play` | `crates/hm-core/src/tool/mocks.rs` 里的**模拟曲库**（`t01`…），`music.play` 走 media 通道但只播静音 |
| 服务配置 | 只有 `llm/llm_fallback/asr/tts` 四类（`crates/hm-core/src/config.rs`），**没有 music** |
| 控制台 | 服务页四张卡，无音乐卡 |
| RUI-local | `source: private`＋`/var/lib/rui/lan_media.json`；`music:` 块含 `cache_dir`/`queue_*`/`resume_hold_ms`；点歌**本地优先** |
| A | `Music { library: Option<Connection>, online: Option<Connection> }`，`Connection{endpoint,user,secret}`；`library` 走 Subsonic/OpenSubsonic，`online` 走 Personal Services |

## 2. 目标形态（照 A，但走 B 的配置契约）

```
config.music = {
  enabled: bool,
  prefer_local: bool,          // 点歌优先级：本地命中就不再走在线（RUI-local 语义）
  library: { endpoint, user, secret },   // NAS Navidrome（Subsonic/OpenSubsonic）
  online:  { endpoint, secret },         // NAS 上的 Personal Services
}
```

- **凭证只经管理 API 写入**，读回只报 `secret_set: true/false`（与既有 `api_key_set` 同规矩），
  **不进 Git、不打日志**。
- 两类连接各自可禁用；只配一类就只用一类；两类都没配时 `music.play` 必须如实回
  `music_unconfigured`，**不允许假装播放**（mock 曲库同时下线）。
- 端点必须是 `http(s)://host:port`，只允许私网/LAN 地址，避免把凭证发给外部（与 A 的
  `valid_url` 同思路）。

## 3. 接口与工具合同

| 形状 | 内容 |
|---|---|
| `GET /api/v0/config/services/music` | 回读：`enabled`/`prefer_local`/`library{endpoint,user,secret_set}`/`online{endpoint,secret_set}` |
| `PUT /api/v0/config/services/music` | 写入（`secret` 缺省＝保留原值，与其它服务一致）；写后落 `config_history`（scope=`music`） |
| `POST /api/v0/config/services/music/test` | 真探针：library 走一次 `search3`（空 `query` 取一条），online 走一次 `search`；回 `ok/latency_ms/count/detail`，**不播音频** |
| `music.search` | 入参 `query`/`artist`/`limit`；**先 library**，`prefer_local` 且本地 ≥1 命中时不再请求 online；每条结果带 `source`（`library`/`online`）与 `id`，模型不得自造 id |
| `music.play` | 入参 `source`＋`id`（必须来自本轮 search）；经 media 通道播；回执带 `device_confirmed` 才算"出声" |

Subsonic 鉴权（A 已验证）：`u=user&t=md5(password+salt)&s=salt&v=1.16.1&c=home-mate&f=json`，
搜索 `/rest/search3.view`，取流 `/rest/stream.view?id=…`。

## 4. 判据（写死在测试/日志里）

| # | 判据 | 方式 |
|---|---|---|
| MU-1 | 未配置时 `music.play` 回 `music_unconfigured`，**0 次播放**、0 次静音假播 | 单测＋真机 |
| MU-2 | 后台填 endpoint/user/口令 → 读回 `secret_set:true`、口令不回显 | API 测试＋浏览器 |
| MU-3 | 探针在 LAN 上真连一次：`ok:true`、`count>0`、`latency_ms` | 真机 |
| MU-4 | 点名一首歌（本地曲库里有）→ media 通道 `device_confirmed`＋`played_ms>0`，**耳朵听到** | 真机（用户在场） |
| MU-5 | `prefer_local=true` 且本地有命中 → 在线源 **0 次请求** | 单测（假 HTTP）＋日志 |
| MU-6 | 点名一首本地没有的歌 → 只有在用户同意/明确要求在线时才请求 online；否则如实说"本地没有" | 单测 |
| MU-7 | 口令不出现在日志、`config_history` 明文、trace | grep 断言 |

## 5. 实施顺序（下一会话）

1. `config.rs` 加 `music` 块（含默认值与 `secret` 同款存储）；`config_history` scope。
2. `tool/music.rs`：Subsonic 客户端（search3/stream）＋ Personal Services 客户端（search/stream，
   协议照 A）＋ `prefer_local` 选择逻辑；删掉 mock 曲库的"假装播放"。
3. 控制台音乐卡（中英双语）：启用、本地优先、两类连接的 endpoint/user/口令、"测试连接"。
4. 判据 MU-1…MU-7 逐条过；MU-4 需用户在场听。

## 5.1 在线源（Personal Services）自己的后台（用户 2026-11-12 夜追加，未实施）

> 用户原话："在线源的 personal service 后面要安排一个后台"。

分界不变（[interfaces-and-tool-layer](interfaces-and-tool-layer.md) 第 53 行）：**上游凭证与采集归 Personal
Services 自己**，Home Mate 的控制台只填"地址＋token"并做真探针。所以那个后台是 Personal Services 的工程，
不并进 Home Mate 的工具后台（B-M6）。

今天只能改 NAS 上的 `.env`（`INTERNAL_TOKEN`、`NAVIDROME_*`、`MUSIC_*_LIMIT`、`MUSIC_ACQ_RESOLVER`…）再重启
容器；`/health` 只回预算计数器。最小可用后台应能：①看/改上游与 resolver 配置（不重启生效）②预算窗口
（search 60/5min、resolve 50/5min、stream 120/5min、并发 4）可视化并可调 ③入库/归档任务与历史（`music-acq`）
④客户端 token 轮换（现在只有一个 `INTERNAL_TOKEN`）。判据：改完不需要重启，`/health` 反映新值，
Home Mate 的 `POST /api/v0/config/services/music/test` 仍然 `ok:true`。

## 6. 凭证（不入 Git）

用户在 2026-11-12 口述了 Navidrome 的 endpoint 与账号，口令由用户在控制台填写；
本文档与仓库**不记录口令**。部署时通过 `PUT /api/v0/config/services/music` 写入设备。
