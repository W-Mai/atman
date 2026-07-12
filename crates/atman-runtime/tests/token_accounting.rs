use atman_runtime::Session;

#[tokio::test]
async fn cumulative_input_tokens_accumulates_across_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();
    assert_eq!(session.cumulative_input_tokens(), 0);
    session.record_llm_call("claude-opus-4.7", 500, 100, 0, 0, None, None);
    assert_eq!(session.cumulative_input_tokens(), 500);
    session.record_llm_call("claude-opus-4.7", 250, 40, 0, 0, None, None);
    assert_eq!(session.cumulative_input_tokens(), 750);
    assert_eq!(session.last_model(), "claude-opus-4.7");
    session.reset_input_tokens_to(120);
    assert_eq!(session.cumulative_input_tokens(), 120);
}
