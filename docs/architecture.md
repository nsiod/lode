# lode architecture

**English** · [中文](architecture.zh-CN.md)

> A general-purpose "update + launch" component: a **small static Rust binary** (a few MB), language-agnostic.
> It verifies **file integrity (sha256)** and **publisher identity (ed25519 signature)**, then launches and manages the service, supporting seamless hot-update and rollback.
> Universal image = `zzci/ubase` + the lode binary (no language runtime required; the runtime, if any, is downloaded and cached at boot); one instance = **single program, single channel**.

This document is the authoritative **architecture spec**; the implementation follows it. It adheres to the **pma-rust** hard locks (edition 2024, `#![forbid(unsafe_code)]`, rustls+aws-lc-rs, deny-warnings, musl+crt-static, cargo-deny/shear/typos/nextest).

---

## 1. Goals and positioning

- **loader (lode)**: a fixed, unchanging "update + launch" component, compiled into a small static binary, baked into the universal image once and never rebuilt. Responsibilities: configuration → PID lock → decide which version to run → download/**verify (integrity + identity)**/install when necessary → atomic activation → launch and **manage the service** as a **child process** → hot-update/rollback per policy → communicate with the app via **`state.json`**.
- **app**: a program in any language, packaged and signed by a trusted publisher, published to a manifest; lode runs it after verification passes. **lode is not tied to the application language** (how it runs is specified by `[command]` in `lode.toml`).
- **Why Rust**: a Bun-compiled lode is ≈ 91MB (it embeds the JS runtime), and the image has to carry it along; a static Rust build (musl) is ≈ a few MB and runs on **any** base (scratch, distroless, or a fuller image like `zzci/ubase`) — this is what truly delivers "language-agnostic + small."
- **Relationship**: `universal image (zzci/ubase + lode)` + `remote manifest/signed packages` → on container start, verify and run the application per policy. Switching apps = switching `[update].manifest` (env `LODE_MANIFEST`), with zero image rebuilds.

### Decided design (hard constraints)

1. **Single program, follows a single channel**: one lode manages exactly one app; a manifest may define **multiple channels** (`[channels.*]`), and a lode instance **follows one of them** per `LODE_CHANNEL`; a version exposes **named assets**, and the operator picks the exact one for this host by filename (`[update].asset`) — no platform detection.
2. **Three-file division of labor**: `lode.toml` (local TOML, pure configuration, **the app does not write it**), `state.json` (local JSON, **written by both lode and app**, the communication hub), `manifest.json` (**remote JSON**, not persisted locally). The app communicates with lode by **reading/writing `state.json`** (reads `available`, writes `target`/`restart_nonce`); other realtime/RPC communication is out-of-band and handled by the app itself. See §7.
3. **Download driven by the manifest, run driven by lode.toml**: an artifact's `format` determines how it lands on disk; the run commands `run`/`exec` (+ optional `[runtime]`) are written in lode.toml, with **no `kind`**.
4. **Never jump versions on its own**: only download the latest when no installed version exists locally; if a version exists, start the existing version first, and leave updates to policy and app/CLI triggers.
5. **Two-layer verification**: integrity (sha256) + publisher identity (ed25519), enforced per `trust.require_signature`.
6. **Child-process launch + service management**: lode always spawns and supervises as a child process, providing start/stop/restart/status; readiness/stop handshakes avoid killing the process prematurely; optional zero-downtime restart (inspired by overseer).
7. **Unified configuration**: `CLI > environment variables (LODE_*) > lode.toml (TOML) > defaults`.
8. **Private sources**: `[http].headers` pass through authentication; private keys/credentials never go into the image.
9. **Rust modularity**: split into multiple files by module (see §14), compiling into a single binary.

---

## 2. Overall architecture

```
universal image (built once)      ┌──────────────────────────────────┐
zzci/ubase                        │  lode (static Rust binary, a few MB) │
                                  └────────────────┬─────────────────┘
                  lode.toml (local config) │ read CLI / env / lode.toml
                                           ▼
   update.manifest ──HTTPS(+headers)──► remote manifest.json (channels + versions→assets[name])
                                           │  (remote, not persisted locally)
                       ┌───────────────────┼────────────────────────────┐
                       ▼                   ▼                            ▼
              select asset by filename ([update].asset)  download → verify sha256 + ed25519   trusted public key
                       │                            │ all pass → land/unpack per format
                       ▼                            ▼
            current ─► versions/<ver>  (rename = atomic switch; metadata stored per version for offline use)
                       │
                       ▼
            lode spawns and manages as a child process:  <lode.toml [command].run/exec>  (start/stop/restart/hot-update/rollback/optional zero-downtime)
                       │                        ▲
            $DATA_DIR/state.json (written by lode+app) │ poll state.json mtime
                       └────────► app ◄───────────┘ app reads state.available as a hint / writes state.target|restart_nonce to trigger
```

---

## 3. Runtime form: Rust modularity + small static binary

- Split into multiple files by module (§14), compiling into a **single static binary**; `*-unknown-linux-musl` + `+crt-static`, with `ldd` showing "not a dynamic executable".
- **Image**: `FROM zzci/ubase` (a general-purpose base with libc/shell/tools, so it can also host a runtime lode downloads at boot) → `COPY lode /usr/bin/lode` → `ENTRYPOINT ["/usr/bin/lode"]`. lode's TLS roots are bundled (webpki-roots), so no system CA certificates are required. lode is a static binary that also runs on a minimal base (`scratch`/distroless) when the app brings its own runtime or is itself static.
- **Dependencies (pure Rust / pma-rust compliant, no tokio/axum, synchronous implementation to stay small)**:
  - HTTP: `ureq` + `rustls` (aws-lc-rs provider, `install_default()` in `main`)
  - Serialization: `serde` + `serde_json` (manifest/state) + `toml` (lode.toml); versioning: `semver`
  - Verification: `sha2`, `ed25519-dalek`, `base64`
  - Unpacking: `flate2` (miniz_oxide, pure Rust) + `tar` (+ optional `zip`)
  - CLI: `clap` (derive, env); errors: `thiserror` (+ `anyhow` only in main)
  - Logging: `tracing` + `tracing-subscriber`
  - Process/signals: `std::process` + `signal-hook` (receive) + `nix` (send signals to child processes, safe API)
  - Robustness: `rlimit` (suppress core dumps)
