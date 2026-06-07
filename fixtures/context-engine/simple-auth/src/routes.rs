use crate::auth::token::{authorize_request, Token};

pub fn handle_private_route(token: Option<Token>, now: u64) -> &'static str {
    if authorize_request(token, now) {
        "ok"
    } else {
        "unauthorized"
    }
}
