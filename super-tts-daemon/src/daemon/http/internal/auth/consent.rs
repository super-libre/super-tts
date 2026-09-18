// SPDX-License-Identifier: GPL-3.0-only
use crate::daemon::http::state::PeerInfo;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Global cap of one on-screen consent popup at a time. See
/// [`ask_user_for_consent`] — without it a same-uid client could drive hundreds
/// of concurrent exclusive-keyboard dialogs (255 distinct consent keys) and lock
/// the desktop (audit 2 Tier 3 #10).
static CONSENT_POPUP: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// Who the daemon believes is calling.
///
/// The two variants are the two transports, and they are not the same kind of
/// claim. [`Self::Native`] is what the kernel says about a peer on the Unix
/// socket; [`Self::Web`] is what a browser says about the page it is running.
/// Keeping them as separate variants rather than one struct with optional
/// fields is what stops a check written for one from silently passing for the
/// other — `is_official_client` reading an absent exe path as "not official"
/// would be correct by accident, and one refactor away from not being.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum PeerIdentity {
    /// A process on the Unix socket, identified by `SO_PEERCRED`: its
    /// `/proc/<pid>/exe`, as the kernel resolves it.
    Native {
        /// `/proc/<pid>/exe`.
        exe_path: PathBuf,
    },
    /// A page on the TCP listener, identified by its `Origin`.
    ///
    /// **This is a weaker claim than [`Self::Native`], and deliberately so.**
    /// The kernel vouches for an exe path; nothing vouches for an origin but
    /// the browser that sent it. A non-browser process can put any string here.
    /// What keeps that from mattering is that the daemon only accepts origins
    /// the user wrote into [`TcpConfig::allowed_origins`](crate::config::TcpConfig::allowed_origins)
    /// — so forging one gets you no further than forging an origin the user
    /// already trusted, on a listener they already turned on.
    ///
    /// The consent dialog says which kind it is asking about, because "allow
    /// this website" and "allow this program" deserve different answers.
    Web {
        /// The full origin as the browser sent it: scheme, host and port.
        origin: String,
    },
}

impl PeerIdentity {
    /// A process on the Unix socket.
    #[cfg(test)]
    pub(crate) fn native(exe_path: impl Into<PathBuf>) -> Self {
        Self::Native {
            exe_path: exe_path.into(),
        }
    }

    /// A browser page served from `origin`.
    #[cfg(test)]
    pub(crate) fn web(origin: impl Into<String>) -> Self {
        Self::Web {
            origin: origin.into(),
        }
    }

    /// One line naming the caller, for logs and the consent dialog. A web peer
    /// is named as a web peer, so a log line can never be read as naming a
    /// binary.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Native { exe_path } => exe_path.display().to_string(),
            Self::Web { origin } => format!("web origin {origin}"),
        }
    }
}

/// Identifies the consent flow uniquely: (`identity`, normalized `scopes`).
/// The user verifies a *caller*, not a self-reported display name, so the
/// deny / dedup key is keyed on the resolved [`PeerIdentity`] plus the
/// requested scope set (sorted + deduped via [`normalize_scopes`] so request
/// order doesn't matter). `app_name` is shown in the popup but isn't part of
/// the identity.
pub(crate) type ConsentKey = (PeerIdentity, Vec<String>);
pub(crate) type ConsentLock = Arc<tokio::sync::Mutex<()>>;

/// Sort + dedup a requested scope list so the consent key and the
/// granted set are independent of the order the client listed them.
pub(crate) fn normalize_scopes(scopes: &[String]) -> Vec<String> {
    let mut v = scopes.to_vec();
    v.sort();
    v.dedup();
    v
}

/// Per-`(exe_path, scope)` async mutex registry used by the
/// `/auth/request` handler to dedup concurrent first-time consent
/// requests. Without this, two clients that ping the daemon at the same
/// time on a fresh install would each spawn their own consent popup;
/// with it, the second blocks until the first finishes and then
/// short-circuits via the reuse-scan against the now-minted token.
///
/// The map is pruned via [`Self::release`] after the auth flow
/// completes so a malicious client can't drive unbounded memory
/// growth by spamming /auth/request with rotating keys.
#[derive(Clone, Default)]
pub(crate) struct ConsentLocks {
    inner: Arc<Mutex<HashMap<ConsentKey, ConsentLock>>>,
}

