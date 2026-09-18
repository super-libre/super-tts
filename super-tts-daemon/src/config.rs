// SPDX-License-Identifier: GPL-3.0-only
use log::{debug, error, warn};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use super_tts_shared::models::notification_method::NotificationMethod;
use super_tts_shared::models::update_beta_optin::UpdateBetaOptIn;
use super_tts_shared::theme::AudioTheme;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    pub device: DeviceConfig,
    pub audio: AudioConfig,
    pub synthesis: SynthesisConfig,
    #[serde(default)]
    pub online: OnlineConfig,
    #[serde(default)]
    pub backends: BackendsConfig,
    #[serde(default)]
    pub update: UpdateConfig,
    #[serde(default)]
    pub http: HttpConfig,
}

/// Self-update checking. Contract: docs/protocol/endpoints/v1/update.md
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateConfig {
    /// Periodic background checks + desktop notification. `POST
    /// /v1/update/check` works regardless of this flag.
    #[serde(default = "default_check_enabled")]
    pub check_enabled: bool,
    /// An unparseable stored value degrades to the default rather than
    /// failing the whole config load.
    #[serde(
        default,
        deserialize_with = "super_tts_shared::utils::serde_helpers::deserialize_or_default"
    )]
    pub beta_optin: UpdateBetaOptIn,
}

fn default_check_enabled() -> bool {
    true
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            check_enabled: true,
            beta_optin: UpdateBetaOptIn::default(),
        }
    }
}

/// User-set configuration for installed backends. Secrets live in the keyring;
/// only non-sensitive **options** are stored here (plaintext).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BackendsConfig {
    /// Per-backend option overrides: backend `source` → (option name → value).
    /// An absent entry means "use the manifest default".
    #[serde(default)]
    pub options: HashMap<String, HashMap<String, String>>,
    /// Per-model settings: backend `source` → (model name → settings).
    #[serde(default)]
    pub models: HashMap<String, HashMap<String, ModelSettings>>,
}

/// Per-model configuration. A struct (not a bare value) so future per-model
/// settings have a home.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelSettings {
    /// Per-model language override: a BCP-47 tag, `"auto"`, or `None`
    /// (Automatic — inherit the global `primary_language`, else the model's primary).
    #[serde(default)]
    pub language: Option<String>,
    /// The device this model loads on: `"cpu"` or `"gpu"`, or `None` to use
    /// [`DeviceConfig::preferred_device`]. A device belongs to a model
    /// because it is a property of the model: a small voice runs fine on the
    /// CPU while the large one beside it needs the GPU, and one global switch
    /// forced a user who wanted the GPU for one of them to accept it for both.
    #[serde(default)]
    pub device: Option<String>,
    /// The voice this model speaks in when an utterance names none: a `voice`
    /// id in any of the three shapes `docs/protocol/backend/config.md` defines,
    /// or `None` for the model's manifest `default_voice`.
    ///
    /// Per model rather than global for the same reason the device is: a voice
    /// id means nothing to another model. `ryan` is one of the nine speakers
    /// the `CustomVoice` checkpoints declare and is not a voice any Kokoro build
    /// has, and a cloned `voice:<uuid>` is refused outright by a model whose
    /// `voice_kinds` do not include `cloned`. One global voice would be wrong
    /// for every model but the one it was picked for.
    #[serde(default)]
    pub voice: Option<String>,
}

/// The device models fall back to when they have none of their own
/// ([`ModelSettings::device`]).
///
/// Still settable through `set_device`, which is the *default* rather than any
/// one model's choice, and still what a config written before per-model
/// devices existed carries — so a user upgrading keeps loading models exactly
/// where they always loaded, without any rewrite of their config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceConfig {
    pub preferred_device: String, // "cpu" or "gpu"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    #[serde(
        default,
        deserialize_with = "super_tts_shared::utils::serde_helpers::deserialize_or_default"
    )]
    pub theme: AudioTheme,
    #[serde(default = "default_volume")]
    pub volume: u8,
}

