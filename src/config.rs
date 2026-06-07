//! Configuration: the resolved [`Config`] plus its loader.
//!
//! Precedence is `CLI > env (LODE_*) > lode.toml > default` (design §10). clap
//! folds env into each CLI field (every global arg carries `env = "LODE_…"`), so
//! [`merge`] sees just two layers — the CLI/env slot and the parsed TOML — and
//! the design's default table fills the rest. CLI-over-env within the first slot
//! is clap's contract; this module owns env/toml-over-default.
//!
//! Header values and trusted keys are stored verbatim and never expanded or
//! logged here; `${ENV}` expansion happens at fetch time in the http module.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::cli::Globals;
use crate::error::{Error, Result};

const DEFAULT_LOG_LEVEL: &str = "info";
const DEFAULT_APP: &str = "app";
/// Default base / run directory. Holds `lode.toml`, `versions/`, `state.json`,
/// `lode.pid` and `runtime/`. Change the whole location with `--data-dir` /
/// `LODE_DATA_DIR` (config is then searched at `$DATA_DIR/lode.toml`).
const DEFAULT_DATA_DIR: &str = "/srv/lode";

/// Starter `lode.toml` scaffolded on first run when none exists (also what
/// `lode-cli init` writes). Kept in sync with the documented example.
pub(crate) const STARTER_TOML: &str = include_str!("../docs/lode.example.toml");
const DEFAULT_GITHUB_API: &str = "https://api.github.com";
const DEFAULT_CHANNEL: &str = "stable";
const DEFAULT_ENTRY_PLACEHOLDER: &str = "{entry}";
const DEFAULT_WORKDIR_PLACEHOLDER: &str = "{dir}";

// --- typed enums (shared by the CLI and TOML layers) -----------------------

/// Update policy (`update.policy`), design §5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Policy {
    /// No background checks; run current/pinned only.
    Off,
    /// Periodically check and advertise, but never auto-apply (default).
    Check,
    /// Periodically check and auto-apply newer versions.
    Auto,
}

/// Signature-enforcement strength (`trust.require_signature`), design §6.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RequireSignature {
    /// Integrity only (sha256), no signature check.
    Off,
    /// Enforce when keys are configured, else warn-and-skip (default).
    Auto,
    /// Always require a valid signature.
    Enforce,
}

/// Readiness determination (`supervise.readiness`), design §8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Readiness {
    /// Alive for `health_grace` seconds counts as ready (default).
    None,
    /// Wait for the app to write `state.ready`.
    State,
}

/// Crash-restart policy (`supervise.restart`), design §8. Gates the bounded
/// backoff machinery; the default `off` makes lode mirror the child's lifecycle.
/// Update / rollback / explicit-restart relaunches happen regardless of policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RestartPolicy {
    /// Never restart on the child's own exit — lode exits with the child's code
    /// (default). The orchestrator owns whole-process restart.
    Off,
    /// Restart only when the child fails (non-zero exit or killed by a signal);
    /// a clean `exit(0)` makes lode exit too.
    OnFailure,
    /// Restart on any child exit (clean or failed).
    Always,
}

/// Restart strategy (`supervise.restart_mode`), design §8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RestartMode {
    /// Stop the old child before starting the new one (default).
    StopStart,
    /// systemd socket-activation protocol; needs `listen`.
    SocketActivation,
    /// Overlap old and new via `SO_REUSEPORT`.
    ReuseportOverlap,
}

// --- resolved config (the 8 sections of design §10) ------------------------

/// Fully resolved lode configuration.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) global: Global,
    pub(crate) update: Update,
    pub(crate) http: Http,
    pub(crate) trust: Trust,
    pub(crate) command: Command,
    pub(crate) runtime: Runtime,
    pub(crate) supervise: Supervise,
    pub(crate) signals: Signals,
    /// `[env]` — extra environment variables injected into the child (on top of
    /// the inherited host env; lode's own `LODE_*` introspection vars still win).
    pub(crate) env: BTreeMap<String, String>,
}

