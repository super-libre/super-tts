// SPDX-License-Identifier: GPL-3.0-only
//! The error bodies every daemon's guards answer with are the engine's.
//! `rate_limited` is re-exported for the envelope contract tests, which hold
//! every error shape the daemon sends against the ones it documents.

#[cfg(test)]
pub(crate) use super_engine_daemon::http::responses::rate_limited;
pub(crate) use super_engine_daemon::http::responses::{invalid_session, reason, scope_denied};
