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
    pub access_token: String,
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
}

impl Device {
    fn connect(api: &hidapi::HidApi, cfg: &Config) -> hidpp::Result<Self> {
        let mut pp = HidPp::open(api)?;
        let profiles_idx = pp.feature_index(FEAT_ONBOARD_PROFILES)?;
        let spy_idx = pp.feature_index(FEAT_MOUSE_BUTTON_SPY)?;
        println!("onboard profiles = index {profiles_idx}, button spy = index {spy_idx}");

        let mut dev = Device {
            pp,
            spy_idx,
            profiles_idx,
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
    let cfg: Config = match toml::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("bad config: {e}");
            std::process::exit(1);
        }
    };

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

    println!("running - ctrl-c to stop and restore the mouse");

    while running.load(Ordering::SeqCst) {
        // Reassert periodically. If the mouse slept and forgot the mapping,
        // the button would silently start producing real clicks again.
        if last_reassert.elapsed() > Duration::from_secs(5) {
            if dev.apply(&cfg).is_err() {
                eprintln!("lost the mouse, reconnecting...");
                if let Ok(d) = Device::connect(&api, &cfg) {
                    dev = d;
                }
            }
            last_reassert = Instant::now();
        }

        let event = match dev.pp.next_event(100) {
            Ok(Some(e)) => e,
            Ok(None) => continue,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };

        // Feature index must match the spy, and the high nibble of byte 3
        // is the event index - 0 is MouseButtonEvent.
        if event[2] != dev.spy_idx || (event[3] >> 4) != 0 {
            continue;
        }

        let state = u16::from_be_bytes([event[4], event[5]]);
        let now_down = state & bit != 0;
        if now_down == held {
            continue;
        }
        held = now_down;

        if held {
            active = pick_action(&rules, default_action);
            match active {
                Action::Key(vk) => platform::key(vk, true),
                Action::Rpc => discord.set_mute(false),
                Action::None => {}
            }
        } else {
            match active {
                Action::Key(vk) => platform::key(vk, false),
                Action::Rpc => discord.set_mute(true),
                Action::None => {}
            }
            active = Action::None;
        }
    }

    // Always give the mouse back in a usable state.
    println!("restoring mouse...");
    if held {
        match active {
            Action::Key(vk) => platform::key(vk, false),
            Action::Rpc => discord.set_mute(true),
            Action::None => {}
        }
    }
    dev.restore();
}

fn pick_action(rules: &[(String, Action)], default: Action) -> Action {
    let Some(exe) = platform::foreground_process() else {
        return default;
    };
    let exe = exe.to_ascii_lowercase();
    for (proc_name, action) in rules {
        if exe == *proc_name {
            return *action;
        }
    }
    default
}
