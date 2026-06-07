//! The `lode/v1` remote manifest: serde types, fetch, and version/asset
//! resolution (design §12, source-adapters §6). The manifest lives remotely and is
//! never persisted locally; per-version metadata is written into the version dir at
//! install time (the `.lode.json` marker, see [`crate::install`]).
//!
//! `fetch` dispatches on the configured source (which `[update]` key is set): the
//! **native** branch downloads a `lode/v1` JSON; the **github** branch maps a
//! GitHub Release (selected by channel/pin via the GitHub API) onto the same
//! internal model by listing the release's own assets, taking the version from the
//! release tag. Both produce the identical internal [`Manifest`] — per-version
//! `assets[]` keyed by the asset **filename** — so `main`/`update` never branch on
//! the source and verify/install are unchanged.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::config::{Config, RequireSignature};
use crate::error::{Error, Result};
use crate::idval::validate_id;

/// The only manifest schema this loader understands.
const SCHEMA: &str = "lode/v1";

/// A parsed `lode/v1` manifest. `key_id`/`sig` are the (optional) top-level
/// publisher identity + whole-catalog signature, verified by
/// [`crate::install::verify_manifest_identity`] over [`Manifest::signing_message`].
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Manifest {
    pub(crate) schema: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) key_id: Option<String>,
    #[serde(default)]
    pub(crate) sig: Option<String>,
    pub(crate) channels: BTreeMap<String, Channel>,
    pub(crate) versions: BTreeMap<String, VersionEntry>,
    /// Fetch-time flag — **never** read from JSON. Set by [`fetch_native`] when
    /// following this native catalog's `latest` pointer would be an unverified
    /// downgrade (§2): the manifest carries no top-level catalog signature and
    /// `require_signature != off`. Enforced in [`resolve_target`]'s
    /// `latest`-following branches. GitHub-sourced manifests leave it `false` — a
    /// release's freshness is its tag authority, not a catalog signature (§5).
    #[serde(skip)]
    pub(crate) block_unsigned_latest: bool,
}

impl Manifest {
    /// Deterministic, `sig`-free serialization of the manifest catalog — the body
    /// of the signed message for the top-level [`Manifest::sig`] (§2). Built from the
    /// already-sorted `channels`/`versions` maps so the publisher and the loader,
    /// both working from the parsed struct, produce identical bytes regardless of
    /// the JSON's key order or whitespace. Each line is a tab-separated record:
    /// `channel\t{name}\t{latest}`, `version\t{id}`, and per asset
    /// `asset\t{name}\t{sha256}` (`name` = the asset filename). The per-asset
    /// `sig`/`key_id` and the runtime-only `url`/`entry`/`size` are deliberately
    /// excluded (each asset's own signature is verified separately); this binds the
    /// catalog shape, the channel pointers and every asset's identity + digest.
    fn canonical_catalog(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        for (name, channel) in &self.channels {
            let _ = writeln!(out, "channel\t{name}\t{}", channel.latest);
        }
        for (id, entry) in &self.versions {
            let _ = writeln!(out, "version\t{id}");
            for a in &entry.assets {
                let _ = writeln!(out, "asset\t{}\t{}", a.name, a.sha256);
            }
        }
        out
    }

    /// The exact bytes signed/verified for the top-level manifest signature: the
    /// framing ([`crate::verify::manifest_message`]) wrapped around this manifest's
    /// `name`, `key_id` and [`Self::canonical_catalog`]. The publisher
    /// (`lode-cli manifest-sign`) and the loader ([`crate::install`]) both call this
    /// so signer and verifier never disagree on the bytes.
    pub(crate) fn signing_message(&self) -> Vec<u8> {
        crate::verify::manifest_message(
            &self.name,
            self.key_id.as_deref().unwrap_or(""),
            &self.canonical_catalog(),
        )
    }
}

/// A release channel: its `latest` points at a key in `versions`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Channel {
    pub(crate) latest: String,
}

/// One published version: optional notes plus its assets, keyed by filename.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct VersionEntry {
    /// Human-readable release notes (shown by `update`).
    #[serde(default)]
    pub(crate) notes: Option<String>,
    pub(crate) assets: Vec<Asset>,
}

/// A single downloadable asset, selected by its filename (`name`). The filename is
/// the §1 signed identity and the §3 selection key; its extension determines the
/// packaging format ([`format_from_name`]) — neither `platform` nor `format` is
/// stored or signed.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Asset {
    /// Asset filename — the selection key (matched against `[update].asset`) and
    /// the identity bound by the §1 signature.
    pub(crate) name: String,
    /// Absolute download URL (runtime; never signed).
    pub(crate) url: String,
    /// Lowercase-hex sha256 of the downloaded file (pre-unpack).
    pub(crate) sha256: String,
    /// base64 ed25519 over the §1 canonical message (the GitHub asset `label`).
    #[serde(default)]
    pub(crate) sig: Option<String>,
    /// Overrides the manifest `key_id` for this asset.
    #[serde(default)]
    #[allow(dead_code)] // advisory per-asset key override; tried via the trusted-key set
    pub(crate) key_id: Option<String>,
    /// Advisory in-archive entry path (§4; runtime, never signed).
    #[serde(default)]
    pub(crate) entry: Option<String>,
    /// Expected byte size (an extra guard, checked at download).
    #[serde(default)]
    pub(crate) size: Option<u64>,
    /// `false` => do not attach `[http].headers` to this URL (e.g. pre-signed).
    #[serde(default = "default_true")]
    pub(crate) auth: bool,
}

