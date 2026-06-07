# 把应用接入 lode

[English](integration.md) · **中文**

接入涉及三个文件,各有唯一归属:

| 文件 | 位置 | 谁写 | 作用 |
|---|---|---|---|
| **`lode.toml`** | 本地 | 运维 | lode 如何拉取并运行你的应用 |
| **`state.json`** | 本地(`$DATA_DIR`) | lode **与** 应用 | 运行时通信(状态 ↔ 请求) |
| **发布源** | 远程 | 发布方 | 已签名的资产清单 —— 原生 `manifest.json` **或** GitHub Releases |

下面三步 —— **配置 → 运行 → 发布** —— 就是完整的接入。运维点名要装哪个资产
(`[update].asset`),该文件名在两个源里都是选择 key。完整签名规范见
[source-adapters.zh-CN.md](source-adapters.zh-CN.md);穷尽字段见
[`lode.example.toml`](lode.example.toml) 与 [`manifest.example.json`](manifest.example.json);
深入设计见[架构文档](architecture.zh-CN.md)。

---

## 1. 配置 lode(`lode.toml`)

运维的文件:*如何拉取与运行你的应用*。应用从不写它。优先级
`CLI > 环境变量(LODE_*) > lode.toml > 默认值`;默认 lode 读取 `/srv/lode/lode.toml`
(用 `LODE_DATA_DIR` 改基目录),首次运行会在那里生成一份起始配置。

```toml
[global]
app      = "myapp"          # 必须与 manifest 的 "name" 一致
data_dir = "/srv/lode"      # 存放 lode.toml + versions/ + state.json + lode.pid + runtime/

[update]
github   = "owner/myapp"                                        # GitHub Releases ……
# manifest = "https://releases.example.com/myapp/manifest.json" # …… 或原生 manifest(二选一)
asset    = "myapp-linux-x64.tar.gz"   # 本机要的资产文件名(选择 key)
channel  = "stable"         # github:stable=/releases/latest,否则最新 prerelease ; 原生:通道名
policy   = "auto"           # off | check | auto
# pin    = "1.4.2"          # 锁定版本(关闭自动更新)
# entry  = "bin/myapp"      # 覆盖归档内 entry;通常省略(默认 {app} 在根)

[trust]
require_signature = "enforce"                       # off | auto | enforce
trusted_keys = ["<key_id>:<base64-公钥>"]           # 来自 `lode-cli keygen`

[command]
run     = "{entry}"         # 裸跑 `lode` → 启动应用({entry} = 安装后的路径)
exec    = "{entry}"         # `lode <args>` → 直通基准
# workdir = "{dir}"         # 可选;省略即版本目录(默认)。需固定部署目录(如读 .env)可写绝对路径

[supervise]
readiness    = "state"      # none | state(仅当应用自报就绪后才提交该版本)
health_grace = 15           # 新版本须存活满的秒数才算 good(否则回滚)
stop_timeout = 10           # SIGKILL 前的优雅停止窗口
restart      = "off"        # off(镜像子进程)| on-failure | always
```

常见形态:

- **自带二进制:** `run = "{entry} serve"`、`exec = "{entry}"`。
- **脚本 + 运行时:** `run = "bun run"`、`exec = "bun"`,再加 `[runtime]` 段,PATH 上没有 `bun` 时下载它——下载后缓存复用,可用 `version` 锁版本;注意运行时下载**不做签名校验**。
- **私有源:** 加 `[http].headers = ["Authorization: Bearer ${TOKEN}"]` —— 随 manifest 与产物请求发送,展开 `${ENV}`。

全部选项及 `[runtime]`/`[signals]`/`restart_*` 见 [`lode.example.toml`](lode.example.toml)。

---

## 2. 应用契约(`state.json`)

*你的应用*要实现的部分。任意语言 —— 读写一个 JSON 文件 + 处理 `SIGTERM`。