fn default_volume() -> u8 {
    100
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OnlineConfig {
    /// Whether online models (that send audio to external APIs) are allowed.
    /// Defaults to false for privacy.
    #[serde(default)]
    pub allow_online_models: bool,
}

/// The daemon's HTTP surface beyond the Unix socket it always serves.
///
/// Contract: `docs/protocol/transport.md`. Every field is defaulted, so a
/// `daemon.toml` written before the section existed loads with the TCP
/// listener off — which is the only state in which the daemon's callers are
/// all peer-credential-verified.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct HttpConfig {
    #[serde(default)]
    pub tcp: TcpConfig,
}

/// The loopback TCP listener, which is what a browser can reach.
///
/// A browser cannot dial a Unix socket, so a web client needs this. What it
/// costs is the identity model: `SO_PEERCRED` gives the Unix socket a caller
/// identity the kernel vouches for, and TCP has no equivalent. A TCP caller is
/// identified by its `Origin` header instead — asserted by the browser, not
/// proven.
///
/// **What guards the surface is therefore consent, not this config.** Any page
/// may *ask*; the dialog names the site and the user decides. That is the same
/// bargain the Unix socket strikes with a binary, one notch weaker because the
/// name comes from the browser rather than the kernel — which is what the
/// dialog says, in those words.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TcpConfig {
    /// Whether the daemon also serves the API on `127.0.0.1:{port}`.
    ///
    /// On by default, so a web client works without the user first finding a
    /// config file. A port that cannot be bound is logged and skipped rather
    /// than fatal: the Unix socket is the daemon's primary transport, and
    /// refusing to start because something else holds this port would take the
    /// whole daemon down for the sake of an extra.
    #[serde(default = "default_tcp_enabled")]
    pub enabled: bool,
    /// The loopback port to bind.
    ///
    /// Fixed rather than OS-chosen because it is half of a browser client's
    /// origin: a port that moved on every restart would invalidate the other
    /// side's bookmarks and its stored token binding. The number itself has no
    /// registered meaning.
    #[serde(default = "default_tcp_port")]
    pub port: u16,
    /// The browser origins allowed to call the API, each a full origin such as
    /// `http://127.0.0.1:8910` — scheme, host and port, no trailing slash.
    /// [`ANY_ORIGIN`] anywhere in the list admits every origin, which is the
    /// default.
    ///
    /// Admitting an origin is permission to *ask*, not permission to act: a
    /// page still faces the consent dialog, and the token it gets is bound to
    /// its own origin and useless to any other. Narrowing this list is for
    /// deployments that want a page stopped before it can put a dialog on the
    /// user's screen at all.
    ///
    /// An empty list admits nothing — the deliberate lockdown, distinct from
    /// the default. Either way the daemon enforces this itself on every
    /// request rather than relying on the CORS headers it also sends: CORS is a
    /// browser-side courtesy that a non-browser client simply ignores, so it
    /// cannot be what guards the surface.
    #[serde(default = "default_allowed_origins")]
    pub allowed_origins: Vec<String>,
}

/// The wildcard entry for [`TcpConfig::allowed_origins`], spelled as CORS
/// spells it.
pub const ANY_ORIGIN: &str = "*";

/// See [`TcpConfig::enabled`].
fn default_tcp_enabled() -> bool {
    true
}

