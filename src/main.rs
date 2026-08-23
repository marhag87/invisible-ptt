//! invisible-ptt
//!
//! Turns a Logitech mouse button into a push-to-talk key that the operating
//! system cannot see.
//!
//! How it works:
//!   1. HID++ 0x8100 SetMode(Host) turns onboard profiles off so the mapping takes effect.
//!   2. HID++ 0x8110 SetMouseButtonMapping with code 0 disables a button for standard HID reports.
//!   3. HID++ 0x8110 StartMouseButtonSpy delivers raw button state as HID++ notifications.
//!
//! The result: no window message, no virtual key, no low-level hook anywhere
//! in Windows observes the button. Only this process knows it was pressed.
//!
//! On Windows it lives in the notification area: no console, no main window,
//! just an icon with a menu (see tray.rs). Everything it would have printed
//! goes to a log file beside the config instead (log.rs).

// A GUI subsystem binary, so nothing pops a console window. Not under `test`:
// the attribute applies to the test harness too, and would silence it.
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

#[macro_use]
mod log;
mod discord;
mod hidpp;
mod icon;
mod platform;
mod tray;

use hidpp::*;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Deserialize)]
struct Config {
    /// Spy index of the button to hijack. 0=left 1=right 2=middle 3=back 4=forward
    button: u8,
    /// One entry per physical button. 0 = invisible to the OS, 1..16 = normal.
    mapping: Vec<u8>,
    /// Used when the foreground process matches no rule.
    #[serde(default = "default_action")]
    default_action: String,
    #[serde(default)]
    rules: Vec<Rule>,
    #[serde(default)]
    discord: DiscordCfg,
}

fn default_action() -> String {
    "none".into()
}

/// A long HID++ frame carries 16 parameter bytes, one per button, and the spy
/// reports button state as a 16-bit mask. Nothing here can address more.
const MAX_BUTTONS: usize = 16;

