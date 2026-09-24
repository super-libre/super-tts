// SPDX-License-Identifier: GPL-3.0-only
//! Host for TTS backends shipped as sandboxed native subprocesses
//! (experimental — gated behind the `subprocess-backends` feature).
//!
//! [`SubprocessBackend`] provisions a backend's model files (downloading from
//! `HuggingFace` into the per-backend directory), has
//! [`super_engine_daemon::subprocess`] spawn the backend binary into a sandbox
//! and load the model, and presents the result through the daemon's
//! [`Synthesize`] trait. The sandbox, the socket and the `/v1` handshake are
//! the engine's; what is here is what a synthesis backend is asked. The
//! backend itself is fully self-contained and shares no code with the daemon.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use http_body_util::BodyExt;
use log::info;
use super_engine_daemon::subprocess::{self as engine, Launch, json_headers};
use super_tts_shared::models::protocol::LoadProgress;

use crate::tts_models::backends::manifest::Manifest;
use crate::tts_models::synthesize::{ModelInfo, ModelInfoData, ModelState, Synthesize};

/// A running, sandboxed subprocess backend usable as a [`Synthesize`] model.
pub struct SubprocessBackend {
    backend: engine::SubprocessBackend,
    model_id: String,
    info: ModelInfoData,
}

impl SubprocessBackend {
    /// Provision the selected model, spawn the sandboxed backend, and load it.
    ///
    /// `backend_dir` holds `backend.toml` and the `entrypoint` binary; model
    /// files are downloaded into `<backend_dir>/<dest>`. `device_pref` is the
    /// resolved accelerator (`"cpu"`, `"cuda"`, `"rocm"`, `"metal"`,
    /// `"vulkan"`), or empty when none resolved, which leaves the backend to
    /// select for itself. `host` is what the machine's GPUs actually are, and
    /// decides which variant of a per-architecture model file is fetched —
    /// deliberately not `device_pref`, so choosing CPU on a CUDA machine does
    /// not discard the CUDA files already on disk. `context_headers` are the
    /// already-formed `x-tts-secret-*` / `x-tts-option-*` pairs to inject on
    /// every request.
    ///
    /// # Errors
    /// Returns an error if provisioning, spawning, or loading fails.
    pub async fn spawn(
        backend_dir: &Path,
        model_name: &str,
        device_pref: &str,
        host: &crate::registry::host_detect::Host,
        tracker: Option<&Arc<crate::download_progress::DownloadProgressTracker>>,
        context_headers: Vec<(String, String)>,
    ) -> Result<Self> {
        let manifest = Manifest::load(backend_dir)?;

        let model = manifest
            .models
            .iter()
            .find(|m| m.name == model_name)
            .ok_or_else(|| anyhow!("model {model_name} not declared in backend.toml"))?;

        // Provision ONLY the selected model's files (lazy per model). The
        // tracker (when present) reports per-file and per-byte progress through
        // `DownloadStateManager` so the settings app's progress bar updates in
        // real time. Each file carries its own URL and destination; `parse`
        // already validated every `destination` as a safe relative path, so the
        // join below cannot escape the backend dir.
        //
        // Entries sharing a destination are per-architecture variants of one
        // file, so the list is resolved against the host first: exactly one
        // variant per destination is fetched, and a manifest that declares no
        // selector — every v1 manifest — comes back as the files it wrote.
        // `instantiate_subprocess` resolves the same list to size the progress
        // card, and both go through this one function so the count and the
        // downloads cannot disagree.
        let selected = crate::registry::compat::select_files(host, model)
            .map_err(|reason| anyhow!("provisioning {model_name}: {reason}"))?;
        let items: Vec<_> = selected
            .iter()
            .map(|spec| crate::tts_models::download::DownloadItem {
                url: spec.url.clone(),
                destination: backend_dir.join(&spec.destination),
                sha256: spec.sha256.clone(),
            })
            .collect();
        info!(
            "provisioning {model_name}: {} files into {}",
            items.len(),
            backend_dir.display()
        );
        crate::tts_models::download::download_files(&items, tracker, 0)
            .await
            .with_context(|| format!("provisioning {model_name}"))?;

        // All files are on disk. Spawning the sandboxed unit and loading
        // weights onto the device is the slow tail (tens of seconds for a
        // multi-GB model on GPU) but isn't byte-tracked — flip the tracker
        // to "loading_model" so the settings app swaps the full download
        // bar for a "Loading model into memory…" indicator instead of
        // freezing on a full bar.
        if let Some(t) = tracker {
            t.mark_loading();
            t.broadcast_progress();
        }

        // What the backend reports of its own load goes to the tracker, so the
        // card can say what the load is doing rather than sit on a full bar.
        let forward = tracker.map(|t| {
            move |load: LoadProgress| {
                t.set_load_progress(load);
                t.broadcast_progress();
            }
        });

        let backend = engine::SubprocessBackend::spawn(
            &super_tts_shared::SUPER_TTS,
            Launch {
                backend_dir,
                entrypoint: &manifest.backend.entrypoint,
                model: model_name,
                provider: model.provider.as_deref(),
                devices: &model.supported_devices,
                device_pref,
                context_headers,
                on_load_progress: forward
                    .as_ref()
                    .map(|f| f as &(dyn Fn(LoadProgress) + Send + Sync)),
            },
        )
        .await?;

        let interval = model
            .processing_interval_ms
            .map_or_else(|| Duration::from_secs(2), Duration::from_millis);
        let info = ModelInfoData::new(
            model_name,
            manifest.backend.source.clone(),
            model.multilingual,
            model.is_online(),
            interval,
        );

        Ok(Self {
            backend,
            model_id: model_name.to_string(),
            info,
        })
    }

