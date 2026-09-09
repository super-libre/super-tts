// SPDX-License-Identifier: GPL-3.0-only
//! Unit tests for the speak path's request validation.

use super::{SpeakError, check_voice};
use crate::tts_models::ModelDefinition;
use std::time::Duration;
use super_tts_registry_types::manifest::{Device, VoiceKind};

/// A model declaring `kinds` and the preset ids in `voices`.
fn model(kinds: Vec<VoiceKind>, voices: &[&str]) -> ModelDefinition {
    ModelDefinition {
        name: "kokoro-82m".into(),
        source: "github.com/jorge-menjivar/super-tts-kokoro".into(),
        is_multilingual: true,
        primary_language: "en".into(),
        supported_languages: vec!["en".into(), "ja".into()],
        estimated_vram_bytes: 0,
        max_input_chars: None,
        processing_interval: Duration::from_millis(0),
        supported_devices: vec![Device::Cpu],
        voice_kinds: kinds,
        clone_ref_seconds: None,
        clone_needs_transcript: false,
        voices: voices
            .iter()
            .map(|v| crate::tts_models::model_definition::PresetVoice {
                id: (*v).to_string(),
                label: (*v).to_string(),
            })
            .collect(),
        default_voice: None,
        realtime: false,
        provider: None,
    }
}

#[test]
fn a_declared_preset_is_accepted() {
    let m = model(vec![VoiceKind::Preset], &["af_heart", "jf_alpha"]);
    assert!(check_voice(&m, "af_heart").is_ok());
    assert!(check_voice(&m, "jf_alpha").is_ok());
}

/// The gap this check exists to close: a manifest that ships a voice pack
/// without declaring it left the id reaching the backend unchecked.
#[test]
fn a_preset_the_model_does_not_declare_is_refused() {
    let m = model(vec![VoiceKind::Preset], &["af_heart"]);
    let err = check_voice(&m, "jf_alpha").expect_err("undeclared voice is refused");
    assert!(matches!(err, SpeakError::UnknownVoice(_)));
    assert!(
        err.to_string().contains("jf_alpha"),
        "the message names the voice asked for: {err}"
    );
}

/// An empty `voices` list is what the manifest uses for a model whose voices
/// are all cloned or described. There is nothing to check a preset against,
/// so it passes rather than being refused out of hand.
#[test]
fn a_model_declaring_no_presets_accepts_any_preset_id() {
    let m = model(vec![VoiceKind::Preset], &[]);
    assert!(check_voice(&m, "anything").is_ok());
}

#[test]
fn an_id_shape_the_model_did_not_opt_into_is_refused() {
    let m = model(vec![VoiceKind::Preset], &["af_heart"]);
    for id in ["voice:6f1b2c3d", "desc:a calm narrator"] {
        let err = check_voice(&m, id).expect_err("shape not opted into is refused");
        assert!(matches!(err, SpeakError::UnknownVoice(_)), "{id}");
    }
}

#[test]
fn opted_into_shapes_are_accepted_and_not_checked_against_presets() {
    let m = model(
        vec![VoiceKind::Preset, VoiceKind::Cloned, VoiceKind::Described],
        &["af_heart"],
    );
    // Neither id names a preset, and neither has to: only a bare id does.
    assert!(check_voice(&m, "voice:6f1b2c3d").is_ok());
    assert!(check_voice(&m, "desc:a calm narrator").is_ok());
    assert!(check_voice(&m, "af_heart").is_ok());
    assert!(check_voice(&m, "af_undeclared").is_err());
}

/// A cloned-only model refuses the bare ids it cannot resolve, which is the
/// same rule read from the other side.
#[test]
fn a_cloned_only_model_refuses_a_bare_id() {
    let m = model(vec![VoiceKind::Cloned], &[]);
    assert!(check_voice(&m, "af_heart").is_err());
    assert!(check_voice(&m, "voice:6f1b2c3d").is_ok());
}

// --- Cloned-voice registration ---------------------------------------------

use crate::daemon::speech::SpeechEngine;
use crate::daemon::types::LoadedModel;
use crate::tts_models::synthesize::{ModelInfo, ModelInfoData, ModelState, Synthesize};
use crate::tts_models::v1::{RegisterVoiceRequest, SynthesisSink, SynthesizeRequest};
use crate::voices::VoiceLibrary;
use std::sync::{Arc, Mutex};

