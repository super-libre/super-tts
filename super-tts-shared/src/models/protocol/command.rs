// SPDX-License-Identifier: GPL-3.0-only

#[derive(Debug)]
pub enum Command {
    /// Synthesize `text` with the active model and play it. Cancels whatever
    /// is currently speaking — see `daemon::speech` for the one-at-a-time
    /// policy and why the utterance id exists from the start.
    Speak {
        text: String,
        /// A `voice` id the model declares, or `None` for its `default_voice`.
        voice: Option<String>,
        /// Optional per-request language override (BCP-47).
        language: Option<String>,
        /// Rate multiplier; backends that cannot vary rate ignore it.
        speed: Option<f32>,
        /// Free-text delivery guidance for models that accept it.
        instructions: Option<String>,
    },
    /// Stop the current utterance and discard queued audio.
    StopSpeaking,
    Ping {
        client_id: Option<String>,
    },
    Status,
    SetAudioTheme {
        theme: String,
    },
    GetAudioTheme,
    TestAudioTheme,
    SetModel {
        model: String,
        /// Backend repo id that serves the model (e.g.
        /// `github.com/super-tts/openai`). Empty selects the first backend
        /// serving `model`.
        source: String,
    },
    GetModel,
    ListModels,
    SetDevice {
        device: String, // "cpu" or "cuda"
    },
    GetDevice,
    GetConfig,
    CancelDownload,
    GetDownloadStatus,
    ListAudioThemes,
    /// The raw wire string, unparsed. `handle_set_notification_method` parses
    /// it (mirrors `SetAudioTheme`) so an unrecognized value can be rejected
    /// with a classified `error_code` (400), not just a bare error string.
    SetNotificationMethod {
        method: String,
    },
    GetNotificationMethod,
    SetUpdateCheckEnabled {
        enabled: bool,
    },
    GetUpdateCheckEnabled,
    /// The raw wire string, unparsed; `handle_set_update_beta_optin` parses it
    /// (mirrors `SetNotificationMethod`).
    SetUpdateBetaOptin {
        value: String,
    },
    GetUpdateBetaOptin,
    SetVolume {
        volume: u8,
    },
    GetVolume,
    SetPrimaryLanguage {
        language: String,
    },
    GetPrimaryLanguage,
    ClearPrimaryLanguage,
    /// Set the per-model language override for a specific `(source, model)`.
    SetModelLanguage {
        source: String,
        model: String,
        language: String,
    },
    /// Read the resolved language block for a specific `(source, model)`.
    GetModelLanguage {
        source: String,
        model: String,
    },
    /// Clear the per-model language override for a specific `(source, model)`.
    ClearModelLanguage {
        source: String,
        model: String,
    },
    SetAllowOnlineModels {
        enabled: bool,
    },
    GetAllowOnlineModels,
    SetCustomModelsDir {
        path: Option<String>,
    },
    GetCustomModelsDir,
    /// List installed backends with their models, secrets, and options.
    ListBackends,
    /// Re-instantiate the active model in place to apply changed secrets/options.
    ReloadActiveModel,
    /// Unload the currently loaded model. The active backend stays selected
    /// so the user can pick another model from it; to fully idle out, clear
    /// the active backend instead.
    UnloadActiveModel,
    /// Set or clear (empty value) one backend's option override.
    SetBackendOption {
        source: String,
        name: String,
        value: String,
    },
    /// Select the active backend. Does not load a model.
    SetActiveBackend {
        source: String,
    },
    /// Get the active backend.
    GetActiveBackend,
    /// Clear the active backend → unload any model, daemon idle.
    ClearActiveBackend,
    /// Read-only GPU inventory + memory. See `docs/protocol/endpoints/v1/gpu_info.md`.
    GetGpuInfo,
}
