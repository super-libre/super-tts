// SPDX-License-Identifier: GPL-3.0-only
//! `/auth` — how a client gets a token and checks the one it has.
//!
//! Contract: `docs/protocol/auth.md`.
//!
//! The two halves are deliberately separate endpoints. [`request`] runs the
//! consent flow and may put a popup on the user's screen; [`status`] never
//! does. A client that conflates them ends up prompting the user every time it
//! wants to know whether its stored token has expired.
//!
//! [`request`] is also the only operation in the whole document that advertises
//! no security at all — it is what a caller has before it has anything.

pub(crate) mod request;
pub(crate) mod status;
