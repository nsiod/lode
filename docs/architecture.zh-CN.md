# lode 架构文档

[English](architecture.md) · **中文**

> 通用的"升级 + 启动"组件:一个**小型 Rust 静态二进制**(几 MB),语言无关。
> 校验**文件完整性(sha256)**与**发布者身份(ed25519 签名)**,再启动并管理服务,支持无缝热更新与回滚。
> 通用镜像 = `zzci/ubase` + lode 二进制(无需任何语言运行时;运行时若有则启动时下载并缓存);一个实例 = **单程序、单通道**。

本文是权威**架构文档**,实现以本文为准。遵循 **pma-rust** 硬锁(edition 2024、`#![forbid(unsafe_code)]`、rustls+aws-lc-rs、deny-warnings、musl+crt-static、cargo-deny/shear/typos/nextest)。

---

## 1. 目标与定位

- **载入器 (lode)**:固定不变的"升级+启动"组件,编译为一个小静态二进制,打进通用镜像一次,永不重建。职责:配置 → PID 锁 → 决定该跑哪个版本 → 必要时下载/**校验(完整性+身份)**/安装 → 原子启用 → 以**子进程**启动并**管理服务** → 按策略热更新/回滚 → 经 **`state.json`** 与 app 沟通。
- **应用 (app)**:任意语言的程序,打成包并由可信发布者签名,发布到 manifest;lode 校验通过后运行。**lode 不绑定应用语言**(运行方式由 `lode.toml` 的 `[command]` 指定)。
- **为什么 Rust**:Bun 编译版 lode ≈ 91MB(内嵌 JS 运行时),镜像还得拖着它;Rust 静态(musl)≈ 几 MB,可跑在**任意**基础镜像上(scratch、distroless,或像 `zzci/ubase` 这样更全的镜像),这才是真正的"语言无关 + 小"。
- **关系**:`通用镜像(zzci/ubase + lode)` + `远程 manifest/签名包` → 容器启动即按策略校验并运行应用。换应用 = 换 `[update].manifest`(env `LODE_MANIFEST`),镜像零重建。

### 已定方案(硬约束)

1. **单程序、跟随单通道**:一个 lode 只管一个 app;manifest 可定义**多个通道**(`[channels.*]`),lode 实例按 `LODE_CHANNEL` **跟随其一**;版本暴露**具名资产(assets)**,operator 按文件名(`[update].asset`)选其一 —— 无平台探测。
2. **三文件分工**:`lode.toml`(本地 TOML,纯配置,**app 不写**)、`state.json`(本地 JSON,**lode 与 app 都写**,通信中枢)、`manifest.json`(**远程 JSON**,不落本地)。app 通过**读写 `state.json`**(读 `available`、写 `target`/`restart_nonce`)与 lode 沟通;其它实时/RPC 通信属带外,由 app 自理。详见 §7。
3. **下载靠 manifest,运行靠 lode.toml**:artifact 的 `format` 决定如何落地;运行命令 `run`/`exec`(+ 可选 `[runtime]`)写在 lode.toml,**无 `kind`**。
4. **不主动跳版本**:仅当本地无任何已装版本时才下载最新;有版本先起现有版本,更新交策略与 app/CLI 触发。
5. **双层校验**:完整性(sha256)+ 发布者身份(ed25519),按 `trust.require_signature` 强制。
6. **子进程启动 + 服务管理**:lode 永远以子进程拉起并监督,提供起停/重启/状态;就绪/停止握手避免提前杀进程;可选无停机重启(借鉴 overseer)。
7. **统一配置**:`CLI > 环境变量(`LODE_*`) > `lode.toml`(TOML) > 默认值`。
8. **私有源**:`[http].headers` 透传鉴权;私钥/凭据不进镜像。
9. **Rust 模块化**:多文件分模块(见 §14),编译产出单一二进制。

---

## 2. 总体架构

```
通用镜像(构建一次)         ┌──────────────────────────────────┐
zzci/ubase                 │  lode (Rust 静态二进制, 几 MB)   │
                          └────────────────┬─────────────────┘
                  lode.toml (本地配置) │ 读 CLI / env / lode.toml
                                           ▼
   update.manifest ──HTTPS(+headers)──► 远程 manifest.json (channels + versions→assets[name])
                                           │  (远程,不落本地)
                       ┌───────────────────┼────────────────────────────┐
                       ▼                   ▼                            ▼
              按文件名(asset)选资产  下载 → 校验 sha256 + ed25519      受信公钥
                       │                            │ 全过 → 按 format 落地/解包
                       ▼                            ▼
            current ─► versions/<ver>  (rename 原子切换;每版本存元数据供离线)
                       │
                       ▼
            lode 以子进程启动并管理:  <lode.toml [command].run/exec>  (起停/重启/热更/回滚/可选无停机)
                       │                        ▲
            $DATA_DIR/state.json (lode+app 共写) │ 轮询 state.json mtime
                       └────────► app ◄───────────┘ app 读 state.available 提示 / 写 state.target|restart_nonce 触发
```

---

## 3. 运行形态:Rust 模块化 + 小静态二进制

- 多文件分模块(§14),编译产出**单一静态二进制**;`*-unknown-linux-musl` + `+crt-static`,`ldd` 显示 "not a dynamic executable"。
- **镜像**:`FROM zzci/ubase`(通用基础镜像,带 libc/shell/工具,也能承载 lode 启动时下载的运行时)→ `COPY lode /usr/bin/lode` → `ENTRYPOINT ["/usr/bin/lode"]`。lode 的 TLS 根证书是内置的(webpki-roots),无需系统 CA 证书。lode 是静态二进制,在极小基础镜像(`scratch`/distroless)上同样能跑——当 app 自带运行时或本身就是静态二进制时。
- **依赖(纯 Rust / pma-rust 合规,无 tokio/axum,同步实现保持小巧)**:
  - HTTP:`ureq` + `rustls`(aws-lc-rs provider,`install_default()` 于 `main`)
  - 序列化:`serde` + `serde_json`(manifest/state)+ `toml`(lode.toml);版本:`semver`
  - 校验:`sha2`、`ed25519-dalek`、`base64`
  - 解包:`flate2`(miniz_oxide 纯 Rust)+ `tar`(+ 可选 `zip`)
  - CLI:`clap`(derive, env);错误:`thiserror`(+ `anyhow` 仅 main)
  - 日志:`tracing` + `tracing-subscriber`
  - 进程/信号:`std::process` + `signal-hook`(收) + `nix`(向子进程发信号,安全 API)
  - 健壮:`rlimit`(抑制 core dump)
