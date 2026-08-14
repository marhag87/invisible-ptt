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
use std::sync::mpsc;

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;

/// Handle to the thread that owns the Discord connection.
///
/// Every Discord operation - opening the pipe, the handshake, AUTHENTICATE,
/// SET_VOICE_SETTINGS - is a blocking round-trip over a pipe with no read
/// timeout, so none of it can happen on the input loop. A Discord that accepts
/// the connection and then stalls (mid-launch, or rate-limited) would otherwise
/// wedge the loop: no button events, and Ctrl-C unable to restore the mouse,
/// because the handler only flips an atomic that nobody is left to read.
///
/// The input loop hands over a message and moves on. Sends are queued, so the
/// press and release of one hold stay in order.
pub struct RpcHandle {
    /// Dropped by shutdown() to end the worker loop.
    tx: Option<mpsc::Sender<Msg>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

enum Msg {
    SetMute(bool),
    /// New credentials after a token refresh; reconnects on the next press.
    Reconfigure(crate::DiscordCfg),
}

impl RpcHandle {
    pub fn spawn(cfg: &crate::DiscordCfg) -> Self {
        let (tx, rx) = mpsc::channel::<Msg>();
        let mut rpc = Rpc::new(cfg);
        let worker = std::thread::spawn(move || {
            // Ends when the sender is dropped.
            for msg in rx {
                match msg {
                    Msg::SetMute(mute) => rpc.set_mute(mute),
                    Msg::Reconfigure(cfg) => rpc = Rpc::new(&cfg),
                }
            }
        });
        RpcHandle {
            tx: Some(tx),
            worker: Some(worker),
        }
    }

