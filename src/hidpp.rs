//! Minimal HID++ 2.0 client, enough to drive ONBOARD PROFILES (0x8100)
//! and MOUSE BUTTON SPY (0x8110) on a Logitech mouse.
//!
//! Frame layout (long report, 20 bytes):
//!   [0] 0x11            report id
//!   [1] device index    0x01 = first device behind a receiver, 0xFF = wired
//!   [2] feature index   resolved at runtime via the ROOT feature
//!   [3] (fn << 4) | swId
//!   [4..20] parameters
//!
//! Responses echo our software id in the low nibble. Notifications carry
//! swId 0, with the event index in the high nibble - that is how we tell
//! a button event apart from a command reply.

use hidapi::{HidApi, HidDevice};
use std::collections::VecDeque;

pub const REPORT_LONG: u8 = 0x11;
pub const REPORT_SHORT: u8 = 0x10;
const SW_ID: u8 = 0x0A; // any non-zero 4-bit value

#[allow(dead_code)] // root feature ID; its index is always 0x00 so it is never resolved
pub const FEAT_ROOT: u16 = 0x0000;
pub const FEAT_ONBOARD_PROFILES: u16 = 0x8100;
pub const FEAT_MOUSE_BUTTON_SPY: u16 = 0x8110;
/// Broadcasts an event when a wireless device reconnects and its volatile
/// state needs re-applying. Optional - not every firmware exposes it.
pub const FEAT_WIRELESS_DEVICE_STATUS: u16 = 0x1D4B;

pub const MODE_ONBOARD: u8 = 0x01;
pub const MODE_HOST: u8 = 0x02;

#[derive(Debug)]
pub enum Error {
    Hid(hidapi::HidError),
    NoDevice,
    Timeout,
    /// HID++ error page returned by the device.
    Device(u8),
    FeatureMissing(u16),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Hid(e) => write!(f, "hid error: {e}"),
            Error::NoDevice => write!(f, "no Logitech HID++ interface found"),
            Error::Timeout => write!(f, "device did not answer"),
            Error::Device(c) => write!(f, "device returned HID++ error 0x{c:02x}"),
            Error::FeatureMissing(id) => write!(f, "device does not support feature 0x{id:04x}"),
        }
    }
}

impl From<hidapi::HidError> for Error {
    fn from(e: hidapi::HidError) -> Self {
        Error::Hid(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub struct HidPp {
    dev: HidDevice,
    dev_index: u8,
    /// Notifications seen while waiting for a command reply.
    pending: VecDeque<[u8; 20]>,
}

impl HidPp {
    /// Find the Logitech HID++ interface and confirm a device answers on it.
    ///
    /// HID++ lives on a vendor-defined collection (usage page 0xFF00), NOT on
    /// the mouse collection. That distinction is what makes this reachable on
    /// Windows at all - Windows will not hand out the mouse collection, but a
    /// vendor collection is fair game.
    pub fn open(api: &HidApi) -> Result<Self> {
        let mut ifaces: Vec<(&str, u16)> = Vec::new();

        for info in api.device_list() {
            if info.vendor_id() != 0x046D {
                continue;
            }
            if info.usage_page() != 0xFF00 {
                continue;
            }
            let path = info.path().to_str().unwrap_or_default();
            ifaces.push((path, info.usage()));
        }

        // A Logitech receiver exposes two vendor collections: usage 0x01 for
        // short reports (0x10) and 0x02 for long reports (0x11). We speak long
        // reports exclusively, so try the long collection first - writing 20
        // bytes to the short one just fails.
        ifaces.sort_by_key(|(_, usage)| if *usage == 0x02 { 0 } else { 1 });

        let mut candidates: Vec<(&str, u8)> = Vec::new();
        for (path, _) in ifaces {
            // 0x01 = first paired device behind a receiver; 0xFF = wired/direct.
            candidates.push((path, 0x01));
            candidates.push((path, 0xFF));
        }

        for (path, idx) in candidates {
            let cpath = std::ffi::CString::new(path).unwrap();
            let Ok(dev) = api.open_path(&cpath) else {
                continue;
            };
            let mut probe = HidPp {
                dev,
                dev_index: idx,
                pending: VecDeque::new(),
            };
            // ROOT/GetProtocolVersion - cheapest liveness check there is.
            if probe.call(0x00, 1, &[]).is_ok() {
                return Ok(probe);
            }
        }
        Err(Error::NoDevice)
    }

    /// Resolve a feature id to its device-specific index.
    ///
    /// Indices are NOT stable across firmware revisions, so never hardcode
    /// them - the 7 and 8 we used by hand on Linux are this mouse's current
    /// values, nothing more.
    pub fn feature_index(&mut self, feature: u16) -> Result<u8> {
        let r = self.call(0x00, 0, &[(feature >> 8) as u8, feature as u8])?;
        match r[0] {
            0 => Err(Error::FeatureMissing(feature)),
            idx => Ok(idx),
        }
    }

    /// Send a command and wait for its reply, queueing any notifications
    /// that arrive in the meantime so the main loop still sees them.
    pub fn call(&mut self, feature_idx: u8, function: u8, params: &[u8]) -> Result<[u8; 16]> {
        let mut out = [0u8; 20];
        out[0] = REPORT_LONG;
        out[1] = self.dev_index;
        out[2] = feature_idx;
        out[3] = (function << 4) | SW_ID;
        out[4..4 + params.len()].copy_from_slice(params);
        self.dev.write(&out)?;

        // Give the device a few reads to answer; wireless can be laggy.
        for _ in 0..40 {
            let mut buf = [0u8; 20];
            let n = self.dev.read_timeout(&mut buf, 50)?;
            if n == 0 {
                continue;
            }

            // Error page: report 0x10 with feature index 0xFF.
            if buf[0] == REPORT_SHORT && buf[2] == 0xFF && buf[3] == out[3] {
                return Err(Error::Device(buf[4]));
            }

            let is_reply =
                buf[1] == self.dev_index && buf[2] == feature_idx && (buf[3] & 0x0F) == SW_ID;

            if is_reply {
                let mut r = [0u8; 16];
                let take = n.saturating_sub(4).min(16);
                r[..take].copy_from_slice(&buf[4..4 + take]);
                return Ok(r);
            }

            // Not ours - a notification. Keep it for the main loop.
            if (buf[3] & 0x0F) == 0 {
                self.pending.push_back(buf);
            }
        }
        Err(Error::Timeout)
    }

    /// Non-blocking-ish read of the next notification.
    pub fn next_event(&mut self, timeout_ms: i32) -> Result<Option<[u8; 20]>> {
        if let Some(e) = self.pending.pop_front() {
            return Ok(Some(e));
        }
        let mut buf = [0u8; 20];
        let n = self.dev.read_timeout(&mut buf, timeout_ms)?;
        if n == 0 {
            return Ok(None);
        }
        if (buf[3] & 0x0F) == 0 {
            return Ok(Some(buf));
        }
        Ok(None)
    }
}
