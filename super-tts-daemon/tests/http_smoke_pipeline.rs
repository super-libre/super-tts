// SPDX-License-Identifier: GPL-3.0-only
//! `/pipeline` HTTP smoke test, driving stage 1 (synthesis).
//!
//! Covers the endpoint family's contract against a live daemon:
//! 1. `GET /pipeline` reports the stages in order, and `GET /pipeline/{stage}`
//!    is narrowed from that same report, so the two can never disagree.
//! 2. A position the pipeline does not have is `404 unknown_stage`, on every
//!    path that carries a stage number.
//! 3. `GET /pipeline/1/backend/list` is what `POST /pipeline/1` accepts, and
//!    `GET /pipeline/1/model/list` is what the selected backend serves.
//! 4. Select a backend, run nothing, deselect — a stage's backend and its model
//!    are separate lifetimes with separate verbs.
//! 5. `/pipeline/1/model/{model}/device` reads and records a device **per
//!    model**, with the validation the retired global `/active_device` had no
//!    place to put.
//! 6. `/pipeline/1/device/list` and `.../model/{model}/device/list` say what a
//!    stage's backend, and one of its models, can be run on here.
//!
//! Uses `SUPER_TTS_KEYRING_MOCK=1` (in-memory keyring) and
//! `SUPER_TTS_AUTO_APPROVE=1` (no GUI) — hermetic, part of default CI.
//!
//! The fixture backend declares one local model (`kokoro-82m`, cpu+gpu) and one
//! that runs remotely (`gpt-4o-tts`, `none`), so both sides of every device
//! answer are exercisable. Both are `wasm` with a placeholder entrypoint: these
//! tests drive selection and validation, which happen before anything is
//! loaded, and the required secret keeps the daemon from loading a model at
//! startup.

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::client::conn::http1::handshake;
use hyper::{Method, Request, StatusCode};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use super_tts_shared::daemon::http_client;
use tokio::net::UnixStream;
use tokio::time::sleep;

const DAEMON_BIN: &str = env!("CARGO_BIN_EXE_super-tts-daemon");

/// The fixture backend's repo id — the `source` half of every model identity in
/// these tests, and what `POST /pipeline/1` is given.
const FIXTURE_SOURCE: &str = "github.com/super-tts/openai";

/// Synthesis is stage 1, and the only stage this build has.
const STAGE: &str = "/pipeline/1";

/// The fixture's local model: declares both devices, so it is the one a device
/// preference can actually be moved on.
const LOCAL_MODEL: &str = "kokoro-82m";

/// The fixture's remote model: `supported_devices = ["none"]`, so it has no
/// local device at all.
const ONLINE_MODEL: &str = "gpt-4o-tts";

