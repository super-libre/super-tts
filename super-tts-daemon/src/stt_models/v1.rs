// SPDX-License-Identifier: GPL-3.0-only
//! Shared `/v1/transcribe` request/response plumbing for the backend hosts.
//!
//! The wasm and subprocess hosts differ only in transport (an in-process
//! component invocation vs a Unix-socket HTTP dial). The request-body build and
//! the response parsing were byte-identical in both — and had started to drift —
//! so they live here. Feature-agnostic: compiled whenever either backend is on.

use anyhow::{Result, anyhow, bail};
use http_body_util::BodyExt;
use super_tts_shared::audio::frames::{AudioParams, Frame, FrameDecoder, FrameKind};

/// A `POST /v1/synthesize` request.
///
/// The daemon has already normalized the text, resolved `auto` to a concrete
/// language, and chunked to the model's `max_input_chars`, so a backend
/// receives one prosodic unit it can synthesize as written.
#[derive(Debug, Default, Clone)]
pub struct SynthesizeRequest<'a> {
    /// The text to speak. Non-empty.
    pub text: &'a str,
    /// A `voice` id, or `None` to use the model's `default_voice`.
    pub voice: Option<&'a str>,
    /// Resolved BCP-47 tag. Never `auto` — the daemon owns detection, since it
    /// owns the text.
    pub language: Option<&'a str>,
    /// Rate multiplier. Backends that cannot vary rate ignore it; the daemon
    /// does not resample to fake it.
    pub speed: Option<f32>,
    /// Free-text delivery guidance for models that accept it.
    pub instructions: Option<&'a str>,
}

/// Serialize a `/v1/synthesize` request body. Optional fields are omitted
/// rather than sent as `null`, so a backend can distinguish "unset" without
/// special-casing.
///
/// # Errors
/// Returns an error if JSON serialization fails (not expected for this shape).
pub fn build_synthesize_body(req: &SynthesizeRequest<'_>) -> Result<Vec<u8>> {
    // Built as a map rather than a `json!` literal that is then downcast, so
    // there is no `as_object_mut().expect(..)` to justify.
    let mut map = serde_json::Map::new();
    map.insert("text".into(), req.text.into());
    if let Some(v) = req.voice {
        map.insert("voice".into(), v.into());
    }
    if let Some(l) = req.language {
        map.insert("language".into(), l.into());
    }
    if let Some(s) = req.speed {
        map.insert("speed".into(), serde_json::json!(s));
    }
    if let Some(i) = req.instructions {
        map.insert("instructions".into(), i.into());
    }
    Ok(serde_json::to_vec(&serde_json::Value::Object(map))?)
}

/// Receives a synthesis response as it decodes.
///
/// A sink rather than a returned stream because the wasm transport reads the
/// body while its `Store` is still borrowed: handing frames out through a
/// callback keeps that borrow local to the call, and both transports then
/// share one pump.
pub trait SynthesisSink {
    /// The response's audio invariants, delivered once before any frame.
    ///
    /// # Errors
    /// Returning an error aborts the pump.
    fn on_params(&mut self, params: AudioParams) -> Result<()>;

    /// One decoded frame, in arrival order.
    ///
    /// # Errors
    /// Returning an error aborts the pump — this is how a cancelled utterance
    /// stops a backend that is still streaming.
    fn on_frame(&mut self, frame: Frame) -> Result<()>;
}

/// Collect a whole synthesis response in memory. Test and fixture helper; the
/// playback path uses a streaming sink instead.
#[derive(Debug, Default)]
pub struct CollectingSink {
    /// Params from the response headers, once seen.
    pub params: Option<AudioParams>,
    /// Every frame, in arrival order.
    pub frames: Vec<Frame>,
}

impl SynthesisSink for CollectingSink {
    fn on_params(&mut self, params: AudioParams) -> Result<()> {
        self.params = Some(params);
        Ok(())
    }

    fn on_frame(&mut self, frame: Frame) -> Result<()> {
        self.frames.push(frame);
        Ok(())
    }
}

impl CollectingSink {
    /// Concatenate every audio payload and decode it as one buffer.
    ///
    /// Decoding the concatenation rather than each frame matters: a backend may
    /// split a frame mid-sample, and per-frame decoding would drop the straddling
    /// bytes.
    #[must_use]
    pub fn samples(&self) -> Vec<f32> {
        let Some(params) = self.params else {
            return Vec::new();
        };
        let mut pcm = Vec::new();
        for f in self.frames.iter().filter(|f| f.kind == FrameKind::Audio) {
            pcm.extend_from_slice(&f.payload);
        }
        params.format.decode(&pcm)
    }
}