/// What one `register_voice` call carried.
#[derive(Debug, Clone, PartialEq)]
struct Registration {
    voice: String,
    transcript: Option<String>,
    pcm_bytes: usize,
}

/// Shared with the test, so what the backend was handed can be read back
/// through the `Box<dyn Synthesize>` the daemon holds it as.
#[derive(Debug, Default)]
struct Log {
    registered: Mutex<Vec<Registration>>,
    released: Mutex<Vec<String>>,
}

/// A backend that records registrations instead of synthesizing anything.
struct RecordingBackend {
    info: ModelInfoData,
    log: Arc<Log>,
}

impl ModelInfo for RecordingBackend {
    fn info(&self) -> &ModelInfoData {
        &self.info
    }
}

impl ModelState for RecordingBackend {
    fn device(&self) -> String {
        "cpu".to_string()
    }
}

#[async_trait::async_trait]
impl Synthesize for RecordingBackend {
    async fn synthesize(
        &self,
        _request: &SynthesizeRequest<'_>,
        _sink: &mut (dyn SynthesisSink + Send),
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn register_voice(&self, request: &RegisterVoiceRequest<'_>) -> anyhow::Result<()> {
        self.log.registered.lock().unwrap().push(Registration {
            voice: request.voice.to_string(),
            transcript: request.transcript.map(str::to_owned),
            pcm_bytes: request.pcm.len(),
        });
        Ok(())
    }

    async fn unregister_voice(&self, voice: &str) -> anyhow::Result<()> {
        self.log.released.lock().unwrap().push(voice.to_string());
        Ok(())
    }
}

/// A mono 16-bit WAV of `seconds` at the canonical rate.
fn clip(seconds: f32) -> Vec<u8> {
    let frames = crate::num_cast::f32_to_usize(
        seconds * crate::num_cast::u32_to_f32(crate::voices::CLIP_SAMPLE_RATE),
    );
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: crate::voices::CLIP_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut w = hound::WavWriter::new(&mut cursor, spec).unwrap();
        for i in 0..frames {
            w.write_sample(i16::try_from(i % 500).unwrap()).unwrap();
        }
        w.finalize().unwrap();
    }
    cursor.into_inner()
}

/// A model that clones, with `budget` seconds of reference audio and an
/// optional transcript requirement.
fn cloning_model(budget: f32, needs_transcript: bool) -> ModelDefinition {
    let mut def = model(vec![VoiceKind::Preset, VoiceKind::Cloned], &["af_heart"]);
    def.clone_ref_seconds = Some(budget);
    def.clone_needs_transcript = needs_transcript;
    def
}

/// A loaded model backed by the recording backend, plus the log to inspect.
fn loaded(definition: ModelDefinition) -> (LoadedModel, Arc<Log>) {
    let log = Arc::new(Log::default());
    let backend = RecordingBackend {
        info: ModelInfoData::new(
            definition.name.clone(),
            definition.source.clone(),
            true,
            false,
            Duration::from_millis(0),
        ),
        log: Arc::clone(&log),
    };
    (LoadedModel::new(definition, Box::new(backend)), log)
}

/// A detached engine reading voices from a library under `dir`.
fn engine(dir: &std::path::Path) -> (SpeechEngine, Arc<VoiceLibrary>) {
    let library = Arc::new(VoiceLibrary::new(dir.join("voices")));
    let engine = SpeechEngine::detached(crate::audio::playback::DeviceFormat {
        sample_rate: 24_000,
        channels: 1,
    })
    .with_voices(Arc::clone(&library));
    (engine, library)
}

