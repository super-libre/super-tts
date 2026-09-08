// SPDX-License-Identifier: GPL-3.0-only
//! Host-side driver for TTS backends shipped as `wasi:http` proxy components
//! (experimental — gated behind the `wasm-backends` feature).
//!
//! A [`WasmBackend`] loads a component, drives the `/v1` contract in-process
//! over wasmtime's `wasi:http` host, and presents the result through the
//! daemon's [`Synthesize`] trait. Secrets and options are injected as
//! `x-tts-secret-*` / `x-tts-option-*` request headers; outbound egress is
//! confined to the backend's `allowed_hosts` plus the endpoint the user
//! authorized through its `base_url` option (see [`host::AllowlistHooks`]).

pub mod host;
pub mod ws_host;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use http_body_util::BodyExt;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::WasiCtx;
use wasmtime_wasi_http::WasiHttpCtx;
use wasmtime_wasi_http::p2::WasiHttpView;
use wasmtime_wasi_http::p2::bindings::ProxyPre;
use wasmtime_wasi_http::p2::bindings::http::types::{ErrorCode, Scheme};

use crate::tts_models::synthesize::{ModelInfo, ModelInfoData, ModelState, Synthesize};
use host::{AllowlistHooks, Host};

/// Instantiation-ready component, pre-linked against one of the two worlds a
/// backend can target. Both worlds export `wasi:http/incoming-handler`, so the
/// batch `/v1` path works for either; only `Realtime` additionally exports
/// `ws-server` and imports `super-tts:realtime/ws`.
enum BackendPre {
    /// A plain `wasi:http` proxy backend (batch `/v1` only).
    Http(ProxyPre<Host>),
    /// A websocket-capable backend (batch `/v1` plus realtime `ws-server`).
    Realtime(ws_host::RealtimeBackendPre<Host>),
}

/// A loaded WASM backend component, usable as a [`Synthesize`] model.
pub struct WasmBackend {
    engine: Engine,
    pre: BackendPre,
    allowed_hosts: Arc<[String]>,
    /// Hosts the *user* authorized via backend options (e.g. a `base_url` set in
    /// the settings UI). Exempt from the SSRF guard — see [`AllowlistHooks`].
    user_allowed_hosts: Arc<[String]>,
    allow_loopback: bool,
    request_headers: Vec<(String, String)>,
    model_id: String,
    /// Whether the active model is realtime-only (`[[models]] realtime = true`),
    /// i.e. served over the `ws.connect` / `ws-server.handle` halves of the
    /// realtime WIT rather than the batch `/v1/synthesize` endpoint.
    realtime: bool,
    info: ModelInfoData,
}

