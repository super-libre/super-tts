// SPDX-License-Identifier: GPL-3.0-only
//! The speak path: text in, audio out of the speakers.
//!
//! Sits between the `/speak` endpoint and the two halves already built — the
//! backend's `POST /v1/synthesize` ([`crate::stt_models::v1`]) and the playback
//! pipeline ([`crate::audio::playback`]) — and owns the small amount of state
//! that belongs to neither: which utterance is current, and how to stop it.
//!
//! **A channel bridges the sink and the ring.** [`SynthesisSink::on_frame`] is
//! synchronous (it is called from inside the frame decoder), while
//! [`Playback::push`] is async because it waits for ring space. So the sink
//! decodes samples and hands them to a pump task over a channel, and the pump
//! does the awaiting. This is what makes playback start while the backend is
//! still producing, rather than after it finishes.
//!
//! **The audio device is opened lazily, on a dedicated thread.** cpal's stream
//! is `!Send` on several backends, so it cannot live in the daemon's shared
//! state alongside everything the async tasks touch. The thread opens the
//! device, sends the [`Playback`] producer half back, and parks holding the
//! stream. Opening lazily rather than at startup keeps a daemon that never
//! speaks from claiming an output device it will not use.
//!
//! **One utterance at a time, newest wins.** A second `/speak` cancels the
//! first rather than queueing behind it. That is the v1 policy, not the end
//! state: every utterance already carries an id, so a real queue with priority
//! and ducking can be added without changing the endpoint's shape.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, anyhow, bail};
use log::{debug, info, warn};
use parking_lot::Mutex;
use super_tts_shared::audio::frames::{AudioParams, Frame, FrameKind};
use tokio::sync::{mpsc, oneshot};

use crate::audio::playback::{DeviceFormat, Playback};
use crate::daemon::types::SharedLoadedModel;
use crate::stt_models::v1::{SynthesisSink, SynthesizeRequest};
use crate::text::chunk::{ChunkPolicy, Chunker};

/// Longest `text` accepted when the model declares no `max_input_chars`.
///
/// A bound has to exist regardless of the manifest: `text` arrives from an
/// authorized client, and an unbounded one would hold a backend and the output
/// device for as long as it liked.
pub const MAX_TEXT_CHARS: usize = 50_000;

/// A running or finished utterance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Utterance {
    /// Opaque id the client cancels or correlates events by.
    pub id: String,
    /// Alignment marks the backend emitted.
    pub marks: usize,
    /// How many synthesis chunks the text was split into.
    pub chunks: usize,
}

/// How an utterance should be spoken, apart from the text itself.
///
/// Grouped so the per-chunk loop can carry them without a closure: a closure
/// returning `SynthesizeRequest<'_>` ties the request's lifetime to the chunk,
/// which does not outlive the borrow of these.
#[derive(Debug, Default, Clone, Copy)]
pub struct SpeakOptions<'a> {
    /// A `voice` id the model declares, or `None` for its `default_voice`.
    pub voice: Option<&'a str>,
    /// Resolved BCP-47 tag.
    pub language: Option<&'a str>,
    /// Rate multiplier.
    pub speed: Option<f32>,
    /// Free-text delivery guidance.
    pub instructions: Option<&'a str>,
}

impl SpeakOptions<'_> {
    /// Copy into owned strings so a session can outlive the request that
    /// started it.
    fn into_owned(self) -> OwnedSpeakOptions {
        OwnedSpeakOptions {
            voice: self.voice.map(str::to_owned),
            language: self.language.map(str::to_owned),
            speed: self.speed,
            instructions: self.instructions.map(str::to_owned),
        }
    }
}

/// [`SpeakOptions`] with owned strings, held by a [`SpeakSession`].
#[derive(Debug, Default, Clone)]
struct OwnedSpeakOptions {
    voice: Option<String>,
    language: Option<String>,
    speed: Option<f32>,
    instructions: Option<String>,
}

impl OwnedSpeakOptions {
    fn as_ref(&self) -> SpeakOptions<'_> {
        SpeakOptions {
            voice: self.voice.as_deref(),
            language: self.language.as_deref(),
            speed: self.speed,
            instructions: self.instructions.as_deref(),
        }
    }
}

/// Something worth telling a streaming client about, as it happens.
#[derive(Debug, Clone, PartialEq)]
pub enum SpeechEvent {
    /// The backend aligned a span of audio to a span of the text.
    Mark(super_tts_shared::audio::frames::Mark),
}

