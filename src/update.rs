//! Self-update against the rolling `latest` GitHub release.
//!
//! CI (see `.github/workflows/ci.yml`, `publish` job) re-points the `latest`
//! tag at every successful develop release and attaches the raw per-platform
//! binaries plus a `SHA256SUMS` manifest over them. The updater compares the
//! SHA-256 of the running executable's file against the manifest entry for
//! this platform; on mismatch it downloads the raw binary, verifies its hash,
//! stages it in full at a sibling `<exe>.new` path, and only then swaps it in
//! (atomic rename on Unix; move-aside-to-`.old` on Windows) — so the target
//! path is never missing for the length of a download, and a crash mid-update
//! never leaves the install empty. The new binary takes effect on the next
//! launch.
//!
//! Entry points: `/update` (manual) and the startup check when
//! `[update] auto = true` (`/autoupdate on`) — both funnel through [`run`],
//! spawned off the event loop in `app::keys::dispatch` / `App::new`.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Direct-download base for assets of the release tagged `latest`.
const RELEASE_DOWNLOAD_BASE: &str =
    "https://github.com/Aver005/pooprusteek/releases/download/latest";
/// The checksum manifest asset: `sha256sum`-format lines over the raw
/// (uncompressed) release binaries.
const CHECKSUMS_ASSET: &str = "SHA256SUMS";

/// The raw-binary asset name for the platform this build targets, or `None`
/// when CI publishes no binary for it (auto-update unsupported). Must stay in
/// step with the `publish` job's extraction step.
pub fn platform_asset() -> Option<&'static str> {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("pooprusteek-windows-x86_64.exe")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("pooprusteek-linux-x86_64")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("pooprusteek-macos-arm64")
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// The on-disk binary already matches the `latest` release.
    UpToDate,
    /// The binary was replaced on disk; takes effect on the next launch.
    Updated { new_hash: String },
}

/// Check the `latest` release and install it when the local binary differs.
/// Network + hashing + the file swap all happen here — callers must run this
/// off the event loop (spawned task) and report the result via `AppEvent`.
pub async fn run() -> Result<UpdateOutcome, String> {
    let Some(asset) = platform_asset() else {
        return Err("auto-update isn't supported for this platform/architecture".to_string());
    };
    let exe =
        std::env::current_exe().map_err(|e| format!("can't locate the running executable: {e}"))?;

    let client = http_client()?;
    let sums_text = fetch(
        &client,
        &format!("{RELEASE_DOWNLOAD_BASE}/{CHECKSUMS_ASSET}"),
    )
    .await?;
    let sums_text = String::from_utf8_lossy(&sums_text);
    let remote_hash = parse_sha256sums(&sums_text, asset).ok_or_else(|| {
        format!("{CHECKSUMS_ASSET} in the latest release has no entry for {asset}")
    })?;

    let local_hash = {
        let exe = exe.clone();
        spawn_blocking(move || {
            let bytes =
                std::fs::read(&exe).map_err(|e| format!("can't read {}: {e}", exe.display()))?;
            Ok(sha256_hex(&bytes))
        })
        .await?
    };
    if local_hash == remote_hash {
        return Ok(UpdateOutcome::UpToDate);
    }

    let bytes = fetch(&client, &format!("{RELEASE_DOWNLOAD_BASE}/{asset}")).await?;
    let expected = remote_hash.clone();
    spawn_blocking(move || {
        if sha256_hex(&bytes) != expected {
            return Err(
                "downloaded binary doesn't match its published checksum — aborting (try /update again)"
                    .to_string(),
            );
        }
        install(&exe, &bytes)
    })
    .await?;

    Ok(UpdateOutcome::Updated {
        new_hash: remote_hash,
    })
}

/// Delete the leftovers a previous update may have left next to the binary:
/// the `<exe>.old` backup (Windows can't remove it while that process runs,
/// so the swap defers it to the next launch) and any `<exe>.new` staged file
/// stranded by a crash between staging and the swap. Call once at startup;
/// silent best-effort (nothing exists on most launches).
pub fn cleanup_stale_backup() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(backup_path(&exe));
        let _ = std::fs::remove_file(staged_path(&exe));
    }
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(concat!("pooprusteek/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(10))
        // Generous whole-request cap: the raw binary is tens of MB and phone
        // hotspots are slow, but a stalled download must not hang forever.
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("request to {url} failed: {e}"))?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(
            "the `latest` release (or this asset) isn't published yet — HTTP 404".to_string(),
        );
    }
    if !status.is_success() {
        return Err(format!("HTTP {status} fetching {url}"));
    }
    Ok(response
        .bytes()
        .await
        .map_err(|e| format!("download from {url} failed: {e}"))?
        .to_vec())
}

