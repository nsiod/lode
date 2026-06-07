//! lode demo (Rust). See ../README.md.
//!
//! Conforms to the language-agnostic lode app contract and shows the three things
//! an app does under lode:
//!   1. START   — bind $PORT and serve; lode runs this binary as its child.
//!   2. READ    — read lode-injected env (LODE_ACTIVE_VERSION / LODE_DATA_DIR /
//!                LODE_INSTANCE) + passthrough host env (PORT, operator [env]).
//!   3. UPGRADE — (a) PASSIVE: announce readiness + handle SIGTERM, so lode's
//!                update/rollback is seamless; (b) ACTIVE: POST /upgrade writes
//!                state.target="latest", POST /restart bumps state.restart_nonce.
//!
//! Standalone (no lode): LODE_DATA_DIR is unset, so the state.json steps are
//! no-ops and you still get a working server for `start` + `read`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};

// Baked at build time; lode's LODE_ACTIVE_VERSION wins at runtime so /version
// matches what lode installed. Bump the Cargo.toml version per release.
const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");

fn version() -> String {
    match env::var("LODE_ACTIVE_VERSION") {
        Ok(v) if !v.is_empty() => v,
        _ => BUILD_VERSION.to_string(),
    }
}

fn log(msg: &str) {
    println!("[demo-rust] {msg}");
}

fn main() {
    // `lode version` passthrough (exec = "{entry}").
    if let Some(arg) = env::args().nth(1) {
        if arg == "version" || arg == "--version" || arg == "-v" {
            println!("{}", version());
            return;
        }
    }

    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);
    let addr = format!("0.0.0.0:{port}");

    // START: bind.
    let server = match tiny_http::Server::http(&addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[demo-rust] bind {addr}: {e}");
            process::exit(1);
        }
    };

    // UPGRADE (passive): graceful stop. SIGTERM/SIGINT flips the flag; the accept
    // loop notices within one recv timeout and exits(0) within stop_timeout.
    let shutdown = Arc::new(AtomicBool::new(false));
    let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&shutdown));
    let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&shutdown));

    log(&format!(
        "starting version={} pid={} instance={} data_dir={} addr={addr}",
        version(),
        process::id(),
        env::var("LODE_INSTANCE").unwrap_or_else(|_| "none".into()),
        env::var("LODE_DATA_DIR").unwrap_or_else(|_| "unset".into()),
    ));

    // UPGRADE (passive): announce readiness so lode (readiness="state") commits us.
    announce_ready();

    while !shutdown.load(Ordering::SeqCst) {
        match server.recv_timeout(Duration::from_millis(200)) {
            Ok(Some(req)) => handle(req),
            Ok(None) => {} // timed out — re-check the shutdown flag
            Err(e) => {
                eprintln!("[demo-rust] recv error: {e}");
                break;
            }
        }
    }
    log("SIGTERM/SIGINT received — cleanup done, exiting 0");
    process::exit(0);
}

fn handle(req: tiny_http::Request) {
    let method = req.method().as_str().to_string();
    let path = req.url().split('?').next().unwrap_or("/").to_string();

    let (code, ctype, body) = match (method.as_str(), path.as_str()) {
        ("GET", "/healthz") => (200, "text/plain; charset=utf-8", "ok\n".to_string()),
        ("GET", "/version") => (200, "text/plain; charset=utf-8", format!("{}\n", version())),
        ("GET", "/env") => (200, "application/json", env_json()), // READ
        ("POST", "/upgrade") => match patch_state(&[("target", json!("latest"))]) {
            Ok(()) => (200, "text/plain; charset=utf-8", "requested update to latest\n".into()),
            Err(e) => (503, "text/plain; charset=utf-8", format!("{e}\n")),
        },
        ("POST", "/restart") => match bump_restart() {
            Ok(()) => (200, "text/plain; charset=utf-8", "requested restart\n".into()),
            Err(e) => (503, "text/plain; charset=utf-8", format!("{e}\n")),
        },
        _ => (404, "text/plain; charset=utf-8", "not found\n".into()),
    };

    let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], ctype.as_bytes())
        .expect("static header");
    let resp = tiny_http::Response::from_string(body)
        .with_status_code(code)
        .with_header(header);
    let _ = req.respond(resp);
}

// READ: surface the env lode injected + passthrough host/operator env.
fn env_json() -> String {
    json!({
        "version": version(),                          // LODE_ACTIVE_VERSION or baked
        "instance": env::var("LODE_INSTANCE").unwrap_or_default(), // unique id per launch
        "dataDir": env::var("LODE_DATA_DIR").ok(),     // where state.json lives
        "port": env::var("PORT").unwrap_or_else(|_| "8080".into()), // host env passthrough
        "greeting": env::var("APP_GREETING").ok(),     // operator [env] / host -e
    })
    .to_string()
}

// --- state.json: the app <-> lode comms file under $LODE_DATA_DIR -----------

fn state_path() -> Option<PathBuf> {
    match env::var("LODE_DATA_DIR") {
        Ok(d) if !d.is_empty() => Some(Path::new(&d).join("state.json")),
        _ => None, // standalone (not under lode)
    }
}

fn read_state(path: &Path) -> Map<String, Value> {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

// Merge fields into state.json (atomic temp + rename), preserving lode's fields.
fn patch_state(fields: &[(&str, Value)]) -> Result<(), String> {
    let path = state_path().ok_or("not running under lode (LODE_DATA_DIR unset)")?;
    let mut state = read_state(&path);
    for (k, v) in fields {
        state.insert((*k).to_string(), v.clone());
    }
    let body = format!(
        "{}\n",
        serde_json::to_string_pretty(&Value::Object(state)).map_err(|e| e.to_string())?
    );
    let tmp = path.with_extension(format!("tmp.{}", process::id()));
    fs::write(&tmp, &body).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })?;
    Ok(())
}

fn bump_restart() -> Result<(), String> {
    let path = state_path().ok_or("not running under lode (LODE_DATA_DIR unset)")?;
    let n = read_state(&path)
        .get("restart_nonce")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    patch_state(&[("restart_nonce", json!(n + 1))])
}

// Announce readiness so lode (readiness="state") marks this version running/good.
fn announce_ready() {
    let inst = env::var("LODE_INSTANCE").unwrap_or_default();
    match patch_state(&[("ready", json!(inst))]) {
        Ok(()) => log(&format!("ready: wrote state.ready={inst}")),
        Err(e) => log(&format!("readiness skipped: {e}")),
    }
}
