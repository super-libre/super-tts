// SPDX-License-Identifier: GPL-3.0-only
//! `POST /v1/speak` and `POST /v1/speak/stop` — synthesize text and play it.
//!
//! The endpoint is deliberately thin: it validates the envelope, hands the body
//! to the command dispatcher, and maps the result. Everything that decides how
//! speech happens lives in [`crate::daemon::speech`].

use crate::daemon::http::internal::helpers::dispatch::{build_request, dispatch, json_response};
use crate::daemon::http::state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use super_tts_shared::models::protocol::{DaemonResponse, ErrorCode};

/// `POST /v1/speak` — synthesize `text` with the active model and play it.
///
/// Answers `202 Accepted` with the utterance id: the request returns once the
/// audio is queued, while the device is still playing it out. The id is what a
/// client cancels by, and it is returned from the first version of this
/// endpoint even though the current policy is "one at a time, newest wins" —
/// adding an id later would break every client written against it.
pub(crate) async fn speak(
    State(state): State<AppState>,
    body: Option<axum::Json<Value>>,
) -> Response {
    let Some(axum::Json(body)) = body else {
        return json_response(&DaemonResponse::error_with_code(
            ErrorCode::InvalidValue,
            "missing request body",
        ))
        .into_response();
    };
    // `text` is checked here as well as in the dispatcher so a missing field is
    // a coded 400 rather than the dispatcher's bare parse message.
    if body.get("text").and_then(Value::as_str).is_none() {
        return json_response(&DaemonResponse::error_with_code(
            ErrorCode::InvalidValue,
            "missing text",
        ))
        .into_response();
    }

    let request = build_request("speak", Some(body));
    let response = dispatch(&state.daemon, request).await;
    if response.status == "success" {
        return (StatusCode::ACCEPTED, axum::Json(response)).into_response();
    }
    json_response(&response).into_response()
}

/// `POST /v1/speak/stop` — stop the current utterance and drop queued audio.
///
/// Succeeds whether or not anything was speaking: a client cancelling on a
/// keypress should not have to know whether it won the race.
pub(crate) async fn speak_stop(State(state): State<AppState>) -> impl IntoResponse {
    let request = build_request("stop_speaking", None);
    json_response(&dispatch(&state.daemon, request).await)
}

pub(crate) fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/speak", axum::routing::post(speak))
        .route("/speak/stop", axum::routing::post(speak_stop))
}
