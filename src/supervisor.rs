//! Supervised-service runtime + CLI passthrough (design §5/§8/§9).
//!
//! [`serve`] is the bare-`lode` path: acquire the single-instance lock, clean up
//! orphans/garbage from a previous run, decide which version to launch (bootstrap
//! the latest only when nothing is installed), then spawn the app as a child and
//! supervise it. By default (`supervise.restart=off`) lode *mirrors* the child:
//! when the child exits on its own, lode exits with its code and lets the
//! orchestrator decide whether to restart. `on-failure`/`always` opt into bounded
//! exponential-backoff restarts. lode itself only ever relaunches the child as
//! part of a lode-initiated transition — an update, a single-strike rollback, or
//! an explicit restart. It also does signal forwarding, graceful stop, and (as
//! PID 1) child-subreaping so re-parented grandchildren never become zombies. On
//! the same short-interval tick the loop also drives the C2 update
//! machinery: it polls `state.json`'s mtime for app-written `target` /
//! `restart_nonce` requests (§7), runs the `[update].policy` check (§5), and — when
//! a target is applied — performs the stop-start hot-update with the readiness/stop
//! handshake (§8) and automatic rollback to `last_good` on failure.
//!
//! [`exec_passthrough`] is the `lode <args>` path: validate the version (bootstrap
//! if none), prepare the same argv/env/runtime, then `exec`-replace into the app —
//! no lock, no supervision, no polling. The replacement uses the safe
//! [`std::os::unix::process::CommandExt::exec`], so the crate keeps
//! `#![forbid(unsafe_code)]`.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::ffi::OsStr;
use std::os::raw::c_int;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant, SystemTime};

use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use signal_hook::iterator::Signals;

use crate::config::{Config, Policy, Readiness, RestartPolicy};
use crate::error::{Error, Result};
use crate::state::{self, HistoryEntry, HistoryResult, State, Status};
use crate::{download, idval, install, manifest};

/// Supervise-loop tick. Bounds signal-forwarding and child-exit latency while
/// leaving headroom for the C2 state-poll / update-observation on the same cadence.
const POLL_TICK: Duration = Duration::from_millis(200);

/// Poll granularity while waiting for a child to exit during a graceful stop.
const STOP_POLL: Duration = Duration::from_millis(50);

/// How often the supervisor re-checks `state.json`'s mtime for app-written
/// `target` / `restart_nonce` requests (design §7: notification via mtime poll).
const STATE_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Cap on the persisted rollout `history` so `state.json` cannot grow unbounded.
const HISTORY_CAP: usize = 20;

// --- public entry points ---

/// Run the app as a supervised service (bare `lode`). Returns the child's exit
/// code on graceful shutdown (or when the restart limit is hit).
pub(crate) fn serve(cfg: &Config) -> Result<ExitCode> {
    set_subreaper();
    let _lock = lock_acquire(cfg)?;
    startup_cleanup(cfg)?;

    let target = resolve_target(cfg)?;
    install::switch_current(cfg, &target.version)?;
    let runtime_dir = ensure_runtime(cfg)?;

    let mut supervisor = Supervisor::new(cfg, target, runtime_dir);
    supervisor.run()
}

/// CLI passthrough (`lode <args>`): validate the version, then `exec`-replace into
/// `[command].exec` + `args`. On success it never returns (the process image is
/// replaced); any failure surfaces as [`Error::Process`].
pub(crate) fn exec_passthrough(cfg: &Config, args: &[String]) -> Result<Infallible> {
    let target = resolve_target(cfg)?;
    let runtime_dir = ensure_runtime(cfg)?;
    let instance = format!("{}-exec", std::process::id());

    let entry = target.entry.to_string_lossy();
    let dir = target.dir.to_string_lossy();
    let command_line = build_exec_argv(&cfg.command.exec, &entry, &dir, args)?;
    let env = child_env(
        std::env::vars(),
        &cfg.env,
        &target.version,
        &cfg.global.data_dir,
        &instance,
        runtime_dir.as_deref(),
    );
    let workdir = PathBuf::from(expand_token(&cfg.command.workdir, &entry, &dir));

    let (program, rest) = command_line
        .split_first()
        .ok_or_else(|| Error::Process("empty exec command".to_owned()))?;
    let mut cmd = Command::new(program);
    cmd.args(rest).current_dir(&workdir).env_clear();
    cmd.envs(env.iter().map(|(k, v)| (k, v)));
    // `exec` only returns on failure; on success this process is replaced.
    let err = cmd.exec();
    Err(Error::Process(format!("exec {program}: {err}")))
}

// --- version resolution (shared by serve + exec) ---

/// A resolved, installed version and the paths needed to launch it.
struct Target {
    version: String,
    dir: PathBuf,
    entry: PathBuf,
}

/// Decide which version to run and locate its entry. Bootstraps the latest only
/// when nothing usable is installed (design §4: never auto-jump versions).
fn resolve_target(cfg: &Config) -> Result<Target> {
    let version = determine_version(cfg)?;
    locate(cfg, &version)
}

/// Build the launch [`Target`] for an already-installed `version` by reading its
/// `.lode.json` marker (design §15). Errors if the version is not installed or its
/// entry is missing. Used by `serve` and by the C2 hot-update apply path.
fn locate(cfg: &Config, version: &str) -> Result<Target> {
    // Defensive: every caller already validated `version`, but it keys
    // `versions/<version>` here too — re-check before the join.
    idval::validate_id("version", version)?;
    let m = install::marker(cfg, version)?;
    let dir = cfg.global.data_dir.join("versions").join(version);
    let entry = dir.join(&m.entry);
    if !entry.is_file() {
        return Err(Error::Process(format!(
            "entry {:?} for version {version} is missing",
            m.entry
        )));
    }
    Ok(Target {
        version: version.to_owned(),
        dir,
        entry,
    })
}

/// Pick the version to launch: an operator `pin` wins (installing it if needed);
/// otherwise the recorded `current` if still installed; otherwise the newest
/// locally installed version; otherwise bootstrap the channel latest.
fn determine_version(cfg: &Config) -> Result<String> {
    if let Some(pin) = cfg.update.pin.as_deref() {
        // A configured pin keys `versions/<pin>`; reject traversal before it is
        // used to probe the installed set or bootstrap.
        idval::validate_id("version", pin)?;
        if version_installed(cfg, pin) {
            return Ok(pin.to_owned());
        }
        return bootstrap(cfg, Some(pin));
    }

    let state_path = cfg.global.data_dir.join("state.json");
    if let Some(st) = state::read(&state_path)?
        && let Some(cur) = st.current.as_deref()
        && version_installed(cfg, cur)
    {
        return Ok(cur.to_owned());
    }

    if let Some(v) = newest_installed(cfg)? {
        return Ok(v);
    }

    bootstrap(cfg, None)
}

/// A version counts as installed once its `.lode.json` marker is present (install
/// writes it last, atomically).
fn version_installed(cfg: &Config, version: &str) -> bool {
    cfg.global
        .data_dir
        .join("versions")
        .join(version)
        .join(".lode.json")
        .is_file()
}

/// The newest installed version (semver-descending), or `None` if none.
fn newest_installed(cfg: &Config) -> Result<Option<String>> {
    let versions_dir = cfg.global.data_dir.join("versions");
    let entries = match std::fs::read_dir(&versions_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut installed = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str()
            && version_installed(cfg, name)
        {
            installed.push(name.to_owned());
        }
    }
    installed.sort_by(|a, b| cmp_desc(a, b));
    Ok(installed.into_iter().next())
}

/// Newest-first version order (valid semver by precedence ahead of non-semver).
fn cmp_desc(a: &str, b: &str) -> std::cmp::Ordering {
    match (semver::Version::parse(a), semver::Version::parse(b)) {
        (Ok(x), Ok(y)) => y.cmp(&x),
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        (Err(_), Err(_)) => b.cmp(a),
    }
}

/// Bootstrap install: fetch the manifest, resolve a target (`requested` > pin >
/// channel latest), download + verify + install it, and activate it (design §5).
fn bootstrap(cfg: &Config, requested: Option<&str>) -> Result<String> {
    install::gc(cfg)?;
    let manifest = manifest::fetch(cfg)?;
    if manifest.name != cfg.global.app {
        return Err(Error::Manifest(format!(
            "manifest name {:?} does not match configured app {:?}",
            manifest.name, cfg.global.app
        )));
    }
    // Verify the catalog's publisher signature before trusting any of its pointers.
    install::verify_manifest_identity(cfg, &manifest)?;
    let target = manifest::resolve_target(
        &manifest,
        &cfg.update.channel,
        cfg.update.pin.as_deref(),
        requested,
    )?;
    let entry = manifest::version_entry(&manifest, &target)?;
    let asset = manifest::select_asset(entry, required_asset(cfg)?)?;
    let (temp, sha256) =
        download::fetch_artifact(cfg, asset, &target, &manifest::allowed_hosts(cfg))?;
    install::install(cfg, &target, asset, &temp, &sha256)?;
    install::switch_current(cfg, &target)?;
    tracing::info!(version = target, "bootstrapped initial version");
    Ok(target)
}

/// The operator-selected asset filename (`[update].asset`) — the source-agnostic
/// selection key for both adapters. There is no platform fallback, so this errors
/// clearly when unset rather than guessing an asset.
fn required_asset(cfg: &Config) -> Result<&str> {
    cfg.update.asset.as_deref().ok_or_else(|| {
        Error::Config(
            "no [update].asset configured — set the asset filename to install (source-adapters §3)"
                .to_owned(),
        )
    })
}

// --- runtime resolution ([runtime], design §4) ---

/// What to do about a configured `[runtime]` before launching the child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimePlan {
    /// No `[runtime]` configured (self-contained binary).
    NotNeeded,
    /// The runtime is already on PATH — nothing to download.
    AlreadyPresent,
    /// A prior download left the runtime in `$DATA_DIR/runtime/` — reuse it (no
    /// network). When `$DATA_DIR` is a persistent volume this makes the download a
    /// one-time cost across restarts.
    Cached,
    /// The runtime is missing — download it and prepend its dir to the child PATH.
    Fetch,
}

/// Decide what to do about the runtime. Precedence: a runtime already on PATH wins
/// (system runtime), then a cached download is reused, then a fresh download. Errors
/// only when the runtime is named but absent from PATH and cache with no `download`
/// URL configured.
fn plan_runtime(
    runtime: Option<&str>,
    download: Option<&str>,
    present: bool,
    cached: bool,
) -> Result<RuntimePlan> {
    match runtime {
        None => Ok(RuntimePlan::NotNeeded),
        Some(_) if present => Ok(RuntimePlan::AlreadyPresent),
        Some(_) if cached => Ok(RuntimePlan::Cached),
        Some(_) if download.is_some() => Ok(RuntimePlan::Fetch),
        Some(name) => Err(Error::Process(format!(
            "runtime {name:?} not found on PATH or in cache, and no [runtime].download configured"
        ))),
    }
}

