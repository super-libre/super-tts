// SPDX-License-Identifier: GPL-3.0-only
//! STT model orchestration.
//!
//! The daemon no longer compiles model inference in-tree; every model is
//! served by an out-of-tree backend discovered on disk (see [`backends`]).
//! [`transcribe`] defines the common trait the hosts present; [`wasm`] and
//! [`subprocess`] are the two backend transports. [`download`] provisions a
//! backend's model files before it is spawned.
//!
//! The previous in-tree Whisper/Voxtral/online implementations now live in
//! their own standalone backend repositories.
pub mod backends;
pub mod dispatch;
pub mod download;
pub mod model_definition;
pub use model_definition::ModelDefinition;
#[cfg(feature = "subprocess-backends")]
pub mod subprocess;
pub mod transcribe;
// The `/v1` request/response *types* are backend-agnostic and are referenced
// from `transcribe`'s trait signature, so the module is always compiled; the
// body pump inside it is what carries the transport-dependent gate.
pub mod v1;
#[cfg(feature = "wasm-backends")]
pub mod wasm;
