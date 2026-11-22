# NAS 部署指南（horner-nas，M4）

日期：2026-11-07（M4 首次部署，全部步骤已按本文实做并验证）。目标机：荣耀 NAS horner-nas `192.168.1.34`（Debian 12.12＋fnOS，x86_64，Ryzen 5 2500U 8 线程，6.7GB RAM，登录信息查 `AI_Workspace/01_Code/device-inventory.md`）。

## 0. 隔离边界（部署不变式）

- **独立目录**：`/home/yes999zc/home-mate/`（二进制＋控制台＋SQLite 数据＋env.local 全在内）。卸载＝`systemctl disable --now home-mate`＋删该目录＋删 `/etc/systemd/system/home-mate.service`。
- **独立端口**：8930（管理 API＋控制台，同端口）、8932（网关监听）。部署前实测扫描均空闲（该机既有服务占用 21/22/80/443/139/445/111/2049/5432/5666/5667/7890/7892/8098/8099/8899/9024/9090/5005/5006 等，互不相交）。**任何端口变更前重新扫描**。
- **不触碰**：Hermes、mihomo、fnOS 服务与 `/vol1` 存储池；不做路由器端口映射（管理 API 无鉴权，仅限家庭内网；绑定的是内网地址 `192.168.1.34` 而非 `0.0.0.0`）。
- **系统级改动仅一项**：新增 systemd unit `/etc/systemd/system/home-mate.service`（`deploy/home-mate.service`，`Restart=on-failure`＋开机自启）。注意 fnOS 用户主组是 `Users`（gid 1001）而非同名组——unit 里只写 `User=`，不写 `Group=`（2026-11-07 实测 `Group=yes999zc` 会 216/GROUP 起不来）。

## 1. 产物构建（开发机，Apple Silicon）

```bash
./deploy/package-nas.sh     # cargo zigbuild --target x86_64-unknown-linux-gnu.2.17 + npm build
# → target/dist/home-mate-nas-<git-describe>-x86_64.tar.gz（~3.6MB）
```

工具链说明：`cross` 0.2.5 官方镜像无 `linux/arm64` manifest（Apple Silicon 上 docker pull 失败），故用 `cargo-zigbuild`（brew 安装）＋zig linker，产物兼容 glibc 2.17+（Debian 12＝2.36，充分覆盖）。二进制无运行时依赖（SQLite bundled、rustls）。

## 2. 部署步骤（已验证顺序）

```bash
# ① 前置检查（端口必须空闲、目标目录干净）
ssh yes999zc@192.168.1.34 'ss -tln | grep -E ":8930|:8932" || echo FREE; ls -d ~/home-mate 2>/dev/null || echo clean'

# ② 传输解包
scp target/dist/home-mate-nas-*-x86_64.tar.gz yes999zc@192.168.1.34:~/
ssh yes999zc@192.168.1.34 'mkdir -p ~/home-mate && tar -C ~/home-mate --strip-components=1 -xzf ~/home-mate-nas-*-x86_64.tar.gz && rm ~/home-mate-nas-*-x86_64.tar.gz'

# ③ 密钥（开发机组装后直传，不打印不入库；模板见 deploy/env.local.example）
#    需要 GLM_API_KEY（取自 ~/.hermes/.env）与 VOLC_API_KEY（取自本仓 data/env.local）
scp <env.local> yes999zc@192.168.1.34:~/home-mate/data/env.local
ssh yes999zc@192.168.1.34 'chmod 600 ~/home-mate/data/env.local'

# ④ systemd（sudo 密码查 device-inventory）
ssh yes999zc@192.168.1.34 'sudo cp ~/home-mate/deploy/home-mate.service /etc/systemd/system/ && sudo systemctl daemon-reload && sudo systemctl enable --now home-mate'
```

## 3. 服务配置（管理 API，与页面同路径）

```bash
N=http://192.168.1.34:8930
# LLM（密钥走 env：GLM_API_KEY）
curl -X PUT $N/api/v0/config/services/llm -H 'Content-Type: application/json' \
  -d '{"provider":"glm","base_url":"https://open.bigmodel.cn/api/paas/v4","model":"glm-4-flash","api_key_env":"GLM_API_KEY"}'
# TTS（密钥走 env：VOLC_API_KEY）
curl -X PUT $N/api/v0/config/services/tts -H 'Content-Type: application/json' \
  -d '{"provider":"volc","model":"seed-tts-2.0","voice":"zh_female_xiaohe_uranus_bigtts","api_key_env":"VOLC_API_KEY"}'
# ASR（开发机 oMLX 内网可达：192.168.1.20:9981；key 走 secrets 表）
curl -X PUT $N/api/v0/config/services/asr -H 'Content-Type: application/json' \
  -d '{"provider":"openai-asr","base_url":"http://192.168.1.20:9981/v1","model":"Qwen3-ASR-0.6B-bf16","api_key":"<omlx-key>"}'
# 三项探针（应全部 ok:true）
for s in llm tts asr; do curl -X POST $N/api/v0/config/services/$s/test; echo; done
```

浏览器控制台：`http://192.168.1.34:8930/`（核心直接托管构建产物，单端口，无需 Node/8931）。

## 4. 已验证行为（2026-11-07 首次部署实测）

| 项 | 结果 |
|---|---|
| systemd 启动 | `active`；`Restart=on-failure` 生效 |
| 三服务探针（从 NAS） | llm 708ms「正常」／tts 667ms 生成 1178ms 音频／asr 308ms「你好，高达。」（跨机到 Mac oMLX） |
| 真实对话轮 | GLM 流式 20 tokens，回答完整，trace 落库（completed） |
| 控制台 | `GET /` 返回构建好的 SPA，静态资源 200＋正确 MIME，未知 /api 路径如实 JSON 404 |
| 干净重启恢复 | `systemctl restart` 后 active，配置/trace 全保留 |
| 崩溃自愈 | `kill -9` 主进程 → systemd 自动换 PID 拉起，数据无损 |
| 跨机设备链路 | Mac 虚拟 dot2-d（`--core 192.168.1.34:8932`）→ NAS 语音轮：GLM 应答→火山 TTS→190KB WAV 回传 Mac 落盘，回执链 accepted→device_confirmed→play_finished(5961ms) 全程 NAS 侧日志可查 |

## 5. 运维备忘

- **升级**：开发机重跑 `deploy/package-nas.sh` → scp 解包覆盖 `bin/`＋`web/`（`data/` 不动）→ `sudo systemctl restart home-mate`。
- **日志**：`sudo journalctl -u home-mate -f`。
- **LLM 读超时**：默认 20s per-chunk，`HM_LLM_READ_TIMEOUT_SECS` 可调（unit 的 `Environment=` 行追加后 daemon-reload＋restart）。
- **ASR 依赖开发机**：oMLX 在 Mac 上（当前监听可内网访问、key 鉴权）；Mac 休眠＝NAS 侧 ASR 探针如实失败（对话文本路径不受影响）。oMLX 的内网暴露范围属用户决策项（见 asr-service-plan §4）。
- **边界**：真实声学（麦克风/唤醒/打断）、dot2-d 实机接入（G2）均未在 NAS 验证；网关进程当前不在 NAS 运行（设备从开发机拨入）。

> **C 阶段状态（2026-11-08）**：NAS 部署路线**退场**——核心已上音箱本体自持（aarch64 musl 静态＋emOS 服务化，docs/HANDOFF §0.10），Mac/NAS 不再是运行时依赖；本文档保留为打包/参考（package-nas.sh 仍可用作 x86_64 打包基线）。
