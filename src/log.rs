//! Tee logging: every line goes to a file as well as to stdout/stderr.
//!
//! The tray build is a Windows GUI subsystem binary, so it has no console and
//! nothing the daemon prints is visible anywhere. The log file is what the
//! tray's "Open log file" opens, and it is now the only account of what the
//! mouse did overnight. Lines still go to stdout/stderr too: that costs
//! nothing when no console is attached, and keeps `cargo run` readable on the
//! Linux side.
//!
//! Use through the `log!` / `logerr!` macros, which mirror `println!` /
//! `eprintln!`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Rotate at a megabyte, keeping one older generation. Steady state writes
/// nothing for hours on end, so the only way to fill this is a mouse that keeps
/// dropping - exactly the case where the recent lines are the interesting ones.
const MAX_BYTES: u64 = 1 << 20;

static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

struct Sink {
    path: PathBuf,
    /// None if the log could not be opened at all - a read-only directory, say.
    /// Logging then degrades to stdout rather than failing the daemon.
    file: Option<File>,
    written: u64,
}

/// Start logging to `path`. Appends: a restart from the tray menu should not
/// throw away the lines that explain why it was restarted.
pub fn init(path: &Path) {
    // On a first run the config directory does not exist yet, and opening a
    // file inside a missing directory just fails - which would silently
    // disable logging for the whole session, starting with the line that says
    // where the config was created.
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let written = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let file = OpenOptions::new().create(true).append(true).open(path).ok();
    let sink = Sink {
        path: path.to_path_buf(),
        file,
        written,
    };
    let _ = SINK.set(Mutex::new(sink));
}

/// One line. Called through `log!` / `logerr!`, never directly.
pub fn line(is_err: bool, args: std::fmt::Arguments) {
    let text = args.to_string();
    if is_err {
        eprintln!("{text}");
    } else {
        println!("{text}");
    }
    // Before init() - or if the mutex is poisoned - the line has already gone
    // to the console, which is all we can do about it.
    if let Some(sink) = SINK.get() {
        if let Ok(mut sink) = sink.lock() {
            sink.write(&text);
        }
    }
}

impl Sink {
    fn write(&mut self, text: &str) {
        if self.written >= MAX_BYTES {
            self.rotate();
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let line = format!("{} {text}\n", crate::platform::timestamp());
        if file.write_all(line.as_bytes()).is_ok() {
            self.written += line.len() as u64;
        }
    }

    fn rotate(&mut self) {
        // Close first: Windows will not rename a file this process has open.
        self.file = None;
        let mut old = self.path.clone().into_os_string();
        old.push(".old");
        let _ = std::fs::rename(&self.path, old);
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .ok();
        self.written = 0;
    }
}

/// Like `println!`, and also into the log file.
macro_rules! log {
    ($($arg:tt)*) => { $crate::log::line(false, format_args!($($arg)*)) };
}

/// Like `eprintln!`, and also into the log file.
macro_rules! logerr {
    ($($arg:tt)*) => { $crate::log::line(true, format_args!($($arg)*)) };
}