impl WasmBackend {
    /// Load a component for a discovered model. The request headers are the
    /// already-formed `x-tts-secret-*` / `x-tts-option-*` pairs to inject.
    ///
    /// `allowed_hosts` are the backend's manifest-pinned `[network].allowed_hosts`
    /// (SSRF-guarded); `user_allowed_hosts` is what the user authorized via a
    /// `base_url` option, whose `host:port` has the guard relaxed (see
    /// [`host::AllowlistHooks::user_allowed_hosts`]).
    ///
    /// # Errors
    /// Returns an error if the component cannot be loaded or linked.
    pub fn with_info(
        component_path: &Path,
        allowed_hosts: Vec<String>,
        user_allowed_hosts: Vec<String>,
        info: ModelInfoData,
        request_headers: Vec<(String, String)>,
        websocket_capability: bool,
        realtime: bool,
    ) -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config)?;
        let component = Component::from_file(&engine, component_path)
            .map_err(|e| anyhow!("loading component {}: {e}", component_path.display()))?;
        Self::verify_imports(&engine, &component)?;
        let mut linker: Linker<Host> = Linker::new(&engine);
        // Link the full wasi command world (the component's Rust std runtime
        // imports `wasi:cli/environment` etc.) plus http. Capabilities remain
        // gated by the locked-down `WasiCtx` below — no preopened directories
        // and no granted sockets — so the component cannot touch the disk or
        // open raw connections; its only egress is the allowlisted
        // `wasi:http/outgoing-handler`.
        // `cli-exit-with-code` is still an `@unstable` WASI 0.2 feature, so
        // `LinkOptions::default()` leaves it out of the linker — but Rust's
        // wasm32-wasip2 std imports it, so every component built with a
        // toolchain that emits that import fails to instantiate unless the
        // host opts in. Enable it so backends stay loadable across toolchains.
        let mut link_options = wasmtime_wasi::p2::bindings::LinkOptions::default();
        link_options.cli_exit_with_code(true);
        wasmtime_wasi::p2::add_to_linker_with_options_async(&mut linker, &link_options)?;
        wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
        // A websocket-capable backend additionally imports
        // `super-tts:realtime/ws` and exports `ws-server`; link the host `ws`
        // impl and pre-instantiate against the realtime world. A plain backend
        // pre-instantiates against the `wasi:http` proxy world unchanged.
        let pre = if websocket_capability {
            ws_host::add_to_linker(&mut linker)?;
            BackendPre::Realtime(ws_host::RealtimeBackendPre::new(
                linker.instantiate_pre(&component)?,
            )?)
        } else {
            BackendPre::Http(ProxyPre::new(linker.instantiate_pre(&component)?)?)
        };
        let model_id = info.name.clone();
        Ok(Self {
            engine,
            pre,
            allowed_hosts: allowed_hosts.into(),
            user_allowed_hosts: user_allowed_hosts.into(),
            allow_loopback: false,
            request_headers,
            model_id,
            realtime,
            info,
        })
    }

    /// The egress policy every invocation of this backend enforces — the one
    /// place the two lists are wired into the hooks, so the batch and realtime
    /// paths cannot drift into disagreeing about which list is which.
    ///
    /// The distinction is load-bearing: `allowed_hosts` is the backend's own
    /// manifest and stays fully SSRF-guarded, while `user_allowed_hosts` is what
    /// the user authorized and has the guard relaxed for its `host:port`. Wiring
    /// them the other way round would hand a backend the relaxation for hosts it
    /// declared itself.
    ///
    /// Both lists are immutable for the backend's lifetime and the hooks only
    /// read them, so they are shared rather than copied: this runs once per
    /// synthesis and once per realtime session.
    #[must_use]
    pub fn allowlist_hooks(&self) -> AllowlistHooks {
        AllowlistHooks {
            allowed_hosts: self.allowed_hosts.clone(),
            user_allowed_hosts: self.user_allowed_hosts.clone(),
            allow_loopback: self.allow_loopback,
        }
    }

    /// Permit this backend's egress to loopback addresses (`127.0.0.1`, `::1`).
    ///
    /// The SSRF guard blocks loopback by default so an untrusted backend can't
    /// reach a service bound to localhost. Enable this ONLY for tests or local
    /// development that point the backend at a mock upstream on loopback —
    /// never for an installed/untrusted backend. Only loopback is relaxed;
    /// link-local, private, and the cloud-metadata endpoint stay blocked.
    #[must_use]
    pub fn permit_loopback_egress(mut self) -> Self {
        self.allow_loopback = true;
        self
    }

    /// Mark this backend's active model as realtime-only. Test-only opt-in;
    /// production sets this through [`Self::with_info`].
    #[must_use]
    pub fn with_realtime(mut self) -> Self {
        self.realtime = true;
        self
    }

    /// Convenience constructor used by the `OpenAI` test harness: synthesizes
    /// an `OpenAI` model identity from `model_id`.
    ///
    /// # Errors
    /// Returns an error if the component cannot be loaded or linked.
    pub fn new(
        component_path: &Path,
        allowed_hosts: Vec<String>,
        model_id: String,
        request_headers: Vec<(String, String)>,
    ) -> Result<Self> {
        let info = ModelInfoData::new(
            model_id,
            "github.com/super-tts/openai",
            true,
            true,
            Duration::from_secs(1),
        );
        Self::with_info(
            component_path,
            allowed_hosts,
            Vec::new(),
            info,
            request_headers,
            false,
            false,
        )
    }

    /// Like [`Self::new`] but loads the component as a websocket-capable
    /// (realtime) backend. Used by realtime backends (e.g. Mistral) whose
    /// component targets the `realtime-backend` world.
    ///
    /// # Errors
    /// Returns an error if the component cannot be loaded or linked.
    pub fn new_realtime(
        component_path: &Path,
        allowed_hosts: Vec<String>,
        model_id: String,
        request_headers: Vec<(String, String)>,
    ) -> Result<Self> {
        let info = ModelInfoData::new(
            model_id,
            "github.com/super-tts/mistral",
            true,
            true,
            Duration::from_secs(1),
        );
        Self::with_info(
            component_path,
            allowed_hosts,
            Vec::new(),
            info,
            request_headers,
            true,
            false,
        )
    }

    /// Reject a component that imports interfaces a sandboxed backend must not
    /// have. WASM backends may import only the `wasi:cli` / `http` / `io` /
    /// `clocks` / `random` interfaces their Rust runtime and the `/v1`
    /// contract need; importing e.g. `wasi:sockets` or `wasi:filesystem` is
    /// refused, so the only network egress is the allowlisted
    /// `wasi:http/outgoing-handler`.
    fn verify_imports(engine: &Engine, component: &Component) -> Result<()> {
        const ALLOWED: &[&str] = &[
            "wasi:cli/",
            "wasi:http/",
            "wasi:io/",
            "wasi:clocks/",
            "wasi:random/",
            // Websocket-capable backends import the daemon-implemented
            // `super-tts:realtime/ws`; a non-ws backend simply won't import it.
            "super-tts:realtime/",
        ];
        for (name, _) in component.component_type().imports(engine) {
            let interface = name.split('@').next().unwrap_or(name);
            if !ALLOWED.iter().any(|p| interface.starts_with(p)) {
                bail!(
                    "backend imports disallowed interface `{name}`: WASM backends \
                     may not access raw sockets or the filesystem"
                );
            }
        }
        Ok(())
    }

    /// Drive one `/v1` request through the component in-process and return its
    /// `(status, body)`.
    async fn invoke(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<(u16, Vec<u8>)> {
        let host = Host {
            table: ResourceTable::new(),
            wasi: WasiCtx::builder().build(),
            http: WasiHttpCtx::new(),
            hooks: self.allowlist_hooks(),
        };
        let mut store = Store::new(&self.engine, host);

        let mut builder = hyper::Request::builder()
            .method(method)
            .uri(format!("http://backend.local{path}"));
        for (key, value) in headers {
            builder = builder.header(key.as_str(), value.as_str());
        }
        let request = builder
            .body(
                http_body_util::Full::new(bytes::Bytes::from(body))
                    .map_err(|never: std::convert::Infallible| -> ErrorCode { match never {} }),
            )
            .context("building backend request")?;

        let (tx, rx) = tokio::sync::oneshot::channel();
        let req = store
            .data_mut()
            .http()
            .new_incoming_request(Scheme::Http, request)?;
        let out = store.data_mut().http().new_response_outparam(tx)?;
        // Both worlds export `wasi:http/incoming-handler`, so batch `/v1`
        // works for a realtime backend's non-realtime models too.
        match &self.pre {
            BackendPre::Http(p) => {
                let proxy = p.instantiate_async(&mut store).await?;
                proxy
                    .wasi_http_incoming_handler()
                    .call_handle(&mut store, req, out)
                    .await?;
            }
            BackendPre::Realtime(p) => {
                let inst = p.instantiate_async(&mut store).await?;
                inst.wasi_http_incoming_handler()
                    .call_handle(&mut store, req, out)
                    .await?;
            }
        }

        let response = rx
            .await
            .context("backend produced no response")?
            .map_err(|e| anyhow!("backend transport error: {e:?}"))?;
        let status = response.status().as_u16();
        let collected = response.into_body().collect().await?.to_bytes();
        Ok((status, collected.to_vec()))
    }

    /// `POST /v1/synthesize` — drive one synthesis and feed `sink` as frames
    /// decode.
    ///
    /// Unlike [`Self::invoke`], the guest call and the body read run
    /// **concurrently**. A `wasi:http` guest hands over its response head
    /// (`ResponseOutparam::set`) before it starts writing the body, so awaiting
    /// the guest to completion first — which is what `invoke` does, correctly,
    /// for the small JSON routes — would buffer the whole utterance and make
    /// time-to-first-audio equal total synthesis time. Joining the two lets the
    /// daemon start playing while the component is still producing.
    ///
    /// # Errors
    /// Returns an error if the component cannot be invoked, the response is not
    /// `200`, the headers do not describe usable audio, or the frame stream is
    /// malformed.
    pub async fn synthesize(
        &self,
        req: &crate::tts_models::v1::SynthesizeRequest<'_>,
        sink: &mut (dyn crate::tts_models::v1::SynthesisSink + Send),
    ) -> Result<()> {
        let host = Host {
            table: ResourceTable::new(),
            wasi: WasiCtx::builder().build(),
            http: WasiHttpCtx::new(),
            hooks: self.allowlist_hooks(),
        };
        let mut store = Store::new(&self.engine, host);

        let body = crate::tts_models::v1::build_synthesize_body(req)?;
        let mut builder = hyper::Request::builder()
            .method("POST")
            .uri("http://backend.local/v1/synthesize")
            .header("content-type", "application/json");
        for (key, value) in &self.request_headers {
            builder = builder.header(key.as_str(), value.as_str());
        }
        builder = builder.header("x-tts-model", self.model_id.as_str());
        let request = builder
            .body(
                http_body_util::Full::new(bytes::Bytes::from(body))
                    .map_err(|never: std::convert::Infallible| -> ErrorCode { match never {} }),
            )
            .context("building synthesize request")?;

        let (tx, rx) = tokio::sync::oneshot::channel();
        let incoming = store
            .data_mut()
            .http()
            .new_incoming_request(Scheme::Http, request)?;
        let out = store.data_mut().http().new_response_outparam(tx)?;

        // The guest half: runs the component to completion, writing the body as
        // it goes.
        let guest = async {
            match &self.pre {
                BackendPre::Http(p) => {
                    let proxy = p.instantiate_async(&mut store).await?;
                    proxy
                        .wasi_http_incoming_handler()
                        .call_handle(&mut store, incoming, out)
                        .await
                }
                BackendPre::Realtime(p) => {
                    let inst = p.instantiate_async(&mut store).await?;
                    inst.wasi_http_incoming_handler()
                        .call_handle(&mut store, incoming, out)
                        .await
                }
            }
        };

        // The reader half: takes the head as soon as the guest publishes it,
        // then decodes frames off the body while the guest keeps writing. It
        // touches no wasmtime state, so it can borrow nothing from `store`.
        let reader = async {
            let response = rx
                .await
                .context("backend produced no response")?
                .map_err(|e| anyhow!("backend transport error: {e:?}"))?;
            let status = response.status().as_u16();
            let (parts, body) = response.into_parts();
            if status != 200 {
                let collected = body.collect().await?.to_bytes();
                return Err(crate::tts_models::v1::synthesize_error(status, &collected));
            }
            crate::tts_models::v1::pump_synthesis(&parts.headers, body, sink).await
        };

        let (guest_result, read_result) = tokio::join!(guest, reader);
        // Report the read failure first: it names what was wrong with the
        // stream, whereas a guest trap on a half-written body is usually the
        // downstream symptom.
        read_result?;
        guest_result?;
        Ok(())
    }

    /// `POST /v1/voices` — hand the backend one cloned voice's reference
    /// audio.
    ///
    /// # Errors
    /// Returns an error if the component cannot be invoked or refuses the
    /// clip.
    pub async fn register_voice(
        &self,
        req: &crate::tts_models::v1::RegisterVoiceRequest<'_>,
    ) -> Result<()> {
        let body = crate::tts_models::v1::build_register_voice_body(req)?;
        let (status, resp) = self
            .invoke("POST", "/v1/voices", &self.voice_headers(), body)
            .await?;
        if (200..300).contains(&status) {
            return Ok(());
        }
        Err(crate::tts_models::v1::voice_error(status, &resp))
    }

    /// `DELETE /v1/voices/{voice}` — drop a registered cloned voice. A `404`
    /// is success: the goal is that the backend not hold the voice.
    ///
    /// # Errors
    /// Returns an error if the component cannot be invoked or refuses.
    pub async fn unregister_voice(&self, voice: &str) -> Result<()> {
        let path = format!("/v1/voices/{}", urlencoding::encode(voice));
        let (status, resp) = self
            .invoke("DELETE", &path, &self.voice_headers(), Vec::new())
            .await?;
        if status == 404 || (200..300).contains(&status) {
            return Ok(());
        }
        Err(crate::tts_models::v1::voice_error(status, &resp))
    }

    /// Headers for the voice routes: whatever the backend's configuration
    /// injects, plus the model the voice is being registered against — a
    /// backend serving several models cannot assume one embedding shape.
    fn voice_headers(&self) -> Vec<(String, String)> {
        let mut headers = self.request_headers.clone();
        headers.push(("content-type".to_string(), "application/json".to_string()));
        headers.push(("x-tts-model".to_string(), self.model_id.clone()));
        headers
    }

    /// `GET /v1/status` — readiness snapshot.
    ///
    /// # Errors
    /// Returns an error if the component cannot be invoked or its response is
    /// not valid JSON.
    pub async fn status(&self) -> Result<serde_json::Value> {
        let (_, body) = self.invoke("GET", "/v1/status", &[], Vec::new()).await?;
        Ok(serde_json::from_slice(&body)?)
    }

    /// `GET /v1/ping` — liveness.
    ///
    /// # Errors
    /// Returns an error if the component cannot be invoked or its response is
    /// not valid JSON.
    pub async fn ping(&self) -> Result<serde_json::Value> {
        let (_, body) = self.invoke("GET", "/v1/ping", &[], Vec::new()).await?;
        Ok(serde_json::from_slice(&body)?)
    }
}