    pub fn set_mute(&self, mute: bool) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Msg::SetMute(mute));
        }
    }

    pub fn reconfigure(&self, cfg: &crate::DiscordCfg) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Msg::Reconfigure(cfg.clone()));
        }
    }

    /// Wait for queued messages to drain, so a final un-mute on the way out is
    /// actually delivered rather than dying with the process.
    ///
    /// Call this *after* the mouse is restored: it is the one place we
    /// deliberately block on Discord, and if Discord is wedged the mouse must
    /// already be back in a usable state before we risk waiting on it.
    pub fn shutdown(&mut self) {
        self.tx = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// The connection itself. Lives on the worker thread; reach it through
/// [`RpcHandle`], never directly from the input loop.
pub struct Rpc {
    cfg_client_id: String,
    cfg_token: String,
    conn: Option<Conn>,
    /// Permanently off: RPC was never configured (no client_id). This is the
    /// only give-up that sticks - a missing Discord *process* is transient and
    /// must keep retrying (it may launch after us), so it does not set this.
    gave_up: bool,
    /// Edge-trigger for the "Discord isn't running" log, so repeated presses
    /// while it is down don't spam. Cleared once a connection succeeds.
    warned_unavailable: bool,
    nonce: u64,
}

struct Conn {
    #[cfg(windows)]
    pipe: std::fs::File,
    #[cfg(not(windows))]
    pipe: std::os::unix::net::UnixStream,
}

impl Rpc {
    fn new(cfg: &crate::DiscordCfg) -> Self {
        Rpc {
            cfg_client_id: cfg.client_id.clone(),
            cfg_token: cfg.access_token.clone(),
            conn: None,
            gave_up: cfg.client_id.is_empty(),
            warned_unavailable: false,
            nonce: 0,
        }
    }

    /// Discord has no "hold to talk" RPC command, so PTT is expressed as
    /// self-mute toggling. Set Discord's input mode to Voice Activity with
    /// sensitivity at minimum, and this gives true push-to-talk semantics.
    fn set_mute(&mut self, mute: bool) {
        if self.gave_up {
            return;
        }
        if self.conn.is_none() {
            if self.connect().is_err() {
                // Discord probably just isn't running yet - it may launch after
                // us, especially at logon. Keep retrying on later presses, but
                // log only on the transition so a burst of presses stays quiet.
                if !self.warned_unavailable {
                    logerr!("discord rpc unavailable (is Discord running?); will retry");
                    self.warned_unavailable = true;
                }
                return;
            }
            // Connected: re-arm the warning for a future disconnect.
            self.warned_unavailable = false;
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
                        logerr!("discord authenticate failed: {msg}");
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

/// Discord's OAuth2 token endpoint, split the way WinHTTP wants it.
const TOKEN_HOST: &str = "discord.com";
const TOKEN_PATH: &str = "/api/oauth2/token";

/// Trade the refresh token for a fresh access token, returning
/// `(access_token, refresh_token)` on success.
///
/// Any failure returns None with a log line and leaves the caller's tokens
/// as-is, so a transient network problem never clobbers a still-valid token.
pub fn refresh(cfg: &crate::DiscordCfg) -> Option<(String, String)> {
    if cfg.client_id.is_empty() || cfg.client_secret.is_empty() || cfg.refresh_token.is_empty() {
        return None;
    }

    // Token/id/secret are base64url / alphanumeric, so they need no form
    // encoding. urlencoding is intentionally avoided to keep deps minimal.
    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}&client_secret={}",
        cfg.refresh_token, cfg.client_id, cfg.client_secret
    );

    match http::post_form(TOKEN_HOST, TOKEN_PATH, body.as_bytes()) {
        Ok(response) => parse_refresh_response(&response),
        Err(e) => {
            logerr!("discord token refresh: {e}");
            None
        }
    }
}

/// One HTTPS POST, which is the entire network surface of this program.
///
/// Windows does it through WinHTTP, from a crate already in the tree: the OS
/// supplies TLS and the root store, and nothing is spawned - which matters
/// because the daemon is a GUI-subsystem process, so any console child it
/// starts gets a window flashed on screen. Deliberately not an HTTP client
/// crate; `reqwest` alone would be an async runtime and a hundred-odd
/// dependencies for one request every six days.
///
/// The response body is returned whatever the status code, because that is
/// where Discord puts its `{"error", "error_description"}` explanation.
#[cfg(windows)]
mod http {
    use windows::core::PCWSTR;
    use windows::Win32::Networking::WinHttp::*;

    /// Closes its handle on the way out of any path, including the early
    /// returns below - there are five of them and WinHTTP handles are not
    /// something to unwind by hand.
    struct Handle(*mut core::ffi::c_void);

    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let _ = WinHttpCloseHandle(self.0);
                }
            }
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    pub fn post_form(host: &str, path: &str, body: &[u8]) -> Result<Vec<u8>, String> {
        let host_w = wide(host);
        let path_w = wide(path);
        unsafe {
            let session = Handle(WinHttpOpen(
                windows::core::w!("invisible-ptt"),
                // Honour whatever proxy Windows is configured with, rather
                // than assuming a direct connection.
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                None,
                None,
                0,
            ));
            if session.0.is_null() {
                return Err(format!("WinHttpOpen failed: {}", last_error()));
            }
            // The old curl call had --max-time 15 so a stalled network could
            // not hang startup. These are the same budget, per phase.
            let _ = WinHttpSetTimeouts(session.0, 5_000, 5_000, 5_000, 15_000);

            let connect = Handle(WinHttpConnect(
                session.0,
                PCWSTR(host_w.as_ptr()),
                INTERNET_DEFAULT_HTTPS_PORT,
                0,
            ));
            if connect.0.is_null() {
                return Err(format!("WinHttpConnect failed: {}", last_error()));
            }

            let request = Handle(WinHttpOpenRequest(
                connect.0,
                windows::core::w!("POST"),
                PCWSTR(path_w.as_ptr()),
                None,
                PCWSTR::null(),
                std::ptr::null(),
                // WINHTTP_FLAG_SECURE is what makes this HTTPS. Without it the
                // client secret would go out in the clear.
                WINHTTP_FLAG_SECURE,
            ));
            if request.0.is_null() {
                return Err(format!("WinHttpOpenRequest failed: {}", last_error()));
            }

            let headers = wide("Content-Type: application/x-www-form-urlencoded");
            // Trailing NUL excluded: WinHTTP takes the length in characters.
            let headers = &headers[..headers.len() - 1];
            WinHttpSendRequest(
                request.0,
                Some(headers),
                Some(body.as_ptr() as *const core::ffi::c_void),
                body.len() as u32,
                body.len() as u32,
                0,
            )
            .map_err(|e| format!("WinHttpSendRequest failed: {e}"))?;

            WinHttpReceiveResponse(request.0, std::ptr::null_mut())
                .map_err(|e| format!("WinHttpReceiveResponse failed: {e}"))?;

            let mut response = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let mut read = 0u32;
                WinHttpReadData(
                    request.0,
                    chunk.as_mut_ptr() as *mut core::ffi::c_void,
                    chunk.len() as u32,
                    &mut read,
                )
                .map_err(|e| format!("WinHttpReadData failed: {e}"))?;
                if read == 0 {
                    break;
                }
                response.extend_from_slice(&chunk[..read as usize]);
                // A token response is a few hundred bytes; anything larger is
                // not something we should keep reading into memory.
                if response.len() > 1 << 20 {
                    break;
                }
            }
            Ok(response)
        }
    }

    fn last_error() -> String {
        windows::core::Error::from_win32().to_string()
    }
}