- **`#![forbid(unsafe_code)]`**:fd 传递(可选无停机)经 `command-fds`/`socket2` 封装,本 crate 不写 unsafe。
- 关键原语已在等价的 Bun 原型本地验证(O_EXCL 锁、原子软链、sha256、ed25519、子进程信号、流式下载);Rust 侧用对应安全 crate 实现。

---

## 4. 语言无关的应用模型:format(打包) + run/exec(运行)

职责分工:**落地交给 manifest 的 `format`,运行交给 `lode.toml` 的 `[command]`**。lode 自身只做选取/下载/校验/启用/监督。

### format —— 打包格式(如何把下载物落地)

| format | 含义 | 落地动作 |
|---|---|---|
| `raw` | URL 即最终单文件(二进制或脚本) | 下载 → 存为 `versions/<v>/<entry 或 basename>` |
| `gz` | 单文件 gzip(`.gz`) | 下载 → gunzip → 单文件 |
| `tar.gz` | 目录树的 gzip tar(`.tar.gz`/`.tgz`) | 下载 → 解包到 `versions/<v>/` |
| `zip` | zip 包 | 下载 → 解压到 `versions/<v>/` |

**`format` 始终由资产文件名的扩展名推导**(最长匹配 —— `.tar.gz`/`.tgz`→`tar.gz`、`.gz`→`gz`、`.zip`→`zip`,否则 `raw`;见 §12),不入 manifest、不参与签名。**sha256 始终对"下载到的原始文件(解包前)"计算。**

### 如何运行(`[command]` 段的 `run` / `exec`,无 `kind`)

"如何运行"全由 operator 在 `lode.toml` 的 `[command]` 段配置;manifest 只声明"下载什么"(`name`/`url`/`sha256` + 建议性 `entry`)。命令是字符串(按空白切)或 argv 数组,占位符 `{entry}`=入口绝对路径、`{dir}`=版本目录。**argv 直接执行,不经 shell;不需要 `kind`。**

- **`run`**:**裸跑 `lode` 时启动 app 的命令**。`{entry}` 缺省时自动追加到末尾。例 `"bun run"` → `bun run <entry>`;自包含二进制 `"{entry} serve"`。
- **`exec`**:**`lode <args>` 透传时的基准命令**,CLI 参数追加其后。例 `"bun"` → `lode run db:init` 即 `bun run db:init`;自包含二进制 `"{entry}"`。
- **`workdir`**:子进程 cwd,`{dir}`(默认)或绝对路径。stdio 继承 lode。
- 安装后对 `entry` 统一 `chmod +x`(对脚本无害,省去 `kind` 判断)。

### 运行时下载(`[runtime]`,可选)

当 `run`/`exec` 依赖某运行时(如 `bun`)时,可在 `lode.toml` 的 `[runtime]` 声明。解析顺序 **PATH → 缓存 → 下载**:先查 PATH;否则复用 `$DATA_DIR/runtime/<name>` 里上次下载的运行时;再否则从 `download` 下载。自包含二进制则省略此表。

- **format / 摊平**:`format` 由 URL 扩展名推导(`raw`/`gz`/`zip`/`tar.gz`);解包后把目标二进制**摊平到 `runtime/<name>`**,所以官方嵌套包(bun 的 `bun-linux-x64/bun`、node 的 `node-vX/bin/node`)和扁平包(deno)、单文件都能用。
- **缓存**:落地的 `runtime/<name>` 在后续启动被复用——`$DATA_DIR` 持久化时下载只发生一次;删掉该文件即可强制重下。
- **不校验**:与应用产物不同,运行时下载**不带 `sha256`/`sig`,不做完整性/身份校验**。请锁版本、用可信源,若需凭据把其 host 加入 `[http].credential_hosts`。
- **版本锁定**(`version` + `version_check`):设了 `version` 后,lode 对即将使用的运行时(PATH/缓存/刚下载)跑 `version_check`(默认 `--version`),要求输出**包含** `version`。PATH/缓存里版本不对会被绕过去重新下载;下载下来还不对则硬报错。
- 最后把 `$DATA_DIR/runtime/` **前置到子进程 PATH**。

```toml
[runtime]
runtime       = "bun"                                    # run/exec 用到的可执行名
download      = "https://example.com/bun-linux-x64.zip"  # PATH/缓存 都没有 bun 时下载
version       = "1.1.38"                                 # 可选:要求此版本(探测输出的子串)
# version_check = "--version"                            # 可选:打印版本的参数(默认 --version)
```

### 资产选择(按文件名)

版本的 `assets[]` 以**文件名**(`name`,如 `myapp-linux-x86_64.tar.gz`)为键。operator 用 `[update].asset` 指定本机要装的资产;lode 按 `name` 在来源的资产列表里匹配。**无平台自动探测、无 arch 别名表** —— 文件名是唯一选择键(native 与 GitHub 两源同一键),其扩展名确定 `format`。文件名按约定带上品牌/平台。见 §6/§12 与 [`docs/source-adapters.zh-CN.md`](source-adapters.zh-CN.md)。

---

## 5. 版本生命周期与更新策略

### 启动决策(不主动跳版本)

```
读 state.json 得 current
┌─ current 不存在(首次/全新数据盘)
│     → 引导:拉远程 manifest → 取 latest → 选 artifact → 下载 + 校验(sha256+sig)+ 落地
│     → current = latest, last_good = latest → 起子进程
└─ current 已存在
      → 直接启用并起 current(不等网络, 快速启动)
      → 之后按策略在后台处理更新
```

current 不存在且既无 `[update].manifest` 也无本地可用版本 → 报错退出。离线/气隙:预放本地 manifest + 版本目录 + 受信公钥即可无网启动。

### 更新策略 `update.policy` = `off` | `check` | `auto`

| 取值 | 中文 | 行为 |
|---|---|---|
| `off` | 关闭 | 不做后台检查;只跑 current/pinned。仍尊重显式 `target`(按需拉一次远程解析条目)。 |
| `check`(默认) | 检测 | 周期拉远程 → 刷新本地 catalog、写 `state.available`,**不自动应用**;app 读到后提示 → 写 `state.target` → lode 执行。 |
| `auto` | 自动 | 周期拉远程 → `latest > current` 则自动设 `target=latest` 并热更。 |

`update.pin` 锁版本(等价 `off` + 固定 target);任何策略下显式 `state.target` 都被尊重并执行(前提校验通过)。检查节奏 `update.check_interval` 秒(0=仅启动查一次;`off` 不计时)。

