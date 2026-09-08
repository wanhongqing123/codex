//! TUI-owned Windows sandbox helpers retained while setup still runs in the local client process.
//!
//! TODO: These helpers inspect and modify the TUI host, so they do not support
//! cross-platform remote app servers. Move readiness and setup to the existing
//! `windowsSandbox/*` RPCs while preserving the pending permission profile,
//! use the server platform reported during initialization, and add a remote
//! equivalent for read-root grants.

use crate::legacy_core::config::Config;
use codex_config::types::WindowsSandboxModeToml;
use codex_features::Feature;
#[cfg(target_os = "windows")]
use codex_otel::SessionTelemetry;
use codex_protocol::config_types::WindowsSandboxLevel;
#[cfg(target_os = "windows")]
use codex_protocol::models::PermissionProfile;
#[cfg(target_os = "windows")]
use codex_utils_absolute_path::AbsolutePathBuf;
#[cfg(target_os = "windows")]
use std::collections::HashMap;
use std::path::Path;
#[cfg(target_os = "windows")]
use std::path::PathBuf;

#[cfg(target_os = "windows")]
pub(crate) fn record_world_writable_scan_result(
    session_telemetry: &SessionTelemetry,
    result: &anyhow::Result<usize>,
) {
    let (flagged_count, result) = match result {
        Ok(flagged_count) => (*flagged_count as i64, "success"),
        Err(_) => (0, "error"),
    };
    session_telemetry.histogram(
        "codex.windows_sandbox.world_writable_scan_flagged_directories",
        flagged_count,
        &[("result", result)],
    );
}

pub(crate) fn level_from_config(config: &Config) -> WindowsSandboxLevel {
    match config.permissions.windows_sandbox_mode {
        Some(WindowsSandboxModeToml::Elevated) => WindowsSandboxLevel::Elevated,
        Some(WindowsSandboxModeToml::Unelevated) => WindowsSandboxLevel::RestrictedToken,
        None if config.features.enabled(Feature::WindowsSandboxElevated) => {
            WindowsSandboxLevel::Elevated
        }
        None if config.features.enabled(Feature::WindowsSandbox) => {
            WindowsSandboxLevel::RestrictedToken
        }
        None => WindowsSandboxLevel::Disabled,
    }
}

#[cfg(target_os = "windows")]
pub(crate) use codex_windows_sandbox::sandbox_setup_is_complete;

/// Whether this installation can actually provision the elevated Windows sandbox.
///
/// Only the elevated path is covered: it is the one that shells out to
/// `codex-windows-sandbox-setup.exe`. A distribution may omit that helper, and
/// offering to "create a sandbox" that cannot be created is worse than not
/// offering it — the launch surfaces as an OS-level "file not found" dialog
/// rather than anything Codex can explain. This says nothing about the
/// unelevated path, which provisions in-process and needs no helper.
///
/// Under `cfg(test)` this reads a thread-local instead of the filesystem: the
/// real probe keys off `current_exe()`, which during tests is the test harness,
/// so it would otherwise report "unavailable" purely because of where the test
/// binary lives.
pub(crate) fn elevated_setup_is_available() -> bool {
    #[cfg(test)]
    {
        test_support::elevated_setup_available()
    }
    #[cfg(all(not(test), target_os = "windows"))]
    {
        codex_windows_sandbox::elevated_setup_helper_is_bundled()
    }
    #[cfg(all(not(test), not(target_os = "windows")))]
    {
        false
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::cell::Cell;

    thread_local! {
        /// Defaults to `true` so existing tests describe a normal install that
        /// carries the helper; flip it to cover a distribution that omits it.
        static ELEVATED_SETUP_AVAILABLE: Cell<bool> = const { Cell::new(true) };
    }

    pub(crate) fn elevated_setup_available() -> bool {
        ELEVATED_SETUP_AVAILABLE.with(Cell::get)
    }

    /// Restores the default when dropped so one test cannot leak into the next.
    pub(crate) struct ElevatedSetupAvailabilityGuard;

    impl Drop for ElevatedSetupAvailabilityGuard {
        fn drop(&mut self) {
            ELEVATED_SETUP_AVAILABLE.with(|cell| cell.set(true));
        }
    }

    pub(crate) fn set_elevated_setup_available(value: bool) -> ElevatedSetupAvailabilityGuard {
        ELEVATED_SETUP_AVAILABLE.with(|cell| cell.set(value));
        ElevatedSetupAvailabilityGuard
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn sandbox_setup_is_complete(_codex_home: &Path) -> bool {
    false
}

#[cfg(target_os = "windows")]
pub(crate) fn run_elevated_setup(
    permission_profile: &PermissionProfile,
    workspace_roots: &[AbsolutePathBuf],
    command_cwd: &Path,
    env_map: &HashMap<String, String>,
    codex_home: &Path,
) -> anyhow::Result<()> {
    let permissions = codex_windows_sandbox::ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
        permission_profile,
        workspace_roots,
    )?;
    codex_windows_sandbox::run_elevated_setup(
        codex_windows_sandbox::SandboxSetupRequest {
            permissions: &permissions,
            command_cwd,
            env_map,
            codex_home,
            proxy_enforced: false,
        },
        codex_windows_sandbox::SetupRootOverrides::default(),
    )
}

#[cfg(target_os = "windows")]
pub(crate) fn elevated_setup_failure_details(err: &anyhow::Error) -> Option<(String, String)> {
    let failure = codex_windows_sandbox::extract_setup_failure(err)?;
    Some((
        failure.code.as_str().to_string(),
        codex_windows_sandbox::sanitize_setup_metric_tag_value(&failure.message),
    ))
}

#[cfg(target_os = "windows")]
pub(crate) fn elevated_setup_failure_metric_name(err: &anyhow::Error) -> &'static str {
    if codex_windows_sandbox::extract_setup_failure(err).is_some_and(|failure| {
        matches!(
            failure.code,
            codex_windows_sandbox::SetupErrorCode::OrchestratorHelperLaunchCanceled
        )
    }) {
        "codex.windows_sandbox.elevated_setup_canceled"
    } else {
        "codex.windows_sandbox.elevated_setup_failure"
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn grant_read_root_non_elevated(
    permission_profile: &PermissionProfile,
    workspace_roots: &[AbsolutePathBuf],
    command_cwd: &Path,
    env_map: &HashMap<String, String>,
    codex_home: &Path,
    read_root: &Path,
) -> anyhow::Result<PathBuf> {
    if !read_root.is_absolute() {
        anyhow::bail!("path must be absolute: {}", read_root.display());
    }
    if !read_root.exists() {
        anyhow::bail!("path does not exist: {}", read_root.display());
    }
    if !read_root.is_dir() {
        anyhow::bail!("path must be a directory: {}", read_root.display());
    }

    let canonical_root = dunce::canonicalize(read_root)?;
    codex_windows_sandbox::run_setup_refresh_with_extra_read_roots(
        permission_profile,
        workspace_roots,
        command_cwd,
        env_map,
        codex_home,
        vec![canonical_root.clone()],
        /*proxy_enforced*/ false,
    )?;
    Ok(canonical_root)
}