/// The loopback port the daemon serves on unless told otherwise.
///
/// Public because it is not only a default: it is the number a browser client
/// hard-codes, the one the published `OpenAPI` document advertises as a server,
/// and the one the protocol docs quote. Those must all move together, so they
/// all read it from here.
///
/// **7300–7309 is the block reserved for the Super family**, one port per
/// daemon: `7300` Super STT, `7301` Super TTS, the rest unused for now. A block
/// rather than a number apiece so a sibling never has to repeat this search,
/// and so a firewall rule or a document can name the whole family at once.
///
/// The band was picked for how empty it is rather than for looking nice. The
/// 7000s has no conventional block reservation — unlike the 5000s, where VNC
/// takes 5900–5999, and the 6000s, where X11 takes 6000–6063 — and 7284–7364
/// is 81 consecutive ports unassigned in IANA's registry, `genstat` at 7283
/// below and `lcm-server` at 7365 above. This block sits in the middle of that.
/// The busy parts of the 7000s are all well outside it: 7860 is Gradio's
/// default, which matters more than its absence from the registry suggests
/// since it is where ML demo apps land; 7777, 7474 and 7687 are likewise taken
/// in practice.
///
/// See [`TcpConfig::port`] for why a fixed number rather than an OS-chosen one.
pub const DEFAULT_TCP_PORT: u16 = 7301;

/// See [`DEFAULT_TCP_PORT`].
fn default_tcp_port() -> u16 {
    DEFAULT_TCP_PORT
}

/// See [`TcpConfig::allowed_origins`].
fn default_allowed_origins() -> Vec<String> {
    vec![ANY_ORIGIN.to_string()]
}

/// Written out rather than derived so a `TcpConfig::default()` and a config
/// deserialized from an absent `[http.tcp]` agree on every field. The derived
/// impl would answer `false`, `0` and `[]` — which is not merely a different
/// default but the opposite behaviour, since `0` means "let the OS choose" to
/// every bind call that saw it and `[]` admits nothing.
impl Default for TcpConfig {
    fn default() -> Self {
        Self {
            enabled: default_tcp_enabled(),
            port: default_tcp_port(),
            allowed_origins: default_allowed_origins(),
        }
    }
}

impl TcpConfig {
    /// The address to bind, or `None` when the listener is switched off.
    ///
    /// Always loopback: a listener reachable from the network would be a
    /// different product with a different threat model, and nothing in the
    /// consent flow is prepared for a caller on another host.
    #[must_use]
    pub fn bind_addr(&self) -> Option<std::net::SocketAddr> {
        self.enabled
            .then(|| std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, self.port)))
    }

    /// Whether `origin` is one the user has allowed.
    ///
    /// [`ANY_ORIGIN`] anywhere in the list admits everything. Otherwise this is
    /// an exact, case-sensitive match: origins are compared as the opaque
    /// strings browsers send rather than parsed and normalized, so there is no
    /// gap between what the user wrote down and what is accepted — a prefix or
    /// suffix match here would let `http://127.0.0.1:8910.evil.test` through.
    ///
    /// Note what this does *not* do: it never widens what a caller becomes. An
    /// admitted origin is still recorded as itself, so a wildcard list grants
    /// every page its own identity rather than a shared one.
    #[must_use]
    pub fn is_origin_allowed(&self, origin: &str) -> bool {
        self.allowed_origins
            .iter()
            .any(|a| a == ANY_ORIGIN || a == origin)
    }

    /// Whether the list is the permissive default.
    ///
    /// Only used to phrase the startup log, so an operator can see which of the
    /// two the running daemon is doing without going to read the config.
    #[must_use]
    pub fn admits_any_origin(&self) -> bool {
        self.allowed_origins.iter().any(|a| a == ANY_ORIGIN)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SynthesisConfig {
    #[serde(default)]
    pub preferred_model: String,
    /// Compatibility shim; the daemon resolves its startup model by
    /// `(preferred_model, preferred_source)` and never reads this.
    ///
    /// It is written, not merely preserved, because `daemon.toml` outlives the
    /// binary that wrote it. Daemons through v0.2.0 resolve their startup model
    /// by `(model, provider, source)`, so a user who rolls back after a bad
    /// upgrade gets an idle daemon — with no error, just a lost selection — if
    /// this key is missing *or* stale. Keeping it in step with the selected
    /// model is what makes the rollback recoverable.
    ///
    /// Delete once no supported daemon resolves by provider.
    #[serde(default)]
    pub preferred_provider: String,
    #[serde(default)]
    pub preferred_source: String,
    /// How a synthesis failure is surfaced to the user. An unparseable stored
    /// value degrades to the default rather than failing the whole config load.
    #[serde(
        default,
        deserialize_with = "super_tts_shared::utils::serde_helpers::deserialize_or_default"
    )]
    pub notification_method: NotificationMethod,
    /// Vestigial: retained for config compatibility. Custom models are now
    /// provided as backends discovered under [`backends_dir`].
    #[serde(default)]
    pub custom_models_dir: Option<String>,
    /// Directory scanned for installed backends. `None` uses the default
    /// (`<data_dir>/super-tts/backends`).
    #[serde(default)]
    pub backends_dir: Option<String>,
    /// Relative install dir (subdir of [`backends_dir`]) of the selected active
    /// backend, or `None` when idle. Metadata (name/source/models) is read from
    /// that dir's `backend.toml`. An active backend with an empty
    /// `preferred_model` means "backend selected, no model loaded".
    #[serde(default)]
    pub active_backend: Option<String>,
    /// Global default synthesis language: a BCP-47 tag, the reserved
    /// `"auto"`, or `None` (no preference; models use their `primary_language`).
    #[serde(default)]
    pub primary_language: Option<String>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            device: DeviceConfig {
                preferred_device: "cpu".to_string(), // Default to CPU for compatibility
            },
            audio: AudioConfig {
                theme: AudioTheme::default(),
                volume: default_volume(),
            },
            synthesis: SynthesisConfig {
                // Empty preference: the daemon stays idle until a model is
                // selected — it never auto-picks one, since loading a model can
                // pull gigabytes.
                preferred_model: String::new(),
                preferred_provider: String::new(),
                preferred_source: String::new(),
                notification_method: NotificationMethod::default(),
                custom_models_dir: None,
                backends_dir: None,
                active_backend: None,
                primary_language: None,
            },
            online: OnlineConfig::default(),
            backends: BackendsConfig::default(),
            update: UpdateConfig::default(),
            http: HttpConfig::default(),
        }
    }
}