### 应用一个 target(热更新 + 回滚)

```
target ≠ current 且可用(stop-start 模式):
  1. 确保已安装(缺则下载 + 校验 + 落地到 versions/<target>)
  2. 写 state.status=updating → 给旧子进程 SIGTERM(app 清理后退出)→ STOP_TIMEOUT 超时才 SIGKILL
  3. 原子切 current 软链 → versions/<target>
  4. 起新子进程;等就绪(§8:readiness=none→存活满 grace;readiness=state→等 state.ready)
  5. 就绪 → status=running、last_good=target;**单次失败即回滚**:就绪超时 / 观察期(health_grace)内退出或崩溃一次 → 切回 last_good 并同样观察之;若 last_good 在其 grace 内也失败 → lode 退出(不再循环重试)

零停机(reuseport-overlap / socket-activation):先起新进程,**等其就绪后再停旧**(§8),避免空窗与提前杀旧。
```

### 启动清理(GC + 孤儿子进程)

`lode` 启动(服务模式)在确定版本之前先做一遍清理:

- **孤儿子进程**:上一个 lode 崩溃可能遗留仍在跑的 app 子进程。接管僵尸锁后,读 `state.json.pid`,若该进程还活着 → 优雅终止(SIGTERM→超时 SIGKILL)再起新的,避免端口/资源冲突与双实例。
- **垃圾回收**:清理中断遗留的 `downloads/*.part`、`versions/<v>.tmp` 半成品;按 `keep_versions` 保留 current + last_good + 最近 N 个版本,其余版本目录删除;`$DATA_DIR/runtime/` 同理只留在用的。
- **校验落盘一致性**:`current` 软链指向的版本若缺失/损坏 → 回退到 last_good 或重新引导。

---

## 6. 验证与信任:文件完整性 + 发布者身份

- **完整性**:对下载到的原始文件求 sha256,必须等于 artifact 的 `sha256`(小写 hex)。
- **身份**:ed25519。发布者私钥签"发布记录",lode 用预置**受信公钥**验签 → 即使 CDN/镜像被投毒或传输被 MITM,也能确认是该发布者所发且未篡改。

### 签名规范字节(精确,UTF-8,`\n` 分隔,无尾换行)

asset 级 `sig` 的签名消息:
```
lode.artifact.v1
{name}
{version}
{sha256}
```
`{name}` 是**资产文件名**(选择键),`{version}` 是发布版本,`{sha256}` 是原始下载文件的小写 hex 摘要。签名绑定*哪个资产*、*哪个版本*、*哪些字节*;`format`/`entry`/`url` 由文件名推导或属 operator 本地,**不**参与签名,故被篡改的 catalog 无法把真签名挪到别的字节、别的资产或别的版本上。用 `key_id`(asset.key_id ?? manifest.key_id)对应的受信公钥验签。

> **签的是 sha256(摘要),不是直接对 bin 流签名。** ed25519 对上面这串规范文本(其中含 `{sha256}`)签名——等价于"签摘要 + 身份字段"。完整 verify 链:**下载字节 → 重算 sha256 →(完整性)== artifact.sha256 →(身份)验这串签名**。sha256 已把内容绑定,故无需把整个二进制流过签名器;这也是发布签名(如签 `SHA256SUMS`)的常规做法。

可选 manifest 顶层 `sig`,由 `lode-cli manifest-sign` 写入,并在解析/下载任何版本**之前**验证,防增删版本、改 channel `latest`、改 URL:
```
lode.manifest.v1
{name}
{key_id}
{canonical}     # 对 channels + versions 的确定性、去 sig 序列化
```
`{canonical}` 由解析后的 manifest(已排序的 `channels`/`versions`)生成,故签名方与验证方产出完全相同的字节,与 JSON 键序/空白无关;顶层 `sig` 本身不计入被签字节。验证时按 manifest 的 `key_id` 选键(选不中则回退逐一尝试所有受信公钥)。

### 受信公钥 / 强度

- `LODE_TRUSTED_KEYS` = 逗号分隔 `key_id:base64(32字节原始 ed25519 公钥)`;或 `LODE_TRUSTED_KEYS_FILE`(每行 `key_id base64`)。支持多把(轮换)。`key_id` = `sha256(公钥32字节)` 的前 16 位十六进制。
- `LODE_REQUIRE_SIGNATURE` = `off`(仅 sha256) | `auto`(默认) | `enforce`(生产推荐)。
  - **`auto` 一旦配置了任一受信公钥即变为 fail-closed**:此时 manifest 签名与 artifact 签名都成为必需 —— 缺签名*或*验签失败都拒绝。仅当**未**配置任何受信公钥时,`auto` 才跳过验签,并把来源记为 **UNVERIFIED**。
  - **`enforce`** 始终要求受信公钥,且 manifest 签名与 artifact 签名都必须有效。

### CLI(发布者)

`lode-cli keygen` / `lode-cli sign <asset> --version <ver> --key <priv>`(或 `--key-env <VAR>`;打印 `sha256`/`sig`/`key_id`,其中 `sig` 同时用作 GitHub 资产 `label`)/ `lode-cli verify <asset> --version <ver> --pubkey <b64> --sig <b64>` / `lode-cli manifest <asset> --version <ver> --url <url> [--entry <e>] --key <priv> --into manifest.json` / `lode-cli manifest-sign --into manifest.json --key <priv>`(为目录写入顶层 `key_id` + `sig`)。私钥离线,lode 只持公钥。

---

## 7. 文件模型与 lode ↔ app 通信

三个概念,职责分明:

| 文件 | 位置 | 格式 | 谁写 | 作用 |
|---|---|---|---|---|
| **`lode.toml`** | 本地 | TOML | **仅 lode 侧**(operator 编写,app 不写) | **app 的配置**:如何启动和配置程序(manifest 地址/通道/exec/workdir/headers/策略/信任/`pin`) |
| **`state.json`** | 本地 | JSON | **lode 与 app 都可写** | 运行时通信中枢:lode 写实际状态,app 写升级/重启请求 |
| **`manifest.json`** | **远程** | JSON | 发布方 | 远程程序的版本目录;lode 按策略拉取,**不落本地**(每版本元数据存于版本目录供离线运行) |

通信全走 **`state.json`**(双向,各自单写字段 + 原子写 temp+rename;lode 轮询其 mtime):