impl ConsentLocks {
    pub(crate) fn lock_for(&self, key: ConsentKey) -> ConsentLock {
        let mut map = self.inner.lock().unwrap();
        map.entry(key)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Drop the registry entry for `key` if no other task is still
    /// holding the `ConsentLock`. Called from the `auth_request`
    /// handler after the consent flow finishes — success or denial.
    /// `strong_count == 2` means exactly the map and our local clone
    /// hold references; anything higher means another in-flight
    /// `auth_request` for the same key is still waiting on the same
    /// mutex and we leave the entry in place for it.
    pub(crate) fn release(&self, key: &ConsentKey, lock: &ConsentLock) {
        let mut map = self.inner.lock().unwrap();
        // Our `lock` reference plus the one inside the map. If
        // anything else is still holding, leave it.
        if Arc::strong_count(lock) <= 2 {
            map.remove(key);
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ConsentDecision {
    Allow,
    Deny,
    Dismissed,
    PopupFailed,
}

/// Spawn the `super-tts-consent` helper binary, wait up to 60s for the
/// user's decision. The helper writes one of `allow` / `deny` / `dismissed`
/// to stdout and exits.
/// Read the consent helper's single-line verdict from its stdout.
async fn read_consent_decision(stdout: tokio::process::ChildStdout) -> ConsentDecision {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut reader = BufReader::new(stdout).lines();
    match reader.next_line().await {
        Ok(Some(line)) => match line.trim() {
            "allow" => ConsentDecision::Allow,
            "deny" => ConsentDecision::Deny,
            _ => ConsentDecision::Dismissed,
        },
        _ => ConsentDecision::Dismissed,
    }
}

pub(crate) async fn ask_user_for_consent(
    app_name: &str,
    scopes: &[String],
    identity: &PeerIdentity,
) -> ConsentDecision {
    // `locate_consent_helper` already logs a specific reason on every
    // failure path (missing / un-canonicalizable / failed metadata check),
    // so we don't emit a second, redundant warning here.
    let Some(helper) = locate_consent_helper() else {
        return ConsentDecision::PopupFailed;
    };

    // Serialize popups globally: at most one consent dialog on screen at a time
    // (audit 2 Tier 3 #10). `/auth/request` is unauthenticated and outside the
    // rate limiter, and the 8 scopes yield 255 distinct `(exe, scopes)` consent
    // keys — each bypassing the per-key dedup — so without this cap a same-uid
    // process could stack hundreds of concurrent exclusive-keyboard overlays and
    // lock the desktop. Excess requests wait for the permit rather than opening
    // in parallel. Acquired before the spawn and held while the dialog is on
    // screen; released before the untimed reap below so a wedged helper can't
    // wedge all consent.
    let Ok(popup_permit) = CONSENT_POPUP.acquire().await else {
        return ConsentDecision::PopupFailed; // semaphore closed (never in practice)
    };

    let mut cmd = tokio::process::Command::new(&helper);
    cmd.env("SUPER_TTS_AUTH_APP_NAME", app_name)
        .env("SUPER_TTS_AUTH_SCOPES", scopes.join(" "));
    match identity {
        PeerIdentity::Native { exe_path } => {
            cmd.env(
                "SUPER_TTS_AUTH_EXE_PATH",
                exe_path.to_string_lossy().as_ref(),
            );
        }
        // A web peer sets the origin variable *instead of* the exe path, never
        // alongside it. The helper decides which dialog to show by which one it
        // was given, so sending both would leave the user reading a sentence
        // about a binary when a website is what is asking.
        PeerIdentity::Web { origin } => {
            cmd.env("SUPER_TTS_AUTH_WEB_ORIGIN", origin);
        }
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("failed to spawn super-tts-consent: {e}");
            return ConsentDecision::PopupFailed;
        }
    };

    let Some(stdout) = child.stdout.take() else {
        return ConsentDecision::PopupFailed;
    };

    let result = tokio::time::timeout(Duration::from_mins(1), read_consent_decision(stdout)).await;
    let _ = child.start_kill();
    // The dialog is being torn down, so release the global one-popup permit
    // *before* the reap. `child.wait()` is untimed; holding the sole global
    // permit across it would let a helper that somehow doesn't reap promptly
    // (a pathological uninterruptible-sleep) wedge all consent daemon-wide.
    // Releasing first keeps the popup cap intact while the reap still completes.
    drop(popup_permit);
    let _ = child.wait().await;

    result.unwrap_or(ConsentDecision::Dismissed)
}

/// Basenames of the first-party client binaries that skip the consent
/// popup when co-located with the daemon binary. See
/// [`is_official_client`] for the full trust check.
const OFFICIAL_CLIENT_NAMES: [&str; 3] =
    ["super-tts-app", "super-tts-cli", "super-tts-cosmic-applet"];

/// First-party trust check: does `exe_path` denote one of our own
/// client binaries, installed alongside the daemon binary itself?
///
/// Mirrors the [`locate_consent_helper`] security model — co-location
/// with the daemon binary plus the same ownership/permission
/// verification. Writing to the daemon's install directory is already
/// sufficient to replace the daemon, so trusting exact-named sibling
/// binaries adds no new attack surface. Returns a plain bool: failure
/// is the common case (every third-party client) and is deliberately
/// not logged here — the caller logs the rare success.
pub(crate) fn is_official_client(identity: &PeerIdentity) -> bool {
    let PeerIdentity::Native { exe_path } = identity else {
        // A web peer is never first-party. The whole check below is about a
        // binary on this filesystem, and a page has none — there is nothing to
        // canonicalize and no ownership to verify, so the only safe answer is
        // the consent popup. A site calling itself `super-tts-app` must not get
        // within reach of the short-circuit.
        return false;
    };
    let Ok(daemon_exe) = std::env::current_exe() else {
        return false;
    };
    let Some(daemon_dir) = daemon_exe.parent() else {
        return false;
    };
    is_official_client_in(daemon_dir, exe_path)
}

/// Testable core of [`is_official_client`] with the daemon's own
/// directory injected. Fail-closed on every non-verifiable branch: a
/// replaced-on-disk exe (`/proc/<pid>/exe` → "… (deleted)") or a
/// symlink resolving outside `daemon_dir` fails canonicalization or
/// the parent check and falls through to the normal consent flow.
fn is_official_client_in(daemon_dir: &Path, exe_path: &Path) -> bool {
    let (Ok(resolved), Ok(daemon_dir)) = (exe_path.canonicalize(), daemon_dir.canonicalize())
    else {
        return false;
    };
    let Some(name) = resolved.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if !OFFICIAL_CLIENT_NAMES.contains(&name) {
        return false;
    }
    if resolved.parent() != Some(daemon_dir.as_path()) {
        return false;
    }
    verify_helper_metadata(&resolved).is_ok()
}

/// Find the consent helper.
///
/// **Security model.** The helper is only ever looked for **alongside the
/// daemon binary itself**. We deliberately do NOT fall back to `PATH`
/// because doing so would let any attacker who can prepend a writable
/// directory to the daemon's `PATH` (a classic privilege-escalation
/// vector) substitute their own helper. Forcing co-location bounds the
/// attack surface to "whoever can write to the directory holding the
/// daemon binary" — which is the same threshold required to replace the
/// daemon itself, so we don't make consent any easier to subvert than
/// the daemon's own integrity.
///
/// On top of that, before returning the path:
/// - We `canonicalize()` it, so symlink-swap shenanigans don't help.
/// - We verify the resolved file is owned by root or the daemon's
///   effective uid (catches "another local user dropped a helper they
///   own into the install dir").
/// - We verify it isn't world-writable.
pub(crate) fn locate_consent_helper() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let candidate = dir.join("super-tts-consent");
    if !candidate.exists() {
        log::warn!(
            "super-tts-consent not found alongside daemon binary at {}; \
             auth_request will be denied with popup_failed",
            candidate.display()
        );
        return None;
    }

    let resolved = match candidate.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            log::warn!(
                "failed to canonicalize consent helper path {}: {e}",
                candidate.display()
            );
            return None;
        }
    };

