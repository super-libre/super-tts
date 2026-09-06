// SPDX-License-Identifier: GPL-3.0-only
//! `/v1/voices` — the cloned-voice library.
//!
//! A voice is a reference recording plus the id (`voice:<uuid>`) that names it
//! on [`POST /speak`](super::speak). These endpoints own the recording; the
//! speak path is what hands it to a backend, the first time a loaded model is
//! asked for that voice.
//!
//! **The clip is uploaded as a raw body, not JSON.** A recording is megabytes
//! of PCM; base64 in a JSON envelope would inflate it by a third and push it
//! through a parser that exists for small objects. Metadata rides in the query
//! string instead, which keeps adding a voice to one request.
//!
//! The house `ok`/`json_error` envelope builders are shared with the backends
//! endpoints, where they were first written; they are house style, not
//! backend-specific.

use super::backends::{json_error, json_error_msg, ok};
use crate::daemon::http::state::AppState;
use crate::voices::VoiceError;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use std::sync::Arc;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/voices", get(list))
        .route(
            "/voices",
            // The upload body is bounded here rather than by the global
            // default: the default exists to stop a runaway JSON object, and a
            // voice recording is legitimately far larger than one.
            post(create).layer(DefaultBodyLimit::max(crate::voices::MAX_UPLOAD_BYTES)),
        )
        .route("/voices/{id}", get(get_one).patch(rename).delete(remove))
        .route("/voices/{id}/audio", get(audio))
}

/// Query parameters of `POST /voices`.
///
/// `label` is optional to `serde` but required by the library, so a request
/// that omits it is refused with the house error envelope rather than axum's
/// bare rejection text.
#[derive(Debug, Deserialize)]
struct NewVoice {
    /// Display name. A library of unnamed voices is unusable.
    #[serde(default)]
    label: Option<String>,
    /// What the clip says, for models that clone in-context.
    #[serde(default)]
    transcript: Option<String>,
}

/// Body of `PATCH /voices/{id}`.
#[derive(Debug, Deserialize)]
struct RenameVoice {
    label: String,
}

/// `GET /voices` — every stored voice, newest first, alongside what the loaded
/// model can do with them.
///
/// The capability rides on this listing rather than on `GET /models` because
/// it is what a voices UI needs and nothing else does: whether the page should
/// offer cloning at all, how long a recording is worth keeping, and whether to
/// ask the user for a transcript. `GET /models` returns `(name, source)` pairs
/// and gaining a voice vocabulary there is a larger change than cloning needs.
async fn list(State(s): State<AppState>) -> Response {
    let voices = match s.daemon.voices.list() {
        Ok(voices) => voices,
        Err(e) => return error(&e),
    };
    let voices: Vec<serde_json::Value> = voices.iter().map(describe).collect();
    ok(&serde_json::json!({
        "status": "success",
        "voices": voices,
        "model": active_model_cloning(&s).await,
    }))
}

/// What the loaded model can do with a cloned voice. `null` when nothing is
/// loaded — the library is still readable, it just cannot be spoken with yet.
async fn active_model_cloning(s: &AppState) -> serde_json::Value {
    use super_tts_registry_types::manifest::VoiceKind;

    let guard = s.daemon.model.read().await;
    let Some(loaded) = guard.as_ref() else {
        return serde_json::Value::Null;
    };
    let def = &loaded.definition;
    serde_json::json!({
        "name":              def.name,
        "source":            def.source,
        "clones":            def.voice_kinds.contains(&VoiceKind::Cloned),
        "clone_ref_seconds": def.clone_ref_seconds,
        "needs_transcript":  def.clone_needs_transcript,
    })
}

/// `POST /voices?label=…[&transcript=…]` — add a voice from a WAV upload.
///
/// Answers `201` with the created voice, whose `voice_id` is what `POST /speak`
/// takes as `voice`.
async fn create(
    State(s): State<AppState>,
    Query(params): Query<NewVoice>,
    body: axum::body::Bytes,
) -> Response {
    if body.is_empty() {
        return json_error_msg(
            StatusCode::BAD_REQUEST,
            "invalid_value",
            "request body is empty; POST the WAV file as the body",
        );
    }
    let library = Arc::clone(&s.daemon.voices);
    // Decoding and resampling a two-minute clip is real CPU work, and it must
    // not run on the runtime thread that also drives playback.
    let created = tokio::task::spawn_blocking(move || {
        library.create(
            &body,
            params.label.as_deref().unwrap_or_default(),
            params.transcript.as_deref(),
        )
    })
    .await;

    match created {
        Ok(Ok(voice)) => (
            StatusCode::CREATED,
            [("content-type", "application/json")],
            serde_json::json!({ "status": "success", "voice": describe(&voice) }).to_string(),
        )
            .into_response(),
        Ok(Err(e)) => error(&e),
        Err(e) => json_error_msg(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            &format!("storing the clip failed: {e}"),
        ),
    }
}

