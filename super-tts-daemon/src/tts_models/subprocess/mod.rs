// SPDX-License-Identifier: GPL-3.0-only
//! Host for TTS backends shipped as sandboxed native subprocesses
//! (experimental — gated behind the `subprocess-backends` feature).
//!
//! [`SubprocessBackend`] provisions a backend's model files (downloading from
//! `HuggingFace` into the per-backend directory), spawns the backend binary in a
//! hardened `systemd-run --user` transient unit, drives the `/v1` contract
//! over a pathname Unix socket, and presents the result through the daemon's
//! [`Synthesize`] trait. The backend itself is fully self-contained and shares
//! no code with the daemon.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use log::{info, warn};
use tokio::net::UnixStream;

use crate::tts_models::backends::manifest::Manifest;
use crate::tts_models::synthesize::{ModelInfo, ModelInfoData, ModelState, Synthesize};

mod systemd;
pub use systemd::cleanup_orphan_units;

/// A running, sandboxed subprocess backend usable as a [`Synthesize`] model.
pub struct SubprocessBackend {
    socket: PathBuf,
    unit: String,
    model_id: String,
    info: ModelInfoData,
    /// Device label reported by the backend's `/v1/status` (e.g. `"cuda"`).
    device: String,
    /// Whether [`Synthesize::shutdown`] already stopped the unit.
    ///
    /// `Drop` is the safety net for crash paths, but on the ordinary path
    /// `shutdown` has already run and a second `systemctl stop` would fail with
    /// [`EXIT_UNIT_NOT_LOADED`] and warn that the subprocess might still be
    /// running — a false alarm on every clean shutdown, and the fastest way to
    /// teach an operator to ignore this log.
    stopped: bool,
}

/// `systemctl`'s exit code for an operation on a unit it does not have loaded.
///
/// For a stop, that is the goal state rather than a failure: the unit is gone,
/// whether this call removed it or it was already collected.
const EXIT_UNIT_NOT_LOADED: i32 = 5;

/// Interpret the result of `systemctl --user stop <unit>` for logging.
fn report_stop(unit: &str, status: std::process::ExitStatus) {
    if status.success() {
        info!("stopped backend unit {unit}");
    } else if status.code() == Some(EXIT_UNIT_NOT_LOADED) {
        info!("backend unit {unit} was already gone");
    } else {
        warn!("systemctl --user stop {unit} exited with {status}; subprocess may still be running");
    }
}

