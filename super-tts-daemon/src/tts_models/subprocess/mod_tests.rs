// SPDX-License-Identifier: GPL-3.0-only
//! The `POST /v1/load` body a subprocess backend receives.

use super::*;
use crate::tts_models::backends::manifest::Manifest;

/// Backends released against the earlier `(name, provider)` model identity
/// validate `provider` on load and answer `400 invalid_model` when it is
/// absent — the shipped xtts backend does exactly this. Dropping the key
/// from the body makes every model of every such backend unloadable, with no
/// version gate that could soften it, so a manifest that declares `provider`
/// must still have it forwarded.
///
/// This is the test that fails if the compatibility echo is deleted before
/// those backends have rolled over.
#[test]
fn load_forwards_the_provider_a_manifest_declares() {
    let body = load_body("kokoro-tiny", Some("local_kokoro"), "cuda");
    assert_eq!(
        body.get("provider").and_then(serde_json::Value::as_str),
        Some("local_kokoro"),
        "/v1/load dropped `provider`; backends validating it answer 400 invalid_model: {body}"
    );
    assert_eq!(
        body.get("name").and_then(serde_json::Value::as_str),
        Some("kokoro-tiny")
    );
    assert_eq!(
        body.get("device").and_then(serde_json::Value::as_str),
        Some("cuda")
    );
}

/// The echo is driven by the manifest, not synthesized: a model that declares
/// no `provider` must not gain one, or a backend that *does* validate the key
/// would start rejecting a load it previously accepted.
#[test]
fn load_omits_provider_and_device_when_unset() {
    let body = load_body("kokoro-tiny", None, "");
    assert!(
        body.get("provider").is_none(),
        "manifest declared no provider but the load body invented one: {body}"
    );
    assert!(
        body.get("device").is_none(),
        "empty device_pref sent: {body}"
    );
    assert_eq!(
        body.as_object().map(serde_json::Map::len),
        Some(1),
        "load body carries unexpected keys: {body}"
    );
}

/// `device` carries the resolved accelerator, so the daemon sends it only
/// when it has resolved one. A `cpu` preference always resolves — it is its
/// own accelerator — and must keep reaching the backend, since that is what
/// pins a load onto the CPU on a machine that has a GPU.
#[test]
fn load_sends_the_resolved_accelerator_and_never_the_bare_preference() {
    for accel in ["cpu", "cuda", "rocm", "vulkan", "metal"] {
        let body = load_body("kokoro-tiny", None, accel);
        assert_eq!(
            body.get("device").and_then(serde_json::Value::as_str),
            Some(accel),
            "the resolved accelerator must reach the backend: {body}"
        );
    }
    // What `resolve_accel` produces when it cannot name an accelerator — an
    // install with no record, which is every backend installed before the
    // record existed. `gpu` is the user's preference, and the contract says
    // this field is not that; an absent `device` means "auto-select", which is
    // the honest signal.
    let unresolved = load_body("kokoro-tiny", None, "");
    assert!(
        unresolved.get("device").is_none(),
        "an unresolved accel must omit `device`, not send a preference: {unresolved}"
    );
}

/// End-to-end over the real parser: the value reaching the wire is the one
/// written in `backend.toml`. Guards the whole path, not just `load_body` —
/// a `ModelEntry::provider` that stopped deserializing would leave the unit
/// tests above passing while every real load lost the key.
#[test]
fn a_manifests_provider_reaches_the_load_body() {
    let toml = r#"
[backend]
source = "github.com/jorge-menjivar/super-tts-xtts"
name = "XTTS"
version = "0.1.0"
kind = "subprocess"
entrypoint = "super-tts-xtts"
contract = "v1"
description = "Test backend."

[[models]]
name = "xtts-flash"
provider = "local_xtts_asr"
multilingual = true
primary_language = "en"
supported_languages = ["en"]
supported_devices = ["cuda"]
"#;
    let manifest = Manifest::parse(toml).expect("fixture manifest parses");
    let model = &manifest.models[0];
    assert_eq!(model.provider.as_deref(), Some("local_xtts_asr"));

    let body = load_body(&model.name, model.provider.as_deref(), "cuda");
    assert_eq!(
        body.get("provider").and_then(serde_json::Value::as_str),
        Some("local_xtts_asr"),
        "the manifest's provider did not reach the load body: {body}"
    );
}

/// The cache key comes from the backend directory's own name, so two
/// backends never share one — a shared cache would be a correctness bug
/// rather than a slow path, since `CubeCL`'s kernel database is keyed inside
/// the file by build and device, not by who wrote it.
#[test]
fn each_backend_gets_its_own_cache_dir() {
    let root = std::path::Path::new("/data/super-tts/backends");
    let qwen = backend_cache_dir(&root.join("app.super-tts.qwen-tts")).expect("named dir");
    let kokoro = backend_cache_dir(&root.join("app.super-tts.kokoro")).expect("named dir");
    assert_ne!(qwen, kokoro);
    assert!(
        qwen.ends_with("backends/app-super-tts-qwen-tts"),
        "{qwen:?}"
    );
    assert!(qwen.starts_with(super_tts_shared::paths::cache_dir()));
}

/// The directory name reaches the path through [`sanitize`], so a backend
/// directory that was somehow named with traversal cannot walk the cache
/// root. The installer will not produce such a name, but this path joins a
/// filesystem-derived string and is the wrong place to rely on that.
#[test]
fn a_traversing_directory_name_cannot_escape_the_cache_root() {
    let dir = std::path::Path::new("/data/super-tts/backends/..");
    // `..` is a path component, not a name — Path::file_name refuses it.
    assert!(backend_cache_dir(dir).is_err());

    let odd = backend_cache_dir(std::path::Path::new("/data/x/a..b/")).expect("named dir");
    assert!(odd.starts_with(super_tts_shared::paths::cache_dir().join("backends")));
    assert!(odd.ends_with("a--b"), "{odd:?}");
}
