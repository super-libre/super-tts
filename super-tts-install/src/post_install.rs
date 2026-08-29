// SPDX-License-Identifier: GPL-3.0-only
//! Post-install steps run back in the unprivileged user process after the
//! root phase has placed files: systemd service (re)start, applet panel
//! restart, launcher-cache nudge, legacy `~/.local` cleanup, and the COSMIC
//! keyboard-shortcut migrate/add. Every step is best-effort (`log::warn!` on
//! failure) except the daemon restart, the one failure the app must hear
//! about.
//!
//! `run`'s WHAT-to-do is a pure decision, separated from the HOW: [`plan`]
//! takes the coarse facts already known (or cheap to check) before any file
//! I/O or process spawn and returns an ordered [`Step`] list with no I/O of
//! its own; `run` computes those inputs, calls `plan`, and executes the
//! result through a thin per-step match. That seam is the whole decision
//! tree's test coverage — see `plan`'s unit tests below — without ever
//! needing a command-runner trait or a mocked OS.

use crate::errors::InstallError;
use crate::stage::Components;

/// Run `bin` with `args`, discarding output, returning whether it exited 0.
/// A spawn failure (binary missing, etc.) also counts as "not ok".
async fn cmd_ok(bin: &str, args: &[&str]) -> bool {
    tokio::process::Command::new(bin)
        .args(args)
        .status()
        .await
        .is_ok_and(|s| s.success())
}

/// Whether `bin` resolves on the current `$PATH`.
fn on_path(bin: &str) -> bool {
    let path_env = std::env::var("PATH").unwrap_or_default();
    crate::escalate::which(bin, &path_env).is_some()
}

/// One post-install action, in the order [`plan`] returns them. Each variant
/// is a single, independent shell-out or filesystem tweak — see `run`'s
/// executor `match` for what each one actually does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// `systemctl --user daemon-reload`.
    DaemonReload,
    /// Remove a legacy `~/.config/systemd/user/super-tts.service` unit that
    /// would otherwise shadow the packaged one and keep launching the (now
    /// deleted) legacy `~/.local/bin` binary.
    RemoveLegacyUnit,
    /// `systemctl --user enable super-tts`.
    Enable,
    /// `systemctl --user restart|start super-tts` (verb picked at execution
    /// time, via `is-active`) — the one step whose failure is a hard error.
    RestartOrStart,
    /// Restart `cosmic-panel` so it picks up the just-updated applet binary.
    RestartPanel,
    /// Nudge COSMIC's launcher caches (app grid + search backend) to rescan
    /// desktop entries.
    NudgeLaunchers,
    /// Remove pre-`/usr/local` per-user leftovers (bins/desktop files/icons).
    CleanupLegacy,
    //
    // The STT build also offered to bind `Super+Space` to
    // `tts record --write`, and migrated an older shortcut's path. Both are
    // gone with the `record` subcommand: this daemon's equivalent gesture is
    // "speak what I have selected", and until text acquisition exists there is
    // no command worth binding. Installing a broken shortcut, or migrating one,
    // is worse than installing none.
}

/// Decide which [`Step`]s to run, and in what order, from facts already known
/// before any post-install I/O: `components` (what was just installed or
/// updated), `applet_was_installed` (captured *before* the root phase ran —
/// the script's `is_update` check), and two environment probes the caller
/// (`run`) is expected to have already made: `systemctl_available` (a `$PATH`
/// lookup) and `panel_running` (a `pgrep` check). `panel_running` only matters
/// when `components.applet && applet_was_installed` — a caller may cheaply
/// pass `true` unconditionally for any other combination and it's simply
/// ignored.
///
/// Order: systemd install/enable/restart, then the applet's panel restart,
/// then the launcher-cache nudge, then legacy cleanup. Every file is already
/// placed by the time this runs — that all happens earlier, in the root phase
/// — so [`Step::CleanupLegacy`] is a single unconditional sweep rather than
/// one pass per component; removing files that were never there is a no-op, so
/// the sweep is idempotent whatever the selection.
#[must_use]
#[allow(clippy::fn_params_excessive_bools)] // interface fixed by the design doc: the booleans are the planner's whole point
pub fn plan(
    components: Components,
    applet_was_installed: bool,
    systemctl_available: bool,
    panel_running: bool,
) -> Vec<Step> {
    let mut steps = Vec::new();

    if components.daemon && systemctl_available {
        steps.push(Step::DaemonReload);
        steps.push(Step::RemoveLegacyUnit);
        steps.push(Step::Enable);
        steps.push(Step::RestartOrStart);
    }

    if components.applet && applet_was_installed && panel_running {
        steps.push(Step::RestartPanel);
    }

    // The launcher nudge is skipped only for a daemon-only install: an app
    // or applet install both add launcher-visible entries that benefit from
    // the rescan.
    let daemon_only = components.daemon && !components.app && !components.applet;
    if !daemon_only {
        steps.push(Step::NudgeLaunchers);
    }

    steps.push(Step::CleanupLegacy);

    steps
}

