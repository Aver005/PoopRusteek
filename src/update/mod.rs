//! Самообновление из GitHub Releases по двум каналам.
//!
//! - **stable** — последний обычный релиз (`releases/latest`), публикуется по
//!   тегу `v*`; ставится только если его версия новее запущенной.
//! - **dev** — rolling-релиз `dev` с каждого пуша в develop; ставится при
//!   несовпадении SHA-256 файла.
//!
//! Каждый релиз несёт `manifest.json` (версия, тег, хэши ассетов). Бинарник
//! качается по тегу из манифеста, сверяется с хэшем и подменяется через
//! [`swap::install`]; новая версия работает со следующего запуска. Контракт с
//! CI — `.memories/reference/AUTO-UPDATE.md`.

#[cfg(windows)]
mod install_record;
mod manifest;
mod swap;

use crate::config::UpdateChannel;
use manifest::{MANIFEST_ASSET, Manifest, Version};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

pub use swap::cleanup_stale_backup;

const RELEASES_BASE: &str = "https://github.com/Aver005/pooprusteek/releases";
/// Tag of the rolling dev release.
const DEV_TAG: &str = "dev";

/// Версия запущенного бинарника.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Платформа в именах ассетов (`pooprusteek-<target>[.exe]`), `None` — CI её не собирает.
pub fn platform_target() -> Option<&'static str> {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("windows-x86_64")
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        Some("windows-arm64")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("linux-x86_64")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("linux-arm64")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("macos-arm64")
    } else {
        None
    }
}

/// Имя «сырого» бинарника этой платформы в релизе.
pub fn platform_asset() -> Option<String> {
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    platform_target().map(|target| format!("pooprusteek-{target}{suffix}"))
}

/// Сборка, уже поставленная этим процессом: повторная замена ломает swap
/// (на Windows `.old` занят, на Linux `current_exe` указывает на удалённый inode).
static INSTALLED_THIS_RUN: OnceLock<String> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// Newer build isn't available on the channel.
    UpToDate { channel_build: String },
    /// The binary was replaced on disk; takes effect on the next launch.
    Updated { build: String },
    /// This process already installed `build`; only a restart is left.
    PendingRestart { build: String },
    /// The channel has nothing published yet (e.g. no tagged release so far).
    NoRelease,
}

/// Помечает процесс как запущенный для установщика Windows (`AppMutex`), на других ОС — ничего.
pub fn register_running_instance() {
    #[cfg(windows)]
    install_record::hold_instance_mutex();
}

/// Check `channel` and install its build when it should replace this one.
/// Network + hashing + the file swap all happen here — callers must run this
/// off the event loop (spawned task) and report the result via `AppEvent`.
pub async fn run(channel: UpdateChannel) -> Result<UpdateOutcome, String> {
    if let Some(build) = INSTALLED_THIS_RUN.get() {
        return Ok(UpdateOutcome::PendingRestart {
            build: build.clone(),
        });
    }
    let Some(asset) = platform_asset() else {
        return Err("auto-update isn't supported for this platform/architecture".to_string());
    };
    let exe =
        std::env::current_exe().map_err(|e| format!("can't locate the running executable: {e}"))?;

    let client = http_client()?;
    // 404 на манифесте — канал пуст (до первого тега), а не сбой.
    let Some(manifest_bytes) = fetch(&client, &manifest_url(channel)).await? else {
        return Ok(UpdateOutcome::NoRelease);
    };
    let manifest = Manifest::parse(&manifest_bytes)?;
    let remote_hash = manifest
        .sha256_for(&asset)
        .map_err(|e| format!("the {} release has {e}", channel.as_str()))?;
    let build = build_label(channel, &manifest);

    // Файл на диске уже этот (например, поставлен установщиком поверх) — трогать нечего.
    let local_hash = {
        let exe = exe.clone();
        spawn_blocking(move || {
            std::fs::read(&exe)
                .map(|bytes| sha256_hex(&bytes))
                .map_err(|e| format!("can't read {}: {e}", exe.display()))
        })
        .await?
    };
    let wanted = local_hash != remote_hash
        && match channel {
            UpdateChannel::Stable => is_newer(&manifest.version, CURRENT_VERSION)?,
            UpdateChannel::Dev => true,
        };
    if !wanted {
        return Ok(UpdateOutcome::UpToDate {
            channel_build: build,
        });
    }

    let url = format!("{RELEASES_BASE}/download/{}/{asset}", manifest.tag);
    let bytes = fetch(&client, &url).await?.ok_or_else(|| {
        format!(
            "{asset} is listed in the manifest but missing at {url} (HTTP 404) — try again later"
        )
    })?;
    #[cfg(windows)]
    let version = manifest.version.clone();
    spawn_blocking(move || {
        if sha256_hex(&bytes) != remote_hash {
            return Err(
                "downloaded binary doesn't match its published checksum — aborting (try /update again)"
                    .to_string(),
            );
        }
        swap::install(&exe, &bytes)?;
        #[cfg(windows)]
        install_record::sync_display_version(&exe, &version);
        Ok(())
    })
    .await?;

    let _ = INSTALLED_THIS_RUN.set(build.clone());
    Ok(UpdateOutcome::Updated { build })
}

