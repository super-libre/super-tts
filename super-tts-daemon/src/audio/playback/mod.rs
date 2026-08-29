// SPDX-License-Identifier: GPL-3.0-only
//! Streaming speech playback: synthesis chunks in, a continuous audio stream
//! out.
//!
//! The daemon's existing output path ([`crate::audio::beeper`]) plays a
//! fully-precomputed, known-length buffer — "these exact 200 ms of sine wave".
//! Speech is the opposite: chunks arrive over time, at a rate that is not the
//! device's, from separate synthesis requests, and must not gap between them or
//! keep sounding once the user interrupts.
//!
//! Three pieces, split by which thread they run on:
//!
//! - [`mixer`] prepares a chunk — resample, channel-map, fade the seam.
//!   Producer side, pure, allocates freely.
//! - [`ring`] is the handoff: a preallocated buffer both sides memcpy through.
//! - [`Playback`] owns the state machine and the cpal stream. The audio
//!   callback only ever locks briefly and memcpys.
//!
//! Everything except the cpal binding is exercised through [`Playback::render`]
//! — the same entry point the callback uses — so the pipeline is testable
//! against a null sink with no audio device present.
//!
//! **Realtime discipline.** The crate builds with `panic = "abort"`, so a panic
//! on the audio callback thread takes the daemon down mid-utterance instead of
//! glitching. The callback path therefore has no indexing that can go out of
//! bounds, no unwrap, no allocation, and never blocks: it takes the state lock
//! with `try_lock` and renders silence if it cannot get it.

pub mod mixer;
pub mod ring;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use log::{debug, warn};
use parking_lot::Mutex;
use super_tts_shared::audio::frames::AudioParams;

pub use mixer::DeviceFormat;
use mixer::SeamFade;
use ring::Ring;

/// How much audio the ring can hold. Two seconds is enough to absorb a slow
/// chunk without the buffer itself becoming a latency floor.
const RING_SECONDS: u32 = 2;

/// How much audio must be buffered before the stream starts.
///
/// Starting on the first sample would underrun immediately on any backend
/// slower than realtime; waiting too long is dead air the user hears as lag.
/// 120 ms is roughly a syllable.
const PREBUFFER_MS: u32 = 120;

/// Default fade applied to each end of every chunk. Applied per end, so a seam
/// dips over roughly twice this. Long enough to hide the discontinuity between
/// two independently synthesized sentences, short enough to be inaudible in
/// speech — and sentence boundaries usually carry natural silence anyway.
const DEFAULT_FADE_MS: u32 = 8;

/// What the stream is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Nothing queued; the callback renders silence.
    Idle,
    /// Filling toward [`PREBUFFER_MS`] before the first sample is heard.
    Prebuffering,
    /// Audio is being consumed.
    Playing,
    /// The producer is done; play out what is buffered, then go idle.
    Draining,
}

/// Counters a settings UI or a test can read back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Interleaved samples the callback had to fill with silence because the
    /// ring was empty while playing.
    pub underrun_samples: u64,
    /// Times the callback could not take the state lock and rendered silence.
    /// Non-zero here is contention, which is a different bug from an underrun.
    pub lock_misses: u64,
}

/// State shared between the producer task and the audio callback.
#[derive(Debug)]
struct Shared {
    ring: Ring,
    state: State,
    prebuffer_samples: usize,
    stats: Stats,
}

impl Shared {
    /// Fill `out` from the ring, silencing whatever the ring could not supply.
    ///
    /// This is the whole consumer side. It runs on the audio callback thread,
    /// so it allocates nothing and cannot panic: `Ring::read` is a bounded
    /// memcpy and the remainder is a slice fill.
    fn render(&mut self, out: &mut [f32]) {
        match self.state {
            State::Idle | State::Prebuffering => {
                out.fill(0.0);
                return;
            }
            State::Playing | State::Draining => {}
        }

        let got = self.ring.read(out);
        if got < out.len() {
            out[got..].fill(0.0);
            if self.state == State::Playing {
                // Draining is expected to end short — that is the utterance
                // finishing, not a failure to keep up.
                self.stats.underrun_samples += (out.len() - got) as u64;
            }
        }
        if self.state == State::Draining && self.ring.is_empty() {
            self.state = State::Idle;
        }
    }

