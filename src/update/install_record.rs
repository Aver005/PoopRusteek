//! Запись установщика в «Приложениях» Windows. После самообновления
//! поправляем `DisplayVersion`, иначе там висит версия из установщика.

use std::path::Path;

/// `AppId` из `packaging/windows/pooprusteek.iss` + суффикс `_is1`, который добавляет Inno Setup.
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{743230F1-F710-4EF9-9F8A-CEE2AAB13D61}_is1";

/// Имя из `AppMutex` в `.iss`: пока мьютекс жив, установщик просит закрыть приложение.
const INSTANCE_MUTEX: &str = "PooprusteekRunning";

/// Держит именованный мьютекс до конца процесса; ошибка не мешает работе.
pub fn hold_instance_mutex() {
    let name: Vec<u16> = INSTANCE_MUTEX
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: имя нуль-терминировано; handle намеренно не закрываем — ОС освободит при выходе.
    unsafe {
        windows_sys::Win32::System::Threading::CreateMutexW(std::ptr::null(), 0, name.as_ptr());
    }
}

/// Молча ничего не делает для portable-копии: ключа нет или он про другую папку.
pub fn sync_display_version(exe: &Path, version: &str) {
    let Some(dir) = exe.parent() else { return };
    let Some(key) = registry::UserKey::open(UNINSTALL_KEY) else {
        return;
    };
    let Some(location) = key.read_string("InstallLocation") else {
        return;
    };
    if same_dir(Path::new(&location), dir) {
        key.write_string("DisplayVersion", version);
    }
}

/// Inno пишет `InstallLocation` с завершающим `\`; регистр в путях Windows не важен.
fn same_dir(a: &Path, b: &Path) -> bool {
    let normalize = |p: &Path| {
        p.to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .replace('/', "\\")
            .to_lowercase()
    };
    normalize(a) == normalize(b)
}

mod registry {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, RegCloseKey,
        RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    };

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Открытый ключ в HKCU; закрывается в `Drop`.
    pub struct UserKey(HKEY);

    impl UserKey {
        pub fn open(subkey: &str) -> Option<Self> {
            let subkey = wide(subkey);
            let mut handle: HKEY = std::ptr::null_mut();
            // SAFETY: строка нуль-терминирована, handle — валидный out-указатель.
            let status = unsafe {
                RegOpenKeyExW(
                    HKEY_CURRENT_USER,
                    subkey.as_ptr(),
                    0,
                    KEY_QUERY_VALUE | KEY_SET_VALUE,
                    &mut handle,
                )
            };
            (status == ERROR_SUCCESS).then_some(Self(handle))
        }

        pub fn read_string(&self, name: &str) -> Option<String> {
            let name = wide(name);
            let mut kind = 0u32;
            let mut buffer = vec![0u16; 1024];
            let mut size = (buffer.len() * 2) as u32;
            // SAFETY: буфер на `size` байт живёт до конца вызова.
            let status = unsafe {
                RegQueryValueExW(
                    self.0,
                    name.as_ptr(),
                    std::ptr::null(),
                    &mut kind,
                    buffer.as_mut_ptr().cast(),
                    &mut size,
                )
            };
            if status != ERROR_SUCCESS || kind != REG_SZ {
                return None;
            }
            buffer.truncate(size as usize / 2);
            let text = String::from_utf16_lossy(&buffer);
            Some(text.trim_end_matches('\0').to_string())
        }

        pub fn write_string(&self, name: &str, value: &str) {
            let name = wide(name);
            let value = wide(value);
            // SAFETY: данные — нуль-терминированная UTF-16 строка длиной `value.len() * 2` байт.
            unsafe {
                RegSetValueExW(
                    self.0,
                    name.as_ptr(),
                    0,
                    REG_SZ,
                    value.as_ptr().cast(),
                    (value.len() * 2) as u32,
                );
            }
        }
    }

    impl Drop for UserKey {
        fn drop(&mut self) {
            // SAFETY: handle получен из успешного RegOpenKeyExW и закрывается один раз.
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_dir_ignores_case_and_trailing_separator() {
        assert!(same_dir(
            Path::new(r"C:\Users\Me\AppData\Local\Programs\Pooprusteek\"),
            Path::new(r"c:\users\me\appdata\local\programs\pooprusteek"),
        ));
        assert!(!same_dir(
            Path::new(r"C:\Programs\Pooprusteek\"),
            Path::new(r"C:\Programs\Other"),
        ));
    }
}
