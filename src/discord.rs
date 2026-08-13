//! Discord local RPC over the IPC pipe.
//!
//! This is process-to-process on your own machine - `\\.\pipe\discord-ipc-N`
//! on Windows, a unix socket under $XDG_RUNTIME_DIR elsewhere. No network hop
//! per press; the internet is only involved once, during OAuth setup.
//!
//! Frame format: [opcode u32 LE][length u32 LE][utf-8 json]
//!
//! NOTE: getting an access_token requires registering an application at
//! https://discord.com/developers/applications and running the OAuth
//! authorize/exchange dance once. See README. As the owner of your own
//! application you are permitted to authorize it without Discord whitelisting
//! it - that restriction applies to distributing it to other users.

use serde_json::json;
use std::io::{Read, Write};

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;

pub struct Rpc {
    cfg_client_id: String,
    cfg_token: String,
    conn: Option<Conn>,
    /// Stop screaming into the log once we know it is not going to work.
    gave_up: bool,
    nonce: u64,
}

struct Conn {
    #[cfg(windows)]
    pipe: std::fs::File,
    #[cfg(not(windows))]
    pipe: std::os::unix::net::UnixStream,
}

impl Rpc {
    pub fn new(cfg: &crate::DiscordCfg) -> Self {
        Rpc {
            cfg_client_id: cfg.client_id.clone(),
            cfg_token: cfg.access_token.clone(),
            conn: None,
            gave_up: cfg.client_id.is_empty(),
            nonce: 0,
        }
    }

    /// Discord has no "hold to talk" RPC command, so PTT is expressed as
    /// self-mute toggling. Set Discord's input mode to Voice Activity with
    /// sensitivity at minimum, and this gives true push-to-talk semantics.
    pub fn set_mute(&mut self, mute: bool) {
        if self.gave_up {
            return;
        }
        if self.conn.is_none() && self.connect().is_err() {
            eprintln!("discord rpc unavailable; falling back to doing nothing");
            self.gave_up = true;
            return;
        }
        self.nonce += 1;
        let payload = json!({
            "cmd": "SET_VOICE_SETTINGS",
            "args": { "mute": mute },
            "nonce": self.nonce.to_string(),
        });
        if self.send(OP_FRAME, &payload).is_err() {
            // Discord restarted, most likely. Drop it and retry next press.
            self.conn = None;
        }
    }

    fn connect(&mut self) -> std::io::Result<()> {
        let mut conn = open_pipe()?;
        // Handshake, then authenticate with the stored token.
        write_frame(
            &mut conn,
            OP_HANDSHAKE,
            &json!({ "v": 1, "client_id": self.cfg_client_id }),
        )?;
        let _ = read_frame(&mut conn);

        if !self.cfg_token.is_empty() {
            self.nonce += 1;
            write_frame(
                &mut conn,
                OP_FRAME,
                &json!({
                    "cmd": "AUTHENTICATE",
                    "args": { "access_token": self.cfg_token },
                    "nonce": self.nonce.to_string(),
                }),
            )?;
            let _ = read_frame(&mut conn);
        }

        self.conn = Some(conn);
        Ok(())
    }

    fn send(&mut self, op: u32, value: &serde_json::Value) -> std::io::Result<()> {
        let conn = self.conn.as_mut().unwrap();
        write_frame(conn, op, value)
    }
}

fn write_frame(conn: &mut Conn, op: u32, value: &serde_json::Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(value)?;
    let mut frame = Vec::with_capacity(8 + body.len());
    frame.extend_from_slice(&op.to_le_bytes());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    conn.pipe.write_all(&frame)?;
    conn.pipe.flush()
}

fn read_frame(conn: &mut Conn) -> std::io::Result<Vec<u8>> {
    let mut header = [0u8; 8];
    conn.pipe.read_exact(&mut header)?;
    let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    let mut body = vec![0u8; len.min(1 << 20)];
    conn.pipe.read_exact(&mut body)?;
    Ok(body)
}

#[cfg(windows)]
fn open_pipe() -> std::io::Result<Conn> {
    use std::fs::OpenOptions;
    let mut last = std::io::Error::new(std::io::ErrorKind::NotFound, "no discord ipc pipe");
    for n in 0..10 {
        match OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!(r"\\.\pipe\discord-ipc-{n}"))
        {
            Ok(pipe) => return Ok(Conn { pipe }),
            Err(e) => last = e,
        }
    }
    Err(last)
}

#[cfg(not(windows))]
fn open_pipe() -> std::io::Result<Conn> {
    use std::os::unix::net::UnixStream;
    let base = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("TMPDIR"))
        .unwrap_or_else(|_| "/tmp".into());
    let mut last = std::io::Error::new(std::io::ErrorKind::NotFound, "no discord ipc socket");
    for n in 0..10 {
        match UnixStream::connect(format!("{base}/discord-ipc-{n}")) {
            Ok(pipe) => return Ok(Conn { pipe }),
            Err(e) => last = e,
        }
    }
    Err(last)
}
