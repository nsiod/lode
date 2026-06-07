// lode demo (Bun + TypeScript). See ../README.md.
//
// Conforms to the language-agnostic lode app contract and shows the three things
// an app does under lode:
//   1. START   — Bun.serve on $PORT; lode runs `bun run app.js` as its child.
//   2. READ    — read lode-injected env (LODE_ACTIVE_VERSION / LODE_DATA_DIR /
//                LODE_INSTANCE) + passthrough host env (PORT, operator [env]).
//   3. UPGRADE — (a) PASSIVE: announce readiness + handle SIGTERM, so lode's
//                update/rollback is seamless; (b) ACTIVE: POST /upgrade writes
//                state.target = "latest", POST /restart bumps state.restart_nonce.
//
// Bundle to ONE file with `bun run package.ts` (-> dist/app.js); that single file
// is the artifact lode installs (asset = "app.js"), run with `run = "bun run"`.

import { existsSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";

// BUILD_VERSION is inlined by package.ts (`--define`); absent when run unbundled.
// `typeof` on an undeclared name is safe (returns "undefined"), so this never throws.
declare const BUILD_VERSION: string;
const baked = typeof BUILD_VERSION === "string" ? BUILD_VERSION : "0.0.0-dev";

// lode's LODE_ACTIVE_VERSION wins so /version matches what lode installed.
const version = Bun.env.LODE_ACTIVE_VERSION || baked;
const port = Number(Bun.env.PORT ?? "8080");
const dataDir = Bun.env.LODE_DATA_DIR;
const instance = Bun.env.LODE_INSTANCE ?? "";
const statePath = dataDir ? join(dataDir, "state.json") : null;

const log = (m: string): void => console.log(`[demo-bun] ${m}`);

// `lode version` passthrough (exec = "bun" → `bun app.js version`).
if (["version", "--version", "-v"].includes(Bun.argv[2] ?? "")) {
  console.log(version);
  process.exit(0);
}

const text = (body: string, status = 200): Response =>
  new Response(body, { status, headers: { "content-type": "text/plain; charset=utf-8" } });
const json = (body: unknown, status = 200): Response =>
  new Response(JSON.stringify(body, null, 2), { status, headers: { "content-type": "application/json" } });

const server = Bun.serve({
  port,
  fetch(req): Response {
    const { pathname } = new URL(req.url);
    switch (pathname) {
      case "/healthz":
        return text("ok\n");
      case "/version":
        return text(`${version}\n`);
      case "/env": // READ
        return json({
          version, // LODE_ACTIVE_VERSION or baked
          instance, // unique id per launch
          dataDir: dataDir ?? null, // where state.json lives
          port, // host env passthrough
          greeting: Bun.env.APP_GREETING ?? null, // operator [env] / host -e
        });
      case "/upgrade": // UPGRADE (active): ask lode to pull latest
        return patchState({ target: "latest" })
          ? text("requested update to latest\n")
          : text("not running under lode (LODE_DATA_DIR unset)\n", 503);
      case "/restart": // UPGRADE (active): ask lode to restart this version
        return bumpRestart()
          ? text("requested restart\n")
          : text("not running under lode (LODE_DATA_DIR unset)\n", 503);
      default:
        return text("not found\n", 404);
    }
  },
});

log(
  `starting version=${version} pid=${process.pid} instance=${instance || "none"} ` +
    `data_dir=${dataDir ?? "unset"} addr=0.0.0.0:${server.port}`,
);

// UPGRADE (passive): graceful stop — drain and exit(0) within supervise.stop_timeout.
let stopping = false;
const shutdown = (sig: string): void => {
  if (stopping) return;
  stopping = true;
  log(`${sig} received — shutting down`);
  server.stop(true);
  log("cleanup done, exiting 0");
  process.exit(0);
};
process.on("SIGTERM", () => shutdown("SIGTERM"));
process.on("SIGINT", () => shutdown("SIGINT"));

// UPGRADE (passive): announce readiness so lode (readiness="state") commits us.
announceReady();

// --- state.json: the app <-> lode comms file under $LODE_DATA_DIR -----------

function readState(): Record<string, unknown> {
  if (!statePath || !existsSync(statePath)) return {};
  try {
    return JSON.parse(readFileSync(statePath, "utf8")) as Record<string, unknown>;
  } catch {
    return {}; // tolerate empty/corrupt
  }
}

// Merge fields into state.json (atomic temp + rename), preserving lode's fields.
function patchState(fields: Record<string, unknown>): boolean {
  if (!statePath) return false;
  const state = { ...readState(), ...fields };
  const tmp = `${statePath}.tmp.${process.pid}`;
  try {
    writeFileSync(tmp, `${JSON.stringify(state, null, 2)}\n`);
    renameSync(tmp, statePath); // atomic replace
    return true;
  } catch (e) {
    try {
      rmSync(tmp);
    } catch {
      /* best-effort cleanup */
    }
    log(`state write failed: ${String(e)}`);
    return false;
  }
}

function bumpRestart(): boolean {
  const cur = readState().restart_nonce;
  const n = typeof cur === "number" ? cur : 0;
  return patchState({ restart_nonce: n + 1 });
}

function announceReady(): void {
  if (!statePath) {
    log("readiness skipped (standalone)");
    return;
  }
  if (patchState({ ready: instance })) log(`ready: wrote state.ready=${instance} -> ${statePath}`);
}
