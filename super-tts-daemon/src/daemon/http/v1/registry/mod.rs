// SPDX-License-Identifier: GPL-3.0-only
//! `/registry/backend/*` — the published catalog and the acts that change what
//! is installed from it.
//!
//! Contracts: `docs/protocol/endpoints/v1/registry/`.
//!
//! Four paths, one subject: [`list`] is what the registry offers, [`refresh`]
//! forces the index to be re-fetched, and [`install`] and [`update`] put bytes
//! on disk. Removing them again is the one act that is not here — it belongs to
//! the thing being removed, at
//! [`DELETE /backend/{backend_id}`](super::backends::uninstall_backend).
//!
//! [`pipeline`] is not an endpoint module. It is the background install
//! machinery [`install`] and [`update`] both hand work to, kept beside them
//! because it is theirs and nothing else's.
//!
//! These endpoints answer failures in their own envelope — see
//! [`registry_error`] — which is why they name `RegistryError` where the rest of
//! `/v1` names `ErrorEnvelope`.
pub(crate) mod install;
pub(crate) mod list;
pub(crate) mod pipeline;
pub(crate) mod refresh;
pub(crate) mod update;

use crate::daemon::http::state::AppState;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

/// Registry error envelope: `{ "status": "error", "error_code": <code>, "error": <code> }`
/// at `status`.
///
/// The registry endpoints historically used a bare `{ "error": <code> }` shape.
/// `error_code` (and `status`) are now emitted alongside the retained `error`
/// key so the whole surface honors `transport.md`'s "`error_code` present on
/// every error" contract without breaking clients that read `error` (audit 2
/// Tier 2 #6). This helper is the single place that shape is built.
pub(crate) fn registry_error(status: StatusCode, code: &str) -> Response {
    (
        status,
        [("content-type", "application/json")],
        serde_json::json!({ "status": "error", "error_code": code, "error": code }).to_string(),
    )
        .into_response()
}

/// [`registry_error`] with a human-readable `message`.
pub(crate) fn registry_error_msg(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        [("content-type", "application/json")],
        serde_json::json!({
            "status": "error",
            "error_code": code,
            "error": code,
            "message": message,
        })
        .to_string(),
    )
        .into_response()
}

/// Registry browse/refresh/install/update routes (settings-scope).
///
/// No path appears here: `routes!` reads each one off the handler's own
/// `#[utoipa::path]`, so the URL a client calls and the URL the document
/// publishes are the same string.
pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list::list_registry_backends))
        .routes(routes!(refresh::refresh_registry))
        .routes(routes!(install::install_registry_backend))
        .routes(routes!(update::update_registry_backend))
}

#[cfg(test)]
mod tests {
    use super::{registry_error, registry_error_msg};
    use axum::http::StatusCode;

    async fn body_of(resp: axum::response::Response) -> (StatusCode, String) {
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("collect body");
        (status, String::from_utf8(bytes.to_vec()).expect("utf8"))
    }

    /// The registry error envelope now carries the machine-readable `error_code`
    /// (and `status`) per transport.md, while retaining the legacy `error` key so
    /// existing clients keep working (audit 2 Tier 2 #6).
    #[tokio::test]
    async fn registry_error_carries_error_code_and_legacy_key() {
        let (status, body) = body_of(registry_error(StatusCode::NOT_FOUND, "not_found")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let v: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(v["status"], "error");
        assert_eq!(v["error_code"], "not_found");
        assert_eq!(v["error"], "not_found"); // retained for back-compat

        let (status, body) = body_of(registry_error_msg(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "provide exactly one of source, repo_url, local_path",
        ))
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let v: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(v["status"], "error");
        assert_eq!(v["error_code"], "bad_request");
        assert_eq!(v["error"], "bad_request");
        assert_eq!(
            v["message"],
            "provide exactly one of source, repo_url, local_path"
        );
    }
}