/// Как показать сборку: у dev одна версия на много сборок, поэтому добавляем коммит.
fn build_label(channel: UpdateChannel, manifest: &Manifest) -> String {
    match channel {
        UpdateChannel::Dev if !manifest.commit.is_empty() => format!(
            "{} ({})",
            manifest.version,
            crate::util::truncate_at_char_boundary(&manifest.commit, 8)
        ),
        _ => manifest.version.clone(),
    }
}

fn manifest_url(channel: UpdateChannel) -> String {
    match channel {
        // `releases/latest` — последний не-prerelease релиз, отдельный тег не нужен.
        UpdateChannel::Stable => format!("{RELEASES_BASE}/latest/download/{MANIFEST_ASSET}"),
        UpdateChannel::Dev => format!("{RELEASES_BASE}/download/{DEV_TAG}/{MANIFEST_ASSET}"),
    }
}

fn is_newer(remote: &str, local: &str) -> Result<bool, String> {
    let parse =
        |text: &str| Version::parse(text).ok_or_else(|| format!("can't parse version {text:?}"));
    Ok(parse(remote)? > parse(local)?)
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

/// `Ok(None)` — HTTP 404; что это значит, решает вызывающий.
async fn fetch(client: &reqwest::Client, url: &str) -> Result<Option<Vec<u8>>, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("request to {url} failed: {e}"))?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(format!("HTTP {status} fetching {url}"));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("download from {url} failed: {e}"))?;
    Ok(Some(bytes.to_vec()))
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
    fn platform_asset_matches_ci_naming() {
        // Имена должны совпадать с `target` в матрице `.github/workflows/build.yml`.
        if let Some(asset) = platform_asset() {
            assert!(asset.starts_with("pooprusteek-"));
            assert_eq!(asset.ends_with(".exe"), cfg!(windows));
        }
    }

    #[test]
    fn stable_only_moves_forward() {
        assert!(is_newer("0.2.0", "0.1.0").unwrap());
        assert!(
            !is_newer("0.1.0", "0.1.0").unwrap(),
            "same version is not an update"
        );
        assert!(!is_newer("0.1.0", "0.2.0").unwrap(), "never downgrade");
        assert!(is_newer("garbage", "0.1.0").is_err());
    }

    #[test]
    fn manifest_urls_point_at_the_right_release() {
        assert_eq!(
            manifest_url(UpdateChannel::Stable),
            "https://github.com/Aver005/pooprusteek/releases/latest/download/manifest.json"
        );
        assert_eq!(
            manifest_url(UpdateChannel::Dev),
            "https://github.com/Aver005/pooprusteek/releases/download/dev/manifest.json"
        );
    }
}