/// Why a speak request could not be served.
#[derive(Debug, thiserror::Error)]
pub enum SpeakError {
    /// `text` was empty or whitespace only.
    #[error("empty_text")]
    EmptyText,
    /// `text` exceeded [`MAX_TEXT_CHARS`].
    #[error("text_too_long")]
    TextTooLong,
    /// No model is loaded.
    #[error("model_not_loaded")]
    NotLoaded,
    /// The audio device could not be opened.
    #[error("audio_device_unavailable: {0}")]
    Device(String),
    /// The backend failed.
    #[error("synthesis_failed: {0}")]
    Synthesis(String),
}

/// What the sink hands the pump.
///
/// A chunk boundary travels through the same channel as the audio rather than
/// being applied directly, so it stays *ordered* with the samples around it.
/// Calling `end_chunk` from the sink would fade a seam into whatever the pump
/// happened to be pushing at that moment.
enum PcmMsg {
    /// Decoded samples, with the format they were decoded in.
    Samples(Vec<f32>, AudioParams),
    /// The synthesis response ended; the next one is a new prosodic unit.
    Boundary,
}

/// Decodes frames and forwards samples to the pump task.
///
/// Holds back any trailing partial sample: a backend may end a frame mid-sample
/// (frame boundaries are its choice, and nothing requires them to be
/// sample-aligned), so the straddling bytes belong to whatever comes next.
struct PlaybackSink {
    tx: mpsc::UnboundedSender<PcmMsg>,
    params: Option<AudioParams>,
    /// Undecoded bytes: a partial sample carried across frames.
    leftover: Vec<u8>,
    marks: usize,
    cancelled: Arc<AtomicBool>,
    /// Marks are forwarded as they decode so a streaming client can highlight
    /// along with the audio; the one-shot path simply drops them.
    events: mpsc::UnboundedSender<SpeechEvent>,
}

impl PlaybackSink {
    /// Bytes per interleaved frame in the declared format.
    fn frame_width(params: AudioParams) -> usize {
        (params.format.bytes_per_sample() * params.channels.max(1) as usize).max(1)
    }

    /// Close the current synthesis chunk.
    ///
    /// Any straddling bytes are dropped: a well-formed response ends on a whole
    /// sample, and carrying a fragment into the next response — which may not
    /// even share a format — would shift every sample after it by a byte.
    fn boundary(&mut self) {
        self.leftover.clear();
        self.params = None;
        let _ = self.tx.send(PcmMsg::Boundary);
    }
}

impl SynthesisSink for PlaybackSink {
    fn on_params(&mut self, params: AudioParams) -> Result<()> {
        self.params = Some(params);
        Ok(())
    }

    fn on_frame(&mut self, frame: Frame) -> Result<()> {
        if self.cancelled.load(Ordering::Relaxed) {
            // An error is how the sink tells the pump to stop reading the
            // backend. The caller maps it back to "cancelled", not a failure.
            bail!("cancelled");
        }
        match frame.kind {
            FrameKind::Audio => {
                let Some(params) = self.params else {
                    bail!("audio frame before response headers were read");
                };
                self.leftover.extend_from_slice(&frame.payload);
                let width = Self::frame_width(params);
                let usable = self.leftover.len() - (self.leftover.len() % width);
                if usable > 0 {
                    let bytes: Vec<u8> = self.leftover.drain(..usable).collect();
                    let samples = params.format.decode(&bytes);
                    // Unbounded, but bounded in practice by the frame decoder's
                    // MAX_TOTAL_AUDIO_BYTES cap on one response. Real
                    // backpressure lives at the ring, which the pump awaits.
                    let _ = self.tx.send(PcmMsg::Samples(samples, params));
                }
            }
            FrameKind::Mark => {
                self.marks += 1;
                if let Ok(mark) = serde_json::from_slice(&frame.payload) {
                    let _ = self.events.send(SpeechEvent::Mark(mark));
                }
            }
            FrameKind::Done | FrameKind::Error => {}
        }
        Ok(())
    }
}

/// The utterance currently in flight: its id and its cancel flag.
///
/// Shared with the live [`SpeakSession`] so the session can release the slot
/// when it ends, and so a cancel arriving from anywhere reaches the flag the
/// session is checking.
type CurrentSlot = Arc<Mutex<Option<(String, Arc<AtomicBool>)>>>;