/// Ensure a configured runtime is available for the child, downloading it into
/// `$DATA_DIR/runtime/` when absent from PATH and not already cached there. Returns
/// the directory to prepend to the child's PATH, or `None` when no runtime download
/// is needed. A previously downloaded runtime (a `runtime/<name>` executable from an
/// earlier launch) is reused without touching the network, so a persistent
/// `$DATA_DIR` makes the download a one-time cost; delete `runtime/<name>` to force a
/// re-download (e.g. to change the runtime version).
fn ensure_runtime(cfg: &Config) -> Result<Option<PathBuf>> {
    let runtime = cfg.runtime.runtime.as_deref();
    let download_url = cfg.runtime.download.as_deref();
    let expected = cfg.runtime.version.as_deref();
    let probe_args = runtime_probe_args(cfg.runtime.version_check.as_deref());
    let path_var = std::env::var("PATH").unwrap_or_default();
    let runtime_dir = cfg.global.data_dir.join("runtime");
    // place_runtime lands the binary at `runtime/<name>`; the same path is the cache
    // key on the next launch.
    let cached_bin = runtime.map(|name| runtime_dir.join(name));

    // Version-gate PATH and cache: a usable runtime must also report the expected
    // version (when one is configured). A wrong-version PATH/cache entry is treated
    // as unusable so we fall through to a fresh download that pins the right version.
    let present_ok = runtime.is_some_and(|name| {
        on_path(name, &path_var)
            && expected.is_none_or(|want| {
                let ok = runtime_version_ok(OsStr::new(name), &probe_args, want);
                if !ok {
                    tracing::warn!(
                        runtime = name,
                        want,
                        "PATH runtime version mismatch; trying cache/download"
                    );
                }
                ok
            })
    });
    let cached_ok = cached_bin.as_deref().is_some_and(|bin| {
        is_executable_file(bin)
            && expected.is_none_or(|want| {
                let ok = runtime_version_ok(bin.as_os_str(), &probe_args, want);
                if !ok {
                    tracing::info!(want, "cached runtime version mismatch; re-downloading");
                }
                ok
            })
    });

    match plan_runtime(runtime, download_url, present_ok, cached_ok)? {
        RuntimePlan::NotNeeded | RuntimePlan::AlreadyPresent => Ok(None),
        RuntimePlan::Cached => {
            tracing::info!(
                runtime = runtime.unwrap_or_default(),
                dir = %runtime_dir.display(),
                "runtime served from cache; skipping download"
            );
            Ok(Some(runtime_dir))
        }
        RuntimePlan::Fetch => {
            // Both are `Some` here (guaranteed by `plan_runtime`).
            let name = runtime.unwrap_or_default();
            let url = download_url.unwrap_or_default();
            let format = infer_format(url);
            tracing::info!(
                runtime = name,
                format,
                "runtime missing from PATH and cache; downloading"
            );
            let asset = runtime_asset(url, name);
            // The runtime download has no manifest origin to be same-origin with,
            // so credentials ride it only when its host is explicitly allowlisted
            // via `[http].credential_hosts`; otherwise they are dropped.
            let (temp, _sha) =
                download::fetch_artifact(cfg, &asset, "runtime", &cfg.http.credential_hosts)?;
            install::place_runtime(&runtime_dir, &temp, format, name)?;
            let _ = std::fs::remove_file(&temp);
            if let Some(want) = expected {
                verify_runtime_version(&runtime_dir.join(name), &probe_args, want)?;
            }
            Ok(Some(runtime_dir))
        }
    }
}

/// Args that make a runtime print its version, from `[runtime].version_check`
/// (whitespace-split), defaulting to `--version`.
fn runtime_probe_args(version_check: Option<&str>) -> Vec<String> {
    match version_check {
        Some(s) if !s.trim().is_empty() => s.split_whitespace().map(str::to_owned).collect(),
        _ => vec!["--version".to_owned()],
    }
}

/// Run `program <args>` and return its combined stdout+stderr, or `None` if the
/// program can't be executed at all (spawn error). Runtimes print their version to
/// either stream, so both are captured.
fn probe_output(program: &OsStr, args: &[String]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Some(text)
}

/// Does `program`'s version-probe output contain `expected`? A probe that fails to
/// execute (wrong arch, missing lib, bad path) counts as not-OK.
fn runtime_version_ok(program: &OsStr, args: &[String], expected: &str) -> bool {
    probe_output(program, args).is_some_and(|out| out.contains(expected))
}

/// Confirm a freshly downloaded runtime reports `expected`; a mismatch (or a probe
/// that won't run) is a hard error — the configured `download` served the wrong
/// version, or `version`/`version_check` is misconfigured.
fn verify_runtime_version(bin: &Path, args: &[String], expected: &str) -> Result<()> {
    match probe_output(bin.as_os_str(), args) {
        Some(out) if out.contains(expected) => {
            tracing::info!(version = expected, "downloaded runtime version verified");
            Ok(())
        }
        Some(out) => Err(Error::Process(format!(
            "downloaded runtime version mismatch: expected {expected:?}, but `{bin} {probe}` reported {got:?}",
            bin = bin.display(),
            probe = args.join(" "),
            got = out.lines().next().unwrap_or("").trim(),
        ))),
        None => Err(Error::Process(format!(
            "could not run `{bin} {probe}` to verify the downloaded runtime version",
            bin = bin.display(),
            probe = args.join(" "),
        ))),
    }
}

/// A synthetic [`manifest::Asset`] for a runtime download, so the runtime reuses
/// the audited [`download`] path. No `sha256`/`size` (the `[runtime]` config carries
/// none); `entry` is the runtime binary name for single-file formats. The format is
/// determined separately by the caller (from the URL, see [`infer_format`]) rather
/// than from `name`, since a runtime binary name carries no packaging suffix.
fn runtime_asset(url: &str, name: &str) -> manifest::Asset {
    manifest::Asset {
        name: name.to_owned(),
        url: url.to_owned(),
        sha256: String::new(),
        sig: None,
        key_id: None,
        entry: Some(name.to_owned()),
        size: None,
        auth: true,
    }
}

/// Is `name` an executable file in any `path_var` (`:`-separated) directory?
fn on_path(name: &str, path_var: &str) -> bool {
    path_var
        .split(':')
        .filter(|dir| !dir.is_empty())
        .any(|dir| is_executable_file(&Path::new(dir).join(name)))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Infer a packaging format from a URL suffix (query/fragment stripped). The
/// suffix checks are case-insensitive (the path is lowercased first), so the
/// extension-comparison lint does not apply.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn infer_format(url: &str) -> &'static str {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    if path.ends_with(".tar.gz") || path.ends_with(".tgz") {
        "tar.gz"
    } else if path.ends_with(".zip") {
        "zip"
    } else if path.ends_with(".gz") {
        "gz"
    } else {
        "raw"
    }
}

// --- argv + environment ---

/// Expand `{entry}`/`{dir}` placeholders in one token. The braces are literal
/// placeholders to substitute, not Rust format args.
#[allow(clippy::literal_string_with_formatting_args)]
fn expand_token(token: &str, entry: &str, dir: &str) -> String {
    token.replace("{entry}", entry).replace("{dir}", dir)
}

/// Build the bare-run argv from `[command].run`: split on whitespace, expand
/// placeholders, and auto-append the entry when no `{entry}` token is present
/// (design §4). Never empty.
fn build_run_argv(command: &str, entry: &str, dir: &str) -> Result<Vec<String>> {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let has_entry = tokens.iter().any(|t| t.contains("{entry}"));
    let mut argv: Vec<String> = tokens.iter().map(|t| expand_token(t, entry, dir)).collect();
    if !has_entry {
        argv.push(entry.to_owned());
    }
    if argv.is_empty() {
        return Err(Error::Process("empty run command".to_owned()));
    }
    Ok(argv)
}

/// Build the passthrough argv from `[command].exec` + `args`: split on whitespace,
/// expand placeholders, then append the user args verbatim (no entry auto-append).
fn build_exec_argv(command: &str, entry: &str, dir: &str, args: &[String]) -> Result<Vec<String>> {
    let mut parts: Vec<String> = command
        .split_whitespace()
        .map(|t| expand_token(t, entry, dir))
        .collect();
    parts.extend(args.iter().cloned());
    if parts.is_empty() {
        return Err(Error::Process("empty exec command".to_owned()));
    }
    Ok(parts)
}

/// Build the child environment: inherit the host env minus all config `LODE_*`
/// vars, apply the operator's `[env]` overrides, optionally prepend the runtime dir
/// to PATH, then inject the read-only introspection vars (design §10). Precedence
/// (low → high): operator `[env]` defaults < inherited host env < runtime
/// PATH-prepend < lode's `LODE_*` vars.
fn child_env(
    host: impl IntoIterator<Item = (String, String)>,
    defined: &BTreeMap<String, String>,
    version: &str,
    data_dir: &Path,
    instance: &str,
    runtime_dir: Option<&Path>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = host
        .into_iter()
        .filter(|(key, _)| !key.starts_with("LODE_"))
        .collect();
    // The `[env]` table is DEFAULTS: applied only for keys the inherited host env
    // doesn't already provide, so a per-deploy `-e KEY=…` (any inherited env var)
    // overrides `[env]`.
    for (key, value) in defined {
        if !env.iter().any(|(k, _)| k == key) {
            env.push((key.to_owned(), value.to_owned()));
        }
    }
    if let Some(dir) = runtime_dir {
        prepend_path(&mut env, dir);
    }
    // lode's introspection vars always win — set (not push) so a `[env]` entry of
    // the same name can't leave a duplicate behind.
    set_env(&mut env, "LODE_ACTIVE_VERSION", version);
    set_env(&mut env, "LODE_DATA_DIR", &data_dir.display().to_string());
    set_env(&mut env, "LODE_INSTANCE", instance);
    env
}

/// Set `key` to `value` in `env`, replacing an existing entry or appending a new
/// one (so the result never holds a duplicate key).
fn set_env(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some((_, slot)) = env.iter_mut().find(|(k, _)| k == key) {
        value.clone_into(slot);
    } else {
        env.push((key.to_owned(), value.to_owned()));
    }
}

/// Prepend `dir` to the PATH entry in `env` (or create PATH if absent).
fn prepend_path(env: &mut Vec<(String, String)>, dir: &Path) {
    let dir = dir.display().to_string();
    if let Some((_, value)) = env.iter_mut().find(|(key, _)| key == "PATH") {
        *value = format!("{dir}:{value}");
    } else {
        env.push(("PATH".to_owned(), dir));
    }
}

/// The `LODE_READINESS` value injected for the child so it knows whether the
/// `state.ready` handshake is expected (design §8).
const fn readiness_label(mode: Readiness) -> &'static str {
    match mode {
        Readiness::None => "none",
        Readiness::State => "state",
    }
}

// --- signals ---

/// What an incoming signal means for the supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// Graceful shutdown: stop the child, release the lock, exit with its code.
    Terminate,
    /// Graceful restart: stop and re-spawn the child.
    Restart,
    /// Forward verbatim to the child.
    Forward,
    /// No supervisor action.
    Ignore,
}

/// Map a received signal to a supervisor action. The configured restart signal
/// wins (and is never forwarded); the termination set is handled next; remaining
/// members of the forward set are forwarded; everything else is ignored.
fn classify(sig: Signal, restart: Option<Signal>, forward: &[Signal]) -> Action {
    if restart == Some(sig) {
        Action::Restart
    } else if matches!(sig, Signal::SIGTERM | Signal::SIGINT | Signal::SIGQUIT) {
        Action::Terminate
    } else if forward.contains(&sig) {
        Action::Forward
    } else {
        Action::Ignore
    }
}