- **lode 写**:`current`/`last_good`/`available`/`status`/`pid`/`last_check`/`last_error`/`history`/`channel`。
- **app 写**(请求):`target`(想升/降到的版本,或 `"latest"`)、`restart_nonce`(递增=请求重启)、`ready`(就绪握手:写成本次启动的 `LODE_INSTANCE` 值表示"我能服务了",见 §8)。
- 典型流程(`policy=check`):lode 把发现的新版写进 `available` → **app 读到后,自己决定升级就把 `target` 写成该版本(或 `"latest"`)** → lode 应用并热更。

**通知机制(双向,都不用额外信号):**

- **app → lode**(请求重启/升级):app **原子写 `state.json`**(改 `target` 或递增 `restart_nonce`);**lode 短间隔(~1s)轮询 `state.json` 的 mtime**,变化即重读并执行。**文件本身就是通知**,无需给 lode 发信号(也避免与转发给子进程的信号冲突)。
- **lode → app**(要求重启,让 app 清理):lode 先把 `state.status` 置为 `updating`(热更)或 `stopping`(关停)让 app 能区分,再给子进程发 **`SIGTERM`**;app 在 SIGTERM 处理器里做清理(排空/flush/释放)后 `exit(0)`,须在 `supervise.stop_timeout` 内,否则 `SIGKILL`。退出后 lode 切版本起新进程(§5)。

> `lode.toml` 是纯配置,**app 不写它**(只有 lode 侧/operator 写);要锁版本用其中的 `pin`。完整 `lode.toml` 见 `docs/lode.example.toml`。

### `$DATA_DIR/state.json`(lode+app 共写)

```json
{
  "current": "1.4.2",
  "last_good": "1.4.2",
  "available": "1.5.0",
  "channel": "stable",
  "status": "running",
  "pid": 12345,
  "last_check": "2026-06-04T22:00:00Z",
  "last_error": null,
  "history": [ { "version": "1.4.2", "at": "2026-06-04T21:00:00Z", "result": "good" } ],

  "target": null,
  "restart_nonce": 0
}
```
lode 拥有的字段 vs app 拥有的字段(`target`/`restart_nonce`)互不重叠;`status` ∈ `starting|running|updating|rolling-back|stopping|stopped|error`;`result` ∈ `good|bad`。

---

## 8. 子进程监督、服务管理与无停机重启(借鉴 overseer)

- 全生命周期由 lode 管理:启动 / 优雅停 / 重启 / 状态 / 热更 / 回滚,经 CLI(§13)与 manifest(§7)对外。
- **容器 init 职责(常作 PID 1)**:lode 安装信号处理器(PID 1 无默认处理器),并**回收僵尸**——设为 child subreaper 并循环 `waitpid` 收割被重定父的孙进程,防僵尸堆积。`docker stop` 给 PID 1 发 `SIGTERM`(默认 10s 后 `SIGKILL`):lode 须在该 grace 内转发并退出(`stop_timeout` 应 < Docker 的 grace)。
- **信号透传(service 模式)**:lode 作为 master,把**外部收到的信号尽量透传给子进程**,不替 app 决定行为:
  - 终止类 `SIGTERM`/`SIGINT`/`SIGQUIT`:lode 发起**优雅关停**——转发给子进程 → 等其退出(超时 SIGKILL)→ 释放锁 → lode 以子进程退出码退出。
  - 透传类 `SIGHUP`/`SIGUSR1`/`SIGUSR2`/`SIGWINCH`/`SIGCONT`/`SIGTSTP`/…:**原样转发给子进程**(如 app 用 `SIGHUP` reload),lode 不消费。
  - 可选 `signals.restart`(env `LODE_RESTART_SIGNAL`,如 `SIGUSR2`):设置后该信号改为**触发 lode 优雅重启**(等价 `restart_nonce++`)且不再透传;**默认不设**,以免占用 app 信号(重启走 state.json/CLI)。
  - `SIGKILL`/`SIGSTOP` 不可捕获(OS 限制);`SIGCHLD` 由 lode 用于感知子进程退出。透传集合可用 `signals.forward`(env `LODE_FORWARD_SIGNALS`)调整。
- **重启策略 `supervise.restart`**(`off`|`on-failure`|`always`,默认 `off`):
  - `off`(默认)= lode **镜像子进程生命周期**——子进程自行退出(非 lode 发起的停止)时,lode 以其退出码退出(`exit(code)`→`code`;`signal(sig)`→`128+sig`),整进程重启交编排;**不**自动拉起。
  - `on-failure` = 仅**失败**(非零退出或被信号杀)时重启;干净 `exit(0)` 则 lode 跟随退出。
  - `always` = 任意退出都重启。
  - `on-failure`/`always` 走**指数退避**(基 `RESTART_BACKOFF`,封顶 `..._MAX`;`RESTART_MAX`>0 达连续上限后 lode 退出、status=error;子进程存活满 `health_grace` 重置连续计数)——这三个键**仅在 `restart != off` 时生效**。
  - **无论何种策略**,lode 发起的转换都照常重启子进程:更新(应用 `state.target` / `policy=auto` 的更新)、回滚、显式重启(`restart_nonce` 或重启信号)。子进程退出时若有**待应用的更新**(`state.target` 指向不同的可安装版本,或 `policy=auto` 下 latest 更新),优先按更新拉起新版本而非原版重启。
- **健康/回滚**:见 §5(回滚为**单次触发**)。

### 运行模式 —— 裸跑=启动,带参=透传(无需标志/子命令)

极简规则:

- **`lode`(裸跑)= 启动并监督服务**:加锁(单实例)→ 确定/安装版本(+ 必要时下载运行时)→ 跑 **`run`**(lode.toml,自动补 `{entry}`)→ 监督(按 `supervise.restart` 策略,默认镜像子进程)→ 按策略轮询热更/回滚 → 信号透传如上。适合 server/daemon。镜像 `ENTRYPOINT ["/usr/bin/lode"]`,`docker run img` 即启动。
- **`lode <args...>`(带任意参数)= CLI 透传**:**不加锁、不监督、不轮询**。校验目标版本(无则引导)→ **exec 替换**为 **`exec` + `<args>`**,透传 stdin/stdout/stderr(含 TTY),信号与退出码由 OS 原生处理。
  - `lode run db:init` → `exec + ["run","db:init"]`(若 `exec="bun"` 则 ≡ `bun run db:init`)。**无需 `--`**;**不用 `run` 子命令**故不与 `bun run` 冲突。
  - 位置参数经 clap `trailing_var_arg` + `allow_hyphen_values` 透传(`lode` 无子命令,所有参数都归 app)。
  - exec 替换用安全的 `std::os::unix::process::CommandExt::exec`,保持 `#![forbid(unsafe_code)]`。