/// `GET /voices/{id}` — one voice's metadata.
async fn get_one(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    match s.daemon.voices.get(&id) {
        Ok(voice) => ok(&serde_json::json!({ "status": "success", "voice": describe(&voice) })),
        Err(e) => error(&e),
    }
}

/// `PATCH /voices/{id}` — rename.
///
/// Only the label. The transcript describes the stored audio and backends
/// cache what they derived from the pair, so changing it would leave every
/// registration already made disagreeing with the library.
async fn rename(
    State(s): State<AppState>,
    Path(id): Path<String>,
    axum::Json(body): axum::Json<RenameVoice>,
) -> Response {
    match s.daemon.voices.rename(&id, &body.label) {
        Ok(voice) => ok(&serde_json::json!({ "status": "success", "voice": describe(&voice) })),
        Err(e) => error(&e),
    }
}

/// `DELETE /voices/{id}` — forget a voice and its recording.
async fn remove(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    let voice = match s.daemon.voices.get(&id) {
        Ok(v) => v,
        Err(e) => return error(&e),
    };
    if let Err(e) = s.daemon.voices.delete(&id) {
        return error(&e);
    }
    release_from_loaded_model(&s, &voice.voice_id()).await;
    ok(&serde_json::json!({ "status": "success", "deleted": voice.voice_id() }))
}

/// `GET /voices/{id}/audio` — the stored clip, so a client can play it back.
async fn audio(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    let library = Arc::clone(&s.daemon.voices);
    match tokio::task::spawn_blocking(move || library.clip_wav(&id)).await {
        Ok(Ok(bytes)) => (
            StatusCode::OK,
            [("content-type", "audio/wav")],
            axum::body::Bytes::from(bytes),
        )
            .into_response(),
        Ok(Err(e)) => error(&e),
        Err(e) => json_error_msg(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            &format!("reading the clip failed: {e}"),
        ),
    }
}

/// Tell the loaded model to drop a voice the user just deleted.
///
/// Best-effort by design: the registration lives in the backend's memory and
/// dies with the instance, so a failure here costs a little VRAM until the next
/// model switch — not correctness. Deleting the voice has already succeeded,
/// and failing the request over the cleanup would tell the user the wrong
/// thing.
async fn release_from_loaded_model(s: &AppState, voice_id: &str) {
    let guard = s.daemon.model.read().await;
    let Some(loaded) = guard.as_ref() else {
        return;
    };
    if !loaded.cloned_voices.lock().remove(voice_id) {
        return;
    }
    if let Err(e) = loaded.instance.unregister_voice(voice_id).await {
        log::warn!("backend kept cloned voice {voice_id} after deletion: {e}");
    }
}

/// One voice on the wire: its stored metadata plus the id clients actually
/// pass to `/speak`, so nothing has to know how to build it.
fn describe(voice: &crate::voices::Voice) -> serde_json::Value {
    let mut value = serde_json::to_value(voice).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(map) = value.as_object_mut() {
        map.insert("voice_id".into(), voice.voice_id().into());
    }
    value
}

/// Map a library error onto the house error envelope.
fn error(e: &VoiceError) -> Response {
    match e {
        VoiceError::InvalidValue(msg) => {
            json_error_msg(StatusCode::BAD_REQUEST, "invalid_value", msg)
        }
        VoiceError::Audio(crate::voices::wav::WavError::TooLong { .. }) => {
            json_error_msg(StatusCode::BAD_REQUEST, "clip_too_long", &e.to_string())
        }
        VoiceError::Audio(_) => {
            json_error_msg(StatusCode::BAD_REQUEST, "unsupported_audio", &e.to_string())
        }
        VoiceError::NotFound => json_error(StatusCode::NOT_FOUND, "not_found"),
        VoiceError::Io(_) => json_error_msg(
            StatusCode::SERVICE_UNAVAILABLE,
            "voice_library_unavailable",
            &e.to_string(),
        ),
    }
}