/// lode's standard forward set when `[signals].forward` is unset (design §8).
fn default_forward() -> Vec<Signal> {
    vec![
        Signal::SIGHUP,
        Signal::SIGUSR1,
        Signal::SIGUSR2,
        Signal::SIGWINCH,
        Signal::SIGCONT,
        Signal::SIGTSTP,
    ]
}

/// Resolve the configured forward set (or the standard set when empty), dropping
/// unparsable names with a warning.
fn forward_signals(configured: &[String]) -> Vec<Signal> {
    if configured.is_empty() {
        return default_forward();
    }
    configured
        .iter()
        .filter_map(|name| {
            let parsed = parse_signal(name);
            if parsed.is_none() {
                tracing::warn!(signal = name.as_str(), "ignoring unknown forward signal");
            }
            parsed
        })
        .collect()
}

/// Parse a signal name, accepting both `SIGHUP` and `HUP` (any case).
fn parse_signal(name: &str) -> Option<Signal> {
    let upper = name.trim().to_ascii_uppercase();
    let canonical = if upper.starts_with("SIG") {
        upper
    } else {
        format!("SIG{upper}")
    };
    canonical.parse().ok()
}

/// signal-hook refuses to register these (they cannot be caught or trigger UB).
const fn is_forbidden(sig: Signal) -> bool {
    matches!(
        sig,
        Signal::SIGKILL | Signal::SIGSTOP | Signal::SIGILL | Signal::SIGFPE | Signal::SIGSEGV
    )
}

// --- process helpers (free functions — unit-testable against a real child) ---

/// Spawn `argv` in `workdir` with exactly `env` (stdio inherited). Returns the
/// child pid; the [`std::process::Child`] is dropped (its `Drop` neither waits nor
/// kills) because the supervisor reaps via `waitpid` to also harvest grandchildren.
fn spawn_process(argv: &[String], workdir: &Path, env: &[(String, String)]) -> Result<Pid> {
    let (program, rest) = argv
        .split_first()
        .ok_or_else(|| Error::Process("empty command".to_owned()))?;
    let mut cmd = Command::new(program);
    cmd.args(rest).current_dir(workdir).env_clear();
    cmd.envs(env.iter().map(|(k, v)| (k, v)));
    let child = cmd
        .spawn()
        .map_err(|e| Error::Process(format!("spawn {program}: {e}")))?;
    i32::try_from(child.id())
        .map(Pid::from_raw)
        .map_err(|_| Error::Process("child pid out of range".to_owned()))
}

/// Gracefully stop a specific child: `SIGTERM`, wait up to `timeout` (never killing
/// early), then `SIGKILL`. Reaps the child and returns its exit status.
fn graceful_stop(pid: Pid, timeout: Duration) -> Option<WaitStatus> {
    let _ = kill(pid, Signal::SIGTERM);
    let deadline = Instant::now() + timeout;
    loop {
        match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => {
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(STOP_POLL);
            }
            Ok(status) => return Some(status),
            Err(_) => return None,
        }
    }
    let _ = kill(pid, Signal::SIGKILL);
    waitpid(pid, None).ok()
}

/// Terminate an external process we cannot reap (an orphan re-parented to init):
/// `SIGTERM`, poll liveness up to `timeout`, then `SIGKILL`.
fn terminate_external(pid: Pid, timeout: Duration) {
    if kill(pid, Signal::SIGTERM).is_err() {
        return; // already gone
    }
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_alive(pid) {
            return;
        }
        std::thread::sleep(STOP_POLL);
    }
    let _ = kill(pid, Signal::SIGKILL);
}

/// Liveness probe via signal 0: alive unless `kill` reports `ESRCH`.
fn process_alive(pid: Pid) -> bool {
    !matches!(kill(pid, None), Err(Errno::ESRCH))
}

/// Translate a child wait status into a process exit code (`128 + signal` for a
/// signalled child, mirroring the shell convention).
fn exit_code_from(status: WaitStatus) -> u8 {
    match status {
        WaitStatus::Exited(_, code) => u8::try_from(code).unwrap_or(0),
        WaitStatus::Signaled(_, sig, _) => u8::try_from(128 + (sig as i32)).unwrap_or(255),
        _ => 0,
    }
}

/// Exponential backoff for the `attempt`-th restart (0-based): `base * 2^attempt`,
/// capped at `max` (all in milliseconds), saturating rather than overflowing.
fn backoff_delay(attempt: u32, base_ms: u64, max_ms: u64) -> Duration {
    let factor = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
    Duration::from_millis(base_ms.saturating_mul(factor).min(max_ms))
}

/// Did the child *fail*? Any outcome other than a clean `exit(0)` (a non-zero
/// exit or a fatal signal) counts as a failure for `restart=on-failure`.
const fn is_failure(status: WaitStatus) -> bool {
    !matches!(status, WaitStatus::Exited(_, 0))
}

/// What to do when the supervised child exits while in the `Run` phase (i.e. not
/// a lode-initiated stop). Computed by the pure [`exit_action`] so the policy is
/// unit-testable without spawning processes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ExitAction {
    /// A lode-update is pending — apply it and launch the named version. Wins
    /// over the restart policy (covers "app wrote state.target then exit(0)").
    ApplyUpdate(String),
    /// Restart the same version after this backoff delay (restart policy active).
    Restart(Duration),
    /// Stop supervising and exit with `code`. `gave_up` is set when the restart
    /// cap was hit (terminal `error` status) vs. a plain mirror exit.
    Exit { code: u8, gave_up: bool },
}

/// Decide what a `Run`-phase child exit means, given the restart `policy`, the
/// child's `status`, any resolved `pending_target` (an installable version
/// different from the running one — caller-resolved, see [`Supervisor::pending_update`]),
/// the consecutive-restart count so far, and the backoff knobs.
///
/// Order: a pending update always wins; otherwise the policy decides between a
/// bounded-backoff restart and mirroring the child (exit with its code). A
/// `restart_max` of `0` means unlimited; reaching the cap exits with `gave_up`.
fn exit_action(
    policy: RestartPolicy,
    status: WaitStatus,
    pending_target: Option<&str>,
    restarts: u32,
    restart_max: u32,
    backoff_base_ms: u64,
    backoff_max_ms: u64,
) -> ExitAction {
    if let Some(version) = pending_target {
        return ExitAction::ApplyUpdate(version.to_owned());
    }
    let code = exit_code_from(status);
    let wants_restart = match policy {
        RestartPolicy::Off => false,
        RestartPolicy::OnFailure => is_failure(status),
        RestartPolicy::Always => true,
    };
    if !wants_restart {
        return ExitAction::Exit {
            code,
            gave_up: false,
        };
    }
    // Bounded restart: give up once `restart_max` consecutive restarts are done.
    if restart_max > 0 && restarts + 1 > restart_max {
        return ExitAction::Exit {
            code,
            gave_up: true,
        };
    }
    ExitAction::Restart(backoff_delay(restarts, backoff_base_ms, backoff_max_ms))
}

// --- pure update / readiness / rollback decision logic (design §5/§8) ---

/// What an [`update.policy`](crate::config::Policy) check should do with the
/// channel-latest version it just resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PolicyAction {
    /// Nothing to do (policy `off`/pinned, or already up to date).
    Idle,
    /// Advertise the newer version in `state.available` without applying it
    /// (`policy=check`): the app decides whether to request it.
    Advertise(String),
    /// Auto-apply the newer version by setting `state.target` (`policy=auto`).
    Apply(String),
}

/// Is `candidate` a newer version than `current`? Compares by semver precedence
/// when both parse; otherwise treats any *different* id as newer (so a publisher
/// can ship a non-semver channel tag without lode getting stuck, while an
/// unchanged id never re-applies).
fn is_newer(candidate: &str, current: &str) -> bool {
    match (
        semver::Version::parse(candidate),
        semver::Version::parse(current),
    ) {
        (Ok(c), Ok(cur)) => c > cur,
        _ => candidate != current,
    }
}

/// Decide what an update check does, given the policy, whether a `pin` is set, the
/// freshly-fetched channel `latest` and the running `current` version (design §5).
/// A `pin` forces [`PolicyAction::Idle`] (pin acts like `off` + a fixed target).
fn policy_action(policy: Policy, pinned: bool, latest: &str, current: &str) -> PolicyAction {
    if pinned || !is_newer(latest, current) {
        return PolicyAction::Idle;
    }
    match policy {
        Policy::Off => PolicyAction::Idle,
        Policy::Check => PolicyAction::Advertise(latest.to_owned()),
        Policy::Auto => PolicyAction::Apply(latest.to_owned()),
    }
}

/// Has the freshly-spawned instance signalled readiness (design §8)? `none` =>
/// alive at least `health_grace`; `state` => the app wrote `state.ready` equal to
/// this spawn's `LODE_INSTANCE`.
fn readiness_met(
    mode: Readiness,
    ready: Option<&str>,
    instance: &str,
    alive_for: Duration,
    health_grace: Duration,
) -> bool {
    match mode {
        Readiness::None => alive_for >= health_grace,
        Readiness::State => ready == Some(instance),
    }
}

/// The outcome of one observation tick on a freshly-applied update target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObserveOutcome {
    /// Keep observing.
    Pending,
    /// The new version is healthy — commit it as `last_good`.
    Commit,
    /// The new version failed — roll back to the previous `last_good`.
    Rollback,
}

/// Fold one observation tick into an outcome: readiness wins (commit); a readiness
/// timeout triggers a rollback; else keep waiting. A crash within the grace window
/// is handled separately (single-strike rollback in [`Supervisor::on_observe_exit`]).
const fn observe_decision(ready: bool, timed_out: bool) -> ObserveOutcome {
    if ready {
        ObserveOutcome::Commit
    } else if timed_out {
        ObserveOutcome::Rollback
    } else {
        ObserveOutcome::Pending
    }
}

/// Append a rollout-history entry, bounding the vector to [`HISTORY_CAP`] (oldest
/// dropped first) so `state.json` stays small.
fn push_history(history: &mut Vec<HistoryEntry>, version: &str, result: HistoryResult, at: String) {
    history.push(HistoryEntry {
        version: version.to_owned(),
        at,
        result,
    });
    if history.len() > HISTORY_CAP {
        let overflow = history.len() - HISTORY_CAP;
        history.drain(0..overflow);
    }
}

/// Current wall-clock time as an RFC 3339 UTC timestamp (`YYYY-MM-DDThh:mm:ssZ`),
/// used for `state.last_check` and `history[].at`. Falls back to the epoch if the
/// clock is before `UNIX_EPOCH` (never panics).
fn now_timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format_rfc3339(secs)
}

/// Format `epoch_secs` (seconds since the Unix epoch, UTC) as `YYYY-MM-DDThh:mm:ssZ`.
#[allow(clippy::cast_possible_wrap)] // epoch seconds stay far within i64 range
fn format_rfc3339(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86_400) as i64;
    let rem = epoch_secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Convert a count of days since the Unix epoch into a civil `(year, month, day)`
/// (Howard Hinnant's algorithm; proleptic Gregorian, valid for all realistic
/// timestamps).
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)] // bounded sub-results
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (year + i64::from(month <= 2), month, day)
}

// --- supervisor ---

/// Where the supervisor is in the update lifecycle.
enum Phase {
    /// Normal supervision: crash-restart + update polling (design §5/§8).
    Run,
    /// Observing a freshly-applied target for readiness + stability before
    /// committing it as `last_good`, or rolling back on failure (design §5).
    Observe(Observe),
}

