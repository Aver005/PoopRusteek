# REFERENCE: Self-update (`/update`, `/autoupdate`, `latest` channel)

> The self-updater and its CI release channel. Source of truth:
> `src/update.rs`, `.github/workflows/ci.yml` (`publish` job),
> `commands/defs/update.rs`, `app/keys/dispatch.rs::apply_update_action`.
> Added 2026-07-07 — see `JOURNAL/2026-07-07.md` pt.3.

## WHAT IT DOES

Compares the SHA-256 of the **running executable's file** against the
`SHA256SUMS` asset of the GitHub Release tagged `latest`. On mismatch it
downloads the raw platform binary, re-verifies its hash, stages it in full
next to the target, and swaps it in. The new binary takes effect on the
**next launch** (a running process can't be replaced live — see SWAP below).

- **`/update`** — manual, one-shot. In a **debug build** (`cargo run`) it
  first opens a confirm modal (`ConfirmAction::Update` → `ConfirmState::
  update_dev`), because a dev binary always mismatches the released hash and
  would otherwise be silently swapped for the release build. Release builds
  update straight away.
- **`/autoupdate [on|off]`** — persists `[update] auto` (default **off**).
  When on, every TUI startup (`App::new`) runs the same check in the
  background, quiet when already current (status line only).

Entry flow: command → `CommandResult::Update(UpdateAction)` →
`apply_update_action` (dispatch.rs) → `app::spawn_update_task(event_tx,
update_in_flight, quiet)` → `update::run()` off the event loop →
`AppEvent::UpdateStatus { message, notable }`.

## SWAP MECHANICS (`update::install` / `promote`)

The running binary is **renamed, never overwritten** — the OS locks a live
exe against write/delete but allows renaming it aside.

1. Download → verify hash → `atomic_write` the bytes to a sibling
   `<exe>.new` (**staged in full first** — the live binary isn't touched
   until the new one exists complete on disk; no window the length of a
   multi-MB download).
2. `promote`:
   - **Unix**: one atomic `rename(<exe>.new, <exe>)` — replaces the dir
     entry; the running process keeps executing the old (now-unlinked)
     inode. No backup, no absent-file window.
   - **Windows**: `rename(<exe>, <exe>.old)` then `rename(<exe>.new, <exe>)`
     — the gap is two metadata ops, not a download. `<exe>.old` can't be
     deleted while the process runs; `cleanup_stale_backup()` (called every
     `App::new`) reaps both `.old` and a stranded `.new` next launch.

## CONTRACT POINTS — DO NOT DESYNC

Load-bearing couplings. Change one side of any row, change the other in the
same commit:

| Contract | Reader (app) | Writer / counterpart (CI or OS) |
|----------|--------------|---------------------------------|
| Raw asset names `pooprusteek-{windows-x86_64.exe, linux-x86_64, macos-arm64}` | `update::platform_asset()` | `publish` job step "Extract raw binaries + SHA256SUMS" (the `mv unpacked/… <name>` lines) |
| Checksum asset name + format (`sha256sum` lines, hashes over **uncompressed** binaries) | `CHECKSUMS_ASSET` + `parse_sha256sums` | `sha256sum … > SHA256SUMS` step in CI |
| Release tag `latest` in the download URL | `RELEASE_DOWNLOAD_BASE` | CI steps `git tag -f latest` / `gh release delete latest` / the second `action-gh-release` (`tag_name: latest`) |
| Repo owner/name in the download URL (`Aver005/pooprusteek`) | `RELEASE_DOWNLOAD_BASE` | the actual GitHub repo path |
| Archive names + inner filename (`pooprusteek-<target>.{zip,tar.gz}`, inside `pooprusteek[.exe]`) | CI extraction step (`unzip`/`tar` + `mv`) | `release-build` job packaging steps |
| Binary name `pooprusteek` | inner filenames above | `[[bin]]` / package name in `Cargo.toml` |
| Step ordering in `publish` | extraction **before** "Create release" (else assets aren't attached); all `latest` steps **after** the versioned release (else a failed dev publish still moves `latest`) | — |
| `permissions: contents: write` on `publish` | — | needed for the `latest` tag push + release create/delete |
| `cleanup_stale_backup()` at startup + `.old`/`.new` suffixes | `App::new` calls it; `backup_path`/`staged_path` define suffixes | Windows leaves `.old`; a crash can leave `.new` |
| Update work runs only via `spawn_update_task` (never on the event loop) | invariant #1 (no network/file I/O in `handle_event`) | — |
| Dev gate = `cfg!(debug_assertions)` | `apply_update_action` `UpdateAction::Run` | release binaries are built `--release` (debug_assertions off) → no prompt |

## FAILURE / FRAGILITY MODES

- **Dropping a `release-build` matrix leg** (e.g. removing macOS) breaks the
  extraction step, which fails the **whole `publish` job** — including the
  ordinary versioned dev release. The extraction now makes all three
  platform archives mandatory for any release.
- **Asset-name drift** between `platform_asset()` and CI → "SHA256SUMS has
  no entry for …" forever.
- **Repo rename** → the hardcoded URL 404s once the old name is free
  (GitHub redirects only while the old owner/name is unclaimed).
- **Manual tinkering with the `latest` tag/release** (deleting it, creating
  the tag without assets) → 404s.
- **The delete→recreate window** for the `latest` release: a client checking
  in those seconds gets a clean 404, not a crash.
- **`SHA256SUMS` guards integrity, not authenticity** — it sits beside the
  binary. Trust rests entirely on TLS + the GitHub account. No signing yet
  (minisign / code-signing is the fix if that ever matters).
- **Downgrade / no version ordering**: the check is identity ("am I exactly
  the latest"), not "am I older". A bad build published to `latest` updates
  everyone with `autoupdate on`. Hash carries no release time — an ordering
  key (build timestamp / CI run number baked into the binary + published in
  the manifest) would be needed to gate downgrades; not implemented.
- **Read-only install dir** (Program Files, root-owned `/usr/local/bin`) →
  the swap fails cleanly; the feature needs write access to the exe's dir.
- **Two running instances** race on the swap (`update_in_flight` is a
  per-process flag, not a lock); launching a new instance exactly during the
  Windows two-rename gap can hit "file not found".
- **Repeated `/update` in one Linux session**: after a swap `/proc/self/exe`
  points at the unlinked `.old`, so a second check without restart errors on
  read. Windows reports "up to date" (the on-disk file is new even though the
  process runs the old one).

## TESTS

`update::tests` — SHA256SUMS parsing (+ binary marker / junk rejection),
sha256 vector, `backup_path`/`staged_path` suffixing, `install` swap +
re-install round-trip (asserts no stale `.new` lingers), platform-asset
contract. Config compat/round-trip: `config_without_update_section_still_loads`.
Command parse: `autoupdate_subcommands_map_to_the_right_actions`.