/// The `daemon.toml` path used by `DaemonConfig::get_config_path()` under
/// `#[cfg(test)]`: a directory under the OS temp dir, unique per test
/// *process* (keyed by pid) and cached for the life of that process so every
/// test in the binary agrees on the same path. This is what keeps unit tests
/// that call `load()`/`save()` off the developer's real config file.
#[cfg(test)]
fn test_config_path() -> PathBuf {
    use std::sync::OnceLock;
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("super-tts-test-config-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        dir.join("daemon.toml")
    })
    .clone()
}

impl DaemonConfig {
    /// Get the config file path.
    ///
    /// Under `#[cfg(test)]` this resolves to a process-local temp file instead
    /// of the real XDG config path, so `load()`/`save()` in this crate's unit
    /// tests can never read or clobber the developer's real
    /// `~/.config/super-tts/daemon.toml`. Known limitation: this only covers
    /// unit tests compiled *into* this crate (`cfg(test)`). Integration tests
    /// under `tests/` link against the crate without `cfg(test)`, so they
    /// still resolve the real path — not addressed here.
    fn get_config_path() -> PathBuf {
        #[cfg(test)]
        {
            test_config_path()
        }
        #[cfg(not(test))]
        {
            super_tts_shared::paths::config_dir().join("daemon.toml")
        }
    }