    if let Err(reason) = verify_helper_metadata(&resolved) {
        log::warn!(
            "consent helper at {} rejected: {reason}",
            resolved.display()
        );
        return None;
    }

    Some(resolved)
}

/// Verify the helper's file metadata is consistent with "trusted binary
/// installed by the user". Returns Err with a static reason on
/// rejection.
#[cfg(unix)]
fn verify_helper_metadata(path: &Path) -> Result<(), &'static str> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).map_err(|_| "cannot stat helper")?;
    let our_uid = unsafe { libc::geteuid() };
    check_helper_metadata(metadata.uid(), our_uid, metadata.mode())
}

/// Testable core of [`verify_helper_metadata`]. Trust binaries owned by
/// the daemon's own uid (source/dev installs) or by root (the packaged
/// /usr/local/bin // /usr/bin install) — whoever controls root already
/// controls the daemon binary itself, so root ownership adds no new
/// attack surface. Anything else is another local user's drop-in.
#[cfg(unix)]
fn check_helper_metadata(owner_uid: u32, our_uid: u32, mode: u32) -> Result<(), &'static str> {
    if owner_uid != our_uid && owner_uid != 0 {
        return Err("helper not owned by root or the daemon's effective uid");
    }
    // Reject world-writable helpers — anyone could swap them out.
    if mode & 0o002 != 0 {
        return Err("helper is world-writable");
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_helper_metadata(_: &Path) -> Result<(), &'static str> {
    Ok(())
}

