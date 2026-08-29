// SPDX-License-Identifier: GPL-3.0-only
//! Unified traits for TTS backends presented to the daemon.
//!
//! Three layered surfaces:
//! - [`ModelInfo`] — static metadata (name, source, capabilities).
//! - [`ModelState`] — runtime state that can change after load (device).
//! - [`Synthesize`] — actual inference.
//!
//! `Synthesize` is a supertrait of `ModelState`, which is a supertrait of
//! `ModelInfo`, so a `Box<dyn Synthesize>` exposes all three. The concrete
//! implementors are the backend hosts ([`WasmBackend`](super::wasm) and
//! [`SubprocessBackend`](super::subprocess)); the daemon never sees model
//! internals.

use anyhow::Result;
use async_trait::async_trait;
use std::time::Duration;

/// Static metadata about a loaded model, built from the discovered backend
/// entry that serves it. Self-contained — there is no static registry.
#[derive(Debug, Clone)]
pub struct ModelInfoData {
    /// Wire-level model name.
    pub name: String,
    /// Repo id of the backend serving this model.
    pub source: String,
    /// Whether the model supports multiple languages.
    pub is_multilingual: bool,
    /// Whether the model is served by a remote API with no local compute.
    /// Derived from the model's `supported_devices` (`none` sentinel) at
    /// construction.
    pub online: bool,
    /// Suggested minimum interval between real-time processing chunks.
    pub processing_interval: Duration,
}

impl ModelInfoData {
    /// Build metadata for a discovered backend model.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        source: impl Into<String>,
        is_multilingual: bool,
        online: bool,
        processing_interval: Duration,
    ) -> Self {
        Self {
            name: name.into(),
            source: source.into(),
            is_multilingual,
            online,
            processing_interval,
        }
    }
}

/// Static metadata accessor surface — implemented by every loaded model.
pub trait ModelInfo: Send + Sync {
    /// The underlying metadata payload.
    fn info(&self) -> &ModelInfoData;
    /// Wire-level name.
    fn display_name(&self) -> &str {
        &self.info().name
    }

    /// Whether this model supports multiple languages.
    fn is_multilingual(&self) -> bool {
        self.info().is_multilingual
    }

    /// Whether this model sends audio to an external API.
    fn is_online(&self) -> bool {
        self.info().online
    }

    /// Suggested minimum interval between real-time processing chunks.
    fn processing_interval(&self) -> Duration {
        self.info().processing_interval
    }
}

/// Runtime state that may change after the model is loaded — currently the
/// device label the backend reports (e.g. `"cpu"`, `"cuda"`, `"remote"`).
pub trait ModelState: ModelInfo {
    /// Short device label the model runs on, as reported by the backend.
    fn device(&self) -> String;
}

/// Common contract for any TTS backend.
///
/// Implementations drive an out-of-tree backend over the `/v1` contract
/// (in-process for WASM, over a Unix socket for subprocesses). The trait is
/// `async` so callers treat both uniformly.
#[async_trait]
pub trait Synthesize: ModelState {
    /// Synthesize speech, feeding `sink` as frames decode.
    ///
    /// `sink` is `&mut dyn` rather than a generic so the trait stays
    /// object-safe: the daemon holds the loaded model as
    /// `Box<dyn Synthesize>`, and a generic method could not be called through
    /// it.
    ///
    /// Required, not defaulted: synthesis is the only thing a backend exists to
    /// do, so a host that cannot answer this is not a backend. It was optional
    /// only while transcription was still the trait's primary call.
    ///
    /// # Errors
    /// Returns an error if the backend cannot be reached, refuses the request,
    /// or produces a malformed frame stream.
    async fn synthesize(
        &self,
        request: &crate::tts_models::v1::SynthesizeRequest<'_>,
        sink: &mut (dyn crate::tts_models::v1::SynthesisSink + Send),
    ) -> Result<()>;

    /// Run a realtime streaming session, pumping frames between the
    /// consumer and an upstream until the session ends. Default:
    /// unsupported. Only WASM backends serving a `realtime` model override
    /// this.
    #[cfg(feature = "wasm-backends")]
    async fn realtime_session(
        &self,
        transport: crate::tts_models::wasm::ws_host::ConsumerStreamTransport,
    ) -> Result<()> {
        let _ = transport;
        anyhow::bail!("this model does not support realtime streaming")
    }

    /// Release any external resources the backend holds. Default no-op.
    ///
    /// Subprocess backends override this to stop the `systemd-run --user`
    /// unit they spawned at load time; in-process WASM backends have
    /// nothing to release. The daemon calls this *before* dropping the
    /// [`LoadedModel`](crate::daemon::types::LoadedModel) so the cleanup
    /// happens in an async context where blocking is appropriate — the
    /// `Drop` impl is a synchronous safety net for crash paths and tests.
    async fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }
}