- **`lode` 本身无任何子命令**;运维与发布操作(status/update/…/keygen/sign/manifest/init)都在软链接 **`lode-cli`** 下(见 §13)。因此 `run`/`migrate` 等永远透传给 app,绝不被遮蔽。

### 重启模式 `supervise.restart_mode`

| 取值 | 行为 | app 配合 |
|---|---|---|
| `stop-start`(默认) | 先停旧再起新,有极短端口空窗 | 无;非网络服务也适用 |
| `socket-activation` | lode 持监听 socket,**用 systemd 套接字激活协议**(`LISTEN_FDS`/`LISTEN_PID`/`LISTEN_FDNAMES`,fd 从 3 起)传给子进程;热更保持 socket、起新进程复用 fd、旧的排空退出 → **真·零停机** | app 支持 socket activation(Go/Rust/Node/nginx…) |
| `reuseport-overlap` | 先起新进程与旧并存,新健康后再停旧 → 零停机 | app 开 `SO_REUSEPORT` |

> **容器里没有 systemd?不影响。** socket-activation 是一套**协议(环境变量 + 继承 fd)**,不依赖 systemd 进程——**lode 自己充当 activator**(绑 socket、传 fd、设 `LISTEN_FDS`),app 只要会读 fd 3 即可。容器内首选更简单的 `reuseport-overlap`,或默认 `stop-start`。
> `socket-activation` 需 `LODE_LISTEN`(如 `0.0.0.0:3000`);fd 传递经 `command-fds` 封装,保持 `#![forbid(unsafe_code)]`。
> 与 overseer 差异:overseer 是**进程内库**且**不自动重启崩溃**(同码退出);lode 是**外部通用监督器**,默认同样镜像子进程(`restart=off`),并提供可选的崩溃重启(`on-failure`/`always`)+ 回滚,是其超集。**零停机为可选高级特性,v1 默认 `stop-start`,socket-activation/overlap 后续按需启用。**

### 就绪 / 停止握手(关键:别在 app 没准备好时就杀进程)

为避免"新进程还没起来就切流量、或旧进程还没清理完就被杀",约定两个握手:

**① 就绪握手(启动方向)`supervise.readiness`**

lode 每次拉起子进程都注入唯一实例号 **`LODE_INSTANCE`**(env)。

| `readiness` | lode 何时认为"已就绪/成功" |
|---|---|
| `none`(默认) | **存活满 `health_grace` 秒**即算就绪/good(适合无就绪信号的程序)。 |
| `state` | **等 app 写 `state.ready == 本次 `LODE_INSTANCE``**(app 自报"我能服务了");在 `supervise.ready_timeout`(默认 30s)内没等到 → 判失败(回滚/重启)。 |

就绪之前,lode **不**做这些事:不置 `status=running`、不标 last_good、**(`reuseport-overlap`/`socket-activation`)不停止旧进程**。→ 旧实例一直顶着,直到新实例自报就绪,真正零停机。

**② 停止握手(关闭方向)`supervise.stop_timeout`**

lode 发 `SIGTERM` 后,**在 `stop_timeout` 秒内绝不 SIGKILL**,给 app 充分清理时间。约定:
- operator 把 `stop_timeout` 设为 ≥ app 最坏清理耗时,且 **< 容器/编排的停止宽限**(如 Docker 默认 10s,否则容器层会先 SIGKILL)。
- app 收到 `SIGTERM` 必须尽快收尾并 `exit(0)`;只有超时才被 SIGKILL。

> 默认 `readiness=none` 时,"就绪"退化为"存活满 `health_grace`"——这对零停机不够严谨,故 `reuseport-overlap`/`socket-activation` 建议配 `readiness=state`。

---

## 9. PID 保护

- `$DATA_DIR/lode.pid`,以 O_EXCL(`create_new`)原子创建,含 lode pid + 应用名。
- 已存在 → 探活(`nix::sys::signal::kill(pid, None)` / `kill -0`):存活→当前进程退出(单实例);`ESRCH`→ 删僵尸锁后接管。
- 正常退出/收信号时删锁。

---

## 10. 配置体系

**优先级:`CLI > 环境变量 > lode.toml 配置文件(`LODE_CONFIG`,TOML) > 默认值`。**

键名:`lode.toml` 用 snake_case(见 `docs/lode.example.toml`);环境变量用 `LODE_*`。

