//! Serving assets: bundles concatenated in load order, single files out
//! of an addon's `static/` directory, and nothing else.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rusdoo_http::assets::AssetHub;
use rusdoo_http::dispatch::OrmService;
use rusdoo_http::routes::router;
use rusdoo_modules::assets::resolve_bundles;
use rusdoo_modules::manifest::parse_manifest;
use rusdoo_orm::registry::Registry;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

/// A throwaway addons tree holding one addon that ships assets.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let root =
            std::env::temp_dir().join(format!("rusdoo-http-assets-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("fixture root");
        Fixture { root }
    }

    fn write(&self, relative: &str, content: &str) {
        let path = self.root.join(relative);
        std::fs::create_dir_all(path.parent().expect("has parent")).expect("subdir");
        std::fs::write(path, content).expect("write");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// An addon `demo` shipping two scripts, a stylesheet and an image.
fn app(fixture: &Fixture) -> axum::Router {
    let manifest_source = r#"{
        'name': 'Demo',
        'assets': {'web.assets_backend': [
            'demo/static/src/a.js',
            'demo/static/src/b.js',
            'demo/static/src/app.css',
        ]},
    }"#;
    let addon = fixture.root.join("demo");
    fixture.write("demo/__manifest__.py", manifest_source);
    fixture.write("demo/static/src/a.js", "const a = 1;");
    fixture.write("demo/static/src/b.js", "const b = 2;");
    fixture.write("demo/static/src/app.css", "body { color: red; }");
    fixture.write("demo/static/img/logo.png", "\u{89}PNG-not-really");
    fixture.write("demo/secret.txt", "outside static/");

    let mut manifest = parse_manifest(manifest_source, "demo").expect("manifest");
    manifest.path = addon.clone();
    let bundles = resolve_bundles(&[&manifest]).expect("bundles resolve");
    let roots: HashMap<String, PathBuf> = [("demo".to_string(), addon)].into_iter().collect();

    let url = std::env::var("RUSDOO_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:rusdoo@localhost:55432/postgres".into());
    let service = OrmService::insecure(
        Arc::new(Registry::new()),
        rusdoo_orm::db::lazy_pool(&url).unwrap(),
    )
    .with_assets(AssetHub::new(bundles, roots));
    router(service)
}

async fn get(app: axum::Router, uri: &str) -> (StatusCode, HashMap<String, String>, String) {
    fetch(app, uri, &[]).await
}

async fn fetch(
    app: axum::Router,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, HashMap<String, String>, String) {
    let mut request = Request::get(uri);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                value.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn a_bundle_is_served_concatenated_in_load_order() {
    let fixture = Fixture::new("bundle");
    let (status, headers, body) = get(app(&fixture), "/web/assets/web.assets_backend.js").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/javascript; charset=utf-8");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    let a = body.find("const a = 1;").expect("a.js in the bundle");
    let b = body.find("const b = 2;").expect("b.js in the bundle");
    assert!(a < b, "declaration order is load order");
    // the css of the same bundle is not in its js answer
    assert!(!body.contains("color: red"));
    // each chunk says where it came from
    assert!(body.contains("/* demo/static/src/a.js */"));
}

#[tokio::test]
async fn the_css_of_a_bundle_is_a_separate_answer() {
    let fixture = Fixture::new("css");
    let (status, headers, body) = get(app(&fixture), "/web/assets/web.assets_backend.css").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/css; charset=utf-8");
    assert!(body.contains("color: red"));
    assert!(!body.contains("const a"));
}

#[tokio::test]
async fn an_unchanged_bundle_costs_a_304() {
    let fixture = Fixture::new("etag");
    let (status, headers, _) = get(app(&fixture), "/web/assets/web.assets_backend.js").await;
    assert_eq!(status, StatusCode::OK);
    let etag = headers["etag"].clone();

    let (status, _, body) = fetch(
        app(&fixture),
        "/web/assets/web.assets_backend.js",
        &[("if-none-match", &etag)],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty());
}

#[tokio::test]
async fn a_versioned_url_may_be_cached_forever() {
    let fixture = Fixture::new("versioned");
    let (status, headers, body) = get(
        app(&fixture),
        "/web/assets/1a2b3c/web.assets_backend.js",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers["cache-control"].contains("immutable"));
    assert!(body.contains("const a = 1;"));

    // while the plain URL must be revalidated
    let (_, headers, _) = get(app(&fixture), "/web/assets/web.assets_backend.js").await;
    assert_eq!(headers["cache-control"], "no-cache");
}

#[tokio::test]
async fn an_unknown_bundle_is_a_404() {
    let fixture = Fixture::new("unknown");
    let (status, _, _) = get(app(&fixture), "/web/assets/web.nope.js").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // a known bundle asked for in a type it has no files of, too
    let (status, _, _) = get(app(&fixture), "/web/assets/web.assets_backend.xml").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_static_file_is_served_with_its_own_type() {
    let fixture = Fixture::new("static");
    let (status, headers, body) = get(app(&fixture), "/demo/static/src/a.js").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/javascript; charset=utf-8");
    assert_eq!(body, "const a = 1;");

    let (status, headers, _) = get(app(&fixture), "/demo/static/img/logo.png").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/png");
}

#[tokio::test]
async fn an_addon_serves_only_its_own_static_directory() {
    let fixture = Fixture::new("confined");
    // a file of the addon that is not under static/
    let (status, _, _) = get(app(&fixture), "/demo/static/../secret.txt").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // traversal out of the addon entirely
    let (status, _, _) = get(app(&fixture), "/demo/static/../../../../etc/passwd").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // an addon that is not installed
    let (status, _, _) = get(app(&fixture), "/nowhere/static/src/a.js").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // a directory is not a file
    let (status, _, _) = get(app(&fixture), "/demo/static/src").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_rpc_routes_still_answer_alongside_the_asset_routes() {
    let fixture = Fixture::new("coexist");
    let response = app(&fixture)
        .oneshot(
            Request::post("/web/dataset/call_kw")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "call",
                                       "params": {"model": "nope", "method": "search"}})
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