/// Owns the output device and the current utterance.
pub struct SpeechEngine {
    /// `None` until the first utterance opens the device.
    playback: tokio::sync::Mutex<Option<Arc<Playback>>>,
    /// The in-flight utterance, if any.
    current: CurrentSlot,
    /// Overrides the real device; set by tests to run with no audio hardware.
    test_format: Option<DeviceFormat>,
}

impl std::fmt::Debug for SpeechEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpeechEngine")
            .field("current", &self.current())
            .finish_non_exhaustive()
    }
}

impl Default for SpeechEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl SpeechEngine {
    /// An engine that opens the default output device on first use.
    #[must_use]
    pub fn new() -> Self {
        Self {
            playback: tokio::sync::Mutex::new(None),
            current: CurrentSlot::default(),
            test_format: None,
        }
    }

    /// An engine backed by a detached (null-sink) pipeline at `format`.
    ///
    /// Exercises every step of the speak path except the cpal binding, so the
    /// endpoint can be tested on a machine with no audio device.
    #[must_use]
    pub fn detached(format: DeviceFormat) -> Self {
        Self {
            playback: tokio::sync::Mutex::new(None),
            current: CurrentSlot::default(),
            test_format: Some(format),
        }
    }

    /// The id of the utterance currently in flight, if any.
    #[must_use]
    pub fn current(&self) -> Option<String> {
        self.current.lock().as_ref().map(|(id, _)| id.clone())
    }

    /// The playback handle, for tests that inspect what was rendered.
    pub async fn playback_handle(&self) -> Option<Arc<Playback>> {
        self.playback.lock().await.as_ref().map(Arc::clone)
    }

    /// The playback handle, opening the device on first use.
    async fn playback(&self) -> Result<Arc<Playback>, SpeakError> {
        let mut guard = self.playback.lock().await;
        if let Some(p) = guard.as_ref() {
            return Ok(Arc::clone(p));
        }
        let playback = match self.test_format {
            Some(format) => Playback::detached(format),
            None => open_device_thread().await?,
        };
        let handle = Arc::new(playback);
        *guard = Some(Arc::clone(&handle));
        Ok(handle)
    }

    /// Stop speaking: flush queued audio and abort any synthesis still running.
    ///
    /// Returns the id of the utterance that was still *synthesizing*, if any.
    /// The ring is flushed either way, and that distinction matters: `speak`
    /// returns once the last sample is queued, so by the time a user hits stop
    /// the utterance is usually finished producing but still coming out of the
    /// speakers. Gating the flush on an in-flight synthesis would make cancel a
    /// no-op for exactly the case people actually use it in.
    pub async fn cancel(&self) -> Option<String> {
        let taken = self.current.lock().take();
        if let Some((_, flag)) = taken.as_ref() {
            flag.store(true, Ordering::Relaxed);
        }
        if let Some(p) = self.playback.lock().await.as_ref() {
            p.cancel();
        }
        let id = taken.map(|(id, _)| id);
        if let Some(id) = &id {
            debug!("cancelled in-flight utterance {id}");
        }
        id
    }