**lode 注入的环境变量:** `LODE_ACTIVE_VERSION`(当前版本)、`LODE_DATA_DIR`
(`state.json` 在 `$LODE_DATA_DIR/state.json`)、`LODE_INSTANCE`(本次启动唯一号 —— 写入
`state.ready`)。宿主环境(如 `PORT`)原样透传;内部 `LODE_*` 已剥离。operator 还可用 `[env]`
表追加变量——它们是**默认值**:同名的宿主 env(如部署时 `-e PORT`)会覆盖它们,而 lode 上述三个变量始终最高。

**state.json** —— lode 写状态、应用写请求,字段不重叠:

```jsonc
{
  // lode 写(应用读):
  "current": "1.4.2", "last_good": "1.4.2", "available": "1.5.0",
  "status": "running",        // starting|running|updating|rolling-back|stopping|stopped|error
  "pid": 12345, "last_check": "…", "last_error": null,
  // 应用写(请求 / 就绪):
  "target": null,             // 某版本或 "latest" => 请求升/降级
  "restart_nonce": 0,          // 递增 => 重启当前版本
  "ready": null               // 写成 LODE_INSTANCE => "我能服务了"
}
```

实现以下契约(除 `SIGTERM` 外均可选,但推荐):

- **优雅停止(必需):** 收到 `SIGTERM` 后排空并在 `stop_timeout` 内 `exit(0)`,否则被 `SIGKILL`。lode 会先置 `status = updating|stopping` 供你区分。
  ```ts
  process.on("SIGTERM", async () => { await drain(); process.exit(0) })
  ```
- **就绪(当 `readiness = "state"`):** 真正能服务后,原子写 `state.ready = LODE_INSTANCE`(临时文件 + rename,保留 lode 的字段)。在此之前 lode 不提交该版本(零停机模式下也不停旧实例);超过 `ready_timeout` 未就绪 → 回滚。
- **健康:** 启动失败要 `exit(非0)`。新版本若在 `health_grace` 内退出,回滚到上一个 good(单次触发)。
- **自报版本**(如 `GET /version`),与 `LODE_ACTIVE_VERSION` 一致。
- **请求更新/重启(可选):** 原子改写 `state.json` —— 设 `target`(版本或 `"latest"`)或递增 `restart_nonce`。lode 轮询文件 mtime(~1s)并执行;文件本身即通知。

> 可运行的 Rust + Bun 示例见 [`../tests/apps`](../tests/apps)。

---

## 3. 发布发布源

lode 解析 **channel → version → asset**,校验后安装/运行。每台主机装哪个资产由**文件名**
(`[update].asset`)决定,每个资产都带一个对规范消息
`lode.artifact.v1\n{name}\n{version}\n{sha256}`(UTF-8、`\n` 分隔、无结尾换行)的 ed25519
签名。`name` 是资产文件名。完整规范(含原生 manifest 形状与字段表)见
[source-adapters.zh-CN.md](source-adapters.zh-CN.md)。

打包 + 签名是**发布方**的事,可在任意 CI 完成。`lode-cli` 是参考实现;任何产出相同签名的
ed25519 工具效果一致。

### 密钥(一次性)

`lode-cli keygen` 打印 `key_id`、`trusted_keys` 条目(`<key_id>:<base64>`,交给运维)、以及
保密种子 —— 离线保存。

### GitHub Releases(`github = "owner/repo"`)

把这份 workflow 放进**你的应用**仓库。它构建你的资产,并**仅当配置了签名密钥时**才对每个
资产签名、把签名作为资产 `label` 上传。没有 key 时上传未签名版本,所以在你采用签名之前也能用。