async fn step_daemon_reload() {
    if !cmd_ok("systemctl", &["--user", "daemon-reload"]).await {
        log::warn!("systemctl --user daemon-reload failed");
    }
}

/// A unit left in `~/.config/systemd/user` by an older install takes
/// precedence over the packaged one — remove it or systemd keeps launching
/// the (now deleted) legacy `~/.local/bin` binary.
fn step_remove_legacy_unit() {
    if let Some(home) = dirs::home_dir() {
        let legacy_unit = home.join(".config/systemd/user/super-tts.service");
        if legacy_unit.exists() {
            let _ = std::fs::remove_file(&legacy_unit);
        }
    }
}

async fn step_enable() {
    if !cmd_ok("systemctl", &["--user", "enable", "super-tts"]).await {
        log::warn!("systemctl --user enable super-tts failed");
    }
}

/// # Errors
/// [`InstallError::PostInstallFailed`] when the final `restart`/`start`
/// exits nonzero — the only step in the whole post-install sequence whose
/// failure is a hard error.
async fn step_restart_or_start() -> Result<(), InstallError> {
    let is_active = cmd_ok(
        "systemctl",
        &["--user", "is-active", "--quiet", "super-tts"],
    )
    .await;
    let verb = if is_active { "restart" } else { "start" };
    if cmd_ok("systemctl", &["--user", verb, "super-tts"]).await {
        Ok(())
    } else {
        Err(InstallError::PostInstallFailed(format!(
            "daemon {verb} failed: run `systemctl --user {verb} super-tts` manually"
        )))
    }
}

async fn step_restart_panel() {
    if !cmd_ok("pkill", &["-f", "cosmic-panel"]).await {
        log::warn!("failed to restart cosmic-panel to load the updated applet");
    }
}

/// Nudge COSMIC's launcher caches (app grid + search backend): both scan
/// desktop entries at session start and miss entries added to a directory
/// they weren't watching. They respawn on demand and rescan.
async fn step_nudge_launchers() {
    let _ = tokio::process::Command::new("pkill")
        .args(["-f", "^cosmic-app-library$"])
        .status()
        .await;
    let _ = tokio::process::Command::new("pkill")
        .args(["-f", "^cosmic-launcher$"])
        .status()
        .await;
    let _ = tokio::process::Command::new("pkill")
        .args(["-f", "^pop-launcher( |$)"])
        .status()
        .await;
}

/// Bins, desktop files, and icons the pre-`/usr/local` per-user install left
/// behind — cleared so they don't shadow (or duplicate in launchers) the
/// fresh install.
fn cleanup_legacy() {
    let Some(home) = dirs::home_dir() else {
        return;
    };

    let bin_dir = home.join(".local/bin");
    for name in [
        "super-tts",
        "super-tts-daemon",
        "super-tts-cli",
        "super-tts-consent",
        "tts",
        "super-tts-app",
        "super-tts-cosmic-applet",
        "super-tts-applet-full",
        "super-tts-applet-left",
        "super-tts-applet-right",
    ] {
        let _ = std::fs::remove_file(bin_dir.join(name));
    }

    let apps_dir = home.join(".local/share/applications");
    for name in [
        "super-tts-app.desktop",
        "super-tts-cosmic-applet-full.desktop",
        "super-tts-cosmic-applet-left.desktop",
        "super-tts-cosmic-applet-right.desktop",
    ] {
        let _ = std::fs::remove_file(apps_dir.join(name));
    }

    let icons_dir = home.join(".local/share/icons");
    let _ = std::fs::remove_file(icons_dir.join("super-tts-app.svg"));
    let _ = std::fs::remove_file(icons_dir.join("hicolor/scalable/apps/super-tts-app.svg"));
    let _ =
        std::fs::remove_file(icons_dir.join("hicolor/scalable/apps/super-tts-cosmic-applet.svg"));

    let _ = std::fs::remove_file(home.join(".local/share/metainfo/super-tts-app.metainfo.xml"));
}

