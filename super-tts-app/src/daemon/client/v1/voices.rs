// SPDX-License-Identifier: GPL-3.0-only
//! `/voices` — the cloned-voice library.
//!
//! The upload is the one call in the app that does not send JSON: the body is
//! the WAV file itself and the metadata rides in the query string, because a
//! recording is megabytes of PCM and base64 in an envelope would inflate it by
//! a third for nothing.

use crate::daemon::client::internal::session::with_settings_token;
use super_tts_shared::daemon::http_client::HttpResult;
use super_tts_shared::daemon::http_client::transport;
use super_tts_shared::models::voices::{
    VoiceDeletedResponse, VoiceInfo, VoiceListResponse, VoiceModelSupport, VoiceResponse,
};

fn enc(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

/// The library plus what the loaded model can do with it
/// (HTTP `GET /voices`).
///
/// Returns both halves because they arrive together and a UI needs both to
/// render one card: the voices, and whether the loaded model can speak in any
/// of them.
pub async fn list_voices() -> HttpResult<(Vec<VoiceInfo>, Option<VoiceModelSupport>)> {
    with_settings_token(|socket, token| async move {
        let resp: VoiceListResponse = transport::get_json(socket, &token, "/voices").await?;
        Ok((resp.voices, resp.model))
    })
    .await
}

/// Add a voice from a WAV file (HTTP `POST /voices`).
///
/// `audio` is a complete WAV file. The daemon downmixes, resamples, and stores
/// it — the app sends whatever the recorder or the file produced.
pub async fn create_voice(
    label: String,
    transcript: Option<String>,
    audio: Vec<u8>,
) -> HttpResult<VoiceInfo> {
    with_settings_token(move |socket, token| {
        let (label, transcript, audio) = (label.clone(), transcript.clone(), audio.clone());
        async move {
            let transcript = transcript
                .as_deref()
                .filter(|t| !t.trim().is_empty())
                .map_or_else(String::new, |t| format!("&transcript={}", enc(t)));
            let path = format!("/voices?label={}{transcript}", enc(&label));
            let resp: VoiceResponse =
                transport::post_bytes(socket, &token, &path, "audio/wav", audio).await?;
            Ok(resp.voice)
        }
    })
    .await
}

/// Rename a voice (HTTP `PATCH /voices/{id}`).
pub async fn rename_voice(id: String, label: String) -> HttpResult<VoiceInfo> {
    with_settings_token(move |socket, token| {
        let (id, label) = (id.clone(), label.clone());
        async move {
            let path = format!("/voices/{}", enc(&id));
            let resp: VoiceResponse = transport::patch_json(
                socket,
                &token,
                &path,
                &serde_json::json!({ "label": label }),
            )
            .await?;
            Ok(resp.voice)
        }
    })
    .await
}

/// Delete a voice and its recording (HTTP `DELETE /voices/{id}`).
pub async fn delete_voice(id: String) -> HttpResult<String> {
    with_settings_token(move |socket, token| {
        let id = id.clone();
        async move {
            let path = format!("/voices/{}", enc(&id));
            let resp: VoiceDeletedResponse = transport::delete_json(socket, &token, &path).await?;
            Ok(resp.deleted)
        }
    })
    .await
}