- **`#![forbid(unsafe_code)]`**: fd passing (optional zero-downtime) goes through `command-fds`/`socket2` wrappers; this crate writes no unsafe code.
- The key primitives were validated locally in an equivalent Bun prototype (O_EXCL lock, atomic symlink, sha256, ed25519, child-process signals, streaming download); the Rust side implements them with corresponding safe crates.

---

## 4. Language-agnostic application model: format (packaging) + run/exec (running)

Division of labor: **landing is delegated to the manifest's `format`, running is delegated to `[command]` in `lode.toml`**. lode itself only does selection/download/verification/activation/supervision.

### format — packaging format (how the downloaded item lands on disk)

| format | Meaning | Landing action |
|---|---|---|
| `raw` | URL is the final single file (binary or script) | download → store as `versions/<v>/<entry or basename>` |
| `gz` | single-file gzip (`.gz`) | download → gunzip → single file |
| `tar.gz` | gzip tar of a directory tree (`.tar.gz`/`.tgz`) | download → unpack into `versions/<v>/` |
| `zip` | zip archive | download → extract into `versions/<v>/` |

**`format` is always derived from the asset filename's extension** (longest match — `.tar.gz`/`.tgz`→`tar.gz`, `.gz`→`gz`, `.zip`→`zip`, else `raw`; see §12). It is never stored in the manifest nor signed. **sha256 is always computed over the "raw downloaded file (before unpacking)."**

### How to run (`run` / `exec` in the `[command]` section, no `kind`)

"How to run" is entirely configured by the operator in the `[command]` section of `lode.toml`; the manifest only declares "what to download" (`name`/`url`/`sha256`/advisory `entry`). The command is a string (split on whitespace) or an argv array, with placeholders `{entry}` = absolute path of the entry point, `{dir}` = the version directory. **argv is executed directly, not through a shell; no `kind` is needed.**

- **`run`**: **the command that launches the app when `lode` runs bare**. When `{entry}` is missing, it is automatically appended at the end. E.g. `"bun run"` → `bun run <entry>`; for a self-contained binary, `"{entry} serve"`.
- **`exec`**: **the base command for `lode <args>` passthrough**, with the CLI arguments appended after it. E.g. `"bun"` → `lode run db:init` means `bun run db:init`; for a self-contained binary, `"{entry}"`.
- **`workdir`**: the child process's cwd, `{dir}` (default) or an absolute path. stdio is inherited from lode.
- After installation, `chmod +x` is applied uniformly to `entry` (harmless for scripts, and saves the `kind` judgment).

### Runtime download (`[runtime]`, optional)

When `run`/`exec` depends on a runtime (such as `bun`), it can be declared in the `[runtime]` section of `lode.toml`. Resolution order is **PATH → cache → download**: lode first checks PATH; otherwise it reuses a previously downloaded runtime from `$DATA_DIR/runtime/<name>`; otherwise it downloads `download`. A self-contained binary omits this table.

- **Format / hoist**: the `format` is inferred from the URL extension (`raw`/`gz`/`zip`/`tar.gz`); after extraction the named binary is hoisted to `runtime/<name>`, so nested official archives work (bun's `bun-linux-x64/bun`, node's `node-vX/bin/node`) as well as flat ones (deno) and single files.
- **Cache**: the placed `runtime/<name>` is reused on later boots — with a persistent `$DATA_DIR` the download is a one-time cost. Delete the file to force a re-download.
- **Unverified**: unlike an app artifact, a runtime download carries **no `sha256`/`sig`** and is **not** integrity/identity-checked. Pin a version, host on a trusted origin, and add its host to `[http].credential_hosts` if it needs credentials.
- **Version pin** (`version` + `version_check`): when `version` is set, lode probes the runtime it's about to use (PATH/cache/downloaded) by running it with `version_check` (default `--version`) and requires the output to **contain** `version`. A wrong-version PATH/cache entry is bypassed for a fresh download; a downloaded mismatch is a hard error.
- lode then **prepends `$DATA_DIR/runtime/` to the child's PATH**.

```toml
[runtime]
runtime       = "bun"                                    # the executable name used by run/exec
download      = "https://example.com/bun-linux-x64.zip"  # downloaded when bun is not found on PATH/cache
version       = "1.1.38"                                 # optional: require this version (substring of the probe output)
# version_check = "--version"                            # optional: arg(s) that print the version (default --version)
```

### Asset selection (by filename)

A version's `assets[]` are keyed by **filename** (`name`, e.g. `myapp-linux-x86_64.tar.gz`). The operator sets `[update].asset` to the exact asset for this host; lode matches it against the source's asset list by `name`. There is **no platform auto-detection and no arch-alias table** — the filename is the single selection key (the same key in both the native and GitHub sources), and its extension fixes the `format`. The filename carries the brand/platform by convention. See §6/§12 and [`docs/source-adapters.md`](source-adapters.md).

---

## 5. Version lifecycle and update strategy

### Startup decision (never jump versions on its own)

```
read state.json to get current
┌─ current does not exist (first run / brand-new data disk)
│     → bootstrap: fetch remote manifest → take latest → select artifact → download + verify (sha256+sig) + land
│     → current = latest, last_good = latest → spawn child process
└─ current already exists
      → activate and start current directly (don't wait for the network, fast startup)
      → afterwards handle updates in the background per policy
```

If current does not exist and there is neither `[update].manifest` nor a locally available version → error and exit. Offline/air-gapped: pre-place a local manifest + version directories + trusted public key for network-free startup.

### Update strategy `update.policy` = `off` | `check` | `auto`

| Value | Name | Behavior |
|---|---|---|
| `off` | Off | No background checks; only runs current/pinned. Still honors an explicit `target` (fetches the remote once on demand to resolve the entry). |
| `check` (default) | Check | Periodically fetch remote → refresh the local catalog, write `state.available`, **does not auto-apply**; once the app reads it, it prompts → writes `state.target` → lode executes. |
| `auto` | Auto | Periodically fetch remote → if `latest > current`, automatically set `target=latest` and hot-update. |

`update.pin` locks a version (equivalent to `off` + a fixed target); under any policy an explicit `state.target` is honored and executed (provided verification passes). The check cadence is `update.check_interval` seconds (0 = check only once at startup; `off` does not time). 