    /// Parse config file `content` into a [`DaemonConfig`], falling back to
    /// defaults on a parse error. Pure (no I/O) so the load/reset decision is
    /// unit-testable without touching the real config path. Returns the config
    /// and whether a reset occurred (so the caller knows to persist defaults).
    ///
    /// Devices are bare `String`s, not an enum, so no `deserialize_or_default`
    /// catches a stale value at the serde layer — they are normalized here
    /// instead. Unlike the wire setters, which reject an unparseable value
    /// outright, a persisted value has no such option: a daemon that refused
    /// to start over a stale config field would be worse than one that falls
    /// back to the default, so the global default degrades to `"cpu"` and a
    /// per-model device to "none of its own" rather than triggering a full
    /// reset. The deprecated `cuda`/`metal` spellings normalize to `gpu`
    /// rather than falling back, the same as the wire setter, so a config
    /// written before this vocabulary still loads onto the accelerator it
    /// always meant.
    fn parse_or_reset(content: &str) -> (Self, bool) {
        use crate::daemon::device_management::parse_device_preference;
        match toml::from_str::<DaemonConfig>(content) {
            Ok(mut config) => {
                config.device.preferred_device =
                    parse_device_preference(&config.device.preferred_device)
                        .unwrap_or_else(|| "cpu".to_string());
                for settings in config
                    .backends
                    .models
                    .values_mut()
                    .flat_map(HashMap::values_mut)
                {
                    settings.device = settings.device.as_deref().and_then(parse_device_preference);
                }
                (config, false)
            }
            Err(e) => {
                warn!("Failed to parse config: {e}. Resetting to defaults.");
                (Self::default(), true)
            }
        }
    }

    /// Load configuration from disk.
    ///
    /// Falls back to defaults when the file is missing or cannot be parsed
    /// (e.g. after a format change). When falling back, the default config is
    /// saved to disk so subsequent loads succeed cleanly. If individual fields
    /// fell back to their defaults (a stale enum value via the shared
    /// `deserialize_or_default` helper), the canonical form is rewritten so the
    /// warning doesn't repeat next startup.
    #[must_use]
    pub fn load() -> Self {
        let config_path = Self::get_config_path();

        let Ok(content) = fs::read_to_string(&config_path) else {
            return Self::default();
        };

        let (config, was_reset) = Self::parse_or_reset(&content);
        if was_reset {
            // Persist the regenerated defaults so subsequent loads are clean.
            if let Err(e) = config.save() {
                error!("Failed to save default config after parse error: {e}");
            }
        } else if let Ok(canonical) = toml::to_string_pretty(&config)
            && canonical != content
            && let Err(e) = config.save()
        {
            error!("Failed to rewrite config in canonical form: {e}");
        }
        config
    }

    /// Save configuration to disk. Blocking (`std::fs::write`); on the async
    /// runtime call it via `persist_config()` / `spawn_blocking`, not inline.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration directory cannot be created,
    /// serialization fails, or the file cannot be written.
    pub fn save(&self) -> anyhow::Result<()> {
        let config_path = Self::get_config_path();

        // Create config directory if it doesn't exist
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let toml_content = toml::to_string_pretty(self)?;
        fs::write(&config_path, toml_content)?;

        debug!("Saved daemon config to {}", config_path.display());
        Ok(())
    }

    // Config mutators are PURE (Tier 3 #3): they mutate in-memory state only.
    // The caller persists once via `persist_config()` (which writes off the
    // async runtime), so there are no blocking `fs::write`s under the config
    // lock and no double writes.

    /// Update the global default device — the one models with no device of
    /// their own fall back to. Per-model choices are untouched, which is the
    /// point of the split: flipping the default must not silently move a model
    /// the user deliberately pinned somewhere else.
    pub fn update_preferred_device(&mut self, device: String) {
        self.device.preferred_device = device;
    }

    /// Set (`Some`) or clear (`None`) the device a model loads on. The value
    /// is the normalized `cpu`/`gpu` preference; callers validate before
    /// storing.
    pub fn update_model_device(&mut self, source: &str, model: &str, device: Option<String>) {
        match device {
            Some(v) => {
                self.backends
                    .models
                    .entry(source.to_string())
                    .or_default()
                    .entry(model.to_string())
                    .or_default()
                    .device = Some(v);
            }
            None => {
                if let Some(models) = self.backends.models.get_mut(source)
                    && let Some(settings) = models.get_mut(model)
                {
                    settings.device = None;
                }
            }
        }
    }

