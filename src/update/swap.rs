//! Замена бинарника на диске. Новый файл сначала целиком пишется в
//! `<exe>.new`, затем подменяет живой: rename на Unix, rename-aside на Windows.

use std::path::{Path, PathBuf};

/// Delete the leftovers a previous update may have left next to the binary:
/// the `<exe>.old` backup (Windows can't remove it while that process runs,
/// so the swap defers it to the next launch) and any `<exe>.new` staged file
/// stranded by a crash between staging and the swap. Call once at startup;
/// silent best-effort (nothing exists on most launches).
pub fn cleanup_stale_backup() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = std::fs::remove_file(staged_path(&exe));
    // `.old` и запасные `.old.<pid>`; занятые ещё работающим процессом просто останутся.
    let (Some(dir), Some(backup_name)) = (
        exe.parent(),
        backup_path(&exe).file_name().map(|n| n.to_owned()),
    ) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let prefix = backup_name.to_string_lossy().into_owned();
    for entry in entries.flatten() {
        if is_backup_name(&entry.file_name().to_string_lossy(), &prefix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// `pooprusteek.exe.old` или `pooprusteek.exe.old.1234`.
fn is_backup_name(name: &str, backup: &str) -> bool {
    name == backup
        || name
            .strip_prefix(backup)
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))
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
pub fn install(exe: &Path, bytes: &[u8]) -> Result<(), String> {
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
    let mut backup = backup_path(exe);
    // `.old` держит ещё работающий старый процесс — уводим в уникальное имя.
    if std::fs::remove_file(&backup).is_err() && backup.exists() {
        let mut name = backup.into_os_string();
        name.push(format!(".{}", std::process::id()));
        backup = PathBuf::from(name);
    }
    std::fs::rename(exe, &backup)
        .map_err(|e| format!("can't move the current binary aside: {e}"))?;
    if let Err(e) = std::fs::rename(staged, exe) {
        let _ = std::fs::rename(&backup, exe);
        return Err(format!("can't install the new binary: {e}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn backup_names_match_only_our_leftovers() {
        let backup = "pooprusteek.exe.old";
        assert!(is_backup_name("pooprusteek.exe.old", backup));
        assert!(is_backup_name("pooprusteek.exe.old.4242", backup));
        assert!(!is_backup_name("pooprusteek.exe", backup));
        assert!(!is_backup_name("pooprusteek.exe.old.", backup));
        assert!(!is_backup_name("pooprusteek.exe.old.bak", backup));
        assert!(!is_backup_name("pooprusteek.exe.older", backup));
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
