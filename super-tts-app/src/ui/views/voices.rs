// SPDX-License-Identifier: GPL-3.0-only
//! The Voices page: the cloned-voice library, and the recording or import that
//! adds to it.
//!
//! The page is readable and editable whatever model is loaded. A voice is
//! stored independently of any model — the same recording is handed to
//! whichever model is loaded next — so gating the library on the active model
//! would hide the user's own recordings behind a setting they may be about to
//! change. What the loaded model *can do* with them is stated at the top
//! instead, and it is the only thing that greys out a control.

use cosmic::Element;
use cosmic::iced::widget::{column, row};
use cosmic::iced::{Alignment, Length};
use cosmic::widget::{self, button, settings, text};
use super_tts_shared::models::voices::VoiceInfo;

use super::common::{error_banner, page_layout};
use crate::core::app::AppModel;
use crate::state::voices::VoicesState;
use crate::ui::messages::{Message, VoicesMessage};

/// Voices page.
pub fn page(app: &AppModel) -> Element<'_, Message> {
    let state = &app.voices;
    let mut blocks = Vec::new();
    if let Some(message) = app.action_error_for(crate::state::ErrorScope::Voices) {
        blocks.push(error_banner(message));
    }
    blocks.push(capability_section(state));
    blocks.push(add_section(state));
    blocks.push(library_section(state));
    page_layout("Voices", settings::view_column(blocks))
}

/// What the loaded model can do with these voices.
///
/// Stated once, at the top, rather than repeated as a disabled tooltip on
/// every row: the answer is a property of the loaded model, not of any one
/// voice.
fn capability_section(state: &VoicesState) -> Element<'_, Message> {
    let (status, detail) = match state.model.as_ref() {
        None => (
            "No model loaded".to_string(),
            "Voices you add are kept. Load a model that clones to speak in them.".to_string(),
        ),
        Some(model) if !model.clones => (
            format!("{} does not clone", model.name),
            "This model speaks only in its own preset voices. Your saved voices are untouched \
             and become available when a cloning model is loaded."
                .to_string(),
        ),
        Some(model) => {
            let budget = model.clone_ref_seconds.map_or_else(
                || "any length of reference audio".to_string(),
                |s| format!("up to {s:.0} seconds of reference audio"),
            );
            let transcript = if model.needs_transcript {
                " It also needs the transcript of what the recording says."
            } else {
                ""
            };
            (
                format!("{} clones voices", model.name),
                format!("It uses {budget} from each recording.{transcript}"),
            )
        }
    };

    settings::section()
        .title("Active model")
        .add(note(status, detail))
        .into()
}

/// A statement plus its explanation, as a section row.
///
/// `settings::item::builder` needs a control to become a widget, and these
/// rows have nothing to operate — they are the page telling the user where
/// they stand.
fn note(heading: String, detail: String) -> Element<'static, Message> {
    column![text::heading(heading), text::caption(detail)]
        .spacing(cosmic::theme::spacing().space_xxxs)
        .width(Length::Fill)
        .into()
}

/// The add-a-voice section: recording, the pending sample, or the two ways to
/// start.
fn add_section(state: &VoicesState) -> Element<'_, Message> {
    if state.is_recording() {
        return recording_section(state);
    }
    if state.pending.is_some() {
        return pending_section(state);
    }

    let controls = row![
        button::suggested("Record").on_press(Message::Voices(VoicesMessage::StartRecording)),
        button::standard("Import a WAV file…").on_press(Message::Voices(VoicesMessage::ImportFile)),
    ]
    .spacing(cosmic::theme::spacing().space_xs)
    .align_y(Alignment::Center);

    settings::section()
        .title("Add a voice")
        .add(
            settings::item::builder("New recording")
                .description(
                    "Read a couple of sentences in a normal speaking voice, somewhere quiet. \
                     Recording stops on its own after two minutes.",
                )
                .control(controls),
        )
        .into()
}

/// While the microphone is open: elapsed time, the input level, and Stop.
fn recording_section(state: &VoicesState) -> Element<'_, Message> {
    let elapsed = state.recorder.as_ref().map_or_else(
        || format!("{:.0} s", state.recording_seconds),
        |r| {
            format!(
                "{:.0} s of {:.0} s",
                state.recording_seconds,
                r.max_seconds()
            )
        },
    );
    let meter = row![
        button::destructive("Stop").on_press(Message::Voices(VoicesMessage::StopRecording)),
        // A floor under any non-zero level so a quiet room still shows the
        // meter moving — an input that reads as flat zero looks like a dead
        // microphone, which is the one thing this control exists to disprove.
        widget::determinate_linear(state.recording_level.max(if state.recording_level > 0.0 {
            0.05
        } else {
            0.0
        }))
        .width(Length::Fill),
    ]
    .spacing(cosmic::theme::spacing().space_xs)
    .align_y(Alignment::Center);

    settings::section()
        .title("Recording")
        .add(settings::item("Elapsed", text::body(elapsed)))
        .add(settings::flex_item("Input level", meter))
        .into()
}

