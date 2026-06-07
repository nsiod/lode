# lode demo apps — Go · Bun (TS) · Rust

Three tiny HTTP services, one per language, that implement the **same**
language-agnostic lode app contract. Each demonstrates the three things an app
does under lode:

| # | Concern | What the demo shows |
|---|---|---|
| 1 | **START** (启动) | bind `$PORT` and serve — lode runs the app as its child |
| 2 | **READ** (读取变量) | read `LODE_ACTIVE_VERSION` / `LODE_DATA_DIR` / `LODE_INSTANCE` + host env (`PORT`, operator `[env]`) → `GET /env` |
| 3 | **UPGRADE** (升级) | *passive*: readiness + `SIGTERM` so lode's update/rollback is seamless · *active*: `POST /upgrade` (→ `state.target="latest"`), `POST /restart` (→ bump `restart_nonce`) |

```
examples/
├── go/    main.go · go.mod · lode.toml          (stdlib only; static binary)
├── bun/   app.ts · package.ts · lode.toml       (package.ts bundles → dist/app.js)
└── rust/  src/main.rs · Cargo.toml · lode.toml  (tiny_http + serde_json + signal-hook)
```

> Demos may use ordinary libraries — lode does not constrain your app's deps; only
> lode itself stays minimal. For a **zero-dependency, std-only** reference see
> [`../tests/apps/web-rust`](../tests/apps/web-rust) and [`../tests/apps/web-bun`](../tests/apps/web-bun).

## The contract (identical across all three)

Every demo exposes:

| Route | Method | Purpose |
|---|---|---|
| `/version` | GET | the running version (`LODE_ACTIVE_VERSION`, else the baked build version) |
| `/healthz` | GET | `200 ok` |
| `/env` | GET | JSON of the injected + passthrough env (the **READ** demo) |
| `/upgrade` | POST | write `state.target = "latest"` — ask lode to pull the newest version |
| `/restart` | POST | bump `state.restart_nonce` — ask lode to restart the current version |

And on the lifecycle side:

- **Readiness** — after the listener is bound it atomically writes
  `state.ready = $LODE_INSTANCE` into `$LODE_DATA_DIR/state.json` (so
  `readiness = "state"` commits the version). No-op when run standalone.
- **Graceful stop** — on `SIGTERM`/`SIGINT` it drains and `exit(0)` within
  `supervise.stop_timeout`, or lode `SIGKILL`s it.
- **`version` subcommand** — `<app> version` prints the version and exits (so
  `lode version` passthrough works when `exec = "{entry}"`).

`state.json` is the app ↔ lode comms file: **lode writes status, the app writes
requests** (`ready` / `target` / `restart_nonce`); the examples patch it
atomically (temp + rename) so lode never reads a half-written file. Full contract:
[`../docs/integration.md`](../docs/integration.md).

## Build

```bash
# Go — static binary (the artifact lode installs as `asset`)
cd go && CGO_ENABLED=0 go build -ldflags "-X main.buildVersion=0.0.1" -o demo-go-linux-x64 .

# Bun — bundle app.ts into ONE file: dist/app.js (the artifact, asset = "app.js")
cd bun && bun run package.ts 0.0.1        # -> dist/app.js

# Rust — static musl binary
cd rust && cargo build --release --target x86_64-unknown-linux-musl
#          -> target/x86_64-unknown-linux-musl/release/lode-demo-rust  (ship as demo-rust-linux-x64)
```

## Run it standalone (no lode) — exercises START + READ

```bash
PORT=8080 go run ./go            # or: bun run bun/app.ts   |   cargo run --manifest-path rust/Cargo.toml
APP_GREETING="hi" PORT=8080 bun run bun/app.ts   # show an operator [env]-style var

curl localhost:8080/version      # -> 0.0.0-dev   (no LODE_ACTIVE_VERSION standalone)
curl localhost:8080/env          # -> {"version":"0.0.0-dev","instance":"","dataDir":null,"port":"8080","greeting":"hi"}
curl -XPOST localhost:8080/upgrade   # -> "not running under lode (LODE_DATA_DIR unset)"  (expected standalone)
```

Standalone, the env lode would inject is empty and the state.json steps no-op —
that's the point: the same binary works with or without lode.

## Run it under lode — exercises all three

Build the artifact (above), publish it (GitHub Releases or a native manifest;
name the asset to match `[update].asset`), point the per-language `lode.toml` at
your source, and run the generic image:

```bash
docker run --rm -e PORT=8080 -p 8080:8080 \
  -v lode-demo:/srv/lode \                       # persist versions/ + runtime cache + state.json
  -v "$PWD/bun/lode.toml:/srv/lode/lode.toml:ro" \
  -e LODE_TRUSTED_KEYS="<key_id>:<base64-pubkey>" \
  docker.io/dotns/lode:latest

curl localhost:8080/env              # version = what lode installed; instance/dataDir populated
curl -XPOST localhost:8080/upgrade   # writes state.target=latest; lode pulls the newest version,
                                     # waits for readiness, and rolls back if it dies in health_grace
```

Override a default at deploy time — the host env wins over the `[env]` table:

```bash
docker run ... -e APP_GREETING="prod" ...   # /env greeting = "prod", not the lode.toml default
```

See the per-language `lode.toml` for the full config, and
[`../docs/recipes/`](../docs/recipes) for the bun/node/deno runtime-download recipes.