    /// Begin a streaming utterance.
    ///
    /// Text is fed in with [`SpeakSession::push`] and completed with
    /// [`SpeakSession::end`]. This is the path an LLM drives: chunks are
    /// synthesized as sentences complete, so speech starts long before the
    /// model has finished generating.
    ///
    /// # Errors
    /// See [`SpeakError`].
    pub async fn begin(
        &self,
        model: &SharedLoadedModel,
        options: SpeakOptions<'_>,
    ) -> Result<SpeakSession, SpeakError> {
        let policy = {
            let guard = model.read().await;
            let loaded = guard.as_ref().ok_or(SpeakError::NotLoaded)?;
            ChunkPolicy::for_model(loaded.definition.max_input_chars)
        };

        // A new utterance supersedes the old. Cancelling before claiming the
        // slot means the ring is already flushed when the first chunk of the
        // new utterance lands, so the two never overlap.
        self.cancel().await;

        let id = format!("utt_{}", uuid::Uuid::new_v4());
        let flag = Arc::new(AtomicBool::new(false));
        *self.current.lock() = Some((id.clone(), Arc::clone(&flag)));

        let playback = self.playback().await?;
        let (tx, mut rx) = mpsc::unbounded_channel::<PcmMsg>();

        // The pump owns the awaiting: it is where ring backpressure is felt,
        // and it runs concurrently with the backend read so audio starts before
        // synthesis finishes.
        let pump = tokio::spawn({
            let playback = Arc::clone(&playback);
            async move {
                while let Some(msg) = rx.recv().await {
                    match msg {
                        PcmMsg::Samples(samples, params) => {
                            if let Err(e) = playback.push(&samples, params).await {
                                warn!("playback push failed: {e}");
                                break;
                            }
                        }
                        PcmMsg::Boundary => playback.end_chunk().await,
                    }
                }
            }
        });

        let (events_tx, events_rx) = mpsc::unbounded_channel::<SpeechEvent>();
        Ok(SpeakSession {
            slot: Arc::clone(&self.current),
            id,
            model: Arc::clone(model),
            options: options.into_owned(),
            normalizer: crate::text::normalize::Normalizer::new(),
            chunker: Chunker::new(policy),
            sink: Some(PlaybackSink {
                tx,
                params: None,
                leftover: Vec::new(),
                marks: 0,
                cancelled: Arc::clone(&flag),
                events: events_tx,
            }),
            events: events_rx,
            pump: Some(pump),
            playback,
            flag,
            chars: 0,
            marks: 0,
            chunks: 0,
            finished: false,
        })
    }

    /// Synthesize `text` with the loaded model and play it.
    ///
    /// Returns once the backend has finished producing and every sample has
    /// been queued; the device plays the tail out afterwards. A second call
    /// cancels the first.
    ///
    /// A thin wrapper over [`begin`](Self::begin): routing the one-shot path
    /// through the same session means the two cannot drift on normalization,
    /// chunking, or seam handling.
    ///
    /// # Errors
    /// See [`SpeakError`].
    pub async fn speak(
        &self,
        model: &SharedLoadedModel,
        text: &str,
        voice: Option<&str>,
        language: Option<&str>,
        speed: Option<f32>,
        instructions: Option<&str>,
    ) -> Result<Utterance, SpeakError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(SpeakError::EmptyText);
        }
        if trimmed.chars().count() > MAX_TEXT_CHARS {
            return Err(SpeakError::TextTooLong);
        }
        // Checked before claiming the utterance slot, so a request that cannot
        // be served never interrupts what is already playing.
        if model.read().await.is_none() {
            return Err(SpeakError::NotLoaded);
        }
        if crate::text::normalize::normalize(trimmed).is_empty() {
            // The input was entirely markup. Nothing to say is not a failure,
            // but it is not an utterance either.
            return Err(SpeakError::EmptyText);
        }

        let options = SpeakOptions {
            voice,
            language,
            speed,
            instructions,
        };
        let mut session = self.begin(model, options).await?;
        session.push(text).await?;
        session.end().await
    }
}

/// One streaming utterance.
///
/// Deltas go in with [`push`](Self::push); [`end`](Self::end) completes it.
/// Sentences are synthesized as they complete, so speech starts while the
/// producer is still writing — which is the entire point of the streaming path.
///
/// Dropping a session without calling `end` cancels it: the pump is stopped and
/// the ring flushed, so an abandoned connection cannot leave audio playing.
pub struct SpeakSession {
    slot: CurrentSlot,
    id: String,
    model: SharedLoadedModel,
    options: OwnedSpeakOptions,
    normalizer: crate::text::normalize::Normalizer,
    chunker: Chunker,
    /// Taken in `end`/`cancel` to close the channel and stop the pump.
    sink: Option<PlaybackSink>,
    events: mpsc::UnboundedReceiver<SpeechEvent>,
    pump: Option<tokio::task::JoinHandle<()>>,
    playback: Arc<Playback>,
    flag: Arc<AtomicBool>,
    chars: usize,
    marks: usize,
    chunks: usize,
    finished: bool,
}

impl std::fmt::Debug for SpeakSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpeakSession")
            .field("id", &self.id)
            .field("chunks", &self.chunks)
            .finish_non_exhaustive()
    }
}

impl SpeakSession {
    /// The utterance id, for correlating events and cancellation.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// How many synthesis chunks have been queued so far.
    ///
    /// Grows as sentences complete, which is what makes the streaming path
    /// observable: it is non-zero before `end` is ever called.
    #[must_use]
    pub fn chunks(&self) -> usize {
        self.chunks
    }

