# web-bun — lode test app (Bun + TypeScript HTTP server)

A minimal **Bun.serve** HTTP service that implements the language-agnostic
**lode app contract** — the same contract as the sibling
[`tests/apps/web-rust`](../web-rust). Uses **only Bun
built-ins** (`Bun.serve`, `node:fs`); **zero external dependencies**, so there
is no `package.json` and no `node_modules`. lode runs it under the `bun` runtime
(`[runtime]`), downloading bun if it is not on `PATH`.

## Contract behaviour

| Aspect | Behaviour |
|---|---|
| HTTP | binds `0.0.0.0:$PORT` (env `PORT`, default `8080`) |
| `GET /version` | the app's own version, plain text (e.g. `0.0.1`) |
| `GET /healthz` | `200 ok` |
| Version source | `LODE_ACTIVE_VERSION` (injected by lode) wins; else baked `BUILD_VERSION` |
| Graceful stop | `SIGTERM`/`SIGINT` → `server.stop(true)` + `exit(0)` sub-second — well inside `supervise.stop_timeout` |
| Readiness | when `LODE_DATA_DIR` is set, atomically writes `state.json` field `ready = $LODE_INSTANCE` (temp + rename, preserving lode's fields) → makes `readiness = "state"` work |
| Bad mode | baked `BUILD_BAD=1` **or** runtime `LODE_APP_BAD=1` → `exit(1)` immediately on startup (crash within `health_grace`) |

## Run standalone

```sh
PORT=8080 bun app.ts
curl -s localhost:8080/version   # -> 0.0.0-dev
curl -s localhost:8080/healthz   # -> ok
# Ctrl-C (SIGINT) -> "cleanup done, exiting 0", exits 0

# print version and exit (also `lode version` passthrough)
bun app.ts version

# exercise the readiness write (writes ./state.json field "ready")
LODE_DATA_DIR=. LODE_INSTANCE=demo-1 bun app.ts
```

## Producing v0.0.1 / v0.0.2 / a crashing v0.0.3

The version is **baked at package time** into a copy of `app.ts` (the
`BUILD_VERSION` line), mirroring the D1 `app.sh` build. A baked `BUILD_BAD=1`
makes the script crash on startup — that is how a real "bad v0.0.3" rollback
artifact is produced.

Use `build.sh` (rewrites the two `BUILD_*` lines into a fresh `app.ts`):

```sh
sh build.sh 0.0.1 dist/0.0.1/app.ts            # good v0.0.1
sh build.sh 0.0.2 dist/0.0.2/app.ts            # good v0.0.2
sh build.sh 0.0.3 dist/0.0.3/app.ts --bad      # crashing v0.0.3 (rollback test)
sha256sum dist/*/app.ts                          # digests for manifest.json
```

> No rebuild needed to *simulate* a bad artifact during a test: run any good
> build with `LODE_APP_BAD=1` and it exits non-zero on startup.

### Optional: single-file bundle

If you prefer one self-contained `.js` artifact over the raw `.ts`:

```sh
sh build.sh 0.0.1 dist/0.0.1/app.ts --bundle    # also emits dist/0.0.1/app.js
# or directly:
bun build app.ts --target bun --outfile app.js
```

Then use `entry = "app.js"` and `run = "bun run"` (so `bun run app.js`).

## Artifact & lode.toml

- **asset / selection key**: `[update].asset = "app.ts"` — the asset filename lode
  installs on this host (or `app.js` if bundled); lode matches it by `name`.
- **format**: `raw`, *derived from the filename extension* (the URL *is* the
  script). The advisory manifest `entry` defaults to that same filename.
- **`[runtime]` required**: lode finds `bun` on `PATH`, else downloads it.

```toml
[command]
run     = "bun"       # bare `lode`   -> bun <entry>        (long-running server)
exec    = "bun"       # `lode <args>` -> bun <args>
# workdir = "{dir}"   # optional; omit for the version dir (default)

[runtime]
runtime  = "bun"
download = "https://example.com/bun-linux-x64.zip"   # used only if bun is absent from PATH
# format = "zip"   # entry = "bun"   # sha256 = "<hex>"
```

> `run = "bun"` runs `bun <entry>`. `run = "bun run"` (→ `bun run <entry>`) is
> equivalent for a script file; use the latter for the bundled `app.js`.

See [`docs/lode.example.toml`](../../../docs/lode.example.toml) for a full `lode.toml`, and
[`tests/compose`](../../compose) for an end-to-end install → update → rollback demo
(signed `manifest.json` + artifacts served to two lode containers).