/// A captured or imported sample, waiting to be named and saved.
fn pending_section(state: &VoicesState) -> Element<'_, Message> {
    let Some(pending) = state.pending.as_ref() else {
        // Unreachable: `add_section` checks before calling. An empty section
        // beats a panic in a view.
        return settings::section().title("Add a voice").into();
    };

    let summary = if pending.seconds > 0.0 {
        format!("{} · {:.1} s", pending.origin.describe(), pending.seconds)
    } else {
        pending.origin.describe()
    };

    let label_input = widget::text_input("Name this voice", &state.label_input)
        .on_input(|s| Message::Voices(VoicesMessage::LabelChanged(s)))
        .width(Length::Fill);

    let transcript_hint = if state.needs_transcript() {
        "Required by the loaded model: type exactly what the recording says."
    } else {
        "Optional. Some models clone from the words as well as the sound — \
         adding it now means the voice works with those too."
    };
    let transcript_input = widget::text_input("What the recording says", &state.transcript_input)
        .on_input(|s| Message::Voices(VoicesMessage::TranscriptChanged(s)))
        .width(Length::Fill);

    let mut save = button::suggested(if state.saving {
        "Saving…"
    } else {
        "Save voice"
    });
    if state.can_save() {
        save = save.on_press(Message::Voices(VoicesMessage::Save));
    }
    let mut discard = button::destructive("Discard");
    if !state.saving {
        discard = discard.on_press(Message::Voices(VoicesMessage::DiscardPending));
    }
    let actions = row![save, discard]
        .spacing(cosmic::theme::spacing().space_xs)
        .align_y(Alignment::Center);

    settings::section()
        .title("New voice")
        .add(settings::item("Sample", text::body(summary)))
        .add(settings::flex_item("Name", label_input))
        .add(
            settings::item::builder("Transcript")
                .description(transcript_hint)
                .control(transcript_input),
        )
        .add(settings::flex_item("", actions))
        .into()
}

/// The stored library, one row per voice.
fn library_section(state: &VoicesState) -> Element<'_, Message> {
    let mut section = settings::section().title("Saved voices");

    if !state.loaded {
        return section
            .add(settings::item("", text::body("Reading the library…")))
            .into();
    }
    if state.voices.is_empty() {
        return section
            .add(note(
                "No voices yet".to_string(),
                "Record or import a sample above, and it appears here with the id you pass \
                 to any app that speaks through Super TTS."
                    .to_string(),
            ))
            .into();
    }

    for voice in &state.voices {
        section = section.add(voice_row(voice, state));
    }
    section.into()
}

/// One saved voice: its facts, and what can be done to it.
fn voice_row<'a>(voice: &'a VoiceInfo, state: &'a VoicesState) -> Element<'a, Message> {
    let renaming = state
        .renaming
        .as_ref()
        .filter(|(id, _)| id == &voice.id)
        .map(|(_, label)| label);
    let deleting = state.deleting.iter().any(|id| id == &voice.id);
    let spacing = cosmic::theme::spacing();

    if let Some(label) = renaming {
        let field = widget::text_input("Name this voice", label)
            .on_input(|s| Message::Voices(VoicesMessage::RenameChanged(s)))
            .on_submit(|_| Message::Voices(VoicesMessage::CommitRename))
            .width(Length::Fill);
        let actions = row![
            button::suggested("Save").on_press(Message::Voices(VoicesMessage::CommitRename)),
            button::standard("Cancel").on_press(Message::Voices(VoicesMessage::CancelRename)),
        ]
        .spacing(spacing.space_xs)
        .align_y(Alignment::Center);
        return settings::flex_item(
            "",
            column![field, actions]
                .spacing(spacing.space_xs)
                .width(Length::Fill),
        )
        .into();
    }

    let mut actions = widget::row::with_capacity(3)
        .spacing(spacing.space_xs)
        .align_y(Alignment::Center);
    if state.model_clones() {
        // Only offered when the loaded model can actually speak it; the
        // alternative is a button whose every press is an error banner.
        actions = actions.push(button::standard("Preview").on_press(Message::Voices(
            VoicesMessage::Preview(voice.voice_id.clone()),
        )));
    }
    if !deleting {
        actions = actions.push(button::standard("Rename").on_press(Message::Voices(
            VoicesMessage::BeginRename {
                id: voice.id.clone(),
                label: voice.label.clone(),
            },
        )));
        actions = actions.push(
            button::destructive("Delete")
                .on_press(Message::Voices(VoicesMessage::Delete(voice.id.clone()))),
        );
    }

    settings::item::builder(voice.label.clone())
        .description(describe(voice, deleting))
        .control(actions)
        .into()
}

/// The facts under a voice's name: how long the recording is, whether it
/// carries a transcript, and the id an app would use.
fn describe(voice: &VoiceInfo, deleting: bool) -> String {
    if deleting {
        return "Deleting…".to_string();
    }
    let transcript = if voice.transcript.is_some() {
        "with transcript"
    } else {
        "no transcript"
    };
    format!(
        "{:.1} s · {transcript} · {}",
        voice.duration_seconds, voice.voice_id
    )
}
