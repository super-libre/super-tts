// SPDX-License-Identifier: GPL-3.0-only
//! The speak path end to end: text in, samples in the playback ring.
//!
//! Drives the real [`SpeechEngine`] against the real `WasmBackend` host and the
//! mock component fixture, with the playback pipeline detached from cpal. So
//! everything between `speak()` and the audio callback is production code —
//! request build, `POST /v1/synthesize`, frame decode, sample decode, the ring
//! — and no audio device is needed, which is what lets this run in CI.
//!
//! Requires the fixture: `just build-mock-wasm-backend`.
#![cfg(feature = "wasm-backends")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super_tts_daemon::audio::playback::DeviceFormat;
use super_tts_daemon::daemon::speech::{SpeakError, SpeechEngine};
use super_tts_daemon::daemon::types::{LoadedModel, SharedLoadedModel};
use super_tts_daemon::stt_models::ModelDefinition;
use super_tts_daemon::stt_models::wasm::WasmBackend;
use super_tts_registry_types::manifest::Device;

/// The mock synthesizes 960 samples of s16le at 24 kHz.
const MOCK_SAMPLES: usize = 960;
const MOCK_RATE: u32 = 24000;

/// Device format for the null sink. 48 kHz mono means the mock's 24 kHz output
/// is resampled on the way in — the ordinary case, so the test covers it.
const DEVICE: DeviceFormat = DeviceFormat {
    sample_rate: 48000,
    channels: 1,
};

fn mock_component() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "tests/fixtures/mock-wasm-backend/target/wasm32-wasip2/release/mock_wasm_backend.wasm",
    );
    p.exists().then_some(p)
}

fn definition() -> ModelDefinition {
    ModelDefinition {
        name: "mock".into(),
        source: "github.com/super-tts/mock".into(),
        is_multilingual: false,
        primary_language: "en".into(),
        supported_languages: vec!["en".into()],
        estimated_vram_bytes: 0,
        processing_interval: Duration::from_millis(0),
        supported_devices: vec![Device::None],
        realtime: false,
        provider: None,
    }
}

/// A shared slot holding the mock backend, or `None` when the fixture is not
/// built (the caller then skips).
fn loaded_model() -> Option<SharedLoadedModel> {
    let path = mock_component()?;
    let backend = WasmBackend::new(&path, Vec::new(), "mock".to_string(), Vec::new())
        .expect("load mock backend");
    Some(Arc::new(tokio::sync::RwLock::new(Some(LoadedModel {
        definition: definition(),
        instance: Box::new(backend),
    }))))
}

/// An empty slot, for the not-loaded path.
fn empty_model() -> SharedLoadedModel {
    Arc::new(tokio::sync::RwLock::new(None))
}

/// Pull `n` samples out of the engine's playback ring the way a device would.
async fn drain(engine: &SpeechEngine, n: usize) -> Vec<f32> {
    let playback = engine
        .playback_handle()
        .await
        .expect("the engine opened a pipeline");
    let mut out = Vec::with_capacity(n);
    let mut buf = vec![0.0_f32; 256];
    while out.len() < n {
        playback.render(&mut buf);
        out.extend_from_slice(&buf);
    }
    out.truncate(n);
    out
}

#[tokio::test]
async fn speak_synthesizes_and_queues_audio_for_playback() {
    let Some(model) = loaded_model() else {
        eprintln!("skipping: mock component not built (run `just build-mock-wasm-backend`)");
        return;
    };
    let engine = SpeechEngine::detached(DEVICE);

    let utterance = engine
        .speak(&model, "hello there", None, None, None, None)
        .await
        .expect("speak should succeed");

    assert!(
        utterance.id.starts_with("utt_"),
        "an utterance id is returned so a client can cancel it, got {}",
        utterance.id
    );
    assert_eq!(utterance.marks, 1, "the mock emits one alignment mark");
    assert_eq!(
        engine.current(),
        None,
        "the slot is released once the utterance is fully queued"
    );

    // 960 samples at 24 kHz resampled to 48 kHz is ~1920, and the ring should
    // hold about that. Exactness is the resampler's business, not this test's.
    let playback = engine.playback_handle().await.expect("pipeline opened");
    let buffered = playback.buffered();
    assert!(
        (1500..=2400).contains(&buffered),
        "expected ~1920 resampled samples queued, got {buffered}"
    );

    let out = drain(&engine, buffered).await;
    assert!(
        out.iter().any(|s| s.abs() > 0.001),
        "the queued audio must be the mock's ramp, not silence"
    );
    assert!(
        out.iter().all(|s| s.is_finite() && s.abs() <= 1.0),
        "every sample stays finite and in range"
    );
}

