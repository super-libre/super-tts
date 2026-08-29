// SPDX-License-Identifier: GPL-3.0-only
//! `GET /v1/speak/stream` — speak text as it is produced.
//!
//! The LLM case: a client sends deltas as tokens arrive, and sentences are
//! synthesized as they complete, so speech starts long before generation
//! finishes. [`crate::text`] does the hard part; this module is transport.
//!
//! A WebSocket rather than a series of POSTs specifically for the back-channel.
//! When the user interrupts a half-generated reply the daemon has to cancel
//! in-flight synthesis, drop the queue, flush the ring **and** tell the client
//! to stop generating. A one-way append API makes that last part awkward.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use log::{debug, warn};
use serde::{Deserialize, Serialize};

use crate::daemon::http::state::AppState;
use crate::daemon::speech::{SpeakError, SpeakOptions, SpeakSession, SpeechEvent};
use crate::daemon::types::SuperTTSDaemon;

/// Abort a session that has sent no frame for this long.
///
/// An idle stream holds the utterance slot and the output device, so a client
/// that goes away without closing (a half-open TCP connection, a crashed
/// process) must not keep them forever.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Maximum concurrent streaming sessions.
///
/// Only one can be *speaking* — the engine is newest-wins — but each holds a
/// task and a buffer, so an authorized client should not be able to open them
/// without limit.
const MAX_SESSIONS: usize = 4;

/// Permits for [`MAX_SESSIONS`]; `try_acquire` rejects rather than queues.
static SESSIONS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(MAX_SESSIONS);

/// A frame from the client.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientFrame {
    /// First frame: how the utterance should be spoken.
    Start {
        #[serde(default)]
        voice: Option<String>,
        #[serde(default)]
        language: Option<String>,
        #[serde(default)]
        speed: Option<f32>,
        #[serde(default)]
        instructions: Option<String>,
    },
    /// More text. Repeatable.
    Text { delta: String },
    /// No more text is coming; play out what remains.
    End,
    /// Stop now and drop queued audio.
    Cancel,
}

/// A frame to the client.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerFrame {
    /// The utterance this session is producing. Sent once, after `start`.
    Utterance { id: String },
    /// The backend aligned a span of audio to a span of the text.
    Mark {
        #[serde(flatten)]
        mark: super_tts_shared::audio::frames::Mark,
    },
    /// How far playback has got, in milliseconds.
    Progress { spoken_ms: u64, queued_ms: u64 },
    /// The utterance is complete. The stream closes after it.
    Done { id: String, chunks: usize },
    /// Fatal; the stream closes after it.
    Error { message: String },
}

/// `GET /v1/speak/stream` — upgrade and stream text in, audio out.
pub(crate) async fn speak_stream_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> Response {
    // Claim the permit before upgrading, so an over-cap client gets a clean
    // `503` rather than a socket that is immediately closed.
    let Ok(permit) = SESSIONS.try_acquire() else {
        warn!("speak stream rejected: {MAX_SESSIONS} sessions already active");
        return (StatusCode::SERVICE_UNAVAILABLE, "speak_sessions_busy").into_response();
    };
    let daemon = Arc::clone(&state.daemon);
    ws.on_upgrade(move |socket| run(socket, daemon, permit))
}

/// Drive one streaming session to completion.
async fn run(
    socket: WebSocket,
    daemon: Arc<SuperTTSDaemon>,
    // Held for the session's lifetime; dropping it frees a permit.
    _permit: tokio::sync::SemaphorePermit<'static>,
) {
    let (mut sink, mut stream) = socket.split();

    // The first frame must be `start`: everything else needs the options it
    // carries, and guessing them would mean a wrong voice rather than an error.
    let session = match wait_for_start(&mut stream, &daemon).await {
        Ok(session) => session,
        Err(message) => {
            let _ = send(&mut sink, &ServerFrame::Error { message }).await;
            let _ = sink.close().await;
            return;
        }
    };
    let mut session = session;

    if send(
        &mut sink,
        &ServerFrame::Utterance {
            id: session.id().to_string(),
        },
    )
    .await
    .is_err()
    {
        return;
    }

    let outcome = pump(&mut stream, &mut sink, &mut session).await;
    match outcome {
        Outcome::Done => {
            let id = session.id().to_string();
            match session.end().await {
                // Terminal frames: the socket closes next either way, so a
                // failed send is nothing left to act on.
                Ok(utterance) => {
                    let _ = send(
                        &mut sink,
                        &ServerFrame::Done {
                            id,
                            chunks: utterance.chunks,
                        },
                    )
                    .await;
                }
                Err(e) => {
                    let _ = send(
                        &mut sink,
                        &ServerFrame::Error {
                            message: e.to_string(),
                        },
                    )
                    .await;
                }
            }
        }
        Outcome::Cancelled => {
            session.cancel().await;
            debug!("speak stream cancelled by client");
        }
        Outcome::Failed(message) => {
            session.cancel().await;
            let _ = send(&mut sink, &ServerFrame::Error { message }).await;
        }
    }
    let _ = sink.close().await;
}

