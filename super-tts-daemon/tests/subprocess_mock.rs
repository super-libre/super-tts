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
"#;

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

    // Build a backend dir outside /tmp: PrivateTmp=yes in the systemd sandbox makes
    // /tmp private, so ReadOnlyPaths=/tmp/... bind mounts fail at namespace setup.
    let dir = dirs::cache_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join(format!("super-tts-mock-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let _cleanup = CleanupDir(dir.clone());
    std::fs::write(dir.join("backend.toml"), MOCK_TOML).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_mock_backend"), dir.join("mock-backend")).unwrap();

    // Spawn + load via the real daemon orchestration.
    let mut backend = SubprocessBackend::spawn(&dir, "mock", "cpu", None)
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
