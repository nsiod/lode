//! Command-line surface (clap).
//!
//! lode is a **multi-call binary** (the [`crate::run`] entry dispatches on the
//! program name):
//!
//! - invoked as **`lode`** → the loader. It has **no subcommands**: bare `lode`
//!   starts and supervises the app; `lode <args>` is transparent passthrough
//!   (exec-replace into the app). See [`LoaderCli`].
//! - invoked as **`lode-cli`** (a symlink to the same binary) → the operator /
//!   publisher multitool: management (`status`/`update`/…) and authoring
//!   (`keygen`/`sign`/`verify`/`manifest`/`init`). See [`ToolCli`].
//!
//! [`Globals`] (shared options) fall back to `LODE_*` env vars; the full
//! precedence (CLI > env > TOML > default) is resolved in [`crate::config`].

use clap::{Args, Parser, Subcommand};

use crate::config::{Policy, Readiness, RequireSignature, RestartMode, RestartPolicy};

/// Shared options for both the loader and `lode-cli`. Every option is `global`
/// and falls back to its `LODE_*` env var.
#[derive(Debug, Args)]
pub(crate) struct Globals {
    /// Log level: trace | debug | info | warn | error.
    #[arg(
        long = "log-level",
        env = "LODE_LOG_LEVEL",
        default_value = "info",
        global = true
    )]
    pub(crate) log_level: String,

    /// Path to the `lode.toml` config file (TOML).
    #[arg(long = "config", env = "LODE_CONFIG", global = true)]
    pub(crate) config: Option<String>,

    // --- [global] ---
    /// Application name (namespaces the data dir + lock; matches the manifest `name`).
    #[arg(long = "app", env = "LODE_APP_NAME", global = true)]
    pub(crate) app: Option<String>,
    /// Data directory holding versions/, `state.json` and the PID lock.
    #[arg(long = "data-dir", env = "LODE_DATA_DIR", global = true)]
    pub(crate) data_dir: Option<String>,

    // --- [update] ---
    /// Native source: lode/v1 manifest URL (mutually exclusive with `--github`).
    #[arg(long = "manifest", env = "LODE_MANIFEST", global = true)]
    pub(crate) manifest: Option<String>,
    /// GitHub source: owner/name (mutually exclusive with `--manifest`).
    #[arg(long = "github", env = "LODE_GITHUB", global = true)]
    pub(crate) github: Option<String>,
    /// GitHub API base URL (for GitHub Enterprise).
    #[arg(long = "github-api", env = "LODE_GITHUB_API", global = true)]
    pub(crate) github_api: Option<String>,
    /// Asset filename to install on this host (the source-agnostic selection key).
    #[arg(long = "asset", env = "LODE_ASSET", global = true)]
    pub(crate) asset: Option<String>,
    /// Override the in-archive entry path (advisory; usually omitted).
    #[arg(long = "entry", env = "LODE_ENTRY", global = true)]
    pub(crate) entry: Option<String>,
    /// Channel to follow.
    #[arg(long = "channel", env = "LODE_CHANNEL", global = true)]
    pub(crate) channel: Option<String>,
    /// Update policy: off | check | auto.
    #[arg(long = "policy", env = "LODE_UPDATE_POLICY", global = true)]
    pub(crate) policy: Option<Policy>,
    /// Check interval in seconds (0 = check once at startup).
    #[arg(long = "interval", env = "LODE_CHECK_INTERVAL", global = true)]
    pub(crate) interval: Option<u64>,
    /// Number of old versions to keep.
    #[arg(long = "keep", env = "LODE_KEEP_VERSIONS", global = true)]
    pub(crate) keep: Option<u32>,
    /// Pin to a specific version/tag (disables auto-update).
    #[arg(long = "pin", env = "LODE_PIN_VERSION", global = true)]
    pub(crate) pin: Option<String>,

    // --- [http] ---
    /// HTTP header passed to downloads (`Name: Value`); repeatable.
    #[arg(
        long = "header",
        env = "LODE_HEADERS",
        value_delimiter = '\n',
        global = true
    )]
    pub(crate) header: Vec<String>,
    /// Extra host allowed to receive `--header` credentials on an artifact
    /// download (beyond the manifest/source origin); repeatable.
    #[arg(
        long = "credential-host",
        env = "LODE_CREDENTIAL_HOSTS",
        value_delimiter = '\n',
        global = true
    )]
    pub(crate) credential_host: Vec<String>,
    /// Allow non-HTTPS (plain http) remote fetches. Loopback http is always allowed.
    #[arg(
        long = "allow-insecure-http",
        env = "LODE_ALLOW_INSECURE_HTTP",
        global = true
    )]
    pub(crate) allow_insecure_http: bool,

    // --- [trust] ---
    /// Signature enforcement: off | auto | enforce.
    #[arg(
        long = "require-signature",
        env = "LODE_REQUIRE_SIGNATURE",
        global = true
    )]
    pub(crate) require_signature: Option<RequireSignature>,
    /// Trusted public keys, comma-separated `key_id:base64`.
    #[arg(long = "trusted-keys", env = "LODE_TRUSTED_KEYS", global = true)]
    pub(crate) trusted_keys: Option<String>,
    /// Path to a trusted-keys file (one `key_id base64` per line).
    #[arg(
        long = "trusted-keys-file",
        env = "LODE_TRUSTED_KEYS_FILE",
        global = true
    )]
    pub(crate) trusted_keys_file: Option<String>,

    // --- [command] ---
    /// Bare-run launch command (`{entry}` auto-appended).
    #[arg(long = "run", env = "LODE_RUN", global = true)]
    pub(crate) run: Option<String>,
    /// CLI-passthrough base command (`lode <args>` appended).
    #[arg(long = "exec", env = "LODE_EXEC", global = true)]
    pub(crate) exec: Option<String>,
    /// Child working directory (`{dir}` or an absolute path).
    #[arg(long = "workdir", env = "LODE_WORKDIR", global = true)]
    pub(crate) workdir: Option<String>,

    // --- [runtime] ---
    /// Runtime executable name used by run/exec.
    #[arg(long = "runtime", env = "LODE_RUNTIME", global = true)]
    pub(crate) runtime: Option<String>,
    /// Download URL for the runtime when it is absent from PATH.
    #[arg(
        long = "runtime-download",
        env = "LODE_RUNTIME_DOWNLOAD",
        global = true
    )]
    pub(crate) runtime_download: Option<String>,
    /// Expected runtime version; probed and required to match (substring).
    #[arg(long = "runtime-version", env = "LODE_RUNTIME_VERSION", global = true)]
    pub(crate) runtime_version: Option<String>,
    /// Arg(s) that print the runtime version (default `--version`).
    #[arg(
        long = "runtime-version-check",
        env = "LODE_RUNTIME_VERSION_CHECK",
        global = true
    )]
    pub(crate) runtime_version_check: Option<String>,

    // --- [supervise] ---
    /// Restart policy: off | on-failure | always (default off).
    #[arg(long = "restart", env = "LODE_RESTART", global = true)]
    pub(crate) restart: Option<RestartPolicy>,
    /// Crash-restart backoff base, milliseconds (only used when restart != off).
    #[arg(long = "restart-backoff", env = "LODE_RESTART_BACKOFF", global = true)]
    pub(crate) restart_backoff: Option<u64>,
    /// Crash-restart backoff cap, milliseconds (only used when restart != off).
    #[arg(
        long = "restart-backoff-max",
        env = "LODE_RESTART_BACKOFF_MAX",
        global = true
    )]
    pub(crate) restart_backoff_max: Option<u64>,
    /// Max consecutive restarts, 0 = unlimited (only used when restart != off).
    #[arg(long = "restart-max", env = "LODE_RESTART_MAX", global = true)]
    pub(crate) restart_max: Option<u32>,
    /// Readiness check: none | state.
    #[arg(long = "readiness", env = "LODE_READINESS", global = true)]
    pub(crate) readiness: Option<Readiness>,
    /// `readiness=state`: seconds to wait for ready before failing.
    #[arg(long = "ready-timeout", env = "LODE_READY_TIMEOUT", global = true)]
    pub(crate) ready_timeout: Option<u64>,
    /// `readiness=none`: seconds a new version must survive to be good.
    #[arg(long = "health-grace", env = "LODE_HEALTH_GRACE", global = true)]
    pub(crate) health_grace: Option<u64>,
    /// Graceful-stop seconds before SIGKILL.
    #[arg(long = "stop-timeout", env = "LODE_STOP_TIMEOUT", global = true)]
    pub(crate) stop_timeout: Option<u64>,
    /// Restart mode: stop-start | socket-activation | reuseport-overlap.
    #[arg(long = "restart-mode", env = "LODE_RESTART_MODE", global = true)]
    pub(crate) restart_mode: Option<RestartMode>,
    /// socket-activation listen address (e.g. 0.0.0.0:3000).
    #[arg(long = "listen", env = "LODE_LISTEN", global = true)]
    pub(crate) listen: Option<String>,

    // --- [signals] ---
    /// Signals forwarded to the child, comma-separated.
    #[arg(long = "forward-signals", env = "LODE_FORWARD_SIGNALS", global = true)]
    pub(crate) forward_signals: Option<String>,
    /// Signal that triggers a graceful restart instead of being forwarded.
    #[arg(long = "restart-signal", env = "LODE_RESTART_SIGNAL", global = true)]
    pub(crate) restart_signal: Option<String>,
}

