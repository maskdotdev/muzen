pub struct Token {
    pub expires_at: u64,
    pub user_id: Option<String>,
}

pub fn authorize_request(token: Option<Token>, now: u64) -> bool {
    match token {
        Some(token) => token.user_id.is_some() && token.expires_at >= now,
        None => false,
    }
}
