use super::*;

#[test]
fn callback_request_fails_after_reader_eof_instead_of_hanging() {
    let transport = InteractiveTransport::new(std::io::Cursor::new(Vec::new()), Vec::new());
    assert!(
        transport.read_frame().expect("transport read").is_none(),
        "empty stdin reads as EOF"
    );
    let error = transport
        .request("model.complete", json!({}))
        .expect_err("callback request after EOF must fail");
    assert!(error.to_string().contains("model.complete"));
}

#[test]
fn malformed_line_surfaces_as_recoverable_parse_error() {
    let input = b"{\"this is not json\n{\"jsonrpc\":\"2.0\",\"method\":\"ping\"}\n".to_vec();
    let transport = InteractiveTransport::new(std::io::Cursor::new(input), Vec::new());
    match transport.read_frame().expect("transport read") {
        Some(TransportEvent::ParseError(message)) => {
            assert!(message.contains("invalid JSON-RPC frame"));
        }
        other => panic!("expected parse error, got {:?}", other.is_some()),
    }
    match transport.read_frame().expect("transport read") {
        Some(TransportEvent::Frame(JsonRpcFrame::Notification)) => {}
        other => panic!(
            "stream continues past parse error, got {:?}",
            other.is_some()
        ),
    }
    assert!(transport.read_frame().expect("transport read").is_none());
}
