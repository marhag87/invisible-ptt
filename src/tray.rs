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

// Status lives in `icon`, next to the shapes it names, because build.rs needs
// it too and cannot reach into this crate. `tray::Status` stays the spelling
// everywhere else.
pub use crate::icon::Status;

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
    use super::{Controls, Status};
    use std::cell::Cell;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::atomic::{AtomicU8, Ordering};
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
        NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    };
    use windows::Win32::UI::WindowsAndMessaging::*;

    /// Our private tray callback message. The icon reports clicks by sending
    /// this to the hidden window, with the real mouse message in lparam.
    const WM_TRAY: u32 = WM_APP + 1;

    /// Take the window down. Posted by `Tray::shutdown` once the daemon has
    /// finished with the mouse.
    ///
    /// This used to be `WM_CLOSE`, which now means the opposite - see the
    /// window procedure. Keeping them separate is what lets the icon stay up
    /// until the restore is actually done.
    const WM_TRAY_QUIT: u32 = WM_APP + 2;

    /// Change the icon. Posted by `Tray::set_status` from the input loop, in
    /// wparam, because `Shell_NotifyIconW` belongs to the thread that owns the
    /// window and the input loop must not wait on anything the tray is doing.
    const WM_TRAY_STATUS: u32 = WM_APP + 3;

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
        /// One per Status, drawn up front: swapping the icon has to be cheap,
        /// and it happens on every press and release.
        icons: [HICON; 3],
        /// Which one is showing. A Cell rather than an atomic because only the
        /// tray thread ever touches it - the input loop posts a message.
        status: Cell<Status>,
    }

    impl State {
        fn icon(&self) -> HICON {
            self.icons[self.status.get() as usize]
        }
    }

    pub struct Tray {
        /// The hidden window, as a plain integer because HWND is not Send.
        /// 0 when the tray failed to start, in which case the daemon simply
        /// runs without a menu rather than not running at all.
        hwnd: isize,
        /// What we last asked for, so `set_status` can be called from the loop
        /// unconditionally and still post nothing in the usual case.
        shown: AtomicU8,
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
            shown: AtomicU8::new(Status::Waiting as u8),
            thread: Some(thread),
        }
    }

    impl Tray {
        /// Show a different icon, if it is not the one already up.
        ///
        /// Called from the input loop on every pass, so the common case must
        /// cost nothing: one atomic swap, and a posted message only when the
        /// status has actually changed. Posting rather than calling because
        /// the shell wants `Shell_NotifyIconW` from the window's own thread,
        /// and because a post never blocks - the tray could be sitting inside
        /// `TrackPopupMenu` with a menu open, and a button press must not wait
        /// for the user to close it.
        pub fn set_status(&self, status: Status) {
            if self.hwnd == 0 || self.shown.swap(status as u8, Ordering::Relaxed) == status as u8 {
                return;
            }
            unsafe {
                let _ = PostMessageW(
                    HWND(self.hwnd as *mut std::ffi::c_void),
                    WM_TRAY_STATUS,
                    WPARAM(status as usize),
                    LPARAM(0),
                );
            }
        }

        /// Take the icon down and wait for the thread. Called on the way out,
        /// so the tray never outlives the daemon as a ghost icon that only
        /// disappears when the user waves the mouse over it.
        pub fn shutdown(mut self) {
            if self.hwnd != 0 {
                unsafe {
                    let _ = PostMessageW(
                        HWND(self.hwnd as *mut std::ffi::c_void),
                        WM_TRAY_QUIT,
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

        // Waiting to begin with: the daemon spawns the tray before it goes
        // looking for the mouse, so that is genuinely where it starts.
        let state = Box::into_raw(Box::new(State {
            controls,
            icons: [
                make_icon(instance, Status::Waiting),
                make_icon(instance, Status::Ready),
                make_icon(instance, Status::Talking),
            ],
            status: Cell::new(Status::Waiting),
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

        let nid = icon_data(hwnd, &*state);
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
        for icon in state.icons {
            let _ = DestroyIcon(icon);
        }
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
                let _ = Shell_NotifyIconW(NIM_ADD, &icon_data(hwnd, state));
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
            // Somebody outside the process wants us gone: an uninstaller
            // going through the Restart Manager, or the shell at sign-out.
            // Both send WM_CLOSE and then wait for the process to exit.
            //
            // The default handling would destroy this window, which ends the
            // tray thread and nothing else - leaving the input loop running
            // with no icon and, far worse, a mouse button still withheld from
            // Windows with no way left to ask for it back. So WM_CLOSE means
            // "stop the daemon" instead: clear `running` and return without
            // destroying anything. The input loop notices within its 100ms
            // timeout, restores the mouse, and calls shutdown(), which posts
            // WM_TRAY_QUIT to finish the job. The icon therefore stays up for
            // exactly as long as the daemon is still holding the mouse.
            WM_CLOSE => {
                stop(hwnd);
                LRESULT(0)
            }
            // The other way the same request arrives. The Restart Manager
            // prefers this for windowed applications - lparam carries
            // ENDSESSION_CLOSEAPP when it is the caller rather than a real
            // sign-out - and answering TRUE means "yes, close me", which
            // DefWindowProcW would say anyway. Stopping on both costs nothing:
            // whichever comes first clears `running`, and the second finds it
            // already cleared.
            WM_QUERYENDSESSION => {
                stop(hwnd);
                LRESULT(1)
            }
            // The input loop's status changed. wparam is the new Status; a
            // value from anywhere else is not one, so check before indexing.
            WM_TRAY_STATUS => {
                if let (Some(state), Some(status)) = (state(hwnd), status_from(wparam)) {
                    state.status.set(status);
                    let _ = Shell_NotifyIconW(NIM_MODIFY, &icon_data(hwnd, state));
                }
                LRESULT(0)
            }
            WM_TRAY_QUIT => {
                let _ = DestroyWindow(hwnd);
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

    /// Ask the daemon to stop, the same way the Exit menu item does.
    ///
    /// Only ever sets the flag - the restore itself belongs to the input loop,
    /// which owns the mouse. Idempotent, because the two messages that call it
    /// can both arrive for one shutdown.
    unsafe fn stop(hwnd: HWND) {
        if let Some(state) = state(hwnd) {
            if state.controls.running.swap(false, Ordering::SeqCst) {
                log!("Windows asked us to close; restoring mouse...");
            }
        }
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

    /// The icon and tooltip for the state we are currently in. Used for the
    /// initial NIM_ADD, for every status change, and to put the icon back when
    /// Explorer restarts - which is why it reads the status rather than taking
    /// one: those three must never disagree about what is showing.
    fn icon_data(hwnd: HWND, state: &State) -> NOTIFYICONDATAW {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: state.icon(),
            ..Default::default()
        };
        // The tooltip carries the same three states in words, for the icon
        // that is 16 pixels across by the time the shell has scaled it.
        let tip = match state.status.get() {
            Status::Waiting => "invisible-ptt - waiting for the mouse",
            Status::Ready => "invisible-ptt - ready",
            Status::Talking => "invisible-ptt - talking",
        };
        for (slot, ch) in nid.szTip.iter_mut().zip(tip.encode_utf16()) {
            *slot = ch;
        }
        nid
    }

    /// wparam back into a Status, rejecting anything else.
    fn status_from(wparam: WPARAM) -> Option<Status> {
        match wparam.0 {
            0 => Some(Status::Waiting),
            1 => Some(Status::Ready),
            2 => Some(Status::Talking),
            _ => None,
        }
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

    /// A 32x32 icon per status, from the shapes in `crate::icon` - the same
    /// ones build.rs bakes into the exe, so the notification area and the
    /// Start menu cannot end up showing two different programs.
    ///
    /// The coverage `icon::bgra` returns is thresholded rather than blended:
    /// `CreateIcon` takes a device-*dependent* bitmap, and whether it reads
    /// the alpha channel at all is not worth depending on, whereas the AND
    /// mask has meant transparency since Windows 3. 1 leaves the background,
    /// 0 draws the pixel - so this is the one place the sense is inverted.
    /// The result is what a point-sampled rasteriser would have drawn, which
    /// is what this was before the shapes had to serve two callers.
    unsafe fn make_icon(instance: HINSTANCE, status: Status) -> HICON {
        const N: u32 = 32;
        let mut colour = crate::icon::bgra(N, status);
        let mut mask = [0xffu8; (N * N / 8) as usize];
        for px in 0..(N * N) as usize {
            let alpha = &mut colour[px * 4 + 3];
            if *alpha >= 128 {
                *alpha = 0xff;
                mask[px / 8] &= !(0x80 >> (px % 8));
            } else {
                colour[px * 4..px * 4 + 4].fill(0);
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
    use super::{Controls, Status};

    /// There is no tray on the Linux side; the daemon runs headless there, as
    /// it always has. See platform.rs for the same arrangement.
    pub struct Tray;

    pub fn spawn(_controls: Controls) -> Tray {
        Tray
    }

    impl Tray {
        /// Nowhere to show it. The status is on stdout there anyway.
        pub fn set_status(&self, _status: Status) {}
        pub fn shutdown(self) {}
    }
}