const fn default_true() -> bool {
    true
}

/// Derive the packaging format from an asset filename's suffix (longest match,
/// case-insensitive, §3): `.tar.gz`/`.tgz` → `tar.gz`, `.gz` → `gz`, `.zip` →
/// `zip`, anything else → `raw`. The extension is authoritative — `format` is never
/// stored or signed.
#[allow(clippy::case_sensitive_file_extension_comparisons)] // lowered first; literals are ASCII
pub(crate) fn format_from_name(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        "tar.gz"
    } else if lower.ends_with(".gz") {
        "gz"
    } else if lower.ends_with(".zip") {
        "zip"
    } else {
        "raw"
    }
}

/// Fetch and parse the manifest for the configured source. Dispatches on which
/// `[update]` key is set (validated mutually-exclusive in [`crate::config`]).
pub(crate) fn fetch(cfg: &Config) -> Result<Manifest> {
    match (cfg.update.manifest.as_deref(), cfg.update.github.as_deref()) {
        (Some(url), _) => fetch_native(cfg, url),
        (None, Some(repo)) => fetch_github(cfg, repo),
        (None, None) => Err(Error::Manifest(
            "no update source configured (set [update].manifest or [update].github)".to_owned(),
        )),
    }
}

/// The hosts permitted to receive `[http].headers` (credentials) on an artifact
/// download for the configured source — the trusted credential same-origin set.
/// The manifest/source origin is implicitly trusted; the operator may extend it
/// via `[http].credential_hosts`. A manifest that points an artifact at any other
/// host gets no credentials (see [`crate::download::fetch_artifact`]).
///
/// Dispatch mirrors [`fetch`] (native wins when `manifest` is set):
/// - native: the `[update].manifest` URL host.
/// - github: the `[update].github_api` host plus GitHub's fixed asset hosts — the
///   token must ride both the API call and the release-asset download.
/// - neither source set: just `[http].credential_hosts`.
pub(crate) fn allowed_hosts(cfg: &Config) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    match (cfg.update.manifest.as_deref(), cfg.update.github.as_deref()) {
        (Some(url), _) => {
            if let Some(host) = crate::http::url_host(url) {
                hosts.push(host.to_owned());
            }
        }
        (None, Some(_)) => {
            if let Some(host) = crate::http::url_host(&cfg.update.github_api) {
                hosts.push(host.to_owned());
            }
            hosts.push("github.com".to_owned());
            hosts.push("objects.githubusercontent.com".to_owned());
            hosts.push("codeload.github.com".to_owned());
        }
        (None, None) => {}
    }
    hosts.extend(cfg.http.credential_hosts.iter().cloned());
    hosts
}

/// Native source: download the `lode/v1` JSON over HTTP, parse it, then apply the
/// two native-only refinements the GitHub adapter never needs:
/// - **§2 downgrade guard:** flag whether following the catalog's `latest` pointer
///   would be an unverified downgrade ([`Manifest::block_unsigned_latest`], enforced
///   by [`resolve_target`]).
/// - **§6 detached signatures:** back-fill any `.sig` sidecar for the install-target
///   asset that carries no inline `sig` ([`resolve_native_sidecars`]).
///
/// Both operate on the freshly-parsed, still-owned manifest, so the downstream
/// `select_asset`/verify/install path stays identical (and source-agnostic) for
/// both adapters.
fn fetch_native(cfg: &Config, url: &str) -> Result<Manifest> {
    let headers = crate::http::expand_headers(&cfg.http.headers)?;
    let bytes = crate::http::get_bytes(url, &headers, cfg.http.allow_insecure)?;
    let mut manifest = parse(&bytes)?;
    // §2: an unsigned native catalog must not be *followed* via `latest` unless the
    // operator pins a version or opts out of signing entirely.
    manifest.block_unsigned_latest =
        manifest.sig.is_none() && cfg.trust.require_signature != RequireSignature::Off;
    // §6: when a signature is required and the selected asset has no inline `sig`,
    // fall back to its `<url>.sig` sidecar.
    resolve_native_sidecars(cfg, &mut manifest, &allowed_hosts(cfg))?;
    Ok(manifest)
}

/// Whether a publisher signature is required under the current trust policy — the
/// precondition for reaching for a detached `.sig` sidecar (§6). Mirrors
/// [`crate::install`]'s fail-closed posture exactly: `enforce` always requires one;
/// `auto` requires one only when trusted keys are configured; `off` never does.
fn signature_required(cfg: &Config) -> Result<bool> {
    Ok(match cfg.trust.require_signature {
        RequireSignature::Off => false,
        RequireSignature::Enforce => true,
        RequireSignature::Auto => !crate::install::trusted_keys(cfg)?.is_empty(),
    })
}

