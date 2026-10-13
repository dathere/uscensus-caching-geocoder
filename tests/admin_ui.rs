//! Embedded admin SPA and Swagger UI (PLAN.md §8.1): public static shells.

mod common;

use common::harness;

#[tokio::test]
async fn shell_assets_and_spa_fallback() {
    // Building the router would panic on a route conflict with Swagger UI's routes.
    let h = harness(|c| c.admin.token = Some("t".into())).await;

    let (status, headers, _) = h.get("/admin").await;
    assert_eq!(status, 308);
    assert_eq!(headers["location"], "/admin/");

    let (status, headers, index) = h.get("/admin/").await;
    assert_eq!(status, 200);
    assert!(
        headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert!(String::from_utf8_lossy(&index).contains("<title>ucgeocoder admin</title>"));

    for (path, ct) in [
        ("/admin/app.js", "javascript"),
        ("/admin/app.css", "text/css"),
        ("/admin/index.html", "text/html"),
    ] {
        let (status, headers, body) = h.get(path).await;
        assert_eq!(status, 200, "{path}");
        assert!(
            headers["content-type"].to_str().unwrap().contains(ct),
            "{path}"
        );
        assert!(!body.is_empty(), "{path}");
    }

    // Client-side routes get the shell; missing files and traversal do not.
    for path in ["/admin/jobs", "/admin/entries/some/deep/route"] {
        let (status, _, body) = h.get(path).await;
        assert_eq!(status, 200, "{path}");
        assert_eq!(body, index, "{path}");
    }
    for path in [
        "/admin/missing.js",
        "/admin/../Cargo.toml",
        "/admin/a/../../Cargo.toml",
        "/admin/%2E%2E/Cargo.toml",
    ] {
        assert_eq!(h.get(path).await.0, 404, "{path}");
    }
}

/// Every `src=`/`href=` value in an HTML document.
fn references(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    for attr in ["src=\"", "href=\""] {
        let mut rest = html;
        while let Some(i) = rest.find(attr) {
            rest = &rest[i + attr.len()..];
            if let Some(end) = rest.find('"') {
                out.push(rest[..end].to_owned());
                rest = &rest[end..];
            }
        }
    }
    out
}

#[tokio::test]
async fn swagger_ui_is_served_from_vendored_assets() {
    let h = harness(|c| c.admin.token = Some("t".into())).await;
    let (status, _, html) = h.get("/admin/docs/").await;
    assert_eq!(status, 200);
    let html = String::from_utf8(html.to_vec()).unwrap();
    let refs = references(&html);
    assert!(!refs.is_empty(), "{html}");
    for r in &refs {
        assert!(
            !r.contains("://") && !r.starts_with("//"),
            "external reference {r}"
        );
        let url = if r.starts_with('/') {
            r.clone()
        } else {
            format!("/admin/docs/{}", r.trim_start_matches("./"))
        };
        let (status, _, body) = h.get(&url).await;
        assert_eq!(status, 200, "{url}");
        assert!(!body.is_empty(), "{url}");
    }

    // The UI is configured with our public spec and no external validator.
    let (status, _, init) = h.get("/admin/docs/swagger-initializer.js").await;
    assert_eq!(status, 200);
    let init = String::from_utf8_lossy(&init);
    assert!(init.contains("/api-docs/openapi.json"), "{init}");
    assert!(init.contains(r#""validatorUrl": "none""#), "{init}");

    // The spec itself stays public with a token configured.
    assert_eq!(h.get("/api-docs/openapi.json").await.0, 200);
    let (status, headers, _) = h.get("/admin/docs").await;
    assert!(status.is_redirection());
    assert_eq!(headers["location"], "/admin/docs/");
}