/// The **loader** CLI (`lode`). No subcommands: bare `lode` starts the supervised
/// service; `lode <args>` forwards everything to the app via exec passthrough.
#[derive(Debug, Parser)]
#[command(name = "lode", version, about, long_about = None)]
pub(crate) struct LoaderCli {
    #[command(flatten)]
    pub(crate) globals: Globals,

    /// App arguments — forwarded verbatim to the child via exec passthrough.
    /// Empty (bare `lode`) starts the supervised service instead.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub(crate) args: Vec<String>,
}

/// The **`lode-cli`** multitool (a symlink to the `lode` binary). Management +
/// publisher subcommands; same global options as the loader.
#[derive(Debug, Parser)]
#[command(name = "lode-cli", version, about = "lode operator + publisher toolkit", long_about = None)]
pub(crate) struct ToolCli {
    #[command(flatten)]
    pub(crate) globals: Globals,

    #[command(subcommand)]
    pub(crate) command: ToolCommand,
}

/// `lode-cli` subcommands. See `docs/architecture.md` §13 and `docs/integration.md`.
#[derive(Debug, Subcommand)]
pub(crate) enum ToolCommand {
    /// Print current/available version and lode state, then exit.
    Status,
    /// Install the latest (or a specific) version; hot-update a running instance.
    Update {
        /// Install this version instead of the channel latest.
        #[arg(long = "version")]
        version: Option<String>,
    },
    /// Roll back to the last known-good (or a specific) version.
    Rollback {
        /// Roll back to this version instead of the recorded `last_good`.
        #[arg(long = "version")]
        version: Option<String>,
    },
    /// Ask a running instance to restart the child process.
    Restart,
    /// List locally installed versions.
    Versions,

