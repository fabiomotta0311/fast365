use f365_teams::diagnostics::{Bundle, Contract, ErrorCategory, Event, State};

#[test]
fn diagnostics_accept_only_typed_fields_and_never_copy_incident_content() {
    let secret = "token=evil; cookie=evil; https://example.test/?sig=evil; message=private";
    let event = Event {
        contract: Contract::SendReconcile,
        state: State::Uncertain,
        error: Some(ErrorCategory::Timeout),
        http_status: None,
        elapsed_ms: 100,
        count: 1,
    };
    let json = serde_json::to_string(&Bundle::new(vec![event])).unwrap();
    for forbidden in [secret, "token", "cookie", "https://", "private", "evil"] {
        assert!(
            !json.contains(forbidden),
            "unexpected {forbidden} in {json}"
        );
    }
    assert!(json.contains("SEND-RECONCILE-001"));
}
