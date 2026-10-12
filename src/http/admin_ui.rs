//! Embedded admin SPA (`admin-ui/`) and Swagger UI (PLAN.md §8.1).
//!
//! Both are public static shells: their data comes from the token-protected `/api/v1`
//! routes (a browser navigation or iframe cannot carry a bearer header). Swagger UI is
//! served from the vendored bundle and pointed at the existing public
//! `/api-docs/openapi.json`, so it never needs the network.

use axum::{
    Router,
    extract::Path,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use rust_embed::RustEmbed;
use utoipa_swagger_ui::{Config, SwaggerUi};

use super::openapi::SPEC_PATH;
use crate::state::SharedState;

#[derive(RustEmbed)]
#[folder = "admin-ui/"]
struct Assets;

pub const DOCS_PATH: &str = "/admin/docs";

pub fn routes() -> Router<SharedState> {
    // `validator_url("none")`: Swagger UI's default validator badge calls an external
    // service.
    let swagger = SwaggerUi::new(DOCS_PATH).config(
        Config::new([SPEC_PATH])
            .persist_authorization(true)
            .validator_url("none"),
    );
    Router::new()
        .route("/admin", get(|| async { Redirect::permanent("/admin/") }))
        .route("/admin/", get(|| async { asset("index.html") }))
        .route("/admin/{*path}", get(spa))
        .merge(swagger)
}

fn asset(path: &str) -> Response {
    let Some(file) = Assets::get(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mime = file.metadata.mimetype().to_owned();
    let mut resp = (StatusCode::OK, file.data.into_owned()).into_response();
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&mime) {
        h.insert(header::CONTENT_TYPE, v);
    }
    // The shell must pick up new builds; assets are small.
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    resp
}

/// An asset, or `index.html` for client-side routes. A missing file with an
/// extension is a real 404 (not the shell), and traversal-looking paths are refused.
async fn spa(Path(path): Path<String>) -> Response {
    if path.starts_with('/') || path.split(['/', '\\']).any(|seg| seg == "..") {
        return StatusCode::NOT_FOUND.into_response();
    }
    if Assets::get(&path).is_some() {
        return asset(&path);
    }
    let last = path.rsplit('/').next().unwrap_or_default();
    if last.contains('.') {
        StatusCode::NOT_FOUND.into_response()
    } else {
        asset("index.html")
    }
}