    /// Whether the session has been cancelled out from under it — by a later
    /// utterance, or by an explicit stop.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    /// Take the next pending event, if any. Non-blocking.
    pub fn try_next_event(&mut self) -> Option<SpeechEvent> {
        self.events.try_recv().ok()
    }

    /// Milliseconds spoken and still queued, for a progress report.
    ///
    /// Both are derived from the device format rather than wall-clock time, so
    /// they describe a position in the utterance and stay correct while the
    /// stream is idle.
    #[must_use]
    pub fn progress(&self) -> (u64, u64) {
        let format = self.playback.format();
        let per_ms = u64::from(format.sample_rate) * u64::from(format.channels.max(1)) / 1000;
        if per_ms == 0 {
            return (0, 0);
        }
        let stats = self.playback.stats();
        let queued = self.playback.buffered() as u64;
        (stats.rendered_samples / per_ms, queued / per_ms)
    }

    /// Feed a delta.
    ///
    /// Synthesizes and queues every sentence the delta completed. Text that
    /// does not yet form a whole sentence stays buffered.
    ///
    /// # Errors
    /// See [`SpeakError`].
    pub async fn push(&mut self, delta: &str) -> Result<(), SpeakError> {
        if self.finished || self.is_cancelled() {
            return Ok(());
        }
        self.chars = self.chars.saturating_add(delta.chars().count());
        if self.chars > MAX_TEXT_CHARS {
            return Err(SpeakError::TextTooLong);
        }
        let normalized = self.normalizer.push(delta);
        let chunks = self.chunker.push(&normalized);
        for chunk in chunks {
            self.synthesize(&chunk).await?;
        }
        Ok(())
    }

    /// Complete the utterance: flush the tail through both stages, synthesize
    /// it, and let the ring drain.
    ///
    /// # Errors
    /// See [`SpeakError`].
    pub async fn end(&mut self) -> Result<Utterance, SpeakError> {
        if !self.finished && !self.is_cancelled() {
            let tail = self.normalizer.finish();
            let mut chunks = self.chunker.push(&tail);
            chunks.extend(self.chunker.finish());
            for chunk in chunks {
                self.synthesize(&chunk).await?;
            }
        }
        self.finished = true;

        // Closing the channel ends the pump once it has drained, so every
        // sample is queued before the ring is told the utterance is over.
        self.sink = None;
        if let Some(pump) = self.pump.take() {
            let _ = pump.await;
        }

        if !self.is_cancelled() {
            self.playback.finish().await;
            info!(
                "utterance {}: {} chars in {} chunk(s), {} marks",
                self.id, self.chars, self.chunks, self.marks
            );
        }
        self.release_slot();
        Ok(Utterance {
            id: self.id.clone(),
            marks: self.marks,
            chunks: self.chunks,
        })
    }

    /// Abandon the utterance: stop the pump and drop queued audio.
    pub async fn cancel(&mut self) {
        self.flag.store(true, Ordering::Relaxed);
        self.finished = true;
        self.sink = None;
        if let Some(pump) = self.pump.take() {
            let _ = pump.await;
        }
        self.playback.cancel();
        self.release_slot();
    }

    /// Release the engine's slot, but only if it still holds this utterance —
    /// a later one may already have claimed it.
    fn release_slot(&self) {
        let mut guard = self.slot.lock();
        if guard.as_ref().is_some_and(|(cur, _)| *cur == self.id) {
            *guard = None;
        }
    }

    /// Synthesize one chunk and queue its audio.
    async fn synthesize(&mut self, chunk: &str) -> Result<(), SpeakError> {
        if chunk.trim().is_empty() || self.is_cancelled() {
            return Ok(());
        }
        let Some(sink) = self.sink.as_mut() else {
            return Ok(());
        };
        // Between chunks only, never before the first: the boundary fades a
        // seam, and there is nothing to fade against at the start.
        if self.chunks > 0 {
            sink.boundary();
        }

        let opts = self.options.as_ref();
        let request = SynthesizeRequest {
            text: chunk,
            voice: opts.voice,
            language: opts.language,
            speed: opts.speed,
            instructions: opts.instructions,
        };
        let result = {
            let guard = self.model.read().await;
            let Some(loaded) = guard.as_ref() else {
                return Err(SpeakError::NotLoaded);
            };
            loaded.instance.synthesize(&request, sink).await
        };
        self.marks = sink.marks;

        match result {
            Ok(()) => {
                self.chunks += 1;
                Ok(())
            }
            // A cancel races the backend by design: the sink aborts the frame
            // pump, which surfaces here as an error the caller must not report
            // as a failure.
            Err(_) if self.is_cancelled() => Ok(()),
            Err(e) => {
                self.playback.cancel();
                Err(SpeakError::Synthesis(e.to_string()))
            }
        }
    }
}

