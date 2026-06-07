# Recipes: Bun / Node / Deno apps under lode

Run a JavaScript/TypeScript service under lode **without building a Docker image per
app**. You build one small base image *once*; after that, shipping or updating an app
is just a new release artifact + a `lode.toml` — no `docker build`.

| Runtime | Recipe | Launch |
|---|---|---|
| Bun  | [`bun.lode.toml`](bun.lode.toml)  | `bun run <entry>` |
| Node | [`node.lode.toml`](node.lode.toml) | `node <entry>` |
| Deno | [`deno.lode.toml`](deno.lode.toml) | `deno run -A <entry>` |

## The idea: download the runtime once, cache it

The base image carries only **lode** (and a libc). On first boot lode downloads the
runtime named in `[runtime]` into its cache at `$DATA_DIR/runtime/<name>`; on every
later boot it finds the cached binary and **reuses it — no network**. So make
`$DATA_DIR` a persistent volume and the runtime download is a one-time cost.

> To upgrade or change the runtime, delete `$DATA_DIR/runtime/<name>` (or the whole
> `runtime/` dir) — the next boot re-downloads it.

A runtime already on `PATH` always wins over the cache, so the same recipe also works
unchanged if you later decide to bake the runtime into the base image.

## Base image: use `dotns/lode` directly

The published `dotns/lode` image is built on `zzci/ubase` — a general-purpose base
(libc + shell + common tools), **not** a minimal/static one — with `lode` on `PATH`
at `/usr/bin/lode`. So the dynamic runtimes load there, and you can run your app on
the image as-is with **no Dockerfile at all** — just mount a config and a cache volume:

```bash
docker run --rm -e PORT=8080 -p 8080:8080 \
  -v lode-myapp:/srv/lode \                       # persists runtime/ cache + versions/
  -v "$PWD/bun.lode.toml:/srv/lode/lode.toml:ro" \
  -e LODE_TRUSTED_KEYS="<key_id>:<base64-pubkey>" \
  docker.io/dotns/lode:latest
```

Build your own image only to bake in extra tools, or to bake in the runtime instead
of downloading it (then drop the `[runtime]` block — a runtime already on `PATH` wins
over the cache):

```dockerfile
FROM oven/bun:1                                       # runtime already on PATH
COPY --from=docker.io/dotns/lode:latest /usr/bin/lode /usr/bin/lode
ENTRYPOINT ["/usr/bin/lode"]
```

## The `[runtime]` download

lode downloads the runtime named in `[runtime]`, lands the binary at `runtime/<name>`,
and reuses it from there on every later boot. It handles both flat and nested
archives: after extracting it hoists the named binary to the root when it isn't
already there, so bun's `bun-linux-x64/bun`, node's `node-vX/bin/node`, and deno's
flat `deno` all work — as do `raw` / `.gz` single-file downloads. The archive must
contain a file named exactly `<name>` (the `[runtime].runtime` value) or the install
fails with a clear error.

Runtime downloads are **UNVERIFIED** — no sha256/sig is checked (unlike app
artifacts). Pin a version, host on a trusted origin, and if the host needs your auth
headers add it to `[http].credential_hosts`.

### Pin the runtime version

Set `[runtime].version` and lode probes the runtime it's about to use (on PATH, in
the cache, or freshly downloaded) by running it with `version_check` (default
`--version`) and requiring the output to **contain** that string:

```toml
[runtime]
runtime       = "bun"
download      = "https://github.com/oven-sh/bun/releases/download/bun-v1.1.38/bun-linux-x64.zip"
version       = "1.1.38"      # require `bun --version` to report this
# version_check = "--version" # default; e.g. deno also uses --version, node prints "v22…"
```

- A cached or PATH runtime of the **wrong** version is bypassed and the configured
  `download` is fetched instead — so bumping `version` (and `download`) rolls the
  runtime forward with no manual `rm $DATA_DIR/runtime/<name>`.
- A freshly downloaded runtime whose version still doesn't match is a **hard error**
  (the URL served the wrong version, or `version`/`version_check` is misconfigured).
- Substring match, so `1.1.38` matches bun's `1.1.38`, node's `v22.11.0` matches
  `22.11.0`, and deno's `deno 2.1.4 (…)` matches `2.1.4`.

---

## Packaging the app (no install step)

lode runs `[command].run` directly — **there is no `npm install` / `bun install`**.
So make the artifact self-sufficient:

- **Bundle to a single file** (recommended): `bun build app.ts --outfile app.js`,
  `esbuild --bundle`, or `ncc`. Ship that one file (`asset = "app.js"`); its `format`
  is `raw` and the entry defaults to that file.
- **Or ship a tarball** with `node_modules` vendored: `asset = "myapp-1.0.0.tar.gz"`,
  and set `entry = "dist/server.js"` (or place the entry at the archive root as `{app}`).
- **Deno** can usually skip bundling — it resolves TS and `npm:` / URL imports at run
  time (`deno vendor` or the cache for reproducibility).

A worked single-file Bun example (artifact + build script + the app contract —
`SIGTERM` handling and the `state.ready` readiness write) lives in
[`../../tests/apps/web-bun`](../../tests/apps/web-bun).

## The app contract (one-time, per app)

For safe zero-rollback updates with `readiness = "state"`, the app must, once it can
serve, atomically write `state.ready = $LODE_INSTANCE` into
`$LODE_DATA_DIR/state.json`, and handle `SIGTERM` by draining and `exit(0)` within
`stop_timeout`. lode injects `LODE_ACTIVE_VERSION`, `LODE_DATA_DIR`, `LODE_INSTANCE`;
host env (e.g. `PORT`) passes through. Full details: [integration §2](../integration.md).
No readiness handshake? Set `readiness = "none"` and lean on `health_grace`.

## Publish & update

Use GitHub Releases (`github = "owner/repo"`) or a native manifest. Name each asset
so `[update].asset` matches it exactly, and sign it (`lode-cli sign`, uploaded as the
GitHub asset `label`, or the `sig` field in a native manifest). With `policy =
"auto"`, pushing a new release rolls every running instance forward — and a version
that dies within `health_grace` is auto-reverted. See [integration §3](../integration.md)
and [source-adapters.md](../source-adapters.md).