### Applying a target (hot-update + rollback)

```
target ≠ current and available (stop-start mode):
  1. ensure it is installed (if missing, download + verify + land into versions/<target>)
  2. write state.status=updating → send SIGTERM to the old child process (the app cleans up and exits) → SIGKILL only after STOP_TIMEOUT
  3. atomically switch the current symlink → versions/<target>
  4. spawn the new child process; wait for readiness (§8: readiness=none → alive for the full grace; readiness=state → wait for state.ready)
  5. ready → status=running, last_good=target; **a single failure triggers rollback**: readiness timeout / exiting or crashing once within the observation window (health_grace) → switch back to last_good and observe it the same way; if last_good also fails within its grace → lode exits (no further retry loop)

Zero-downtime (reuseport-overlap / socket-activation): start the new process first, **stop the old one only after it becomes ready** (§8), avoiding a gap and premature killing of the old process.
```

### Startup cleanup (GC + orphan child processes)

`lode` startup (service mode) runs a cleanup pass before determining the version:

- **Orphan child processes**: a previous lode crash may have left an app child process still running. After taking over the stale lock, read `state.json.pid`, and if that process is still alive → terminate it gracefully (SIGTERM → SIGKILL on timeout) before starting a new one, avoiding port/resource conflicts and dual instances.
- **Garbage collection**: clean up `downloads/*.part` and `versions/<v>.tmp` half-finished artifacts left by interruptions; per `keep_versions`, retain current + last_good + the most recent N versions, deleting the rest of the version directories; `$DATA_DIR/runtime/` likewise keeps only what's in use.
- **Verify on-disk consistency**: if the version the `current` symlink points to is missing/corrupted → fall back to last_good or re-bootstrap.

---

## 6. Verification and trust: file integrity + publisher identity

- **Integrity**: compute sha256 over the raw downloaded file; it must equal the artifact's `sha256` (lowercase hex).
- **Identity**: ed25519. The publisher's private key signs a "release record," and lode verifies the signature with the pre-placed **trusted public key** → even if the CDN/mirror is poisoned or the transfer is MITM'd, it can confirm that the publisher issued it and that it has not been tampered with.

### Signature canonical bytes (exact, UTF-8, `\n`-separated, no trailing newline)

The signature message for an asset-level `sig`:
```
lode.artifact.v1
{name}
{version}
{sha256}
```
`{name}` is the **asset filename** (the selection key), `{version}` the release version, `{sha256}` the lowercase-hex digest of the raw downloaded file. The signature binds *which asset*, *which release*, and *which bytes*; `format`/`entry`/`url` are derived from the filename or are operator-local and are **not** signed, so a tampered catalog cannot move a genuine signature onto different bytes, a different asset, or a different version. Verify with the trusted public key corresponding to `key_id` (asset.key_id ?? manifest.key_id).

> **What is signed is the sha256 (digest), not the bin stream directly.** ed25519 signs the canonical text string above (which contains `{sha256}`) — equivalent to "signing the digest + identity fields." The full verify chain: **downloaded bytes → recompute sha256 → (integrity) == artifact.sha256 → (identity) verify this signature**. The sha256 already binds the content, so there is no need to stream the entire binary through the signer; this is also the conventional approach for release signing (such as signing `SHA256SUMS`).

Optional top-level manifest `sig`, set by `lode-cli manifest-sign` and verified before any version is resolved/downloaded, preventing the addition/removal of versions, a swapped channel `latest`, or rewritten URLs:
```
lode.manifest.v1
{name}
{key_id}
{canonical}     # deterministic, sig-free serialization of channels + versions
```
`{canonical}` is built from the parsed manifest (sorted `channels`/`versions`), so signer and verifier produce identical bytes regardless of JSON key order or whitespace; the top-level `sig` itself is excluded from the signed body. The key is selected by the manifest's `key_id` (falling back to trying every trusted key).

### Trusted public keys / strength

- `LODE_TRUSTED_KEYS` = comma-separated `key_id:base64(32-byte raw ed25519 public key)`; or `LODE_TRUSTED_KEYS_FILE` (each line `key_id base64`). Multiple keys are supported (rotation). `key_id` = the first 16 hex digits of `sha256(32-byte public key)`.
- `LODE_REQUIRE_SIGNATURE` = `off` (sha256 only) | `auto` (default) | `enforce` (recommended for production).
  - **`auto` is fail-closed once any trusted key is configured**: both the manifest signature and the artifact signature are then required — a missing *or* invalid signature is rejected. Only when **no** trusted key is configured does `auto` skip the signature check and log the source as **UNVERIFIED**.
  - **`enforce`** always requires trusted keys plus a valid manifest signature and a valid artifact signature.

### CLI (publisher)

`lode-cli keygen` / `lode-cli sign <asset> --version <ver> --key <priv>` (or `--key-env <VAR>`; prints `sha256`/`sig`/`key_id`, where `sig` doubles as the GitHub asset `label`) / `lode-cli verify <asset> --version <ver> --pubkey <b64> --sig <b64>` / `lode-cli manifest <asset> --version <ver> --url <url> [--entry <e>] --key <priv> --into manifest.json` / `lode-cli manifest-sign --into manifest.json --key <priv>` (stamp the catalog's top-level `key_id` + `sig`). The private key stays offline; lode holds only the public key.

---

## 7. File model and lode ↔ app communication

Three concepts, with clear responsibilities:

| File | Location | Format | Who writes | Role |
|---|---|---|---|---|
| **`lode.toml`** | local | TOML | **only the lode side** (written by the operator; the app does not write it) | **the app's configuration**: how to launch and configure the program (manifest URL/channel/exec/workdir/headers/policy/trust/`pin`) |
| **`state.json`** | local | JSON | **both lode and app can write** | runtime communication hub: lode writes the actual state, the app writes update/restart requests |
| **`manifest.json`** | **remote** | JSON | publisher | the remote program's version catalog; lode fetches it per policy, **not persisted locally** (each version's metadata is stored in the version directory for offline running) |

All communication goes through **`state.json`** (bidirectional, each side writing its own fields + atomic temp+rename write; lode polls its mtime):