    /// Promote out of `Prebuffering` once enough is buffered.
    fn maybe_start(&mut self) {
        if self.state == State::Prebuffering && self.ring.available() >= self.prebuffer_samples {
            self.state = State::Playing;
        }
    }
}

/// The playback pipeline: producer API plus the output stream.
///
/// The cpal stream is optional so the pipeline can be built and driven with no
/// audio device — [`Playback::render`] is the same code the callback runs.
pub struct Playback {
    shared: Arc<Mutex<Shared>>,
    device: DeviceFormat,
    fade: SeamFade,
    /// Held to keep the stream alive; dropping it stops playback.
    stream: Option<cpal::Stream>,
}

// cpal's `Stream` is `!Send` on some backends because it is tied to the thread
// that created it. The daemon builds and drops a `Playback` on one task, and
// the callback owns only the `Arc<Mutex<Shared>>`, so the handle never actually
// moves across threads while a stream is live.
#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl Send for Playback {}

impl Playback {
    /// Build a pipeline for a device format without opening a stream.
    ///
    /// This is the null-sink constructor: everything except the cpal binding
    /// behaves exactly as it does in production, and [`Playback::render`] pulls
    /// samples the way the callback would.
    #[must_use]
    pub fn detached(device: DeviceFormat) -> Self {
        let capacity =
            (device.sample_rate * RING_SECONDS) as usize * device.channels.max(1) as usize;
        let prebuffer_samples = (u64::from(device.sample_rate) * u64::from(PREBUFFER_MS) / 1000)
            as usize
            * device.channels.max(1) as usize;
        Self {
            shared: Arc::new(Mutex::new(Shared {
                ring: Ring::new(capacity.max(1)),
                state: State::Idle,
                prebuffer_samples,
                stats: Stats::default(),
            })),
            device,
            fade: SeamFade::new(DEFAULT_FADE_MS, device),
            stream: None,
        }
    }