/// State carried while observing a freshly-applied (or rolled-back) version.
/// Rollback is single-strike: any exit within the grace window, or a readiness
/// timeout, fails the observation (design §5).
struct Observe {
    /// The version being observed (now `current`).
    applied: String,
    /// The version to roll back to on failure (the one we replaced), or `None`
    /// when this *is* the rollback observation (`applied` == `last_good`): with
    /// no further fallback, a failure here makes lode exit.
    fallback: Option<String>,
    /// Deadline for the readiness handshake (`readiness=state`, design §8).
    deadline: Instant,
}

/// Owns the supervise loop state for one served version.
struct Supervisor<'c> {
    cfg: &'c Config,
    target: Target,
    runtime_dir: Option<PathBuf>,
    forward: Vec<Signal>,
    restart: Option<Signal>,
    /// The live child, or `None` while backing off before a restart.
    child: Option<Pid>,
    /// When the current child was spawned (to reset the backoff after `grace`).
    spawn_at: Instant,
    /// Consecutive crash restarts in the current crash-loop.
    restart_count: u32,
    /// When to re-spawn after a backoff (`None` once spawned).
    restart_at: Option<Instant>,
    /// Monotonic per-spawn counter feeding `LODE_INSTANCE`.
    instance_seq: u64,
    /// The current child's `LODE_INSTANCE` value (for the readiness handshake).
    instance: String,
    /// Per-process random key mixed into `LODE_INSTANCE`, so a stale `state.ready`
    /// from a previous lode — even one that reused this OS pid — can never satisfy
    /// a fresh spawn's readiness handshake.
    boot: String,
    /// Update lifecycle: normal supervision vs. observing an applied target.
    phase: Phase,
    /// When `state.json`'s mtime was last polled (`None` => never).
    last_state_poll: Option<Instant>,
    /// The mtime observed at the last poll, to skip re-reads when unchanged.
    last_state_mtime: Option<SystemTime>,
    /// The highest `restart_nonce` already serviced (so each bump acts once).
    last_nonce: u64,
    /// When the next policy update check is due (`None` => no further checks).
    next_check_at: Option<Instant>,
}

impl<'c> Supervisor<'c> {
    fn new(cfg: &'c Config, target: Target, runtime_dir: Option<PathBuf>) -> Self {
        // Seed the serviced nonce from any existing state so a pre-existing
        // `restart_nonce` does not trigger a spurious restart on startup.
        let last_nonce = state::read(&cfg.global.data_dir.join("state.json"))
            .ok()
            .flatten()
            .map_or(0, |st| st.restart_nonce);
        // Schedule the first update check immediately for check/auto (unless
        // pinned); `off`/pinned never checks (design §5).
        let next_check_at = if cfg.update.pin.is_some() || matches!(cfg.update.policy, Policy::Off)
        {
            None
        } else {
            Some(Instant::now())
        };
        Self {
            forward: forward_signals(&cfg.signals.forward),
            restart: cfg.signals.restart.as_deref().and_then(parse_signal),
            cfg,
            target,
            runtime_dir,
            child: None,
            spawn_at: Instant::now(),
            restart_count: 0,
            restart_at: None,
            instance_seq: 0,
            instance: String::new(),
            boot: random_boot_key(),
            phase: Phase::Run,
            last_state_poll: None,
            last_state_mtime: None,
            last_nonce,
            next_check_at,
        }
    }

    /// Run the supervise loop until a termination signal or the restart limit.
    fn run(&mut self) -> Result<ExitCode> {
        let mut signals = Signals::new(self.registration_set())
            .map_err(|e| Error::Process(format!("install signal handlers: {e}")))?;

        // v1 implements `stop-start` fully; the zero-downtime modes are optional /
        // out of scope (design §8) and fall back to stop-start with this note.
        if !matches!(
            self.cfg.supervise.restart_mode,
            crate::config::RestartMode::StopStart
        ) {
            tracing::info!(
                mode = ?self.cfg.supervise.restart_mode,
                "restart_mode is not yet supported; using stop-start (v1 default, design §8)"
            );
        }

        self.set_status(Status::Starting)?;
        self.spawn()?;

        loop {
            for raw in signals.pending() {
                let Ok(sig) = Signal::try_from(raw) else {
                    continue;
                };
                match classify(sig, self.restart, &self.forward) {
                    Action::Terminate => return self.shutdown(),
                    Action::Restart => self.graceful_restart()?,
                    Action::Forward => {
                        if let Some(pid) = self.child {
                            let _ = kill(pid, sig);
                        }
                    }
                    Action::Ignore => {}
                }
            }

            if let Some(status) = self.reap()
                && let Some(code) = self.on_child_exit(status)?
            {
                return Ok(ExitCode::from(code));
            }

            if self.child.is_none() && self.restart_at.is_some_and(|at| Instant::now() >= at) {
                self.restart_at = None;
                self.respawn()?;
            }

            // C2: honour app-written requests, run the policy update check, and
            // drive the readiness/rollback observation of an applied target. A
            // failed rollback-target observation exits lode (no restart loop).
            self.poll_state()?;
            self.maybe_check_update();
            if let Some(code) = self.poll_observe()? {
                return Ok(ExitCode::from(code));
            }

            std::thread::sleep(POLL_TICK);
        }
    }

    /// The signals to register: termination set + forward set + restart signal,
    /// minus the forbidden ones, deduplicated.
    fn registration_set(&self) -> Vec<c_int> {
        let mut wanted = vec![Signal::SIGTERM, Signal::SIGINT, Signal::SIGQUIT];
        wanted.extend(self.forward.iter().copied());
        if let Some(sig) = self.restart {
            wanted.push(sig);
        }
        let mut ints: Vec<c_int> = Vec::new();
        for sig in wanted {
            if is_forbidden(sig) {
                tracing::warn!(
                    signal = sig.as_str(),
                    "refusing to register forbidden signal"
                );
                continue;
            }
            let raw = sig as c_int;
            if !ints.contains(&raw) {
                ints.push(raw);
            }
        }
        ints
    }

    /// Launch the child process for the current `target`, recording its pid,
    /// spawn time and `LODE_INSTANCE`. Does *not* touch `state.json` — the caller
    /// writes the phase-appropriate status afterwards.
    fn spawn_child(&mut self) -> Result<Pid> {
        self.instance_seq = self.instance_seq.saturating_add(1);
        let instance = format!("{}-{}-{}", std::process::id(), self.boot, self.instance_seq);
        let entry = self.target.entry.to_string_lossy();
        let dir = self.target.dir.to_string_lossy();
        let argv = build_run_argv(&self.cfg.command.run, &entry, &dir)?;
        let mut env = child_env(
            std::env::vars(),
            &self.cfg.env,
            &self.target.version,
            &self.cfg.global.data_dir,
            &instance,
            self.runtime_dir.as_deref(),
        );
        // Tell the app which readiness contract is in force so it knows whether to
        // run the `state.ready` handshake (design §8); a self-introspection var,
        // like the other `LODE_*` injected above.
        env.push((
            "LODE_READINESS".to_owned(),
            readiness_label(self.cfg.supervise.readiness).to_owned(),
        ));
        let workdir = PathBuf::from(expand_token(&self.cfg.command.workdir, &entry, &dir));

        let pid = spawn_process(&argv, &workdir, &env)?;
        self.child = Some(pid);
        self.spawn_at = Instant::now();
        self.instance.clone_from(&instance);
        tracing::info!(
            version = self.target.version,
            pid = pid.as_raw(),
            instance,
            "spawned child"
        );
        Ok(pid)
    }

    /// Spawn the child and report it `running` (the bare-start / graceful-restart
    /// path — always in the `Run` phase).
    fn spawn(&mut self) -> Result<()> {
        let pid = self.spawn_child()?;
        self.write_running_state(pid)
    }

    /// Re-spawn after a backoff, writing the status appropriate to the current
    /// phase (`running` while supervising, `updating` while observing a target).
    fn respawn(&mut self) -> Result<()> {
        let pid = self.spawn_child()?;
        match self.phase {
            Phase::Observe(_) => self.write_observing_state(pid),
            Phase::Run => self.write_running_state(pid),
        }
    }

