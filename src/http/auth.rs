//! Bearer-token check for admin routes (PLAN.md §8): when `admin.token` is set, requests
//! need `Authorization: Bearer <token>`; unset leaves the routes open (with a startup
//! warning).

use axum::{
    extract::{Request, State},
    http::header,
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::{error::AppError, state::SharedState};

/// Compare without an early exit on the first differing byte.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn authorized(token: Option<&str>, authorization: Option<&str>) -> bool {
    let Some(token) = token else {
        return true;
    };
    authorization
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|given| same(given.trim().as_bytes(), token.as_bytes()))
}

pub async fn require_token(State(st): State<SharedState>, req: Request, next: Next) -> Response {
    let settings = st.settings();
    let given = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if authorized(settings.config.admin.token.as_deref(), given) {
        next.run(req).await
    } else {
        AppError::Unauthorized("A valid bearer token is required".into()).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_rules() {
        assert!(authorized(None, None));
        assert!(authorized(Some("s3cret"), Some("Bearer s3cret")));
        assert!(!authorized(Some("s3cret"), None));
        assert!(!authorized(Some("s3cret"), Some("Bearer s3cre")));
        assert!(!authorized(Some("s3cret"), Some("Bearer s3cret2")));
        assert!(!authorized(Some("s3cret"), Some("Basic s3cret")));
    }
}