/// Back-fill detached `.sig` sidecars (§6) into the native manifest for the asset
/// this host installs (`[update].asset`) in the version(s) its `latest`/`pin`
/// pointer selects. A sidecar is fetched only when a signature is required and the
/// asset carries no inline `sig` (which always wins — [`effective_sig`]); the fetch
/// is best-effort, so a missing or unreachable sidecar simply leaves the asset
/// unsigned and the downstream [`crate::install`] verification decides per policy
/// (fail-closed under `enforce`/`auto`+keys). Same-origin credential rules apply to
/// the sidecar fetch exactly as to the asset download.
///
/// Scope: only the `latest`/`pin` target asset(s) are pre-resolved (a bounded
/// number of fetches — at most the channel latest plus the pin). Installing an
/// *explicit* non-latest, non-pinned version whose asset is sidecar-only (`update
/// --version X`) is not pre-resolved here; pin that version or give it an inline
/// `sig`. No-op for the GitHub adapter, which never calls this.
fn resolve_native_sidecars(
    cfg: &Config,
    manifest: &mut Manifest,
    allowed_hosts: &[String],
) -> Result<()> {
    if !signature_required(cfg)? {
        return Ok(());
    }
    let Some(asset_name) = cfg.update.asset.as_deref() else {
        return Ok(());
    };
    // The version(s) a no-explicit-version install resolves to: this host's channel
    // `latest` (when present) plus any configured `pin`. Collected before the
    // mutable pass so the immutable lookups don't overlap the `get_mut` borrow.
    let mut targets: Vec<String> = Vec::new();
    if let Ok(latest) = channel_latest(manifest, &cfg.update.channel) {
        targets.push(latest);
    }
    if let Some(pin) = cfg.update.pin.as_deref() {
        targets.push(pin.to_owned());
    }
    for version in targets {
        let Some(entry) = manifest.versions.get_mut(&version) else {
            continue;
        };
        let Some(asset) = entry.assets.iter_mut().find(|a| a.name == asset_name) else {
            continue;
        };
        // Inline `sig` always wins — only reach for a sidecar when it is absent.
        let sidecar = if asset.sig.is_none() {
            fetch_sidecar(cfg, asset, allowed_hosts)
        } else {
            None
        };
        asset.sig = effective_sig(asset.sig.as_deref(), sidecar.as_deref());
    }
    Ok(())
}

/// Fetch an asset's detached signature sidecar at `<url>.sig` (§6), returning its
/// trimmed body as the base64 signature, or `None` when the sidecar is absent or
/// unreachable (best-effort — verification still happens at install). `[http]`
/// credentials ride the sidecar fetch only when the asset opts in (`auth`) and the
/// sidecar host is same-origin/allowlisted, reusing [`crate::download`]'s gate so
/// the rule is identical to the asset download.
fn fetch_sidecar(cfg: &Config, asset: &Asset, allowed_hosts: &[String]) -> Option<String> {
    let url = format!("{}.sig", asset.url);
    let headers = if asset.auth
        && !cfg.http.headers.is_empty()
        && crate::download::host_allowed(&url, allowed_hosts)
    {
        // Can't build the credentials (e.g. an unset `${ENV}`) → skip the sidecar;
        // the asset download surfaces the real header error.
        crate::http::expand_headers(&cfg.http.headers).ok()?
    } else {
        Vec::new()
    };
    match crate::http::get_bytes(&url, &headers, cfg.http.allow_insecure) {
        Ok(body) => Some(sidecar_signature(&body)),
        Err(e) => {
            tracing::debug!(error = %e, "native .sig sidecar fetch failed; asset left unsigned");
            None
        }
    }
}

/// The base64 signature carried by a detached `.sig` sidecar: its body read as
/// UTF-8 with trailing whitespace/newlines trimmed (publishers commonly append a
/// newline). The §1 verification re-trims, so this only normalises the stored form.
fn sidecar_signature(body: &[u8]) -> String {
    String::from_utf8_lossy(body).trim_end().to_owned()
}

/// The signature to use for a native asset: an inline `sig` ALWAYS wins; a detached
/// sidecar is only the fallback (§6). `None` when neither is present.
fn effective_sig(inline: Option<&str>, sidecar: Option<&str>) -> Option<String> {
    inline.or(sidecar).map(ToOwned::to_owned)
}

/// GitHub source: pick the release for the configured channel/pin via the GitHub
/// API, then map the release's own asset list onto the internal [`Manifest`] — no
/// `manifest.json` asset. The release `tag_name` (minus a leading `v`) is the
/// version, so `select_asset`/verify/install downstream are identical to the
/// native source.
fn fetch_github(cfg: &Config, repo: &str) -> Result<Manifest> {
    let (owner, name) = parse_repo(repo)?;
    let base = cfg.update.github_api.trim_end_matches('/');
    // `[http].headers` (e.g. an `Authorization` token for private repos) ride both
    // the API call and the asset download; values are never logged (see http.rs).
    let headers = crate::http::expand_headers(&cfg.http.headers)?;
    let channel = cfg.update.channel.as_str();
    let pin = cfg.update.pin.as_deref();
    let insecure = cfg.http.allow_insecure;

    let release = select_release(base, owner, name, channel, pin, &headers, insecure)?;
    map_release(&cfg.global.app, release, channel, pin)
}