    /// Reap our child plus any re-parented grandchildren (subreaper). Returns the
    /// supervised child's status if it exited this pass, else `None`.
    fn reap(&mut self) -> Option<WaitStatus> {
        let mut child_status = None;
        loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                // No more children have changed state, or none remain (ECHILD).
                Ok(WaitStatus::StillAlive) | Err(_) => break,
                Ok(status) => {
                    if status.pid() == self.child {
                        self.child = None;
                        child_status = Some(status);
                    }
                    // else: a grandchild we adopted — reaped and discarded.
                }
            }
        }
        child_status
    }

    /// Handle a child exit reaped in the run loop (i.e. *not* a lode-initiated
    /// stop). Returns `Some(code)` when lode should exit with that code, or `None`
    /// after scheduling a restart / applying an update / rolling back.
    ///
    /// A pending update always wins (update-on-exit). Otherwise the
    /// `supervise.restart` policy decides between a bounded-backoff restart and
    /// mirroring the child (exit with its code). While observing a freshly-applied
    /// version this is a single-strike rollback (design §5).
    fn on_child_exit(&mut self, status: WaitStatus) -> Result<Option<u8>> {
        if matches!(self.phase, Phase::Observe(_)) {
            return self.on_observe_exit(status);
        }

        // A child that survived the grace window starts a fresh restart sequence.
        if self.spawn_at.elapsed() >= Duration::from_secs(self.cfg.supervise.health_grace) {
            self.restart_count = 0;
        }

        let pending = self.pending_update();
        match exit_action(
            self.cfg.supervise.restart,
            status,
            pending.as_deref(),
            self.restart_count,
            self.cfg.supervise.restart_max,
            self.cfg.supervise.restart_backoff,
            self.cfg.supervise.restart_backoff_max,
        ) {
            ExitAction::ApplyUpdate(version) => {
                tracing::info!(version, "child exited with an update pending; applying");
                self.apply_target(&version)?;
                if self.child.is_some() {
                    return Ok(None); // now observing the new version
                }
                // The update could not be started (apply_target recorded why);
                // lode surfaces the failure and exits rather than spin childless.
                tracing::error!(version, "pending update could not be started; lode exiting");
                let code = exit_code_from(status);
                let code = if code == 0 { 1 } else { code };
                self.mutate_state(|st| {
                    st.status = Some(Status::Error);
                    st.pid = None;
                })?;
                Ok(Some(code))
            }
            ExitAction::Restart(delay) => {
                self.schedule_restart(status, delay);
                Ok(None)
            }
            ExitAction::Exit { code, gave_up } => {
                self.finish_exit(code, gave_up)?;
                Ok(Some(code))
            }
        }
    }

    /// Record a scheduled backoff restart of the same version (`restart` policy
    /// active). The new child is spawned by the run loop once `restart_at` is due.
    fn schedule_restart(&mut self, status: WaitStatus, delay: Duration) {
        self.restart_count = self.restart_count.saturating_add(1);
        let code = exit_code_from(status);
        let backoff_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
        tracing::warn!(
            version = self.target.version,
            code,
            restart = self.restart_count,
            backoff_ms,
            "child exited; scheduling restart"
        );
        self.restart_at = Some(Instant::now() + delay);
    }

    /// Stop supervising and exit: write the terminal status — `error` when lode
    /// gave up (restart cap) or the child failed, `stopped` on a clean mirror exit.
    fn finish_exit(&self, code: u8, gave_up: bool) -> Result<()> {
        if gave_up {
            tracing::error!(
                version = self.target.version,
                code,
                limit = self.cfg.supervise.restart_max,
                "restart limit reached; lode exiting"
            );
            self.set_error(&format!(
                "restart limit ({}) reached",
                self.cfg.supervise.restart_max
            ))
        } else if code == 0 {
            tracing::info!(
                version = self.target.version,
                "child exited cleanly; lode exiting"
            );
            self.set_stopped()
        } else {
            tracing::error!(
                version = self.target.version,
                code,
                "child exited; lode exiting"
            );
            self.set_error(&format!("child exited with code {code}"))
        }
    }

    /// Resolve a lode-update to apply when the child exits: an app/auto-written
    /// `state.target` naming a different version, or — under `policy=auto` — a
    /// channel latest newer than current. Best-effort; IO failures yield `None`.
    fn pending_update(&self) -> Option<String> {
        let path = self.cfg.global.data_dir.join("state.json");
        if let Ok(Some(st)) = state::read(&path)
            && let Some(target) = st.target
            && target != self.target.version
        {
            return Some(target);
        }
        if matches!(self.cfg.update.policy, Policy::Auto)
            && self.cfg.update.pin.is_none()
            && let Some(latest) = self.resolve_latest()
            && is_newer(&latest, &self.target.version)
        {
            return Some(latest);
        }
        None
    }

    /// Resolve the channel-latest version from a freshly-fetched manifest
    /// (best-effort; any fetch/parse/mismatch error yields `None`).
    fn resolve_latest(&self) -> Option<String> {
        let manifest = manifest::fetch(self.cfg).ok()?;
        if manifest.name != self.cfg.global.app {
            return None;
        }
        manifest::resolve_target(
            &manifest,
            &self.cfg.update.channel,
            self.cfg.update.pin.as_deref(),
            Some("latest"),
        )
        .ok()
    }

    /// Handle a child exit while observing a freshly-activated version: a single
    /// strike rolls back to the fallback (or exits if this *was* the rollback).
    fn on_observe_exit(&mut self, status: WaitStatus) -> Result<Option<u8>> {
        let code = exit_code_from(status);
        tracing::warn!(
            version = self.target.version,
            code,
            "freshly-activated version exited within the grace window"
        );
        self.observe_failed("crashed within health grace", Some(status))
    }

    /// Graceful restart (configured restart signal): stop the child, reset the
    /// backoff and re-spawn immediately.
    fn graceful_restart(&mut self) -> Result<()> {
        tracing::info!(version = self.target.version, "graceful restart requested");
        if self.child.is_some() {
            self.set_status(Status::Stopping)?;
            self.stop_child();
        }
        self.restart_count = 0;
        self.restart_at = None;
        self.spawn()
    }

    /// Graceful shutdown: stop the child and exit with its code.
    fn shutdown(&mut self) -> Result<ExitCode> {
        tracing::info!("termination signal received; stopping child");
        self.set_status(Status::Stopping)?;
        let code = self.stop_child().map_or(0, exit_code_from);
        self.set_stopped()?;
        Ok(ExitCode::from(code))
    }

    /// Stop the current child (if any), returning its exit status.
    fn stop_child(&mut self) -> Option<WaitStatus> {
        let pid = self.child.take()?;
        graceful_stop(pid, Duration::from_secs(self.cfg.supervise.stop_timeout))
    }

    // --- state.json (read-modify-write, preserving app-owned fields) ---

    fn write_running_state(&self, pid: Pid) -> Result<()> {
        let pid_u32 = u32::try_from(pid.as_raw()).ok();
        let version = self.target.version.clone();
        self.mutate_state(|st| {
            st.status = Some(Status::Running);
            st.current = Some(version.clone());
            if st.last_good.is_none() {
                st.last_good = Some(version);
            }
            st.pid = pid_u32;
        })
    }

    fn set_status(&self, status: Status) -> Result<()> {
        self.mutate_state(|st| st.status = Some(status))
    }

    fn set_stopped(&self) -> Result<()> {
        self.mutate_state(|st| {
            st.status = Some(Status::Stopped);
            st.pid = None;
        })
    }

    fn set_error(&self, message: &str) -> Result<()> {
        let message = message.to_owned();
        self.mutate_state(|st| {
            st.status = Some(Status::Error);
            st.last_error = Some(message);
            st.pid = None;
        })
    }

    fn mutate_state(&self, edit: impl FnOnce(&mut State)) -> Result<()> {
        let path = self.cfg.global.data_dir.join("state.json");
        let mut st = state::read(&path)?.unwrap_or_default();
        edit(&mut st);
        state::write(&path, &st)
    }

    /// Report the child as `updating` (current + pid) while observing it, and
    /// consume the `target` request that triggered the apply.
    fn write_observing_state(&self, pid: Pid) -> Result<()> {
        let pid_u32 = u32::try_from(pid.as_raw()).ok();
        let version = self.target.version.clone();
        self.mutate_state(|st| {
            st.status = Some(Status::Updating);
            st.current = Some(version);
            st.pid = pid_u32;
            st.target = None;
        })
    }

    /// Record a non-fatal `last_error` without disturbing `status`/`pid` (the
    /// child keeps running on the current version).
    fn note_error(&self, message: &str) -> Result<()> {
        let message = message.to_owned();
        self.mutate_state(|st| st.last_error = Some(message))
    }

    /// Clear a consumed `target` request from `state.json`.
    fn clear_target(&self) -> Result<()> {
        self.mutate_state(|st| st.target = None)
    }

    // --- C2: app-request poll, update policy, apply / observe / rollback (§5/§7/§8) ---

    /// Poll `state.json`'s mtime (~1s) and, on a change, honour app-written
    /// requests: a bumped `restart_nonce` (graceful restart) or a new `target`
    /// (hot-update). Only acted on in the `Run` phase so an in-flight update is
    /// never interrupted (design §7).
    fn poll_state(&mut self) -> Result<()> {
        if !self.state_poll_due() {
            return Ok(());
        }
        self.last_state_poll = Some(Instant::now());
        let path = self.cfg.global.data_dir.join("state.json");
        let mtime = state::mtime(&path)?;
        if mtime == self.last_state_mtime {
            return Ok(());
        }
        self.last_state_mtime = mtime;

        let Some(st) = state::read(&path)? else {
            return Ok(());
        };

        // A bumped restart nonce is high-water-marked so it acts exactly once,
        // even though lode's own writes also move the file's mtime.
        if st.restart_nonce > self.last_nonce {
            self.last_nonce = st.restart_nonce;
            if matches!(self.phase, Phase::Run) {
                tracing::info!(nonce = st.restart_nonce, "restart requested via state.json");
                return self.graceful_restart();
            }
        }

        // A target different from the running version is a hot-update request
        // (apply consumes it by clearing `target`).
        if matches!(self.phase, Phase::Run)
            && let Some(target) = st.target
            && target != self.target.version
        {
            return self.apply_target(&target);
        }
        Ok(())
    }

    /// Is the ~1s `state.json` poll due?
    fn state_poll_due(&self) -> bool {
        self.last_state_poll
            .is_none_or(|t| t.elapsed() >= STATE_POLL_INTERVAL)
    }

    /// Run the policy update check when due, then schedule the next one.
    fn maybe_check_update(&mut self) {
        if !self.update_check_due() {
            return;
        }
        self.run_update_check();
        self.schedule_next_check();
    }

    /// Is a policy update check due?
    fn update_check_due(&self) -> bool {
        self.next_check_at.is_some_and(|at| Instant::now() >= at)
    }

    /// Schedule the next check: never (`check_interval=0` => once at startup), or
    /// `check_interval` seconds out.
    fn schedule_next_check(&mut self) {
        self.next_check_at = if self.cfg.update.check_interval == 0 {
            None
        } else {
            Some(Instant::now() + Duration::from_secs(self.cfg.update.check_interval))
        };
    }

    /// Fetch the manifest and apply the `[update].policy`: `check` advertises a
    /// newer version in `state.available`; `auto` sets `state.target` to apply it
    /// (design §5). Best-effort — network/parse failures are logged, never fatal.
    fn run_update_check(&self) {
        let manifest = match manifest::fetch(self.cfg) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "update check: manifest fetch failed");
                let _ = self.note_error(&format!("update check: {e}"));
                return;
            }
        };
        if manifest.name != self.cfg.global.app {
            tracing::warn!(
                manifest = manifest.name,
                app = self.cfg.global.app,
                "update check: manifest name mismatch"
            );
            return;
        }
        let latest = match manifest::resolve_target(
            &manifest,
            &self.cfg.update.channel,
            self.cfg.update.pin.as_deref(),
            Some("latest"),
        ) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "update check: cannot resolve channel latest");
                return;
            }
        };

        let action = policy_action(
            self.cfg.update.policy,
            self.cfg.update.pin.is_some(),
            &latest,
            &self.target.version,
        );
        // `available` advertises a newer version (cleared when up to date);
        // `target` is only set for `auto` and must never clobber an app request.
        let (available, target) = match &action {
            PolicyAction::Idle => (None, None),
            PolicyAction::Advertise(v) => (Some(v.clone()), None),
            PolicyAction::Apply(v) => (Some(v.clone()), Some(v.clone())),
        };
        let now = now_timestamp();
        let channel = self.cfg.update.channel.clone();
        if let Err(e) = self.mutate_state(|st| {
            st.last_check = Some(now);
            st.channel = Some(channel);
            st.available = available;
            if let Some(target) = target {
                st.target = Some(target);
            }
        }) {
            tracing::warn!(error = %e, "update check: state write failed");
            return;
        }
        match action {
            PolicyAction::Idle => {
                tracing::debug!(
                    latest,
                    current = self.target.version,
                    "update check: up to date"
                );
            }
            PolicyAction::Advertise(v) => {
                tracing::info!(available = v, "update check: newer version available");
            }
            PolicyAction::Apply(v) => {
                tracing::info!(target = v, "update check: auto-applying newer version");
            }
        }
    }

    /// Apply an update `target` via the stop-start hot-update (design §5): ensure
    /// it is installed, graceful-stop the old child, atomically switch `current`,
    /// start the new child, and enter the readiness/rollback observation window.
    /// Install/locate failures keep the current version running.
    fn apply_target(&mut self, version: &str) -> Result<()> {
        if version == self.target.version {
            return self.clear_target(); // already on it — just drop the request
        }
        tracing::info!(
            from = self.target.version,
            to = version,
            "applying update target"
        );

        if let Err(e) = self.ensure_installed(version) {
            tracing::error!(error = %e, version, "cannot install update target; staying on current");
            self.note_error(&format!("install {version}: {e}"))?;
            return self.clear_target();
        }
        let new_target = match locate(self.cfg, version) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(error = %e, version, "update target not usable; staying on current");
                self.note_error(&format!("locate {version}: {e}"))?;
                return self.clear_target();
            }
        };

        let fallback = self.target.version.clone();
        self.set_status(Status::Updating)?;
        if self.child.is_some() {
            self.stop_child();
        }
        install::switch_current(self.cfg, version)?;
        self.target = new_target;
        self.restart_count = 0;
        self.restart_at = None;
        let pid = self.spawn_child()?;
        self.write_observing_state(pid)?;
        self.phase = Phase::Observe(Observe {
            applied: version.to_owned(),
            fallback: Some(fallback),
            deadline: Instant::now() + Duration::from_secs(self.cfg.supervise.ready_timeout),
        });
        Ok(())
    }

    /// Ensure `version` is installed, downloading + verifying + installing it (via
    /// the audited [`crate::install`] path) when absent. No-op if already present.
    fn ensure_installed(&self, version: &str) -> Result<()> {
        if version_installed(self.cfg, version) {
            return Ok(());
        }
        tracing::info!(version, "update target not installed; downloading");
        let manifest = manifest::fetch(self.cfg)?;
        if manifest.name != self.cfg.global.app {
            return Err(Error::Manifest(format!(
                "manifest name {:?} does not match configured app {:?}",
                manifest.name, self.cfg.global.app
            )));
        }
        let entry = manifest::version_entry(&manifest, version)?;
        let asset = manifest::select_asset(entry, required_asset(self.cfg)?)?;
        let (temp, sha256) =
            download::fetch_artifact(self.cfg, asset, version, &manifest::allowed_hosts(self.cfg))?;
        install::install(self.cfg, version, asset, &temp, &sha256)
    }

    /// One observation tick on a freshly-applied target: commit it as `last_good`
    /// once ready, or roll back on a readiness timeout / crash threshold (§5/§8).
    fn poll_observe(&mut self) -> Result<Option<u8>> {
        if !matches!(self.phase, Phase::Observe(_)) {
            return Ok(None);
        }
        let ready = self.observe_ready()?;
        let timed_out = self.observe_timed_out();
        match observe_decision(ready, timed_out) {
            ObserveOutcome::Pending => Ok(None),
            ObserveOutcome::Commit => {
                self.commit_update()?;
                Ok(None)
            }
            ObserveOutcome::Rollback => self.observe_failed("readiness timeout", None),
        }
    }

    /// Has the observed child signalled readiness for this spawn (design §8)?
    /// A dead child (between crash and backoff respawn) is never ready.
    fn observe_ready(&self) -> Result<bool> {
        if self.child.is_none() {
            return Ok(false);
        }
        let ready_field = match self.cfg.supervise.readiness {
            Readiness::None => None,
            Readiness::State => {
                let path = self.cfg.global.data_dir.join("state.json");
                state::read(&path)?.and_then(|st| st.ready)
            }
        };
        Ok(readiness_met(
            self.cfg.supervise.readiness,
            ready_field.as_deref(),
            &self.instance,
            self.spawn_at.elapsed(),
            Duration::from_secs(self.cfg.supervise.health_grace),
        ))
    }

    /// Has the `readiness=state` handshake exceeded `ready_timeout`? (No timeout
    /// applies in `none` mode — it resolves via grace-survival or the crash count.)
    fn observe_timed_out(&self) -> bool {
        matches!(self.cfg.supervise.readiness, Readiness::State)
            && match &self.phase {
                Phase::Observe(obs) => Instant::now() >= obs.deadline,
                Phase::Run => false,
            }
    }

    /// Commit the observed target: mark it `running` + `last_good`, append a `good`
    /// history entry, prune old versions, and return to the `Run` phase.
    fn commit_update(&mut self) -> Result<()> {
        let applied = match &self.phase {
            Phase::Observe(obs) => obs.applied.clone(),
            Phase::Run => return Ok(()),
        };
        tracing::info!(version = applied, "update ready — committing as last_good");
        self.restart_count = 0;
        let at = now_timestamp();
        self.mutate_state(|st| {
            st.status = Some(Status::Running);
            st.current = Some(applied.clone());
            st.last_good = Some(applied.clone());
            st.available = None;
            st.last_error = None;
            push_history(&mut st.history, &applied, HistoryResult::Good, at);
        })?;
        if let Err(e) = install::prune(self.cfg, Some(&applied), Some(&applied)) {
            tracing::warn!(error = %e, "prune after update failed");
        }
        self.phase = Phase::Run;
        Ok(())
    }

    /// A failed observation (crash within grace, or readiness timeout). Roll back
    /// to the fallback and observe it; if there is no fallback — we were already
    /// observing `last_good` — lode gives up and exits (`Some(code)`), with no
    /// restart loop. `status` carries the child's exit status for a crash, `None`
    /// for a readiness timeout.
    fn observe_failed(&mut self, reason: &str, status: Option<WaitStatus>) -> Result<Option<u8>> {
        let (applied, fallback) = match &self.phase {
            Phase::Observe(obs) => (obs.applied.clone(), obs.fallback.clone()),
            Phase::Run => return Ok(None),
        };
        if let Some(fallback) = fallback {
            self.rollback_to(&applied, &fallback, reason)?;
            return Ok(None);
        }

        // No further fallback: the rollback target (last_good) itself failed.
        tracing::error!(
            version = applied,
            reason,
            "rollback target failed; lode exiting"
        );
        if self.child.is_some() {
            self.stop_child();
        }
        self.phase = Phase::Run;
        let code = status.map_or(1, exit_code_from);
        let code = if code == 0 { 1 } else { code };
        let at = now_timestamp();
        self.mutate_state(|st| {
            st.status = Some(Status::Error);
            st.last_error = Some(format!("rollback target {applied} failed: {reason}"));
            st.pid = None;
            push_history(&mut st.history, &applied, HistoryResult::Bad, at);
        })?;
        Ok(Some(code))
    }

    /// Roll back the failed `applied` version to `fallback` (the version it
    /// replaced): stop the failed child, switch `current` back, spawn the fallback
    /// and OBSERVE it (a fresh activation that must itself survive its grace),
    /// appending a `bad` history entry for `applied` (design §5).
    fn rollback_to(&mut self, applied: &str, fallback: &str, reason: &str) -> Result<()> {
        tracing::warn!(
            failed = applied,
            fallback,
            reason,
            "update failed — rolling back"
        );
        self.set_status(Status::RollingBack)?;
        if self.child.is_some() {
            self.stop_child();
        }
        self.restart_count = 0;
        self.restart_at = None;

        if !version_installed(self.cfg, fallback) {
            // The known-good version is gone — keep the failed version running as a
            // best effort rather than leaving nothing supervised.
            tracing::error!(fallback, "rollback target is not installed");
            self.note_error(&format!("rollback target {fallback} not installed"))?;
            self.phase = Phase::Run;
            let pid = self.spawn_child()?;
            return self.write_running_state(pid);
        }

        install::switch_current(self.cfg, fallback)?;
        self.target = locate(self.cfg, fallback)?;
        let pid = self.spawn_child()?;
        let pid_u32 = u32::try_from(pid.as_raw()).ok();
        let at = now_timestamp();
        self.mutate_state(|st| {
            st.status = Some(Status::Updating);
            st.current = Some(fallback.to_owned());
            st.pid = pid_u32;
            push_history(&mut st.history, applied, HistoryResult::Bad, at);
        })?;
        // Observe the rollback target; with no further fallback, a failure exits.
        self.phase = Phase::Observe(Observe {
            applied: fallback.to_owned(),
            fallback: None,
            deadline: Instant::now() + Duration::from_secs(self.cfg.supervise.ready_timeout),
        });
        Ok(())
    }
}

