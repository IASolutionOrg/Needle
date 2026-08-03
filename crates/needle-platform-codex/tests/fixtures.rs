use needle_platform_codex::{
    CompactInput, HookConfig, SessionStartInput, StopInput, handle_post_compact,
    handle_pre_compact, handle_session_start, handle_stop,
};

#[test]
fn codex_01440_fixtures_accept_missing_and_unknown_fields() {
    let start: SessionStartInput =
        serde_json::from_str(include_str!("fixtures/session-start.json")).unwrap();
    assert!(handle_session_start(&start, &HookConfig::default()).is_ok());
    let valid: StopInput = serde_json::from_str(include_str!("fixtures/stop-valid.json")).unwrap();
    assert_eq!(handle_stop(&valid, &HookConfig::default()).unwrap().decision, None);
    let missing: StopInput =
        serde_json::from_str(include_str!("fixtures/stop-missing-fields.json")).unwrap();
    assert_eq!(handle_stop(&missing, &HookConfig::default()).unwrap().decision, None);
    let unknown: StopInput =
        serde_json::from_str(include_str!("fixtures/stop-unknown-fields.json")).unwrap();
    assert!(unknown.extra.get("new_codex_field").and_then(|value| value.as_object()).is_some());
    let pre: CompactInput =
        serde_json::from_str(include_str!("fixtures/pre-compact.json")).unwrap();
    let post: CompactInput =
        serde_json::from_str(include_str!("fixtures/post-compact.json")).unwrap();
    assert!(serde_json::to_value(handle_pre_compact(&pre)).unwrap().is_object());
    assert!(serde_json::to_value(handle_post_compact(&post)).unwrap().is_object());
}
