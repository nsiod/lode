//! Verified install: integrity (sha256) + identity (ed25519) → extract by format
//! → atomic activation → prune, plus startup GC (design §4/§5/§6/§15).
//!
//! Flow per version: verify the downloaded file's digest against the manifest and
//! (per `trust.require_signature`) its signature over the §6 canonical message;
//! stage the unpacked tree under `versions/<ver>.tmp`; `chmod +x` the entry; write
//! the per-version `.lode.json` marker; then atomically swap it into
//! `versions/<ver>`. Activation flips the `current` symlink via a temp-symlink
//! rename. Pruning keeps `current` + `last_good` + the newest `keep_versions`.

use std::cmp::Ordering;
use std::collections::{HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read as _};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{Config, RequireSignature};
use crate::error::{Error, Result};
use crate::idval::{validate_entry, validate_id};
use crate::manifest::{Asset, Manifest, format_from_name};

/// Per-version metadata written to `versions/<ver>/.lode.json` so a version can
/// be launched offline without re-consulting the manifest (design §15). Read back
/// by the supervisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Marker {
    pub(crate) version: String,
    pub(crate) entry: String,
    pub(crate) format: String,
}

/// Verify and install one version into `versions/<version>`.
///
/// `asset.name` (the asset filename) + `version` + `computed_sha` form the §1
/// canonical signed message. The packaging `format` is derived from `asset.name`
/// (never stored or signed). `temp_path` is the downloaded file (its sha256 is
/// `computed_sha`). On success the version dir holds the unpacked tree + marker
/// and the temp file is removed; on failure no partial version dir is left.
pub(crate) fn install(
    cfg: &Config,
    version: &str,
    asset: &Asset,
    temp_path: &Path,
    computed_sha: &str,
) -> Result<()> {
    // Guard the id + advisory in-archive entry before either reaches a path join
    // (the `<version>.tmp` staging dir below, or the `entry` placed inside it).
    validate_id("version", version)?;
    if let Some(entry) = asset.entry.as_deref() {
        validate_entry(entry)?;
    }

    verify_integrity(asset, computed_sha)?;
    verify_identity(cfg, version, asset, computed_sha)?;

    let versions_dir = cfg.global.data_dir.join("versions");
    fs::create_dir_all(&versions_dir)?;
    let staging = versions_dir.join(format!("{version}.tmp"));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;

    // Stage into the `.tmp` dir; tear it down on any failure so we never leave a
    // half-extracted version behind.
    if let Err(e) = stage(cfg, asset, temp_path, &staging, version) {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }

    let final_dir = versions_dir.join(version);
    if final_dir.exists() {
        fs::remove_dir_all(&final_dir)
            .map_err(|e| Error::Install(format!("replace versions/{version}: {e}")))?;
    }
    fs::rename(&staging, &final_dir)
        .map_err(|e| Error::Install(format!("finalize versions/{version}: {e}")))?;

    let _ = fs::remove_file(temp_path);
    Ok(())
}

/// Extract the asset into `staging`, make the entry executable and write the
/// marker. The packaging `format` is derived from the asset filename (§3). Kept
/// separate so [`install`] can clean up `staging` on any error.
fn stage(
    cfg: &Config,
    asset: &Asset,
    temp_path: &Path,
    staging: &Path,
    version: &str,
) -> Result<()> {
    let format = format_from_name(&asset.name);
    let entry = extract(cfg, asset, format, temp_path, staging)?;
    chmod_x(&staging.join(&entry))?;
    write_marker(staging, version, &entry, format)?;
    Ok(())
}