| 环境变量 | CLI | lode.toml 键 | 默认 | 含义 |
|---|---|---|---|---|
| `LODE_CONFIG` | `--config <path>` | — | `lode.toml` | lode.toml 配置文件路径(TOML) |
| **`[global]`** | | | | |
| `LODE_APP_NAME` | `--app <name>` | `global.app` | `app` | 应用名(命名数据目录/锁;须与 manifest `name` 一致) |
| `LODE_DATA_DIR` | `--data-dir <path>` | `global.data_dir` | `/srv/lode` | 运行/基目录:`lode.toml` + 版本/状态/锁 + `runtime/`;默认也在此查找 `lode.toml`,缺失则自动生成一份起始配置 |
| `LODE_LOG_LEVEL` | `--log-level <lvl>` | `global.log_level` | `info` | trace/debug/info/warn/error |
| **`[update]` —— 源 + 升级策略** | | | | |
| `LODE_MANIFEST` | `--manifest <url>` | `update.manifest` | — | **native 源**:lode/v1 manifest URL(与 `github` 二选一) |
| `LODE_GITHUB` | `--github <owner/name>` | `update.github` | — | **github 源**:仓库(与 `manifest` 二选一) |
| `LODE_GITHUB_API` | `--github-api <url>` | `update.github_api` | `https://api.github.com` | (github)API 基址(GHE) |
| `LODE_ASSET` | `--asset <file>` | `update.asset` | — | 本机要装的资产**文件名**(选择键,§4/§12) |
| `LODE_ENTRY` | `--entry <path>` | `update.entry` | — | 覆盖包内 entry 路径(建议性,通常省略,§4) |
| `LODE_CHANNEL` | `--channel <name>` | `update.channel` | `stable` | 跟随的通道(manifest 可定义多个) |
| `LODE_UPDATE_POLICY` | `--policy <off\|check\|auto>` | `update.policy` | `check` | 更新策略,§5 |
| `LODE_CHECK_INTERVAL` | `--interval <sec>` | `update.check_interval` | `300` | 检查间隔秒;0=仅启动一次 |
| `LODE_KEEP_VERSIONS` | `--keep <n>` | `update.keep_versions` | `3` | 保留旧版本数 |
| `LODE_PIN_VERSION` | `--pin <ver>` | `update.pin` | — | 锁定版本(operator) |
| **`[http]` —— 拉取凭据** | | | | |
| `LODE_HEADERS` | `--header <h>`(可重复) | `http.headers` | — | 透传给 manifest/artifact/runtime 下载的 HTTP 头(`"Name: Value"`),支持 `${ENV}` 展开,§11 |
| **`[trust]` —— 验签** | | | | |
| `LODE_REQUIRE_SIGNATURE` | `--require-signature <off\|auto\|enforce>` | `trust.require_signature` | `auto` | 验签强度,§6 |
| `LODE_TRUSTED_KEYS` | `--trusted-keys <list>` | `trust.trusted_keys` | — | 受信公钥 `key_id:base64`,逗号分隔 |
| `LODE_TRUSTED_KEYS_FILE` | `--trusted-keys-file <path>` | `trust.trusted_keys_file` | — | 受信公钥文件 |
| **`[command]` —— 怎么运行** | | | | |
| `LODE_RUN` | `--run <cmd>` | `command.run` | `{entry}` | **裸跑启动命令**(`{entry}` 缺省自动追加),见 §4 |
| `LODE_EXEC` | `--exec <cmd>` | `command.exec` | `{entry}` | **CLI 透传基准命令**(`lode <args>` 追加其后),见 §4 |
| `LODE_WORKDIR` | `--workdir <path>` | `command.workdir` | `{dir}` | 子进程 cwd(版本目录或绝对路径),见 §4 |
| **`[env]` —— 额外子进程环境变量**(仅配置文件;无 CLI/env 覆盖) | | | | |
| — | — | `[env]`(表) | — | 注入子进程的额外环境变量,作为**默认值**——同名的宿主 env 会覆盖它(lode 自身的 `LODE_*` 高于一切),见 §4 |
| **`[runtime]` —— 可选运行时** | | | | |
| `LODE_RUNTIME` | `--runtime <name>` | `runtime.runtime` | — | 运行时可执行名(run/exec 用),见 §4 |
| `LODE_RUNTIME_DOWNLOAD` | `--runtime-download <url>` | `runtime.download` | — | 运行时缺失时的下载地址(下载后缓存复用;不做签名校验) |
| `LODE_RUNTIME_VERSION` | `--runtime-version <ver>` | `runtime.version` | — | 要求的运行时版本;探测后按子串匹配,见 §4 |
| `LODE_RUNTIME_VERSION_CHECK` | `--runtime-version-check <args>` | `runtime.version_check` | `--version` | 打印运行时版本的参数(仅在设了 `version` 时用) |
| **`[supervise]` —— 看护(重启策略/健康/回滚/停止/重启模式)** | | | | |
| `LODE_RESTART` | `--restart <off\|on-failure\|always>` | `supervise.restart` | `off` | 重启策略:`off`=镜像子进程(lode 随其退出);`on-failure`=仅崩溃重启;`always`=任意退出都重启,§8 |
| `LODE_RESTART_BACKOFF` | `--restart-backoff <ms>` | `supervise.restart_backoff` | `500` | 重启退避基数(指数);仅 `restart != off` 时生效 |
| `LODE_RESTART_BACKOFF_MAX` | `--restart-backoff-max <ms>` | `supervise.restart_backoff_max` | `30000` | 退避上限;仅 `restart != off` 时生效 |
| `LODE_RESTART_MAX` | `--restart-max <n>` | `supervise.restart_max` | `0` | 连续重启上限,0=无限;仅 `restart != off` 时生效 |
| `LODE_READINESS` | `--readiness <none\|state>` | `supervise.readiness` | `none` | 就绪判定:`none`=存活满 grace;`state`=等 app 写 `state.ready`,§8 |
| `LODE_READY_TIMEOUT` | `--ready-timeout <sec>` | `supervise.ready_timeout` | `30` | `readiness=state` 时等待就绪上限,超时判失败 |
| `LODE_HEALTH_GRACE` | `--health-grace <sec>` | `supervise.health_grace` | `15` | (readiness=none)新版需存活满此秒才算 good;亦为单次回滚的观察窗口 |
| `LODE_STOP_TIMEOUT` | `--stop-timeout <sec>` | `supervise.stop_timeout` | `10` | 优雅停超时后 SIGKILL |
| `LODE_RESTART_MODE` | `--restart-mode <mode>` | `supervise.restart_mode` | `stop-start` | 重启策略,§8 |
| `LODE_LISTEN` | `--listen <addr>` | `supervise.listen` | — | socket-activation 监听地址 |
| **`[signals]` —— 信号** | | | | |
| `LODE_FORWARD_SIGNALS` | `--forward-signals <list>` | `signals.forward` | (标准集) | 透传给子进程的信号集,§8 |
| `LODE_RESTART_SIGNAL` | `--restart-signal <sig>` | `signals.restart` | — | 触发优雅重启的信号(默认不设),§8 |

子进程环境:透传宿主环境并**剥离配置类 `LODE_*`**;把 operator 的 `[env]` 表作为**默认值**应用(仅对宿主 env 没有的 key);把运行时目录前置到 PATH;再注入只读自省变量 `LODE_ACTIVE_VERSION`、`LODE_DATA_DIR`、`LODE_INSTANCE`(本次启动唯一号,用于就绪握手,§8)。优先级 低→高:`[env]` 默认值 < 继承的宿主 env < 运行时 PATH 前置 < 注入的 `LODE_*`。

---

## 11. 私有包 / 鉴权(header 列表透传)

下载 manifest 与 artifact 时,把 `headers` 列表里的 HTTP 头**原样透传**到请求——一套机制覆盖任意鉴权(Bearer、`X-Api-Key`、自定义头都行),无需为每种方案单独建模:

```toml
# lode.toml
headers = [
  "Authorization: Bearer ${RELEASE_TOKEN}",   # ${ENV} 从 lode 环境展开，密钥不落文件
  "X-Api-Key: ${API_KEY}",
]
```

- `${VAR}` 在加载时从 lode 进程环境展开(推荐用容器 secret/env 注入,头里不写明文)。
- artifact 设 `"auth": false` → 该 URL **不加** headers(如已自带签名的预签名链接)。
- CLI:`--header "Name: Value"`(可重复);env:`LODE_HEADERS`(换行分隔)。
- 鉴权(传输层"能下")与签名(信任层"确实是他发的",§6)正交,私有源建议都用。
- headers(含展开后的值)/私钥**永不写日志/`state.json`**;日志对 URL 查询串脱敏。

---

## 12. Manifest 格式规范(lode/v1) —— 完整约定

