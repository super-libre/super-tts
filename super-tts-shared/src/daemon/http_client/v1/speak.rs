// SPDX-License-Identifier: GPL-3.0-only
//! Client half of `POST /v1/speak` and `POST /v1/speak/stop`.
//!
//! Speaking is fire-and-forget by design: the daemon answers `202` as soon as
//! the utterance is queued and keeps playing after the connection closes, so
//! there is no streaming variant here. A client that wants to follow the
//! utterance subscribes to the `speaking_state` / `speech_progress` topics on
//! `GET /events` instead — those outlive any one request, which is what a
//! status widget actually needs.

use super::super::internal::error::HttpResult;
use super::super::internal::transport;
use crate::models::protocol::DaemonResponse;
use std::path::PathBuf;

/// Per-request synthesis options. Every field is optional: with all of them
/// unset the daemon uses the active model's default voice, its configured
/// language, and the backend's natural rate.
#[derive(Debug, Default, Clone)]
pub struct SpeakOptions {
    /// A voice id the active model declares, or `None` for its `default_voice`.
    pub voice: Option<String>,
    /// BCP-47 language override.
    pub language: Option<String>,
    /// Rate multiplier; backends that cannot vary rate ignore it.
    pub speed: Option<f32>,
    /// Free-text delivery guidance, for models that accept it.
    pub instructions: Option<String>,
}

/// Build the `POST /speak` body, omitting every option the caller left unset so
/// the daemon sees "not specified" rather than an explicit null.
fn speak_body(text: &str, opts: &SpeakOptions) -> serde_json::Value {
    let mut data = serde_json::json!({ "text": text });
    if let Some(voice) = &opts.voice {
        data["voice"] = serde_json::Value::String(voice.clone());
    }
    if let Some(language) = &opts.language {
        data["language"] = serde_json::Value::String(language.clone());
    }
    if let Some(speed) = opts.speed {
        data["speed"] = serde_json::json!(speed);
    }
    if let Some(instructions) = &opts.instructions {
        data["instructions"] = serde_json::Value::String(instructions.clone());
    }
    data
}

/// `POST /speak` — synthesize `text` with the active model and play it.
///
/// Returns once the utterance is *queued*, not once it finishes: the response
/// carries `utterance_id`, which is what correlates the playback events with
/// this call.
///
/// # Errors
/// Returns an error if the daemon HTTP listener isn't reachable or the
/// response can't be parsed.
pub async fn speak(
    socket_path: PathBuf,
    token: &str,
    text: &str,
    opts: SpeakOptions,
) -> HttpResult<DaemonResponse> {
    let req = transport::build_post_json("/speak", &speak_body(text, &opts), Some(token))?;
    transport::send_request::<DaemonResponse>(&socket_path, req).await
}

/// `POST /speak/stop` — stop the current utterance and drop queued audio.
///
/// Succeeds whether or not anything was speaking, so a client cancelling on a
/// keypress doesn't have to win a race to get a clean result.
///
/// # Errors
/// Returns an error if the daemon HTTP listener isn't reachable or the
/// response can't be parsed.
pub async fn speak_stop(socket_path: PathBuf, token: &str) -> HttpResult<DaemonResponse> {
    let req = transport::build_post_json("/speak/stop", &serde_json::json!({}), Some(token))?;
    transport::send_request::<DaemonResponse>(&socket_path, req).await
}
