//! The tray icon and its right-click menu - the daemon's entire user interface.
//!
//! There is no main window and no console: the icon is the only sign the
//! daemon is alive, and the menu is the only way to talk to it.
//!
//! It runs on its own thread with its own message loop, and it has to.
//! `TrackPopupMenu` does not return until the user picks something or clicks
//! away, and nothing may block the input loop (see `discord::RpcHandle` for the
//! same argument, and the same consequence: a wedged loop means no button
//! events and a mouse that never gets restored). Nothing here touches the
//! mouse; the menu items only flip atomics that the input loop reads, or shell
//! out to the OS.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// What the menu is allowed to drive.
///
/// Reload sends a config rather than a signal: parsing happens here, on the
/// tray thread, so the input loop never does file IO and never has to decide
/// what to do about a typo. What reaches it is already valid.
#[derive(Clone)]
// The Linux stub reads none of these, and a binary crate's dead_code analysis
// does not care that they are pub.
#[cfg_attr(not(windows), allow(dead_code))]
pub struct Controls {
    pub running: Arc<AtomicBool>,
    pub reload: std::sync::mpsc::Sender<crate::Config>,
    pub config: PathBuf,
    pub log: PathBuf,
}

// Tray is only ever held in a local, never named, but re-export it anyway:
// a handle type nobody can spell is a trap for the next caller.
#[allow(unused_imports)]
pub use imp::{spawn, Tray};

