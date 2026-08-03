use needle_bench::{MetricPrecision, parse_codex_jsonl};

#[test]
fn structured_success_fixture_requires_terminal_and_known_tool_item() {
    let parsed = parse_codex_jsonl(include_str!("fixtures/codex-success.jsonl"));
    assert_eq!(parsed.thread_id.as_deref(), Some("01234567-89ab-cdef-0123-456789abcdef"));
    assert!(parsed.terminal_event);
    assert_eq!(parsed.terminal_success, Some(true));
    assert_eq!(parsed.tool_call_success, Some(true));
    assert_eq!(parsed.continuation_success, Some(true));
    assert_eq!(parsed.usage.precision, MetricPrecision::Exact);
}

#[test]
fn structured_error_fixture_cannot_be_success() {
    let parsed = parse_codex_jsonl(include_str!("fixtures/codex-error.jsonl"));
    assert_eq!(parsed.terminal_success, Some(false));
    assert_eq!(parsed.tool_call_success, Some(false));
    assert_eq!(parsed.continuation_success, Some(false));
}

#[test]
fn aggregate_and_partial_metrics_keep_precision_without_float_coercion() {
    let aggregate = parse_codex_jsonl(include_str!("fixtures/codex-aggregate.jsonl"));
    assert_eq!(aggregate.usage.input_precision, MetricPrecision::Aggregate);
    assert_eq!(aggregate.usage.precision, MetricPrecision::Aggregate);
    let partial = parse_codex_jsonl(include_str!("fixtures/codex-partial.jsonl"));
    assert_eq!(partial.usage.input_precision, MetricPrecision::Exact);
    assert_eq!(partial.usage.output_precision, MetricPrecision::Unavailable);
    assert_eq!(partial.usage.precision, MetricPrecision::Partial);
}