// --- setup helpers ---

/// Acquire the single-instance PID lock (RAII; released on drop).
fn lock_acquire(cfg: &Config) -> Result<crate::lock::LockGuard> {
    crate::lock::acquire(&cfg.global.data_dir, &cfg.global.app)
}

/// Become a child subreaper so re-parented grandchildren are reaped by us (PID 1
/// init duty). Best-effort: a failure is logged, not fatal.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn set_subreaper() {
    if let Err(e) = nix::sys::prctl::set_child_subreaper(true) {
        tracing::warn!(error = %e, "could not set child subreaper");
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn set_subreaper() {}

/// Startup cleanup (design §5): terminate an orphaned app child left by a crashed
/// lode (from `state.pid`), then GC interrupted downloads / staging.
fn startup_cleanup(cfg: &Config) -> Result<()> {
    let state_path = cfg.global.data_dir.join("state.json");
    if let Some(st) = state::read(&state_path)?
        && let Some(pid) = st.pid
        && let Ok(raw) = i32::try_from(pid)
    {
        let pid = Pid::from_raw(raw);
        if process_alive(pid) {
            tracing::warn!(
                pid = raw,
                "terminating orphaned app child from a previous lode"
            );
            terminate_external(pid, Duration::from_secs(cfg.supervise.stop_timeout));
        }
    }
    clear_stale_ready(&state_path)?;
    install::gc(cfg)
}

/// Drop a stale `state.ready` left by a previous lode run so it can never be
/// mistaken for this run's first spawn — defence-in-depth alongside the
/// per-process random `LODE_INSTANCE` key ([`random_boot_key`]). All other state
/// fields (`current` / `last_good` / …) are preserved.
fn clear_stale_ready(state_path: &Path) -> Result<()> {
    if let Some(mut st) = state::read(state_path)?
        && st.ready.is_some()
    {
        st.ready = None;
        state::write(state_path, &st)?;
    }
    Ok(())
}

/// A per-process random key (8 lowercase hex chars) mixed into `LODE_INSTANCE`.
/// With the monotonic per-spawn `seq` it makes the readiness-handshake id unique
/// across lode restarts too — even if the OS reuses this pid — so a stale
/// `state.ready` can never false-match a fresh spawn. Degrades to a fixed key
/// (pid + seq still disambiguate) only if the OS RNG is unavailable.
fn random_boot_key() -> String {
    let mut bytes = [0u8; 4];
    if getrandom::getrandom(&mut bytes).is_err() {
        return "00000000".to_owned();
    }
    format!("{:08x}", u32::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- signal classification ---

    #[test]
    fn classify_termination_signals() {
        let fwd = default_forward();
        for sig in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGQUIT] {
            assert_eq!(classify(sig, None, &fwd), Action::Terminate);
        }
    }

    #[test]
    fn classify_forward_and_ignore() {
        let fwd = default_forward();
        assert_eq!(classify(Signal::SIGHUP, None, &fwd), Action::Forward);
        assert_eq!(classify(Signal::SIGCONT, None, &fwd), Action::Forward);
        // SIGPIPE is in neither set.
        assert_eq!(classify(Signal::SIGPIPE, None, &fwd), Action::Ignore);
    }

    #[test]
    fn classify_restart_signal_wins_over_forward() {
        let fwd = default_forward();
        // SIGUSR2 is in the default forward set, but a configured restart signal
        // takes precedence and is never forwarded.
        assert_eq!(
            classify(Signal::SIGUSR2, Some(Signal::SIGUSR2), &fwd),
            Action::Restart
        );
        assert_eq!(
            classify(Signal::SIGHUP, Some(Signal::SIGUSR2), &fwd),
            Action::Forward
        );
    }

    #[test]
    fn forward_signals_default_and_parsed() {
        assert_eq!(forward_signals(&[]).len(), 6);
        assert_eq!(
            forward_signals(&["SIGHUP".to_owned(), "usr1".to_owned()]),
            vec![Signal::SIGHUP, Signal::SIGUSR1]
        );
        // Unparsable names are dropped, not fatal.
        assert_eq!(
            forward_signals(&["bogus".to_owned(), "HUP".to_owned()]),
            vec![Signal::SIGHUP]
        );
    }

    #[test]
    fn parse_signal_accepts_both_forms() {
        assert_eq!(parse_signal("SIGHUP"), Some(Signal::SIGHUP));
        assert_eq!(parse_signal("hup"), Some(Signal::SIGHUP));
        assert_eq!(parse_signal(" Sigusr2 "), Some(Signal::SIGUSR2));
        assert_eq!(parse_signal("nonsense"), None);
    }

    #[test]
    fn forbidden_signals_detected() {
        assert!(is_forbidden(Signal::SIGKILL));
        assert!(is_forbidden(Signal::SIGSTOP));
        assert!(!is_forbidden(Signal::SIGTERM));
    }

    // --- backoff schedule ---

    #[test]
    fn backoff_doubles_then_caps() {
        let base = 500;
        let max = 30_000;
        assert_eq!(backoff_delay(0, base, max), Duration::from_millis(500));
        assert_eq!(backoff_delay(1, base, max), Duration::from_secs(1));
        assert_eq!(backoff_delay(2, base, max), Duration::from_secs(2));
        assert_eq!(backoff_delay(6, base, max), Duration::from_secs(30));
        // A huge attempt saturates to the cap instead of overflowing.
        assert_eq!(backoff_delay(99, base, max), Duration::from_secs(30));
    }

    // --- exit codes ---

    #[test]
    fn exit_code_from_status() {
        let pid = Pid::from_raw(1234);
        assert_eq!(exit_code_from(WaitStatus::Exited(pid, 7)), 7);
        assert_eq!(
            exit_code_from(WaitStatus::Signaled(pid, Signal::SIGTERM, false)),
            128 + 15
        );
        assert_eq!(exit_code_from(WaitStatus::StillAlive), 0);
    }

    // --- env stripping + injection ---

    #[test]
    fn child_env_strips_lode_and_injects() {
        let host = vec![
            ("LODE_MANIFEST".to_owned(), "https://x".to_owned()),
            ("LODE_DATA_DIR".to_owned(), "/old".to_owned()),
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            ("HOME".to_owned(), "/root".to_owned()),
        ];
        let env = child_env(
            host,
            &BTreeMap::new(),
            "1.2.3",
            Path::new("/data"),
            "inst-9",
            None,
        );
        let map: std::collections::HashMap<_, _> = env.into_iter().collect();

        // All config LODE_* are stripped from the inherited set...
        assert!(!map.contains_key("LODE_MANIFEST"));
        // ...and host env passes through.
        assert_eq!(map.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(map.get("HOME").map(String::as_str), Some("/root"));
        // Introspection vars are injected (LODE_DATA_DIR re-set to the resolved dir).
        assert_eq!(
            map.get("LODE_ACTIVE_VERSION").map(String::as_str),
            Some("1.2.3")
        );
        assert_eq!(map.get("LODE_DATA_DIR").map(String::as_str), Some("/data"));
        assert_eq!(map.get("LODE_INSTANCE").map(String::as_str), Some("inst-9"));
    }

    #[test]
    fn child_env_defined_are_defaults_host_wins() {
        let host = vec![
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            ("NODE_ENV".to_owned(), "development".to_owned()),
        ];
        let defined: BTreeMap<String, String> = [
            ("NODE_ENV".to_owned(), "production".to_owned()), // host has it → host wins
            ("APP_FLAG".to_owned(), "on".to_owned()),         // host lacks it → default applied
            ("LODE_DATA_DIR".to_owned(), "/hijack".to_owned()), // lode's var still wins below
        ]
        .into_iter()
        .collect();
        let env = child_env(host, &defined, "1.0.0", Path::new("/data"), "i", None);

        // Exactly one entry per key — defaults fill gaps, they don't duplicate.
        assert_eq!(env.iter().filter(|(k, _)| k == "NODE_ENV").count(), 1);
        assert_eq!(env.iter().filter(|(k, _)| k == "LODE_DATA_DIR").count(), 1);

        let map: std::collections::HashMap<_, _> = env.into_iter().collect();
        // Inherited host env wins over a same-named [env] default (12-factor `-e`).
        assert_eq!(map.get("NODE_ENV").map(String::as_str), Some("development"));
        // A [env] key the host lacks is applied as the default.
        assert_eq!(map.get("APP_FLAG").map(String::as_str), Some("on"));
        // lode's injected vars still win over any [env] of the same name.
        assert_eq!(map.get("LODE_DATA_DIR").map(String::as_str), Some("/data"));
    }

    #[test]
    fn child_env_host_path_wins_over_defined_then_runtime_prepends() {
        // A host PATH beats a [env] PATH default; the runtime dir still prepends.
        let host = vec![("PATH".to_owned(), "/usr/bin".to_owned())];
        let mut defined = BTreeMap::new();
        defined.insert("PATH".to_owned(), "/opt/bin".to_owned()); // ignored: host has PATH
        let env = child_env(
            host,
            &defined,
            "1.0.0",
            Path::new("/data"),
            "i",
            Some(Path::new("/rt")),
        );
        let path = env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone());
        assert_eq!(path.as_deref(), Some("/rt:/usr/bin"));
    }

    #[test]
    fn child_env_prepends_runtime_to_path() {
        let host = vec![("PATH".to_owned(), "/usr/bin".to_owned())];
        let env = child_env(
            host,
            &BTreeMap::new(),
            "1.0.0",
            Path::new("/data"),
            "i",
            Some(Path::new("/data/runtime")),
        );
        let path = env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone());
        assert_eq!(path.as_deref(), Some("/data/runtime:/usr/bin"));
    }

    #[test]
    fn child_env_prepends_runtime_to_defined_path() {
        // When the host has no PATH, the [env] default is used — and still extended
        // by the runtime prepend.
        let mut defined = BTreeMap::new();
        defined.insert("PATH".to_owned(), "/opt/bin".to_owned());
        let env = child_env(
            Vec::new(),
            &defined,
            "1.0.0",
            Path::new("/data"),
            "i",
            Some(Path::new("/rt")),
        );
        let path = env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone());
        assert_eq!(path.as_deref(), Some("/rt:/opt/bin"));
    }

    #[test]
    fn child_env_creates_path_when_absent() {
        let env = child_env(
            Vec::new(),
            &BTreeMap::new(),
            "1.0.0",
            Path::new("/data"),
            "i",
            Some(Path::new("/rt")),
        );
        let path = env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone());
        assert_eq!(path.as_deref(), Some("/rt"));
    }

    // --- argv build + placeholder expansion ---

    #[test]
    fn run_argv_auto_appends_entry() {
        assert_eq!(
            build_run_argv("bun run", "/v/app.js", "/v").unwrap(),
            vec!["bun", "run", "/v/app.js"]
        );
        // Default run = "{entry}" → just the entry, no append.
        assert_eq!(
            build_run_argv("{entry}", "/v/app", "/v").unwrap(),
            vec!["/v/app"]
        );
        // Explicit {entry} placeholder is expanded, not appended.
        assert_eq!(
            build_run_argv("{entry} serve --dir {dir}", "/v/app", "/v").unwrap(),
            vec!["/v/app", "serve", "--dir", "/v"]
        );
    }

    #[test]
    fn exec_argv_appends_args_without_entry() {
        assert_eq!(
            build_exec_argv(
                "bun",
                "/v/app",
                "/v",
                &["run".to_owned(), "db:init".to_owned()]
            )
            .unwrap(),
            vec!["bun", "run", "db:init"]
        );
        // exec base "{entry}" runs the binary itself with the args.
        assert_eq!(
            build_exec_argv("{entry}", "/v/app", "/v", &["--flag".to_owned()]).unwrap(),
            vec!["/v/app", "--flag"]
        );
    }

    // --- runtime decision + format inference ---

    #[test]
    fn runtime_plan_decisions() {
        assert_eq!(
            plan_runtime(None, None, false, false).unwrap(),
            RuntimePlan::NotNeeded
        );
        // Already on PATH → skip the download (system runtime wins over cache).
        assert_eq!(
            plan_runtime(Some("bun"), Some("https://x/bun.zip"), true, true).unwrap(),
            RuntimePlan::AlreadyPresent
        );
        // Off PATH but cached from a prior launch → reuse, no network (even if a
        // download URL is set).
        assert_eq!(
            plan_runtime(Some("bun"), Some("https://x/bun.zip"), false, true).unwrap(),
            RuntimePlan::Cached
        );
        // Off PATH, no cache, no download URL, but cached present → still reused.
        assert_eq!(
            plan_runtime(Some("bun"), None, false, true).unwrap(),
            RuntimePlan::Cached
        );
        // Missing + download configured → fetch.
        assert_eq!(
            plan_runtime(Some("bun"), Some("https://x/bun.zip"), false, false).unwrap(),
            RuntimePlan::Fetch
        );
        // Missing everywhere + no download → error.
        assert!(plan_runtime(Some("bun"), None, false, false).is_err());
    }

    #[test]
    fn infer_format_from_suffix() {
        assert_eq!(infer_format("https://x/bun.tar.gz"), "tar.gz");
        assert_eq!(infer_format("https://x/bun.tgz"), "tar.gz");
        assert_eq!(infer_format("https://x/bun.zip?token=1"), "zip");
        assert_eq!(infer_format("https://x/bun.gz"), "gz");
        assert_eq!(infer_format("https://x/bun"), "raw");
    }

    #[test]
    fn on_path_finds_executable() {
        let dir = std::env::temp_dir().join(format!("lode-onpath-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("mytool");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path_var = dir.display().to_string();
        assert!(on_path("mytool", &path_var));
        assert!(!on_path("absent", &path_var));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runtime_probe_args_defaults_to_version_flag() {
        assert_eq!(runtime_probe_args(None), vec!["--version".to_owned()]);
        assert_eq!(runtime_probe_args(Some("  ")), vec!["--version".to_owned()]);
        assert_eq!(runtime_probe_args(Some("-v")), vec!["-v".to_owned()]);
        assert_eq!(
            runtime_probe_args(Some("eval Bun.version")),
            vec!["eval".to_owned(), "Bun.version".to_owned()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn runtime_version_probe_matches_and_rejects() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("lode-rtver-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A stand-in runtime whose `--version` prints "1.1.38" (like bun's output).
        let bin = dir.join("fakert");
        std::fs::write(&bin, b"#!/bin/sh\necho 1.1.38\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let args = runtime_probe_args(None);

        // Substring match handles bare and prefixed (e.g. node's "v22…") forms.
        assert!(runtime_version_ok(bin.as_os_str(), &args, "1.1.38"));
        assert!(runtime_version_ok(bin.as_os_str(), &args, "1.1"));
        assert!(!runtime_version_ok(bin.as_os_str(), &args, "1.2.0"));
        // A binary that can't be executed → not OK, never a panic.
        assert!(!runtime_version_ok(
            dir.join("absent").as_os_str(),
            &args,
            "1.1.38"
        ));

        // verify_runtime_version: ok on match, Err on mismatch / unrunnable.
        assert!(verify_runtime_version(&bin, &args, "1.1.38").is_ok());
        assert!(matches!(
            verify_runtime_version(&bin, &args, "9.9.9"),
            Err(Error::Process(_))
        ));
        assert!(matches!(
            verify_runtime_version(&dir.join("absent"), &args, "1.1.38"),
            Err(Error::Process(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- real-child process helpers (specific-pid; CI-safe) ---

    #[cfg(unix)]
    #[test]
    fn graceful_stop_terminates_a_sleeping_child() {
        // Spawn a long sleep; SIGTERM ends it immediately, well within the timeout.
        let pid = spawn_process(
            &["sleep".to_owned(), "30".to_owned()],
            Path::new("/"),
            &[("PATH".to_owned(), "/usr/bin:/bin".to_owned())],
        )
        .unwrap();
        let status = graceful_stop(pid, Duration::from_secs(5));
        assert!(matches!(
            status,
            Some(WaitStatus::Signaled(_, Signal::SIGTERM, _))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn graceful_stop_reaps_already_exited_child() {
        // A child that exits on its own becomes a zombie until reaped; graceful_stop
        // signals (a no-op for a zombie) then reaps, yielding its real exit code.
        let pid = spawn_process(
            &["sh".to_owned(), "-c".to_owned(), "exit 4".to_owned()],
            Path::new("/"),
            &[("PATH".to_owned(), "/usr/bin:/bin".to_owned())],
        )
        .unwrap();
        // Give it a moment to exit.
        std::thread::sleep(Duration::from_millis(100));
        let status = graceful_stop(pid, Duration::from_secs(5));
        assert!(matches!(status, Some(WaitStatus::Exited(_, 4))));
    }

    // --- C2: semver newer-than comparison ---

    #[test]
    fn is_newer_compares_semver_then_falls_back() {
        assert!(is_newer("1.5.0", "1.4.2"));
        assert!(!is_newer("1.4.2", "1.5.0"));
        // Equal precedence is never newer (no auto-apply loop).
        assert!(!is_newer("1.4.2", "1.4.2"));
        // Semver beats lexicographic: 1.10.0 > 1.9.0.
        assert!(is_newer("1.10.0", "1.9.0"));
        // Non-semver ids: any *different* id counts as newer; identical does not.
        assert!(is_newer("nightly-2", "nightly-1"));
        assert!(!is_newer("nightly-1", "nightly-1"));
    }

    // --- C2: policy decision (off / check / auto + pin) ---

    #[test]
    fn policy_action_off_and_pinned_are_idle() {
        // `off` never acts; a pin forces idle regardless of policy.
        assert_eq!(
            policy_action(Policy::Off, false, "2.0.0", "1.0.0"),
            PolicyAction::Idle
        );
        assert_eq!(
            policy_action(Policy::Auto, true, "2.0.0", "1.0.0"),
            PolicyAction::Idle
        );
        assert_eq!(
            policy_action(Policy::Check, true, "2.0.0", "1.0.0"),
            PolicyAction::Idle
        );
    }

    #[test]
    fn policy_action_check_advertises_and_auto_applies() {
        assert_eq!(
            policy_action(Policy::Check, false, "2.0.0", "1.0.0"),
            PolicyAction::Advertise("2.0.0".to_owned())
        );
        assert_eq!(
            policy_action(Policy::Auto, false, "2.0.0", "1.0.0"),
            PolicyAction::Apply("2.0.0".to_owned())
        );
    }

    #[test]
    fn policy_action_idle_when_not_newer() {
        // Already current → nothing to advertise/apply, for both check and auto.
        assert_eq!(
            policy_action(Policy::Check, false, "1.0.0", "1.0.0"),
            PolicyAction::Idle
        );
        assert_eq!(
            policy_action(Policy::Auto, false, "1.0.0", "2.0.0"),
            PolicyAction::Idle
        );
    }

    // --- C2: readiness gating (none vs state) ---

    #[test]
    fn readiness_none_waits_for_grace() {
        let grace = Duration::from_secs(15);
        assert!(!readiness_met(
            Readiness::None,
            None,
            "p-1",
            Duration::from_secs(5),
            grace
        ));
        assert!(readiness_met(
            Readiness::None,
            None,
            "p-1",
            Duration::from_secs(15),
            grace
        ));
        // `none` ignores any app-written ready value.
        assert!(readiness_met(
            Readiness::None,
            Some("anything"),
            "p-1",
            Duration::from_secs(20),
            grace
        ));
    }

    #[test]
    fn readiness_state_matches_this_instance_only() {
        let grace = Duration::from_secs(15);
        // Matches only when the app reported *this* spawn's instance id; uptime
        // does not matter in `state` mode.
        assert!(readiness_met(
            Readiness::State,
            Some("p-2"),
            "p-2",
            Duration::from_secs(0),
            grace
        ));
        // A stale ready from a previous instance does not count.
        assert!(!readiness_met(
            Readiness::State,
            Some("p-1"),
            "p-2",
            Duration::from_secs(99),
            grace
        ));
        assert!(!readiness_met(
            Readiness::State,
            None,
            "p-2",
            Duration::from_secs(99),
            grace
        ));
    }

    #[test]
    fn random_boot_key_is_8_lowercase_hex() {
        let k = random_boot_key();
        assert_eq!(k.len(), 8, "boot key should be 8 hex chars: {k:?}");
        assert!(
            k.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "boot key must be lowercase hex: {k:?}"
        );
    }

    #[test]
    fn clear_stale_ready_drops_ready_keeps_the_rest() {
        let dir = std::env::temp_dir().join(format!("lode-clearready-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");

        // A prior run left `current` plus a now-stale readiness handshake.
        let st = State {
            current: Some("0.0.1".to_owned()),
            ready: Some("12345-deadbeef-2".to_owned()),
            ..State::default()
        };
        state::write(&path, &st).unwrap();

        clear_stale_ready(&path).unwrap();

        let back = state::read(&path).unwrap().unwrap();
        assert_eq!(back.ready, None, "stale ready must be cleared");
        assert_eq!(
            back.current.as_deref(),
            Some("0.0.1"),
            "other fields must be preserved"
        );

        // Idempotent: a second pass on already-clear state succeeds as a no-op.
        clear_stale_ready(&path).unwrap();
        assert_eq!(state::read(&path).unwrap().unwrap().ready, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- C3: restart-policy exit decision ---

    #[test]
    fn is_failure_only_clean_exit_is_success() {
        let pid = Pid::from_raw(1);
        assert!(!is_failure(WaitStatus::Exited(pid, 0)));
        assert!(is_failure(WaitStatus::Exited(pid, 1)));
        assert!(is_failure(WaitStatus::Signaled(
            pid,
            Signal::SIGKILL,
            false
        )));
    }

    #[test]
    fn exit_action_off_always_mirrors_child() {
        let pid = Pid::from_raw(1);
        // restart=off: a clean exit and a crash both exit lode with the code.
        assert_eq!(
            exit_action(
                RestartPolicy::Off,
                WaitStatus::Exited(pid, 0),
                None,
                0,
                0,
                500,
                30_000
            ),
            ExitAction::Exit {
                code: 0,
                gave_up: false
            }
        );
        assert_eq!(
            exit_action(
                RestartPolicy::Off,
                WaitStatus::Exited(pid, 7),
                None,
                0,
                0,
                500,
                30_000
            ),
            ExitAction::Exit {
                code: 7,
                gave_up: false
            }
        );
    }

    #[test]
    fn exit_action_on_failure_mirrors_clean_restarts_crash() {
        let pid = Pid::from_raw(1);
        // Clean exit → mirror (exit 0).
        assert_eq!(
            exit_action(
                RestartPolicy::OnFailure,
                WaitStatus::Exited(pid, 0),
                None,
                0,
                0,
                500,
                30_000
            ),
            ExitAction::Exit {
                code: 0,
                gave_up: false
            }
        );
        // Crash → restart with the base backoff.
        assert_eq!(
            exit_action(
                RestartPolicy::OnFailure,
                WaitStatus::Signaled(pid, Signal::SIGKILL, false),
                None,
                0,
                0,
                500,
                30_000
            ),
            ExitAction::Restart(Duration::from_millis(500))
        );
    }

    #[test]
    fn exit_action_always_restarts_then_gives_up_at_cap() {
        let pid = Pid::from_raw(1);
        // Clean exit still restarts; backoff doubles with the consecutive count.
        assert_eq!(
            exit_action(
                RestartPolicy::Always,
                WaitStatus::Exited(pid, 0),
                None,
                0,
                0,
                500,
                30_000
            ),
            ExitAction::Restart(Duration::from_millis(500))
        );
        assert_eq!(
            exit_action(
                RestartPolicy::Always,
                WaitStatus::Exited(pid, 0),
                None,
                2,
                0,
                500,
                30_000
            ),
            ExitAction::Restart(Duration::from_secs(2))
        );
        // restart_max=2 allows 2 restarts; the 3rd exit (restarts already 2) gives
        // up and exits with the child's code and a terminal (error) status.
        assert_eq!(
            exit_action(
                RestartPolicy::Always,
                WaitStatus::Exited(pid, 3),
                None,
                2,
                2,
                500,
                30_000
            ),
            ExitAction::Exit {
                code: 3,
                gave_up: true
            }
        );
    }

    #[test]
    fn exit_action_pending_update_wins_over_policy() {
        let pid = Pid::from_raw(1);
        // A pending different target applies the update regardless of policy —
        // even restart=off (mirror) and even past the restart cap with always.
        assert_eq!(
            exit_action(
                RestartPolicy::Off,
                WaitStatus::Exited(pid, 0),
                Some("2.0.0"),
                0,
                0,
                500,
                30_000
            ),
            ExitAction::ApplyUpdate("2.0.0".to_owned())
        );
        assert_eq!(
            exit_action(
                RestartPolicy::Always,
                WaitStatus::Exited(pid, 0),
                Some("2.0.0"),
                9,
                2,
                500,
                30_000
            ),
            ExitAction::ApplyUpdate("2.0.0".to_owned())
        );
    }

    // --- C2/C3: target-application observation state transitions ---

    #[test]
    fn observe_decision_commit_rollback_pending() {
        // Ready wins, even if it also timed out.
        assert_eq!(observe_decision(true, true), ObserveOutcome::Commit);
        assert_eq!(observe_decision(true, false), ObserveOutcome::Commit);
        // Not ready + timed out → rollback (single-strike).
        assert_eq!(observe_decision(false, true), ObserveOutcome::Rollback);
        // Not ready, no timeout → keep waiting.
        assert_eq!(observe_decision(false, false), ObserveOutcome::Pending);
    }

    // --- C2: history append + cap ---

    #[test]
    fn push_history_appends_and_caps() {
        let mut history = Vec::new();
        push_history(&mut history, "1.0.0", HistoryResult::Good, "t0".to_owned());
        push_history(&mut history, "1.1.0", HistoryResult::Bad, "t1".to_owned());
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].version, "1.0.0");
        assert_eq!(history[1].result, HistoryResult::Bad);

        // Exceeding the cap drops the oldest entries first.
        for i in 0..HISTORY_CAP {
            push_history(
                &mut history,
                &format!("9.0.{i}"),
                HistoryResult::Good,
                "t".to_owned(),
            );
        }
        assert_eq!(history.len(), HISTORY_CAP);
        // The two seed entries fell off the front.
        assert_eq!(history[0].version, "9.0.0");
    }

    // --- C2: timestamp formatting ---

    #[test]
    fn format_rfc3339_known_epochs() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        // A leap-day instant: 2024-02-29T12:00:00Z.
        assert_eq!(format_rfc3339(1_709_208_000), "2024-02-29T12:00:00Z");
    }

    #[test]
    fn readiness_label_maps_modes() {
        assert_eq!(readiness_label(Readiness::None), "none");
        assert_eq!(readiness_label(Readiness::State), "state");
    }
}
