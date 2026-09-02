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
        voices: voices.iter().map(|v| (*v).to_string()).collect(),
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
