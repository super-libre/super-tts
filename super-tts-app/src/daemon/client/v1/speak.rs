// SPDX-License-Identifier: GPL-3.0-only
//! `/speak` — synthesize text and play it through the daemon.
//!
//! Both calls are ordinary requests, not streams: the daemon answers as soon as
//! the utterance is queued and keeps playing after the connection closes. The
//! Speech page follows what happens next through the `speaking_state` /
//! `speech_progress` topics on `GET /events`, which is also what keeps its
//! badge right when something *else* asked the daemon to speak.

use crate::daemon::client::internal::response::require_unit;
use crate::daemon::client::internal::session::with_settings_token;
use super_tts_shared::daemon::http_client;
use super_tts_shared::daemon::http_client::{HttpResult, SpeakOptions};

/// `POST /speak` — speak `text` with the active model and its default voice.
///
/// Returns the utterance id the daemon assigned, so the caller can tell its own
/// utterance apart from one another client started.
pub async fn speak_command(text: String) -> HttpResult<Option<String>> {
    with_settings_token(|socket, token| {
        let text = text.clone();
        async move {
            let resp = http_client::speak(socket, &token, &text, SpeakOptions::default()).await?;
            let utterance_id = resp.utterance_id.clone();
            require_unit(resp, "speak_command")?;
            Ok(utterance_id)
        }
    })
    .await
}

/// `POST /speak/stop` — stop the current utterance and drop queued audio.
pub async fn stop_speaking_command() -> HttpResult<()> {
    with_settings_token(|socket, token| async move {
        let resp = http_client::speak_stop(socket, &token).await?;
        require_unit(resp, "stop_speaking_command")
    })
    .await
}