/// The Linux smoke-test build keeps shelling out to curl: WinHTTP does not
/// exist there, and this half of the program is only ever exercised by hand.
#[cfg(not(windows))]
mod http {
    use std::io::Write;
    use std::process::{Command, Stdio};

    pub fn post_form(host: &str, path: &str, body: &[u8]) -> Result<Vec<u8>, String> {
        // Over stdin, never argv, so the client secret stays out of the
        // process listing.
        let mut child = Command::new("curl")
            .args(["-sS", "--max-time", "15", "-d", "@-"])
            .arg(format!("https://{host}{path}"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not run curl: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(body);
            // stdin drops here, closing the pipe so curl sends the request.
        }
        let out = child
            .wait_with_output()
            .map_err(|e| format!("curl failed: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "curl exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(out.stdout)
    }
}

/// Pull `(access_token, refresh_token)` out of Discord's token-endpoint JSON.
/// Returns None (logging why) on malformed JSON or an error response - Discord
/// signals failure with `{"error", "error_description"}` rather than tokens.
fn parse_refresh_response(bytes: &[u8]) -> Option<(String, String)> {
    let json: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(e) => {
            logerr!("discord token refresh: could not parse response: {e}");
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
            logerr!(
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
    config_path: &std::path::Path,
    access_token: &str,
    refresh_token: &str,
) -> std::io::Result<()> {
    let text = std::fs::read_to_string(config_path)?;
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    doc["discord"]["access_token"] = toml_edit::value(access_token);
    doc["discord"]["refresh_token"] = toml_edit::value(refresh_token);

    let mut tmp = config_path.to_path_buf().into_os_string();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    std::fs::write(&tmp, doc.to_string())?;
    std::fs::rename(&tmp, config_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one test that actually goes out to the internet, so it does not run
    /// by default: `cargo test -- --ignored`. Worth running after touching
    /// anything in `http`, because a broken refresh is invisible until a token
    /// expires a week later and push-to-talk quietly stops working.
    #[test]
    #[ignore = "makes a real request to Discord's token endpoint"]
    fn post_form_reaches_discords_token_endpoint() {
        let body = b"grant_type=refresh_token&refresh_token=x&client_id=x&client_secret=x";
        let response = http::post_form(TOKEN_HOST, TOKEN_PATH, body).expect("the request itself");
        // Discord rejects the junk credentials, and that rejection is the
        // proof: the request was formed, sent over TLS, and answered with a
        // body we could read. Which *shape* of refusal it picks is Discord's
        // business - it has more than one - so assert only that we got JSON
        // back and that no tokens came out of it.
        let json: serde_json::Value = serde_json::from_slice(&response)
            .unwrap_or_else(|e| panic!("not JSON: {e}: {}", String::from_utf8_lossy(&response)));
        assert!(json.is_object(), "unexpected response: {json}");
        assert_eq!(parse_refresh_response(&response), None);
    }

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

        persist_tokens(&path, "new_access", "new_refresh").unwrap();

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
        std::fs::write(&path, "[discord]\naccess_token = \"old\"\n").unwrap();

        persist_tokens(&path, "a", "r").unwrap();

        let result = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert!(result.contains("access_token = \"a\""));
        assert!(result.contains("refresh_token = \"r\""));
    }
}