/// How a session's frame loop ended.
enum Outcome {
    /// The client sent `end`, or closed cleanly.
    Done,
    /// The client sent `cancel`, or went away mid-utterance.
    Cancelled,
    /// Something failed; the message is for the client.
    Failed(String),
}

/// Read frames until the session ends.
async fn pump(
    stream: &mut futures::stream::SplitStream<WebSocket>,
    sink: &mut futures::stream::SplitSink<WebSocket, Message>,
    session: &mut SpeakSession,
) -> Outcome {
    loop {
        let next = tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await;
        let Ok(next) = next else {
            return Outcome::Failed("idle_timeout".into());
        };
        let Some(Ok(message)) = next else {
            // A disconnect mid-utterance is a cancel: the listener is gone, and
            // finishing would speak to an empty room while holding the device.
            return Outcome::Cancelled;
        };

        let text = match message {
            Message::Text(t) => t.to_string(),
            // Binary carries nothing in this protocol; ping/pong are handled by
            // axum, and a Close ends the session cleanly.
            Message::Close(_) => return Outcome::Done,
            _ => continue,
        };

        let frame: ClientFrame = match serde_json::from_str(&text) {
            Ok(f) => f,
            Err(e) => return Outcome::Failed(format!("invalid_frame: {e}")),
        };

        match frame {
            ClientFrame::Start { .. } => {
                return Outcome::Failed("start_already_sent".into());
            }
            ClientFrame::Text { delta } => {
                if let Err(e) = session.push(&delta).await {
                    return Outcome::Failed(e.to_string());
                }
            }
            ClientFrame::End => return Outcome::Done,
            ClientFrame::Cancel => return Outcome::Cancelled,
        }

        // A later utterance can supersede this one at any time; the client
        // should learn that rather than keep sending into a dead session.
        if session.is_cancelled() {
            return Outcome::Failed("superseded".into());
        }

        // Drain whatever the backend reported while that delta was synthesized.
        while let Some(SpeechEvent::Mark(mark)) = session.try_next_event() {
            if send(sink, &ServerFrame::Mark { mark }).await.is_err() {
                return Outcome::Cancelled;
            }
        }
        let (spoken_ms, queued_ms) = session.progress();
        if send(
            sink,
            &ServerFrame::Progress {
                spoken_ms,
                queued_ms,
            },
        )
        .await
        .is_err()
        {
            return Outcome::Cancelled;
        }
    }
}

/// Read the opening `start` frame and begin a session.
async fn wait_for_start(
    stream: &mut futures::stream::SplitStream<WebSocket>,
    daemon: &SuperTTSDaemon,
) -> Result<SpeakSession, String> {
    let next = tokio::time::timeout(IDLE_TIMEOUT, stream.next())
        .await
        .map_err(|_| "idle_timeout".to_string())?;
    let Some(Ok(Message::Text(text))) = next else {
        return Err("expected_start_frame".into());
    };
    let frame: ClientFrame =
        serde_json::from_str(&text).map_err(|e| format!("invalid_frame: {e}"))?;
    let ClientFrame::Start {
        voice,
        language,
        speed,
        instructions,
    } = frame
    else {
        return Err("expected_start_frame".into());
    };

    let options = SpeakOptions {
        voice: voice.as_deref(),
        language: language.as_deref(),
        speed,
        instructions: instructions.as_deref(),
    };
    daemon
        .speech
        .begin(&daemon.model, options)
        .await
        .map_err(|e| match e {
            SpeakError::NotLoaded => "model_not_loaded".to_string(),
            other => other.to_string(),
        })
}

