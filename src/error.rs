//! Central error type for lode.
//!
//! One [`Error`] threads through every module via the crate [`Result`] alias.
//! `#[from]` conversions let leaf modules use `?` directly on foreign errors,
//! while the message-carrying domain variants give each subsystem a typed home.
//! The message-carrying domain variants are deliberately broad; `#[allow(dead_code)]`
//! covers the few not constructed on every build path.

use thiserror::Error;

/// Crate-wide result alias.
pub(crate) type Result<T> = std::result::Result<T, Error>;

/// Every fallible operation in lode surfaces as one of these.
#[derive(Debug, Error)]
#[allow(dead_code)] // not every domain variant is constructed on all build paths
pub(crate) enum Error {
    /// Configuration could not be resolved or failed validation.
    #[error("config: {0}")]
    Config(String),

    /// HTTP fetch of a manifest, artifact or runtime failed.
    #[error("http: {0}")]
    Http(String),

    /// A remote manifest was malformed or referenced a missing entry.
    #[error("manifest: {0}")]
    Manifest(String),

    /// Downloading or unpacking an artifact failed.
    #[error("download: {0}")]
    Download(String),

    /// Installing a version (atomic swap, permissions, GC) failed.
    #[error("install: {0}")]
    Install(String),

    /// Integrity (sha256) or signature (ed25519) verification failed.
    #[error("verify: {0}")]
    Verify(String),

    /// The PID lock could not be acquired or a stale lock could not be reclaimed.
    #[error("lock: {0}")]
    Lock(String),

    /// Reading or writing `state.json` failed beyond a plain I/O error.
    #[error("state: {0}")]
    State(String),

    /// Spawning, signalling or supervising the child process failed.
    #[error("process: {0}")]
    Process(String),

    /// Filesystem / I/O error.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// JSON (manifest / state) (de)serialisation error.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// TOML (`lode.toml`) parse error.
    #[error("toml: {0}")]
    Toml(#[from] toml::de::Error),

    /// Integer parse error (numeric config supplied as text).
    #[error("parse: {0}")]
    ParseInt(#[from] std::num::ParseIntError),
}