    /// Generate an ed25519 publisher keypair.
    Keygen {
        /// Write `<prefix>.key` (private) and `<prefix>.pub` (public) instead of only printing.
        #[arg(long)]
        out: Option<String>,
    },
    /// Sign an asset (emit sha256 + signature; the signature is the GitHub `label`).
    /// Provide the key with exactly one of `--key` (file) or `--key-env` (env var).
    Sign {
        /// Path to the asset file (its basename is the signed `name`).
        artifact: String,
        /// Release version (bound into the signature).
        #[arg(long = "version")]
        version: String,
        /// Path to the private key file (base64 seed, from `keygen`).
        #[arg(long)]
        key: Option<String>,
        /// Read the base64 private seed from this env var (e.g. a CI secret) instead
        /// of a key file — the key never touches disk.
        #[arg(long = "key-env")]
        key_env: Option<String>,
    },
    /// Verify an asset's sha256 + signature locally.
    Verify {
        /// Path to the asset file (its basename is the signed `name`).
        artifact: String,
        /// Release version (bound into the signature).
        #[arg(long = "version")]
        version: String,
        /// Base64 public key.
        #[arg(long)]
        pubkey: String,
        /// Base64 signature.
        #[arg(long)]
        sig: String,
    },
    /// Sign an asset and emit (or create-or-merge with `--into`) a `lode/v1` manifest.
    Manifest {
        /// Path to the asset file (its basename is the asset `name`).
        artifact: String,
        /// Release version this asset belongs to.
        #[arg(long = "version")]
        version: String,
        /// Download URL for this asset in the manifest (runtime; not signed).
        #[arg(long, default_value = "https://...")]
        url: String,
        /// Advisory in-archive entry path (optional; not signed).
        #[arg(long)]
        entry: Option<String>,
        /// Expected byte size (optional integrity guard).
        #[arg(long)]
        size: Option<u64>,
        /// Channel whose `latest` is set to this version.
        #[arg(long, default_value = "stable")]
        channel: String,
        /// Path to the private key file (base64 seed, from `keygen`).
        #[arg(long)]
        key: String,
        /// Create-or-merge into this `manifest.json` instead of printing.
        #[arg(long)]
        into: Option<String>,
    },
    /// Sign a complete `lode/v1` manifest in place (set its top-level `key_id` + `sig`).
    ManifestSign {
        /// The `manifest.json` to sign in place.
        #[arg(long = "into")]
        into: String,
        /// Path to the private key file (base64 seed, from `keygen`).
        #[arg(long)]
        key: String,
    },
    /// Write a starter `lode.toml` (the documented example config).
    Init {
        /// Destination path; prints to stdout if omitted.
        path: Option<String>,
    },
}
