# web-rust — lode test app (std-only Rust HTTP server)

A minimal, **dependency-free** (Rust `std` only) HTTP service that implements the
language-agnostic **lode app contract** — the same contract as the sibling
[`tests/apps/web-bun`](../web-bun). lode loads it in the
integration tests to drive install → update → rollback.

It compiles to a small, self-contained, **static-friendly** binary and lives in
its **own** Cargo workspace (the empty `[workspace]` table in `Cargo.toml`), so
lode's root cargo gate never builds it.

## Contract behaviour

| Aspect | Behaviour |
|---|---|
| HTTP | binds `0.0.0.0:$PORT` (env `PORT`, default `8080`) |
| `GET /version` | the app's own version, plain text (e.g. `0.0.1`) |
| `GET /healthz` | `200 ok` |
| Version source | `LODE_ACTIVE_VERSION` (injected by lode) wins; else baked `BUILD_VERSION` |
| Graceful stop | traps `SIGTERM`/`SIGINT`, drains, `exit(0)` sub-second (≤ ~50 ms) — well inside `supervise.stop_timeout` |
| Readiness | when `LODE_DATA_DIR` is set, atomically writes `state.json` field `ready = $LODE_INSTANCE` (temp + rename, preserving lode's fields) → makes `readiness = "state"` work |
| Bad mode | baked `BUILD_BAD=1` **or** runtime `LODE_APP_BAD=1` → `exit(1)` immediately on startup (crash within `health_grace`) |

## Build & run standalone

```sh
cargo build --release            # -> target/release/web-rust  (version 0.0.0-dev)
PORT=8080 ./target/release/web-rust
curl -s localhost:8080/version   # -> 0.0.0-dev
curl -s localhost:8080/healthz   # -> ok
# Ctrl-C (SIGINT) -> "cleanup done, exiting 0", exits 0

# print version and exit (also `lode version` passthrough when exec = "{entry}")
./target/release/web-rust version

# exercise the readiness write (writes ./state.json field "ready")
LODE_DATA_DIR=. LODE_INSTANCE=demo-1 ./target/release/web-rust
```

## Producing v0.0.1 / v0.0.2 / a crashing v0.0.3

The version is **baked at build time** (via `build.rs`). Precedence:
`BUILD_VERSION` env > a `VERSION` file next to `Cargo.toml` > `0.0.0-dev`.
A baked `BUILD_BAD=1` makes the binary crash on startup — that is how a real
"bad v0.0.3" rollback artifact is produced.

Use `build.sh` (wraps the env vars + `cargo build` + copies the binary):

```sh
sh build.sh 0.0.1 dist/0.0.1/web-rust            # good v0.0.1
sh build.sh 0.0.2 dist/0.0.2/web-rust            # good v0.0.2
sh build.sh 0.0.3 dist/0.0.3/web-rust --bad      # crashing v0.0.3 (rollback test)
sha256sum dist/*/web-rust                         # digests for manifest.json
```

Equivalently by hand:

```sh
BUILD_VERSION=0.0.1               cargo build --release && cp target/release/web-rust dist/0.0.1/web-rust
BUILD_VERSION=0.0.2               cargo build --release && cp target/release/web-rust dist/0.0.2/web-rust
BUILD_VERSION=0.0.3 BUILD_BAD=1   cargo build --release && cp target/release/web-rust dist/0.0.3/web-rust
```

> No rebuild needed to *simulate* a bad artifact during a test: run any good
> build with `LODE_APP_BAD=1` and it exits non-zero on startup.

## Artifact & lode.toml

- **asset / selection key**: `[update].asset = "web-rust"` — the asset filename lode
  installs on this host; lode matches it by `name`. Name assets per OS/arch (e.g.
  `web-rust-linux-x86_64`) so each host's `asset` selects the right build.
- **format**: `raw`, *derived from the filename extension* (the URL *is* the single
  binary). A `tar.gz` works too — the `.tar.gz` suffix selects that format.
- **No `[runtime]`**: self-contained binary.

```toml
[command]
run     = "{entry}"   # bare `lode`   -> run the binary as the long-running server
exec    = "{entry}"   # `lode <args>` -> binary <args>  (e.g. `lode version`)
# workdir = "{dir}"   # optional; omit for the version dir (default)
```

See [`docs/lode.example.toml`](../../../docs/lode.example.toml) for a full `lode.toml`, and
[`tests/compose`](../../compose) for an end-to-end install → update → rollback demo
(signed `manifest.json` + artifacts served to two lode containers).