远程 manifest 由发布方提供,**格式为 JSON**(UTF-8),由 lode 从 `[update].manifest` 拉取,**不存本地**。**完整最大化示例见 [`docs/manifest.example.json`](./manifest.example.json)**。结构约定:

- 顶层:`schema`(必填,`"lode/v1"`)、`name`(必填,须与 `lode.toml` 的 `app` 一致)、`key_id`(可选,默认签名公钥 id)、`sig`(可选,catalog 的 ed25519 签名,§6)。
- `channels`(必填):对象,键为通道名,值含 `latest`(版本 id)。**可多个通道**,lode 按 `channel` 跟随其一。
- `versions`(必填):对象,键为版本 id(被通道 `latest` 引用),值含 `notes`(可选)+ `assets` 数组(≥1)。
- 每个资产以**文件名**(`name`)为键;operator 用 `[update].asset` 选其一:

| 字段 | 必填 | 说明 |
|---|---|---|
| `name` | ✓ | 资产**文件名**(如 `myapp-linux-x86_64.tar.gz`)—— 选择键与被签身份;其扩展名确定 `format`(§4) |
| `url` | ✓ | 绝对下载地址 |
| `sha256` | ✓ | 下载文件(解包前)的小写 hex 摘要 |
| `sig` | 条件 | base64 ed25519,对 §6 消息 `(name, version, sha256)` 签名;`require_signature=enforce` 时必填(`auto` 一旦配了受信公钥也必填) |
| `key_id` | | 覆盖顶层 `key_id` |
| `entry` | | 建议性包内 entry 路径(§4);解析顺序 manifest `entry` > `[update].entry` > 约定 |
| `size` | | 期望字节数(多一道防护) |
| `auth` | | 默认 `true`;`false`=该 URL 不加透传 headers(预签名) |

> **无 `platform`、无 `format`、无 `kind`**:资产按文件名选,format 由扩展名推导,运行命令(`run`/`exec`/`workdir` + 可选 `[runtime]`)全在 `lode.toml`(§4/§7/§10)。manifest 只声明"下载什么"(`name`/`url`/`sha256` + 建议性 `entry`),由 operator 决定"怎么跑"。安装后统一 `chmod +x` entry。

**最小示例(单文件 JS 脚本,公开源、未签名)**:
```json
{
  "schema": "lode/v1",
  "name": "hello",
  "channels": { "stable": { "latest": "1.0.0" } },
  "versions": {
    "1.0.0": { "assets": [
      { "name": "hello.js",
        "url": "https://releases.example.com/hello-1.0.0.js",
        "sha256": "<hex>" }
    ] }
  }
}
```
（operator 设 `[update].asset = "hello.js"`。运行命令在 `lode.toml` 配:脚本用 `run = "bun run"`、`exec = "bun"`;并按需加 `[runtime]` 下载 bun。）

### 打包(发布流程,语言无关)

```bash
# 二进制（Go/Rust/bun --compile）：给资产命名,让扩展名确定 format
tar -czf myapp-linux-x86_64.tar.gz -C build myapp
lode-cli sign myapp-linux-x86_64.tar.gz --version 1.5.0 --key publisher.key
#  → 打印 sha256 + sig + key_id(sig 同时用作 GitHub 资产 label)
lode-cli manifest myapp-linux-x86_64.tar.gz --version 1.5.0 \
    --url https://releases.example.com/1.5.0/myapp-linux-x86_64.tar.gz \
    --entry myapp --key publisher.key --into manifest.json   # 按 name upsert 资产
lode-cli manifest-sign --into manifest.json --key publisher.key   # §6 catalog 签名

# 单文件 JS（bun build --outfile）：无打包扩展名 → raw
bun build ./src/index.ts --target bun --outfile hello.js
lode-cli sign hello.js --version 1.0.0 --key publisher.key
```

打包与签名是**发布方自理**(在各自 CI 用任意工具完成),lode 不提供打包脚本,只约定规范;流程见 [`docs/source-adapters.zh-CN.md`](source-adapters.zh-CN.md) 与 [`docs/integration.zh-CN.md`](integration.zh-CN.md)(build → sha256 → 对 `(name, version, sha256)` 签名 → 组装 manifest)。`lode-cli keygen`/`sign`/`manifest`/`manifest-sign` 为参考实现。

### Manifest 来源:native / GitHub —— 由"设了哪个 key"决定(二选一,无独立 `source`)

原生 manifest 是权威格式(显式、可签名、可放任意静态托管:S3 / OSS / GitHub raw / gh-release 资产)。为"简单通用",再支持把 **GitHub Releases** 直接当来源,适配成同一套内部模型,后续下载/校验/安装流程不变。**来源由 `[update]` 里设了哪个 key 决定**(两者互斥,都设则报错):

| 设置的 key | 来源 | lode 行为 |
|---|---|---|
| `[update].manifest = "<url>"` | native | 拉取上面规范的 `lode/v1` JSON。 |
| `[update].github = "owner/name"`(GHE 另配 `github_api`) | github | **直接用 GitHub 原生端点**,无需自己算"最新";把一个 release 映射成一个版本。 |

**channel ↔ GitHub 端点**(用 GitHub 自带的 `latest`/`tags`,不重造轮子):

| 场景 | GitHub 端点 | 说明 |
|---|---|---|
| `channel=stable` | `GET /repos/{repo}/releases/latest` | GitHub 的 `latest` = 最新的非 prerelease/非 draft release |
| `channel=beta`/其它 | `GET /repos/{repo}/releases` → 取最新 `prerelease==true` | 预发通道 |
| `pin=<tag>` | `GET /repos/{repo}/releases/tags/{tag}` | 锁定具体 tag |

**release 自身的资产即 catalog** —— **没有 `manifest.json` 资产**。适配器:
1. 按上表选出 release(latest/prerelease/tag);
2. 把每个 release 资产映射为内部资产:`name`=资产文件名、`sha256`=资产 `digest`(GitHub 计算,再对下载字节复核)、`sig`=资产 **`label`**(API 返回的唯一自由字符串槽)、`url`=`browser_download_url`;
3. 之后与 native 完全相同(选 `name` 与 `[update].asset` 匹配的资产 → 校验 sha256+ed25519 → 安装)。

- **版本号**:用 release 的 `tag_name`(数字前的 `v` 前缀去掉)—— 以 GitHub 为准。
- **GitHub 无 catalog(顶层)签名**:新鲜度由 tag 权威保证;每资产 `sig`(label)仍保护各自下载。
- **私有 repo**:`[http].headers` 放 GitHub token(`Authorization: Bearer <PAT>`),API 与同主机资产下载都带。

