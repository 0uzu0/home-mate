# 部署

当前形态是每台音箱一个核心（本机回环 `127.0.0.1:8932` 默认开启，亦可通过 `HM_GATEWAY_ALLOW_LAN=1` 或运行时管理开关接受局域网/手机多网关并发接入），也可通过根目录 `Dockerfile` / `docker-compose.yml` 在 NAS 上以容器运行独立核心。不要安装 `deploy/home-mate.service`：它是早期 NAS systemd 存档，不再受支持。

## 音箱（当前）

| 路径 | 作用 |
|---|---|
| `deploy/build-speaker.sh` | 在开发机打出 aarch64 musl 核心和控制台，放到 `speaker/` 目录 |
| `deploy/speaker/hm-core-supervisor.sh` | 音箱上拉起核心。网关绑定 `127.0.0.1:8932`，控制台在 `0.0.0.0:8930` |
| `deploy/bootstrap/install.sh` | 空机种子：把监督器放到 init 会执行的位置。种子不是已激活的发布 |
| `deploy/release/README.md` | 签名发布包：离线签名、装到设备、激活和回滚 |

日常升级走签名包，不走下面的 NAS 打包。空机第一次起来用种子脚本，之后用 `hm-release install --activate`。

## NAS 时代存档（不可安装）

下面只说明旧包里有什么。没有安装命令。`deploy/package-nas.sh` 仍会把这些文件打进旧的 x86_64 tar，打包行为没改，打出来的 unit 也不能拿去 `systemctl enable`。

| 路径 | 内容 |
|---|---|
| `bin/hm-core` | 当年为 NAS 交叉编译的核心 |
| `web/dist/` | 当年随包的控制台 |
| `deploy/home-mate.service` | 旧 systemd 单元。按原样安装起不来，只留在树里因为别的文档还点这个路径 |
| `deploy/env.local.example` | 密钥模板。填好的 `data/env.local` 永不入库 |

完整的旧步骤记录在 `docs/nas-deployment.md`。那份文档里的安装命令同样不要执行。