impl SubprocessBackend {
    /// Provision the selected model, spawn the sandboxed backend, and load it.
    ///
    /// `backend_dir` holds `backend.toml` and the `entrypoint` binary; model
    /// files are downloaded into `<backend_dir>/<dest>`. `device_pref` is the
    /// resolved accelerator (`"cpu"`, `"cuda"`, `"rocm"`, `"metal"`,
    /// `"vulkan"`), or empty when none resolved, which leaves the backend to
    /// select for itself.
    ///
    /// # Errors
    /// Returns an error if provisioning, spawning, or loading fails.
    pub async fn spawn(
        backend_dir: &Path,
        model_name: &str,
        device_pref: &str,
        tracker: Option<&Arc<crate::download_progress::DownloadProgressTracker>>,
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
        let items: Vec<_> = model
            .files
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

        // Socket under the runtime dir (pathname socket — survives PrivateNetwork).
        // Route through the shared validated helper so it gets the same
        // traversal/prefix/length guards as the daemon's own sockets, instead
        // of a raw `$XDG_RUNTIME_DIR` join (Tier 2 #7).
        let socket = super_tts_shared::validation::secure_runtime_path(&format!(
            "backends/{}.sock",
            sanitize(model_name)
        ));
        // `secure_runtime_path` always returns at least
        // `<runtime>/tts/backends/<name>.sock`, so a missing parent is
        // unreachable. Surface that as an error rather than inventing a
        // directory: the previous fallback named `/tmp/stt/backends`, which was
        // both the wrong path and a world-writable home for backend sockets.
        let socket_dir = socket
            .parent()
            .context("backend socket path has no parent directory")?;
        std::fs::create_dir_all(socket_dir)?;
        let _ = std::fs::remove_file(&socket);

        let binary = backend_dir.join(&manifest.backend.entrypoint);
        anyhow::ensure!(
            binary.exists(),
            "backend binary not found: {}",
            binary.display()
        );

        // The one writable, durable path the sandbox grants. Created here
        // rather than by the backend: the unit's `ReadWritePaths` needs the
        // directory to exist when it spawns.
        let cache_dir = backend_cache_dir(backend_dir)?;
        std::fs::create_dir_all(&cache_dir)
            .with_context(|| format!("creating backend cache dir {}", cache_dir.display()))?;

        let unit = format!(
            "super-tts-backend-{}-{}",
            sanitize(model_name),
            std::process::id()
        );

        systemd::spawn_systemd_unit(
            &unit,
            &binary,
            backend_dir,
            socket_dir,
            &cache_dir,
            &socket,
            &model.supported_devices,
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

        let mut backend = Self {
            socket,
            unit,
            model_id: model_name.to_string(),
            info,
            device: "unknown".to_string(),
            stopped: false,
        };

        backend.wait_for_ping(Duration::from_secs(30)).await?;
        backend
            .load(model_name, model.provider.as_deref(), device_pref)
            .await?;
        Ok(backend)
    }

    /// Poll `/v1/ping` until the backend is serving or the deadline passes.
    async fn wait_for_ping(&self, timeout: Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Ok((200, _)) = self.request("GET", "/v1/ping", &[], Vec::new()).await {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                bail!(
                    "backend did not start within {timeout:?}.\n{}",
                    self.unit_logs()
                );
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// `POST /v1/load` then poll `/v1/status` until `ready` (or `error`),
    /// capturing the device label the backend reports.
    async fn load(&mut self, name: &str, provider: Option<&str>, device_pref: &str) -> Result<()> {
        let body = serde_json::to_vec(&load_body(name, provider, device_pref))?;
        let (status, resp) = self
            .request("POST", "/v1/load", &json_headers(), body)
            .await?;
        anyhow::ensure!(
            status == 202 || status == 200,
            "/v1/load returned {status}: {}",
            String::from_utf8_lossy(&resp)
        );

        // Loading the model onto the GPU can take a while.
        let deadline = std::time::Instant::now() + Duration::from_mins(10);
        loop {
            let (_, resp) = self.request("GET", "/v1/status", &[], Vec::new()).await?;
            let json: serde_json::Value = serde_json::from_slice(&resp)?;
            match json.get("state").and_then(|v| v.as_str()) {
                Some("ready") => {
                    let device = json.get("device").and_then(|v| v.as_str()).unwrap_or("?");
                    info!("backend ready (device={device})");
                    self.device = device.to_string();
                    return Ok(());
                }
                Some("error") => bail!(
                    "backend load failed: {}",
                    json.get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                ),
                _ => {}
            }
            if std::time::Instant::now() >= deadline {
                bail!("backend load timed out");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// One HTTP request over the backend's Unix socket.
    async fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<(u16, Vec<u8>)> {
        let stream = UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("connect {}", self.socket.display()))?;
        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;
        tokio::spawn(async move {
            let _ = conn.await;
        });

        let mut builder = hyper::Request::builder()
            .method(method)
            .uri(path)
            .header("host", "backend.local");
        for (k, v) in headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        let req = builder.body(Full::new(Bytes::from(body)))?;

        let resp = sender.send_request(req).await?;
        let status = resp.status().as_u16();
        let bytes = resp.into_body().collect().await?.to_bytes().to_vec();
        Ok((status, bytes))
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

        let stream = UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("connect {}", self.socket.display()))?;
        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;
        // The connection task must keep running while the body streams — the
        // response is not complete when `send_request` resolves.
        let pump = tokio::spawn(async move {
            let _ = conn.await;
        });

        let request = hyper::Request::builder()
            .method("POST")
            .uri("/v1/synthesize")
            .header("host", "backend.local")
            .header("content-type", "application/json")
            .header("x-tts-model", self.model_id.as_str())
            .body(Full::new(Bytes::from(body)))?;

        let resp = sender.send_request(request).await?;
        let status = resp.status().as_u16();
        let (parts, body) = resp.into_parts();
        let result = if status == 200 {
            crate::tts_models::v1::pump_synthesis(&parts.headers, body, sink).await
        } else {
            let collected = body.collect().await?.to_bytes();
            Err(crate::tts_models::v1::synthesize_error(status, &collected))
        };
        pump.abort();
        result
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
        let mut headers = json_headers();
        headers.push(("x-tts-model".to_string(), self.model_id.clone()));
        let (status, resp) = self.request("POST", "/v1/voices", &headers, body).await?;
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
        let (status, resp) = self.request("DELETE", &path, &[], Vec::new()).await?;
        if status == 404 || (200..300).contains(&status) {
            return Ok(());
        }
        Err(crate::tts_models::v1::voice_error(status, &resp))
    }

    /// Capture recent unit logs for diagnostics.
    fn unit_logs(&self) -> String {
        std::process::Command::new("journalctl")
            .args(["--user", "-u", &self.unit, "--no-pager", "-n", "30"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    }
}

impl Drop for SubprocessBackend {
    fn drop(&mut self) {
        // Best-effort: stop the transient unit (SIGTERM) and remove the socket.
        //
        // `Drop` is synchronous, so we call `std::process::Command` directly;
        // it blocks the runtime worker thread while `systemctl --user stop`
        // waits for the unit to exit (usually under a second). Surfaces the
        // result so a failure doesn't silently leave the subprocess running
        // — the previous `let _ = …` swallowed every error, which made the
        // "backend not stopping" failure mode invisible.
        if self.stopped {
            return;
        }
        match std::process::Command::new("systemctl")
            .args(["--user", "stop", &self.unit])
            .status()
        {
            Ok(status) => report_stop(&self.unit, status),
            Err(e) => {
                warn!("failed to invoke systemctl to stop {}: {e}", self.unit);
            }
        }
        let _ = std::fs::remove_file(&self.socket);
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
        self.device.clone()
    }
}

#[async_trait]
impl Synthesize for SubprocessBackend {
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

    /// Stop the `systemd-run --user` transient unit asynchronously and
    /// remove the socket file. Called by the daemon before the
    /// [`LoadedModel`](crate::daemon::types::LoadedModel) is dropped — gives
    /// us a real `.await` instead of blocking the runtime in `Drop`. After
    /// this returns the synchronous `Drop` impl returns immediately, which is
    /// what the `stopped` flag is for; `Drop` stays for the crash paths where
    /// this never ran.
    async fn shutdown(&mut self) -> Result<()> {
        let status = tokio::process::Command::new("systemctl")
            .args(["--user", "stop", &self.unit])
            .status()
            .await
            .with_context(|| format!("invoke systemctl to stop {}", self.unit))?;
        report_stop(&self.unit, status);
        // Set even when the stop reported a failure: the attempt was made, and
        // `Drop` repeating it moments later would not succeed where this did
        // not. The startup sweep is what recovers a unit that truly survived.
        self.stopped = true;
        let _ = std::fs::remove_file(&self.socket);
        Ok(())
    }
}

fn json_headers() -> Vec<(String, String)> {
    vec![("content-type".to_string(), "application/json".to_string())]
}

/// Build the `POST /v1/load` body. `name` is always present; `device` only
/// when the daemon resolved an accelerator to name, and `provider` only when
/// the model's manifest declares one.
///
/// `provider` is a compatibility echo (see [`ModelEntry::provider`]): backends
/// released against the earlier `(name, provider)` identity answer
/// `400 invalid_model` for a load body that omits it, so whatever the manifest
/// declares is forwarded verbatim.
///
/// [`ModelEntry::provider`]: crate::tts_models::backends::manifest::ModelEntry::provider
fn load_body(name: &str, provider: Option<&str>, device_pref: &str) -> serde_json::Value {
    let mut load = serde_json::json!({ "name": name });
    if let Some(provider) = provider {
        load["provider"] = serde_json::json!(provider);
    }
    if !device_pref.is_empty() {
        load["device"] = serde_json::json!(device_pref);
    }
    load
}

/// The writable, durable cache directory granted to a backend's sandbox.
///
/// Everything else the sandbox exposes is read-only or discarded: the backend
/// directory is `ReadOnlyPaths`, `$HOME` is `ProtectHome=read-only`, and the
/// writable `/tmp` is `PrivateTmp`, so it dies with the unit. A backend with
/// nothing to keep never notices. One that compiles its GPU kernels at runtime
/// does: `CubeCL` (the Burn backends) spends about twenty seconds compiling a
/// few hundred kernels, caches them keyed by build and device, and without a
/// durable home pays that on *every* load rather than once per install.
///
/// Keyed on the backend directory's name — the backend id the installer names
/// it after — so backends never share a cache, and an in-place upgrade keeps
/// the one it warmed. [`sanitize`] is what keeps a directory name from
/// steering the path anywhere else.
fn backend_cache_dir(backend_dir: &Path) -> Result<PathBuf> {
    let key = backend_dir
        .file_name()
        .and_then(|n| n.to_str())
        .context("backend directory has no name to key its cache on")?;
    Ok(super_tts_shared::paths::cache_dir()
        .join("backends")
        .join(sanitize(key)))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