> 最简用法只需 `github = "owner/repo"` + `asset = "<filename>"`,`stable` 直接走 `/releases/latest`。两源都产出相同的内部资产列表 → 同一校验/安装路径。CI 签名可选 —— 见 [`docs/source-adapters.zh-CN.md`](source-adapters.zh-CN.md) §5 的 release workflow 配方。

---

## 13. CLI —— multi-call 二进制(`lode` / `lode-cli`)

lode 是 **multi-call 二进制**,按 `argv[0]` 分流:`lode-cli` 是同一二进制的**软链接**,随二进制一同发布;镜像内 `/usr/bin/lode` 与 `/usr/bin/lode-cli` 均在 PATH 上。

```
# 以 lode 调用 = 纯加载器,无任何子命令
lode                       # 裸跑 = 启动并监督服务(跑 lode.toml 的 exec)
lode <app 参数...>          # 带参 = CLI 透传(跑 exec + 参数);lode run db:init ≡ bun run db:init

# 以 lode-cli 调用(软链接)= 运维 + 发布工具箱
lode-cli <子命令> [args]

加载器 lode（无子命令,故每个参数都明确属于 app,绝不抢 app 的 run 等词）:
  (裸跑)        启动并监督服务:加锁 → 确定版本 → 跑 exec → 按策略轮询热更/回滚
  <app 参数>    CLI 透传:校验版本 → exec 替换为 `exec` + 参数;不加锁/不监督

lode-cli 管理（写 state.json,与运行中的服务实例沟通）:
  status       打印 state.json + 远程 manifest 摘要后退出
  update       安装最新（或 --version <v>）；有服务在跑则写 state.json 的 target 热更，否则仅安装
  rollback     把 state.json 的 target 设为 last_good（或 --version <v>）
  restart      递增 state.json 的 restart_nonce，让服务重启子进程
  versions     列出本地已安装版本

lode-cli 发布/签名(发布者,详见 docs/integration.zh-CN.md):
  keygen       生成 ed25519 私钥/公钥/key_id
  sign         对 artifact 算 sha256 + 产出 sig
  verify       本地校验 artifact 的 sha256 + sig
  manifest     签名并生成 / 合并(--into)lode/v1 manifest
  init         写出起始 lode.toml(示例配置)
```

全局参数即 §10 的 `--xxx`(clap 解析,带 `env` 回退),覆盖 env 与 lode.toml。

---

## 14. Rust 模块布局(多文件)

```
Cargo.toml          # package + [workspace] + [workspace.lints] + [profile.*] + 依赖
Cargo.lock          # 提交（二进制项目）
rust-toolchain.toml # channel = "1.96.0", 组件, musl targets
.cargo/config.toml  # +crt-static (musl), git-fetch-with-cli
deny.toml clippy.toml rustfmt.toml .config/nextest.toml
src/
  main.rs           # #![forbid(unsafe_code)]；装 aws-lc-rs provider；panic hook；rlimit 抑 core；subreaper(僵尸回收)；CLI 分发
  cli.rs            # clap 定义 + 子命令
  config.rs         # Config + lode.toml 解析 + 合并(CLI>env>lode.toml>默认) + 校验
  error.rs          # thiserror 错误类型
  logging.rs        # tracing 初始化
  idval.rs          # 不可信 id(版本 / 资产 entry / 运行时名)的路径分量校验
  manifest.rs       # serde 类型(JSON) + 远程拉取 + semver 比较(不落本地)
  http.rs           # ureq(rustls/aws-lc-rs) + headers 透传 + 脱敏
  verify.rs         # sha256 + ed25519 verify/sign/keygen
  download.rs       # 流式下载到 temp + sha256 + 按 format 解包
  install.rs        # versions 目录 + 原子软链切换 + prune;启动 GC(清 *.part/*.tmp、按 keep_versions 回收)
  lock.rs           # PID 锁(O_EXCL) + 僵尸锁接管
  state.rs          # state.json 原子读写
  supervisor.rs     # spawn / 退避重启 / 信号转发 / 优雅停 / 健康观察 / 回滚 / 重启模式 / 启动时清理孤儿子进程
  commands/         # run.rs status.rs update.rs rollback.rs restart.rs versions.rs keygen.rs sign.rs verify_cmd.rs
```

---

## 15. 数据目录布局

```
$DATA_DIR/
  lode.toml                # 本地配置(operator 写,app 不写;也可放别处用 --config 指)
  lode.pid                 # PID 锁
  state.json                 # 实际状态(lode 自动生成,app 只读)
  downloads/<ver>.part       # 下载暂存
  versions/<ver>/            # 各版本(raw/gz 落单文件 / tar.gz/zip 解包 / binary chmod)
    .lode.json             #   该版本元数据(entry/format 等)供离线运行
  current -> versions/<ver>  # 原子切换的当前软链
# 注:manifest.json 在远程,不存本地。
```

---

## 16. 安全与边界

- 双层校验:sha256 + ed25519;`enforce` 拒绝一切未签名/验签失败。
- 失败隔离:下载/校验失败弃 `*.part`;新版崩溃自动回滚——均不影响在跑版本。
- 原子性:状态/清单 temp+rename;版本切换软链 rename,无中间态。
- 凭据:token/私钥不落日志/状态;URL 查询串脱敏;私钥只在发布侧。
- 进程:子进程用 argv 数组,不经 shell;`#![forbid(unsafe_code)]`;`rlimit` 抑制 core dump;panic hook 打结构化日志。
- 离线可用:无网回退本地已装版本 + 本地 manifest + 本地公钥。

---

## 17. 交付物

- `src/` 等 —— Rust 模块化源码,编译产出**单一静态二进制 `lode`**。
- `Dockerfile` —— 通用镜像(`FROM zzci/ubase` + `COPY lode /usr/bin/lode`);用预构建的发布二进制构建。
- `tests/` —— bun + TypeScript 端到端测试套件(`tests/src`),示例 app(`tests/apps/web-rust`、`tests/apps/web-bun`)与 docker-compose 集成(`tests/compose`)。
- `docs/integration.zh-CN.md` —— 端到端集成指南(配置 `lode.toml` → 应用契约 → 发布 manifest)。
- `README.md` / `README.zh-CN.md` —— 与本文对齐的总览(英文 / 中文)。

---

## 18. 子进程升级集成

app 作者接入(优雅退出契约、感知更新、触发升级、socket-activation 可选)见 **`docs/integration.zh-CN.md`**。
