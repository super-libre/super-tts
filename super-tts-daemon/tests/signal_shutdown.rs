// SPDX-License-Identifier: GPL-3.0-only
//! The daemon must shut down gracefully on the signals that actually reach it.
//!
//! `systemctl stop` and a plain `kill` send SIGTERM. A daemon that watched only
//! for SIGINT died to SIGTERM's default disposition, skipping the path that
//! stops the `systemd-run --user` backend unit — and since that unit is not a
//! child in the daemon's cgroup, nothing else reaps it. The result was a
//! subprocess backend left running with a whole model resident.
//!
//! These tests read the exit status, which is what separates the two cases: a
//! graceful stop reaches `std::process::exit(0)`, while a default-disposition
//! kill leaves the process signalled, with no exit code at all.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio::time::sleep;

const DAEMON_BIN: &str = env!("CARGO_BIN_EXE_super-tts-daemon");

/// Removes the temp `XDG_RUNTIME_DIR`, and kills the daemon if a test failed
/// before it could stop on its own.
struct DaemonGuard {
    child: Child,
    xdg_runtime_dir: PathBuf,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.xdg_runtime_dir);
    }
}

/// Monotonic per-call counter so tests in this binary get unique paths.
fn next_test_uniq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static UNIQ: AtomicU64 = AtomicU64::new(0);
    UNIQ.fetch_add(1, Ordering::Relaxed)
}

/// Spawn a daemon against isolated XDG dirs and wait for its socket.
///
/// `XDG_DATA_HOME` is isolated so no backend is discovered: these tests are
/// about the signal path, and a real backend would make them depend on a
/// systemd user session.
async fn start_daemon() -> (DaemonGuard, PathBuf) {
    let xdg = std::env::temp_dir().join(format!(
        "tts-signal-test-{}-{}",
        std::process::id(),
        next_test_uniq()
    ));
    std::fs::create_dir_all(xdg.join("tts")).expect("create xdg/tts dir");
    let config_home = xdg.join("config");
    std::fs::create_dir_all(&config_home).expect("create xdg/config dir");
    let data_home = xdg.join("data");
    std::fs::create_dir_all(&data_home).expect("create xdg/data dir");
    // Isolate the cache too: the registry client persists its index under
    // XDG_CACHE_HOME, so a shared one is the developer's own, and test daemons
    // running side by side overwrite each other's.
    let cache_home = xdg.join("cache");
    std::fs::create_dir_all(&cache_home).expect("create test cache dir");

    let http_socket = xdg.join("tts").join("super-tts-http.sock");

    let child = Command::new(DAEMON_BIN)
        .env("SUPER_TTS_KEYRING_MOCK", "1")
        .env("XDG_RUNTIME_DIR", &xdg)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_DATA_HOME", &data_home)
        .env("XDG_CACHE_HOME", &cache_home)
        .env("SUPER_TTS_AUTO_APPROVE", "1")
        .env("SUPER_TTS_MUTE_CUES", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn super-tts-daemon");

    let guard = DaemonGuard {
        child,
        xdg_runtime_dir: xdg,
    };

    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        if Path::new(&http_socket).exists() {
            // The socket file appearing is not quite the same as the signal
            // handler being installed; both happen during startup, and the
            // handler is registered first.
            sleep(Duration::from_millis(300)).await;
            return (guard, http_socket);
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "daemon did not bind its socket within 120s ({})",
        http_socket.display()
    );
}

/// Send `signal` to the daemon and wait for it to exit.
///
/// Returns the exit status, or `None` if it was still running at the deadline.
async fn signal_and_wait(
    guard: &mut DaemonGuard,
    signal: i32,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let pid = i32::try_from(guard.child.id()).expect("pid fits i32");
    // Safety: `pid` is this test's own direct child, still unreaped, so the id
    // cannot have been recycled onto an unrelated process.
    let sent = unsafe { libc::kill(pid, signal) };
    assert_eq!(sent, 0, "kill({pid}, {signal}) failed");

    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match guard.child.try_wait().expect("try_wait") {
            Some(status) => return Some(status),
            None => sleep(Duration::from_millis(100)).await,
        }
    }
    None
}

/// The regression this file exists for. Before the fix the daemon exited
/// *signalled* rather than with a code, because SIGTERM fell through to the
/// default disposition and the graceful path never ran.
#[tokio::test]
async fn sigterm_shuts_down_gracefully() {
    use std::os::unix::process::ExitStatusExt;

    let (mut guard, _socket) = start_daemon().await;
    let status = signal_and_wait(&mut guard, libc::SIGTERM, Duration::from_secs(30))
        .await
        .expect("daemon must exit within 30s of SIGTERM");

    assert_eq!(
        status.signal(),
        None,
        "SIGTERM killed the daemon outright instead of being handled; \
         the graceful path (which stops the backend unit) never ran"
    );
    assert_eq!(
        status.code(),
        Some(0),
        "graceful shutdown must exit 0, got {status:?}"
    );
}

/// SIGINT already worked; it must keep working now that it shares the wait.
#[tokio::test]
async fn sigint_still_shuts_down_gracefully() {
    use std::os::unix::process::ExitStatusExt;

    let (mut guard, _socket) = start_daemon().await;
    let status = signal_and_wait(&mut guard, libc::SIGINT, Duration::from_secs(30))
        .await
        .expect("daemon must exit within 30s of SIGINT");

    assert_eq!(status.signal(), None, "SIGINT was not handled: {status:?}");
    assert_eq!(
        status.code(),
        Some(0),
        "expected a clean exit, got {status:?}"
    );
}

/// A second, independent witness that the graceful path ran.
///
/// The listener task unlinks its socket on the way out. The exit code only says
/// the *process* got to exit cleanly; this says the listener's own cleanup ran
/// too. A restart would survive either way — `start_http_server` unlinks a
/// leftover socket before binding — so this is about proving the shutdown path
/// executed, not about preventing a bind failure.
#[tokio::test]
async fn graceful_shutdown_removes_the_listener_socket() {
    let (mut guard, socket) = start_daemon().await;
    assert!(socket.exists(), "socket should exist while running");

    signal_and_wait(&mut guard, libc::SIGTERM, Duration::from_secs(30))
        .await
        .expect("daemon must exit within 30s of SIGTERM");

    assert!(
        !socket.exists(),
        "graceful shutdown left {} behind, so the listener's cleanup never ran",
        socket.display()
    );
}