    /// The headers a model request carries: the JSON body, and which model
    /// it is for.
    fn model_headers(&self) -> Vec<(String, String)> {
        let mut headers = json_headers();
        headers.push(("x-tts-model".to_string(), self.model_id.clone()));
        headers
    }

    /// `POST /v1/synthesize` — drive one synthesis and feed `sink` as frames
    /// decode.
    ///
    /// The body is read incrementally rather than collected: a subprocess
    /// backend writes PCM as it produces it, so the daemon can begin playing
    /// well before synthesis ends.
    ///
    /// # Errors
    /// Returns an error if the socket dial or request fails, the response is
    /// not `200`, the headers do not describe usable audio, or the frame stream
    /// is malformed.
    pub async fn synthesize(
        &self,
        req: &crate::tts_models::v1::SynthesizeRequest<'_>,
        sink: &mut (dyn crate::tts_models::v1::SynthesisSink + Send),
    ) -> Result<()> {
        let body = crate::tts_models::v1::build_synthesize_body(req)?;
        // The connection is held until the body has been read: the response
        // is not complete when its head arrives.
        let (resp, _connection) = self
            .backend
            .send("POST", "/v1/synthesize", &self.model_headers(), body)
            .await?;
        let status = resp.status().as_u16();
        let (parts, body) = resp.into_parts();
        if status == 200 {
            crate::tts_models::v1::pump_synthesis(&parts.headers, body, sink).await
        } else {
            let collected = body.collect().await?.to_bytes();
            Err(crate::tts_models::v1::synthesize_error(status, &collected))
        }
    }

    /// `POST /v1/voices` — hand the backend one cloned voice's reference
    /// audio.
    ///
    /// # Errors
    /// Returns an error if the socket dial fails or the backend refuses the
    /// clip.
    pub async fn register_voice(
        &self,
        req: &crate::tts_models::v1::RegisterVoiceRequest<'_>,
    ) -> Result<()> {
        let body = crate::tts_models::v1::build_register_voice_body(req)?;
        let (status, resp) = self
            .backend
            .request("POST", "/v1/voices", &self.model_headers(), body)
            .await?;
        if (200..300).contains(&status) {
            return Ok(());
        }
        Err(crate::tts_models::v1::voice_error(status, &resp))
    }

    /// `DELETE /v1/voices/{voice}` — drop a registered cloned voice.
    ///
    /// The id is percent-encoded into the path, so the `voice:` prefix travels
    /// as one path segment whatever a backend's router does with a raw colon.
    ///
    /// # Errors
    /// Returns an error if the socket dial fails or the backend refuses. A
    /// `404` is success: the goal is that the backend not hold the voice.
    pub async fn unregister_voice(&self, voice: &str) -> Result<()> {
        let path = format!("/v1/voices/{}", urlencoding::encode(voice));
        let (status, resp) = self
            .backend
            .request("DELETE", &path, &[], Vec::new())
            .await?;
        if status == 404 || (200..300).contains(&status) {
            return Ok(());
        }
        Err(crate::tts_models::v1::voice_error(status, &resp))
    }
}

impl ModelInfo for SubprocessBackend {
    fn info(&self) -> &ModelInfoData {
        &self.info
    }
}

impl ModelState for SubprocessBackend {
    /// Device label the backend reported at load time (e.g. `"cuda"`).
    fn device(&self) -> String {
        self.backend.device().to_string()
    }
}

#[async_trait]
impl Synthesize for SubprocessBackend {
    /// Swap the injected secret/option pairs. The next `/v1` request carries
    /// them; one already in flight keeps the set it was built with.
    ///
    /// `user_allowed_hosts` is ignored, and there is nothing here to ignore it
    /// with: the sandbox gives a subprocess backend no network, so it has no
    /// egress to authorize and `base_url` means nothing to it.
    fn reconfigure(&self, context: crate::tts_models::synthesize::BackendContext) {
        self.backend.set_context_headers(context.headers);
    }

    /// Forward to the inherent streaming implementation.
    async fn synthesize(
        &self,
        request: &crate::tts_models::v1::SynthesizeRequest<'_>,
        sink: &mut (dyn crate::tts_models::v1::SynthesisSink + Send),
    ) -> Result<()> {
        Self::synthesize(self, request, sink).await
    }

    async fn register_voice(
        &self,
        request: &crate::tts_models::v1::RegisterVoiceRequest<'_>,
    ) -> Result<()> {
        Self::register_voice(self, request).await
    }

    async fn unregister_voice(&self, voice: &str) -> Result<()> {
        Self::unregister_voice(self, voice).await
    }

    /// Stop the sandboxed backend asynchronously and remove the socket file.
    /// Called by the daemon before the
    /// [`LoadedModel`](crate::daemon::types::LoadedModel) is dropped — gives
    /// us a real `.await` instead of blocking the runtime in `Drop`. After
    /// this returns, the synchronous `Drop` path is a no-op and stays for
    /// crash paths and tests.
    async fn shutdown(&mut self) -> Result<()> {
        self.backend.shutdown().await;
        Ok(())
    }
}

/// Stop backends left behind by a previous daemon run. See
/// [`super_engine_daemon::subprocess::cleanup_orphans`].
pub async fn cleanup_orphan_units() {
    engine::cleanup_orphans(&super_tts_shared::SUPER_TTS).await;
}
