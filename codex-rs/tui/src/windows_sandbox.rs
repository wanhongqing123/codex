//! Windows sandbox configuration, managed requirements, and executor selection for the TUI.

use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ConfigReadResponse;
use codex_app_server_protocol::ConfigRequirementsReadResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::WindowsSandboxImplementation;
use codex_app_server_protocol::WindowsSandboxSetupMode;
use codex_protocol::config_types::WindowsSandboxLevel;
use uuid::Uuid;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct WindowsSandboxConfig {
    pub(crate) mxc_selected: bool,
    pub(crate) mode: Option<WindowsSandboxSetupMode>,
    // None means policy has not been loaded; a loaded null list allows both modes.
    pub(crate) requirements: Option<Option<Vec<WindowsSandboxSetupMode>>>,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl WindowsSandboxConfig {
    pub(crate) fn from_responses(
        config: &ConfigReadResponse,
        requirements: ConfigRequirementsReadResponse,
    ) -> Self {
        let configured_sandbox = config
            .config
            .additional
            .get("windows")
            .and_then(|windows| windows.get("sandbox"));
        let mxc_selected = configured_sandbox
            .and_then(|implementation| serde_json::from_value(implementation.clone()).ok())
            == Some(WindowsSandboxImplementation::Mxc);
        let mut state = Self {
            mxc_selected,
            mode: configured_sandbox
                .and_then(|mode| serde_json::from_value(mode.clone()).ok())
                .or_else(|| {
                    if mxc_selected {
                        return None;
                    }
                    let features = config.config.additional.get("features")?;
                    [
                        (
                            "elevated_windows_sandbox",
                            WindowsSandboxSetupMode::Elevated,
                        ),
                        (
                            "experimental_windows_sandbox",
                            WindowsSandboxSetupMode::Unelevated,
                        ),
                        (
                            "enable_experimental_windows_sandbox",
                            WindowsSandboxSetupMode::Unelevated,
                        ),
                    ]
                    .into_iter()
                    .find_map(|(key, mode)| {
                        (features.get(key).and_then(serde_json::Value::as_bool) == Some(true))
                            .then_some(mode)
                    })
                }),
            requirements: Some(
                requirements
                    .requirements
                    .and_then(|requirements| requirements.allowed_windows_sandbox_implementations)
                    .map(|allowed| {
                        allowed
                            .into_iter()
                            .filter_map(|implementation| match implementation {
                                WindowsSandboxImplementation::Elevated => {
                                    Some(WindowsSandboxSetupMode::Elevated)
                                }
                                WindowsSandboxImplementation::Unelevated => {
                                    Some(WindowsSandboxSetupMode::Unelevated)
                                }
                                WindowsSandboxImplementation::Mxc => None,
                            })
                            .collect()
                    }),
            ),
        };
        if !state.mxc_selected
            && let Some(Some(allowed)) = &state.requirements
            && !state.mode.is_some_and(|mode| allowed.contains(&mode))
        {
            // Managed requirements prefer elevated when the configured value is disallowed.
            state.mode = [
                WindowsSandboxSetupMode::Elevated,
                WindowsSandboxSetupMode::Unelevated,
            ]
            .into_iter()
            .find(|mode| allowed.contains(mode));
        }
        state
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.mxc_selected || self.mode.is_some()
    }

    pub(crate) fn level(&self) -> WindowsSandboxLevel {
        match self.mode {
            Some(WindowsSandboxSetupMode::Elevated) => WindowsSandboxLevel::Elevated,
            Some(WindowsSandboxSetupMode::Unelevated) => WindowsSandboxLevel::RestrictedToken,
            None => WindowsSandboxLevel::Disabled,
        }
    }

    pub(crate) fn allows(&self, mode: WindowsSandboxSetupMode) -> bool {
        match &self.requirements {
            Some(Some(allowed)) => allowed.contains(&mode),
            Some(None) => true,
            None => false,
        }
    }

    pub(crate) fn requires_elevated(&self) -> bool {
        self.mode == Some(WindowsSandboxSetupMode::Elevated)
            && matches!(self.requirements, Some(Some(_)))
    }

    pub(crate) async fn read(
        handle: AppServerRequestHandle,
        cwd: String,
    ) -> color_eyre::Result<Self> {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let config = crate::config_update::read_effective_config(handle.clone(), cwd).await?;
            let requirements = handle
                .request_typed(ClientRequest::ConfigRequirementsRead {
                    request_id: RequestId::String(format!(
                        "tui-windows-sandbox-requirements-{}",
                        Uuid::new_v4()
                    )),
                    params: None,
                })
                .await?;
            Ok(Self::from_responses(&config, requirements))
        })
        .await?
    }
}