/// Resolve the release for `channel`/`pin` using GitHub's native endpoints
/// (design §12): a `pin` hits `/releases/tags/{tag}`; the `stable` channel hits
/// `/releases/latest` (GitHub's newest non-draft, non-prerelease); any other
/// channel lists `/releases` and takes the newest prerelease.
fn select_release(
    base: &str,
    owner: &str,
    name: &str,
    channel: &str,
    pin: Option<&str>,
    headers: &[(String, String)],
    allow_insecure: bool,
) -> Result<GhRelease> {
    if let Some(tag) = pin {
        let url = format!("{base}/repos/{owner}/{name}/releases/tags/{tag}");
        return get_json(&url, headers, allow_insecure);
    }
    if channel == "stable" {
        let url = format!("{base}/repos/{owner}/{name}/releases/latest");
        return get_json(&url, headers, allow_insecure);
    }
    let url = format!("{base}/repos/{owner}/{name}/releases");
    let list: Vec<GhRelease> = get_json(&url, headers, allow_insecure)?;
    select_prerelease(list)
}

/// Fetch `url` (with `headers`) and deserialize the JSON body into `T`.
fn get_json<T: serde::de::DeserializeOwned>(
    url: &str,
    headers: &[(String, String)],
    allow_insecure: bool,
) -> Result<T> {
    Ok(serde_json::from_slice(&crate::http::get_bytes(
        url,
        headers,
        allow_insecure,
    )?)?)
}

/// Split a `"owner/name"` repo spec, rejecting empty parts or extra slashes.
fn parse_repo(repo: &str) -> Result<(&str, &str)> {
    let repo = repo.trim();
    match repo.split_once('/') {
        Some((owner, name)) if !owner.is_empty() && !name.is_empty() && !name.contains('/') => {
            Ok((owner, name))
        }
        _ => Err(Error::Manifest(format!(
            "github repo must be \"owner/name\", got {repo:?}"
        ))),
    }
}

/// Newest prerelease in a `/releases` listing (GitHub returns them newest-first),
/// skipping drafts. Errors when the channel has no prerelease yet.
fn select_prerelease(releases: Vec<GhRelease>) -> Result<GhRelease> {
    releases
        .into_iter()
        .find(|r| r.prerelease && !r.draft)
        .ok_or_else(|| Error::Manifest("no prerelease release found for this channel".to_owned()))
}

/// Map a GitHub release's own asset list onto the internal [`Manifest`] (§5). Each
/// release asset becomes an [`Asset`] keyed by its filename; the release `tag_name`
/// (minus a leading `v`) is the version, and `app` is the manifest `name` (it must
/// match `[global].app`). The result carries one synthesised `channel` whose
/// `latest` points at that version; a set `pin` (a raw tag) is also registered as a
/// version id so downstream pin resolution — which is literal (see
/// [`resolve_target`]) — still resolves to this release.
fn map_release(
    app: &str,
    release: GhRelease,
    channel: &str,
    pin: Option<&str>,
) -> Result<Manifest> {
    let ver = strip_v(&release.tag_name).to_owned();
    // The release tag becomes a `versions` key (→ a filesystem path); validate it
    // before it can be inserted or used downstream.
    validate_id("version", &ver)?;

    let assets: Vec<Asset> = release.assets.into_iter().map(gh_asset_to_asset).collect();
    if assets.is_empty() {
        return Err(Error::Manifest(format!("release {ver:?} has no assets")));
    }
    let entry = VersionEntry {
        notes: None,
        assets,
    };

    let mut versions = BTreeMap::new();
    if let Some(tag) = pin
        && tag != ver.as_str()
    {
        // The raw pin tag is registered as its own version key, so guard it too.
        validate_id("version", tag)?;
        versions.insert(tag.to_owned(), entry.clone());
    }
    versions.insert(ver.clone(), entry);

    let mut channels = BTreeMap::new();
    channels.insert(channel.to_owned(), Channel { latest: ver });

    Ok(Manifest {
        schema: SCHEMA.to_owned(),
        name: app.to_owned(),
        key_id: None,
        sig: None,
        channels,
        versions,
        // GitHub freshness is tag authority, not a catalog signature (§5) — never
        // block following `latest`.
        block_unsigned_latest: false,
    })
}

/// Map one GitHub release asset onto an internal [`Asset`]: filename → `name`;
/// `browser_download_url` → `url`; `digest` (minus the `sha256:` prefix) → `sha256`;
/// `label` (the only arbitrary-string slot the API returns) → `sig`; `size` carried
/// through. GitHub has no advisory `entry` slot, and credentials may ride the asset
/// host (gated by `allowed_hosts`), so `entry` is `None` and `auth` is on.
fn gh_asset_to_asset(a: GhAsset) -> Asset {
    let sha256 = a
        .digest
        .as_deref()
        .map(|d| d.strip_prefix("sha256:").unwrap_or(d).to_owned())
        .unwrap_or_default();
    Asset {
        name: a.name,
        url: a.browser_download_url,
        sha256,
        sig: a.label,
        key_id: None,
        entry: None,
        size: a.size,
        auth: true,
    }
}

