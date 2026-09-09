// SPDX-License-Identifier: GPL-3.0-only
//! Drives the daemon's real `SubprocessBackend` orchestration (manifest parse,
//! socket, systemd-run spawn, ping/load/status/synthesize, teardown) against the
//! `mock_backend` fixture — no GPU, model, or network. Needs a systemd `--user`
//! session, so it is gated behind `SUPER_TTS_TEST_SUBPROCESS=1` and skipped on
//! hosted CI runners.
//!
//! Run: `SUPER_TTS_TEST_SUBPROCESS=1` cargo test -p super-tts-daemon \
//!        --features test-fixtures --test `subprocess_mock` -- --nocapture
#![cfg(all(feature = "subprocess-backends", feature = "test-fixtures"))]

use super_tts_daemon::registry::host_detect;
use super_tts_daemon::tts_models::subprocess::SubprocessBackend;
use super_tts_daemon::tts_models::synthesize::Synthesize;
use super_tts_daemon::tts_models::v1::{CollectingSink, SynthesizeRequest};
use super_tts_shared::audio::frames::{FrameKind, SampleFormat};

/// Removes the per-test backend dir on scope exit — including panic unwinds, so a
/// failed assertion doesn't leak `~/.cache/super-tts-mock-test-<pid>`.
struct CleanupDir(std::path::PathBuf);
impl Drop for CleanupDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const MOCK_TOML: &str = r#"
[backend]
source = "github.com/jorge-menjivar/super-tts-piper"
name = "Mock"
version = "0.0.0"
kind = "subprocess"
entrypoint = "mock-backend"
contract = "v1"
description = "Test backend."

[[models]]
name = "mock"
multilingual = false
primary_language = "en"
supported_languages = ["en"]
supported_devices = ["cpu"]

# A second model so two tests in one process do not collide: the transient unit
# is named for the model and the pid, and the daemon itself never runs two
# subprocess backends at once, so the name is only ambiguous under a test.
[[models]]
name = "mock-options"
multilingual = false
primary_language = "en"
supported_languages = ["en"]
supported_devices = ["cpu"]
"#;

/// A backend directory holding the manifest and the mock binary, plus the guard
/// that removes it.
///
/// Built outside `/tmp`: `PrivateTmp=yes` in the systemd sandbox makes `/tmp`
/// private, so a `ReadOnlyPaths=/tmp/...` bind mount fails at namespace setup.
/// `label` keeps two tests in one process from sharing a directory, and with it
/// a unit name.
fn seed_backend_dir(label: &str) -> (std::path::PathBuf, CleanupDir) {
    let dir = dirs::cache_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join(format!(
            "super-tts-mock-test-{label}-{}",
            std::process::id()
        ));
    std::fs::create_dir_all(&dir).unwrap();
    let cleanup = CleanupDir(dir.clone());
    std::fs::write(dir.join("backend.toml"), MOCK_TOML).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_mock_backend"), dir.join("mock-backend")).unwrap();
    (dir, cleanup)
}

#[tokio::test]
async fn subprocess_orchestration_against_mock() {
    if std::env::var("SUPER_TTS_TEST_SUBPROCESS").is_err() {
        return; // needs a systemd --user session
    }
    // `main` installs the provider for the real daemon; an integration test is
    // its own process and has to do the same, or the first reqwest client built
    // during spawn panics with "No provider set". Without this the test fails
    // before it reaches any assertion — which is why the gap survived: it only
    // runs with SUPER_TTS_TEST_SUBPROCESS=1, never on CI.
    super_tts_daemon::install_crypto_provider();

    let (dir, _cleanup) = seed_backend_dir("orchestration");

    // Spawn + load via the real daemon orchestration.
    // The real probe: the mock declares no `[[models.files]]` at all, so
    // selection has nothing to resolve, but `spawn` takes the host the daemon
    // hands it and the test should exercise the same call.
    let host = host_detect::detect();
    let mut backend = SubprocessBackend::spawn(&dir, "mock", "cpu", &host, None, Vec::new())
        .await
        .expect("spawn + load mock backend");

    // Synthesize drives /v1/synthesize → a framed s16le ramp read incrementally
    // off the socket. The ramp is the point: silence would survive a decoder
    // that dropped a frame or lost the bytes straddling a frame boundary.
    let mut sink = CollectingSink::default();
    backend
        .synthesize(
            &SynthesizeRequest {
                text: "hello",
                ..Default::default()
            },
            &mut sink,
        )
        .await
        .expect("synthesize");

    let params = sink.params.expect("params arrive before any frame");
    assert_eq!(params.sample_rate, 24000);
    assert_eq!(params.channels, 1);
    assert_eq!(params.format, SampleFormat::S16Le);
    assert_eq!(
        sink.frames
            .iter()
            .filter(|f| f.kind == FrameKind::Audio)
            .count(),
        4,
        "the mock splits the ramp across 4 frames"
    );

    let samples = sink.samples();
    assert_eq!(samples.len(), 960);
    for (i, got) in samples.iter().enumerate() {
        let want = f32::from(i16::try_from(i).unwrap()) / 32768.0;
        assert!(
            (got - want).abs() < 1e-6,
            "sample {i}: got {got}, want {want}"
        );
    }
    assert_eq!(
        sink.frames.last().expect("frames were received").kind,
        FrameKind::Done,
        "the stream must end with a terminal frame"
    );

    backend.shutdown().await.expect("clean shutdown");
}

/// The contract says every `/v1` request carries the user's `[[options]]` as
/// `x-tts-option-*` headers, whichever transport. A backend that steers on one
/// — the Qwen backend designs its voice from a pair of them — reads them off
/// the request, so the daemon has to put them there.
///
/// Asserted on `/v1/synthesize` specifically: it is the one route that builds
/// its own request rather than going through `request`, because it streams its
/// response, and it is the route the options are for.
#[tokio::test]
async fn option_headers_reach_the_subprocess() {
    if std::env::var("SUPER_TTS_TEST_SUBPROCESS").is_err() {
        return; // needs a systemd --user session
    }
    super_tts_daemon::install_crypto_provider();

    let (dir, _cleanup) = seed_backend_dir("headers");

    let headers = vec![
        (
            "x-tts-option-voice_design_preset".to_string(),
            "Deep narrator (male)".to_string(),
        ),
        (
            "x-tts-option-voice_design_description".to_string(),
            "A hoarse pirate".to_string(),
        ),
    ];
    let host = host_detect::detect();
    let mut backend = SubprocessBackend::spawn(&dir, "mock-options", "cpu", &host, None, headers)
        .await
        .expect("spawn + load mock backend");

    let mut sink = CollectingSink::default();
    backend
        .synthesize(
            &SynthesizeRequest {
                text: "hello",
                ..Default::default()
            },
            &mut sink,
        )
        .await
        .expect("synthesize");

    let mark = sink
        .frames
        .iter()
        .find(|f| f.kind == FrameKind::Mark)
        .expect("the mock emits one mark frame");
    let echoed = String::from_utf8_lossy(&mark.payload);
    assert!(
        echoed.contains(
            r#""options":"voice_design_description=A hoarse pirate voice_design_preset=Deep narrator (male)""#
        ),
        "the option headers must arrive on the streaming request: {echoed}"
    );

    backend.shutdown().await.expect("clean shutdown");
}