/// A server-local connection can still select remote executors. Missing selections are unknown.
pub(crate) fn host_from_environments(
    environments: Option<&[codex_app_server_protocol::ThreadEnvironment]>,
) -> crate::app::WindowsSandboxHost {
    use crate::app::WindowsSandboxHost;
    let Some(environments) = environments.filter(|environments| !environments.is_empty()) else {
        return WindowsSandboxHost::Unknown;
    };
    let local = environments
        .iter()
        .any(|environment| environment.environment_id == codex_exec_server::LOCAL_ENVIRONMENT_ID);
    let remote = environments
        .iter()
        .any(|environment| environment.environment_id != codex_exec_server::LOCAL_ENVIRONMENT_ID);
    match (local, remote) {
        (true, false) => WindowsSandboxHost::Local,
        (true, true) => WindowsSandboxHost::Mixed,
        (false, true) => WindowsSandboxHost::Remote,
        (false, false) => WindowsSandboxHost::Unknown,
    }
}

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

/// Environment switch an embedder sets to stop *offering* the Windows sandbox.
pub(crate) const SUPPRESS_OPTIONAL_PROMPT_ENV: &str =
    "CODEX_SUPPRESS_OPTIONAL_WINDOWS_SANDBOX_PROMPT";

/// Whether the host asked us to stop offering to turn the sandbox on.
///
/// This suppresses an *offer*, nothing else. It does not change the sandbox
/// level, write any config, or relax an approval policy: a sandbox that policy
/// *requires* still prompts, because that prompt is how a required sandbox gets
/// provisioned. It exists because the optional nudge fires on
/// `trust_decision_was_made && level == Disabled`, and "off" is spelled by
/// omitting the `[windows] sandbox` key — so an embedder that deliberately runs
/// without a sandbox is re-asked for every new directory it ever opens, with no
/// way to record the answer.
pub(crate) fn optional_prompt_is_suppressed() -> bool {
    #[cfg(test)]
    {
        test_support::optional_prompt_suppressed()
    }
    #[cfg(not(test))]
    {
        matches!(
            std::env::var(SUPPRESS_OPTIONAL_PROMPT_ENV).as_deref(),
            Ok("1") | Ok("true")
        )
    }
}

/// Whether the trust screen may say that continuing will create a sandbox.
///
/// The hint keys off the same `level == Disabled` that triggers the offer, so
/// suppressing the offer without suppressing this would leave the onboarding
/// screen promising a sandbox that is never created and never even asked about.
/// Keeping both behind one predicate is what stops the copy and the behaviour
/// drifting apart.
pub(crate) fn trust_screen_may_promise_sandbox(config: &WindowsSandboxConfig) -> bool {
    !config.is_enabled() && !optional_prompt_is_suppressed()
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::cell::Cell;

    thread_local! {
        /// Defaults to `true` so existing tests describe a normal install that
        /// carries the helper; flip it to cover a distribution that omits it.
        static ELEVATED_SETUP_AVAILABLE: Cell<bool> = const { Cell::new(true) };

        /// Defaults to `false` so existing tests keep describing a standalone
        /// Codex, which still offers the sandbox.
        static OPTIONAL_PROMPT_SUPPRESSED: Cell<bool> = const { Cell::new(false) };
    }

    pub(crate) fn optional_prompt_suppressed() -> bool {
        OPTIONAL_PROMPT_SUPPRESSED.with(Cell::get)
    }

    /// Restores the default when dropped so one test cannot leak into the next.
    pub(crate) struct OptionalPromptSuppressionGuard;

    impl Drop for OptionalPromptSuppressionGuard {
        fn drop(&mut self) {
            OPTIONAL_PROMPT_SUPPRESSED.with(|cell| cell.set(false));
        }
    }

    pub(crate) fn set_optional_prompt_suppressed(value: bool) -> OptionalPromptSuppressionGuard {
        OPTIONAL_PROMPT_SUPPRESSED.with(|cell| cell.set(value));
        OptionalPromptSuppressionGuard
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

#[cfg(test)]
#[path = "windows_sandbox_tests.rs"]
mod tests;
