// SPDX-License-Identifier: GPL-3.0-only
//! `GET /v1/speak/stream` really upgrades, and the upgraded connection is
//! usable.
//!
//! Every other smoke test sends one request and reads one response, which is a
//! shape the daemon can serve while its WebSocket support is entirely broken:
//! hyper writes `101 Switching Protocols` from the router's response like any
//! other, and only afterwards decides whether to hand the IO to
//! `hyper::upgrade::on` or drop it. A test that stopped at the `101` would have
//! passed throughout the period when no frame could cross.
//!
//! So this one goes past the handshake and asserts a frame comes back.
//!
//! Uses `SUPER_TTS_AUTO_APPROVE=1` (no GUI) + `SUPER_TTS_KEYRING_MOCK=1`
//! (in-memory keyring), so it runs in the default `cargo test` flow.

use futures_util::{SinkExt, StreamExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use super_tts_shared::daemon::http_client;
use tokio::net::UnixStream;
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const DAEMON_BIN: &str = env!("CARGO_BIN_EXE_super-tts-daemon");

struct DaemonGuard {
    child: Child,
    cleanup_paths: Vec<PathBuf>,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for p in &self.cleanup_paths {
            let _ = std::fs::remove_file(p);
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

async fn start_daemon() -> (DaemonGuard, PathBuf) {
    let unique = format!("tts-speak-stream-{}", std::process::id());
    let tmp = std::env::temp_dir();
    let http_socket = tmp.join(format!("{unique}-http.sock"));
    let config_home = tmp.join(format!("{unique}-config"));
    std::fs::create_dir_all(&config_home).expect("create test config dir");
    let data_home = tmp.join(format!("{unique}-data"));
    std::fs::create_dir_all(&data_home).expect("create test data dir");

    let child = Command::new(DAEMON_BIN)
        .env("SUPER_TTS_KEYRING_MOCK", "1")
        .env("SUPER_TTS_AUTO_APPROVE", "1")
        .env("SUPER_TTS_MUTE_CUES", "1")
        .env("SUPER_TTS_HTTP_SOCKET", &http_socket)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_DATA_HOME", &data_home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn super-tts-daemon");

    // Hand the child to the guard before the readiness loop: the timeout
    // panic below must still kill and reap the daemon, not leak it.
    let guard = DaemonGuard {
        child,
        cleanup_paths: vec![http_socket.clone(), config_home, data_home],
    };

    let deadline = Instant::now() + Duration::from_mins(2);
    while Instant::now() < deadline {
        if Path::new(&http_socket).exists()
            && http_client::auth_request(http_socket.clone(), "speak-stream-smoke", &["status"])
                .await
                .is_ok()
        {
            return (guard, http_socket);
        }
        sleep(Duration::from_millis(200)).await;
    }
    panic!("daemon HTTP listener not ready within 120s");
}

/// A frame crosses the upgraded connection in each direction.
///
/// No model is loaded in a fresh test daemon, so a well-formed `start` is
/// answered with `error: model_not_loaded` — which is the point. The assertion
/// is not *which* frame comes back but that one does at all: reaching the
/// handler means the `101` was followed by a live socket rather than by a
/// hang-up.
///
/// Without `.with_upgrades()` on the server's connection builder, hyper writes
/// the `101` and then drops the IO instead of handing it to
/// `hyper::upgrade::on`. The handshake still completes — `tungstenite` sees a
/// valid `101` — and the first read then fails, which is exactly how this
/// shipped broken without any test noticing.
#[tokio::test]
async fn speak_stream_upgrades_to_a_usable_websocket() {
    let (_guard, socket) = start_daemon().await;

    let auth = http_client::auth_request(socket.clone(), "speak-stream-smoke", &["speak"])
        .await
        .expect("mint a speak-scoped token");

    let stream = UnixStream::connect(&socket)
        .await
        .expect("connect to the daemon socket");

    let mut request = "ws://tts.local/v1/speak/stream"
        .into_client_request()
        .expect("build the upgrade request");
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", auth.session_token)
            .parse()
            .expect("a bearer header"),
    );

    let (mut ws, response) = tokio_tungstenite::client_async(request, stream)
        .await
        .expect("the handshake completes");
    assert_eq!(
        response.status(),
        101,
        "the daemon must answer the upgrade with 101"
    );

    ws.send(Message::Text(r#"{"type":"start"}"#.into()))
        .await
        .expect("send the start frame");

    // A generous bound: this is a liveness assertion, not a latency one. On the
    // broken path the read fails immediately rather than timing out, so the
    // budget costs nothing when the test is going to fail.
    let frame = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("the daemon answers within 10s rather than leaving the socket silent")
        .expect("the stream is still open")
        .expect("the frame is readable");

    let text = frame
        .into_text()
        .expect("the daemon answers with a text frame");
    let parsed: serde_json::Value =
        serde_json::from_str(&text).expect("the answer is a JSON frame");
    assert_eq!(
        parsed["type"], "error",
        "a fresh daemon has no model loaded, so `start` is refused: {text}"
    );
    assert_eq!(
        parsed["message"], "model_not_loaded",
        "the refusal names the reason: {text}"
    );
}