struct DaemonGuard {
    child: Child,
    cleanup_paths: Vec<PathBuf>,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for p in &self.cleanup_paths {
            let _ = std::fs::remove_file(p);
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

fn next_test_uniq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static UNIQ: AtomicU64 = AtomicU64::new(0);
    UNIQ.fetch_add(1, Ordering::Relaxed)
}

/// Seed the fixture backend into `<data_home>/super-tts/backends/fixture-openai/`.
fn seed_fixture_backend(data_home: &Path) {
    let backend_dir = data_home
        .join("super-tts")
        .join("backends")
        .join("fixture-openai");
    std::fs::create_dir_all(&backend_dir).expect("create fixture backend dir");

    let toml = r#"[backend]
source = "github.com/super-tts/openai"
name = "Fixture OpenAI"
version = "1.0.0"
kind = "wasm"
entrypoint = "openai.wasm"
contract = "v1"
description = "Test backend."
license = "Apache-2.0"

[network]
allowed_hosts = ["api.openai.com"]

[[secrets]]
name = "openai_api_key"
label = "OpenAI API key"
description = "Your OpenAI API key."
required = true

[[models]]
name = "kokoro-82m"
primary_language = "en"
multilingual = true
supported_languages = ["en", "es", "es-MX"]
supported_devices = ["cpu", "gpu"]

[[models]]
name = "gpt-4o-tts"
primary_language = "en"
multilingual = true
supported_languages = ["en", "es"]
supported_devices = ["none"]
"#;
    std::fs::write(backend_dir.join("backend.toml"), toml).expect("write fixture backend.toml");
    // Placeholder entrypoint so the manifest can reference it and selection can
    // confirm the backend's files are on disk.
    std::fs::write(backend_dir.join("openai.wasm"), b"").expect("write placeholder entrypoint");
}

async fn start_daemon(scopes: &[&str]) -> (DaemonGuard, PathBuf, String) {
    let unique = format!("tts-pipeline-{}-{}", std::process::id(), next_test_uniq());
    let tmp = std::env::temp_dir();
    let http_socket = tmp.join(format!("{unique}-http.sock"));
    let config_home = tmp.join(format!("{unique}-config"));
    let data_home = tmp.join(format!("{unique}-data"));

    std::fs::create_dir_all(&config_home).expect("create test config dir");
    std::fs::create_dir_all(&data_home).expect("create test data dir");

    seed_fixture_backend(&data_home);

    let child = Command::new(DAEMON_BIN)
        .env("SUPER_TTS_KEYRING_MOCK", "1")
        .env("SUPER_TTS_AUTO_APPROVE", "1")
        .env("SUPER_TTS_MUTE_CUES", "1")
        .env("SUPER_TTS_HTTP_SOCKET", &http_socket)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_DATA_HOME", &data_home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn super-tts-daemon");

    // Hand the child to the guard before the readiness loop: the timeout panic
    // below must still kill and reap the daemon, not leak it.
    let guard = DaemonGuard {
        child,
        cleanup_paths: vec![http_socket.clone(), config_home, data_home],
    };

    let deadline = Instant::now() + Duration::from_mins(2);
    while Instant::now() < deadline {
        if Path::new(&http_socket).exists()
            && http_client::auth_request(http_socket.clone(), "pipeline-smoke-probe", &["status"])
                .await
                .is_ok()
        {
            let auth = http_client::auth_request(http_socket.clone(), "pipeline-smoke", scopes)
                .await
                .expect("auth_request for test scopes");
            return (guard, http_socket, auth.session_token);
        }
        sleep(Duration::from_millis(200)).await;
    }
    panic!("daemon HTTP listener not ready within 120s");
}

/// Issue an HTTP request and return `(status, json_body)`.
async fn raw_request(
    socket_path: &PathBuf,
    method: Method,
    path: &str,
    token: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let stream = UnixStream::connect(socket_path).await.expect("connect");
    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, conn) = handshake::<_, Full<Bytes>>(io).await.expect("handshake");
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let body_bytes = body
        .map(|b| serde_json::to_vec(&b).expect("encode body"))
        .unwrap_or_default();

    let mut builder = Request::builder()
        .method(method)
        .uri(format!("http://tts.local/v1{path}"))
        .header("host", "tts.local")
        .header("authorization", format!("Bearer {token}"));
    if !body_bytes.is_empty() {
        builder = builder
            .header("content-type", "application/json")
            .header("content-length", body_bytes.len().to_string());
    }
    let req = builder
        .body(Full::new(Bytes::from(body_bytes)))
        .expect("build req");

    let resp = sender.send_request(req).await.expect("send req");
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn get(p: &PathBuf, path: &str, token: &str) -> (StatusCode, serde_json::Value) {
    raw_request(p, Method::GET, path, token, None).await
}

async fn post_req(
    p: &PathBuf,
    path: &str,
    token: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    raw_request(p, Method::POST, path, token, Some(body)).await
}

async fn delete_req(p: &PathBuf, path: &str, token: &str) -> (StatusCode, serde_json::Value) {
    raw_request(p, Method::DELETE, path, token, None).await
}