- **lode writes**: `current`/`last_good`/`available`/`status`/`pid`/`last_check`/`last_error`/`history`/`channel`.
- **app writes** (requests): `target` (the version to upgrade/downgrade to, or `"latest"`), `restart_nonce` (incremented = restart request), `ready` (readiness handshake: written as this startup's `LODE_INSTANCE` value to indicate "I can serve now," see §8).
- Typical flow (`policy=check`): lode writes the newly discovered version into `available` → **once the app reads it, it decides on its own to upgrade by writing `target` to that version (or `"latest"`)** → lode applies and hot-updates.

**Notification mechanism (bidirectional, neither needs an extra signal):**

- **app → lode** (request a restart/upgrade): the app **atomically writes `state.json`** (changing `target` or incrementing `restart_nonce`); **lode polls `state.json`'s mtime at a short interval (~1s)**, and on a change re-reads and executes. **The file itself is the notification**, so there's no need to send a signal to lode (which also avoids conflicting with signals forwarded to the child process).
- **lode → app** (request a restart, letting the app clean up): lode first sets `state.status` to `updating` (hot-update) or `stopping` (shutdown) so the app can distinguish them, then sends **`SIGTERM`** to the child process; in its SIGTERM handler the app does cleanup (draining/flush/release) and then `exit(0)`, which must happen within `supervise.stop_timeout`, otherwise `SIGKILL`. After exit, lode switches the version and starts a new process (§5).

> `lode.toml` is pure configuration, **the app does not write it** (only the lode side/operator writes it); to lock a version, use `pin` in it. For a complete `lode.toml`, see `docs/lode.example.toml`.

### `$DATA_DIR/state.json` (written by lode+app)

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
The fields owned by lode vs. the fields owned by the app (`target`/`restart_nonce`) do not overlap; `status` ∈ `starting|running|updating|rolling-back|stopping|stopped|error`; `result` ∈ `good|bad`.

---

## 8. Child-process supervision, service management, and zero-downtime restart (inspired by overseer)

- The full lifecycle is managed by lode: start / graceful stop / restart / status / hot-update / rollback, exposed via the CLI (§13) and the manifest (§7).
- **Container init responsibilities (often runs as PID 1)**: lode installs signal handlers (PID 1 has no default handlers) and **reaps zombies** — it sets itself as a child subreaper and loops `waitpid` to harvest reparented grandchildren, preventing zombie buildup. `docker stop` sends `SIGTERM` to PID 1 (default `SIGKILL` after 10s): lode must forward it and exit within that grace period (`stop_timeout` should be < Docker's grace).
- **Signal passthrough (service mode)**: lode acts as the master, **forwarding externally received signals to the child process as much as possible**, without deciding behavior on the app's behalf:
  - Termination-class `SIGTERM`/`SIGINT`/`SIGQUIT`: lode initiates a **graceful shutdown** — forward to the child process → wait for it to exit (SIGKILL on timeout) → release the lock → lode exits with the child process's exit code.
  - Passthrough-class `SIGHUP`/`SIGUSR1`/`SIGUSR2`/`SIGWINCH`/`SIGCONT`/`SIGTSTP`/…: **forwarded as-is to the child process** (e.g. an app using `SIGHUP` to reload); lode does not consume them.
  - Optional `signals.restart` (env `LODE_RESTART_SIGNAL`, e.g. `SIGUSR2`): once set, that signal instead **triggers a graceful lode restart** (equivalent to `restart_nonce++`) and is no longer forwarded; **unset by default**, to avoid occupying an app signal (restart goes via state.json/CLI).
  - `SIGKILL`/`SIGSTOP` cannot be caught (OS restriction); `SIGCHLD` is used by lode to sense child-process exit. The passthrough set can be adjusted with `signals.forward` (env `LODE_FORWARD_SIGNALS`).
- **Restart policy `supervise.restart`** (`off`|`on-failure`|`always`, default `off`):
  - `off` (default) = lode **mirrors the child process's lifecycle** — when the child process exits on its own (a stop not initiated by lode), lode exits with its exit code (`exit(code)` → `code`; `signal(sig)` → `128+sig`), leaving whole-process restart to the orchestrator; it does **not** auto-relaunch.
  - `on-failure` = restart only on **failure** (a non-zero exit or being killed by a signal); on a clean `exit(0)`, lode follows and exits.
  - `always` = restart on any exit.
  - `on-failure`/`always` use **exponential backoff** (base `RESTART_BACKOFF`, capped at `..._MAX`; `RESTART_MAX`>0 — after the consecutive cap is reached, lode exits with status=error; the consecutive count resets once the child process stays alive for the full `health_grace`) — these three keys **take effect only when `restart != off`**.
  - **Regardless of policy**, transitions initiated by lode restart the child process as usual: updates (applying `state.target` / a `policy=auto` update), rollback, an explicit restart (`restart_nonce` or the restart signal). When the child process exits and there is a **pending update** (`state.target` points to a different installable version, or a latest update under `policy=auto`), the new version is launched per the update rather than restarting the original version.
- **Health/rollback**: see §5 (rollback is **single-shot**).

### Run modes — bare = launch, with arguments = passthrough (no flags/subcommands needed)

A minimal rule:

- **`lode` (bare) = launch and supervise the service**: lock (single instance) → determine/install the version (+ download the runtime if needed) → run **`run`** (from lode.toml, auto-appending `{entry}`) → supervise (per the `supervise.restart` policy, default mirrors the child process) → poll for hot-update/rollback per policy → signal passthrough as above. Suited to a server/daemon. With image `ENTRYPOINT ["/usr/bin/lode"]`, `docker run img` launches it.
- **`lode <args...>` (with any arguments) = CLI passthrough**: **no lock, no supervision, no polling**. Verify the target version (bootstrap if none) → **exec replace** with **`exec` + `<args>`**, passing through stdin/stdout/stderr (including TTY), with signals and exit codes handled natively by the OS.
  - `lode run db:init` → `exec + ["run","db:init"]` (if `exec="bun"`, then ≡ `bun run db:init`). **No `--` needed**; **no `run` subcommand is used**, so it doesn't conflict with `bun run`.
  - Positional arguments pass through via clap `trailing_var_arg` + `allow_hyphen_values` (`lode` has no subcommands; all arguments belong to the app).
  - exec replacement uses the safe `std::os::unix::process::CommandExt::exec`, preserving `#![forbid(unsafe_code)]`.
- **`lode` itself has no subcommands at all**; operations and publishing actions (status/update/…/keygen/sign/manifest/init) all live under the symlink **`lode-cli`** (see §13). Therefore `run`/`migrate` and the like are always passed through to the app and never shadowed.

### Restart mode `supervise.restart_mode`

| Value | Behavior | App cooperation |
|---|---|---|
| `stop-start` (default) | stop the old then start the new, with a very short port gap | none; works for non-network services too |
| `socket-activation` | lode holds the listening socket and **uses the systemd socket-activation protocol** (`LISTEN_FDS`/`LISTEN_PID`/`LISTEN_FDNAMES`, fds starting at 3) to pass it to the child process; on hot-update it keeps the socket, starts the new process to reuse the fd, and lets the old one drain and exit → **true zero-downtime** | the app supports socket activation (Go/Rust/Node/nginx…) |
| `reuseport-overlap` | start the new process to coexist with the old, then stop the old once the new is healthy → zero-downtime | the app opens `SO_REUSEPORT` |

> **No systemd in the container? It doesn't matter.** socket-activation is a **protocol (environment variables + inherited fds)** that does not depend on the systemd process — **lode acts as the activator itself** (binds the socket, passes the fd, sets `LISTEN_FDS`), and the app only needs to read fd 3. Inside a container, prefer the simpler `reuseport-overlap`, or the default `stop-start`.
> `socket-activation` requires `LODE_LISTEN` (e.g. `0.0.0.0:3000`); fd passing goes through the `command-fds` wrapper, preserving `#![forbid(unsafe_code)]`.
> Difference from overseer: overseer is an **in-process library** and **does not auto-restart on crash** (it exits with the same code); lode is an **external general-purpose supervisor**, which by default also mirrors the child process (`restart=off`) and additionally offers optional crash restart (`on-failure`/`always`) + rollback, making it a superset. **Zero-downtime is an optional advanced feature; v1 defaults to `stop-start`, with socket-activation/overlap enabled later as needed.**

### Readiness / stop handshake (key: don't kill the process before the app is ready)

To avoid "switching traffic before the new process is up, or killing the old process before it finishes cleanup," two handshakes are agreed upon:

**① Readiness handshake (startup direction) `supervise.readiness`**

Every time lode spawns the child process, it injects a unique instance number **`LODE_INSTANCE`** (env).

| `readiness` | When lode considers it "ready/successful" |
|---|---|
| `none` (default) | **Alive for the full `health_grace` seconds** counts as ready/good (suitable for programs with no readiness signal). |
| `state` | **Wait for the app to write `state.ready == this startup's `LODE_INSTANCE``** (the app self-reports "I can serve now"); if it isn't received within `supervise.ready_timeout` (default 30s) → judged a failure (rollback/restart). |

Before readiness, lode does **not** do these things: it does not set `status=running`, does not mark last_good, and **(`reuseport-overlap`/`socket-activation`) does not stop the old process**. → The old instance keeps holding up until the new instance self-reports readiness, achieving true zero-downtime.

**② Stop handshake (shutdown direction) `supervise.stop_timeout`**

After lode sends `SIGTERM`, it **absolutely will not SIGKILL within `stop_timeout` seconds**, giving the app ample time to clean up. The agreement:
- The operator sets `stop_timeout` to ≥ the app's worst-case cleanup time, and **< the container/orchestrator's stop grace** (e.g. Docker's default 10s, otherwise the container layer will SIGKILL first).
- On receiving `SIGTERM`, the app must finish up as quickly as possible and `exit(0)`; only a timeout results in SIGKILL.

> With the default `readiness=none`, "ready" degenerates into "alive for the full `health_grace`" — which is not rigorous enough for zero-downtime, so `reuseport-overlap`/`socket-activation` are recommended with `readiness=state`.

---

## 9. PID protection

- `$DATA_DIR/lode.pid`, atomically created with O_EXCL (`create_new`), containing the lode pid + the application name.
- Already exists → probe liveness (`nix::sys::signal::kill(pid, None)` / `kill -0`): alive → the current process exits (single instance); `ESRCH` → delete the stale lock and take over.
- Delete the lock on normal exit/signal receipt.

---

## 10. Configuration system

**Priority: `CLI > environment variables > lode.toml config file (`LODE_CONFIG`, TOML) > defaults`.**

Key names: `lode.toml` uses snake_case (see `docs/lode.example.toml`); environment variables use `LODE_*`.

| Environment variable | CLI | lode.toml key | Default | Meaning |
|---|---|---|---|---|
| `LODE_CONFIG` | `--config <path>` | — | `lode.toml` | path to the lode.toml config file (TOML) |
| **`[global]`** | | | | |
| `LODE_APP_NAME` | `--app <name>` | `global.app` | `app` | application name (names the data directory/lock; must match the manifest `name`) |
| `LODE_DATA_DIR` | `--data-dir <path>` | `global.data_dir` | `/srv/lode` | runtime/base directory: `lode.toml` + versions/state/lock + `runtime/`; by default `lode.toml` is also looked up here, and if missing a starter config is auto-generated |
| `LODE_LOG_LEVEL` | `--log-level <lvl>` | `global.log_level` | `info` | trace/debug/info/warn/error |
| **`[update]` — source + upgrade strategy** | | | | |
| `LODE_MANIFEST` | `--manifest <url>` | `update.manifest` | — | **native source**: lode/v1 manifest URL (mutually exclusive with `github`) |
| `LODE_GITHUB` | `--github <owner/name>` | `update.github` | — | **github source**: the repository (mutually exclusive with `manifest`) |
| `LODE_GITHUB_API` | `--github-api <url>` | `update.github_api` | `https://api.github.com` | (github) API base URL (GHE) |
| `LODE_ASSET` | `--asset <file>` | `update.asset` | — | the asset **filename** to install on this host (the selection key, §4/§12) |
| `LODE_ENTRY` | `--entry <path>` | `update.entry` | — | override the in-archive entry path (advisory; usually omitted, §4) |
| `LODE_CHANNEL` | `--channel <name>` | `update.channel` | `stable` | the channel to follow (a manifest may define multiple) |
| `LODE_UPDATE_POLICY` | `--policy <off\|check\|auto>` | `update.policy` | `check` | update strategy, §5 |
| `LODE_CHECK_INTERVAL` | `--interval <sec>` | `update.check_interval` | `300` | check interval in seconds; 0 = only once at startup |
| `LODE_KEEP_VERSIONS` | `--keep <n>` | `update.keep_versions` | `3` | number of old versions to retain |
| `LODE_PIN_VERSION` | `--pin <ver>` | `update.pin` | — | lock a version (operator) |
| **`[http]` — fetch credentials** | | | | |
| `LODE_HEADERS` | `--header <h>` (repeatable) | `http.headers` | — | HTTP headers passed through to manifest/artifact/runtime downloads (`"Name: Value"`), supporting `${ENV}` expansion, §11 |
| **`[trust]` — signature verification** | | | | |
| `LODE_REQUIRE_SIGNATURE` | `--require-signature <off\|auto\|enforce>` | `trust.require_signature` | `auto` | signature-verification strength, §6 |
| `LODE_TRUSTED_KEYS` | `--trusted-keys <list>` | `trust.trusted_keys` | — | trusted public keys `key_id:base64`, comma-separated |
| `LODE_TRUSTED_KEYS_FILE` | `--trusted-keys-file <path>` | `trust.trusted_keys_file` | — | trusted public keys file |
| **`[command]` — how to run** | | | | |
| `LODE_RUN` | `--run <cmd>` | `command.run` | `{entry}` | **bare-run launch command** (`{entry}` auto-appended when missing), see §4 |
| `LODE_EXEC` | `--exec <cmd>` | `command.exec` | `{entry}` | **CLI passthrough base command** (`lode <args>` appended after it), see §4 |
| `LODE_WORKDIR` | `--workdir <path>` | `command.workdir` | `{dir}` | child process cwd (version directory or absolute path), see §4 |
| **`[env]` — extra child env** (config-file only; no CLI/env override) | | | | |
| — | — | `[env]` (table) | — | extra env vars for the child, as **defaults** — an inherited host env var of the same name wins (lode's own `LODE_*` win over all), see §4 |
| **`[runtime]` — optional runtime** | | | | |
| `LODE_RUNTIME` | `--runtime <name>` | `runtime.runtime` | — | runtime executable name (used by run/exec), see §4 |
| `LODE_RUNTIME_DOWNLOAD` | `--runtime-download <url>` | `runtime.download` | — | download URL when the runtime is missing (cached + reused; not signature-verified) |
| `LODE_RUNTIME_VERSION` | `--runtime-version <ver>` | `runtime.version` | — | required runtime version; probed and matched as a substring of the probe output, §4 |
| `LODE_RUNTIME_VERSION_CHECK` | `--runtime-version-check <args>` | `runtime.version_check` | `--version` | arg(s) that print the runtime version (used only with `version`) |
| **`[supervise]` — supervision (restart policy/health/rollback/stop/restart mode)** | | | | |
| `LODE_RESTART` | `--restart <off\|on-failure\|always>` | `supervise.restart` | `off` | restart policy: `off` = mirror the child process (lode exits with it); `on-failure` = restart only on crash; `always` = restart on any exit, §8 |
| `LODE_RESTART_BACKOFF` | `--restart-backoff <ms>` | `supervise.restart_backoff` | `500` | restart backoff base (exponential); effective only when `restart != off` |
| `LODE_RESTART_BACKOFF_MAX` | `--restart-backoff-max <ms>` | `supervise.restart_backoff_max` | `30000` | backoff cap; effective only when `restart != off` |
| `LODE_RESTART_MAX` | `--restart-max <n>` | `supervise.restart_max` | `0` | consecutive restart cap, 0 = unlimited; effective only when `restart != off` |
| `LODE_READINESS` | `--readiness <none\|state>` | `supervise.readiness` | `none` | readiness determination: `none` = alive for the full grace; `state` = wait for the app to write `state.ready`, §8 |
| `LODE_READY_TIMEOUT` | `--ready-timeout <sec>` | `supervise.ready_timeout` | `30` | with `readiness=state`, the max wait for readiness; a timeout is judged a failure |
| `LODE_HEALTH_GRACE` | `--health-grace <sec>` | `supervise.health_grace` | `15` | (readiness=none) the new version must stay alive for this many seconds to be good; also the observation window for single-shot rollback |
| `LODE_STOP_TIMEOUT` | `--stop-timeout <sec>` | `supervise.stop_timeout` | `10` | SIGKILL after the graceful-stop timeout |
| `LODE_RESTART_MODE` | `--restart-mode <mode>` | `supervise.restart_mode` | `stop-start` | restart strategy, §8 |
| `LODE_LISTEN` | `--listen <addr>` | `supervise.listen` | — | socket-activation listen address |
| **`[signals]` — signals** | | | | |
| `LODE_FORWARD_SIGNALS` | `--forward-signals <list>` | `signals.forward` | (standard set) | the set of signals forwarded to the child process, §8 |
| `LODE_RESTART_SIGNAL` | `--restart-signal <sig>` | `signals.restart` | — | the signal that triggers a graceful restart (unset by default), §8 |

Child-process environment: pass through the host environment and **strip configuration-class `LODE_*`**; apply the operator's `[env]` table as **defaults** (only for keys the host env doesn't already set); prepend the runtime dir to PATH; then inject the read-only introspection variables `LODE_ACTIVE_VERSION`, `LODE_DATA_DIR`, `LODE_INSTANCE` (the unique number for this startup, used for the readiness handshake, §8). Precedence low→high: `[env]` defaults < inherited host env < runtime PATH-prepend < injected `LODE_*`.

---

## 11. Private packages / authentication (header list passthrough)

When downloading the manifest and artifacts, the HTTP headers in the `headers` list are **passed through as-is** to the request — one mechanism covers any authentication (Bearer, `X-Api-Key`, custom headers all work), with no need to model each scheme separately:

```toml
# lode.toml
headers = [
  "Authorization: Bearer ${RELEASE_TOKEN}",   # ${ENV} is expanded from lode's environment, so the secret never lands in a file
  "X-Api-Key: ${API_KEY}",
]
```

- `${VAR}` is expanded from the lode process environment at load time (it's recommended to inject via container secret/env, not writing plaintext in the header).
- Setting `"auth": false` on an artifact → that URL has **no** headers added (e.g. a pre-signed link that already carries its own signature).
- CLI: `--header "Name: Value"` (repeatable); env: `LODE_HEADERS` (newline-separated).
- Authentication (the transport layer's "able to download") and signature (the trust layer's "really issued by them," §6) are orthogonal; for private sources, using both is recommended.
- headers (including their expanded values) / private keys are **never written to logs/`state.json`**; logs redact URL query strings.

---

## 12. Manifest format spec (lode/v1) — the complete contract

The remote manifest is provided by the publisher, in **JSON format** (UTF-8), fetched by lode from `[update].manifest`, and **not stored locally**. **For a complete maximal example, see [`docs/manifest.example.json`](./manifest.example.json)**. Structural contract:

- Top level: `schema` (required, `"lode/v1"`), `name` (required, must match `app` in `lode.toml`), `key_id` (optional, the default signing public-key id), `sig` (optional, the ed25519 catalog signature, §6).
- `channels` (required): an object, keyed by channel name, with values containing `latest` (a version id). **Multiple channels are allowed**, and lode follows one per `channel`.
- `versions` (required): an object, keyed by version id (referenced by a channel's `latest`), with values containing `notes` (optional) + an `assets` array (≥1).
- Each asset is keyed by its **filename** (`name`); the operator selects one via `[update].asset`:

| Field | Required | Description |
|---|---|---|
| `name` | ✓ | the asset **filename** (e.g. `myapp-linux-x86_64.tar.gz`) — the selection key and the signed identity; its extension fixes the `format` (§4) |
| `url` | ✓ | absolute download URL |
| `sha256` | ✓ | lowercase hex digest of the downloaded file (before unpacking) |
| `sig` | conditional | base64 ed25519 over the §6 message `(name, version, sha256)`; required under `require_signature=enforce` (and under `auto` once a trusted key is set) |
| `key_id` | | overrides the top-level `key_id` |
| `entry` | | advisory in-archive entry path (§4); resolution is manifest `entry` > `[update].entry` > convention |
| `size` | | expected byte count (an extra layer of protection) |
| `auth` | | default `true`; `false` = no passthrough headers added to this URL (pre-signed) |

> **No `platform`, no `format`, no `kind`**: the asset is chosen by filename, the format is derived from its extension, and the run commands (`run`/`exec`/`workdir` + optional `[runtime]`) all live in `lode.toml` (§4/§7/§10). The manifest only declares "what to download" (`name`/`url`/`sha256` + advisory `entry`); the operator decides "how to run." After installation, `chmod +x` is applied uniformly to the entry.

**Minimal example (single-file JS script, public source, unsigned)**:
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
(The operator sets `[update].asset = "hello.js"`. The run command is configured in `lode.toml`: for a script use `run = "bun run"`, `exec = "bun"`; add `[runtime]` to download bun as needed.)

### Packaging (release flow, language-agnostic)

```bash
# binary (Go/Rust/bun --compile): name the asset so its extension fixes the format
tar -czf myapp-linux-x86_64.tar.gz -C build myapp
lode-cli sign myapp-linux-x86_64.tar.gz --version 1.5.0 --key publisher.key
#  → prints sha256 + sig + key_id (sig also doubles as the GitHub asset label)
lode-cli manifest myapp-linux-x86_64.tar.gz --version 1.5.0 \
    --url https://releases.example.com/1.5.0/myapp-linux-x86_64.tar.gz \
    --entry myapp --key publisher.key --into manifest.json   # upsert the asset by name
lode-cli manifest-sign --into manifest.json --key publisher.key   # §6 catalog signature

# single-file JS (bun build --outfile): no packaging extension → raw
bun build ./src/index.ts --target bun --outfile hello.js
lode-cli sign hello.js --version 1.0.0 --key publisher.key
```

Packaging and signing are **handled by the publisher** (any tooling in their own CI); lode ships no packaging script, only the convention; for the flow see [`docs/source-adapters.md`](source-adapters.md) and [`docs/integration.md`](integration.md) (build → sha256 → sign `(name, version, sha256)` → assemble the manifest). `lode-cli keygen`/`sign`/`manifest`/`manifest-sign` are the reference implementation.

### Manifest source: native / GitHub — determined by "which key is set" (mutually exclusive, no separate `source`)

The native manifest is the authoritative format (explicit, signable, and placeable on any static hosting: S3 / OSS / GitHub raw / gh-release asset). For "simple and universal," it also supports using **GitHub Releases** directly as a source, adapted into the same internal model, with the subsequent download/verify/install flow unchanged. **The source is determined by which key is set in `[update]`** (the two are mutually exclusive; setting both is an error):

| Key set | Source | lode behavior |
|---|---|---|
| `[update].manifest = "<url>"` | native | fetch the `lode/v1` JSON specified above. |
| `[update].github = "owner/name"` (configure `github_api` separately for GHE) | github | **use GitHub's native endpoints directly**, with no need to compute "the latest" yourself; map one release to one version. |

**channel ↔ GitHub endpoint** (use GitHub's built-in `latest`/`tags`, without reinventing the wheel):

| Scenario | GitHub endpoint | Description |
|---|---|---|
| `channel=stable` | `GET /repos/{repo}/releases/latest` | GitHub's `latest` = the most recent non-prerelease/non-draft release |
| `channel=beta`/others | `GET /repos/{repo}/releases` → take the most recent `prerelease==true` | pre-release channel |
| `pin=<tag>` | `GET /repos/{repo}/releases/tags/{tag}` | lock a specific tag |

**The release's own assets are the catalog** — there is **no `manifest.json` asset**. The adapter:
1. selects the release per the table above (latest/prerelease/tag);
2. maps each release asset to an internal asset: `name` = the asset filename, `sha256` = the asset `digest` (GitHub-computed, re-verified against the downloaded bytes), `sig` = the asset **`label`** (the only free-string slot the API returns), `url` = `browser_download_url`;
3. afterwards it is exactly the same as native (select the asset whose `name` matches `[update].asset` → verify sha256 + ed25519 → install).

- **Version number**: use the release's `tag_name` (with a leading `v` before a digit stripped) — GitHub is authoritative for the version.
- **No catalog (manifest-level) signature on GitHub**: freshness comes from tag authority; per-asset `sig` (the label) still protects each download.
- **Private repo**: put the GitHub token in `[http].headers` (`Authorization: Bearer <PAT>`), carried by the API and same-host asset downloads.

> The simplest usage needs only `github = "owner/repo"` + `asset = "<filename>"`; `stable` goes straight to `/releases/latest`. Both sources produce the same internal asset list → the same verify/install path. Signing in CI is optional — see the release-workflow recipe in [`docs/source-adapters.md`](source-adapters.md) §5.

---

## 13. CLI — multi-call binary (`lode` / `lode-cli`)

lode is a **multi-call binary**, dispatching by `argv[0]`: `lode-cli` is a **symlink** to the same binary, released together with it; inside the image both `/usr/bin/lode` and `/usr/bin/lode-cli` are on PATH.

```
# invoked as lode = pure loader, no subcommands at all
lode                       # bare run = launch and supervise the service (runs lode.toml's exec)
lode <app args...>         # with arguments = CLI passthrough (runs exec + args); lode run db:init ≡ bun run db:init

# invoked as lode-cli (symlink) = operations + publishing toolbox
lode-cli <subcommand> [args]

loader lode (no subcommands, so each argument clearly belongs to the app and never steals the app's run etc.):
  (bare run)    launch and supervise the service: lock → determine version → run exec → poll for hot-update/rollback per policy
  <app args>    CLI passthrough: verify version → exec-replace with `exec` + args; no lock/no supervision

lode-cli management (writes state.json, communicates with the running service instance):
  status       print state.json + a remote manifest summary, then exit
  update       install the latest (or --version <v>); if a service is running, write target in state.json to hot-update, otherwise just install
  rollback     set target in state.json to last_good (or --version <v>)
  restart      increment restart_nonce in state.json, having the service restart the child process
  versions     list the locally installed versions

lode-cli publishing/signing (publisher, see docs/integration.md):
  keygen       generate an ed25519 private key/public key/key_id
  sign         compute sha256 for an artifact + produce a sig
  verify       verify an artifact's sha256 + sig locally
  manifest     sign and generate / merge (--into) a lode/v1 manifest
  init         write out a starter lode.toml (example config)
```

The global arguments are the `--xxx` of §10 (parsed by clap, with `env` fallback), overriding env and lode.toml.

---

## 14. Rust module layout (multiple files)

```
Cargo.toml          # package + [workspace] + [workspace.lints] + [profile.*] + dependencies
Cargo.lock          # committed (binary project)
rust-toolchain.toml # channel = "1.96.0", components, musl targets
.cargo/config.toml  # +crt-static (musl), git-fetch-with-cli
deny.toml clippy.toml rustfmt.toml .config/nextest.toml
src/
  main.rs           # #![forbid(unsafe_code)]; install aws-lc-rs provider; panic hook; rlimit suppress core; subreaper (zombie reaping); CLI dispatch
  cli.rs            # clap definitions + subcommands
  config.rs         # Config + lode.toml parsing + merge (CLI>env>lode.toml>defaults) + validation
  error.rs          # thiserror error types
  logging.rs        # tracing initialization
  idval.rs          # path-component validation for untrusted ids (version / asset entry / runtime name)
  manifest.rs       # serde types (JSON) + both source adapters + select asset by name + format-from-extension (not persisted locally)
  http.rs           # ureq (rustls/aws-lc-rs) + headers passthrough + redaction
  verify.rs         # sha256 + ed25519 verify/sign/keygen
  download.rs       # streaming download to temp + sha256 + unpack per format
  install.rs        # versions directory + atomic symlink switch + prune; startup GC (clean *.part/*.tmp, reclaim per keep_versions)
  lock.rs           # PID lock (O_EXCL) + stale-lock takeover
  state.rs          # state.json atomic read/write
  supervisor.rs     # spawn / backoff restart / signal forwarding / graceful stop / health observation / rollback / restart mode / clean up orphan child processes at startup
  commands/         # run.rs status.rs update.rs rollback.rs restart.rs versions.rs keygen.rs sign.rs verify_cmd.rs
```

---

## 15. Data directory layout

```
$DATA_DIR/
  lode.toml                # local config (operator writes, app doesn't; can also be placed elsewhere and pointed to with --config)
  lode.pid                 # PID lock
  state.json                 # actual state (auto-generated by lode, read-only for the app)
  downloads/<ver>.part       # download staging
  versions/<ver>/            # each version (raw/gz lands a single file / tar.gz/zip unpacked / binary chmod)
    .lode.json             #   that version's metadata (entry/format etc.) for offline running
  current -> versions/<ver>  # the atomically switched current symlink
# Note: manifest.json is remote, not stored locally.
```

---

## 16. Security and boundaries

- Two-layer verification: sha256 + ed25519; `enforce` rejects anything unsigned/with a failed signature.
- Failure isolation: on download/verification failure, discard `*.part`; a new version crashing triggers automatic rollback — neither affects the running version.
- Atomicity: state/manifest temp+rename; version switching is a symlink rename, with no intermediate state.
- Credentials: tokens/private keys never land in logs/state; URL query strings are redacted; the private key stays only on the publishing side.
- Process: the child process uses an argv array, not through a shell; `#![forbid(unsafe_code)]`; `rlimit` suppresses core dumps; the panic hook emits structured logs.
- Offline-capable: with no network, fall back to the locally installed version + local manifest + local public key.

---

## 17. Deliverables

- `src/` etc. — modular Rust source, compiling into a **single static binary `lode`**.
- `Dockerfile` — the universal image (`FROM zzci/ubase` + `COPY lode /usr/bin/lode`); built from prebuilt release binaries.
- `tests/` — a bun + TypeScript end-to-end test suite (`tests/src`), example apps (`tests/apps/web-rust`, `tests/apps/web-bun`), and docker-compose integration (`tests/compose`).
- `docs/integration.md` — the end-to-end integration guide (configure `lode.toml` → app contract → publish the manifest).
- `README.md` / `README.zh-CN.md` — an overview aligned with this document (English / Chinese).

---

## 18. Child-process upgrade integration

For app authors to integrate (the graceful-exit contract, sensing updates, triggering upgrades, optional socket-activation), see **`docs/integration.md`**.
