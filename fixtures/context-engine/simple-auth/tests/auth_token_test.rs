use simple_auth::auth::token::{authorize_request, Token};

#[test]
fn rejects_anonymous_request() {
    assert!(!authorize_request(None, 10));
}

#[test]
fn rejects_expired_token() {
    assert!(!authorize_request(
        Some(Token {
            expires_at: 9,
            user_id: Some("user-1".to_string()),
        }),
        10,
    ));
}