    /// A model's own device, if it has one.
    #[must_use]
    pub fn model_device(&self, source: &str, model: &str) -> Option<&str> {
        self.backends
            .models
            .get(source)
            .and_then(|m| m.get(model))
            .and_then(|s| s.device.as_deref())
    }

    /// The device `(source, model)` loads on: its own, else the global
    /// default. Every load path asks this rather than reading
    /// `device.preferred_device` directly, so a model with no device of its
    /// own keeps loading where the global preference always put it, and one
    /// with its own is not dragged along by a change to that default.
    #[must_use]
    pub fn effective_device(&self, source: &str, model: &str) -> String {
        self.model_device(source, model)
            .unwrap_or(&self.device.preferred_device)
            .to_string()
    }

    /// Update audio theme.
    pub fn update_audio_theme(&mut self, theme: AudioTheme) {
        self.audio.theme = theme;
    }

    /// Update preferred model + source, and the legacy `preferred_provider`
    /// the model declares (see [`SynthesisConfig::preferred_provider`] for
    /// why a stale value is as bad as a missing one).
    pub fn update_preferred_model(
        &mut self,
        model: String,
        source: String,
        provider: Option<String>,
    ) {
        self.synthesis.preferred_model = model;
        self.synthesis.preferred_source = source;
        self.synthesis.preferred_provider = provider.unwrap_or_default();
    }

    /// Clear the loaded-model preference (model + source) while keeping the
    /// active backend selected. Used by the unload path so a daemon restart
    /// stays idle instead of reloading the unloaded model.
    pub fn clear_preferred_model(&mut self) {
        self.synthesis.preferred_model = String::new();
        self.synthesis.preferred_source = String::new();
        self.synthesis.preferred_provider = String::new();
    }

    /// Set the active backend (its relative install dir) and drop the loaded
    /// model preference — selecting a backend does not load a model.
    pub fn update_active_backend(&mut self, dir: String) {
        self.synthesis.active_backend = Some(dir);
        self.synthesis.preferred_model = String::new();
        self.synthesis.preferred_provider = String::new();
    }

    /// Repoint the active backend to `new_dir` without touching the loaded
    /// model preference.
    ///
    /// Deliberately narrower than [`update_active_backend`]: that method
    /// clears `preferred_model`/`preferred_provider` because it models the
    /// user *choosing a different backend*, where any previously loaded
    /// model no longer applies. This method models an install-time directory
    /// rename of the *same* backend — its identity, models, and selection are
    /// unchanged, only the on-disk directory name moved. Using
    /// `update_active_backend` here would silently wipe the user's model
    /// choice as a side effect of an update. Do not merge the two.
    pub fn rename_active_backend(&mut self, new_dir: String) {
        self.synthesis.active_backend = Some(new_dir);
    }

    /// Clear the active backend and the loaded-model preference (→ idle).
    pub fn clear_active_backend(&mut self) {
        self.synthesis.active_backend = None;
        self.synthesis.preferred_model = String::new();
        self.synthesis.preferred_source = String::new();
        self.synthesis.preferred_provider = String::new();
    }

    /// Update master volume.
    pub fn update_volume(&mut self, volume: u8) {
        self.audio.volume = volume;
    }

    /// Set or clear a backend option override. An empty `value` clears the
    /// override (the backend falls back to its manifest default).
    pub fn update_backend_option(&mut self, source: String, name: String, value: String) {
        if value.is_empty() {
            if let Some(opts) = self.backends.options.get_mut(&source) {
                opts.remove(&name);
                if opts.is_empty() {
                    self.backends.options.remove(&source);
                }
            }
        } else {
            self.backends
                .options
                .entry(source)
                .or_default()
                .insert(name, value);
        }
    }