/// Find `asset`'s hash in `sha256sum`-format text: `<64-hex>  <name>` per
/// line (a leading `*` binary marker on the name is tolerated).
fn parse_sha256sums(text: &str, asset: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?;
        let name = name.strip_prefix('*').unwrap_or(name);
        (name == asset && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `pooprusteek.exe` → `pooprusteek.exe.old` (appends, never replaces, the
/// extension — `with_extension` would turn `.exe` into `.old`).
fn backup_path(exe: &Path) -> PathBuf {
    let mut name = exe.as_os_str().to_owned();
    name.push(".old");
    PathBuf::from(name)
}

/// `pooprusteek.exe` → `pooprusteek.exe.new` — where the downloaded binary is
/// staged in full before the swap. Same append rationale as [`backup_path`].
fn staged_path(exe: &Path) -> PathBuf {
    let mut name = exe.as_os_str().to_owned();
    name.push(".new");
    PathBuf::from(name)
}

/// Install the new binary. The new bytes are **fully materialized first** at a
/// sibling `<exe>.new` path (via `atomic_write` — complete-or-absent, never
/// partial); only once that staged file exists on disk is the live binary
/// touched, so there is never a window where the target path is missing for
/// the length of a multi-MB download. `promote` then does the swap: on Unix a
/// single atomic rename replaces the running executable (the process keeps the
/// old inode alive); on Windows the live file is moved to `<exe>.old` first
/// (it can't be overwritten while running), leaving only a two-rename gap.
fn install(exe: &Path, bytes: &[u8]) -> Result<(), String> {
    let staged = staged_path(exe);

    crate::util::atomic_write(&staged, bytes)
        .map_err(|e| format!("can't stage the new binary: {e}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)) {
            let _ = std::fs::remove_file(&staged);
            return Err(format!("can't mark the new binary executable: {e}"));
        }
    }

    if let Err(e) = promote(exe, &staged) {
        // Leave the install untouched and don't litter a half-swapped `.new`.
        let _ = std::fs::remove_file(&staged);
        return Err(e);
    }
    Ok(())
}

/// Move the fully-staged file onto the live executable. Split by platform:
/// the two OSes permit different operations against a *running* binary.
#[cfg(unix)]
fn promote(exe: &Path, staged: &Path) -> Result<(), String> {
    // One atomic rename swings the directory entry to the new inode; the
    // running process keeps executing the old (now-unlinked) one. No backup
    // and no moment where `exe` is absent.
    std::fs::rename(staged, exe).map_err(|e| format!("can't install the new binary: {e}"))
}

#[cfg(windows)]
fn promote(exe: &Path, staged: &Path) -> Result<(), String> {
    // Windows won't overwrite or delete a running exe but will rename it
    // aside. Move the live binary to `.old`, then the staged file into its
    // place; if that second rename fails, roll the old one back so the
    // install path is never left empty. The gap is two metadata ops, not a
    // download. `.old` can't be removed while this process runs —
    // cleanup_stale_backup reaps it next launch.
    let backup = backup_path(exe);
    let _ = std::fs::remove_file(&backup);
    std::fs::rename(exe, &backup)
        .map_err(|e| format!("can't move the current binary aside: {e}"))?;
    if let Err(e) = std::fs::rename(staged, exe) {
        let _ = std::fs::rename(&backup, exe);
        return Err(format!("can't install the new binary: {e}"));
    }
    Ok(())
}

async fn spawn_blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| format!("update worker task failed: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_matches_known_vector() {
        // Standard test vector: SHA-256("abc").
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn parse_sha256sums_finds_the_right_entry() {
        let hash_a = "a".repeat(64);
        let hash_b = "B".repeat(64);
        let text = format!(
            "{hash_a}  pooprusteek-linux-x86_64\n{hash_b}  pooprusteek-windows-x86_64.exe\n"
        );
        assert_eq!(
            parse_sha256sums(&text, "pooprusteek-windows-x86_64.exe").as_deref(),
            Some("b".repeat(64).as_str()),
            "entry is found and lowercased"
        );
        assert_eq!(
            parse_sha256sums(&text, "pooprusteek-linux-x86_64").as_deref(),
            Some(hash_a.as_str())
        );
        assert_eq!(parse_sha256sums(&text, "pooprusteek-macos-arm64"), None);
    }

    #[test]
    fn parse_sha256sums_tolerates_binary_marker_and_rejects_junk() {
        let hash = "c".repeat(64);
        let starred = format!("{hash} *pooprusteek-linux-x86_64\n");
        assert_eq!(
            parse_sha256sums(&starred, "pooprusteek-linux-x86_64").as_deref(),
            Some(hash.as_str())
        );
        // Wrong length / non-hex hashes never match.
        assert_eq!(
            parse_sha256sums(
                "deadbeef  pooprusteek-linux-x86_64",
                "pooprusteek-linux-x86_64"
            ),
            None
        );
        let non_hex = format!("{}  pooprusteek-linux-x86_64", "z".repeat(64));
        assert_eq!(parse_sha256sums(&non_hex, "pooprusteek-linux-x86_64"), None);
        assert_eq!(parse_sha256sums("", "pooprusteek-linux-x86_64"), None);
    }

    #[test]
    fn backup_path_appends_old_suffix() {
        assert_eq!(
            backup_path(Path::new("C:/bin/pooprusteek.exe")),
            PathBuf::from("C:/bin/pooprusteek.exe.old"),
            ".exe must be kept, not replaced"
        );
        assert_eq!(
            backup_path(Path::new("/usr/local/bin/pooprusteek")),
            PathBuf::from("/usr/local/bin/pooprusteek.old")
        );
    }

    #[test]
    fn platform_asset_is_known_on_release_targets() {
        // On the three targets CI publishes for, the name must match the
        // publish job's extraction step; elsewhere None (updater declines).
        if let Some(asset) = platform_asset() {
            assert!(asset.starts_with("pooprusteek-"));
        }
    }

    #[test]
    fn install_swaps_bytes_and_survives_reinstall() {
        let dir = std::env::temp_dir().join("pooprusteek_update_test");
        let _ = std::fs::create_dir_all(&dir);
        let exe = dir.join("fake-binary.exe");
        std::fs::write(&exe, b"old-version").unwrap();

        install(&exe, b"new-version").unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new-version");
        // The staged file is consumed by the swap, never left behind.
        assert!(!staged_path(&exe).exists(), "staged .new must not linger");

        // A second update over the first one (possibly with a stale .old or
        // .new still present) must also succeed.
        std::fs::write(backup_path(&exe), b"stale-old").unwrap();
        std::fs::write(staged_path(&exe), b"stale-new").unwrap();
        install(&exe, b"newer-version").unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"newer-version");
        assert!(!staged_path(&exe).exists(), "staged .new must not linger");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
