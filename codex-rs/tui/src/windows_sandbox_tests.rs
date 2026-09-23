use super::*;
use codex_app_server_protocol::WindowsSandboxSetupMode::Elevated;
use codex_app_server_protocol::WindowsSandboxSetupMode::Unelevated;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn windows_sandbox_server_policy_controls_setup_choices() {
    for (configured, allowed, mode, expected) in [
        (
            Some("unelevated"),
            serde_json::Value::Null,
            Some(Unelevated),
            (true, true, false, true),
        ),
        (
            Some("unelevated"),
            json!(["elevated"]),
            Some(Elevated),
            (true, false, true, true),
        ),
        (
            None,
            json!(["unelevated", "elevated"]),
            Some(Elevated),
            (true, true, true, true),
        ),
        (
            None,
            json!(["unelevated"]),
            Some(Unelevated),
            (false, true, false, true),
        ),
        (
            Some("unelevated"),
            json!([]),
            None,
            (false, false, false, false),
        ),
        (
            Some("mxc"),
            json!(["elevated"]),
            None,
            (true, false, false, true),
        ),
    ] {
        let config = serde_json::from_value(json!({
            "config": {
                "windows": {"sandbox": configured},
                "features": {"elevated_windows_sandbox": configured == Some("mxc")}
            },
            "origins": {}
        }))
        .unwrap();
        let requirements = serde_json::from_value(json!({
            "requirements": {"allowedWindowsSandboxImplementations": allowed}
        }))
        .unwrap();
        let state = WindowsSandboxConfig::from_responses(&config, requirements);
        assert_eq!(state.mode, mode);
        assert_eq!(
            (
                state.allows(Elevated),
                state.allows(Unelevated),
                state.requires_elevated(),
                state.is_enabled(),
            ),
            expected
        );
    }
    for (key, mode) in [
        ("elevated_windows_sandbox", Elevated),
        ("experimental_windows_sandbox", Unelevated),
        ("enable_experimental_windows_sandbox", Unelevated),
    ] {
        for (explicit, expected) in [(None, mode), (Some("elevated"), Elevated)] {
            let config = serde_json::from_value(json!({
                "config": {"windows": {"sandbox": explicit}, "features": {key: true}},
                "origins": {}
            }))
            .unwrap();
            let requirements = serde_json::from_value(json!({"requirements": null})).unwrap();
            assert_eq!(
                WindowsSandboxConfig::from_responses(&config, requirements).mode,
                Some(expected)
            );
        }
    }
    let unknown = WindowsSandboxConfig::default();
    assert!(!unknown.allows(Elevated));
    assert!(!unknown.allows(Unelevated));
}

#[test]
fn embedded_prompt_suppression_preserves_server_sandbox_policy() {
    let config = serde_json::from_value(json!({
        "config": {"windows": {"sandbox": "elevated"}}, "origins": {}
    }))
    .unwrap();
    let requirements = serde_json::from_value(json!({
        "requirements": {"allowedWindowsSandboxImplementations": ["elevated"]}
    }))
    .unwrap();
    let state = WindowsSandboxConfig::from_responses(&config, requirements);
    let before = state.clone();
    let _suppressed = test_support::set_optional_prompt_suppressed(/*value*/ true);
    assert!(!trust_screen_may_promise_sandbox(&state));
    assert!(state.requires_elevated());
    assert!(!state.allows(Unelevated));
    assert_eq!(state, before);
}

#[test]
fn embedded_prompt_suppression_withdraws_only_the_optional_onboarding_offer() {
    let config = serde_json::from_value(json!({"config": {}, "origins": {}})).unwrap();
    let requirements = serde_json::from_value(json!({"requirements": null})).unwrap();
    let state = WindowsSandboxConfig::from_responses(&config, requirements);
    assert!(trust_screen_may_promise_sandbox(&state));
    let before = state.clone();
    let _suppressed = test_support::set_optional_prompt_suppressed(/*value*/ true);
    assert!(!trust_screen_may_promise_sandbox(&state));
    assert_eq!(state, before);
}
