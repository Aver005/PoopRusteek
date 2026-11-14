//! Системный буфер обмена без крейта: запись — штатной утилитой ОС, чтение — WinAPI или утилитой.
//! Всё блокирует, поэтому зовётся только из `spawn_blocking`.

use std::io::{self, Write};
use std::process::{Command, Stdio};

/// Утилита буфера и то, в какой кодировке она ждёт текст.
struct Tool {
    program: &'static str,
    args: &'static [&'static str],
    /// `clip.exe` читает UTF-16 только с BOM, иначе кириллица — кракозябры.
    utf16: bool,
}

#[cfg(not(target_os = "macos"))]
const CLIP_EXE: Tool = Tool {
    program: "clip.exe",
    args: &[],
    utf16: true,
};

/// Утилиты по порядку: пишет первая, что нашлась.
#[cfg(windows)]
const TOOLS: &[Tool] = &[CLIP_EXE];
#[cfg(target_os = "macos")]
const TOOLS: &[Tool] = &[Tool {
    program: "pbcopy",
    args: &[],
    utf16: false,
}];
/// `clip.exe` последним: в WSL без WSLg других утилит нет, а interop его запустит.
#[cfg(all(unix, not(target_os = "macos")))]
const TOOLS: &[Tool] = &[
    Tool {
        program: "wl-copy",
        args: &[],
        utf16: false,
    },
    Tool {
        program: "xclip",
        args: &["-selection", "clipboard"],
        utf16: false,
    },
    Tool {
        program: "xsel",
        args: &["--clipboard", "--input"],
        utf16: false,
    },
    CLIP_EXE,
];

pub fn copy(text: &str) -> io::Result<()> {
    let mut last_error = None;
    for tool in TOOLS {
        match pipe_into(tool, &encode(text, tool.utf16)) {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| io::Error::other("no clipboard tool for this OS")))
}

fn encode(text: &str, utf16: bool) -> Vec<u8> {
    if !utf16 {
        return text.as_bytes().to_vec();
    }
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    bytes
}

/// Вывод утилиты глушится: терминал принадлежит TUI.
fn pipe_into(tool: &Tool, payload: &[u8]) -> io::Result<()> {
    let mut child = Command::new(tool.program)
        .args(tool.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // stdin закрывается в конце выражения: утилита ждёт EOF, прежде чем выйти.
    let written = match child.stdin.take() {
        Some(mut stdin) => stdin.write_all(payload),
        None => Err(io::Error::other("clipboard tool has no stdin")),
    };
    // Ждать и при сбое записи — иначе на Unix остаётся зомби.
    let status = child.wait()?;
    written?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{} exited with {status}",
            tool.program
        )))
    }
}

/// Ctrl+V и Shift+Insert. Терминал с bracketed paste сам их перехватывает и шлёт `Event::Paste`;
/// до приложения они доходят там, где терминал не вставляет (консоль Windows в raw-режиме).
pub fn is_paste_shortcut(key: &crossterm::event::KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};
    let mods = key.modifiers;
    match key.code {
        KeyCode::Char('v' | 'V') => {
            mods.contains(KeyModifiers::CONTROL) && !mods.contains(KeyModifiers::ALT)
        }
        KeyCode::Insert => mods == KeyModifiers::SHIFT,
        _ => false,
    }
}

/// Текст из буфера обмена; пустая строка, если текста там нет.
pub fn read() -> io::Result<String> {
    read_platform()
}

/// Консоль Windows в raw-режиме сама не вставляет, поэтому буфер читаем напрямую.
#[cfg(windows)]
fn read_platform() -> io::Result<String> {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;

    struct Opened;
    impl Drop for Opened {
        fn drop(&mut self) {
            // SAFETY: буфер открыт этим потоком в `OpenClipboard` и закрывается один раз.
            unsafe { CloseClipboard() };
        }
    }

    // Буфер мог на миг занять другой процесс: несколько коротких попыток.
    let _opened = (0..5)
        .find_map(|attempt| {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            // SAFETY: без окна-владельца буфер открывается за текущим потоком.
            (unsafe { OpenClipboard(std::ptr::null_mut()) } != 0).then_some(Opened)
        })
        .ok_or_else(|| io::Error::other("the clipboard is busy"))?;

    let format = u32::from(CF_UNICODETEXT);
    // SAFETY: буфер открыт выше и держится до конца функции.
    if unsafe { IsClipboardFormatAvailable(format) } == 0 {
        return Ok(String::new());
    }
    // SAFETY: буфер открыт; дескриптор принадлежит буферу, освобождать его нельзя.
    let handle = unsafe { GetClipboardData(format) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: дескриптор получен из `GetClipboardData` и действителен, пока буфер открыт.
    let data = unsafe { GlobalLock(handle) }.cast::<u16>();
    if data.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: блок заблокирован и занимает `GlobalSize` байт; дальше его конца не читаем.
    let units = unsafe { std::slice::from_raw_parts(data, GlobalSize(handle) / 2) };
    let end = units
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(units.len());
    let text = String::from_utf16_lossy(&units[..end]);
    // SAFETY: блок заблокирован выше этим же дескриптором.
    unsafe { GlobalUnlock(handle) };
    Ok(text)
}

/// Утилиты чтения по порядку: отвечает первая, что нашлась.
#[cfg(target_os = "macos")]
const READ_TOOLS: &[(&str, &[&str])] = &[("pbpaste", &[])];
#[cfg(all(unix, not(target_os = "macos")))]
const READ_TOOLS: &[(&str, &[&str])] = &[
    ("wl-paste", &["--no-newline"]),
    ("xclip", &["-selection", "clipboard", "-o"]),
    ("xsel", &["--clipboard", "--output"]),
];

#[cfg(unix)]
fn read_platform() -> io::Result<String> {
    let mut last_error = None;
    for (program, args) in READ_TOOLS {
        let output = Command::new(program)
            .args(*args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        match output {
            Ok(output) if output.status.success() => {
                return Ok(crate::util::decode_process_output(&output.stdout).into_owned());
            }
            Ok(output) => {
                last_error = Some(io::Error::other(format!(
                    "{program} exited with {}",
                    output.status
                )));
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| io::Error::other("no clipboard tool for this OS")))
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn paste_shortcuts_are_ctrl_v_and_shift_insert_only() {
        let key = |code, mods| super::is_paste_shortcut(&KeyEvent::new(code, mods));
        assert!(key(KeyCode::Char('v'), KeyModifiers::CONTROL));
        assert!(key(
            KeyCode::Char('V'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        ));
        assert!(key(KeyCode::Insert, KeyModifiers::SHIFT));
        assert!(!key(KeyCode::Char('v'), KeyModifiers::NONE));
        // AltGr на Windows — это Ctrl+Alt: набранный так знак не вставка.
        assert!(!key(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        ));
        assert!(!key(KeyCode::Insert, KeyModifiers::NONE));
    }

    #[test]
    fn clip_exe_gets_utf16_with_a_bom_and_the_rest_utf8() {
        assert_eq!(super::encode("Я", true), [0xFF, 0xFE, 0x2F, 0x04]);
        assert_eq!(super::encode("Я", false), "Я".as_bytes());
    }
}
