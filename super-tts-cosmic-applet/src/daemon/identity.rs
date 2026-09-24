// SPDX-License-Identifier: GPL-3.0-only

//! The applet's single daemon identity: the session `AppId`, the human-facing
//! app name, the requested scopes, and the SSE topics. Both the `/ping`
//! liveness client and the `/events` subscription must present the SAME
//! `AppId` + scopes so they share one cached widget-scope token; keeping them
//! here (instead of a copy in each) makes that invariant a fact rather than an
//! eyeball check (App-let Tier 3 #19). The two copies previously even disagreed
//! on the display name.

use super_tts_shared::daemon::session::AppId;

/// Stable identity caching the applet's widget-scope session token under
/// `(super-tts-session, super-tts-cosmic-applet)`.
pub const APP_ID: AppId = super_tts_shared::daemon::session::app_id("super-tts-cosmic-applet");

/// Human-facing name shown in the daemon's consent prompt.
pub const APP_NAME: &str = "Super TTS COSMIC Applet";

/// Scopes the applet requests: `playback_events` for the speaking indicator,
/// `audio_visualization` for the frequency-band meter.
pub const SCOPES: &[&str] = &["playback_events", "audio_visualization"];

/// `/events` SSE topics the applet subscribes to.
///
/// `speech_progress` is here for one transition: an utterance is accepted
/// before any audio exists for it, and with a remote backend that gap is long
/// enough to look like a hang. `spoken_ms > 0` is the daemon saying the first
/// samples reached the device, which is what turns the working animation off.
pub const TOPICS: &[&str] = &["speaking_state", "speech_progress", "frequency_bands"];