/// The mock's ramp is monotonically rising, so the queued audio must rise too.
/// A decoder that dropped a frame, misordered chunks, or lost the bytes
/// straddling a frame boundary would break monotonicity even though the audio
/// would still "sound like something".
#[tokio::test]
async fn the_queued_audio_preserves_the_backends_ramp() {
    let Some(model) = loaded_model() else {
        eprintln!("skipping: mock component not built");
        return;
    };
    let engine = SpeechEngine::detached(DeviceFormat {
        // Match the mock's rate so no resampling smooths over an ordering bug.
        sample_rate: MOCK_RATE,
        channels: 1,
    });

    engine
        .speak(&model, "ramp", None, None, None, None)
        .await
        .expect("speak");

    let playback = engine.playback_handle().await.expect("pipeline opened");
    assert_eq!(
        playback.buffered(),
        MOCK_SAMPLES,
        "at a matching rate every sample arrives exactly once"
    );
    let out = drain(&engine, MOCK_SAMPLES).await;

    // The seam fade attenuates both ends, so check the middle, which is
    // untouched and must be strictly rising.
    let mid = &out[200..MOCK_SAMPLES - 200];
    for w in mid.windows(2) {
        assert!(
            w[1] > w[0],
            "the ramp must stay monotonic through the pipeline: {} then {}",
            w[0],
            w[1]
        );
    }
}

#[tokio::test]
async fn speaking_without_a_model_is_a_coded_refusal() {
    let engine = SpeechEngine::detached(DEVICE);
    let err = engine
        .speak(&empty_model(), "hello", None, None, None, None)
        .await
        .expect_err("no model is loaded");
    assert!(matches!(err, SpeakError::NotLoaded));
    assert_eq!(
        engine.current(),
        None,
        "a refused request claims no utterance slot"
    );
}

#[tokio::test]
async fn empty_and_oversized_text_are_refused_before_the_backend_is_touched() {
    let Some(model) = loaded_model() else {
        eprintln!("skipping: mock component not built");
        return;
    };
    let engine = SpeechEngine::detached(DEVICE);

    for blank in ["", "   ", "\n\t "] {
        let err = engine
            .speak(&model, blank, None, None, None, None)
            .await
            .expect_err("blank text is refused");
        assert!(matches!(err, SpeakError::EmptyText), "for {blank:?}");
    }

    let huge = "a".repeat(super_tts_daemon::daemon::speech::MAX_TEXT_CHARS + 1);
    let err = engine
        .speak(&model, &huge, None, None, None, None)
        .await
        .expect_err("oversized text is refused");
    assert!(matches!(err, SpeakError::TextTooLong));

    assert!(
        engine.playback_handle().await.is_none(),
        "a refused request must not even open the audio device"
    );
}

#[tokio::test]
async fn cancel_clears_the_queued_audio() {
    let Some(model) = loaded_model() else {
        eprintln!("skipping: mock component not built");
        return;
    };
    let engine = SpeechEngine::detached(DEVICE);
    engine
        .speak(&model, "hello", None, None, None, None)
        .await
        .expect("speak");
    let playback = engine.playback_handle().await.expect("pipeline opened");
    assert!(playback.buffered() > 0);

    engine.cancel().await;
    assert_eq!(
        playback.buffered(),
        0,
        "cancel drops queued audio rather than playing it out"
    );

    let mut buf = vec![0.0_f32; 512];
    playback.render(&mut buf);
    assert!(
        buf.iter().all(|s| *s == 0.0),
        "nothing queued before the cancel is still heard"
    );
}

/// Cancelling when nothing is speaking is a no-op, not an error: a client
/// bound to a keypress should not have to know whether it won the race.
#[tokio::test]
async fn cancelling_nothing_is_harmless() {
    let engine = SpeechEngine::detached(DEVICE);
    assert_eq!(engine.cancel().await, None);
}

/// The v1 policy is newest-wins. The second utterance must not be mixed into
/// whatever the first left in the ring.
#[tokio::test]
async fn a_second_utterance_supersedes_the_first() {
    let Some(model) = loaded_model() else {
        eprintln!("skipping: mock component not built");
        return;
    };
    let engine = SpeechEngine::detached(DeviceFormat {
        sample_rate: MOCK_RATE,
        channels: 1,
    });

    let first = engine
        .speak(&model, "one", None, None, None, None)
        .await
        .expect("first speak");
    let second = engine
        .speak(&model, "two", None, None, None, None)
        .await
        .expect("second speak");

    assert_ne!(first.id, second.id, "each utterance gets its own id");
    let playback = engine.playback_handle().await.expect("pipeline opened");
    assert_eq!(
        playback.buffered(),
        MOCK_SAMPLES,
        "the ring holds exactly the second utterance, not both concatenated"
    );
}
