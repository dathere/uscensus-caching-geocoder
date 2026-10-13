//! The `/api/v1` router and its OpenAPI document (PLAN.md §8), served unauthenticated
//! at `/api-docs/openapi.json`. `info` comes from Cargo metadata (package version, not
//! the git sha, so the document is stable across commits).

use axum::Router;
use utoipa::{
    Modify, OpenApi,
    openapi::{
        OpenApi as Spec,
        security::{HttpAuthScheme, HttpBuilder, SecurityScheme},
    },
};
use utoipa_axum::router::OpenApiRouter;

use super::{api_admin, api_cache, api_jobs};
use crate::state::SharedState;

pub const SPEC_PATH: &str = "/api-docs/openapi.json";

#[derive(OpenApi)]
#[openapi(
    modifiers(&BearerAuth),
    security(("bearerAuth" = [])),
    tags(
        (name = "jobs", description = "Durable async geocoding jobs"),
        (name = "cache", description = "Cache inspection and maintenance"),
        (name = "admin", description = "Catalog, runtime config and upstream state")
    )
)]
struct ApiDoc;

struct BearerAuth;

impl Modify for BearerAuth {
    fn modify(&self, openapi: &mut Spec) {
        openapi
            .components
            .get_or_insert_with(Default::default)
            .add_security_scheme(
                "bearerAuth",
                SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).build()),
            );
    }
}

/// Every `/api/v1` route (unauthenticated; the caller adds the token layer) and the
/// OpenAPI document describing them.
pub fn api() -> (Router<SharedState>, Spec) {
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .merge(api_jobs::router())
        .merge(api_cache::router())
        .merge(api_admin::router())
        .split_for_parts()
}
