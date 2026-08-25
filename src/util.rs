/// Truncate `s` to at most `max_bytes` bytes without splitting a UTF-8 char.
/// Returns a slice ending on a char boundary. Use instead of `&s[..n]` for any
/// content that may contain multibyte UTF-8 (PTY/ANSI output, JSON bodies, etc).
pub fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    &s[..s.floor_char_boundary(max_bytes)]
}

/// Decode child-process output that is *usually* UTF-8 but not always.
///
/// Use for every byte stream read off a pipe or PTY instead of a bare
/// `from_utf8_lossy`. On Windows some tools write UTF-16: `wsl.exe` emits its
/// own diagnostics that way (so a WSL failure behind the `bash` tool reached
/// the model as `\x1d\x045\x04 \x00C\x04…`, unreadable to model and human
/// alike), as do parts of .NET and WMI. `POWERSHELL_UTF8_PREFIX` in
/// `tools::shell` solves the sibling problem — PowerShell writing the OEM
/// codepage — but nothing can make a foreign executable stop writing UTF-16.
///
/// Detection is by BOM first, then by where the *implausible* bytes sit. A
/// byte below 0x20 (other than tab/CR/LF) does not occur in real text output,
/// but in UTF-16 it is the high half of every character below U+2000 — `0x00`
/// for ASCII, `0x04` for Cyrillic. When those land consistently on one side of
/// each byte pair the stream is UTF-16, and that side gives the endianness.
/// Counting NULs alone is not enough: `Н` (U+041D) encodes as `1D 04`, so a
/// Russian message contains no NULs at all.
pub fn decode_process_output(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    match utf16_encoding(bytes) {
        Some(encoding) => std::borrow::Cow::Owned(decode_utf16(bytes, encoding)),
        None => {
            // A UTF-8 BOM would otherwise survive into the text as U+FEFF and
            // show up as a stray glyph at the start of the output.
            let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
            String::from_utf8_lossy(body)
        }
    }
}

/// Which UTF-16 flavour `bytes` is, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Utf16 {
    Little,
    Big,
}

/// A byte that text output does not contain, but a UTF-16 high half does.
fn implausible_in_text(byte: u8) -> bool {
    (byte < 0x20 && byte != b'\t' && byte != b'\n' && byte != b'\r') || byte == 0x7F
}

fn utf16_encoding(bytes: &[u8]) -> Option<Utf16> {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return Some(Utf16::Little);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return Some(Utf16::Big);
    }
    // No BOM: infer from byte-pair structure. Only the first chunk is
    // examined — enough to classify, and bounded for very large outputs.
    let sample = &bytes[..bytes.len().min(512)];
    if sample.len() < 4 {
        return None;
    }
    let pairs = sample.len() / 2;
    // UTF-16LE puts the high half second in each pair, UTF-16BE first.
    let (mut second, mut first) = (0usize, 0usize);
    for pair in sample[..pairs * 2].chunks_exact(2) {
        if implausible_in_text(pair[1]) {
            second += 1;
        }
        if implausible_in_text(pair[0]) {
            first += 1;
        }
    }
    // Real UTF-16 text hits the dominant side on nearly every pair; UTF-8
    // hits neither, and binary hits both at random parity. Requiring both a
    // clear majority and a wide margin keeps those two out.
    let decisive = |dominant: usize, other: usize| dominant * 5 > pairs * 2 && other * 4 < dominant;
    if decisive(second, first) {
        Some(Utf16::Little)
    } else if decisive(first, second) {
        Some(Utf16::Big)
    } else {
        None
    }
}

