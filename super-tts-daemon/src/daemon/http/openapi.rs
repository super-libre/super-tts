// SPDX-License-Identifier: GPL-3.0-only
//! The `OpenAPI` document for the daemon protocol.
//!
//! The document is generated from the router, not written beside it: every
//! `/v1` route is registered through [`utoipa_axum::routes!`], which reads the
//! `#[utoipa::path]` attribute on the handler it points at. A route and its
//! documentation are therefore one declaration — adding a route without
//! documenting it does not compile, and changing a path changes both.
//!
//! `just openapi` writes the result to `docs/protocol/openapi.json`;
//! `just openapi-check` fails when the committed file is stale, so a protocol
//! change cannot merge without the published spec moving with it.
//!
//! The prose reference under `docs/protocol/` is not replaced by any of this.
//! It explains *when* to call an endpoint and how the pieces fit; the spec
//! states the shapes exactly, for tooling and for a client generator.

use utoipa::Modify;
use utoipa::OpenApi;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};

/// Base document: everything that is true of the protocol as a whole rather
/// than of one endpoint. The paths and schemas are filled in from the router
/// (see [`super::v1::openapi`]).
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Super TTS daemon protocol",
        description = "\
HTTP/1.1 + JSON over a Unix domain socket at `$XDG_RUNTIME_DIR/tts/super-tts-http.sock` \
(override with `SUPER_TTS_HTTP_SOCKET`). There is no TCP listener: the socket's \
filesystem permissions are the first layer of access control, and the daemon reads \
`SO_PEERCRED` on each connection to identify the calling binary.

Every endpoint except `POST /v1/auth/request` requires `Authorization: Bearer <token>`. \
A token is minted only after the user approves your app in a consent popup, and is bound \
to the approved binary — an app cannot widen its own permissions. See `docs/protocol/auth.md`.

Because the transport is a Unix socket, the `servers` entry below is nominal; point your \
client at the socket and use any `Host`. With curl:

```
curl --unix-socket \"$XDG_RUNTIME_DIR/tts/super-tts-http.sock\" \\
     -H \"Authorization: Bearer $TTS_TOKEN\" \\
     http://tts.local/v1/ping
```",
        license(name = "GPL-3.0-only", identifier = "GPL-3.0-only"),
        contact(name = "Super TTS", url = "https://github.com/jorge-menjivar/super-tts"),
    ),
    servers((url = "http://tts.local", description = "Nominal host; the transport is the Unix socket")),
    modifiers(&BearerAuth),
    tags(
        (name = "auth", description = "Consent handshake and token probing."),
        (name = "health", description = "Liveness and what the daemon is currently running."),
        (name = "speak", description = "Synthesize text, play it, and stop it — including the incremental WebSocket path for text that is still being produced."),
        (name = "events", description = "Server-Sent Events for playback state, audio levels, model and download progress."),
        (name = "pipeline", description = "The ordered stages an utterance passes through: which backend fills each, which model runs there, and on what device. Super TTS has one stage today; it is addressed by position so a second could be appended without a second endpoint family."),
        (name = "settings", description = "Stored daemon preferences, one value apiece, all under `/v1/settings`: audio cues, notification method, volume, language, update policy. Sharing the `settings` scope is not the same as being a setting \u{2014} `backend`, `pipeline` and `registry` are guarded by it too."),
        (name = "hardware", description = "What the daemon can see of this machine: GPUs, drivers, runtimes."),
        (name = "update", description = "Whether a newer daemon exists, and asking it to look now."),
        (name = "backends", description = "Installed backends: their models, options and secrets."),
        (name = "registry", description = "The published backend catalog: browse, install, update, uninstall."),
        (name = "voices", description = "The cloned-voice library: reference recordings the user owns, and the ids that name them on `POST /v1/speak`."),
    ),
)]
pub(crate) struct ApiDoc;

/// The one security scheme: the session token from `POST /v1/auth/request`,
/// presented as a bearer token. Declared here rather than per endpoint so the
/// scheme has a single definition; which *scopes* each endpoint needs is stated
/// on the endpoint, since that is where it differs.
struct BearerAuth;

impl Modify for BearerAuth {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi
            .components
            .as_mut()
            .expect("the derived document always carries a components object");
        components.add_security_scheme(
            "session_token",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "Session token from `POST /v1/auth/request`. Bound to the calling \
                         binary and valid for 30 days.",
                    ))
                    .build(),
            ),
        );
    }
}