/// `[global]` — identity and storage.
#[derive(Debug, Clone)]
pub(crate) struct Global {
    pub(crate) app: String,
    pub(crate) data_dir: PathBuf,
    pub(crate) log_level: String,
}

/// `[update]` — source and upgrade policy.
#[derive(Debug, Clone)]
pub(crate) struct Update {
    /// native source (mutually exclusive with [`Self::github`]).
    pub(crate) manifest: Option<String>,
    /// github source (mutually exclusive with [`Self::manifest`]).
    pub(crate) github: Option<String>,
    pub(crate) github_api: String,
    /// The asset filename to install on this host — the source-agnostic selection
    /// key (source-adapters §3/§7). Required to resolve a download.
    pub(crate) asset: Option<String>,
    /// Override the in-archive entry path (source-adapters §4); usually omitted.
    pub(crate) entry: Option<String>,
    pub(crate) channel: String,
    pub(crate) policy: Policy,
    pub(crate) check_interval: u64,
    pub(crate) keep_versions: u32,
    pub(crate) pin: Option<String>,
}

/// `[http]` — fetch credentials. Values are stored raw (never expanded/logged).
#[derive(Debug, Clone)]
pub(crate) struct Http {
    pub(crate) headers: Vec<String>,
    /// Extra hosts (beyond the manifest/source origin) that may receive
    /// [`Self::headers`] on an artifact download. Empty by default; same-origin
    /// is always allowed (see [`crate::download::fetch_artifact`]).
    pub(crate) credential_hosts: Vec<String>,
    /// Permit non-HTTPS (plain `http`) remote fetches. Off by default; loopback
    /// http is always allowed regardless. See [`crate::http`].
    pub(crate) allow_insecure: bool,
}

/// `[trust]` — publisher-identity verification.
#[derive(Debug, Clone)]
pub(crate) struct Trust {
    pub(crate) require_signature: RequireSignature,
    pub(crate) trusted_keys: Vec<String>,
    pub(crate) trusted_keys_file: Option<String>,
}

/// `[command]` — how to launch the app.
#[derive(Debug, Clone)]
pub(crate) struct Command {
    pub(crate) run: String,
    pub(crate) exec: String,
    pub(crate) workdir: String,
}

/// `[runtime]` — optional runtime dependency.
// `runtime` mirrors the `[runtime] runtime = "…"` TOML key, so the field name is
// fixed by the schema even though it repeats the struct name.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone)]
pub(crate) struct Runtime {
    pub(crate) runtime: Option<String>,
    pub(crate) download: Option<String>,
    /// Expected runtime version. When set, lode probes the runtime (PATH, cache,
    /// or freshly downloaded) and requires its self-reported version to *contain*
    /// this string; a wrong-version PATH/cache entry is bypassed for a fresh
    /// download, and a downloaded mismatch is a hard error.
    pub(crate) version: Option<String>,
    /// Argument(s) that make the runtime print its version (whitespace-split,
    /// appended to the runtime binary). Defaults to `--version`. Only used when
    /// [`version`](Self::version) is set.
    pub(crate) version_check: Option<String>,
}

/// `[supervise]` — restart policy / health / rollback / stop / restart mode.
#[derive(Debug, Clone)]
pub(crate) struct Supervise {
    /// Crash-restart policy. `off` (default) mirrors the child; `on-failure` /
    /// `always` enable the bounded backoff below.
    pub(crate) restart: RestartPolicy,
    /// Backoff base/cap and consecutive-restart cap — only used when
    /// [`restart`](Self::restart) is not `off`.
    pub(crate) restart_backoff: u64,
    pub(crate) restart_backoff_max: u64,
    pub(crate) restart_max: u32,
    pub(crate) readiness: Readiness,
    pub(crate) ready_timeout: u64,
    pub(crate) health_grace: u64,
    pub(crate) stop_timeout: u64,
    pub(crate) restart_mode: RestartMode,
    pub(crate) listen: Option<String>,
}