#[cfg(windows)]
mod imp {
    use super::Controls;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::sync::OnceLock;
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ,
    };
    use windows::Win32::UI::Shell::{
        FindExecutableW, ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD,
        NIM_DELETE, NOTIFYICONDATAW,
    };
    use windows::Win32::UI::WindowsAndMessaging::*;

    /// Our private tray callback message. The icon reports clicks by sending
    /// this to the hidden window, with the real mouse message in lparam.
    const WM_TRAY: u32 = WM_APP + 1;

    // Menu command ids. 0 is reserved: TrackPopupMenu returns it for "the user
    // clicked away", so no item may use it.
    const ID_SETTINGS: usize = 1;
    const ID_LOG: usize = 2;
    const ID_RELOAD: usize = 3;
    const ID_AUTOSTART: usize = 4;
    const ID_EXIT: usize = 5;

    const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
    const RUN_VALUE: PCWSTR = w!("invisible-ptt");

    /// Lives for as long as the window does, reachable from the window
    /// procedure through GWLP_USERDATA.
    struct State {
        controls: Controls,
        icon: HICON,
    }

    pub struct Tray {
        /// The hidden window, as a plain integer because HWND is not Send.
        /// 0 when the tray failed to start, in which case the daemon simply
        /// runs without a menu rather than not running at all.
        hwnd: isize,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    pub fn spawn(controls: Controls) -> Tray {
        let (ready, started) = mpsc::channel::<isize>();
        let thread = std::thread::spawn(move || unsafe { run(controls, ready) });
        // Wait for the window to exist: shutdown() needs its handle, and a
        // Ctrl-C arriving in the first millisecond would otherwise leave the
        // icon behind.
        let hwnd = started.recv().unwrap_or(0);
        Tray {
            hwnd,
            thread: Some(thread),
        }
    }

    impl Tray {
        /// Take the icon down and wait for the thread. Called on the way out,
        /// so the tray never outlives the daemon as a ghost icon that only
        /// disappears when the user waves the mouse over it.
        pub fn shutdown(mut self) {
            if self.hwnd != 0 {
                unsafe {
                    let _ = PostMessageW(
                        HWND(self.hwnd as *mut std::ffi::c_void),
                        WM_CLOSE,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    unsafe fn run(controls: Controls, ready: mpsc::Sender<isize>) {
        // ShellExecuteW can delegate to shell extensions, which are COM
        // objects; on a thread with no apartment those quietly fail rather
        // than open anything.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let module = GetModuleHandleW(None).unwrap_or_default();
        let instance = HINSTANCE(module.0);
        let class = w!("invisible-ptt-tray");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let state = Box::into_raw(Box::new(State {
            controls,
            icon: make_icon(instance),
        }));
        // A message-only window (HWND_MESSAGE) would be tidier, but the tray
        // needs a window that can be brought to the foreground for the menu.
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("invisible-ptt"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            instance,
            Some(state as *const std::ffi::c_void),
        );
        let hwnd = match hwnd {
            Ok(hwnd) => hwnd,
            Err(e) => {
                logerr!("tray: could not create its window: {e}");
                let _ = ready.send(0);
                drop(Box::from_raw(state));
                return;
            }
        };

        let nid = icon_data(hwnd, (*state).icon);
        if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
            logerr!("tray: the shell refused the icon; running without it");
        }
        let _ = ready.send(hwnd.0 as isize);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        let state = Box::from_raw(state);
        let _ = DestroyIcon(state.icon);
        CoUninitialize();
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // Explorer restarted and every tray icon went with it. Put ours back,
        // or the daemon keeps running with no way to reach it.
        let restarted = taskbar_created();
        if restarted != 0 && msg == restarted {
            if let Some(state) = state(hwnd) {
                let _ = Shell_NotifyIconW(NIM_ADD, &icon_data(hwnd, state.icon));
            }
            return LRESULT(0);
        }

        match msg {
            WM_CREATE => {
                let cs = lparam.0 as *const CREATESTRUCTW;
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
                LRESULT(0)
            }
            WM_TRAY => {
                // Left click gets the same menu as right: there is no window
                // for it to open, and an icon that ignores clicks looks broken.
                let click = lparam.0 as u32;
                if click == WM_RBUTTONUP || click == WM_LBUTTONUP {
                    if let Some(state) = state(hwnd) {
                        show_menu(hwnd, state);
                    }
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    unsafe fn state<'a>(hwnd: HWND) -> Option<&'a State> {
        (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const State).as_ref()
    }

    unsafe fn show_menu(hwnd: HWND, state: &State) {
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        let autostart = autostart_enabled();
        let checked = if autostart { MF_CHECKED } else { MF_UNCHECKED };
        let _ = AppendMenuW(menu, MF_STRING | MF_DISABLED, 0, w!("invisible-ptt"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, ID_SETTINGS, w!("Open settings file"));
        let _ = AppendMenuW(menu, MF_STRING, ID_LOG, w!("Open log file"));
        let _ = AppendMenuW(menu, MF_STRING, ID_RELOAD, w!("Reload settings"));
        let _ = AppendMenuW(
            menu,
            MF_STRING | checked,
            ID_AUTOSTART,
            w!("Start automatically at sign-in"),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, w!("Exit"));

        let mut at = POINT::default();
        let _ = GetCursorPos(&mut at);
        // Mandatory: without it the menu stays up after the user clicks
        // elsewhere, because a tray menu's owner is never the active window.
        let _ = SetForegroundWindow(hwnd);
        let picked = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
            at.x,
            at.y,
            0,
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);

        match picked.0 as usize {
            ID_SETTINGS => open(&state.controls.config),
            ID_LOG => open(&state.controls.log),
            ID_RELOAD => reload(&state.controls),
            ID_AUTOSTART => set_autostart(!autostart, &state.controls.config),
            ID_EXIT => {
                log!("exit requested from the tray");
                state.controls.running.store(false, Ordering::SeqCst);
            }
            _ => {}
        }
        // The companion to SetForegroundWindow above: without a message to
        // process afterwards the window can miss the next click on the icon.
        let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    }

    fn icon_data(hwnd: HWND, icon: HICON) -> NOTIFYICONDATAW {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: icon,
            ..Default::default()
        };
        for (slot, ch) in nid.szTip.iter_mut().zip("invisible-ptt".encode_utf16()) {
            *slot = ch;
        }
        nid
    }

    /// Broadcast by the shell when the taskbar is recreated.
    fn taskbar_created() -> u32 {
        static MSG_ID: OnceLock<u32> = OnceLock::new();
        *MSG_ID.get_or_init(|| unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) })
    }

    /// Re-read the config file and hand it to the input loop.
    ///
    /// The reading, parsing, and validating all happen here for two reasons:
    /// the input loop must not do file IO, and a rejected config deserves to be
    /// said out loud - which means a message box, which would freeze that loop
    /// until someone dismissed it. So the loop only ever receives a config that
    /// is already known good, and a typo costs nothing but a dialog.
    fn reload(controls: &Controls) {
        match crate::load_config(&controls.config) {
            Ok(cfg) => {
                log!("reload requested from the tray");
                if controls.reload.send(cfg).is_err() {
                    logerr!("reload: nothing left to reload into");
                }
            }
            Err(e) => {
                logerr!("reload failed: {e}");
                crate::platform::error_box(&format!(
                    "{e}\n\nNothing was changed - the settings already running are still in effect."
                ));
            }
        }
    }

    /// Open a file for editing, in front of everything else.
    ///
    /// Two things this has to get right, both of which looked like "the menu
    /// item does nothing" when they were wrong:
    ///
    /// - **Foreground rights.** The editor is a *different* process, and
    ///   Windows lets it raise its window only if we hand over the foreground
    ///   first. Without `AllowSetForegroundWindow` it opens behind whatever the
    ///   user was looking at, with nothing but a blinking taskbar button.
    /// - **Association.** Neither `.toml` nor `.log` normally has one. Asking
    ///   the shell to "open" an unassociated file gets the *How do you want to
    ///   open this file?* chooser, so check first and go straight to Notepad
    ///   when there is no handler.
    fn open(path: &Path) {
        let file = wide(path);
        let quoted = wide_quoted(path);
        unsafe {
            // ASFW_ANY: we do not know the editor's pid, and cannot, since the
            // shell may hand the file to an already-running instance.
            let _ = AllowSetForegroundWindow(ASFW_ANY);
            let result = if has_handler(&file) {
                ShellExecuteW(
                    None,
                    w!("open"),
                    PCWSTR(file.as_ptr()),
                    None,
                    None,
                    SW_SHOWNORMAL,
                )
            } else {
                ShellExecuteW(
                    None,
                    w!("open"),
                    w!("notepad.exe"),
                    PCWSTR(quoted.as_ptr()),
                    None,
                    SW_SHOWNORMAL,
                )
            };
            // ShellExecuteW reports failure as a "handle" of 32 or less. Say so
            // rather than leaving another silent menu item.
            if result.0 as usize <= 32 {
                logerr!(
                    "could not open {} (ShellExecute returned {})",
                    path.display(),
                    result.0 as usize
                );
            }
        }
    }

    /// Whether Windows knows an executable for this file. `FindExecutableW`
    /// answers exactly that, and unlike the association APIs it needs no COM.
    unsafe fn has_handler(file: &[u16]) -> bool {
        let mut found = [0u16; 260];
        FindExecutableW(PCWSTR(file.as_ptr()), None, &mut found).0 as usize > 32
    }

    /// True when we have an entry under HKCU ...\Run.
    ///
    /// Read fresh every time the menu opens rather than cached, so the tick
    /// tells the truth even if the entry was changed behind our back.
    fn autostart_enabled() -> bool {
        unsafe {
            let mut key = HKEY::default();
            if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, 0, KEY_READ, &mut key).is_err() {
                return false;
            }
            let exists = RegQueryValueExW(key, RUN_VALUE, None, None, None, None).is_ok();
            let _ = RegCloseKey(key);
            exists
        }
    }

    /// Add or remove the sign-in entry.
    ///
    /// HKCU ...\Run rather than a scheduled task because it is what this needs:
    /// the daemon must run in the interactive session as the user, which is
    /// exactly what the Run key does, and a one-click toggle cannot ask for the
    /// elevation a task registration might.
    fn set_autostart(on: bool, config: &Path) {
        let outcome = unsafe {
            let mut key = HKEY::default();
            if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, 0, KEY_WRITE, &mut key).is_err() {
                logerr!("could not open the Run key; start at sign-in unchanged");
                return;
            }
            let result = if on {
                let command = match std::env::current_exe() {
                    Ok(exe) => format!("\"{}\" \"{}\"", exe.display(), config.display()),
                    Err(e) => {
                        logerr!("could not find our own path: {e}");
                        let _ = RegCloseKey(key);
                        return;
                    }
                };
                // The value is a wide string including its terminator, and the
                // registry wants its length in bytes.
                let value: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
                let bytes = std::slice::from_raw_parts(
                    value.as_ptr() as *const u8,
                    std::mem::size_of_val(&value[..]),
                );
                RegSetValueExW(key, RUN_VALUE, 0, REG_SZ, Some(bytes))
            } else {
                RegDeleteValueW(key, RUN_VALUE)
            };
            let _ = RegCloseKey(key);
            result.ok()
        };
        match outcome {
            Ok(()) if on => log!("will start at sign-in"),
            Ok(()) => log!("will no longer start at sign-in"),
            Err(e) => logerr!("could not change start at sign-in: {e}"),
        }
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    /// The same, quoted, for use as a command-line argument.
    fn wide_quoted(path: &Path) -> Vec<u16> {
        let quote = std::iter::once(u16::from(b'"'));
        quote
            .clone()
            .chain(path.as_os_str().encode_wide())
            .chain(quote)
            .chain(Some(0))
            .collect()
    }

    /// A 32x32 icon drawn in code: a dot inside a ring.
    ///
    /// No image file in the repo, and no orientation to get wrong - the shape
    /// is symmetric, which matters because the bitmap row order CreateIcon
    /// expects is not something to have to be sure about. Mid-blue so it is
    /// legible on both a light and a dark taskbar.
    unsafe fn make_icon(instance: HINSTANCE) -> HICON {
        const N: usize = 32;
        // The AND mask is 1 bit per pixel and selects transparency: 1 leaves
        // the background, 0 draws the colour from the XOR bitmap.
        let mut mask = [0xffu8; N * N / 8];
        let mut colour = [0u8; N * N * 4];
        for y in 0..N {
            for x in 0..N {
                let dx = x as f32 - 15.5;
                let dy = y as f32 - 15.5;
                let r = (dx * dx + dy * dy).sqrt();
                if !(r <= 7.0 || (11.0..=14.5).contains(&r)) {
                    continue;
                }
                let px = (y * N + x) * 4;
                colour[px] = 0xf0; // B
                colour[px + 1] = 0x9b; // G
                colour[px + 2] = 0x2e; // R
                colour[px + 3] = 0xff; // A
                let bit = y * N + x;
                mask[bit / 8] &= !(0x80 >> (bit % 8));
            }
        }
        CreateIcon(
            instance,
            N as i32,
            N as i32,
            1,
            32,
            mask.as_ptr(),
            colour.as_ptr(),
        )
        .unwrap_or_else(|_| LoadIconW(None, IDI_APPLICATION).unwrap_or_default())
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Controls;

    /// There is no tray on the Linux side; the daemon runs headless there, as
    /// it always has. See platform.rs for the same arrangement.
    pub struct Tray;

    pub fn spawn(_controls: Controls) -> Tray {
        Tray
    }

    impl Tray {
        pub fn shutdown(self) {}
    }
}