/// Atomically point `current` at `versions/<version>` via a temp-symlink rename.
/// The link target is relative (`versions/<ver>`) so the data dir stays movable.
#[cfg(unix)]
pub(crate) fn switch_current(cfg: &Config, version: &str) -> Result<()> {
    let data_dir = &cfg.global.data_dir;
    let version_dir = data_dir.join("versions").join(version);
    if !version_dir.is_dir() {
        return Err(Error::Install(format!(
            "cannot activate {version}: versions/{version} is not installed"
        )));
    }
    let current = data_dir.join("current");
    let tmp = data_dir.join(format!(".current.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let target = Path::new("versions").join(version);
    std::os::unix::fs::symlink(&target, &tmp)
        .map_err(|e| Error::Install(format!("create current symlink: {e}")))?;
    fs::rename(&tmp, &current).map_err(|e| Error::Install(format!("activate current: {e}")))?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn switch_current(_cfg: &Config, _version: &str) -> Result<()> {
    Err(Error::Install(
        "symlink-based current switch is only supported on unix".to_owned(),
    ))
}

/// Prune installed versions, keeping `current` + `last_good` + the newest
/// `keep_versions` (by semver). Everything else under `versions/` is removed.
pub(crate) fn prune(cfg: &Config, current: Option<&str>, last_good: Option<&str>) -> Result<()> {
    let versions_dir = cfg.global.data_dir.join("versions");
    let installed = collect_version_dirs(&versions_dir)?;
    let keep_n = usize::try_from(cfg.update.keep_versions).unwrap_or(usize::MAX);

    let mut keep: HashSet<&str> = HashSet::new();
    if let Some(c) = current {
        keep.insert(c);
    }
    if let Some(g) = last_good {
        keep.insert(g);
    }
    for v in installed.iter().take(keep_n) {
        keep.insert(v.as_str());
    }

    for v in &installed {
        if !keep.contains(v.as_str()) {
            let dir = versions_dir.join(v);
            fs::remove_dir_all(&dir)
                .map_err(|e| Error::Install(format!("prune {}: {e}", dir.display())))?;
        }
    }
    Ok(())
}

/// Startup garbage collection: drop interrupted `downloads/*.part` and
/// half-extracted `versions/*.tmp`. Best-effort — individual failures are
/// ignored (used by the supervisor at startup, design §5).
// `Result` is part of the supervisor's startup-sequence contract (it calls `gc`
// alongside other fallible setup), even though GC itself swallows I/O errors.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn gc(cfg: &Config) -> Result<()> {
    let data_dir = &cfg.global.data_dir;
    if let Ok(entries) = fs::read_dir(data_dir.join("downloads")) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().ends_with(".part") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    if let Ok(entries) = fs::read_dir(data_dir.join("versions")) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().ends_with(".tmp") {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }
    Ok(())
}

/// Read a version's `.lode.json` marker (written at install time) so the
/// supervisor can launch it offline — `entry`/`format` without re-fetching the
/// manifest (design §15). Errors if the version is not installed.
pub(crate) fn marker(cfg: &Config, version: &str) -> Result<Marker> {
    let path = cfg
        .global
        .data_dir
        .join("versions")
        .join(version)
        .join(".lode.json");
    let bytes = fs::read(&path)
        .map_err(|e| Error::Install(format!("read marker {}: {e}", path.display())))?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Place a downloaded runtime payload into `runtime_dir`, reusing the same
/// per-`format` extraction as a version install (design §4 "[runtime]"). `raw`/`gz`
/// land as `runtime_dir/<name>`; archives are unpacked into `runtime_dir` and the
/// named binary is hoisted to `runtime_dir/<name>` (see [`hoist_runtime_bin`]) so it
/// always ends up at the root the supervisor puts on the child PATH — even when the
/// archive nests it. The placed binary is made executable. No digest/signature check
/// — the `[runtime]` config carries none.
pub(crate) fn place_runtime(
    runtime_dir: &Path,
    temp_path: &Path,
    format: &str,
    name: &str,
) -> Result<()> {
    // `name` keys `runtime/<name>`; reject any traversal before the join.
    validate_id("runtime name", name)?;
    fs::create_dir_all(runtime_dir)?;
    match format {
        "raw" => {
            fs::copy(temp_path, runtime_dir.join(name))
                .map_err(|e| Error::Install(format!("place runtime: {e}")))?;
        }
        "gz" => gunzip_file(temp_path, &runtime_dir.join(name))?,
        "tar.gz" | "tgz" => {
            unpack_tar_gz(temp_path, runtime_dir)?;
            hoist_runtime_bin(runtime_dir, name)?;
        }
        "zip" => {
            unpack_zip(temp_path, runtime_dir)?;
            hoist_runtime_bin(runtime_dir, name)?;
        }
        other => {
            return Err(Error::Install(format!(
                "unsupported runtime format {other:?}"
            )));
        }
    }
    let bin = runtime_dir.join(name);
    if bin.is_file() {
        chmod_x(&bin)?;
    }
    Ok(())
}

/// Guarantee the runtime binary sits at `runtime_dir/<name>` after an archive
/// extraction. Official runtime archives commonly nest the binary (bun's `.zip` is
/// `bun-linux-x64/bun`; node's `.tar.gz` is `node-vX/bin/node`; deno's `.zip` is a
/// flat `deno` at the root). When it isn't already at the root we locate it in the
/// extracted tree (shallowest match) and move it up, so the supervisor's single PATH
/// entry can find it. Errors when the archive contains no such file — a clearer
/// failure than a later "command not found" at launch.
fn hoist_runtime_bin(runtime_dir: &Path, name: &str) -> Result<()> {
    let target = runtime_dir.join(name);
    if target.is_file() {
        return Ok(()); // already at the root (a flat archive like deno's)
    }
    find_named_file(runtime_dir, name)?.map_or_else(
        || {
            Err(Error::Install(format!(
                "runtime binary {name:?} not found in the downloaded archive"
            )))
        },
        |found| {
            fs::rename(&found, &target)
                .map_err(|e| Error::Install(format!("hoist runtime {name:?}: {e}")))
        },
    )
}

/// Breadth-first search under `root` for a regular file whose final path component
/// equals `name`, returning the shallowest match (so a top-level binary wins over a
/// same-named file buried deeper). Symlinks are skipped, never followed, so a hostile
/// archive symlink can't redirect the search outside the tree. `root` is already
/// bounded by the extraction entry cap, so the walk is bounded.
fn find_named_file(root: &Path, name: &str) -> Result<Option<PathBuf>> {
    let wanted = OsStr::new(name);
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    queue.push_back(root.to_path_buf());
    while let Some(dir) = queue.pop_front() {
        let mut subdirs: Vec<PathBuf> = Vec::new();
        for entry in fs::read_dir(&dir).map_err(|e| Error::Install(format!("scan runtime: {e}")))? {
            let entry = entry.map_err(|e| Error::Install(format!("scan runtime: {e}")))?;
            let file_type = entry
                .file_type()
                .map_err(|e| Error::Install(format!("scan runtime: {e}")))?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_file() {
                if entry.file_name() == wanted {
                    return Ok(Some(entry.path()));
                }
            } else if file_type.is_dir() {
                subdirs.push(entry.path());
            }
        }
        queue.extend(subdirs); // enqueue this level's dirs after its files → BFS by depth
    }
    Ok(None)
}

// --- verification ----------------------------------------------------------

/// Integrity: the downloaded digest must equal the asset's `sha256` (lowercase
/// hex). `computed_sha` is already lowercase hex from [`crate::verify`].
fn verify_integrity(asset: &Asset, computed_sha: &str) -> Result<()> {
    let expected = asset.sha256.trim();
    if computed_sha.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(Error::Verify(format!(
            "sha256 mismatch: manifest {expected}, downloaded {computed_sha}"
        )))
    }
}

/// Identity: ed25519 over the §1 canonical message (binding the asset filename +
/// version + digest), enforced per `trust.require_signature`:
/// - `off` => skip (integrity only).
/// - `auto` => **fail-closed when ANY trusted key is configured**: a missing OR
///   invalid signature is rejected; skip with an UNVERIFIED warning only when no
///   key is configured.
/// - `enforce` => a valid signature (and trusted keys) is mandatory.
fn verify_identity(cfg: &Config, version: &str, asset: &Asset, computed_sha: &str) -> Result<()> {
    let keys = trusted_keys(cfg)?;
    match cfg.trust.require_signature {
        RequireSignature::Off => Ok(()),
        RequireSignature::Auto => {
            if keys.is_empty() {
                tracing::warn!(
                    version,
                    "no trusted keys configured; asset is UNVERIFIED (require_signature=auto)"
                );
                return Ok(());
            }
            // Keys are configured ⇒ fail-closed: a missing signature is rejected,
            // just like an invalid one (no more warn-and-skip).
            let sig = asset.sig.as_deref().ok_or_else(|| {
                Error::Verify(format!(
                    "trusted keys are configured but the {version} asset has no signature \
                     (require_signature=auto is fail-closed)"
                ))
            })?;
            check_sig(version, asset, computed_sha, sig, &keys)
        }
        RequireSignature::Enforce => {
            if keys.is_empty() {
                return Err(Error::Verify(
                    "require_signature=enforce but no trusted keys are configured".to_owned(),
                ));
            }
            let sig = asset.sig.as_deref().ok_or_else(|| {
                Error::Verify(format!(
                    "require_signature=enforce but the {version} asset has no signature"
                ))
            })?;
            check_sig(version, asset, computed_sha, sig, &keys)
        }
    }
}

/// Bridge to [`crate::verify::verify_artifact_sig`], mapping its error into the
/// crate's `verify:` domain. The signed identity is the asset filename
/// (`asset.name`) + version + digest — never `platform`/`format`/`entry`/`url`.
fn check_sig(version: &str, asset: &Asset, sha: &str, sig: &str, keys: &[String]) -> Result<()> {
    crate::verify::verify_artifact_sig(&asset.name, version, sha, sig, keys)
        .map_err(|e| Error::Verify(e.to_string()))
}

/// Manifest identity: the top-level ed25519 signature over the canonical
/// manifest message ([`Manifest::signing_message`]), enforced per
/// `trust.require_signature` and mirroring [`verify_identity`]'s posture:
/// - `off` => skip (the per-artifact checks still bind each download).
/// - `auto` => **fail-closed when ANY trusted key is configured**: a missing OR
///   invalid manifest signature is rejected; skip with an UNVERIFIED warning only
///   when no key is configured.
/// - `enforce` => trusted keys AND a valid manifest signature are mandatory.
///
/// Call this immediately after [`crate::manifest::fetch`], before resolving a
/// target or downloading, so a tampered catalog (swapped `latest`, added/removed
/// versions, rewritten urls) is rejected up front.
pub(crate) fn verify_manifest_identity(cfg: &Config, manifest: &Manifest) -> Result<()> {
    let keys = trusted_keys(cfg)?;
    match cfg.trust.require_signature {
        RequireSignature::Off => Ok(()),
        RequireSignature::Auto => {
            if keys.is_empty() {
                tracing::warn!(
                    manifest = manifest.name,
                    "no trusted keys configured; manifest is UNVERIFIED (require_signature=auto)"
                );
                return Ok(());
            }
            let sig = manifest.sig.as_deref().ok_or_else(|| {
                Error::Verify(
                    "trusted keys are configured but the manifest has no signature \
                     (require_signature=auto is fail-closed)"
                        .to_owned(),
                )
            })?;
            check_manifest_sig(manifest, sig, &keys)
        }
        RequireSignature::Enforce => {
            if keys.is_empty() {
                return Err(Error::Verify(
                    "require_signature=enforce but no trusted keys are configured".to_owned(),
                ));
            }
            let sig = manifest.sig.as_deref().ok_or_else(|| {
                Error::Verify(
                    "require_signature=enforce but the manifest has no signature".to_owned(),
                )
            })?;
            check_manifest_sig(manifest, sig, &keys)
        }
    }
}

/// Bridge to [`crate::verify::verify_manifest_sig`] over the manifest's canonical message,
/// mapping its error into the crate's `verify:` domain. The manifest's declared
/// `key_id` selects the trusted key (with a try-all fallback inside `verify`).
fn check_manifest_sig(manifest: &Manifest, sig: &str, keys: &[String]) -> Result<()> {
    let message = manifest.signing_message();
    crate::verify::verify_manifest_sig(keys, manifest.key_id.as_deref(), &message, sig)
        .map_err(|e| Error::Verify(e.to_string()))
}

/// A one-line, secret-free summary of the manifest's effective trust posture for
/// `status` to surface prominently. Mirrors [`verify_manifest_identity`] but
/// reports rather than enforces, so the operator sees VERIFIED / UNVERIFIED /
/// VERIFICATION FAILED consistent with `require_signature`.
pub(crate) fn manifest_trust_posture(cfg: &Config, manifest: &Manifest) -> String {
    let keys = match trusted_keys(cfg) {
        Ok(keys) => keys,
        Err(e) => return format!("VERIFICATION FAILED: {e}"),
    };
    match cfg.trust.require_signature {
        RequireSignature::Off => "off (integrity only; signature checks disabled)".to_owned(),
        RequireSignature::Auto if keys.is_empty() => {
            "UNVERIFIED (no trusted keys configured)".to_owned()
        }
        RequireSignature::Enforce if keys.is_empty() => {
            "VERIFICATION FAILED: require_signature=enforce but no trusted keys configured"
                .to_owned()
        }
        RequireSignature::Auto | RequireSignature::Enforce => {
            match verify_manifest_identity(cfg, manifest) {
                Ok(()) => "VERIFIED (manifest signature valid)".to_owned(),
                Err(e) => format!("VERIFICATION FAILED: {e}"),
            }
        }
    }
}

/// Collect trusted keys from `[trust].trusted_keys` plus, if set, the
/// `trusted_keys_file` (one `key_id base64` per line; `#` comments and blanks
/// skipped). Entry parsing is handled in [`crate::verify`]. Exposed to
/// [`crate::manifest`] so the native `.sig` sidecar fallback (§6) can ask whether a
/// signature is required (`auto` with keys, or `enforce`) using the same key set.
pub(crate) fn trusted_keys(cfg: &Config) -> Result<Vec<String>> {
    let mut keys = cfg.trust.trusted_keys.clone();
    if let Some(path) = cfg.trust.trusted_keys_file.as_deref() {
        let text = fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("read trusted_keys_file {path}: {e}")))?;
        for line in text.lines() {
            let line = line.trim();
            if !line.is_empty() && !line.starts_with('#') {
                keys.push(line.to_owned());
            }
        }
    }
    Ok(keys)
}