impl Config {
    /// Reject configurations that would otherwise panic or misfire silently.
    ///
    /// Both of these are reachable by typo: an over-long mapping runs off the
    /// end of the 20-byte frame in HidPp::call, and a button index past 15
    /// overflows the `1 << button` mask - which panics in debug, and in release
    /// quietly wraps round to hijack some entirely different button.
    fn validate(&self) -> std::result::Result<(), String> {
        if self.mapping.is_empty() || self.mapping.len() > MAX_BUTTONS {
            return Err(format!(
                "mapping has {} entries; it needs one per physical button, 1 to {MAX_BUTTONS}",
                self.mapping.len()
            ));
        }
        if usize::from(self.button) >= self.mapping.len() {
            return Err(format!(
                "button = {} but the mapping only covers buttons 0 to {}",
                self.button,
                self.mapping.len() - 1
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct Rule {
    /// Executable name, case-insensitive, e.g. "chrome.exe"
    process: String,
    action: String,
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct DiscordCfg {
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
    #[serde(default)]
    pub access_token: String,
    /// Long-lived token used to mint a fresh access_token before the 7-day
    /// expiry. Rotates on each refresh, so it is written back to config.toml.
    #[serde(default)]
    pub refresh_token: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Action {
    None,
    Key(u16),
    Rpc,
}

fn parse_action(s: &str) -> Action {
    let s = s.trim();
    if s.eq_ignore_ascii_case("rpc") {
        return Action::Rpc;
    }
    if s.eq_ignore_ascii_case("none") || s.is_empty() {
        return Action::None;
    }
    if let Some(rest) = s.strip_prefix("key:") {
        let rest = rest.trim();
        let vk = if let Some(hex) = rest.strip_prefix("0x") {
            u16::from_str_radix(hex, 16).unwrap_or(0)
        } else if rest.len() == 1 {
            // A single character: use its uppercase ASCII value, which is
            // how Windows virtual-key codes work for letters and digits.
            rest.to_ascii_uppercase().as_bytes()[0] as u16
        } else {
            platform::vk_by_name(rest)
        };
        if vk != 0 {
            return Action::Key(vk);
        }
    }
    // Anything else is a typo. Say so: an unrecognised action is indis-
    // tinguishable at runtime from a button that simply does nothing.
    logerr!("warning: could not parse action '{s}', treating as none");
    Action::None
}

struct Device {
    pp: HidPp,
    spy_idx: u8,
    profiles_idx: u8,
    /// Feature index of WirelessDeviceStatus (0x1D4B), if the firmware has it.
    /// Its notification means "reconnected, reconfigure me".
    wireless_idx: Option<u8>,
}

impl Device {
    fn connect(api: &hidapi::HidApi, cfg: &Config) -> hidpp::Result<Self> {
        let mut pp = HidPp::open(api)?;
        let profiles_idx = pp.feature_index(FEAT_ONBOARD_PROFILES)?;
        let spy_idx = pp.feature_index(FEAT_MOUSE_BUTTON_SPY)?;
        // Optional: broadcast on reconnect after sleep/power-cycle - our cue to
        // re-apply volatile state. Firmware without it leans on the poll alone.
        let wireless_idx = pp.feature_index(FEAT_WIRELESS_DEVICE_STATUS).ok();
        let wake = match wireless_idx {
            Some(i) => format!("index {i}"),
            None => "unsupported (polling only)".to_string(),
        };
        log!(
            "onboard profiles = index {profiles_idx}, button spy = index {spy_idx}, wake events = {wake}"
        );

        let mut dev = Device {
            pp,
            spy_idx,
            profiles_idx,
            wireless_idx,
        };
        // What the mouse looked like before we touched it, for the log.
        match dev.probe(cfg) {
            Ok(p) => log!("mouse was: {}", p.describe()),
            Err(e) => logerr!("warning: could not read the mouse state: {e}"),
        }
        dev.apply(cfg)?;
        log!("connected");
        report_visibility(cfg);
        // Say that only once it is true. apply() returning Ok means the mouse
        // acknowledged three writes, not that they took effect - Host mode in
        // particular is what stops an onboard profile from quietly overriding
        // the mapping, and its absence looks identical from the write side.
        match dev.probe(cfg) {
            Ok(p) if p.matches(&cfg.mapping) => {}
            Ok(p) => logerr!("warning: the mouse did not take it: {}", p.describe()),
            Err(e) => logerr!("warning: could not confirm the mouse state: {e}"),
        }
        Ok(dev)
    }

    /// Assert the full desired state: Host mode, button mapping, and the spy.
    ///
    /// All three are volatile - the mouse forgets them when it sleeps or
    /// power-cycles - so this gets reasserted periodically. The spy MUST be
    /// re-armed here, not just at connect: after a power-cycle the receiver
    /// channel stays valid, so these writes succeed without ever triggering a
    /// reconnect, but the mouse has dropped the spy. Re-sending the mapping
    /// alone restores suppression while leaving the button silent - back stops
    /// navigating, yet no notifications arrive and the action never fires.
    fn apply(&mut self, cfg: &Config) -> hidpp::Result<()> {
        self.pp.call(self.profiles_idx, FN_SET_MODE, &[MODE_HOST])?;
        self.pp.call(self.spy_idx, FN_SET_MAPPING, &cfg.mapping)?;
        self.pp.call(self.spy_idx, FN_START_SPY, &[])?;
        Ok(())
    }

    /// Read back the state `apply()` asserts, writing nothing.
    ///
    /// Two zero-parameter getters: 0x8100 fn2 GetMode and 0x8110 fn3
    /// GetMouseButtonMapping. Being reads they cannot glitch a held button the
    /// way re-sending the mapping does, which is why the periodic check runs
    /// through here and only writes when this says the mouse has drifted.
    ///
    /// The spy is a gap: IMouseButtonSpy stops at fn4, so whether it is still
    /// armed cannot be read. The mapping stands in for it. Confirmed
    /// 2026-08-14: a sleep leaves the mouse in Onboard mode with the factory
    /// mapping, i.e. the reset that drops the spy also drops both readable
    /// pieces, so there is no observed state where the spy is gone and this
    /// still reports a match.
    fn probe(&mut self, cfg: &Config) -> hidpp::Result<Probe> {
        let mode = self.pp.call(self.profiles_idx, FN_GET_MODE, &[])?[0];
        let mapping = self.pp.call(self.spy_idx, FN_GET_MAPPING, &[])?;
        // Compare only the span the config covers, for the same reason
        // restore() writes only that span: bytes past the last physical button
        // are whatever the firmware feels like returning.
        Ok(Probe {
            mode,
            mapping: mapping[..cfg.mapping.len()].to_vec(),
        })
    }

    /// Re-enable every button, then hand control back to the onboard profile.
    ///
    /// Spans exactly the buttons the config's mapping spans: anything we
    /// disabled is in there by definition, and a hardcoded five would leave a
    /// button disabled until replug on any mouse with more than five.
    fn restore(&mut self, cfg: &Config) {
        let all: Vec<u8> = (1..=cfg.mapping.len() as u8).collect();
        let _ = self.pp.call(self.spy_idx, FN_SET_MAPPING, &all);
        let _ = self.pp.call(self.spy_idx, FN_STOP_SPY, &[]);
        let _ = self
            .pp
            .call(self.profiles_idx, FN_SET_MODE, &[MODE_ONBOARD]);
    }
}

/// What the mouse reports its own state to be. See `Device::probe`.
struct Probe {
    mode: u8,
    mapping: Vec<u8>,
}

impl Probe {
    /// True when the mouse is already in the state `apply()` would assert -
    /// except for the spy, which cannot be read back at all.
    fn matches(&self, mapping: &[u8]) -> bool {
        self.mode == MODE_HOST && self.mapping == mapping
    }

    /// One line for the log.
    fn describe(&self) -> String {
        let mode = match self.mode {
            MODE_HOST => "host",
            MODE_ONBOARD => "onboard",
            _ => "?",
        };
        let map = self
            .mapping
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        format!("mode={:02x} ({mode}), mapping=[{map}]", self.mode)
    }
}

/// What the daemon writes on a first run.
///
/// Deliberately *inert* - nothing disabled, no action - and a separate file
/// from `config.toml.example`, which is the fully configured reference the
/// README points at. Installing a push-to-talk daemon must not be the moment
/// your browser's back button stops working, especially when the example's
/// `rpc` action cannot do anything until Discord credentials exist. So the
/// button keeps working until the user says otherwise; the comments in the
/// file say how.
const STARTER: &str = include_str!("../config.toml.starter");

/// Where the config lives, in order of precedence:
///
///   1. the path given on the command line - explicit always wins, which is
///      what the tray's sign-in entry and `cargo run` pass;
///   2. `config.toml` beside the executable, *if it already exists* - a
///      portable install, and what every install before this one looked like;
///   3. `%APPDATA%\invisible-ptt\config.toml`, created on first run.
///
/// Never the current directory, which is what this used to default to: started
/// from the Run key the working directory is wherever the shell left it.
/// Always absolute, because the tray opens this path, the Run key entry embeds
/// it, and a restart passes it on.
fn config_path() -> PathBuf {
    if let Some(arg) = std::env::args_os().nth(1) {
        let path = PathBuf::from(arg);
        return std::path::absolute(&path).unwrap_or(path);
    }
    if let Ok(exe) = std::env::current_exe() {
        let beside = exe.with_file_name("config.toml");
        if beside.is_file() {
            return beside;
        }
    }
    match platform::config_dir() {
        Some(dir) => dir.join("config.toml"),
        None => PathBuf::from("config.toml"),
    }
}

/// Write a starter config, creating its directory.
///
/// A settings file that does not exist is the one thing the tray cannot
/// explain: the menu item opens nothing, and there is no window to say why.
/// So the daemon always has a config to point at, even on a first run with no
/// arguments and nothing installed.
fn create_config(path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, STARTER)
}

/// Connect, waiting for the mouse to turn up rather than giving up on it.
///
/// At sign-in the receiver routinely has not enumerated yet. The console app
/// could exit and let Task Scheduler retry it a minute later; a tray app
/// cannot, because a process that quits before its icon settles is
/// indistinguishable from one that never started. So it waits - which is only
/// what the runtime reconnect already does when the mouse disappears later.
///
/// Returns None if Exit or Restart was chosen while waiting.
fn connect_when_available(
    api: &hidapi::HidApi,
    cfg: &Config,
    running: &AtomicBool,
) -> Option<Device> {
    let mut reported = false;
    loop {
        match Device::connect(api, cfg) {
            Ok(dev) => return Some(dev),
            Err(e) => {
                // Once only: at sign-in this can go on for a minute, and the
                // same line every five seconds buries the log.
                if !reported {
                    logerr!("could not set up the mouse: {e}");
                    logerr!("waiting for it; note that G HUB holds the HID++ channel open");
                    reported = true;
                }
            }
        }
        // Five seconds between attempts, but checked often enough that Exit
        // doesn't appear to hang.
        for _ in 0..20 {
            if !running.load(Ordering::SeqCst) {
                return None;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

/// Read, parse, and validate a config.
///
/// The error is a sentence fit to show a human, because both callers have to
/// explain themselves to one: startup in a message box before exiting, and the
/// tray's Reload in a message box that says nothing was changed.
fn load_config(path: &Path) -> std::result::Result<Config, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let cfg: Config = toml::from_str(&text).map_err(|e| format!("bad config: {e}"))?;
    cfg.validate().map_err(|e| format!("bad config: {e}"))?;
    Ok(cfg)
}

/// Lowercase the rule keys once, so matching a foreground process is a plain
/// comparison. See `match_action`.
fn compile_rules(cfg: &Config) -> Vec<(String, Action)> {
    cfg.rules
        .iter()
        .map(|r| (r.process.to_ascii_lowercase(), parse_action(&r.action)))
        .collect()
}

/// Whether the mapping actually hides the button being watched.
///
/// Configuring this takes two edits - the `0` in `mapping` and an action - and
/// doing only the second is the easy mistake, one whose symptom is
/// "push-to-talk works but my browser still navigates back". So say which case
/// we are in, both at startup and after a reload, rather than printing the
/// same confident line either way.
fn report_visibility(cfg: &Config) {
    if cfg.mapping.get(usize::from(cfg.button)) == Some(&0) {
        log!("button {} is invisible to the OS", cfg.button);
    } else {
        log!(
            "button {} still reaches Windows normally - put a 0 in mapping[{}] to hide it",
            cfg.button,
            cfg.button
        );
    }
}

/// Nowhere to print to and no window to put it in, so a startup failure gets a
/// message box. Without one the app would simply never appear.
fn fatal(msg: &str) -> ! {
    logerr!("{msg}");
    platform::error_box(msg);
    std::process::exit(1)
}

fn main() {
    let path = config_path();
    // Beside the config, which is the one directory the daemon already assumes
    // it can write to (token refresh rewrites config.toml in place).
    let log_path = path.with_file_name("invisible-ptt.log");
    log::init(&log_path);
    // The log outlives any one session, so mark where this one starts.
    log!("--- invisible-ptt starting on {}", path.display());

    if !path.exists() {
        match create_config(&path) {
            // Loud on purpose: this is the one moment the user has to be told
            // where their settings live, and Open settings file is how they
            // get back to it.
            Ok(()) => log!(
                "first run: wrote a starter config to {}. It changes nothing until you edit it - Open settings file in the tray menu.",
                path.display()
            ),
            Err(e) => fatal(&format!("could not create {}: {e}", path.display())),
        }
    }

    let mut cfg = match load_config(&path) {
        Ok(cfg) => cfg,
        Err(e) => fatal(&e),
    };

    // Discord access tokens expire after 7 days. If we have the credentials to
    // refresh, do so at startup and write the rotated tokens back, so a restart
    // always begins with a valid token and no browser round-trip is needed.
    let mut refreshed_at_startup = false;
    if let Some((access, refresh)) = discord::refresh(&cfg.discord) {
        cfg.discord.access_token = access.clone();
        cfg.discord.refresh_token = refresh.clone();
        refreshed_at_startup = true;
        match discord::persist_tokens(&path, &access, &refresh) {
            Ok(()) => log!("refreshed discord access token"),
            Err(e) => logerr!(
                "warning: refreshed token but could not save it to {}: {e}",
                path.display()
            ),
        }
    }

    // Rebuilt from scratch on every reload, hence mutable.
    let mut default_action = parse_action(&cfg.default_action);
    let mut rules = compile_rules(&cfg);

    let api = match hidapi::HidApi::new() {
        Ok(a) => a,
        Err(e) => fatal(&format!("hidapi init failed: {e}")),
    };

    let running = Arc::new(AtomicBool::new(true));
    // What the tray's Pause tick says. The loop owns what has actually been
    // done to the mouse (`paused_now` below); this is only the request.
    let paused = Arc::new(AtomicBool::new(false));
    // The tray parses a reloaded config and sends it here; this thread owns
    // everything that has to change as a result.
    let (reload_tx, reload_rx) = std::sync::mpsc::channel::<Config>();
    {
        let r = running.clone();
        let _ = ctrlc::set_handler(move || r.store(false, Ordering::SeqCst));
    }

    // Before the mouse, so that waiting for it still gives the user something
    // to click Exit on.
    let tray = tray::spawn(tray::Controls {
        running: running.clone(),
        paused: paused.clone(),
        reload: reload_tx,
        config: path.clone(),
        log: log_path,
    });

    // Where the icon starts anyway, said out loud because this is the one
    // place it stays for any length of time: at sign-in the receiver has
    // routinely not enumerated yet, and a grey icon is the only thing telling
    // the user the wait is the mouse's and not the daemon's.
    tray.set_status(tray::Status::Waiting);

    let mut dev = match connect_when_available(&api, &cfg, &running) {
        Some(dev) => dev,
        None => {
            // Exit chosen before the mouse ever turned up. Nothing to restore:
            // we never got as far as changing anything.
            tray.shutdown();
            return;
        }
    };

    let mut discord = discord::RpcHandle::spawn(&cfg.discord);
    let mut bit: u16 = 1 << cfg.button;
    // A config from the tray, waiting for a safe moment to be applied.
    let mut pending_reload: Option<Config> = None;
    // Whether the mouse has actually been handed back, as opposed to whether
    // the menu says it should be. The two differ for as long as the write is
    // waiting on the held-button gate below, and every "do we still own this
    // mouse" decision in the loop asks this one, not the atomic.
    let mut paused_now = false;
    let mut held = false;
    let mut active: Action = Action::None;
    let mut last_poll = Instant::now();
    // Tracks reachability so the poll only logs on the transition, not every
    // failed attempt while the mouse is away. Also what the icon shows.
    let mut connected = true;
    // Something suggested the mouse may have gone, so run the fallback poll on
    // the next pass instead of up to 30s later. Only ever set while we still
    // believe we are connected: once the poll agrees the mouse is gone, going
    // back to the slow cadence is what keeps a long absence from re-enumerating
    // every HID device on the machine several times a second. A false alarm
    // costs two reads and changes nothing.
    let mut check_now = false;
    // Full physical button bitmask from the last spy event, plus when we last
    // saw any button activity. Re-sending the button mapping glitches a held
    // button into a momentary release - catastrophic mid-game (e.g. an
    // interrupted hold-to-fire). So any write is deferred until no button is
    // held AND things have been quiet briefly.
    let mut button_state: u16 = 0;
    let mut last_button_event = Instant::now();
    // Refresh the Discord token a day before its 7-day expiry so a daemon left
    // running for weeks never lapses. A successful startup refresh puts the
    // next one six days out; a failed one must not, because the token on disk
    // may be nearly expired and we just learned we cannot replace it. That case
    // is the common one, not the exotic one: the daemon is meant to run at
    // logon, where the network is routinely not up yet.
    let refresh_window = Duration::from_secs(6 * 24 * 60 * 60);
    let retry_window = Duration::from_secs(60 * 60);
    let mut next_refresh = Instant::now()
        + if refreshed_at_startup {
            refresh_window
        } else {
            retry_window
        };

    log!("running - Exit from the tray menu stops and restores the mouse");

    while running.load(Ordering::SeqCst) {
        // Recomputed here rather than at each of the places these change,
        // because there are a dozen of those - every release() call site, both
        // reconnect paths - and one of them would eventually be missed, leaving
        // a green icon on a mouse that had gone away. The loop comes back
        // through here within its 100ms event timeout, and immediately after
        // handling a press or a release, so "immediate" is what this looks
        // like. `set_status` costs an atomic compare when nothing has changed.
        //
        // An action of `none` is deliberately not Talking: the button is down,
        // but nothing is being transmitted, and an icon that says otherwise on
        // the inert starter config would be a lie on the very first run.
        //
        // Paused outranks the rest, including a missing mouse: while paused
        // nothing polls for the mouse, so `connected` is only as fresh as the
        // moment we stood down - and "paused" is the honest answer to what the
        // daemon is doing either way.
        tray.set_status(if paused_now {
            tray::Status::Paused
        } else if !connected {
            tray::Status::Waiting
        } else if held && active != Action::None {
            tray::Status::Talking
        } else {
            tray::Status::Ready
        });

        // Pause and resume, from the tray. Pausing means handing the mouse
        // back exactly as Exit would - the button navigates again, the spy
        // stops - which is a write, so it waits for the same gate as every
        // other write: nothing held, and quiet for 500ms. In practice that is
        // the 500ms after the click that closed the menu.
        if paused.load(Ordering::SeqCst) != paused_now
            && button_state == 0
            && last_button_event.elapsed() > Duration::from_millis(500)
        {
            paused_now = !paused_now;
            if paused_now {
                // The action in flight is ours to end: with the spy stopped,
                // the release event for a button held right now is never coming.
                release(&mut held, &mut active, &discord);
                dev.restore(&cfg);
                log!("paused - the mouse is Windows' again until you resume");
            } else {
                match dev.apply(&cfg) {
                    Ok(()) => {
                        log!("resumed");
                        report_visibility(&cfg);
                        // Nothing for the poll to find; give it a full interval.
                        last_poll = Instant::now();
                    }
                    // The mouse is away. Leave last_poll alone - it has not
                    // run since before the pause, so the poll below fires on
                    // this very pass and reconnects.
                    Err(e) => logerr!("resumed, but the mouse did not take it: {e}"),
                }
            }
        }

        // Reload settings, from the tray. It arrives already parsed and
        // validated - a bad file never gets this far, and the running
        // configuration survives it untouched.
        if let Ok(new) = reload_rx.try_recv() {
            pending_reload = Some(new);
        }
        // Applied under the same rule as the reassert below: never write to
        // the mouse while a button is down. The click that opened the menu is
        // long since up, so in practice this waits 500ms and no longer.
        if pending_reload.is_some()
            && button_state == 0
            && last_button_event.elapsed() > Duration::from_millis(500)
        {
            let new = pending_reload.take().expect("checked on the line above");
            // The action in flight belongs to the old configuration and may
            // not exist in the new one. A synthesised key left logically down
            // would never come back up.
            release(&mut held, &mut active, &discord);
            // Reconnecting Discord costs a handshake on the next press, so
            // only do it when the credentials actually changed.
            let credentials_changed = new.discord.client_id != cfg.discord.client_id
                || new.discord.access_token != cfg.discord.access_token
                || new.discord.refresh_token != cfg.discord.refresh_token;
            cfg = new;
            default_action = parse_action(&cfg.default_action);
            rules = compile_rules(&cfg);
            bit = 1 << cfg.button;
            if credentials_changed {
                discord.reconfigure(&cfg.discord);
                // Whatever the old token's schedule was, it is not this one's.
                next_refresh = Instant::now();
            }
            if paused_now {
                // Nothing goes to the mouse while paused - writing the new
                // mapping is precisely what un-pausing means. Resume applies it.
                log!("reloaded {} (still paused)", path.display());
            } else {
                // Writing the new mapping is also the only way to give back a
                // button the old one had disabled.
                match dev.apply(&cfg) {
                    Ok(()) => {
                        log!("reloaded {}", path.display());
                        report_visibility(&cfg);
                    }
                    // The mouse is away; the reconnect below re-applies whatever
                    // is current, which is now the config we just swapped in.
                    Err(e) => logerr!("reloaded, but the mouse did not take it: {e}"),
                }
            }
        }

        // Proactive Discord token refresh for long-lived sessions. On success
        // hand the new credentials to the RPC worker, which drops the current
        // connection and re-authenticates on the next press.
        if !cfg.discord.refresh_token.is_empty() && Instant::now() >= next_refresh {
            if let Some((access, refresh)) = discord::refresh(&cfg.discord) {
                cfg.discord.access_token = access.clone();
                cfg.discord.refresh_token = refresh.clone();
                let _ = discord::persist_tokens(&path, &access, &refresh);
                discord.reconfigure(&cfg.discord);
                log!("refreshed discord access token");
                next_refresh = Instant::now() + refresh_window;
            } else {
                next_refresh = Instant::now() + retry_window;
            }
        }

        // Fallback check. The wake event below handles the common
        // sleep/power-cycle case immediately; this only catches sleep modes
        // that drop volatile state without broadcasting a reconnect. Poll
        // slowly when we have the wake event, quickly when flying blind.
        let interval = if dev.wireless_idx.is_some() {
            Duration::from_secs(30)
        } else {
            Duration::from_secs(5)
        };
        // Only ever write when nothing is held and the buttons have been quiet
        // for a moment - never in the middle of a hold or a burst of clicks.
        // Deferring is safe: an active session keeps the mouse awake, so it
        // hasn't forgotten anything, and the wake event covers the cases where
        // it has.
        // Not while paused: the mouse is meant to be in its own state, and
        // this is the one thing in the loop whose whole job is to put it back
        // in ours.
        if !paused_now
            && (check_now || last_poll.elapsed() > interval)
            && button_state == 0
            && last_button_event.elapsed() > Duration::from_millis(500)
        {
            last_poll = Instant::now();
            check_now = false;
            // Read before writing. In steady state the mouse still holds
            // everything we asked for, so this writes nothing at all - which is
            // the point. The gate above is checked before apply() sends its
            // three round-trips, so a button going down in between still gets
            // its mapping rewritten underneath it; the only way to make that
            // race vanish is to stop doing the write. What is left fires solely
            // when the mouse has reset itself, and a mouse that has reset was
            // asleep, so nothing can be held.
            //
            // Skipped while the mouse is unreachable: there is nothing to read
            // there, and the reconnect below is what that state needs anyway.
            let fresh = connected
                && match dev.probe(&cfg) {
                    Ok(p) if p.matches(&cfg.mapping) => true,
                    Ok(p) => {
                        logerr!("mouse forgot its configuration ({})", p.describe());
                        false
                    }
                    // Unreadable means unreachable, which the reconnect handles.
                    Err(_) => false,
                };
            if !fresh {
                // apply() failing means the mouse is unreachable; try a full
                // reconnect, which also covers a receiver replug (dead handle,
                // no wake event). Log only when reachability actually flips.
                let ok = dev.apply(&cfg).is_ok()
                    || match Device::connect(&api, &cfg) {
                        Ok(d) => {
                            dev = d;
                            true
                        }
                        Err(_) => false,
                    };
                if ok && !connected {
                    logerr!("mouse back");
                } else if !ok && connected {
                    logerr!("lost the mouse; waiting for it to come back...");
                }
                connected = ok;
            }
        }

        let event = match dev.pp.next_event(100) {
            Ok(Some(e)) => e,
            Ok(None) => continue,
            Err(_) => {
                // A read error means the device is gone; drop any stale "held"
                // state so a stuck bitmask can't block the reconnect poll. The
                // release event for a button held right now will never arrive,
                // so end the hold ourselves rather than leave it open.
                button_state = 0;
                release(&mut held, &mut active, &discord);
                // Have the poll confirm it now rather than at its own pace: an
                // unplugged receiver is exactly the case that never broadcasts
                // anything, and until the poll runs the icon still claims to be
                // listening. It is the poll that decides - a single failed read
                // is not evidence enough to grey out an icon by itself.
                check_now = connected;
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };

        // WirelessDeviceStatus broadcast: the mouse just reconnected and
        // dropped Host mode, the mapping, and the spy. Re-arm immediately
        // rather than waiting out the fallback poll.
        if Some(event[2]) == dev.wireless_idx {
            // A mouse that wakes while we are paused has come back in the
            // state we want it in - the factory one. Leave it there.
            if paused_now {
                continue;
            }
            logerr!("wake event -> reasserting mouse state");
            // The wake line already announces recovery, so update state
            // silently to keep the poll from printing "mouse back" too. A hold
            // cannot have survived the disconnect, and its release event is
            // gone with it, so close it out here.
            button_state = 0;
            release(&mut held, &mut active, &discord);
            // Deliberately does not probe first. The mouse has just come back
            // from a state that drops everything, so the read would only cost
            // two round-trips to confirm what we already know, and recovery
            // here is meant to be instant. Nothing can be held across a
            // disconnect either, so the write is free of the race the poll
            // avoids. The duplicate broadcast makes this run twice; apply() is
            // idempotent.
            connected = dev.apply(&cfg).is_ok();
            last_poll = Instant::now();
            continue;
        }

        // Feature index must match the spy, and the high nibble of byte 3
        // is the event index - 0 is MouseButtonEvent. Paused means the spy is
        // stopped, so there should be nothing here to read; belt and braces,
        // because acting on a button Windows can also see would fire twice.
        if paused_now || event[2] != dev.spy_idx || (event[3] >> 4) != 0 {
            continue;
        }

        let state = u16::from_be_bytes([event[4], event[5]]);
        // Remember the full button state so the reassert can stay clear of any
        // hold, not just the PTT button.
        button_state = state;
        last_button_event = Instant::now();
        let now_down = state & bit != 0;
        if now_down == held {
            continue;
        }

        if now_down {
            held = true;
            active = pick_action(&rules, default_action);
            match active {
                Action::Key(vk) => platform::key(vk, true),
                Action::Rpc => discord.set_mute(false),
                Action::None => {}
            }
        } else {
            release(&mut held, &mut active, &discord);
        }
    }

    // Always give the mouse back in a usable state.
    log!("restoring mouse...");
    release(&mut held, &mut active, &discord);
    dev.restore(&cfg);
    // Only now wait for that release to actually reach Discord: this is the one
    // place we block on it, so the mouse is already restored if Discord hangs.
    discord.shutdown();
    tray.shutdown();
}

/// End the hold in progress, if there is one.
///
/// Every path out of a hold goes through here, including the two where the
/// mouse vanishes mid-press and the release event is never coming. Those used
/// to leave the action asserted indefinitely: a synthesised key stays logically
/// down in Windows until its key-up arrives, and Discord keeps transmitting -
/// a hot mic being the one failure a push-to-talk key must not have.
fn release(held: &mut bool, active: &mut Action, discord: &discord::RpcHandle) {
    if !*held {
        return;
    }
    match *active {
        Action::Key(vk) => platform::key(vk, false),
        Action::Rpc => discord.set_mute(true),
        Action::None => {}
    }
    *held = false;
    *active = Action::None;
}

fn pick_action(rules: &[(String, Action)], default: Action) -> Action {
    match platform::foreground_process() {
        Some(exe) => match_action(&exe, rules, default),
        None => default,
    }
}

/// Match a foreground executable name against the rules: case-insensitive and
/// exact (no substring), first hit wins, else the default. Rule keys are
/// already lowercased when the rule list is built.
fn match_action(exe: &str, rules: &[(String, Action)], default: Action) -> Action {
    let exe = exe.to_ascii_lowercase();
    for (proc_name, action) in rules {
        if exe == *proc_name {
            return *action;
        }
    }
    default
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(mode: u8, mapping: &[u8]) -> Probe {
        Probe {
            mode,
            mapping: mapping.to_vec(),
        }
    }

    #[test]
    fn probe_matches_only_in_host_mode_with_the_configured_mapping() {
        let want = [1, 2, 3, 0, 5];
        assert!(probe(MODE_HOST, &want).matches(&want));
        // Host mode with the mapping forgotten: back navigates again.
        assert!(!probe(MODE_HOST, &[1, 2, 3, 4, 5]).matches(&want));
        // Mapping intact but onboard profiles back in charge, which silently
        // overrides it - the state that made SetMode mandatory in the first place.
        assert!(!probe(MODE_ONBOARD, &want).matches(&want));
    }

    #[test]
    fn probe_notices_a_button_other_than_the_ptt_one() {
        // Divergence anywhere in the span counts: a mapping we did not write is
        // a mapping the mouse has reset, whichever byte gives it away.
        assert!(!probe(MODE_HOST, &[1, 2, 3, 0, 0]).matches(&[1, 2, 3, 0, 5]));
    }

    #[test]
    fn parse_action_rpc_is_case_insensitive_and_trimmed() {
        assert_eq!(parse_action("rpc"), Action::Rpc);
        assert_eq!(parse_action("RPC"), Action::Rpc);
        assert_eq!(parse_action("  Rpc  "), Action::Rpc);
    }

    #[test]
    fn parse_action_single_char_key_uses_uppercase_vk() {
        // Windows VKs for letters/digits are the uppercase ASCII byte.
        assert_eq!(parse_action("key:V"), Action::Key(0x56));
        assert_eq!(parse_action("key:v"), Action::Key(0x56));
        assert_eq!(parse_action("key:5"), Action::Key(0x35));
    }

    #[test]
    fn parse_action_hex_and_named_keys() {
        assert_eq!(parse_action("key:0x13"), Action::Key(0x13));
        assert_eq!(parse_action("key:PAUSE"), Action::Key(0x13));
        assert_eq!(parse_action("key:F13"), Action::Key(0x7C));
        // Whitespace inside the value is trimmed.
        assert_eq!(parse_action("key: F13 "), Action::Key(0x7C));
    }

    #[test]
    fn parse_action_invalid_falls_back_to_none() {
        assert_eq!(parse_action("none"), Action::None);
        assert_eq!(parse_action("wat"), Action::None);
        assert_eq!(parse_action("key:"), Action::None); // empty value
        assert_eq!(parse_action("key:0xZZ"), Action::None); // bad hex -> vk 0
        assert_eq!(parse_action("key:NOPE"), Action::None); // unknown name -> vk 0
    }

    #[test]
    fn match_action_is_case_insensitive_exact_first_wins() {
        // Rule keys arrive already lowercased (see how `rules` is built).
        let rules = vec![
            ("chrome.exe".to_string(), Action::Key(0x56)),
            ("cs2.exe".to_string(), Action::Rpc),
        ];
        // Case-insensitive on the incoming exe name.
        assert_eq!(
            match_action("Chrome.EXE", &rules, Action::None),
            Action::Key(0x56)
        );
        assert_eq!(match_action("cs2.exe", &rules, Action::None), Action::Rpc);
        // No rule -> default.
        assert_eq!(
            match_action("notepad.exe", &rules, Action::Rpc),
            Action::Rpc
        );
        // Exact only: a substring must not match.
        assert_eq!(
            match_action("notchrome.exe", &rules, Action::None),
            Action::None
        );
    }

    fn config(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn the_starter_config_parses_and_validates() {
        // What create_config() writes on first run. If it ever stops being a
        // working config, a first run is a message box, not a daemon.
        let cfg: Config = toml::from_str(STARTER).expect("the starter must parse");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn the_starter_config_changes_nothing_until_configured() {
        // The whole point of shipping a starter separate from the example:
        // installing the daemon must not take a mouse button away from someone
        // who has not yet said which one, or asked for anything to happen.
        let cfg: Config = toml::from_str(STARTER).unwrap();
        assert!(
            cfg.mapping.iter().all(|&code| code != 0),
            "the starter must not disable a button: {:?}",
            cfg.mapping
        );
        assert_eq!(parse_action(&cfg.default_action), Action::None);
        // A rule would fire its action on top of the button still working
        // normally, which is not inert either.
        assert!(cfg.rules.is_empty());
    }

    #[test]
    fn the_documented_example_is_a_working_config() {
        // Not embedded in the binary - only the starter is - but it is what
        // the README tells people to copy, so it has to parse and validate.
        let text = include_str!("../config.toml.example");
        let cfg: Config = toml::from_str(text).expect("config.toml.example must parse");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_accepts_the_shipped_example() {
        let cfg = config("button = 3\nmapping = [1, 2, 3, 0, 5]\n");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_a_button_the_mapping_does_not_cover() {
        // Would build a mask for a button that cannot be reported.
        let cfg = config("button = 5\nmapping = [1, 2, 3, 0, 5]\n");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_mappings_that_do_not_fit_a_frame() {
        // 17 entries would run off the end of the 20-byte HID++ frame.
        let long = format!("button = 0\nmapping = [{}]\n", vec!["1"; 17].join(", "));
        assert!(config(&long).validate().is_err());
        assert!(config("button = 0\nmapping = []\n").validate().is_err());
        // The boundary itself is fine.
        let full = format!("button = 15\nmapping = [{}]\n", vec!["1"; 16].join(", "));
        assert!(config(&full).validate().is_ok());
    }

    #[test]
    fn config_defaults() {
        // A minimal config: only the required fields, no rules or discord.
        let cfg: Config = toml::from_str("button = 3\nmapping = [1, 2, 3, 0, 5]\n").unwrap();
        assert_eq!(cfg.button, 3);
        assert_eq!(cfg.mapping, vec![1, 2, 3, 0, 5]);
        assert_eq!(cfg.default_action, "none"); // default_action() default
        assert!(cfg.rules.is_empty());
        assert!(cfg.discord.client_id.is_empty());
        assert!(cfg.discord.refresh_token.is_empty());
    }
}