impl Drop for SpeakSession {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // An abandoned session — a dropped WebSocket, a panicking handler —
        // must not leave audio playing. The pump ends on its own once the sink
        // is gone; the ring is flushed synchronously here because `Drop` has no
        // await.
        self.flag.store(true, Ordering::Relaxed);
        self.sink = None;
        if let Some(pump) = self.pump.take() {
            pump.abort();
        }
        self.playback.cancel();
        self.release_slot();
    }
}

/// Open the default output device on a dedicated thread and return the producer
/// half.
///
/// The thread keeps the `!Send` cpal stream and parks; it ends with the
/// process. This is the whole reason the stream is split from [`Playback`].
async fn open_device_thread() -> Result<Playback, SpeakError> {
    let (tx, rx) = oneshot::channel::<Result<Playback>>();
    std::thread::Builder::new()
        .name("super-tts-playback".into())
        .spawn(move || {
            let opened = (|| {
                use cpal::traits::{DeviceTrait, HostTrait};
                let host = cpal::default_host();
                let device = host
                    .default_output_device()
                    .ok_or_else(|| anyhow!("no default output device"))?;
                let config = device.default_output_config()?;
                Playback::open(&device, &config)
            })();

            match opened {
                Ok((playback, stream)) => {
                    if tx.send(Ok(playback)).is_err() {
                        return; // caller gave up; dropping `stream` closes the device
                    }
                    // Hold the stream for the process's life. Parking keeps the
                    // thread off the scheduler entirely.
                    let _stream = stream;
                    loop {
                        std::thread::park();
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
            }
        })
        .map_err(|e| SpeakError::Device(e.to_string()))?;

    match rx.await {
        Ok(Ok(p)) => Ok(p),
        Ok(Err(e)) => {
            warn!("playback device unavailable: {e}");
            Err(SpeakError::Device(e.to_string()))
        }
        Err(_) => Err(SpeakError::Device("playback thread died".into())),
    }
}

impl crate::daemon::types::SuperTTSDaemon {
    /// Handle `Command::Speak`.
    ///
    /// Returns as soon as the backend has finished producing and every sample
    /// is queued; the device plays out the tail afterwards. Errors are mapped
    /// to the same coded shape the transcribe paths use, so a client switches
    /// on `error_code` rather than parsing prose.
    pub async fn handle_speak(
        &self,
        text: String,
        voice: Option<String>,
        language: Option<String>,
        speed: Option<f32>,
        instructions: Option<String>,
    ) -> super_tts_shared::models::protocol::DaemonResponse {
        use super_tts_shared::models::protocol::{DaemonResponse, ErrorCode};

        let result = self
            .speech
            .speak(
                &self.model,
                &text,
                voice.as_deref(),
                language.as_deref(),
                speed,
                instructions.as_deref(),
            )
            .await;

        match result {
            Ok(utterance) => DaemonResponse::success().with_utterance_id(utterance.id),
            Err(e @ (SpeakError::EmptyText | SpeakError::TextTooLong)) => {
                DaemonResponse::error_with_code(ErrorCode::InvalidValue, &e.to_string())
            }
            Err(SpeakError::NotLoaded) => {
                DaemonResponse::error_with_code(ErrorCode::ModelNotLoaded, "model_not_loaded")
            }
            Err(e) => DaemonResponse::error(&e.to_string()),
        }
    }

    /// Handle `Command::StopSpeaking`. Stopping when nothing is speaking is a
    /// success, not an error — a client that cancels on a keypress should not
    /// have to know whether it won the race.
    pub async fn handle_stop_speaking(&self) -> super_tts_shared::models::protocol::DaemonResponse {
        use super_tts_shared::models::protocol::DaemonResponse;
        match self.speech.cancel().await {
            Some(id) => DaemonResponse::success().with_utterance_id(id),
            None => DaemonResponse::success(),
        }
    }
}
