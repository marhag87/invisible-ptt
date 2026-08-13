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
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
        VIRTUAL_KEY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

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
            Some(
                full.rsplit(['\\', '/'])
                    .next()
                    .unwrap_or(&full)
                    .to_string(),
            )
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
}

#[cfg(not(windows))]
mod imp {
    pub fn foreground_process() -> Option<String> {
        None
    }
    pub fn key(vk: u16, down: bool) {
        println!("[stub] key 0x{vk:02x} {}", if down { "down" } else { "up" });
    }
}

pub use imp::{foreground_process, key};