```yaml
# .github/workflows/release.yml —— 为 lode 发布你的应用资产
on:
  release:
    types: [published]      # 建 release(UI 或 `gh release create`);本 workflow 附加资产
permissions:
  contents: write
jobs:
  release:
    runs-on: ubuntu-latest
    env:
      GH_TOKEN: ${{ github.token }}
      TAG: ${{ github.event.release.tag_name }}
      LODE_SIGNING_KEY: ${{ secrets.LODE_SIGNING_KEY }}   # 可选 —— 设了才启用签名
    steps:
      - uses: actions/checkout@v4

      - name: Build assets                # -> dist/<app>-<os>-<arch>.<ext>(由你提供)
        run: ./build.sh "$TAG"

      - name: Publish(仅当配置了 key 才签)
        run: |
          set -euo pipefail
          if [ -n "${LODE_SIGNING_KEY:-}" ]; then
            curl -fsSL https://github.com/dotns/lode/releases/latest/download/lode-linux-x64.tar.gz \
              | tar -xz lode lode-cli                 # 取 lode-cli 用于签名
          fi
          for f in dist/*; do
            if [ -n "${LODE_SIGNING_KEY:-}" ]; then
              sig=$(./lode-cli sign "$f" --version "$TAG" --key-env LODE_SIGNING_KEY)
              gh release upload "$TAG" "$f#$sig" --clobber     # label = 签名
            else
              gh release upload "$TAG" "$f" --clobber          # 未签名
            fi
          done
```

- **启用签名:** `lode-cli keygen` 一次;把保密种子放进仓库的 `LODE_SIGNING_KEY` secret
  (离线另存一份),并把公开的 `trusted_keys` 条目交给运维。没设 secret → 资产以未签名上传
  (在你采用签名前没问题;签名分支不会执行)。
- lode 选 `name` 等于运维 `[update].asset` 的资产;`sha256` 取自资产 `digest`(对字节复验),
  `version` 取自 tag。`channel = stable` → `/releases/latest`;其它 channel → 最新非草稿
  prerelease;`pin` → 指定 tag。无需 `manifest.json` 资源。私有库:token 放 `[http].headers`。
- 资产命名 `<app>-<os>-<arch>.<ext>`;每台主机的运维把 `[update].asset` 设为本机对应的确切
  文件名。

### 原生 manifest(`manifest = "https://.../manifest.json"`)

托管一份 `lode/v1` manifest,其每版 `assets[]` 按 `name`,外加资产托管在任意 HTTPS URL:

```bash
lode-cli manifest "$f" --version 1.5.0 --url "$URL" --entry bin/myapp \
    --key private.key --into manifest.json   # 按 name upsert 资产,设 channels.latest
lode-cli manifest-sign --into manifest.json --key private.key   # 对目录签名
```

manifest 形状 + 逐资产字段表见 [source-adapters.zh-CN.md §6](source-adapters.zh-CN.md)。
`channels.<c>.latest` 必须被签(`manifest-sign`)或由运维 `pin` 死版本。

### 签名模型(两源通用)

- artifact 签名只绑定 **`name`(文件名)/ `version` / `sha256`**。`format` 从文件名后缀推导
  (`.tar.gz`/`.tgz` → tar.gz、`.gz` → gz、`.zip` → zip、否则 raw)。`entry` 是 **advisory**、
  **永不签名**(优先级:manifest 提示 > `lode.toml [update].entry` > `{app}` 在归档根)。
- `require_signature = enforce` 下,每个安装的资产都必须带有效签名(github:`label`;native:
  `sig` 字段或 `.sig` sidecar)。`auto` 一旦配置了任一受信公钥即 fail-closed;无公钥时安装为
  **UNVERIFIED** 并告警。

### 清单

- [ ] 每台主机的 `[update].asset` 写明本平台对应的确切资产文件名。
- [ ] `sha256` 针对原始文件;`sig` 针对 `name/version/sha256`,`key_id` 受信。
- [ ] github:签名设为资产 **`label`**。native:`sig` 内嵌或 `.sig` sidecar,且最后一次改目录后重新 `manifest-sign`。
- [ ] `channels.<c>.latest` 指向真实版本(native),或 tag/latest 可解析(github)。
- [ ] 私钥离线;运维只持公开的 `trusted_keys` 并设 `require_signature = enforce`。
