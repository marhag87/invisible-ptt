//! Foreground-window detection and synthetic keystrokes.
//!
//! Windows gets the real implementation. Everything else gets stubs so the
//! HID++ half can be built and smoke-tested on the Linux live image.

pub fn vk_by_name(name: &str) -> u16 {
    match name.to_ascii_uppercase().as_str() {
        "PAUSE" => 0x13,
        "SCROLLLOCK" => 0x91,
        "RCTRL" | "RCONTROL" => 0xA3,
        "LCTRL" | "LCONTROL" => 0xA2,
        "RSHIFT" => 0xA1,
        "RALT" | "RMENU" => 0xA5,
        "F13" => 0x7C,
        "F14" => 0x7D,
        "F15" => 0x7E,
        "F16" => 0x7F,
        "F17" => 0x80,
        "F18" => 0x81,
        "F19" => 0x82,
        "F20" => 0x83,
        "F21" => 0x84,
        "F22" => 0x85,
        "F23" => 0x86,
        "F24" => 0x87,
        _ => 0,
    }
}

#[cfg(windows)]
mod imp {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, MAX_PATH};
    use windows::Win32::System::SystemInformation::GetLocalTime;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
        VIRTUAL_KEY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId, MessageBoxW, MB_ICONERROR, MB_OK,
    };

    pub fn foreground_process() -> Option<String> {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0.is_null() {
                return None;
            }
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == 0 {
                return None;
            }
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;

            let mut buf = [0u16; MAX_PATH as usize];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut len,
            )
            .is_ok();
            let _ = CloseHandle(handle);
            if !ok {
                return None;
            }

            let full = String::from_utf16_lossy(&buf[..len as usize]);
            // Keep only the executable name.
            Some(full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string())
        }
    }

    pub fn key(vk: u16, down: bool) {
        unsafe {
            let input = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(vk),
                        wScan: 0,
                        dwFlags: if down {
                            KEYBD_EVENT_FLAGS(0)
                        } else {
                            KEYEVENTF_KEYUP
                        },
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        }
    }

    pub fn timestamp() -> String {
        let t = unsafe { GetLocalTime() };
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
        )
    }

    /// `%APPDATA%\invisible-ptt` - roaming, per-user, and writable without
    /// elevation, which the exe's own folder is not once it lives in Program
    /// Files. The daemon rewrites its config there on every token refresh.
    pub fn config_dir() -> Option<std::path::PathBuf> {
        std::env::var_os("APPDATA").map(|dir| std::path::PathBuf::from(dir).join("invisible-ptt"))
    }

    pub fn error_box(msg: &str) {
        let text: Vec<u16> = msg.encode_utf16().chain(Some(0)).collect();
        unsafe {
            MessageBoxW(
                None,
                windows::core::PCWSTR(text.as_ptr()),
                windows::core::w!("invisible-ptt"),
                MB_ICONERROR | MB_OK,
            );
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn foreground_process() -> Option<String> {
        None
    }
    pub fn key(vk: u16, down: bool) {
        println!("[stub] key 0x{vk:02x} {}", if down { "down" } else { "up" });
    }

    /// Seconds since the epoch. The log is a Windows feature - on Linux this
    /// only has to be monotonic enough to order the smoke-test output.
    pub fn timestamp() -> String {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("{secs}")
    }

    /// The XDG equivalent, so the Linux smoke-test build behaves like a
    /// citizen rather than dropping files wherever it was started.
    pub fn config_dir() -> Option<std::path::PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
            })?;
        Some(base.join("invisible-ptt"))
    }

    pub fn error_box(msg: &str) {
        eprintln!("{msg}");
    }
}

pub use imp::{config_dir, error_box, foreground_process, key, timestamp};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vk_by_name_known_and_case_insensitive() {
        assert_eq!(vk_by_name("F13"), 0x7C);
        assert_eq!(vk_by_name("f13"), 0x7C);
        assert_eq!(vk_by_name("PAUSE"), 0x13);
        assert_eq!(vk_by_name("rctrl"), 0xA3);
    }

    #[test]
    fn vk_by_name_unknown_is_zero() {
        assert_eq!(vk_by_name("NOPE"), 0);
        assert_eq!(vk_by_name(""), 0);
    }
}