/// Read a framed `/v1/synthesize` response body, feeding `sink` as frames
/// complete.
///
/// Generic over the body so the wasm and subprocess transports share it —
/// wasmtime's and hyper's bodies differ in type but both implement
/// `http_body::Body<Data = Bytes>`.
///
/// # Errors
/// Returns an error when the headers do not describe usable audio, the frame
/// stream is malformed or exceeds its caps, the body errors mid-read, or the
/// backend sent an `error` frame.
pub async fn pump_synthesis<B>(
    headers: &hyper::http::HeaderMap,
    body: B,
    sink: &mut impl SynthesisSink,
) -> Result<()>
where
    B: http_body::Body<Data = bytes::Bytes>,
    B::Error: std::fmt::Display,
{
    let params = AudioParams::from_headers(|name| headers.get(name).and_then(|v| v.to_str().ok()))?;
    sink.on_params(params)?;

    let mut decoder = FrameDecoder::new();
    let mut body = std::pin::pin!(body);
    while let Some(next) = body.frame().await {
        let chunk = next.map_err(|e| anyhow!("reading synthesis body: {e}"))?;
        let Some(data) = chunk.data_ref() else {
            continue; // trailers carry nothing for this contract
        };
        decoder.push(data);
        while let Some(frame) = decoder.next_frame()? {
            if frame.kind == FrameKind::Error {
                bail!("{}", error_frame_message(&frame.payload));
            }
            sink.on_frame(frame)?;
        }
        // Stop reading as soon as the backend has said it is done, rather than
        // waiting for the body to close: a backend that never closes would
        // otherwise hold the utterance open forever.
        if decoder.saw_terminal() {
            break;
        }
    }
    decoder.finish()?;
    Ok(())
}

/// Pull the human-readable message out of an `error` frame payload, falling
/// back to a fixed string when it is not the documented shape.
fn error_frame_message(payload: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(payload)
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "synthesis failed".to_string())
}

/// Turn a non-`200` synthesize response into an error, preferring the
/// backend's own `detail`/`message` (it reaches the user) over the raw body.
#[must_use]
pub fn synthesize_error(status: u16, body: &[u8]) -> anyhow::Error {
    let msg = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("detail")
                .or_else(|| v.get("message"))
                .and_then(|m| m.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "synthesis failed".to_string());
    anyhow!("backend returned {status}: {msg}")
}

/// Serialize a `/v1/transcribe` request body: `audio` samples embedded as-is at
/// `sample_rate` (the caller resamples first if its transport needs a fixed
/// rate), plus an optional `language` override.
///
/// # Errors
/// Returns an error if JSON serialization fails (not expected for this shape).
pub(crate) fn build_transcribe_body(
    audio: &[f32],
    sample_rate: u32,
    language: Option<&str>,
) -> Result<Vec<u8>> {
    let mut body = serde_json::json!({
        "audio_data": audio,
        "sample_rate": sample_rate,
    });
    if let Some(lang) = language {
        body["language"] = serde_json::Value::String(lang.to_string());
    }
    Ok(serde_json::to_vec(&body)?)
}

/// Parse a `/v1/transcribe` response: the transcript on `200`, else the
/// backend's own `detail`/`message` surfaced as the error (it's shown to the
/// user, so prefer it over the raw HTTP body).
///
/// # Errors
/// Returns an error if the body isn't JSON, a `200` is missing `transcription`,
/// or a non-`200` carries a backend error message.
pub(crate) fn parse_transcribe_response(status: u16, resp: &[u8]) -> Result<String> {
    let json: serde_json::Value = serde_json::from_slice(resp)
        .map_err(|e| anyhow!("parsing backend transcribe response: {e}"))?;
    if status == 200 {
        json["transcription"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| anyhow!("backend response missing transcription"))
    } else {
        let msg = json
            .get("detail")
            .and_then(|v| v.as_str())
            .or_else(|| json.get("message").and_then(|v| v.as_str()))
            .unwrap_or("transcription failed");
        bail!("{msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::{build_transcribe_body, parse_transcribe_response};

    #[test]
    fn body_embeds_audio_rate_and_optional_language() {
        let with_lang = build_transcribe_body(&[0.1, -0.2], 16000, Some("en")).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&with_lang).unwrap();
        assert_eq!(v["sample_rate"], 16000);
        assert_eq!(v["audio_data"].as_array().unwrap().len(), 2);
        assert_eq!(v["language"], "en");

        let no_lang = build_transcribe_body(&[], 8000, None).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&no_lang).unwrap();
        assert!(v.get("language").is_none());
    }

    #[test]
    fn parse_returns_transcription_on_200() {
        let text = parse_transcribe_response(200, br#"{"transcription":"hello"}"#).unwrap();
        assert_eq!(text, "hello");
    }

    #[test]
    fn parse_surfaces_detail_then_message_on_error() {
        let e = parse_transcribe_response(500, br#"{"detail":"oom","message":"m"}"#).unwrap_err();
        assert_eq!(e.to_string(), "oom");
        let e = parse_transcribe_response(500, br#"{"message":"just message"}"#).unwrap_err();
        assert_eq!(e.to_string(), "just message");
        let e = parse_transcribe_response(500, br"{}").unwrap_err();
        assert_eq!(e.to_string(), "transcription failed");
    }

    #[test]
    fn parse_errors_on_200_without_transcription() {
        assert!(parse_transcribe_response(200, br#"{"other":1}"#).is_err());
    }
}
