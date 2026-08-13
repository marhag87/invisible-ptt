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

mod discord;
mod hidpp;
mod platform;

use hidpp::*;
use serde::Deserialize;
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

#[derive(Debug, Deserialize)]
struct Rule {
    /// Executable name, case-insensitive, e.g. "chrome.exe"
    process: String,
    action: String,
}

#[derive(Debug, Default, Deserialize)]
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
        eprintln!("warning: could not parse action '{s}', treating as none");
    }
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
        println!(
            "onboard profiles = index {profiles_idx}, button spy = index {spy_idx}, wake events = {wake}"
        );

        let mut dev = Device {
            pp,
            spy_idx,
            profiles_idx,
            wireless_idx,
        };
        dev.apply(cfg)?;
        println!(
            "connected; button {} is now invisible to the OS",
            cfg.button
        );
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
        self.pp.call(self.profiles_idx, 1, &[MODE_HOST])?;
        self.pp.call(self.spy_idx, 4, &cfg.mapping)?;
        self.pp.call(self.spy_idx, 1, &[])?; // StartMouseButtonSpy
        Ok(())
    }

    fn restore(&mut self) {
        // Re-enable every button, then hand control back to the onboard profile.
        let all: Vec<u8> = (1..=self.mapping_len()).map(|i| i as u8).collect();
        let _ = self.pp.call(self.spy_idx, 4, &all);
        let _ = self.pp.call(self.spy_idx, 2, &[]); // StopMouseButtonSpy
        let _ = self.pp.call(self.profiles_idx, 1, &[MODE_ONBOARD]);
    }

    fn mapping_len(&self) -> usize {
        5
    }
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());

    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("could not read {path}: {e}");
            eprintln!("see config.toml.example");
            std::process::exit(1);
        }
    };
    let mut cfg: Config = match toml::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("bad config: {e}");
            std::process::exit(1);
        }
    };

    // Discord access tokens expire after 7 days. If we have the credentials to
    // refresh, do so at startup and write the rotated tokens back, so a restart
    // always begins with a valid token and no browser round-trip is needed.
    if let Some((access, refresh)) = discord::refresh(&cfg.discord) {
        cfg.discord.access_token = access.clone();
        cfg.discord.refresh_token = refresh.clone();
        match discord::persist_tokens(&path, &access, &refresh) {
            Ok(()) => println!("refreshed discord access token"),
            Err(e) => eprintln!("warning: refreshed token but could not save it to {path}: {e}"),
        }
    }

    let default_action = parse_action(&cfg.default_action);
    let rules: Vec<(String, Action)> = cfg
        .rules
        .iter()
        .map(|r| (r.process.to_ascii_lowercase(), parse_action(&r.action)))
        .collect();

    let api = match hidapi::HidApi::new() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("hidapi init failed: {e}");
            std::process::exit(1);
        }
    };

    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        let _ = ctrlc::set_handler(move || r.store(false, Ordering::SeqCst));
    }

    let mut dev = match Device::connect(&api, &cfg) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("could not set up the mouse: {e}");
            eprintln!("is G HUB running? it holds the HID++ channel open.");
            std::process::exit(1);
        }
    };

    let mut discord = discord::Rpc::new(&cfg.discord);
    let bit: u16 = 1 << cfg.button;
    let mut held = false;
    let mut active: Action = Action::None;
    let mut last_reassert = Instant::now();
    // Tracks reachability so the poll only logs on the transition, not every
    // failed attempt while the mouse is away.
    let mut connected = true;
    // Full physical button bitmask from the last spy event, plus when we last
    // saw any button activity. The reassert re-sends the button mapping, which
    // glitches a held button into a momentary release - catastrophic mid-game
    // (e.g. an interrupted hold-to-fire). So the reassert is deferred until no
    // button is held AND things have been quiet briefly.
    let mut button_state: u16 = 0;
    let mut last_button_event = Instant::now();
    // Refresh the Discord token a day before its 7-day expiry so a daemon left
    // running for weeks never lapses. Startup already refreshed, so the first
    // one is six days out; on failure we retry within the hour.
    let refresh_window = Duration::from_secs(6 * 24 * 60 * 60);
    let mut next_refresh = Instant::now() + refresh_window;

    println!("running - ctrl-c to stop and restore the mouse");

    while running.load(Ordering::SeqCst) {
        // Proactive Discord token refresh for long-lived sessions. On success
        // rebuild the RPC client so it re-authenticates with the new token.
        if !cfg.discord.refresh_token.is_empty() && Instant::now() >= next_refresh {
            if let Some((access, refresh)) = discord::refresh(&cfg.discord) {
                cfg.discord.access_token = access.clone();
                cfg.discord.refresh_token = refresh.clone();
                let _ = discord::persist_tokens(&path, &access, &refresh);
                discord = discord::Rpc::new(&cfg.discord);
                println!("refreshed discord access token");
                next_refresh = Instant::now() + refresh_window;
            } else {
                next_refresh = Instant::now() + Duration::from_secs(60 * 60);
            }
        }

        // Fallback reassert. The wake event below handles the common
        // sleep/power-cycle case immediately; this only catches sleep modes
        // that drop volatile state without broadcasting a reconnect. Poll
        // slowly when we have the wake event, quickly when flying blind.
        let interval = if dev.wireless_idx.is_some() {
            Duration::from_secs(30)
        } else {
            Duration::from_secs(5)
        };
        // Only ever reassert when nothing is held and the buttons have been
        // quiet for a moment - never in the middle of a hold or a burst of
        // clicks. Deferring is safe: an active session keeps the mouse awake,
        // so it hasn't forgotten anything, and the wake event covers the cases
        // where it has.
        if last_reassert.elapsed() > interval
            && button_state == 0
            && last_button_event.elapsed() > Duration::from_millis(500)
        {
            // apply() failing means the mouse is unreachable; try a full
            // reconnect, which also covers a receiver replug (dead handle, no
            // wake event). Log only when reachability actually flips.
            let ok = dev.apply(&cfg).is_ok()
                || match Device::connect(&api, &cfg) {
                    Ok(d) => {
                        dev = d;
                        true
                    }
                    Err(_) => false,
                };
            if ok && !connected {
                eprintln!("mouse back");
            } else if !ok && connected {
                eprintln!("lost the mouse; waiting for it to come back...");
            }
            connected = ok;
            last_reassert = Instant::now();
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
                release(&mut held, &mut active, &mut discord);
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };

        // WirelessDeviceStatus broadcast: the mouse just reconnected and
        // dropped Host mode, the mapping, and the spy. Re-arm immediately
        // rather than waiting out the fallback poll.
        if Some(event[2]) == dev.wireless_idx {
            eprintln!("wake event -> reasserting mouse state");
            // The wake line already announces recovery, so update state
            // silently to keep the poll from printing "mouse back" too. A hold
            // cannot have survived the disconnect, and its release event is
            // gone with it, so close it out here.
            button_state = 0;
            release(&mut held, &mut active, &mut discord);
            connected = dev.apply(&cfg).is_ok();
            last_reassert = Instant::now();
            continue;
        }

        // Feature index must match the spy, and the high nibble of byte 3
        // is the event index - 0 is MouseButtonEvent.
        if event[2] != dev.spy_idx || (event[3] >> 4) != 0 {
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
            release(&mut held, &mut active, &mut discord);
        }
    }

    // Always give the mouse back in a usable state.
    println!("restoring mouse...");
    release(&mut held, &mut active, &mut discord);
    dev.restore();
}

/// End the hold in progress, if there is one.
///
/// Every path out of a hold goes through here, including the two where the
/// mouse vanishes mid-press and the release event is never coming. Those used
/// to leave the action asserted indefinitely: a synthesised key stays logically
/// down in Windows until its key-up arrives, and Discord keeps transmitting -
/// a hot mic being the one failure a push-to-talk key must not have.
fn release(held: &mut bool, active: &mut Action, discord: &mut discord::Rpc) {
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