// --- extraction ------------------------------------------------------------

/// Hard ceiling on cumulative *decompressed* bytes written from any single
/// archive/gz, enforced regardless of the manifest `size` (which bounds the
/// *compressed* download, not the expansion). Bounds a zip/gzip bomb: a tiny
/// artifact that expands to fill the disk (design §16, `DoS` guard).
const MAX_DECOMPRESSED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Hard ceiling on the number of entries in a `tar.gz`/`zip` archive, so an
/// archive padded with millions of tiny entries can't exhaust inodes / CPU.
const MAX_ARCHIVE_ENTRIES: usize = 100_000;

/// Land the downloaded file into `dest_dir` per `format` (derived from the asset
/// filename), returning the entry's path relative to `dest_dir`. Verifies the entry
/// exists afterward. `format` is one of `raw`/`gz`/`tar.gz`/`zip`.
fn extract(
    cfg: &Config,
    asset: &Asset,
    format: &str,
    temp_path: &Path,
    dest_dir: &Path,
) -> Result<String> {
    let entry = match format {
        "raw" => {
            let rel = single_entry(cfg, asset, false)?;
            let dest = safe_join(dest_dir, &rel)?;
            ensure_parent(&dest)?;
            fs::copy(temp_path, &dest)
                .map_err(|e| Error::Install(format!("place raw asset: {e}")))?;
            rel
        }
        "gz" => {
            let rel = single_entry(cfg, asset, true)?;
            let dest = safe_join(dest_dir, &rel)?;
            ensure_parent(&dest)?;
            gunzip_file(temp_path, &dest)?;
            rel
        }
        "tar.gz" => {
            unpack_tar_gz(temp_path, dest_dir)?;
            archive_entry(cfg, asset)?
        }
        "zip" => {
            unpack_zip(temp_path, dest_dir)?;
            archive_entry(cfg, asset)?
        }
        other => return Err(Error::Install(format!("unsupported format {other:?}"))),
    };

    let entry_path = safe_join(dest_dir, &entry)?;
    if !entry_path.is_file() {
        return Err(Error::Install(format!(
            "entry {entry:?} not found after extracting {format} archive"
        )));
    }
    Ok(entry)
}

/// Resolve the entry path (§4): the advisory `asset.entry`, else the operator's
/// `[update].entry`, else `fallback` (the convention). Empty values count as unset.
fn resolved_entry(cfg: &Config, asset: &Asset, fallback: &str) -> String {
    let advisory = asset.entry.as_deref().filter(|e| !e.is_empty());
    let operator = cfg.update.entry.as_deref().filter(|e| !e.is_empty());
    advisory
        .or(operator)
        .map_or_else(|| fallback.to_owned(), ToOwned::to_owned)
}

/// The single-file entry name for `raw`/`gz` (§4): advisory > `[update].entry` >
/// the URL basename (with the `.gz` suffix stripped when `strip_gz`).
fn single_entry(cfg: &Config, asset: &Asset, strip_gz: bool) -> Result<String> {
    let base = url_basename(&asset.url);
    let fallback = if strip_gz {
        base.strip_suffix(".gz").unwrap_or(&base).to_owned()
    } else {
        base
    };
    let name = resolved_entry(cfg, asset, &fallback);
    if name.is_empty() {
        return Err(Error::Install(format!(
            "cannot determine entry filename from {}",
            asset.url
        )));
    }
    Ok(name)
}

/// The in-archive `entry` for `tar.gz`/`zip` (§4): advisory > `[update].entry` >
/// the app name (`{app}` at the archive root) as the convention.
fn archive_entry(cfg: &Config, asset: &Asset) -> Result<String> {
    let name = resolved_entry(cfg, asset, &cfg.global.app);
    if name.is_empty() {
        return Err(Error::Install(
            "cannot determine archive entry (no advisory entry, no [update].entry, empty app)"
                .to_owned(),
        ));
    }
    Ok(name)
}

/// Last path segment of a URL, with any query/fragment stripped.
fn url_basename(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit('/').next().unwrap_or(path).to_owned()
}

/// Join `rel` under `base`, rejecting absolute paths and `..` traversal so an
/// archive/entry can never escape the version dir.
fn safe_join(base: &Path, rel: &str) -> Result<PathBuf> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(Error::Install(format!(
            "unsafe entry path (absolute): {rel:?}"
        )));
    }
    for comp in rel_path.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            _ => return Err(Error::Install(format!("unsafe entry path: {rel:?}"))),
        }
    }
    Ok(base.join(rel_path))
}

/// Ensure the parent directory of `path` exists.
fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

/// Gunzip a single-file `.gz` into `dest`, refusing output past the decompressed
/// cap so a gzip bomb can't fill the disk.
fn gunzip_file(src: &Path, dest: &Path) -> Result<()> {
    gunzip_file_capped(src, dest, MAX_DECOMPRESSED_BYTES)
}

