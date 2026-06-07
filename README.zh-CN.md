# lode

[English](README.md) · **中文**

> 通用的**「校验 · 启动 · 更新」加载器**:一个极小的静态 Rust 二进制,校验已打包应用的
> **完整性**与**发布者身份**,启动、监督并热更新它。把它打进通用镜像一次 —— 换应用只是换一份
> manifest,**永不重建镜像**。

- **镜像:** `docker.io/dotns/lode`([Docker Hub](https://hub.docker.com/r/dotns/lode))
- **二进制:** Linux(x86_64 / aarch64,musl 静态)+ macOS(x86_64 / arm64) —— [Releases](https://github.com/dotns/lode/releases)
- **平台:** 仅 Unix(lode 是进程监督器 —— PID-1 子进程收割、信号转发、`exec` 直通)。

## 从这里开始 —— 按角色

| 你是… | 你想… | 去看 |
|---|---|---|
| **运维** | 在容器里跑一个应用并保持更新 | [快速开始](#快速开始) + [`lode.example.toml`](docs/lode.example.toml) |
| **应用作者** | 让你的应用可被 lode 更新 | [集成 §2 — 应用契约](docs/integration.zh-CN.md) |
| **发布方** | 打包、签名并发布一个版本 | [集成 §3 — 发布版本](docs/integration.zh-CN.md) |
| **想了解原理** | 理解设计 | [架构文档](docs/architecture.zh-CN.md) |

[集成指南](docs/integration.zh-CN.md)覆盖完整链路 —— 配置(`lode.toml`)→ 运行(`state.json`)→ 发布(`manifest.json`)。

完整文档索引(双语):[`docs/`](docs/README.zh-CN.md)。可运行示例:
[`tests/apps`](tests/apps)(一个 Rust + 一个 Bun 服务)与 [`tests/compose`](tests/compose)(实时更新/回滚)。

## 快速开始

让 lode 指向一份已签名的 manifest,再运行通用镜像。默认 lode 读取 `/srv/lode/lode.toml`,
状态也存在 `/srv/lode` 下:

```bash
docker run --rm \
  -v "$PWD/lode.toml:/srv/lode/lode.toml:ro" \
  -e LODE_TRUSTED_KEYS="<key_id>:<base64-公钥>" \
  docker.io/dotns/lode:latest
```

一份最小的 `lode.toml`(全部选项见 [`docs/lode.example.toml`](docs/lode.example.toml)):

```toml
[global]
app = "myapp"
[update]
manifest = "https://releases.example.com/myapp/manifest.json"   # 或:github = "owner/repo"
policy   = "auto"                                               # off | check | auto
[command]
run = "{entry}"                                                 # 如何启动应用
[trust]
require_signature = "enforce"
```

> 首次运行若 `/srv/lode/lode.toml` 不存在,lode 会在那里生成一份起始配置并提示你填写来源。
> 用 `LODE_DATA_DIR` 改基目录。若改用 `--manifest`/`--github`(或 `LODE_*`)则无需配置文件。

要构建你自己的应用镜像,把 lode 叠到任意基础镜像上:

```dockerfile
FROM oven/bun:1                       # 或任何你的应用所需运行时
COPY --from=docker.io/dotns/lode:latest /usr/bin/lode /usr/bin/lode
ENTRYPOINT ["/usr/bin/lode"]
```

## 工作原理

```
通用镜像                  ┌─────────────────────────────────────┐
zzci/ubase         ────► │  lode  (静态 Rust 二进制)            │
                         └───────────────────┬─────────────────┘
                                             │ lode.toml + 环境变量 + CLI
                                             ▼
   [update].manifest ──HTTPS(+headers)──► manifest.json  (channels → versions → assets[name])
                                             │  (远程;从不落本地)
                              选择平台 ──────┤── 下载 → 校验 sha256 + ed25519
                                             ▼
                    $DATA_DIR/versions/<ver>  ──(原子 rename)──► current
                                             │
                                             ▼
              lode            → 执行 `run`  (受监督服务:自动更新 + 回滚)
              lode <args…>    → 执行 `exec` + <args>  (一次性 CLI 直通)
```

## 一个二进制,两种身份

lode 是 **multi-call 二进制**。以 `lode` 调用是加载器,**没有任何子命令** —— 每个参数都属于应用,
加载器绝不遮蔽应用 CLI。以 **`lode-cli`**(随之发布的软链接)调用则是运维 / 发布工具箱。

| 调用 | 作用 |
|---|---|
| `lode` | 启动并监督应用(`[command].run`);按策略自动更新 |
| `lode <args…>` | 直通:执行 `[command].exec` + `<args>`(如 `lode run db:init`) |
| `lode-cli status` / `update` / `rollback` / `restart` / `versions` | 管理运行中的实例(经 `state.json`) |
| `lode-cli keygen` / `sign` / `verify` / `manifest` / `init` | 发布方 / 运维工具 |

## 三个文件

- **`lode.toml`** —— 本地 TOML;运维的配置(如何拉取与运行)。应用从不写它。→ [`docs/lode.example.toml`](docs/lode.example.toml)
- **`state.json`** —— 本地 JSON;运行时通信。lode 写状态,应用写请求(`target`/`restart_nonce`/`ready`)。→ [集成 §2](docs/integration.zh-CN.md)
- **`manifest.json`** —— 远程 JSON;已签名的版本目录(从不落本地)。→ [`docs/manifest.example.json`](docs/manifest.example.json)

## 关键行为

- **更新** `[update].policy = off | check | auto`;来源是 `manifest`(原生 `lode/v1` JSON)**或** `github = "owner/repo"`(Releases)。
- **回滚** —— 新版本若在 `health_grace` 内退出,回滚到上一个已知良好版本(单次触发)。
- **重启** `[supervise].restart = off | on-failure | always` —— `off`(默认)镜像子进程;lode 主动发起的更新/回滚/重启总会重新拉起。
- **信任** —— `sha256` + `ed25519`;设 `[trust].trusted_keys` + `require_signature = off | auto | enforce`。签名是发布方的事 —— 见[集成 §3](docs/integration.zh-CN.md)。
- **私有源** —— `[http].headers`(支持 `${ENV}` 展开)随每次拉取发送。

## 从源码构建

```bash
cargo build --profile dist --target x86_64-unknown-linux-musl    # 静态发布二进制
cargo fmt --check && cargo clippy --all-targets && cargo test    # 门禁
cd tests && bun install && LODE_BIN=../target/debug/lode bun test src/   # e2e
```

技术栈遵循 **pma-rust**(edition 2024、`#![forbid(unsafe_code)]`、deny-warnings、rustls + aws-lc-rs、musl + `+crt-static`)。

## 许可证

MIT