    /// The configured override for a backend option, if any.
    #[must_use]
    pub fn backend_option(&self, source: &str, name: &str) -> Option<&str> {
        self.backends
            .options
            .get(source)
            .and_then(|opts| opts.get(name))
            .map(String::as_str)
    }

    pub fn update_primary_language(&mut self, language: Option<String>) {
        self.synthesis.primary_language = language;
    }

    #[must_use]
    pub fn primary_language(&self) -> Option<&str> {
        self.synthesis.primary_language.as_deref()
    }

    /// Set (`Some`) or clear (`None`) a per-model language override.
    pub fn update_model_language(
        &mut self,
        source: String,
        model: String,
        language: Option<String>,
    ) {
        match language {
            Some(v) => {
                self.backends
                    .models
                    .entry(source)
                    .or_default()
                    .entry(model)
                    .or_default()
                    .language = Some(v);
            }
            None => {
                if let Some(models) = self.backends.models.get_mut(&source)
                    && let Some(settings) = models.get_mut(&model)
                {
                    settings.language = None;
                }
            }
        }
    }

    #[must_use]
    pub fn model_language(&self, source: &str, model: &str) -> Option<&str> {
        self.backends
            .models
            .get(source)
            .and_then(|m| m.get(model))
            .and_then(|s| s.language.as_deref())
    }

    /// Set (`Some`) or clear (`None`) the voice a model speaks in by default.
    pub fn update_model_voice(&mut self, source: String, model: String, voice: Option<String>) {
        match voice {
            Some(v) => {
                self.backends
                    .models
                    .entry(source)
                    .or_default()
                    .entry(model)
                    .or_default()
                    .voice = Some(v);
            }
            None => {
                if let Some(models) = self.backends.models.get_mut(&source)
                    && let Some(settings) = models.get_mut(&model)
                {
                    settings.voice = None;
                }
            }
        }
    }

    #[must_use]
    pub fn model_voice(&self, source: &str, model: &str) -> Option<&str> {
        self.backends
            .models
            .get(source)
            .and_then(|m| m.get(model))
            .and_then(|s| s.voice.as_deref())
    }
}

#[cfg(test)]
mod language_config_tests {
    use super::*;

    #[test]
    fn primary_and_model_language_round_trip_through_toml() {
        let mut cfg = DaemonConfig::default();
        assert_eq!(cfg.primary_language(), None);

        cfg.update_primary_language(Some("es-MX".to_string()));
        cfg.update_model_language(
            "github.com/x/kokoro".to_string(),
            "kokoro-large".to_string(),
            Some("fr".to_string()),
        );

        let toml = toml::to_string(&cfg).expect("serialize");
        let back: DaemonConfig = toml::from_str(&toml).expect("deserialize");

        assert_eq!(back.primary_language(), Some("es-MX"));
        assert_eq!(
            back.model_language("github.com/x/kokoro", "kokoro-large"),
            Some("fr")
        );
        assert_eq!(back.model_language("github.com/x/kokoro", "absent"), None);
    }

    /// The voice rides in the same per-model record as the language and the
    /// device, so it has to survive the same round trip through TOML.
    #[test]
    fn a_model_voice_round_trips_through_toml() {
        let mut cfg = DaemonConfig::default();
        cfg.update_model_voice(
            "github.com/x/qwen".into(),
            "qwen3-tts-0.6b-custom-voice".into(),
            Some("aiden".into()),
        );
        let toml = toml::to_string(&cfg).expect("config serializes");
        let back: DaemonConfig = toml::from_str(&toml).expect("config parses");
        assert_eq!(
            back.model_voice("github.com/x/qwen", "qwen3-tts-0.6b-custom-voice"),
            Some("aiden")
        );
        assert_eq!(back.model_voice("github.com/x/qwen", "absent"), None);
    }