/// Everything that happens back in the user process after the root phase has
/// placed files. Computes the environment probes [`plan`] needs, gets back
/// an ordered [`Step`] list, and executes it through a thin per-step
/// `match` — that seam is what makes the whole decision tree ([`plan`]'s
/// unit tests) testable without ever touching the filesystem or a real
/// shell-out.
///
/// `applet_was_installed` must be captured *before* the root phase ran (the
/// script's `is_update` check) — it decides whether the panel needs
/// restarting to pick up a *changed* applet binary, not whether the applet
/// is present now.
///
/// # Errors
/// [`InstallError::PostInstallFailed`] only when the daemon restart/start
/// itself fails ([`Step::RestartOrStart`]) — every other step is best-effort
/// and only logs a warning.
pub async fn run(components: &Components, applet_was_installed: bool) -> Result<(), InstallError> {
    let systemctl_available = on_path("systemctl");
    if components.daemon && !systemctl_available {
        log::warn!("systemctl not found on PATH; skipping daemon service setup");
    }
    // Only worth the `pgrep` shell-out when it could actually matter —
    // `plan` re-checks the same `components.applet && applet_was_installed`
    // guard itself, so passing `false` when it doesn't apply is equivalent.
    let panel_running =
        components.applet && applet_was_installed && cmd_ok("pgrep", &["-f", "cosmic-panel"]).await;

    for step in plan(
        *components,
        applet_was_installed,
        systemctl_available,
        panel_running,
    ) {
        match step {
            Step::DaemonReload => step_daemon_reload().await,
            Step::RemoveLegacyUnit => step_remove_legacy_unit(),
            Step::Enable => step_enable().await,
            // C8 INVARIANT: this is the ONLY `?` in this match. Every other
            // step function returns `()`, not `Result` — best-effort by
            // construction, not merely by convention here — so adding a `?`
            // to another arm first requires changing that step's own
            // function signature. If you're doing that deliberately,
            // update `only_restart_or_start_step_is_a_hard_error` below (it
            // pins this match having exactly one `?`, on this arm) and this
            // module's doc comment, which documents `RestartOrStart` as the
            // sole hard error.
            Step::RestartOrStart => step_restart_or_start().await?,
            Step::RestartPanel => step_restart_panel().await,
            Step::NudgeLaunchers => step_nudge_launchers().await,
            Step::CleanupLegacy => cleanup_legacy(),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_three() -> Components {
        Components {
            daemon: true,
            app: true,
            applet: true,
        }
    }

    fn daemon_only() -> Components {
        Components {
            daemon: true,
            app: false,
            applet: false,
        }
    }

    /// C8: pins that `Step::RestartOrStart` is the ONLY hard-error arm in
    /// `run`'s per-step executor match. `run` itself can't be driven
    /// directly without shelling out to real systemd/COSMIC commands — and
    /// several steps (`cleanup_legacy`, ...) touch the
    /// real `$HOME`, so calling them for real from a test would risk
    /// mutating the test-runner's actual machine, not a real seam to test
    /// through. Absent an executable seam, this pins the invariant
    /// structurally instead: it re-reads this file's own source and counts
    /// `?` inside the per-step match, so a future edit that adds a second
    /// `?` arm (silently promoting a best-effort step to a hard error)
    /// fails this test rather than passing unnoticed. Also scans for
    /// `return Err` — the other obvious spelling of a hard error, one a
    /// future arm could use without ever touching the `?` count above.
    #[test]
    fn only_restart_or_start_step_is_a_hard_error() {
        let src = include_str!("post_install.rs");
        let match_start = src
            .find("match step {")
            .expect("run's per-step executor match must exist");
        let match_end = src[match_start..]
            .find("\n    }\n")
            .expect("the per-step match must close");
        let match_block = &src[match_start..match_start + match_end];
        // Strip `//`-comment lines before counting: this very match block
        // carries a doc comment that itself mentions the character being
        // counted, which would otherwise inflate the count.
        let code_only: String = match_block
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(
            code_only.matches('?').count(),
            1,
            "expected exactly one `?` in run's executor match (Step::RestartOrStart); \
             found a different count — every other step must stay best-effort:\n{code_only}"
        );
        assert!(
            code_only.contains("Step::RestartOrStart => step_restart_or_start().await?"),
            "the one `?` must be on the RestartOrStart arm specifically:\n{code_only}"
        );
        // `?` isn't the only way an arm could turn hard-error: an arm could
        // instead be a block that does `return Err(..)` directly, which
        // wouldn't move the `?` count above and would slip past the assert
        // just made. Catch that spelling too.
        assert_eq!(
            code_only.matches("return Err").count(),
            0,
            "found a `return Err` in run's executor match outside the `?` \
             this test already pins — every other step must stay best-effort:\n{code_only}"
        );
    }

    #[test]
    fn plan_daemon_only_skips_launcher_nudge_and_applet_restart() {
        // Even with every environment probe favorable (systemctl+cosmic
        // available, applet "already installed", panel "running") — none of
        // that matters when the applet isn't part of THIS run's components.
        let steps = plan(daemon_only(), true, true, true);
        assert!(!steps.contains(&Step::NudgeLaunchers));
        assert!(!steps.contains(&Step::RestartPanel));
    }

    #[test]
    fn plan_applet_restart_requires_selected_installed_and_running_all_three() {
        let applet_only = Components {
            daemon: false,
            app: false,
            applet: true,
        };
        assert!(plan(applet_only, true, false, true).contains(&Step::RestartPanel));
        // Not previously installed (a fresh applet install, not an update):
        // no restart even if the panel happens to be running.
        assert!(!plan(applet_only, false, false, true).contains(&Step::RestartPanel));
        // Previously installed, but the panel isn't currently running:
        // nothing to restart.
        assert!(!plan(applet_only, true, false, false).contains(&Step::RestartPanel));
        // Applet not selected THIS run (e.g. a daemon-only update), even
        // though it was installed before and the panel is running.
        assert!(!plan(daemon_only(), true, false, true).contains(&Step::RestartPanel));
    }

    #[test]
    fn plan_no_systemctl_means_no_systemd_steps() {
        let steps = plan(all_three(), true, false, true);
        assert!(!steps.contains(&Step::DaemonReload));
        assert!(!steps.contains(&Step::RemoveLegacyUnit));
        assert!(!steps.contains(&Step::Enable));
        assert!(!steps.contains(&Step::RestartOrStart));
    }

    #[test]
    fn plan_no_daemon_component_means_no_systemd_steps_even_if_available() {
        let app_only = Components {
            daemon: false,
            app: true,
            applet: false,
        };
        let steps = plan(app_only, false, true, false);
        assert!(!steps.iter().any(|s| matches!(
            s,
            Step::DaemonReload | Step::RemoveLegacyUnit | Step::Enable | Step::RestartOrStart
        )));
    }

    #[test]
    fn plan_cleanup_legacy_always_runs_regardless_of_every_other_gate() {
        assert!(plan(Components::default(), false, false, false).contains(&Step::CleanupLegacy));
        assert!(plan(all_three(), true, true, true).contains(&Step::CleanupLegacy));
    }

    #[test]
    fn plan_app_only_nudges_launchers_with_no_systemd_or_shortcut_steps() {
        let app_only = Components {
            daemon: false,
            app: true,
            applet: false,
        };
        let steps = plan(app_only, false, true, false);
        assert!(steps.contains(&Step::NudgeLaunchers));
        assert!(
            !steps
                .iter()
                .any(|s| matches!(s, Step::DaemonReload | Step::RestartOrStart))
        );
    }

    #[test]
    fn plan_full_update_interactive_uses_the_documented_step_order() {
        // Everything on: daemon+app+applet all selected (an "all" update),
        // the applet was already installed and its panel is currently
        // running, systemctl and cosmic-panel both available, interactive
        // session. The documented order for the "all" case: systemd
        // install/enable/(re)start, applet panel restart, launcher nudge,
        // legacy cleanup.
        let steps = plan(all_three(), true, true, true);
        assert_eq!(
            steps,
            vec![
                Step::DaemonReload,
                Step::RemoveLegacyUnit,
                Step::Enable,
                Step::RestartOrStart,
                Step::RestartPanel,
                Step::NudgeLaunchers,
                Step::CleanupLegacy,
            ]
        );
    }
}