/// The clip is pushed once, trimmed to what the model declared, and the second
/// utterance costs nothing — which is the whole reason registration is a
/// separate route from synthesis.
#[tokio::test]
async fn registers_a_cloned_voice_once_and_trims_it_to_the_model_s_budget() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, library) = engine(dir.path());
    let voice = library
        .create(&clip(6.0), "Ada", Some("the reference sentence"))
        .expect("stores the clip");
    let (model, log) = loaded(cloning_model(2.0, false));

    engine
        .ensure_cloned_voice(&model, &voice.voice_id())
        .await
        .expect("registers");

    let registered = log.registered.lock().unwrap().clone();
    assert_eq!(registered.len(), 1, "one registration: {registered:?}");
    assert_eq!(registered[0].voice, voice.voice_id());
    assert_eq!(
        registered[0].transcript.as_deref(),
        Some("the reference sentence"),
        "the transcript rides along even when the model did not demand it"
    );
    assert_eq!(
        registered[0].pcm_bytes,
        2 * 2 * crate::voices::CLIP_SAMPLE_RATE as usize,
        "two seconds of mono s16 at the canonical rate, not the six stored"
    );
    assert!(
        model.cloned_voices.lock().contains(&voice.voice_id()),
        "the instance remembers what it was given"
    );

    engine
        .ensure_cloned_voice(&model, &voice.voice_id())
        .await
        .expect("second use is a no-op");
    assert_eq!(
        log.registered.lock().unwrap().len(),
        1,
        "a cached voice is not pushed again"
    );
}

/// A preset id never touches the library — the daemon must not require one to
/// exist for models that do not clone.
#[tokio::test]
async fn a_preset_voice_never_reaches_the_library() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, _library) = engine(dir.path());
    let (model, log) = loaded(cloning_model(10.0, false));

    engine
        .ensure_cloned_voice(&model, "af_heart")
        .await
        .expect("a preset needs no registration");
    assert!(log.registered.lock().unwrap().is_empty());
}

/// A `voice:<uuid>` the library never stored is bad input, not a synthesis
/// failure: it must not reach the backend or raise a desktop notice.
#[tokio::test]
async fn a_voice_the_library_does_not_have_is_refused_as_bad_input() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, _library) = engine(dir.path());
    let (model, log) = loaded(cloning_model(10.0, false));

    let err = engine
        .ensure_cloned_voice(&model, "voice:2f8a2d0e-0000-4000-8000-000000000000")
        .await
        .expect_err("an unknown voice is refused");
    assert!(
        matches!(err, SpeakError::UnknownVoice(_)),
        "expected UnknownVoice, got {err}"
    );
    assert!(log.registered.lock().unwrap().is_empty());
}

/// In-context cloning needs the words as well as the audio. Refusing when the
/// voice was stored without them says so once, at the point the pair is
/// assembled, instead of failing every request that follows.
#[tokio::test]
async fn a_model_that_clones_in_context_refuses_a_clip_with_no_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, library) = engine(dir.path());
    let silent = library.create(&clip(3.0), "No words", None).unwrap();
    let spoken = library
        .create(&clip(3.0), "With words", Some("hello"))
        .unwrap();
    let (model, log) = loaded(cloning_model(10.0, true));

    let err = engine
        .ensure_cloned_voice(&model, &silent.voice_id())
        .await
        .expect_err("a transcript-less clip is refused");
    assert!(matches!(err, SpeakError::UnknownVoice(_)), "got {err}");
    assert!(
        err.to_string().contains("No words"),
        "the message names the voice the user has to fix: {err}"
    );
    assert!(log.registered.lock().unwrap().is_empty());

    engine
        .ensure_cloned_voice(&model, &spoken.voice_id())
        .await
        .expect("the same model accepts a clip that has one");
    assert_eq!(log.registered.lock().unwrap().len(), 1);
}

/// A daemon built without a library cannot resolve a cloned id, and says so
/// rather than pushing an empty clip.
#[tokio::test]
async fn an_engine_with_no_library_refuses_a_cloned_id() {
    let engine = SpeechEngine::detached(crate::audio::playback::DeviceFormat {
        sample_rate: 24_000,
        channels: 1,
    });
    let (model, log) = loaded(cloning_model(10.0, false));

    let err = engine
        .ensure_cloned_voice(&model, "voice:2f8a2d0e-0000-4000-8000-000000000000")
        .await
        .expect_err("no library, no cloned voice");
    assert!(matches!(err, SpeakError::UnknownVoice(_)), "got {err}");
    assert!(log.registered.lock().unwrap().is_empty());
}