/// Point stage 1 at the fixture backend. Selection is validation only — nothing
/// is loaded — which is what makes every model-addressed path below resolvable
/// without a model ever coming up.
async fn select_fixture_backend(sock: &PathBuf, token: &str) {
    let (st, body) = post_req(
        sock,
        STAGE,
        token,
        serde_json::json!({ "source": FIXTURE_SOURCE }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "select the fixture backend: {body}");
}

/// The pipeline reports its stages in order, each naming what it is for, and
/// one position read on its own agrees with the list it came from.
///
/// This is the shape a stage added ahead of synthesis would join, so the array
/// and the `role` on each entry are the contract rather than an implementation
/// detail — a client written against `/pipeline/1` already knows how to drive a
/// position that does not exist yet.
#[tokio::test]
async fn the_pipeline_reports_its_stages_in_order() {
    let (_guard, sock, token) = start_daemon(&["settings"]).await;

    let (st, body) = get(&sock, "/pipeline", &token).await;
    assert_eq!(st, StatusCode::OK, "GET /pipeline: {body}");
    assert_eq!(body["status"], "success", "{body}");
    let stages = body["pipeline"].as_array().expect("an ordered stage list");
    assert_eq!(stages.len(), 1, "this build has one stage: {body}");
    assert_eq!(stages[0]["stage"], 1, "{body}");
    assert_eq!(stages[0]["role"], "synthesis", "{body}");
    assert_eq!(stages[0]["enabled"], false, "nothing is running: {body}");
    assert!(
        stages[0]["source"].is_null(),
        "an empty stage reports an explicit null, not an absent key: {body}"
    );

    // One position, narrowed from the same report.
    let (st, one) = get(&sock, STAGE, &token).await;
    assert_eq!(st, StatusCode::OK, "GET /pipeline/1: {one}");
    assert_eq!(one["stage"], stages[0], "the two views must agree: {one}");
    // The model is one level down: a stage reports the backend filling it and
    // says nothing about what that backend is running.
    assert!(
        one["stage"].get("model").is_none() && one["stage"].get("loaded").is_none(),
        "a stage must not carry model fields: {one}"
    );

    // And they keep agreeing once the stage is filled.
    select_fixture_backend(&sock, &token).await;
    let (_, one) = get(&sock, STAGE, &token).await;
    assert_eq!(one["stage"]["source"], FIXTURE_SOURCE, "{one}");
    assert_eq!(one["stage"]["name"], "Fixture OpenAI", "{one}");
    let (_, whole) = get(&sock, "/pipeline", &token).await;
    assert_eq!(whole["pipeline"][0], one["stage"], "{whole}");
}

/// A position the pipeline does not have is a `404` that says so, on every path
/// carrying a stage number — rather than a silent no-op, or a 404 a client
/// cannot tell from a typo'd path.
///
/// The message names the stages that do exist because the number in the URL is
/// the client's only handle on the pipeline's shape.
#[tokio::test]
async fn a_stage_the_pipeline_does_not_have_is_not_found() {
    let (_guard, sock, token) = start_daemon(&["settings"]).await;

    for path in [
        "/pipeline/2",
        "/pipeline/0",
        "/pipeline/2/model",
        "/pipeline/2/model/list",
        "/pipeline/2/backend/list",
        "/pipeline/2/device/list",
        "/pipeline/2/model/kokoro-82m/device",
        "/pipeline/2/model/kokoro-82m/device/list",
        "/pipeline/2/model/kokoro-82m/language",
        "/pipeline/2/model/kokoro-82m/language/list",
    ] {
        let (st, body) = get(&sock, path, &token).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "GET {path}: {body}");
        assert_eq!(body["error_code"], "unknown_stage", "GET {path}: {body}");
        assert!(
            body["message"].as_str().is_some_and(|m| m.contains('1')),
            "the refusal names the stage that does exist: {body}"
        );
    }

    // The mutating verbs answer the same way, so a client cannot write to a
    // position it cannot read.
    let (st, body) = post_req(
        &sock,
        "/pipeline/2",
        &token,
        serde_json::json!({ "source": FIXTURE_SOURCE }),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error_code"], "unknown_stage", "{body}");
    let (st, body) = delete_req(&sock, "/pipeline/2/model", &token).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error_code"], "unknown_stage", "{body}");
}

/// A stage's backend list is what its `POST` accepts, and its model list is
/// what the backend it holds serves.
///
/// The model list being empty before a backend is chosen is the load-bearing
/// half: it reads as "choose a backend first" rather than as "this build has no
/// models", which is what a picker filled from the whole catalog would suggest.
#[tokio::test]
async fn a_stage_lists_the_backends_that_fill_it_and_the_models_it_runs() {
    let (_guard, sock, token) = start_daemon(&["settings"]).await;

    let (st, body) = get(&sock, "/pipeline/1/backend/list", &token).await;
    assert_eq!(st, StatusCode::OK, "GET the stage's backends: {body}");
    assert_eq!(body["status"], "success", "{body}");
    let offered: Vec<&str> = body["backends"]
        .as_array()
        .expect("backends is a list")
        .iter()
        .filter_map(|b| b["source"].as_str())
        .collect();
    assert!(
        offered.contains(&FIXTURE_SOURCE),
        "the installed backend synthesizes, so the only stage offers it: {offered:?}"
    );

    // Nothing selected: the stage runs no models, and says so with an empty
    // list rather than an error.
    let (st, body) = get(&sock, "/pipeline/1/model/list", &token).await;
    assert_eq!(st, StatusCode::OK, "GET the stage's models: {body}");
    assert_eq!(
        body["available_models"],
        serde_json::json!([]),
        "an empty stage runs nothing: {body}"
    );

    // The list is the promise: every backend offered is one POST takes.
    for source in &offered {
        let (st, body) = post_req(
            &sock,
            STAGE,
            &token,
            serde_json::json!({ "source": source }),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::OK,
            "offered {source} but refused it: {body}"
        );
    }

    // Filled, the model list is that backend's models, as `[name, source]`
    // pairs — the flat picker, not the whole catalog.
    let (st, body) = get(&sock, "/pipeline/1/model/list", &token).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let models = body["available_models"].as_array().expect("a list");
    assert_eq!(models.len(), 2, "both of the fixture's models: {body}");
    assert!(
        models
            .iter()
            .any(|m| m[0] == LOCAL_MODEL && m[1] == FIXTURE_SOURCE),
        "each entry pairs the name with the backend that serves it: {body}"
    );
}

/// Filling a stage and running a model in it are separate acts with separate
/// verbs, and `DELETE /pipeline/1` empties the stage without a model ever
/// having been loaded into it.
///
/// The regression: a Deselect that reported success while leaving the stage
/// pointed at the backend would leave a client rendering a card for a selection
/// the daemon no longer honors.
#[tokio::test]
async fn a_stage_is_filled_and_emptied_without_loading_anything() {
    let (_guard, sock, token) = start_daemon(&["settings"]).await;

    let (st, body) = post_req(
        &sock,
        STAGE,
        &token,
        serde_json::json!({ "source": FIXTURE_SOURCE }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        body["active_backend"]["source"], FIXTURE_SOURCE,
        "the mutation names what the stage now holds, so a client can render \
         the result of its own click without a second read: {body}"
    );
    assert_eq!(
        body["active_backend"]["model_loaded"], false,
        "selecting a backend loads nothing: {body}"
    );

    // The model slot is reported whether or not anything fills it.
    let (st, body) = get(&sock, "/pipeline/1/model", &token).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["model"]["stage"], 1, "{body}");
    assert!(body["model"]["model"].is_null(), "{body}");
    assert_eq!(body["model"]["loaded"], false, "{body}");
    assert!(
        body["model"]["switch"].is_null(),
        "nothing in flight: {body}"
    );

    // Empty it: the backend is forgotten, and the stage reads as it did before
    // anything was chosen.
    let (st, body) = delete_req(&sock, STAGE, &token).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "success", "{body}");
    let (_, body) = get(&sock, STAGE, &token).await;
    assert!(
        body["stage"]["source"].is_null(),
        "Deselect must forget the backend, not just stop it: {body}"
    );
    assert_eq!(body["stage"]["enabled"], false, "{body}");
}

