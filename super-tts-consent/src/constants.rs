// SPDX-License-Identifier: GPL-3.0-only
//! Static content shown in the consent popup.
//!
//! Each `*_PERMISSIONS` array is the bullet list for one scope. A token
//! can carry several scopes, so the popup renders the union of these for
//! every scope the requesting app asked for. Edit the strings here to
//! change what the user is told they're approving — keep each entry
//! concise (≤ one wrapped line on a typical screen) and user-meaningful.
//! These are what the user reads in the dialog, not a developer reference.

/// Bullets for the `speak` scope.
///
/// Phrased around what the user gives up — control of the speakers — rather
/// than what the app gains. Interrupting is called out because it is the part
/// that surprises people: granting this lets the app cut off whatever else is
/// being read aloud.
pub const SPEAK_PERMISSIONS: &[&str] = &[
    "Play synthesized speech through your speakers",
    "Interrupt or stop speech this or another app started",
];

/// Bullets for the `playback_events` scope.
///
/// Reading these events reveals *when* the machine is speaking and how far
/// through it is — not the text — so the bullets say exactly that rather than
/// implying access to content the scope does not grant.
pub const PLAYBACK_EVENTS_PERMISSIONS: &[&str] = &[
    "See when speech starts and stops",
    "See how far through the current speech playback has got",
];

/// Bullets for the `status` scope.
pub const STATUS_PERMISSIONS: &[&str] = &[
    "Read which voice model and device are currently active",
    "See whether the machine is speaking right now",
];

/// Bullets for the `settings` scope.
pub const SETTINGS_PERMISSIONS: &[&str] = &[
    "Read and change every daemon setting (model, device, audio cues, volume)",
    "Allow or block sending text to online providers",
    "Install, update, and remove voice backends",
];

/// Bullets for the `audio_visualization` scope.
pub const AUDIO_VISUALIZATION_PERMISSIONS: &[&str] =
    &["Receive audio visualization data (frequency bars) while speech is playing"];

/// Bullets for the `daemon_status` scope.
pub const DAEMON_STATUS_PERMISSIONS: &[&str] =
    &["Monitor model changes, downloads, and backend installation progress"];

/// Bullets for the `secrets` scope shown in the consent popup.
pub const SECRETS_PERMISSIONS: &[&str] = &[
    "Store, update, and clear this backend's API credentials",
    "Cannot read or display any stored credential value",
];

/// Fallback bullets shown if the daemon spawns the popup with a scope
/// the helper doesn't recognize. Should never appear in production.
pub const UNKNOWN_SCOPE_PERMISSIONS: &[&str] = &[
    "Unknown scope — the requesting app sent something the daemon doesn't recognize. Denying is safe.",
];