impl ModelInfo for WasmBackend {
    fn info(&self) -> &ModelInfoData {
        &self.info
    }
}

impl ModelState for WasmBackend {
    /// WASM backends front a remote API; they have no local compute device.
    fn device(&self) -> String {
        "remote".to_string()
    }
}

#[async_trait]
impl Synthesize for WasmBackend {
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

    /// Run one consumer realtime session: instantiate the component and invoke
    /// its `super-tts:realtime/ws-server.handle` export with the daemon-injected
    /// headers and a host-owned consumer stream. Returns when the guest's
    /// handler returns. Only valid for websocket-capable backends.
    ///
    /// # Errors
    /// Returns an error if the backend is not realtime-capable, instantiation
    /// fails, or the guest's handler returns a `ws-error`.
    #[cfg(feature = "wasm-backends")]
    async fn realtime_session(&self, transport: ws_host::ConsumerStreamTransport) -> Result<()> {
        let BackendPre::Realtime(pre) = &self.pre else {
            bail!("backend is not websocket-capable");
        };
        let host = Host {
            table: ResourceTable::new(),
            wasi: WasiCtx::builder().build(),
            http: WasiHttpCtx::new(),
            hooks: self.allowlist_hooks(),
        };
        let mut store = Store::new(&self.engine, host);
        // Inject the same x-tts-* headers a batch call gets, plus the model id.
        let mut headers: Vec<(String, Vec<u8>)> = self
            .request_headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone().into_bytes()))
            .collect();
        headers.push((
            "x-tts-model".to_string(),
            self.model_id.clone().into_bytes(),
        ));
        let consumer = store
            .data_mut()
            .table
            .push(ws_host::ConsumerStreamResource::new(transport))?;
        let inst = pre.instantiate_async(&mut store).await?;
        inst.super_tts_realtime_ws_server()
            .call_handle(&mut store, &headers, consumer)
            .await?
            .map_err(|e| anyhow!("ws-server.handle returned error: {e:?}"))
    }
}