/// A device belongs to a model, not to the daemon: setting one for a model that
/// is not loaded records it — loading nothing — and reading it back through the
/// stage answers from that record. Two models on the same backend keep their
/// own.
///
/// This is what the retired global `/active_device` could not express. A small
/// preset-voice model runs fine on the CPU while the cloning model beside it
/// needs the GPU, and one global setting made each choice overwrite the other.
#[tokio::test]
async fn a_models_device_is_its_own() {
    let (_guard, sock, token) = start_daemon(&["settings"]).await;
    select_fixture_backend(&sock, &token).await;

    let local = format!("/pipeline/1/model/{LOCAL_MODEL}/device");

    // The default, before anything is set.
    let (st, body) = get(&sock, &local, &token).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["device"], "cpu", "{body}");
    assert_eq!(body["resolved_accel"], "cpu", "cpu needs no resolution");
    assert!(
        body["available_devices"]
            .as_array()
            .expect("available_devices is a list")
            .iter()
            .any(|d| d == "cpu"),
        "every host offers the CPU: {body}"
    );

    // `cuda` is the deprecated input spelling, accepted and normalized to `gpu`.
    let (st, body) = post_req(
        &sock,
        &local,
        &token,
        serde_json::json!({ "device": "cuda" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["device"], "gpu", "{body}");
    assert_eq!(
        body["resolved_accel"],
        serde_json::Value::Null,
        "a gpu choice has resolved to nothing until a load confirms it: {body}"
    );

    // Recorded, and nothing was loaded to record it.
    let (_, body) = get(&sock, &local, &token).await;
    assert_eq!(body["device"], "gpu", "the choice is recorded: {body}");
    let (_, body) = get(&sock, "/pipeline/1/model", &token).await;
    assert_eq!(body["model"]["loaded"], false, "nothing was loaded: {body}");

    // The other model on the same backend is untouched by that write — the
    // preference is per `(source, model)`, which is the whole point.
    let (st, body) = get(
        &sock,
        &format!("/pipeline/1/model/{ONLINE_MODEL}/device"),
        &token,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        body["device"], "none",
        "a model that synthesizes remotely has no local device: {body}"
    );
}

/// What the device verb refuses: a value that is not a device, a model that
/// runs remotely and so has no local device, a model the stage's backend does
/// not serve, and a stage with nothing selected to resolve the model against.
///
/// Each refusal is a distinct code because a client acts differently on each —
/// re-render the picker, hide the control, re-read the model list, or send the
/// user to pick a backend.
#[tokio::test]
async fn the_device_verb_validates_the_model_and_the_device() {
    let (_guard, sock, token) = start_daemon(&["settings"]).await;
    let local = format!("/pipeline/1/model/{LOCAL_MODEL}/device");

    // Nothing selected yet: there is nothing to resolve a bare model against.
    let (st, body) = get(&sock, &local, &token).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error_code"], "invalid_backend", "{body}");

    select_fixture_backend(&sock, &token).await;

    // A value that is not a device at all.
    let (st, body) = post_req(
        &sock,
        &local,
        &token,
        serde_json::json!({ "device": "definitely-not-a-real-device" }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error_code"], "invalid_device", "{body}");
    let (_, body) = get(&sock, &local, &token).await;
    assert_eq!(body["device"], "cpu", "the bogus value was not recorded");

    // A model that runs remotely: reading reports the manifest's `none`,
    // setting is refused.
    let online = format!("/pipeline/1/model/{ONLINE_MODEL}/device");
    let (st, body) = get(&sock, &online, &token).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["device"], "none", "{body}");
    assert_eq!(body["available_devices"], serde_json::json!([]), "{body}");
    let (st, body) = post_req(
        &sock,
        &online,
        &token,
        serde_json::json!({ "device": "cpu" }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error_code"], "invalid_device", "{body}");

    // A model this stage's backend does not serve.
    let (st, body) = get(&sock, "/pipeline/1/model/not-a-model/device", &token).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error_code"], "invalid_model", "{body}");

    // And the stage itself still has to exist.
    let (st, body) = get(&sock, "/pipeline/9/model/kokoro-82m/device", &token).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error_code"], "unknown_stage", "{body}");
}

/// The two device lists: a model's is what this install can offer that model,
/// and the stage's is the union over the models its backend serves — the
/// broader answer, for a picker painted before a model is chosen.
///
/// Showing a device control that will be wrong once a model is picked is worse
/// than showing the union and narrowing it, which is why both exist.
#[tokio::test]
async fn device_lists_for_a_model_and_for_a_stage() {
    let (_guard, sock, token) = start_daemon(&["settings"]).await;

    // Nothing selected: nothing to list against, and the stage still has to
    // exist.
    let (st, body) = get(&sock, "/pipeline/1/device/list", &token).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error_code"], "invalid_backend", "{body}");
    let (st, body) = get(&sock, "/pipeline/9/device/list", &token).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error_code"], "unknown_stage", "{body}");

    select_fixture_backend(&sock, &token).await;

    let (st, body) = get(
        &sock,
        &format!("/pipeline/1/model/{LOCAL_MODEL}/device/list"),
        &token,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let for_model = body["available_devices"]
        .as_array()
        .expect("a list")
        .clone();
    assert_eq!(
        for_model.first(),
        Some(&serde_json::json!("cpu")),
        "cpu first, always: {body}"
    );

    // A model that runs remotely offers nothing — the shape a client hides its
    // device control on, rather than special-casing a status.
    let (st, body) = get(
        &sock,
        &format!("/pipeline/1/model/{ONLINE_MODEL}/device/list"),
        &token,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["available_devices"], serde_json::json!([]), "{body}");

    // The stage's list is the union: the remote model adds nothing, so it is
    // the local model's list.
    let (st, body) = get(&sock, "/pipeline/1/device/list", &token).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        body["available_devices"].as_array(),
        Some(&for_model),
        "with one local model, the stage's list is that model's: {body}"
    );
}

/// Every `/pipeline` path is settings-scope. A token granted the client scopes
/// must not be able to read the pipeline's shape, let alone change it.
#[tokio::test]
async fn pipeline_endpoints_require_the_settings_scope() {
    let (_guard, sock, token) = start_daemon(&["speak", "status"]).await;

    for (method, path) in [
        (Method::GET, "/pipeline"),
        (Method::GET, STAGE),
        (Method::DELETE, STAGE),
        (Method::GET, "/pipeline/1/backend/list"),
        (Method::GET, "/pipeline/1/model"),
        (Method::GET, "/pipeline/1/model/list"),
        (Method::GET, "/pipeline/1/device/list"),
        (Method::GET, "/pipeline/1/model/kokoro-82m/device"),
        (Method::GET, "/pipeline/1/model/kokoro-82m/device/list"),
    ] {
        let (st, body) = raw_request(&sock, method.clone(), path, &token, None).await;
        assert_eq!(
            st,
            StatusCode::FORBIDDEN,
            "{method} /v1{path} must be 403 for a client-scope token: {body}"
        );
        assert_eq!(body["message"], "scope_denied", "{method} {path}: {body}");
    }
}
