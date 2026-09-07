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

/// `POST /speak` — speak `text` in one specific voice.
///
/// The Voices page's preview: a cloned voice is only judgeable by ear, and the
/// daemon already owns the output device, so hearing one is a normal utterance
/// with `voice` set rather than anything the app plays itself.
///
/// The utterance id is discarded — a preview is not something the page later
/// correlates or cancels, and the speaking badge is driven by events either
/// way.
pub async fn speak_in_voice(text: String, voice: String) -> HttpResult<()> {
    with_settings_token(move |socket, token| {
        let (text, voice) = (text.clone(), voice.clone());
        async move {
            let options = SpeakOptions {
                voice: Some(voice),
                ..SpeakOptions::default()
            };
            let resp = http_client::speak(socket, &token, &text, options).await?;
            require_unit(resp, "speak_in_voice")
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
