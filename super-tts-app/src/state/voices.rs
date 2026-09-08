// SPDX-License-Identifier: GPL-3.0-only
//! Voices-page state: the library, what the loaded model can do with it, and
//! the half-finished sample the user is assembling.

use super_tts_shared::models::voices::{VoiceInfo, VoiceModelSupport};

/// The sample waiting to be named and saved.
///
/// Recording and importing converge here, so everything after the sample is
/// obtained — naming it, adding a transcript, saving it — is one path with one
/// set of validation rules.
#[derive(Debug, Clone)]
pub struct PendingSample {
    /// A complete WAV file, ready to upload.
    pub wav: Vec<u8>,
    /// Length in seconds, for the "18.4 s captured" readout.
    pub seconds: f32,
    /// Where it came from, which is the only thing the two paths still differ
    /// on once the bytes exist.
    pub origin: SampleOrigin,
}

/// How a pending sample was obtained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleOrigin {
    /// Captured from the microphone in this window.
    Recorded,
    /// Imported from a file, named here so the user can tell two samples apart
    /// before either is saved.
    Imported(String),
}

impl SampleOrigin {
    /// A short label for the pending-sample card.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Recorded => "Recorded just now".to_string(),
            Self::Imported(name) => format!("Imported from {name}"),
        }
    }
}

/// Everything the Voices page renders and edits.
#[derive(Debug, Default)]
pub struct VoicesState {
    /// The library, newest first.
    pub voices: Vec<VoiceInfo>,
    /// What the loaded model can do with a cloned voice; `None` while nothing
    /// is loaded.
    pub model: Option<VoiceModelSupport>,
    /// True until the first listing arrives, so an empty library and a library
    /// that has not been read yet do not look the same.
    pub loaded: bool,
    /// A capture in progress. Its presence *is* the recording state — there is
    /// no separate flag to keep in step with it.
    pub recorder: Option<crate::audio::recorder::Recorder>,
    /// Seconds captured so far, refreshed by the recording tick.
    pub recording_seconds: f32,
    /// Peak input level, `0.0`–`1.0`, for the meter.
    pub recording_level: f32,
    /// The sample waiting to be saved.
    pub pending: Option<PendingSample>,
    /// Label field for the pending sample.
    pub label_input: String,
    /// Transcript field for the pending sample.
    pub transcript_input: String,
    /// The upload is in flight; the Save button is spinning.
    pub saving: bool,
    /// Voice id currently being renamed, and the text being typed.
    pub renaming: Option<(String, String)>,
    /// Voice ids with a delete in flight, so the row can grey out.
    pub deleting: Vec<String>,
}

impl VoicesState {
    /// Whether a capture is running right now.
    #[must_use]
    pub fn is_recording(&self) -> bool {
        self.recorder.is_some()
    }

    /// Whether the loaded model can speak in a cloned voice.
    #[must_use]
    pub fn model_clones(&self) -> bool {
        self.model.as_ref().is_some_and(|m| m.clones)
    }

    /// Whether the loaded model needs a transcript alongside the audio, which
    /// is what turns the transcript field from optional into required.
    #[must_use]
    pub fn needs_transcript(&self) -> bool {
        self.model.as_ref().is_some_and(|m| m.needs_transcript)
    }

    /// Whether the pending sample can be saved as it stands.
    ///
    /// The transcript counts only when the loaded model needs one. The library
    /// outlives the loaded model, so a transcript-less voice is perfectly
    /// valid — it just cannot be handed to *this* model, and the button is
    /// where that is said rather than after an upload the daemon refuses.
    #[must_use]
    pub fn can_save(&self) -> bool {
        self.pending.is_some()
            && !self.saving
            && !self.label_input.trim().is_empty()
            && (!self.needs_transcript() || !self.transcript_input.trim().is_empty())
    }

    /// Drop the pending sample and the fields that describe it.
    pub fn clear_pending(&mut self) {
        self.pending = None;
        self.label_input.clear();
        self.transcript_input.clear();
    }
}