/// Drop a leading `v` from a release tag when it precedes a digit (the `vX.Y.Z`
/// convention, e.g. `v1.5.0` → `1.5.0`); any other tag passes through unchanged.
fn strip_v(tag: &str) -> &str {
    tag.strip_prefix('v')
        .filter(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or(tag)
}

/// One GitHub Release (`/releases/latest`, `/releases/tags/{tag}`, or an element
/// of `/releases`). Only the fields the adapter consumes are modelled; serde
/// ignores the rest.
#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

/// One asset attached to a release; `browser_download_url` is fetched through
/// [`crate::http`] (with `[http].headers`, so a token reaches private repos).
/// `digest` carries the API's `sha256:<hex>` integrity hash; `label` is the
/// publisher signature slot (§5).
#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    size: Option<u64>,
}

/// Parse and structurally validate a `lode/v1` manifest from raw JSON bytes.
pub(crate) fn parse(bytes: &[u8]) -> Result<Manifest> {
    let manifest: Manifest = serde_json::from_slice(bytes)?;
    if manifest.schema != SCHEMA {
        return Err(Error::Manifest(format!(
            "unsupported schema {:?} (expected {SCHEMA:?})",
            manifest.schema
        )));
    }
    if manifest.channels.is_empty() {
        return Err(Error::Manifest("manifest declares no channels".to_owned()));
    }
    if manifest.versions.is_empty() {
        return Err(Error::Manifest("manifest declares no versions".to_owned()));
    }
    Ok(manifest)
}

/// Resolve a target spec to a concrete version id present in `manifest.versions`.
///
/// Precedence: an explicit `requested` version wins (the literal `"latest"`
/// re-resolves to the channel latest); otherwise a configured `pin` wins; else
/// the channel's `latest`. The resolved id must exist in `versions`.
///
/// Following the channel `latest` pointer (the default, or an explicit `"latest"`,
/// with no `pin`) is gated by [`guard_latest_pointer`]: an unsigned native catalog
/// is refused as a downgrade risk (§2). An explicit concrete version or a `pin` is
/// the operator's responsibility and is never blocked.
pub(crate) fn resolve_target(
    manifest: &Manifest,
    channel: &str,
    pin: Option<&str>,
    requested: Option<&str>,
) -> Result<String> {
    let want = match requested {
        Some("latest") => {
            guard_latest_pointer(manifest, pin)?;
            channel_latest(manifest, channel)?
        }
        Some(version) => version.to_owned(),
        None => {
            if let Some(version) = pin {
                version.to_owned()
            } else {
                guard_latest_pointer(manifest, None)?;
                channel_latest(manifest, channel)?
            }
        }
    };
    // The resolved id keys `versions/<id>` and `downloads/<id>.part`; reject any
    // traversal before it touches a path (covers a malicious native `versions`
    // key advertised as the channel latest, a pin, or an explicit request).
    validate_id("version", &want)?;
    if !manifest.versions.contains_key(&want) {
        return Err(Error::Manifest(format!(
            "version {want:?} not present in manifest"
        )));
    }
    Ok(want)
}

/// Refuse to *follow* a native catalog's channel `latest` pointer when doing so
/// would be an unverified downgrade (§2): the manifest carries no verified catalog
/// signature ([`Manifest::block_unsigned_latest`], set by [`fetch_native`]) and the
/// operator has not pinned a version. An explicit version request or a `pin`
/// (`pin.is_some()`) takes responsibility for the choice and is never blocked here;
/// GitHub manifests never set the flag (their freshness is tag authority, §5), so
/// this is a no-op for the GitHub adapter.
fn guard_latest_pointer(manifest: &Manifest, pin: Option<&str>) -> Result<()> {
    if pin.is_none() && manifest.block_unsigned_latest {
        return Err(Error::Manifest(
            "refusing to follow the channel `latest` pointer: this native manifest has no \
             verified catalog signature and no version is pinned, so following `latest` risks a \
             silent downgrade. Sign the manifest (`lode-cli manifest-sign`), pin a version with \
             [update].pin, or set [trust].require_signature = \"off\" to override."
                .to_owned(),
        ));
    }
    Ok(())
}

/// The `latest` version id advertised by `channel`.
fn channel_latest(manifest: &Manifest, channel: &str) -> Result<String> {
    manifest
        .channels
        .get(channel)
        .map(|c| c.latest.clone())
        .ok_or_else(|| Error::Manifest(format!("channel {channel:?} not present in manifest")))
}

/// Borrow the entry for an exact `version` id.
pub(crate) fn version_entry<'a>(manifest: &'a Manifest, version: &str) -> Result<&'a VersionEntry> {
    manifest
        .versions
        .get(version)
        .ok_or_else(|| Error::Manifest(format!("version {version:?} not present in manifest")))
}