    /// A voice and a language set on the same model do not overwrite each
    /// other: both live in one `ModelSettings`, so a careless `or_default`
    /// would blank whichever was written first.
    #[test]
    fn a_voice_and_a_language_coexist_on_one_model() {
        let mut cfg = DaemonConfig::default();
        cfg.update_model_language("s".into(), "m".into(), Some("fr".into()));
        cfg.update_model_voice("s".into(), "m".into(), Some("ryan".into()));
        assert_eq!(cfg.model_language("s", "m"), Some("fr"));
        assert_eq!(cfg.model_voice("s", "m"), Some("ryan"));

        // And clearing one leaves the other alone.
        cfg.update_model_voice("s".into(), "m".into(), None);
        assert_eq!(cfg.model_voice("s", "m"), None);
        assert_eq!(cfg.model_language("s", "m"), Some("fr"));
    }

    #[test]
    fn clearing_model_language_sets_none() {
        let mut cfg = DaemonConfig::default();
        cfg.update_model_language("s".into(), "m".into(), Some("de".into()));
        cfg.update_model_language("s".into(), "m".into(), None);
        assert_eq!(cfg.model_language("s", "m"), None);
    }
}

#[cfg(test)]
mod device_config_tests {
    use super::*;

    /// A model with no device of its own loads where the global default
    /// says; one with its own loads there regardless of the default.
    #[test]
    fn a_model_device_overrides_the_global_default() {
        let mut cfg = DaemonConfig::default();
        assert_eq!(cfg.effective_device("s", "m"), "cpu");

        cfg.device.preferred_device = "gpu".to_string();
        assert_eq!(
            cfg.effective_device("s", "m"),
            "gpu",
            "inherits the default"
        );

        cfg.update_model_device("s", "m", Some("cpu".into()));
        assert_eq!(cfg.model_device("s", "m"), Some("cpu"));
        assert_eq!(cfg.effective_device("s", "m"), "cpu");
        assert_eq!(
            cfg.effective_device("s", "other"),
            "gpu",
            "a sibling model is untouched"
        );

        cfg.update_model_device("s", "m", None);
        assert_eq!(cfg.model_device("s", "m"), None);
        assert_eq!(cfg.effective_device("s", "m"), "gpu");
    }

    /// Setting a device on a model that already has a language keeps the
    /// language, and vice versa: both live in the same per-model row.
    #[test]
    fn device_and_language_share_the_model_row() {
        let mut cfg = DaemonConfig::default();
        cfg.update_model_language("s".into(), "m".into(), Some("fr".into()));
        cfg.update_model_device("s", "m", Some("gpu".into()));
        assert_eq!(cfg.model_language("s", "m"), Some("fr"));
        assert_eq!(cfg.model_device("s", "m"), Some("gpu"));

        let toml = toml::to_string(&cfg).expect("serialize");
        let back: DaemonConfig = toml::from_str(&toml).expect("deserialize");
        assert_eq!(back.model_language("s", "m"), Some("fr"));
        assert_eq!(back.model_device("s", "m"), Some("gpu"));
    }

    /// A persisted per-model device is normalized on load the same way the
    /// global default is: the deprecated `cuda` spelling still means `gpu`,
    /// and junk degrades to "no device of its own" rather than resetting the
    /// whole config.
    #[test]
    fn persisted_model_devices_are_normalized_on_load() {
        let content = r#"
[device]
preferred_device = "cpu"

[audio]
theme = "classic"
volume = 80

[synthesis]
preferred_model = ""

[backends.models."github.com/x/kokoro".large]
device = "cuda"

[backends.models."github.com/x/kokoro".small]
device = "xpu"
language = "fr"
"#;
        let (cfg, reset) = DaemonConfig::parse_or_reset(content);
        assert!(!reset);
        assert_eq!(
            cfg.model_device("github.com/x/kokoro", "large"),
            Some("gpu")
        );
        assert_eq!(cfg.model_device("github.com/x/kokoro", "small"), None);
        assert_eq!(
            cfg.model_language("github.com/x/kokoro", "small"),
            Some("fr"),
            "normalizing the device leaves the rest of the row alone"
        );
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