/// [`gunzip_file`] with the byte cap as a parameter so a test can drive the
/// bound with a small value (the production caller passes
/// [`MAX_DECOMPRESSED_BYTES`]).
fn gunzip_file_capped(src: &Path, dest: &Path, max_bytes: u64) -> Result<()> {
    let input = fs::File::open(src)?;
    let decoder = flate2::read::GzDecoder::new(io::BufReader::new(input));
    let mut out = fs::File::create(dest)?;
    // `take(max + 1)` lets at most one byte past the cap through, enough to detect
    // the overrun without decompressing the whole bomb.
    let mut limited = decoder.take(max_bytes.saturating_add(1));
    let written = io::copy(&mut limited, &mut out)
        .map_err(|e| Error::Install(format!("gunzip {}: {e}", src.display())))?;
    if written > max_bytes {
        return Err(Error::Install(format!(
            "gunzip {}: decompressed output exceeds {max_bytes} byte cap",
            src.display()
        )));
    }
    Ok(())
}

/// Unpack a gzip tar into `dest_dir`, bounding both the entry count and the
/// cumulative decompressed bytes (zip-bomb guard). The `tar` crate's per-entry
/// `unpack_in` guards against `..` traversal and absolute paths (entries that
/// would escape are skipped), matching the prior whole-archive `unpack`.
///
/// Permission handling: we do NOT trust archive-supplied modes. The crate
/// already masks off setuid/setgid/sticky by default (`preserve_permissions`
/// is `false`), but it would still carry the low `0o777` bits — including
/// world-writable — verbatim. So we iterate entries, unpack each safely, and
/// clamp regular files and directories to a fixed safe mode exactly like
/// [`unpack_zip`] (this also preserves the exec bit on non-entry executables,
/// e.g. bundled helper scripts). The designated entry is additionally
/// `chmod +x`'d by [`stage`].
fn unpack_tar_gz(src: &Path, dest_dir: &Path) -> Result<()> {
    unpack_tar_gz_capped(src, dest_dir, MAX_DECOMPRESSED_BYTES, MAX_ARCHIVE_ENTRIES)
}

/// [`unpack_tar_gz`] with the caps as parameters so a test can drive the bounds
/// with small values (the production caller passes the module consts). Entries
/// are iterated so the per-entry declared size and the running count are checked
/// *before* writing, rejecting a bomb without expanding it to disk.
fn unpack_tar_gz_capped(
    src: &Path,
    dest_dir: &Path,
    max_bytes: u64,
    max_entries: usize,
) -> Result<()> {
    let input = fs::File::open(src)?;
    let decoder = flate2::read::GzDecoder::new(io::BufReader::new(input));
    let mut archive = tar::Archive::new(decoder);
    // Explicit, though already the crate default: never apply suid/sgid/sticky.
    archive.set_preserve_permissions(false);
    let entries = archive
        .entries()
        .map_err(|e| Error::Install(format!("read tar.gz: {e}")))?;
    let mut count: usize = 0;
    let mut total: u64 = 0;
    for entry in entries {
        let mut entry = entry.map_err(|e| Error::Install(format!("read tar.gz entry: {e}")))?;
        count += 1;
        if count > max_entries {
            return Err(Error::Install(format!(
                "tar.gz archive has too many entries (cap {max_entries})"
            )));
        }
        total = total.saturating_add(entry.size());
        if total > max_bytes {
            return Err(Error::Install(format!(
                "tar.gz decompressed size exceeds {max_bytes} byte cap"
            )));
        }
        // `unpack_in` writes the entry under `dest_dir`, returning `Ok(false)`
        // for any path containing `..` (the same traversal guard as `unpack`).
        if !entry
            .unpack_in(dest_dir)
            .map_err(|e| Error::Install(format!("extract tar.gz: {e}")))?
        {
            continue;
        }
        #[cfg(unix)]
        clamp_tar_entry(&entry, dest_dir)?;
    }
    Ok(())
}

/// Clamp the on-disk mode of a freshly-unpacked tar `entry` (regular files and
/// directories only) to a safe permission set, mirroring [`unpack_zip`].
#[cfg(unix)]
fn clamp_tar_entry<R: io::Read>(entry: &tar::Entry<'_, R>, dest_dir: &Path) -> Result<()> {
    let entry_type = entry.header().entry_type();
    if !(entry_type.is_file() || entry_type.is_dir()) {
        return Ok(());
    }
    let mode = entry.header().mode().ok();
    let rel = entry
        .path()
        .map_err(|e| Error::Install(format!("tar.gz entry path: {e}")))?;
    // Recompute the path exactly as `unpack_in` does: only `Normal` components
    // are joined under `dest_dir` (leading `/`, `.` and prefixes are dropped).
    let mut out_path = dest_dir.to_path_buf();
    for comp in rel.components() {
        if let Component::Normal(part) = comp {
            out_path.push(part);
        }
    }
    if out_path != dest_dir {
        set_clamped_mode(&out_path, mode, entry_type.is_dir())?;
    }
    Ok(())
}

/// (`enclosed_name` returns `None`), bounding the entry count and the cumulative
/// extracted bytes (zip-bomb guard). Archive-supplied unix modes are NOT honored
/// verbatim — they are clamped via [`set_clamped_mode`] so a hostile archive
/// can't smuggle in setuid/setgid/sticky or world-writable bits.
fn unpack_zip(src: &Path, dest_dir: &Path) -> Result<()> {
    unpack_zip_capped(src, dest_dir, MAX_DECOMPRESSED_BYTES, MAX_ARCHIVE_ENTRIES)
}

/// [`unpack_zip`] with the caps as parameters so a test can drive the bounds with
/// small values (the production caller passes the module consts). The per-entry
/// copy is `take`-capped against the *remaining* byte budget so a bomb is
/// rejected mid-extraction without expanding it fully to disk.
fn unpack_zip_capped(
    src: &Path,
    dest_dir: &Path,
    max_bytes: u64,
    max_entries: usize,
) -> Result<()> {
    let file = fs::File::open(src)?;
    let mut archive = zip::ZipArchive::new(io::BufReader::new(file))
        .map_err(|e| Error::Install(format!("open zip: {e}")))?;
    if archive.len() > max_entries {
        return Err(Error::Install(format!(
            "zip archive has too many entries (cap {max_entries})"
        )));
    }
    let mut total: u64 = 0;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| Error::Install(format!("read zip entry {i}: {e}")))?;
        let Some(rel) = entry.enclosed_name() else {
            return Err(Error::Install(format!(
                "unsafe zip entry path: {:?}",
                entry.name()
            )));
        };
        let out_path = dest_dir.join(&rel);
        if entry.is_dir() {
            fs::create_dir_all(&out_path)?;
            #[cfg(unix)]
            set_clamped_mode(&out_path, entry.unix_mode(), true)?;
            continue;
        }
        ensure_parent(&out_path)?;
        let mut out = fs::File::create(&out_path)?;
        // Read at most one byte past the remaining budget — enough to detect the
        // overrun without decompressing the rest of a bomb.
        let budget = max_bytes.saturating_sub(total);
        let written = {
            let mut limited = (&mut entry).take(budget.saturating_add(1));
            io::copy(&mut limited, &mut out).map_err(|e| {
                Error::Install(format!("write zip entry {}: {e}", out_path.display()))
            })?
        };
        total = total.saturating_add(written);
        if total > max_bytes {
            return Err(Error::Install(format!(
                "zip decompressed size exceeds {max_bytes} byte cap"
            )));
        }
        #[cfg(unix)]
        set_clamped_mode(&out_path, entry.unix_mode(), false)?;
    }
    Ok(())
}

/// Clamp `path` to a safe permission set when the archive recorded a unix
/// `mode`. Always strips setuid/setgid/sticky (`0o7000`) and any group/other
/// write bits by mapping to a fixed mode: `0o755` for a directory or a file
/// carrying any execute bit (`0o111`), else `0o644`. A `None` mode (e.g. a zip
/// authored where no unix mode is recorded) leaves the default create perms.
#[cfg(unix)]
fn set_clamped_mode(path: &Path, mode: Option<u32>, is_dir: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    if let Some(mode) = mode {
        let clamped = if is_dir || mode & 0o111 != 0 {
            0o755
        } else {
            0o644
        };
        fs::set_permissions(path, fs::Permissions::from_mode(clamped))?;
    }
    Ok(())
}

