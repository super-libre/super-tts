// SPDX-License-Identifier: GPL-3.0-only
//! Mock subprocess backend for `tests/subprocess_mock.rs`. Serves the `/v1`
//! contract over `SUPER_TTS_BACKEND_SOCKET` with canned responses — loads no
//! model and needs no GPU or network. Built only with the `test-fixtures`
//! feature.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use serde_json::{Value, json};
use tokio::net::UnixListener;

struct AppState {
    loaded: AtomicBool,
}

#[tokio::main]
async fn main() {
    let socket = std::env::var("SUPER_TTS_BACKEND_SOCKET").expect("SUPER_TTS_BACKEND_SOCKET");
    let state = Arc::new(AppState {
        loaded: AtomicBool::new(false),
    });
    let app = Router::new()
        .route(
            "/v1/ping",
            get(|| async { Json(json!({ "status": "success", "message": "pong" })) }),
        )
        .route("/v1/status", get(status))
        .route("/v1/load", post(load))
        .route("/v1/synthesize", post(synthesize))
        .route(
            "/v1/cancel",
            post(|| async { Json(json!({ "status": "success", "message": "Cancelled" })) }),
        )
        .layer(DefaultBodyLimit::disable())
        .with_state(state);

    if let Some(parent) = std::path::Path::new(&socket).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).expect("bind socket");

    loop {
        let (stream, _) = listener.accept().await.expect("accept");
        let app = app.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let svc = TowerToHyperService::new(app);
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, svc)
                .await;
        });
    }
}

async fn status(State(s): State<Arc<AppState>>) -> Json<Value> {
    if s.loaded.load(Ordering::SeqCst) {
        Json(json!({
            "status": "success",
            "state": "ready",
            "device": "cpu",
            "model": { "name": "mock" }
        }))
    } else {
        Json(json!({ "status": "success", "state": "starting" }))
    }
}

async fn load(State(s): State<Arc<AppState>>, _body: String) -> impl IntoResponse {
    s.loaded.store(true, Ordering::SeqCst);
    (
        StatusCode::ACCEPTED,
        Json(json!({ "status": "success", "message": "Loading started" })),
    )
}

/// Sample rate the mock declares on `/v1/synthesize`.
pub const MOCK_SAMPLE_RATE: u32 = 24000;
/// Samples the mock synthesizes, split across [`MOCK_AUDIO_FRAMES`] frames so
/// the test exercises reassembly rather than a single-frame happy path.
pub const MOCK_SAMPLE_COUNT: usize = 960;
/// How many audio frames the ramp is split across.
pub const MOCK_AUDIO_FRAMES: usize = 4;

/// The known ramp the mock emits: sample `i` is `i` as an `i16`. A ramp rather
/// than silence, so a decoder that drops or misaligns bytes yields a visibly
/// wrong sequence instead of a plausible one.
#[must_use]
pub fn mock_samples() -> Vec<i16> {
    (0..MOCK_SAMPLE_COUNT)
        .map(|i| i16::try_from(i).unwrap_or(i16::MAX))
        .collect()
}

/// `POST /v1/synthesize` — the framed binary body: several `audio` frames
/// carrying the ramp as s16le, one `mark`, then `done`.
///
/// Frames are built by hand rather than with the shared codec: this fixture
/// stands in for a third-party backend, so encoding it independently means the
/// test checks the wire format instead of round-tripping one implementation
/// against itself.
const KIND_AUDIO: u8 = 0x01;
const KIND_MARK: u8 = 0x02;
const KIND_DONE: u8 = 0x03;

/// `[u8 kind][u32 len little-endian][payload]`.
fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(kind);
    out.extend_from_slice(
        &u32::try_from(payload.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    out.extend_from_slice(payload);
    out
}

async fn synthesize(State(s): State<Arc<AppState>>, _body: String) -> axum::response::Response {
    use axum::http::header::CONTENT_TYPE;

    if !s.loaded.load(Ordering::SeqCst) {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "status": "error", "message": "not_ready" })),
        )
            .into_response();
    }

    let pcm: Vec<u8> = mock_samples()
        .iter()
        .flat_map(|s| s.to_le_bytes())
        .collect();
    let mut body = Vec::new();
    for chunk in pcm.chunks(pcm.len() / MOCK_AUDIO_FRAMES) {
        body.extend(frame(KIND_AUDIO, chunk));
    }
    body.extend(frame(
        KIND_MARK,
        br#"{"start_ms":0,"end_ms":40,"start_char":0,"end_char":5}"#,
    ));
    body.extend(frame(KIND_DONE, b"{}"));

    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, "application/vnd.super-tts.frames"),
            (
                axum::http::HeaderName::from_static("x-tts-sample-rate"),
                "24000",
            ),
            (axum::http::HeaderName::from_static("x-tts-channels"), "1"),
            (axum::http::HeaderName::from_static("x-tts-format"), "s16le"),
        ],
        body,
    )
        .into_response()
}