    /// Open an output stream on `device` and start it.
    ///
    /// The stream runs continuously, rendering silence while idle. Starting it
    /// once and leaving it running is deliberate: the first sound after a
    /// device wakes costs 1–2 s on a Bluetooth sink, and paying that on every
    /// utterance is exactly the lag the prebuffer is trying to avoid.
    ///
    /// # Errors
    /// Returns an error if the device's format is unsupported or the stream
    /// cannot be built or started.
    pub fn open(device: &cpal::Device, config: &cpal::SupportedStreamConfig) -> Result<Self> {
        let format = DeviceFormat {
            sample_rate: config.sample_rate(),
            channels: config.channels(),
        };
        let mut playback = Self::detached(format);
        let shared = Arc::clone(&playback.shared);

        let err_fn = |e| warn!("playback stream error: {e}");
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => device.build_output_stream(
                &config.config(),
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    // `try_lock`, never `lock`: blocking here misses the
                    // device deadline, and a rare silent buffer is a far
                    // better failure than a stall.
                    if let Some(mut guard) = shared.try_lock() {
                        guard.render(out);
                    } else {
                        out.fill(0.0);
                    }
                },
                err_fn,
                None,
            )?,
            cpal::SampleFormat::I16 => {
                // Scratch buffer allocated once, outside the callback.
                let mut scratch: Vec<f32> = Vec::new();
                device.build_output_stream(
                    &config.config(),
                    move |out: &mut [i16], _: &cpal::OutputCallbackInfo| {
                        if scratch.len() < out.len() {
                            // Grows only until it has seen the device's largest
                            // buffer, which cpal settles on within a few
                            // callbacks; steady state allocates nothing.
                            scratch.resize(out.len(), 0.0);
                        }
                        let block = &mut scratch[..out.len()];
                        if let Some(mut guard) = shared.try_lock() {
                            guard.render(block);
                        } else {
                            block.fill(0.0);
                        }
                        for (dst, src) in out.iter_mut().zip(block.iter()) {
                            *dst = crate::num_cast::f32_to_i16(
                                src.clamp(-1.0, 1.0) * f32::from(i16::MAX),
                            );
                        }
                    },
                    err_fn,
                    None,
                )?
            }
            other => anyhow::bail!("unsupported output sample format: {other}"),
        };
        stream.play().context("starting playback stream")?;
        playback.stream = Some(stream);
        debug!(
            "playback stream open at {} Hz, {} channels",
            format.sample_rate, format.channels
        );
        Ok(playback)
    }

    /// The device format this pipeline renders to.
    #[must_use]
    pub fn format(&self) -> DeviceFormat {
        self.device
    }

    /// Current state.
    #[must_use]
    pub fn state(&self) -> State {
        self.shared.lock().state
    }

    /// Underrun and contention counters.
    #[must_use]
    pub fn stats(&self) -> Stats {
        self.shared.lock().stats
    }

    /// Interleaved samples currently buffered.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.shared.lock().ring.available()
    }

    /// Queue one synthesis chunk, waiting for ring space as needed.
    ///
    /// `params` describes the chunk as the backend produced it; the pipeline
    /// resamples and channel-maps to the device. Awaiting on a full ring is the
    /// backpressure that keeps a fast backend from buffering an entire article
    /// ahead of playback.
    ///
    /// # Errors
    /// Returns an error if resampling fails.
    pub async fn push(&mut self, samples: &[f32], params: AudioParams) -> Result<()> {
        let mut prepared = mixer::prepare(samples, params, self.device)?;
        if prepared.is_empty() {
            return Ok(());
        }
        self.fade.apply(&mut prepared);
        self.write_all(&prepared).await;
        Ok(())
    }

    /// Signal end of utterance: let the ring drain and idle afterwards. The
    /// stream stays open, so the next utterance does not pay device wake-up
    /// latency again.
    ///
    /// Nothing is held back to flush — [`SeamFade`] already faded the last
    /// chunk's tail to zero, so the utterance ends on silence by construction.
    pub fn finish(&mut self) {
        let mut guard = self.shared.lock();
        if guard.state != State::Idle {
            // Promote out of prebuffering even if the utterance was shorter
            // than the threshold, or a one-word reply would never be heard.
            guard.state = State::Draining;
        }
    }

    /// Stop immediately: discard buffered audio and the held tail.
    ///
    /// This is the barge-in path, so it is measured in how fast it goes quiet —
    /// dropping the ring means the next callback renders silence, bounded by
    /// one device buffer rather than by whatever was queued.
    pub fn cancel(&mut self) {
        let mut guard = self.shared.lock();
        guard.ring.clear();
        guard.state = State::Idle;
    }

    /// Render one buffer exactly as the audio callback would.
    ///
    /// The null-sink entry point: tests drive this instead of a device, and it
    /// is the same code path the cpal callback runs.
    pub fn render(&self, out: &mut [f32]) {
        self.shared.lock().render(out);
    }

    /// Write every sample, yielding while the ring is full.
    async fn write_all(&self, mut samples: &[f32]) {
        loop {
            {
                let mut guard = self.shared.lock();
                let n = guard.ring.write(samples);
                samples = &samples[n..];
                if guard.state == State::Idle {
                    guard.state = State::Prebuffering;
                }
                guard.maybe_start();
                if samples.is_empty() {
                    return;
                }
            }
            // The ring is full, which means the consumer is the bottleneck —
            // exactly the situation backpressure is for. Sleep for a fraction
            // of the buffer rather than spinning.
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

#[cfg(test)]
mod tests;
