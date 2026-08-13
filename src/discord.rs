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
            // Discord replies with evt "ERROR" and a message when the token is
            // bad or expired. Surface it instead of sailing on silently - a
            // lapsed token would otherwise just make PTT stop with no clue why.
            if let Ok(reply) = read_frame(&mut conn) {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&reply) {
                    if v.get("evt").and_then(|e| e.as_str()) == Some("ERROR") {
                        let msg = v
                            .get("data")
                            .and_then(|d| d.get("message"))
                            .and_then(|m| m.as_str())
                            .unwrap_or("unknown error");
                        eprintln!("discord authenticate failed: {msg}");
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "discord rejected the access token (it may be expired)",
                        ));
                    }
                }
            }
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

/// Trade the refresh token for a fresh access token via Discord's OAuth2
/// endpoint, returning `(access_token, refresh_token)` on success.
///
/// This shells out to `curl` (bundled with Windows 10 1803+ and 11) rather
/// than pulling an HTTPS/TLS stack into the binary. The credentials go in over
/// stdin, never as argv, so they do not show up in a process listing. Any
/// failure returns None with a log line and leaves the caller's tokens as-is,
/// so a transient network problem never clobbers a still-valid token.
pub fn refresh(cfg: &crate::DiscordCfg) -> Option<(String, String)> {
    use std::process::{Command, Stdio};

    if cfg.client_id.is_empty() || cfg.client_secret.is_empty() || cfg.refresh_token.is_empty() {
        return None;
    }

    // Token/id/secret are base64url / alphanumeric, so they need no form
    // encoding. urlencoding is intentionally avoided to keep deps minimal.
    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}&client_secret={}",
        cfg.refresh_token, cfg.client_id, cfg.client_secret
    );

    let mut child = match Command::new("curl")
        // --max-time so a network stall can't hang daemon startup.
        .args([
            "-sS",
            "--max-time",
            "15",
            "-d",
            "@-",
            "https://discord.com/api/oauth2/token",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("discord token refresh: could not run curl: {e}");
            return None;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(body.as_bytes());
        // stdin drops here, closing the pipe so curl sends the request.
    }
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("discord token refresh: curl failed: {e}");
            return None;
        }
    };
    if !out.status.success() {
        eprintln!(
            "discord token refresh: curl exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return None;
    }

    parse_refresh_response(&out.stdout)
}

/// Pull `(access_token, refresh_token)` out of Discord's token-endpoint JSON.
/// Returns None (logging why) on malformed JSON or an error response - Discord
/// signals failure with `{"error", "error_description"}` rather than tokens.
fn parse_refresh_response(bytes: &[u8]) -> Option<(String, String)> {
    let json: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("discord token refresh: could not parse response: {e}");
            return None;
        }
    };
    match (
        json.get("access_token").and_then(|v| v.as_str()),
        json.get("refresh_token").and_then(|v| v.as_str()),
    ) {
        (Some(access), Some(refresh)) => Some((access.to_string(), refresh.to_string())),
        _ => {
            // Discord reports failures as {"error", "error_description"}.
            eprintln!(
                "discord token refresh rejected: {}",
                json.get("error_description")
                    .or_else(|| json.get("error"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("no tokens in response")
            );
            None
        }
    }
}

/// Write the rotated tokens back into config.toml in place, preserving all
/// comments and layout (toml_edit, not a full re-serialize). Uses a temp file
/// plus rename so a crash mid-write cannot corrupt the config or lose the
/// single-use refresh token.
pub fn persist_tokens(
    config_path: &str,
    access_token: &str,
    refresh_token: &str,
) -> std::io::Result<()> {
    let text = std::fs::read_to_string(config_path)?;
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    doc["discord"]["access_token"] = toml_edit::value(access_token);
    doc["discord"]["refresh_token"] = toml_edit::value(refresh_token);

    let tmp = format!("{config_path}.tmp");
    std::fs::write(&tmp, doc.to_string())?;
    std::fs::rename(&tmp, config_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_refresh_response_extracts_both_tokens() {
        let body = br#"{"access_token":"AAA","token_type":"Bearer","expires_in":604800,"refresh_token":"RRR","scope":"rpc rpc.voice.write"}"#;
        assert_eq!(
            parse_refresh_response(body),
            Some(("AAA".to_string(), "RRR".to_string()))
        );
    }

    #[test]
    fn parse_refresh_response_rejects_error_body() {
        // Discord's failure shape - must not be mistaken for success.
        let body = br#"{"error":"invalid_grant","error_description":"Invalid refresh token"}"#;
        assert_eq!(parse_refresh_response(body), None);
    }

    #[test]
    fn parse_refresh_response_rejects_partial_and_garbage() {
        assert_eq!(parse_refresh_response(br#"{"access_token":"AAA"}"#), None);
        assert_eq!(parse_refresh_response(b"not json at all"), None);
    }

    fn temp_config_path(tag: u32) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "invisible-ptt-persist-{}-{tag}.toml",
            std::process::id()
        ))
    }

    #[test]
    fn persist_tokens_rewrites_values_and_keeps_comments() {
        let path = temp_config_path(line!());
        let path_str = path.to_str().unwrap();
        let original = "\
# leading comment
button = 3
mapping = [1, 2, 3, 0, 5]

[discord]
# keep me
client_id = \"cid\"
access_token = \"old_access\"
refresh_token = \"old_refresh\"
";
        std::fs::write(&path, original).unwrap();

        persist_tokens(path_str, "new_access", "new_refresh").unwrap();

        let result = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();

        // Values updated...
        assert!(result.contains("access_token = \"new_access\""));
        assert!(result.contains("refresh_token = \"new_refresh\""));
        assert!(!result.contains("old_access"));
        assert!(!result.contains("old_refresh"));
        // ...comments and untouched keys preserved.
        assert!(result.contains("# leading comment"));
        assert!(result.contains("# keep me"));
        assert!(result.contains("client_id = \"cid\""));
    }

    #[test]
    fn persist_tokens_inserts_missing_refresh_token_key() {
        // First-run config that has no refresh_token line yet.
        let path = temp_config_path(line!());
        let path_str = path.to_str().unwrap();
        std::fs::write(&path, "[discord]\naccess_token = \"old\"\n").unwrap();

        persist_tokens(path_str, "a", "r").unwrap();

        let result = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert!(result.contains("access_token = \"a\""));
        assert!(result.contains("refresh_token = \"r\""));
    }
}
