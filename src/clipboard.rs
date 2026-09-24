//! Запись в системный буфер обмена штатной утилитой ОС, без крейта.
//! Блокирует до выхода утилиты, поэтому зовётся только из `spawn_blocking`.

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

#[cfg(test)]
mod tests {
    #[test]
    fn clip_exe_gets_utf16_with_a_bom_and_the_rest_utf8() {
        assert_eq!(super::encode("Я", true), [0xFF, 0xFE, 0x2F, 0x04]);
        assert_eq!(super::encode("Я", false), "Я".as_bytes());
    }
}