fn decode_utf16(bytes: &[u8], encoding: Utf16) -> String {
    let body = match encoding {
        Utf16::Little => bytes.strip_prefix(&[0xFF, 0xFE]).unwrap_or(bytes),
        Utf16::Big => bytes.strip_prefix(&[0xFE, 0xFF]).unwrap_or(bytes),
    };
    // A trailing odd byte is a truncated code unit (a capped or still-streaming
    // read) — drop it rather than losing the whole decode to it.
    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|pair| match encoding {
            Utf16::Little => u16::from_le_bytes([pair[0], pair[1]]),
            Utf16::Big => u16::from_be_bytes([pair[0], pair[1]]),
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// Truncate to `max_bytes` with an ellipsis suffix, char-boundary safe.
pub fn truncate_with_ellipsis(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let cut = max_bytes.saturating_sub(3);
    let head = truncate_at_char_boundary(s, cut);
    format!("{head}...")
}

/// Write `contents` to `path` atomically: write to a sibling `.tmp` file, then
/// rename over the target. A crash mid-write leaves the old file intact instead
/// of a truncated one. Use for every persisted file (sessions, config,
/// whitelist, mcp.json) — never `std::fs::write` directly on user data.
pub fn atomic_write(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension({
        let mut ext = path
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default();
        ext.push_str(".tmp");
        ext
    });
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&tmp, contents)?;
    // On Windows, `rename` fails if the target exists; remove it first. The
    // window between remove and rename is smaller than a truncate-in-place.
    #[cfg(windows)]
    {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    std::fs::rename(&tmp, path)
}

/// Expand a leading `~` or `~/` to the user's home directory. Plain `~foo`
/// (other-user syntax) is returned unchanged. The single shared impl — do not
/// hand-roll tilde handling at call sites (two past copies were both wrong).
pub fn expand_tilde(path: &str) -> std::path::PathBuf {
    if path == "~"
        && let Some(home) = dirs::home_dir()
    {
        return home;
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\"))
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    std::path::PathBuf::from(path)
}

/// Total physical RAM in bytes, or `None` when it can't be determined.
/// Best-effort and platform-specific; never panics. Used to size
/// memory-bounded work (the embedder batch) to the host it runs on.
pub fn total_ram_bytes() -> Option<u64> {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        // SAFETY: the documented calling convention is a zeroed
        // MEMORYSTATUSEX with `dwLength` set to its size; the call only
        // writes into the struct and returns nonzero on success.
        unsafe {
            let mut status: MEMORYSTATUSEX = std::mem::zeroed();
            status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
            if GlobalMemoryStatusEx(&mut status) != 0 {
                return Some(status.ullTotalPhys);
            }
        }
        None
    }
    #[cfg(target_os = "linux")]
    {
        // /proc/meminfo, first line: `MemTotal:       16333764 kB`
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        let rest = text.lines().find_map(|l| l.strip_prefix("MemTotal:"))?;
        let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
        Some(kb * 1024)
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        let mut mem: u64 = 0;
        let mut size = std::mem::size_of::<u64>();
        let mut mib = [libc::CTL_HW, libc::HW_MEMSIZE];
        // SAFETY: sysctl reads hw.memsize into `mem`; `mib`/`size` describe a
        // valid u64 output buffer and the query has no memory side effects.
        let ok = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                &mut mem as *mut u64 as *mut libc::c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        (ok == 0 && mem > 0).then_some(mem)
    }
    #[cfg(not(any(
        target_os = "windows",
        target_os = "linux",
        target_os = "macos",
        target_os = "ios"
    )))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode as UTF-16LE the way a Windows console tool does.
    fn utf16le(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn utf16be(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_be_bytes).collect()
    }

    #[test]
    fn plain_utf8_passes_through_untouched() {
        assert_eq!(decode_process_output(b"hello, world"), "hello, world");
        let cyrillic = "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}, world";
        assert_eq!(decode_process_output(cyrillic.as_bytes()), cyrillic);
    }

    #[test]
    fn utf8_bom_is_stripped_not_rendered() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"ok");
        assert_eq!(decode_process_output(&bytes), "ok");
    }

    #[test]
    fn bomless_utf16le_is_recovered() {
        // The real shape of the bug: wsl.exe writing a Russian error message.
        let message = "\u{41d}\u{435} \u{443}\u{434}\u{430}\u{43b}\u{43e}\u{441}\u{44c} \
                       \u{43f}\u{43e}\u{434}\u{43a}\u{43b}\u{44e}\u{447}\u{438}\u{442}\u{44c} \
                       \u{434}\u{438}\u{441}\u{43a}: WSL2";
        assert_eq!(decode_process_output(&utf16le(message)), message);
    }

    #[test]
    fn utf16_boms_are_honoured_in_both_endiannesses() {
        let mut le = vec![0xFF, 0xFE];
        le.extend(utf16le("left"));
        assert_eq!(decode_process_output(&le), "left");

        let mut be = vec![0xFE, 0xFF];
        be.extend(utf16be("right"));
        assert_eq!(decode_process_output(&be), "right");
    }

    #[test]
    fn bomless_utf16be_is_recovered() {
        assert_eq!(
            decode_process_output(&utf16be("big endian text")),
            "big endian text"
        );
    }

    #[test]
    fn a_truncated_utf16_code_unit_does_not_lose_the_rest() {
        // A capped or still-streaming read can end mid-unit.
        let mut bytes = utf16le("abcdefgh");
        bytes.pop();
        assert_eq!(decode_process_output(&bytes), "abcdefg");
    }

    #[test]
    fn short_and_empty_inputs_are_not_mistaken_for_utf16() {
        assert_eq!(decode_process_output(b""), "");
        assert_eq!(decode_process_output(b"ok"), "ok");
        // A lone NUL is too little evidence to switch decoders.
        assert_eq!(decode_process_output(b"a\0b").len(), 3);
    }

    #[test]
    fn binary_output_still_decodes_lossily_rather_than_panicking() {
        // Half NULs but no consistent position: not UTF-16, so lossy UTF-8.
        let bytes = [0x00, 0x01, 0x02, 0x00, 0xFF, 0x00, 0x00, 0x9F];
        let text = decode_process_output(&bytes);
        assert!(!text.is_empty());
    }

    #[test]
    fn atomic_write_creates_and_replaces() {
        let dir = std::env::temp_dir().join("pooprusteek_util_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("data.json");
        atomic_write(&path, b"first").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        atomic_write(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert!(!path.with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn total_ram_is_plausible_on_this_host() {
        // Exercises the platform FFI/parse path. Any real host this test
        // runs on has well over 256 MB; a `None` (unsupported target /
        // probe failure) is tolerated so exotic sandboxes don't fail here.
        if let Some(bytes) = total_ram_bytes() {
            assert!(
                bytes > 256 * 1024 * 1024,
                "implausibly small total RAM: {bytes} bytes"
            );
        }
    }

    #[test]
    fn expand_tilde_handles_home_and_passthrough() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/x/y"), home.join("x/y"));
        assert_eq!(
            expand_tilde("/abs/path"),
            std::path::PathBuf::from("/abs/path")
        );
        assert_eq!(
            expand_tilde("rel/path"),
            std::path::PathBuf::from("rel/path")
        );
    }
}

/// Render one tool definition (name, description, parameter list from a
/// JSON schema) as the markdown-ish block used in the system prompt, the
/// `/tools` listing, and semantic tool hints. Required params sort first.
pub fn format_tool_definition(name: &str, description: &str, schema: &serde_json::Value) -> String {
    let mut result = format!("- `{name}`: {description}");

    if let Some(props) = schema.get("properties").and_then(|p| p.as_object())
        && !props.is_empty()
    {
        result.push_str("\n  Parameters:");
        let required = schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<std::collections::HashSet<_>>()
            })
            .unwrap_or_default();
        let mut params: Vec<(&String, &serde_json::Value)> = props.iter().collect();
        params.sort_by(|a, b| {
            let a_req = required.contains(a.0);
            let b_req = required.contains(b.0);
            b_req.cmp(&a_req).then(a.0.cmp(b.0))
        });
        for (param_name, param_info) in params {
            let param_type = param_info
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("any");
            let param_desc = param_info
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            let req_str = if required.contains(param_name) {
                "required"
            } else {
                "optional"
            };
            result.push_str(&format!(
                "\n    \u{2022} `{param_name}` ({param_type}, {req_str}): {param_desc}"
            ));
        }
    }

    result
}

/// Human-readable byte size (B / KB / MB). Lived in `app/mod.rs` until the
/// 2026-07-15 sweep — `tools/shell_control` reaching up into `app` for a
/// pure formatter was one of the three tools→app dependencies.
pub fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// Coarse human-readable duration (s / m / h / d). Same move as
/// [`format_size`].
pub fn format_duration_secs(seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}