/// Resolve who is calling, from the [`PeerInfo`] the accept loop attached.
///
/// A peer on the Unix socket is its `/proc/<pid>/exe`. `None` means the peer
/// can't be identified — a missing `PeerInfo`/pid (`SO_PEERCRED` unsupported,
/// peer process gone) or a kernel-denied `/proc/<pid>/exe` readlink (Yama
/// `ptrace_scope`, systemd `ProtectProc=`, a sandboxed daemon, pid recycling).
///
/// A peer that arrived over TCP has no kernel-attested identity at all, so it
/// is resolved from its `Origin` instead — see [`PeerInfo::web_origin`]. That
/// field is only ever set by the origin gate, which has already checked the
/// value against the user's allowlist; nothing here re-derives it from a
/// header, so there is exactly one place an origin can enter the system.
///
/// The caller **must fail closed** on `None`: the consent model verifies a
/// *caller*, so an unidentifiable peer must not be prompted for (a
/// `<unknown>`-labelled dialog is meaningless to approve) nor minted a token
/// bound to a bogus identity that the `/events` exe-watch would then spuriously
/// revoke (audit 2 Tier 3 #9). Each failure is logged with its specific reason.
pub(crate) fn resolve_peer_identity(
    peer: Option<&PeerInfo>,
    context: &str,
) -> Option<PeerIdentity> {
    let Some(peer) = peer else {
        log::warn!("{context}: no PeerInfo extension attached — cannot identify the caller");
        return None;
    };
    if let Some(origin) = &peer.web_origin {
        return Some(PeerIdentity::Web {
            origin: origin.clone(),
        });
    }
    let Some(pid) = peer.pid else {
        log::warn!(
            "{context}: PeerInfo had no pid (SO_PEERCRED returned no credentials); cannot resolve exe"
        );
        return None;
    };
    let path = format!("/proc/{pid}/exe");
    match std::fs::read_link(&path) {
        Ok(exe_path) => Some(PeerIdentity::Native { exe_path }),
        Err(e) => {
            log::warn!("{context}: read_link({path}) failed: {e}; cannot identify peer pid {pid}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `check_helper_metadata`: the ownership/permission gate shared by
    /// the consent-helper lookup and the official-client trust check.
    mod helper_metadata {
        use super::super::check_helper_metadata;

        #[test]
        fn owned_by_daemon_uid_is_trusted() {
            assert!(check_helper_metadata(1000, 1000, 0o755).is_ok());
        }

        #[test]
        fn root_owned_is_trusted() {
            // The packaged install (/usr/local/bin, /usr/bin) is
            // root-owned; root could already replace the daemon binary
            // itself, so this adds no new attack surface.
            assert!(check_helper_metadata(0, 1000, 0o755).is_ok());
        }

        #[test]
        fn other_local_user_is_rejected() {
            assert!(check_helper_metadata(1001, 1000, 0o755).is_err());
        }

        #[test]
        fn world_writable_is_rejected_even_when_root_owned() {
            assert!(check_helper_metadata(0, 1000, 0o757).is_err());
        }
    }

    #[test]
    fn normalize_sorts_and_dedups() {
        let got = normalize_scopes(&[
            "speak".to_string(),
            "status".to_string(),
            "speak".to_string(),
        ]);
        assert_eq!(got, vec!["speak".to_string(), "status".to_string()]);
    }

    #[test]
    fn normalize_is_order_independent() {
        let a = normalize_scopes(&["settings".to_string(), "status".to_string()]);
        let b = normalize_scopes(&["status".to_string(), "settings".to_string()]);
        assert_eq!(a, b, "request order must not change the consent key");
    }

    /// First-party trust check (`is_official_client_in`): exact-name
    /// allowlist + co-location with the daemon dir + metadata
    /// verification, fail-closed on every non-verifiable branch.
    mod official_client {
        use super::super::is_official_client_in;
        use std::os::unix::fs::PermissionsExt;
        use std::path::{Path, PathBuf};

        fn write_executable(dir: &Path, name: &str, mode: u32) -> PathBuf {
            let path = dir.join(name);
            std::fs::write(&path, b"\x7fELF").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            path
        }

        #[test]
        fn official_name_co_located_is_trusted() {
            let dir = tempfile::tempdir().unwrap();
            let app = write_executable(dir.path(), "super-tts-app", 0o755);
            assert!(is_official_client_in(dir.path(), &app));
        }

        #[test]
        fn unlisted_name_co_located_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let other = write_executable(dir.path(), "super-tts-extra", 0o755);
            assert!(
                !is_official_client_in(dir.path(), &other),
                "co-location alone must not confer trust"
            );
        }

        #[test]
        fn official_name_in_foreign_dir_is_rejected() {
            let daemon_dir = tempfile::tempdir().unwrap();
            let foreign = tempfile::tempdir().unwrap();
            let app = write_executable(foreign.path(), "super-tts-app", 0o755);
            assert!(
                !is_official_client_in(daemon_dir.path(), &app),
                "an official name outside the daemon dir must not be trusted"
            );
        }

        #[test]
        fn world_writable_official_binary_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let app = write_executable(dir.path(), "super-tts-cli", 0o757);
            assert!(
                !is_official_client_in(dir.path(), &app),
                "a world-writable binary could be swapped by anyone"
            );
        }

        #[test]
        fn missing_exe_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            assert!(
                !is_official_client_in(dir.path(), &dir.path().join("super-tts-app")),
                "a nonexistent (e.g. replaced-on-disk) exe must fail closed"
            );
        }

        #[test]
        fn symlink_resolving_outside_daemon_dir_is_rejected() {
            let daemon_dir = tempfile::tempdir().unwrap();
            let foreign = tempfile::tempdir().unwrap();
            let target = write_executable(foreign.path(), "super-tts-cli", 0o755);
            let link = daemon_dir.path().join("super-tts-cli");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert!(
                !is_official_client_in(daemon_dir.path(), &link),
                "canonicalization must unmask a symlink escaping the daemon dir"
            );
        }
    }
}
