// SPDX-License-Identifier: GPL-3.0-only
/// What the daemon is doing with an utterance, as the applet sees it.
///
/// Three states, not two, because accepting an utterance and producing audio
/// for it are separated by however long the backend takes — a remote API can
/// leave that gap seconds wide, and a widget that jumps straight to a flat
/// visualizer looks broken. [`Self::Synthesizing`] is that gap.
#[derive(Debug, Clone)]
pub enum SpeechState {
    Idle,
    Synthesizing,
    Speaking,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonConnectionState {
    Connecting,
    Connected,
    Error(String),
    /// User denied the consent prompt (or the daemon's sticky deny
    /// cache short-circuited a fresh request). The widget
    /// subscription has terminated to avoid spamming retries. The
    /// applet UI shows a hint to restart the daemon and a button
    /// that triggers `Message::RetryAuthorization`.
    Blocked(String),
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum IsOpen {
    None,
    VisualizationTheme,
    WorkingAnimation,
    VisualizationColors,
}