/// Send one frame, reporting whether the socket is still usable.
async fn send(
    sink: &mut futures::stream::SplitSink<WebSocket, Message>,
    frame: &ServerFrame,
) -> Result<(), ()> {
    let Ok(json) = serde_json::to_string(frame) else {
        return Err(());
    };
    sink.send(Message::Text(json.into())).await.map_err(|_| ())
}

pub(crate) fn routes() -> axum::Router<AppState> {
    axum::Router::new().route("/speak/stream", axum::routing::get(speak_stream_handler))
}

#[cfg(test)]
mod tests {
    use super::{ClientFrame, ServerFrame};
    use super_tts_shared::audio::frames::Mark;

    /// The wire shape is the contract with every client, and a field rename in
    /// Rust would silently change it. These pin the exact JSON.
    #[test]
    fn client_frames_parse_from_their_documented_shape() {
        let start: ClientFrame = serde_json::from_str(
            r#"{"type":"start","voice":"af_bella","language":"en","speed":1.25}"#,
        )
        .expect("start parses");
        let ClientFrame::Start {
            voice,
            language,
            speed,
            instructions,
        } = start
        else {
            panic!("expected start");
        };
        assert_eq!(voice.as_deref(), Some("af_bella"));
        assert_eq!(language.as_deref(), Some("en"));
        assert_eq!(speed, Some(1.25));
        assert_eq!(instructions, None);

        // Every option is optional: a bare start is valid.
        assert!(matches!(
            serde_json::from_str::<ClientFrame>(r#"{"type":"start"}"#),
            Ok(ClientFrame::Start { .. })
        ));

        let text: ClientFrame =
            serde_json::from_str(r#"{"type":"text","delta":"Hello"}"#).expect("text parses");
        assert!(matches!(text, ClientFrame::Text { delta } if delta == "Hello"));

        assert!(matches!(
            serde_json::from_str::<ClientFrame>(r#"{"type":"end"}"#),
            Ok(ClientFrame::End)
        ));
        assert!(matches!(
            serde_json::from_str::<ClientFrame>(r#"{"type":"cancel"}"#),
            Ok(ClientFrame::Cancel)
        ));
    }

    #[test]
    fn an_unknown_or_malformed_frame_is_rejected_rather_than_ignored() {
        for bad in [
            r#"{"type":"speak","text":"hi"}"#,
            r#"{"type":"text"}"#,
            r"not json",
            r"{}",
        ] {
            assert!(
                serde_json::from_str::<ClientFrame>(bad).is_err(),
                "{bad} must not parse"
            );
        }
    }

    #[test]
    fn server_frames_serialize_to_their_documented_shape() {
        let utterance =
            serde_json::to_string(&ServerFrame::Utterance { id: "utt_1".into() }).unwrap();
        assert_eq!(utterance, r#"{"type":"utterance","id":"utt_1"}"#);

        // A mark is flattened, so its fields sit beside `type` rather than
        // nested — that is what the protocol doc shows.
        let mark = serde_json::to_string(&ServerFrame::Mark {
            mark: Mark {
                start_ms: Some(120),
                end_ms: Some(380),
                start_char: Some(6),
                end_char: Some(11),
            },
        })
        .unwrap();
        assert_eq!(
            mark,
            r#"{"type":"mark","start_ms":120,"end_ms":380,"start_char":6,"end_char":11}"#
        );

        // A backend with only word timings omits the character range entirely
        // rather than sending nulls.
        let partial = serde_json::to_string(&ServerFrame::Mark {
            mark: Mark {
                start_ms: Some(0),
                end_ms: Some(40),
                start_char: None,
                end_char: None,
            },
        })
        .unwrap();
        assert_eq!(partial, r#"{"type":"mark","start_ms":0,"end_ms":40}"#);

        let progress = serde_json::to_string(&ServerFrame::Progress {
            spoken_ms: 100,
            queued_ms: 900,
        })
        .unwrap();
        assert_eq!(
            progress,
            r#"{"type":"progress","spoken_ms":100,"queued_ms":900}"#
        );

        let done = serde_json::to_string(&ServerFrame::Done {
            id: "utt_1".into(),
            chunks: 3,
        })
        .unwrap();
        assert_eq!(done, r#"{"type":"done","id":"utt_1","chunks":3}"#);

        let error = serde_json::to_string(&ServerFrame::Error {
            message: "model_not_loaded".into(),
        })
        .unwrap();
        assert_eq!(error, r#"{"type":"error","message":"model_not_loaded"}"#);
    }
}