/// `[signals]` — signal forwarding. Empty `forward` => lode's standard set (§8).
#[derive(Debug, Clone)]
pub(crate) struct Signals {
    pub(crate) forward: Vec<String>,
    pub(crate) restart: Option<String>,
}

// --- raw TOML layer (everything optional) ----------------------------------

#[derive(Debug, Default, Deserialize)]
struct TomlConfig {
    #[serde(default)]
    global: TomlGlobal,
    #[serde(default)]
    update: TomlUpdate,
    #[serde(default)]
    http: TomlHttp,
    #[serde(default)]
    trust: TomlTrust,
    #[serde(default)]
    command: TomlCommand,
    #[serde(default)]
    runtime: TomlRuntime,
    #[serde(default)]
    supervise: TomlSupervise,
    #[serde(default)]
    signals: TomlSignals,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
struct TomlGlobal {
    app: Option<String>,
    data_dir: Option<String>,
    log_level: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct TomlUpdate {
    manifest: Option<String>,
    github: Option<String>,
    github_api: Option<String>,
    asset: Option<String>,
    entry: Option<String>,
    channel: Option<String>,
    policy: Option<Policy>,
    check_interval: Option<u64>,
    keep_versions: Option<u32>,
    pin: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct TomlHttp {
    headers: Option<Vec<String>>,
    credential_hosts: Option<Vec<String>>,
    allow_insecure: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct TomlTrust {
    require_signature: Option<RequireSignature>,
    trusted_keys: Option<Vec<String>>,
    trusted_keys_file: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct TomlCommand {
    run: Option<String>,
    exec: Option<String>,
    workdir: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct TomlRuntime {
    runtime: Option<String>,
    download: Option<String>,
    version: Option<String>,
    version_check: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct TomlSupervise {
    restart: Option<RestartPolicy>,
    restart_backoff: Option<u64>,
    restart_backoff_max: Option<u64>,
    restart_max: Option<u32>,
    readiness: Option<Readiness>,
    ready_timeout: Option<u64>,
    health_grace: Option<u64>,
    stop_timeout: Option<u64>,
    restart_mode: Option<RestartMode>,
    listen: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct TomlSignals {
    forward: Option<Vec<String>>,
    restart: Option<String>,
}

// --- resolution ------------------------------------------------------------

/// Resolve the effective configuration from CLI/env (`cli`), `lode.toml` and the
/// design defaults, then validate it.
pub(crate) fn resolve(cli: &Globals) -> Result<Config> {
    let toml = load_toml(cli)?;
    let cfg = merge(cli, &toml);
    validate(&cfg)?;
    Ok(cfg)
}

/// Locate and parse `lode.toml`. An explicit `--config`/`LODE_CONFIG` must exist;
/// otherwise the default search (`$DATA_DIR/lode.toml`, then `./lode.toml`) is
/// best-effort and a missing file yields the all-defaults config (design §15).
fn load_toml(cli: &Globals) -> Result<TomlConfig> {
    if let Some(path) = cli.config.as_ref() {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("read config {path}: {e}")))?;
        return Ok(toml::from_str(&text)?);
    }
    let data_dir = cli.data_dir.as_deref().unwrap_or(DEFAULT_DATA_DIR);
    let in_data = Path::new(data_dir).join("lode.toml");
    let default_path = if in_data.is_file() {
        in_data
    } else if Path::new("lode.toml").is_file() {
        PathBuf::from("lode.toml")
    } else {
        // No lode.toml anywhere. A source given via env/CLI lets us run file-less;
        // otherwise scaffold a starter at `$DATA_DIR/lode.toml` and guide the
        // operator to fill it in (design §15).
        if cli.manifest.is_some() || cli.github.is_some() {
            return Ok(TomlConfig::default());
        }
        return Err(scaffold_starter_config(&in_data));
    };
    let text = std::fs::read_to_string(&default_path)?;
    Ok(toml::from_str(&text)?)
}

/// First-run convenience: write a starter `lode.toml` at `path` (best-effort,
/// creating parent dirs) and return a guiding error so the loader stops cleanly
/// instead of failing later on a placeholder source.
fn scaffold_starter_config(path: &Path) -> Error {
    if !path.exists() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::write(path, STARTER_TOML) {
            Ok(()) => {
                tracing::info!(path = %path.display(), "no lode.toml found — wrote a starter config");
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "could not write starter lode.toml");
            }
        }
    }
    Error::Config(format!(
        "no lode.toml found — wrote a starter to {}; set [update].manifest (or [update].github) \
         to your real source and re-run, or pass --manifest/--github (LODE_MANIFEST/LODE_GITHUB)",
        path.display()
    ))
}

/// Merge the CLI/env layer over the TOML layer over the defaults. Infallible;
/// semantic checks live in [`validate`].
fn merge(cli: &Globals, t: &TomlConfig) -> Config {
    Config {
        global: merge_global(cli, &t.global),
        update: merge_update(cli, &t.update),
        http: merge_http(cli, &t.http),
        trust: merge_trust(cli, &t.trust),
        command: merge_command(cli, &t.command),
        runtime: merge_runtime(cli, &t.runtime),
        supervise: merge_supervise(cli, &t.supervise),
        signals: merge_signals(cli, &t.signals),
        // `[env]` is config-file only — no CLI/env override layer. To override an
        // entry at deploy time, set it directly in the process env (it wins as a
        // host env var; see `child_env`).
        env: t.env.clone(),
    }
}

fn merge_global(cli: &Globals, t: &TomlGlobal) -> Global {
    // `--log-level` keeps its clap default of "info"; fall back to the TOML value
    // only when the CLI/env slot is still at that default.
    let log_level = if cli.log_level == DEFAULT_LOG_LEVEL {
        t.log_level
            .clone()
            .unwrap_or_else(|| DEFAULT_LOG_LEVEL.to_owned())
    } else {
        cli.log_level.clone()
    };
    Global {
        app: cli
            .app
            .clone()
            .or_else(|| t.app.clone())
            .unwrap_or_else(|| DEFAULT_APP.to_owned()),
        data_dir: cli
            .data_dir
            .clone()
            .or_else(|| t.data_dir.clone())
            .map_or_else(|| PathBuf::from(DEFAULT_DATA_DIR), PathBuf::from),
        log_level,
    }
}

fn merge_update(cli: &Globals, t: &TomlUpdate) -> Update {
    Update {
        manifest: cli.manifest.clone().or_else(|| t.manifest.clone()),
        github: cli.github.clone().or_else(|| t.github.clone()),
        github_api: cli
            .github_api
            .clone()
            .or_else(|| t.github_api.clone())
            .unwrap_or_else(|| DEFAULT_GITHUB_API.to_owned()),
        asset: cli.asset.clone().or_else(|| t.asset.clone()),
        entry: cli.entry.clone().or_else(|| t.entry.clone()),
        channel: cli
            .channel
            .clone()
            .or_else(|| t.channel.clone())
            .unwrap_or_else(|| DEFAULT_CHANNEL.to_owned()),
        policy: cli.policy.or(t.policy).unwrap_or(Policy::Check),
        check_interval: cli.interval.or(t.check_interval).unwrap_or(300),
        keep_versions: cli.keep.or(t.keep_versions).unwrap_or(3),
        pin: cli.pin.clone().or_else(|| t.pin.clone()),
    }
}

fn merge_http(cli: &Globals, t: &TomlHttp) -> Http {
    let headers = if cli.header.is_empty() {
        t.headers.clone().unwrap_or_default()
    } else {
        cli.header.clone()
    };
    let credential_hosts = if cli.credential_host.is_empty() {
        t.credential_hosts.clone().unwrap_or_default()
    } else {
        cli.credential_host.clone()
    };
    // `--allow-insecure-http` is a one-way switch (it can only turn the gate on),
    // so CLI-true wins; otherwise fall back to the TOML value, default false.
    let allow_insecure = cli.allow_insecure_http || t.allow_insecure.unwrap_or(false);
    Http {
        headers,
        credential_hosts,
        allow_insecure,
    }
}

fn merge_trust(cli: &Globals, t: &TomlTrust) -> Trust {
    let trusted_keys = cli.trusted_keys.as_ref().map_or_else(
        || t.trusted_keys.clone().unwrap_or_default(),
        |list| split_csv(list),
    );
    Trust {
        require_signature: cli
            .require_signature
            .or(t.require_signature)
            .unwrap_or(RequireSignature::Auto),
        trusted_keys,
        trusted_keys_file: cli
            .trusted_keys_file
            .clone()
            .or_else(|| t.trusted_keys_file.clone()),
    }
}

fn merge_command(cli: &Globals, t: &TomlCommand) -> Command {
    Command {
        run: cli
            .run
            .clone()
            .or_else(|| t.run.clone())
            .unwrap_or_else(|| DEFAULT_ENTRY_PLACEHOLDER.to_owned()),
        exec: cli
            .exec
            .clone()
            .or_else(|| t.exec.clone())
            .unwrap_or_else(|| DEFAULT_ENTRY_PLACEHOLDER.to_owned()),
        workdir: cli
            .workdir
            .clone()
            .or_else(|| t.workdir.clone())
            .unwrap_or_else(|| DEFAULT_WORKDIR_PLACEHOLDER.to_owned()),
    }
}

fn merge_runtime(cli: &Globals, t: &TomlRuntime) -> Runtime {
    Runtime {
        runtime: cli.runtime.clone().or_else(|| t.runtime.clone()),
        download: cli.runtime_download.clone().or_else(|| t.download.clone()),
        version: cli.runtime_version.clone().or_else(|| t.version.clone()),
        version_check: cli
            .runtime_version_check
            .clone()
            .or_else(|| t.version_check.clone()),
    }
}

fn merge_supervise(cli: &Globals, t: &TomlSupervise) -> Supervise {
    Supervise {
        restart: cli.restart.or(t.restart).unwrap_or(RestartPolicy::Off),
        restart_backoff: cli.restart_backoff.or(t.restart_backoff).unwrap_or(500),
        restart_backoff_max: cli
            .restart_backoff_max
            .or(t.restart_backoff_max)
            .unwrap_or(30_000),
        restart_max: cli.restart_max.or(t.restart_max).unwrap_or(0),
        readiness: cli.readiness.or(t.readiness).unwrap_or(Readiness::None),
        ready_timeout: cli.ready_timeout.or(t.ready_timeout).unwrap_or(30),
        health_grace: cli.health_grace.or(t.health_grace).unwrap_or(15),
        stop_timeout: cli.stop_timeout.or(t.stop_timeout).unwrap_or(10),
        restart_mode: cli
            .restart_mode
            .or(t.restart_mode)
            .unwrap_or(RestartMode::StopStart),
        listen: cli.listen.clone().or_else(|| t.listen.clone()),
    }
}

fn merge_signals(cli: &Globals, t: &TomlSignals) -> Signals {
    let forward = cli.forward_signals.as_ref().map_or_else(
        || t.forward.clone().unwrap_or_default(),
        |list| split_csv(list),
    );
    Signals {
        forward,
        restart: cli.restart_signal.clone().or_else(|| t.restart.clone()),
    }
}

/// Split a comma-separated list, trimming whitespace and dropping empties.
fn split_csv(list: &str) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// Semantic validation: source XOR, and numeric ranges. (Enum values are already
/// checked by clap/serde at parse time.)
fn validate(cfg: &Config) -> Result<()> {
    match (&cfg.update.manifest, &cfg.update.github) {
        (Some(_), Some(_)) => {
            return Err(Error::Config(
                "update.manifest and update.github are mutually exclusive (set exactly one)"
                    .to_owned(),
            ));
        }
        (None, None) => {
            tracing::debug!("no update source configured (neither manifest nor github set)");
        }
        _ => {}
    }
    if cfg.supervise.restart_backoff_max < cfg.supervise.restart_backoff {
        return Err(Error::Config(format!(
            "supervise.restart_backoff_max ({}) must be >= restart_backoff ({})",
            cfg.supervise.restart_backoff_max, cfg.supervise.restart_backoff
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Globals` with no flags set (the all-`None` / default-`log_level` baseline),
    /// so [`merge`] sees an empty CLI/env layer.
    fn blank_cli() -> Globals {
        Globals {
            log_level: DEFAULT_LOG_LEVEL.to_owned(),
            config: None,
            app: None,
            data_dir: None,
            manifest: None,
            github: None,
            github_api: None,
            asset: None,
            entry: None,
            channel: None,
            policy: None,
            interval: None,
            keep: None,
            pin: None,
            header: Vec::new(),
            credential_host: Vec::new(),
            allow_insecure_http: false,
            require_signature: None,
            trusted_keys: None,
            trusted_keys_file: None,
            run: None,
            exec: None,
            workdir: None,
            runtime: None,
            runtime_download: None,
            runtime_version: None,
            runtime_version_check: None,
            restart: None,
            restart_backoff: None,
            restart_backoff_max: None,
            restart_max: None,
            readiness: None,
            ready_timeout: None,
            health_grace: None,
            stop_timeout: None,
            restart_mode: None,
            listen: None,
            forward_signals: None,
            restart_signal: None,
        }
    }

    #[test]
    fn default_fallback() {
        let cfg = merge(&blank_cli(), &TomlConfig::default());
        assert_eq!(cfg.global.app, "app");
        assert_eq!(cfg.global.data_dir, PathBuf::from("/srv/lode"));
        assert_eq!(cfg.global.log_level, "info");
        assert_eq!(cfg.update.policy, Policy::Check);
        assert_eq!(cfg.update.check_interval, 300);
        assert_eq!(cfg.update.keep_versions, 3);
        assert_eq!(cfg.update.channel, "stable");
        assert_eq!(cfg.update.github_api, "https://api.github.com");
        assert_eq!(cfg.trust.require_signature, RequireSignature::Auto);
        assert_eq!(cfg.command.run, "{entry}");
        assert_eq!(cfg.command.workdir, "{dir}");
        assert_eq!(cfg.supervise.readiness, Readiness::None);
        assert_eq!(cfg.supervise.restart, RestartPolicy::Off);
        assert_eq!(cfg.supervise.restart_mode, RestartMode::StopStart);
        assert_eq!(cfg.supervise.restart_backoff, 500);
        assert!(cfg.http.headers.is_empty());
        assert!(cfg.http.credential_hosts.is_empty());
        assert!(!cfg.http.allow_insecure); // secure by default
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn credential_hosts_from_toml_then_cli_overrides() {
        // TOML supplies the allowlist when the CLI slot is empty…
        let t = TomlHttp {
            credential_hosts: Some(vec!["cdn.example".to_owned()]),
            ..TomlHttp::default()
        };
        assert_eq!(
            merge_http(&blank_cli(), &t).credential_hosts,
            vec!["cdn.example".to_owned()]
        );
        // …and `--credential-host` (repeatable) overrides it entirely.
        let mut cli = blank_cli();
        cli.credential_host = vec!["a.example".to_owned(), "b.example".to_owned()];
        assert_eq!(
            merge_http(&cli, &t).credential_hosts,
            vec!["a.example".to_owned(), "b.example".to_owned()]
        );
    }

    #[test]
    fn allow_insecure_http_precedence() {
        // Default: the gate is off.
        assert!(
            !merge(&blank_cli(), &TomlConfig::default())
                .http
                .allow_insecure
        );

        // TOML opts in.
        let t = TomlConfig {
            http: TomlHttp {
                allow_insecure: Some(true),
                ..TomlHttp::default()
            },
            ..TomlConfig::default()
        };
        assert!(merge(&blank_cli(), &t).http.allow_insecure);

        // The CLI flag forces it on even when TOML is unset/false.
        let mut cli = blank_cli();
        cli.allow_insecure_http = true;
        assert!(merge(&cli, &TomlConfig::default()).http.allow_insecure);
    }

    #[test]
    fn toml_only() {
        let t = TomlConfig {
            global: TomlGlobal {
                app: Some("myapp".to_owned()),
                ..TomlGlobal::default()
            },
            update: TomlUpdate {
                policy: Some(Policy::Auto),
                check_interval: Some(60),
                manifest: Some("https://example.com/m.json".to_owned()),
                ..TomlUpdate::default()
            },
            trust: TomlTrust {
                trusted_keys: Some(vec!["id:key".to_owned()]),
                ..TomlTrust::default()
            },
            command: TomlCommand {
                run: Some("bun run".to_owned()),
                ..TomlCommand::default()
            },
            ..TomlConfig::default()
        };
        let cfg = merge(&blank_cli(), &t);
        assert_eq!(cfg.global.app, "myapp");
        assert_eq!(cfg.update.policy, Policy::Auto);
        assert_eq!(cfg.update.check_interval, 60);
        assert_eq!(
            cfg.update.manifest.as_deref(),
            Some("https://example.com/m.json")
        );
        assert_eq!(cfg.trust.trusted_keys, vec!["id:key".to_owned()]);
        assert_eq!(cfg.command.run, "bun run");
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn cli_overrides_toml() {
        // The CLI/env slot (clap folds env into these same fields) wins over TOML;
        // CLI-over-env within the slot is clap's own contract.
        let t = TomlConfig {
            global: TomlGlobal {
                app: Some("from_toml".to_owned()),
                ..TomlGlobal::default()
            },
            update: TomlUpdate {
                policy: Some(Policy::Auto),
                check_interval: Some(60),
                ..TomlUpdate::default()
            },
            ..TomlConfig::default()
        };

        let mut cli = blank_cli();
        cli.policy = Some(Policy::Off);
        cli.interval = Some(9);
        cli.app = Some("from_cli".to_owned());

        let cfg = merge(&cli, &t);
        assert_eq!(cfg.update.policy, Policy::Off);
        assert_eq!(cfg.update.check_interval, 9);
        assert_eq!(cfg.global.app, "from_cli");
    }

    #[test]
    fn parses_partial_toml_with_defaulted_sections() {
        // Missing sections and missing keys must default rather than error, so a
        // sparse lode.toml is valid.
        let parsed: TomlConfig =
            toml::from_str("[update]\npolicy = \"auto\"\nmanifest = \"https://x/m.json\"\n")
                .unwrap();
        let cfg = merge(&blank_cli(), &parsed);
        assert_eq!(cfg.update.policy, Policy::Auto);
        assert_eq!(cfg.update.manifest.as_deref(), Some("https://x/m.json"));
        assert_eq!(cfg.global.app, "app"); // default, no [global] table present
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn parses_example_toml() {
        // The shipped example must round-trip through the parser + merge cleanly.
        let text = include_str!("../docs/lode.example.toml");
        let parsed: TomlConfig = toml::from_str(text).unwrap();
        let cfg = merge(&blank_cli(), &parsed);
        assert_eq!(cfg.global.app, "myapp");
        assert_eq!(cfg.update.policy, Policy::Check);
        assert_eq!(cfg.command.exec, "bun");
        assert_eq!(cfg.supervise.restart_mode, RestartMode::StopStart);
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn manifest_github_xor_rejected() {
        let mut cli = blank_cli();
        cli.manifest = Some("https://example.com/m.json".to_owned());
        cli.github = Some("owner/name".to_owned());
        let cfg = merge(&cli, &TomlConfig::default());
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn backoff_range_rejected() {
        let mut cli = blank_cli();
        cli.restart_backoff = Some(1000);
        cli.restart_backoff_max = Some(500);
        let cfg = merge(&cli, &TomlConfig::default());
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn split_csv_trims_and_drops_empties() {
        assert_eq!(split_csv(" a , b ,, c "), vec!["a", "b", "c"]);
        assert!(split_csv("").is_empty());
    }
}