/// Select the asset whose filename matches `asset_name` exactly (§3). There is no
/// platform fallback and no `any` wildcard: the operator names the exact asset for
/// this host via `[update].asset`. Errors when no asset matches.
pub(crate) fn select_asset<'a>(entry: &'a VersionEntry, asset_name: &str) -> Result<&'a Asset> {
    entry
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .ok_or_else(|| Error::Manifest(format!("no asset named {asset_name:?} in this version")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> Manifest {
        parse(include_bytes!("../docs/manifest.example.json")).unwrap()
    }

    #[test]
    fn parses_example_manifest() {
        let m = example();
        assert_eq!(m.schema, "lode/v1");
        assert_eq!(m.name, "myapp");
        assert_eq!(m.channels["stable"].latest, "1.5.0");
        assert!(m.versions.contains_key("1.5.0"));
        // `auth` defaults to true; the darwin-arm64 1.5.0 asset omits it.
        let darwin = m.versions["1.5.0"]
            .assets
            .iter()
            .find(|a| a.name == "myapp-darwin-arm64.tar.gz")
            .unwrap();
        assert!(darwin.auth);
    }

    #[test]
    fn rejects_wrong_schema() {
        let bad = br#"{"schema":"bogus","name":"x","channels":{},"versions":{}}"#;
        assert!(parse(bad).is_err());
    }

    #[test]
    fn format_from_name_longest_match() {
        assert_eq!(format_from_name("app-linux-x86_64.tar.gz"), "tar.gz");
        assert_eq!(format_from_name("app.TGZ"), "tar.gz"); // case-insensitive
        assert_eq!(format_from_name("app.gz"), "gz");
        assert_eq!(format_from_name("app.zip"), "zip");
        // No recognised suffix → raw (dots that aren't packaging suffixes).
        assert_eq!(format_from_name("myapp-1.5.0"), "raw");
        assert_eq!(format_from_name("myapp"), "raw");
    }

    #[test]
    fn signing_message_is_stable_and_tamper_sensitive() {
        let m = example();
        // Reproducible: re-parsing the same bytes yields identical signing bytes,
        // independent of JSON key order (BTreeMaps are sorted; Vec order is parsed).
        assert_eq!(m.signing_message(), example().signing_message());
        // The v1 framing and identity prefix are present.
        let msg = String::from_utf8(m.signing_message()).unwrap();
        assert!(msg.starts_with("lode.manifest.v1\nmyapp\n9787ab11e55b4cbc\n"));
        // The top-level `sig` is NOT part of the signed body.
        assert!(!msg.contains("<optional"));
        // Assets are bound by filename + digest (no platform/format/url/entry).
        assert!(msg.contains("asset\tmyapp-linux-x86_64.tar.gz\t"));

        // Tampering with a channel pointer changes the bytes (so the sig fails).
        let mut tampered = m.clone();
        tampered.channels.insert(
            "stable".to_owned(),
            Channel {
                latest: "9.9.9".to_owned(),
            },
        );
        assert_ne!(m.signing_message(), tampered.signing_message());

        // Tampering with an asset digest also changes the bytes.
        let mut digest_swap = m.clone();
        digest_swap.versions.get_mut("1.5.0").unwrap().assets[0].sha256 = "00".to_owned();
        assert_ne!(m.signing_message(), digest_swap.signing_message());

        // The declared key_id is bound into the message (catalog identical).
        let mut rekeyed = m.clone();
        rekeyed.key_id = Some("ffffffffffffffff".to_owned());
        assert_ne!(m.signing_message(), rekeyed.signing_message());
    }

    #[test]
    fn resolves_channel_pin_and_explicit() {
        let m = example();
        // channel latest
        assert_eq!(resolve_target(&m, "stable", None, None).unwrap(), "1.5.0");
        assert_eq!(
            resolve_target(&m, "beta", None, None).unwrap(),
            "1.6.0-beta.2"
        );
        // explicit "latest" re-resolves to the channel latest (ignores pin)
        assert_eq!(
            resolve_target(&m, "stable", Some("1.5.0"), Some("latest")).unwrap(),
            "1.5.0"
        );
        // pin wins when nothing explicit is requested
        assert_eq!(
            resolve_target(&m, "stable", Some("1.6.0-beta.2"), None).unwrap(),
            "1.6.0-beta.2"
        );
        // explicit version wins over pin
        assert_eq!(
            resolve_target(&m, "stable", Some("1.5.0"), Some("1.6.0-beta.2")).unwrap(),
            "1.6.0-beta.2"
        );
    }

    #[test]
    fn rejects_unknown_channel_and_version() {
        let m = example();
        assert!(resolve_target(&m, "nope", None, None).is_err());
        assert!(resolve_target(&m, "stable", None, Some("9.9.9")).is_err());
        assert!(resolve_target(&m, "stable", Some("9.9.9"), None).is_err());
    }

    #[test]
    fn selects_asset_by_name_or_errors() {
        let m = example();
        let v150 = version_entry(&m, "1.5.0").unwrap();
        // Exact filename match.
        assert_eq!(
            select_asset(v150, "myapp-linux-x86_64.tar.gz")
                .unwrap()
                .name,
            "myapp-linux-x86_64.tar.gz"
        );
        assert_eq!(
            select_asset(v150, "myapp-darwin-arm64.tar.gz")
                .unwrap()
                .name,
            "myapp-darwin-arm64.tar.gz"
        );
        // No platform fallback / no "any": an unknown filename errors.
        assert!(select_asset(v150, "myapp-windows-x86_64.zip").is_err());
        let beta = version_entry(&m, "1.6.0-beta.2").unwrap();
        assert!(select_asset(beta, "myapp-linux-x86_64.tar.gz").is_err());
    }

    // --- native: detached `.sig` sidecar (§6) ------------------------------

    #[test]
    fn sidecar_signature_trims_trailing_whitespace() {
        // A trailing newline (the common case) is stripped.
        assert_eq!(sidecar_signature(b"AbC123==\n"), "AbC123==");
        // CRLF and a run of trailing whitespace/blank lines too.
        assert_eq!(sidecar_signature(b"AbC123==\r\n"), "AbC123==");
        assert_eq!(sidecar_signature(b"AbC123==  \n\n"), "AbC123==");
        // No trailing whitespace → unchanged.
        assert_eq!(sidecar_signature(b"AbC123=="), "AbC123==");
        // Only the *trailing* run is trimmed — interior bytes are preserved.
        assert_eq!(sidecar_signature(b"ab cd\n"), "ab cd");
    }

    #[test]
    fn effective_sig_inline_always_wins_sidecar_is_fallback() {
        // Inline `sig` wins even when a sidecar is present.
        assert_eq!(
            effective_sig(Some("inline"), Some("sidecar")).as_deref(),
            Some("inline")
        );
        // No inline → fall back to the sidecar.
        assert_eq!(
            effective_sig(None, Some("sidecar")).as_deref(),
            Some("sidecar")
        );
        // Inline with no sidecar → inline.
        assert_eq!(
            effective_sig(Some("inline"), None).as_deref(),
            Some("inline")
        );
        // Neither → unsigned.
        assert_eq!(effective_sig(None, None), None);
    }

    // --- native: §2 downgrade guard (`latest` pointer) ---------------------

    #[test]
    fn downgrade_guard_refuses_unsigned_unpinned_latest() {
        let mut m = example();
        // Simulate `fetch_native` marking an unsigned native catalog under a signing
        // policy (`require_signature != off`).
        m.block_unsigned_latest = true;
        // Following `latest` with no pin is refused — both the default resolution and
        // an explicit `"latest"` request.
        assert!(resolve_target(&m, "stable", None, None).is_err());
        assert!(resolve_target(&m, "stable", None, Some("latest")).is_err());
        // …but a pin takes responsibility for the choice and is allowed…
        assert_eq!(
            resolve_target(&m, "stable", Some("1.5.0"), None).unwrap(),
            "1.5.0"
        );
        assert_eq!(
            resolve_target(&m, "stable", Some("1.5.0"), Some("latest")).unwrap(),
            "1.5.0"
        );
        // …as is an explicit concrete version (not a pointer-follow).
        assert_eq!(
            resolve_target(&m, "stable", None, Some("1.5.0")).unwrap(),
            "1.5.0"
        );
    }

    #[test]
    fn downgrade_guard_allows_signed_or_exempt_latest() {
        let mut m = example();
        // A verified/exempt catalog (flag clear — signed manifest, or
        // require_signature=off, or a GitHub source) follows `latest` normally.
        m.block_unsigned_latest = false;
        assert_eq!(resolve_target(&m, "stable", None, None).unwrap(), "1.5.0");
        assert_eq!(
            resolve_target(&m, "stable", None, Some("latest")).unwrap(),
            "1.5.0"
        );
    }

    // --- GitHub Releases adapter -------------------------------------------

    /// A `/releases` listing (newest-first): a draft prerelease, then the newest
    /// real prerelease (with signed + unsigned assets), an older prerelease, and a
    /// stable release. The release's own assets are the catalog — no manifest.json.
    const RELEASES_LIST: &[u8] = br#"[
      { "tag_name": "v1.7.0-beta.1", "prerelease": true, "draft": true, "assets": [] },
      { "tag_name": "v1.6.0-beta.2", "prerelease": true, "draft": false,
        "assets": [
          { "name": "myapp-linux-x86_64.tar.gz",
            "browser_download_url": "https://r/beta2/myapp-linux-x86_64.tar.gz",
            "digest": "sha256:a97ad2265ae84cdeff1219b1c83db8e6f096e444c81f733bc93355f0fff368a1",
            "label": "Izn/bTO7W4gOFBlpPswTE6Zjmyfqkt==",
            "size": 1234 },
          { "name": "myapp-darwin-arm64.tar.gz",
            "browser_download_url": "https://r/beta2/myapp-darwin-arm64.tar.gz",
            "digest": "sha256:b1c2d3e4f5060718293a4b5c6d7e8f90112233445566778899aabbccddeeff00" } ] },
      { "tag_name": "v1.6.0-beta.1", "prerelease": true, "draft": false,
        "assets": [ { "name": "myapp-linux-x86_64.tar.gz",
          "browser_download_url": "https://r/beta1/myapp-linux-x86_64.tar.gz" } ] },
      { "tag_name": "v1.5.0", "prerelease": false, "draft": false,
        "assets": [ { "name": "myapp-linux-x86_64.tar.gz",
          "browser_download_url": "https://r/stable/myapp-linux-x86_64.tar.gz",
          "digest": "sha256:deadbeef" } ] }
    ]"#;

    #[test]
    fn parse_repo_accepts_and_rejects() {
        assert_eq!(parse_repo("owner/name").unwrap(), ("owner", "name"));
        assert_eq!(parse_repo("  owner/name  ").unwrap(), ("owner", "name"));
        assert!(parse_repo("owner").is_err());
        assert!(parse_repo("owner/name/extra").is_err());
        assert!(parse_repo("/name").is_err());
        assert!(parse_repo("owner/").is_err());
    }

    #[test]
    fn strip_v_only_before_a_digit() {
        assert_eq!(strip_v("v1.5.0"), "1.5.0");
        assert_eq!(strip_v("1.5.0"), "1.5.0");
        assert_eq!(strip_v("v2024.1"), "2024.1");
        // `v` not followed by a digit is part of the tag, not a prefix.
        assert_eq!(strip_v("vNext"), "vNext");
        assert_eq!(strip_v("nightly"), "nightly");
    }

    #[test]
    fn select_prerelease_skips_draft_and_stable() {
        let list: Vec<GhRelease> = serde_json::from_slice(RELEASES_LIST).unwrap();
        // Newest non-draft prerelease (draft v1.7.0-beta.1 skipped; stable ignored).
        assert_eq!(select_prerelease(list).unwrap().tag_name, "v1.6.0-beta.2");
    }

    #[test]
    fn select_prerelease_errors_when_none() {
        let list: Vec<GhRelease> = serde_json::from_slice(
            br#"[ { "tag_name": "v1.0.0", "prerelease": false, "draft": false, "assets": [] } ]"#,
        )
        .unwrap();
        assert!(select_prerelease(list).is_err());
    }

    #[test]
    fn map_release_maps_github_assets() {
        // The selected release's own asset list becomes the internal catalog: the
        // tag is the version, `digest` (minus `sha256:`) is the sha256, `label` is
        // the signature, `size` carries through, and assets select by filename.
        let list: Vec<GhRelease> = serde_json::from_slice(RELEASES_LIST).unwrap();
        let release = select_prerelease(list).unwrap();
        assert_eq!(release.tag_name, "v1.6.0-beta.2");

        let m = map_release("myapp", release, "beta", None).unwrap();
        assert_eq!(m.schema, "lode/v1");
        assert_eq!(m.name, "myapp"); // from the configured app, not a manifest asset
        assert!(m.key_id.is_none());
        assert_eq!(m.channels["beta"].latest, "1.6.0-beta.2");
        assert_eq!(
            resolve_target(&m, "beta", None, None).unwrap(),
            "1.6.0-beta.2"
        );

        let entry = version_entry(&m, "1.6.0-beta.2").unwrap();
        let signed = select_asset(entry, "myapp-linux-x86_64.tar.gz").unwrap();
        assert_eq!(
            signed.sha256,
            "a97ad2265ae84cdeff1219b1c83db8e6f096e444c81f733bc93355f0fff368a1"
        );
        assert_eq!(
            signed.sig.as_deref(),
            Some("Izn/bTO7W4gOFBlpPswTE6Zjmyfqkt==")
        );
        assert_eq!(signed.size, Some(1234));
        assert!(signed.entry.is_none()); // GitHub has no advisory entry slot
        assert!(signed.auth);
        // The format is derived from the filename, never stored.
        assert_eq!(format_from_name(&signed.name), "tar.gz");
        // An asset without a `label` maps to an unsigned asset.
        let unsigned = select_asset(entry, "myapp-darwin-arm64.tar.gz").unwrap();
        assert!(unsigned.sig.is_none());
    }

    #[test]
    fn map_release_strips_v_and_registers_pin_alias() {
        let release: GhRelease = serde_json::from_slice(
            br#"{ "tag_name": "v1.5.0", "assets": [
              { "name": "myapp-linux-x86_64.tar.gz",
                "browser_download_url": "https://r/stable/myapp-linux-x86_64.tar.gz",
                "digest": "sha256:deadbeef" } ] }"#,
        )
        .unwrap();
        // A raw-tag pin must also resolve, since `resolve_target`'s pin branch is
        // literal — both the stripped id and the raw tag key the same release.
        let m = map_release("myapp", release, "stable", Some("v1.5.0")).unwrap();
        assert!(m.versions.contains_key("1.5.0"));
        assert!(m.versions.contains_key("v1.5.0"));
        assert_eq!(
            resolve_target(&m, "stable", Some("v1.5.0"), None).unwrap(),
            "v1.5.0"
        );
        assert_eq!(resolve_target(&m, "stable", None, None).unwrap(), "1.5.0");
    }

    #[test]
    fn map_release_rejects_empty_asset_list() {
        let release: GhRelease =
            serde_json::from_slice(br#"{ "tag_name": "v1.0.0", "assets": [] }"#).unwrap();
        assert!(map_release("myapp", release, "stable", None).is_err());
    }
}