/// Make `path` executable (`+x`). No-op off unix; harmless for scripts.
#[cfg(unix)]
fn chmod_x(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() | 0o111);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn chmod_x(_path: &Path) -> Result<()> {
    Ok(())
}

/// Write the per-version `.lode.json` marker into `dir`.
fn write_marker(dir: &Path, version: &str, entry: &str, format: &str) -> Result<()> {
    let marker = Marker {
        version: version.to_owned(),
        entry: entry.to_owned(),
        format: format.to_owned(),
    };
    let json = serde_json::to_vec_pretty(&marker)?;
    fs::write(dir.join(".lode.json"), json)?;
    Ok(())
}

/// Directory names under `versions/`, newest-first, excluding `*.tmp` staging.
fn collect_version_dirs(versions_dir: &Path) -> Result<Vec<String>> {
    let entries = match fs::read_dir(versions_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        let is_staging = Path::new(&name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("tmp"));
        if !is_staging {
            out.push(name);
        }
    }
    out.sort_by(|a, b| cmp_desc(a, b));
    Ok(out)
}

/// Newest-first version order: valid semver by precedence (descending) ahead of
/// any non-semver name; non-semver falls back to reverse lexicographic.
fn cmp_desc(a: &str, b: &str) -> Ordering {
    match (semver::Version::parse(a), semver::Version::parse(b)) {
        (Ok(x), Ok(y)) => y.cmp(&x),
        (Ok(_), Err(_)) => Ordering::Less,
        (Err(_), Ok(_)) => Ordering::Greater,
        (Err(_), Err(_)) => b.cmp(a),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use base64::Engine as _;
    use ed25519_dalek::{Signer as _, SigningKey};

    use super::*;
    use crate::config::{
        Command, Global, Http, Policy, Readiness, RequireSignature, RestartMode, RestartPolicy,
        Runtime, Signals, Supervise, Trust, Update,
    };

    const B64: base64::engine::general_purpose::GeneralPurpose =
        base64::engine::general_purpose::STANDARD;

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lode-install-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cfg_for(
        data_dir: PathBuf,
        require_signature: RequireSignature,
        trusted_keys: Vec<String>,
    ) -> Config {
        Config {
            global: Global {
                app: "myapp".to_owned(),
                data_dir,
                log_level: "info".to_owned(),
            },
            update: Update {
                manifest: None,
                github: None,
                github_api: "https://api.github.com".to_owned(),
                asset: None,
                entry: None,
                channel: "stable".to_owned(),
                policy: Policy::Check,
                check_interval: 300,
                keep_versions: 3,
                pin: None,
            },
            http: Http {
                headers: Vec::new(),
                credential_hosts: Vec::new(),
                allow_insecure: false,
            },
            trust: Trust {
                require_signature,
                trusted_keys,
                trusted_keys_file: None,
            },
            command: Command {
                run: "{entry}".to_owned(),
                exec: "{entry}".to_owned(),
                workdir: "{dir}".to_owned(),
            },
            runtime: Runtime {
                runtime: None,
                download: None,
                version: None,
                version_check: None,
            },
            supervise: Supervise {
                restart: RestartPolicy::Off,
                restart_backoff: 500,
                restart_backoff_max: 30_000,
                restart_max: 0,
                readiness: Readiness::None,
                ready_timeout: 30,
                health_grace: 15,
                stop_timeout: 10,
                restart_mode: RestartMode::StopStart,
                listen: None,
            },
            signals: Signals {
                forward: Vec::new(),
                restart: None,
            },
            env: std::collections::BTreeMap::new(),
        }
    }

    /// Write `bytes` to `downloads/<ver>.part` and return (path, sha256).
    fn stage_download(cfg: &Config, version: &str, bytes: &[u8]) -> (PathBuf, String) {
        let downloads = cfg.global.data_dir.join("downloads");
        fs::create_dir_all(&downloads).unwrap();
        let temp = downloads.join(format!("{version}.part"));
        fs::write(&temp, bytes).unwrap();
        let sha = crate::verify::sha256_hex(bytes);
        (temp, sha)
    }

    /// Build an [`Asset`]; `name`'s suffix drives the derived packaging format and
    /// is the §1 signed identity. `url` is fixed to a loopback `app.bin`.
    fn asset(name: &str, sha256: &str, entry: Option<&str>) -> Asset {
        Asset {
            name: name.to_owned(),
            url: "http://127.0.0.1/app.bin".to_owned(),
            sha256: sha256.to_owned(),
            sig: None,
            key_id: None,
            entry: entry.map(ToOwned::to_owned),
            size: None,
            auth: true,
        }
    }

    fn read_marker(dir: &Path, version: &str) -> Marker {
        let bytes = fs::read(dir.join("versions").join(version).join(".lode.json")).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[cfg(unix)]
    fn is_executable(path: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt as _;
        fs::metadata(path).unwrap().permissions().mode() & 0o111 != 0
    }

    #[test]
    fn installs_raw_artifact() {
        let dir = scratch("raw");
        let cfg = cfg_for(dir.clone(), RequireSignature::Off, Vec::new());
        let body = b"#!/bin/sh\necho hi\n";
        let (temp, sha) = stage_download(&cfg, "1.0.0", body);
        let art = asset("app.bin", &sha, Some("app.sh"));

        install(&cfg, "1.0.0", &art, &temp, &sha).unwrap();

        let entry = dir.join("versions/1.0.0/app.sh");
        assert_eq!(fs::read(&entry).unwrap(), body);
        assert!(!temp.exists(), "the .part download is removed on success");
        let marker = read_marker(&dir, "1.0.0");
        assert_eq!(marker.entry, "app.sh");
        assert_eq!(marker.format, "raw");
        #[cfg(unix)]
        assert!(is_executable(&entry));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn raw_entry_defaults_to_url_basename() {
        let dir = scratch("rawbase");
        let cfg = cfg_for(dir.clone(), RequireSignature::Off, Vec::new());
        let body = b"binary-bytes";
        let (temp, sha) = stage_download(&cfg, "2.0.0", body);
        let art = asset("app.bin", &sha, None); // url basename = app.bin

        install(&cfg, "2.0.0", &art, &temp, &sha).unwrap();
        assert_eq!(fs::read(dir.join("versions/2.0.0/app.bin")).unwrap(), body);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn installs_gz_artifact() {
        let dir = scratch("gz");
        let cfg = cfg_for(dir.clone(), RequireSignature::Off, Vec::new());
        let plain = b"decompressed contents\n";
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(plain).unwrap();
        let gz = enc.finish().unwrap();
        let (temp, sha) = stage_download(&cfg, "1.2.0", &gz);
        let art = asset("app.gz", &sha, Some("app"));

        install(&cfg, "1.2.0", &art, &temp, &sha).unwrap();
        assert_eq!(fs::read(dir.join("versions/1.2.0/app")).unwrap(), plain);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn installs_tar_gz_artifact() {
        let dir = scratch("targz");
        let cfg = cfg_for(dir.clone(), RequireSignature::Off, Vec::new());
        let file_bytes = b"tarred binary\n";
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(file_bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "bin/app", &file_bytes[..])
            .unwrap();
        let tar_bytes = builder.into_inner().unwrap();
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&tar_bytes).unwrap();
        let targz = enc.finish().unwrap();

        let (temp, sha) = stage_download(&cfg, "1.3.0", &targz);
        let art = asset("app.tar.gz", &sha, Some("bin/app"));

        install(&cfg, "1.3.0", &art, &temp, &sha).unwrap();
        let entry = dir.join("versions/1.3.0/bin/app");
        assert_eq!(fs::read(&entry).unwrap(), file_bytes);
        #[cfg(unix)]
        assert!(is_executable(&entry));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn installs_zip_artifact() {
        let dir = scratch("zip");
        let cfg = cfg_for(dir.clone(), RequireSignature::Off, Vec::new());
        let file_bytes = b"zipped binary\n";
        let mut zw = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file("app", opts).unwrap();
        zw.write_all(file_bytes).unwrap();
        let zip_bytes = zw.finish().unwrap().into_inner();

        let (temp, sha) = stage_download(&cfg, "1.4.0", &zip_bytes);
        let art = asset("app.zip", &sha, Some("app"));

        install(&cfg, "1.4.0", &art, &temp, &sha).unwrap();
        assert_eq!(
            fs::read(dir.join("versions/1.4.0/app")).unwrap(),
            file_bytes
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[cfg(unix)]
    #[test]
    fn unpack_zip_clamps_setuid_and_perms() {
        let dir = scratch("zipperms");
        let dest = dir.join("out");
        fs::create_dir_all(&dest).unwrap();

        let opts = |mode: u32| {
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .unix_permissions(mode)
        };
        let mut zw = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        // A setuid + world-writable executable: must lose suid/sgid/sticky and
        // group/other write, but stay executable -> 0o755.
        zw.start_file("evil", opts(0o4777)).unwrap();
        zw.write_all(b"#!/bin/sh\n").unwrap();
        // A plain data file: stays 0o644.
        zw.start_file("data", opts(0o644)).unwrap();
        zw.write_all(b"data").unwrap();
        // A directory entry with sgid + sticky + world-writable -> 0o755.
        zw.add_directory("sub", opts(0o3777)).unwrap();
        let zip_bytes = zw.finish().unwrap().into_inner();

        let src = dir.join("a.zip");
        fs::write(&src, &zip_bytes).unwrap();
        unpack_zip(&src, &dest).unwrap();

        let evil = dest.join("evil");
        assert_eq!(mode_of(&evil) & 0o7000, 0, "setuid/sgid/sticky stripped");
        assert_eq!(mode_of(&evil), 0o755, "executable stays runnable, clamped");
        assert_eq!(
            mode_of(&dest.join("data")),
            0o644,
            "data file clamped to 0o644"
        );
        assert_eq!(
            mode_of(&dest.join("sub")),
            0o755,
            "directory clamped to 0o755"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn unpack_tar_gz_clamps_setuid_and_perms() {
        let dir = scratch("tarperms");
        let dest = dir.join("out");
        fs::create_dir_all(&dest).unwrap();

        let mut builder = tar::Builder::new(Vec::new());
        let append = |b: &mut tar::Builder<Vec<u8>>, name: &str, mode: u32, body: &[u8]| {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(mode);
            header.set_cksum();
            b.append_data(&mut header, name, body).unwrap();
        };
        // setuid + world-writable exec, plain data file, and a sgid dir.
        append(&mut builder, "bin/evil", 0o4777, b"#!/bin/sh\n");
        append(&mut builder, "share/data", 0o646, b"data");
        let mut dh = tar::Header::new_gnu();
        dh.set_entry_type(tar::EntryType::Directory);
        dh.set_size(0);
        dh.set_mode(0o2777);
        dh.set_cksum();
        builder.append_data(&mut dh, "lib/", &b""[..]).unwrap();
        let tar_bytes = builder.into_inner().unwrap();
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&tar_bytes).unwrap();
        let targz = enc.finish().unwrap();

        let src = dir.join("a.tar.gz");
        fs::write(&src, &targz).unwrap();
        unpack_tar_gz(&src, &dest).unwrap();

        let evil = dest.join("bin/evil");
        assert_eq!(mode_of(&evil) & 0o7000, 0, "setuid/sgid/sticky stripped");
        assert_eq!(mode_of(&evil), 0o755, "executable stays runnable, clamped");
        assert_eq!(
            mode_of(&dest.join("share/data")),
            0o644,
            "data file clamped to 0o644 (world-write dropped)"
        );
        assert_eq!(
            mode_of(&dest.join("lib")) & 0o7000,
            0,
            "dir has no special bits"
        );
        assert_eq!(
            mode_of(&dest.join("lib")),
            0o755,
            "directory clamped to 0o755"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_sha256_mismatch() {
        let dir = scratch("badsha");
        let cfg = cfg_for(dir.clone(), RequireSignature::Off, Vec::new());
        let (temp, _real) = stage_download(&cfg, "1.0.0", b"abc");
        let art = asset("app.bin", "00".repeat(32).as_str(), Some("app"));

        let err = install(
            &cfg,
            "1.0.0",
            &art,
            &temp,
            &crate::verify::sha256_hex(b"abc"),
        )
        .unwrap_err();
        assert!(matches!(err, Error::Verify(_)));
        assert!(!dir.join("versions/1.0.0").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn enforce_verifies_signature_happy_and_tampered() {
        let dir = scratch("sig");
        let signing = SigningKey::from_bytes(&[3u8; 32]);
        let pub_b64 = B64.encode(signing.verifying_key().to_bytes());
        let cfg = cfg_for(
            dir.clone(),
            RequireSignature::Enforce,
            vec![format!("testid:{pub_b64}")],
        );

        let body = b"signed payload";
        let (temp, sha) = stage_download(&cfg, "1.0.0", body);
        // Canonical §1 message: asset filename + version + digest only.
        // `asset("app.bin", …)` → the signed `name` is the filename `app.bin`.
        let msg = format!("lode.artifact.v1\napp.bin\n1.0.0\n{sha}");
        let sig = B64.encode(signing.sign(msg.as_bytes()).to_bytes());

        let mut art = asset("app.bin", &sha, Some("app"));
        art.sig = Some(sig);
        install(&cfg, "1.0.0", &art, &temp, &sha).unwrap();
        assert!(dir.join("versions/1.0.0/app").exists());

        // Tampered: an untrusted key signs the same message → must fail.
        let dir2 = scratch("sig2");
        let attacker = SigningKey::from_bytes(&[9u8; 32]);
        let cfg2 = cfg_for(
            dir2.clone(),
            RequireSignature::Enforce,
            vec![format!("testid:{pub_b64}")],
        );
        let (temp2, sha2) = stage_download(&cfg2, "1.0.0", body);
        let msg2 = format!("lode.artifact.v1\napp.bin\n1.0.0\n{sha2}");
        let bad_sig = B64.encode(attacker.sign(msg2.as_bytes()).to_bytes());
        let mut art2 = asset("app.bin", &sha2, Some("app"));
        art2.sig = Some(bad_sig);
        let err = install(&cfg2, "1.0.0", &art2, &temp2, &sha2).unwrap_err();
        assert!(matches!(err, Error::Verify(_)));
        assert!(!dir2.join("versions/1.0.0").exists());

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&dir2);
    }

    #[test]
    fn enforce_without_signature_errors() {
        let dir = scratch("nosig");
        let signing = SigningKey::from_bytes(&[5u8; 32]);
        let pub_b64 = B64.encode(signing.verifying_key().to_bytes());
        let cfg = cfg_for(dir.clone(), RequireSignature::Enforce, vec![pub_b64]);
        let (temp, sha) = stage_download(&cfg, "1.0.0", b"x");
        let art = asset("app.bin", &sha, Some("app")); // sig=None
        let err = install(&cfg, "1.0.0", &art, &temp, &sha).unwrap_err();
        assert!(matches!(err, Error::Verify(_)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_with_keys_is_fail_closed_for_missing_artifact_signature() {
        // require_signature=auto + a configured key REJECTS an unsigned artifact: fail-closed.
        let dir = scratch("autoartmissing");
        let signing = SigningKey::from_bytes(&[15u8; 32]);
        let pub_b64 = B64.encode(signing.verifying_key().to_bytes());
        let cfg = cfg_for(
            dir.clone(),
            RequireSignature::Auto,
            vec![format!("kid:{pub_b64}")],
        );
        let (temp, sha) = stage_download(&cfg, "1.0.0", b"x");
        let art = asset("app.bin", &sha, Some("app")); // sig=None
        let err = install(&cfg, "1.0.0", &art, &temp, &sha).unwrap_err();
        assert!(matches!(err, Error::Verify(_)));
        assert!(!dir.join("versions/1.0.0").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_without_keys_skips_artifact_signature() {
        // No trusted keys ⇒ auto skips the signature check (UNVERIFIED) and installs.
        let dir = scratch("autoartnokeys");
        let cfg = cfg_for(dir.clone(), RequireSignature::Auto, Vec::new());
        let body = b"#!/bin/sh\necho hi\n";
        let (temp, sha) = stage_download(&cfg, "1.0.0", body);
        let art = asset("app.bin", &sha, Some("app")); // sig=None, but no keys
        install(&cfg, "1.0.0", &art, &temp, &sha).unwrap();
        assert!(dir.join("versions/1.0.0/app").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A one-channel, one-version, one-artifact manifest for the manifest-signature
    /// tests (no I/O — built directly from the struct).
    fn manifest_fixture() -> Manifest {
        use std::collections::BTreeMap;

        use crate::manifest::{Channel, VersionEntry};

        let mut channels = BTreeMap::new();
        channels.insert(
            "stable".to_owned(),
            Channel {
                latest: "1.0.0".to_owned(),
            },
        );
        let mut versions = BTreeMap::new();
        versions.insert(
            "1.0.0".to_owned(),
            VersionEntry {
                notes: None,
                assets: vec![asset("app.bin", "abc", Some("app"))],
            },
        );
        Manifest {
            schema: "lode/v1".to_owned(),
            name: "myapp".to_owned(),
            key_id: None,
            sig: None,
            channels,
            versions,
            block_unsigned_latest: false,
        }
    }

    /// Sign `manifest` in place (stamp `key_id` + `sig`) with `signing`, mirroring
    /// `lode-cli manifest-sign`.
    fn sign_manifest(manifest: &mut Manifest, signing: &SigningKey) {
        let id = crate::verify::key_id(&signing.verifying_key().to_bytes());
        manifest.key_id = Some(id);
        let sig = B64.encode(signing.sign(&manifest.signing_message()).to_bytes());
        manifest.sig = Some(sig);
    }

    #[test]
    fn verify_manifest_identity_happy_tampered_and_fail_closed() {
        let dir = scratch("manifestsig");
        let signing = SigningKey::from_bytes(&[20u8; 32]);
        let id = crate::verify::key_id(&signing.verifying_key().to_bytes());
        let pub_b64 = B64.encode(signing.verifying_key().to_bytes());
        let trusted = vec![format!("{id}:{pub_b64}")];

        let mut m = manifest_fixture();
        sign_manifest(&mut m, &signing);

        let enforce = cfg_for(dir.clone(), RequireSignature::Enforce, trusted.clone());
        let auto = cfg_for(dir.clone(), RequireSignature::Auto, trusted.clone());

        // Valid signature → ok under both enforce and auto.
        assert!(verify_manifest_identity(&enforce, &m).is_ok());
        assert!(verify_manifest_identity(&auto, &m).is_ok());
        // …and the status posture reports VERIFIED.
        assert!(manifest_trust_posture(&enforce, &m).starts_with("VERIFIED"));

        // Tampered catalog (channel latest swapped) → the signature no longer matches.
        let mut tampered = m.clone();
        tampered.channels.insert(
            "stable".to_owned(),
            crate::manifest::Channel {
                latest: "9.9.9".to_owned(),
            },
        );
        assert!(verify_manifest_identity(&enforce, &tampered).is_err());
        assert!(manifest_trust_posture(&enforce, &tampered).starts_with("VERIFICATION FAILED"));

        // auto + keys + MISSING manifest sig → fail-closed (reject).
        let mut unsigned = m.clone();
        unsigned.sig = None;
        assert!(verify_manifest_identity(&auto, &unsigned).is_err());
        assert!(verify_manifest_identity(&enforce, &unsigned).is_err());

        // off → skip even without a signature.
        let off = cfg_for(dir.clone(), RequireSignature::Off, trusted);
        assert!(verify_manifest_identity(&off, &unsigned).is_ok());

        // auto + NO keys → skip (UNVERIFIED), even without a signature.
        let nokeys = cfg_for(dir.clone(), RequireSignature::Auto, Vec::new());
        assert!(verify_manifest_identity(&nokeys, &unsigned).is_ok());
        assert!(manifest_trust_posture(&nokeys, &unsigned).starts_with("UNVERIFIED"));

        // A trusted key that did not sign this manifest → reject.
        let other_pub = B64.encode(
            SigningKey::from_bytes(&[99u8; 32])
                .verifying_key()
                .to_bytes(),
        );
        let wrong = cfg_for(
            dir.clone(),
            RequireSignature::Enforce,
            vec![format!("x:{other_pub}")],
        );
        assert!(verify_manifest_identity(&wrong, &m).is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_unsafe_entry_path() {
        assert!(safe_join(Path::new("/base"), "../escape").is_err());
        assert!(safe_join(Path::new("/base"), "/etc/passwd").is_err());
        assert!(safe_join(Path::new("/base"), "ok/nested").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn switch_current_is_relative_symlink() {
        let dir = scratch("switch");
        let cfg = cfg_for(dir.clone(), RequireSignature::Off, Vec::new());
        fs::create_dir_all(dir.join("versions/1.5.0")).unwrap();
        switch_current(&cfg, "1.5.0").unwrap();
        let target = fs::read_link(dir.join("current")).unwrap();
        assert_eq!(target, Path::new("versions/1.5.0"));
        // Re-activation overwrites the existing symlink atomically.
        fs::create_dir_all(dir.join("versions/1.6.0")).unwrap();
        switch_current(&cfg, "1.6.0").unwrap();
        assert_eq!(
            fs::read_link(dir.join("current")).unwrap(),
            Path::new("versions/1.6.0")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_keeps_current_last_good_and_recent() {
        let dir = scratch("prune");
        let mut cfg = cfg_for(dir.clone(), RequireSignature::Off, Vec::new());
        cfg.update.keep_versions = 2;
        let versions = dir.join("versions");
        for v in ["1.0.0", "1.1.0", "1.2.0", "1.3.0", "1.4.0"] {
            fs::create_dir_all(versions.join(v)).unwrap();
        }
        // keep newest 2 = {1.4.0, 1.3.0}, plus current=1.1.0, last_good=1.0.0
        prune(&cfg, Some("1.1.0"), Some("1.0.0")).unwrap();
        for v in ["1.4.0", "1.3.0", "1.1.0", "1.0.0"] {
            assert!(versions.join(v).exists(), "{v} should be kept");
        }
        assert!(!versions.join("1.2.0").exists(), "1.2.0 should be pruned");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn marker_reads_back_after_install() {
        let dir = scratch("marker");
        let cfg = cfg_for(dir.clone(), RequireSignature::Off, Vec::new());
        let body = b"#!/bin/sh\necho hi\n";
        let (temp, sha) = stage_download(&cfg, "1.0.0", body);
        let art = asset("app.bin", &sha, Some("app.sh"));
        install(&cfg, "1.0.0", &art, &temp, &sha).unwrap();

        let m = marker(&cfg, "1.0.0").unwrap();
        assert_eq!(m.version, "1.0.0");
        assert_eq!(m.entry, "app.sh");
        assert_eq!(m.format, "raw");
        assert!(marker(&cfg, "9.9.9").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn place_runtime_raw_and_gz() {
        let dir = scratch("runtime");
        let runtime_dir = dir.join("runtime");

        // raw: copied verbatim to runtime/<name> and made executable.
        let raw = dir.join("bun.part");
        fs::write(&raw, b"#!/bin/sh\necho bun\n").unwrap();
        place_runtime(&runtime_dir, &raw, "raw", "bun").unwrap();
        let placed = runtime_dir.join("bun");
        assert_eq!(fs::read(&placed).unwrap(), b"#!/bin/sh\necho bun\n");
        #[cfg(unix)]
        assert!(is_executable(&placed));

        // gz: gunzipped to runtime/<name>.
        let plain = b"decoded runtime\n";
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(plain).unwrap();
        let gz_bytes = enc.finish().unwrap();
        let gz = dir.join("rt.gz");
        fs::write(&gz, &gz_bytes).unwrap();
        place_runtime(&runtime_dir, &gz, "gz", "tool").unwrap();
        assert_eq!(fs::read(runtime_dir.join("tool")).unwrap(), plain);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn place_runtime_hoists_nested_archive_binary() {
        let dir = scratch("runtime-hoist");

        // bun-style .zip: the binary is nested under bun-linux-x64/bun.
        let zd = dir.join("rt-zip");
        let zsrc = dir.join("bun.zip");
        fs::write(
            &zsrc,
            zip_of(&[("bun-linux-x64/bun", b"#!/bin/sh\necho bun\n")]),
        )
        .unwrap();
        place_runtime(&zd, &zsrc, "zip", "bun").unwrap();
        let bun = zd.join("bun"); // hoisted to the root
        assert_eq!(fs::read(&bun).unwrap(), b"#!/bin/sh\necho bun\n");
        #[cfg(unix)]
        assert!(is_executable(&bun));

        // node-style .tar.gz: the binary is nested under node-vX/bin/node, with
        // siblings (npm) that must be ignored.
        let td = dir.join("rt-tar");
        let tsrc = dir.join("node.tar.gz");
        fs::write(
            &tsrc,
            tar_gz_of(&[
                ("node-v22.0.0-linux-x64/bin/node", b"node-bin\n"),
                ("node-v22.0.0-linux-x64/bin/npm", b"npm-script\n"),
            ]),
        )
        .unwrap();
        place_runtime(&td, &tsrc, "tar.gz", "node").unwrap();
        assert_eq!(fs::read(td.join("node")).unwrap(), b"node-bin\n");

        // deno-style flat .zip: already at the root → left in place.
        let dd = dir.join("rt-flat");
        let dsrc = dir.join("deno.zip");
        fs::write(&dsrc, zip_of(&[("deno", b"deno-bin\n")])).unwrap();
        place_runtime(&dd, &dsrc, "zip", "deno").unwrap();
        assert_eq!(fs::read(dd.join("deno")).unwrap(), b"deno-bin\n");

        // archive missing the named binary → clear error (not a silent no-op).
        let ed = dir.join("rt-missing");
        let esrc = dir.join("other.zip");
        fs::write(&esrc, zip_of(&[("some/other-tool", b"x\n")])).unwrap();
        let err = place_runtime(&ed, &esrc, "zip", "bun").unwrap_err();
        assert!(matches!(err, Error::Install(_)));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn gc_removes_part_and_tmp() {
        let dir = scratch("gc");
        let cfg = cfg_for(dir.clone(), RequireSignature::Off, Vec::new());
        fs::create_dir_all(dir.join("downloads")).unwrap();
        fs::create_dir_all(dir.join("versions/1.0.0")).unwrap();
        fs::create_dir_all(dir.join("versions/1.1.0.tmp")).unwrap();
        fs::write(dir.join("downloads/1.0.0.part"), b"partial").unwrap();

        gc(&cfg).unwrap();
        assert!(!dir.join("downloads/1.0.0.part").exists());
        assert!(!dir.join("versions/1.1.0.tmp").exists());
        assert!(dir.join("versions/1.0.0").exists(), "real version kept");
        let _ = fs::remove_dir_all(&dir);
    }

    // --- decompression caps (zip-bomb / DoS guard) ------------------------
    //
    // The caps are exercised via the `*_capped` helpers with small limits so a
    // tiny, fast fixture provably trips a bound; in production the same helpers
    // run with the generous module consts.

    fn gz_of(plain: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(plain).unwrap();
        enc.finish().unwrap()
    }

    fn tar_gz_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, bytes) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, &bytes[..]).unwrap();
        }
        gz_of(&builder.into_inner().unwrap())
    }

    fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zw = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in files {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(bytes).unwrap();
        }
        zw.finish().unwrap().into_inner()
    }

    #[test]
    fn gunzip_rejects_output_past_cap() {
        let dir = scratch("gzbomb");
        // 4 KiB of zeros compresses to a handful of bytes — a fast stand-in for
        // a gzip bomb: small on disk, large when expanded.
        let src = dir.join("bomb.gz");
        fs::write(&src, gz_of(&vec![0u8; 4096])).unwrap();
        let dest = dir.join("out");

        let err = gunzip_file_capped(&src, &dest, 1024).unwrap_err();
        assert!(matches!(err, Error::Install(_)));
        // A generous cap lets the same input through.
        gunzip_file_capped(&src, &dest, 8192).unwrap();
        assert_eq!(fs::read(&dest).unwrap().len(), 4096);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unpack_tar_gz_rejects_too_many_entries() {
        let dir = scratch("tarcount");
        let names: Vec<String> = (0..5).map(|i| format!("f{i}")).collect();
        let files: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b"x"[..])).collect();
        let src = dir.join("a.tar.gz");
        fs::write(&src, tar_gz_of(&files)).unwrap();
        let out = dir.join("out");
        fs::create_dir_all(&out).unwrap();

        let err = unpack_tar_gz_capped(&src, &out, u64::MAX, 3).unwrap_err();
        assert!(matches!(err, Error::Install(_)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unpack_tar_gz_rejects_oversize() {
        let dir = scratch("tarsize");
        let src = dir.join("a.tar.gz");
        fs::write(&src, tar_gz_of(&[("big", &vec![0u8; 4096])])).unwrap();
        let out = dir.join("out");
        fs::create_dir_all(&out).unwrap();

        let err = unpack_tar_gz_capped(&src, &out, 1024, 100).unwrap_err();
        assert!(matches!(err, Error::Install(_)));
        // Generous caps → extracts.
        let out2 = dir.join("out2");
        fs::create_dir_all(&out2).unwrap();
        unpack_tar_gz_capped(&src, &out2, 8192, 100).unwrap();
        assert_eq!(fs::read(out2.join("big")).unwrap().len(), 4096);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unpack_zip_rejects_too_many_entries() {
        let dir = scratch("zipcount");
        let src = dir.join("a.zip");
        fs::write(&src, zip_of(&[("a", b"1"), ("b", b"2"), ("c", b"3")])).unwrap();
        let out = dir.join("out");
        fs::create_dir_all(&out).unwrap();

        let err = unpack_zip_capped(&src, &out, u64::MAX, 2).unwrap_err();
        assert!(matches!(err, Error::Install(_)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unpack_zip_rejects_cumulative_oversize() {
        let dir = scratch("zipsize");
        // Two 2 KiB entries: cumulative 4 KiB trips a 3 KiB cap — proving the cap
        // is tracked *across* entries, not per-entry.
        let src = dir.join("a.zip");
        fs::write(
            &src,
            zip_of(&[("a", &vec![1u8; 2048]), ("b", &vec![2u8; 2048])]),
        )
        .unwrap();
        let out = dir.join("out");
        fs::create_dir_all(&out).unwrap();

        let err = unpack_zip_capped(&src, &out, 3072, 100).unwrap_err();
        assert!(matches!(err, Error::Install(_)));
        // Generous cap → both entries extract.
        let out2 = dir.join("out2");
        fs::create_dir_all(&out2).unwrap();
        unpack_zip_capped(&src, &out2, 1024 * 1024, 100).unwrap();
        assert_eq!(fs::read(out2.join("a")).unwrap().len(), 2048);
        assert_eq!(fs::read(out2.join("b")).unwrap().len(), 2048);
        let _ = fs::remove_dir_all(&dir);
    }
}
