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

/// The `web` addon as Odoo ships it: a contribution bundle and the
/// `web.assets_web` bundle that includes it and adds the entry points.
fn web_addon_app(fixture: &Fixture) -> axum::Router {
    let manifest_source = r#"{
        'name': 'Web',
        'assets': {
            'web.assets_backend': ['web/static/src/module_loader.js'],
            'web.assets_web': [
                ('include', 'web.assets_backend'),
                'web/static/src/start.js',
            ],
        },
    }"#;
    let addon = fixture.root.join("web");
    fixture.write("web/__manifest__.py", manifest_source);
    fixture.write("web/static/src/module_loader.js", "// @odoo-module ignore");
    fixture.write("web/static/src/start.js", "// @odoo-module ignore");

    let mut manifest = parse_manifest(manifest_source, "web").expect("manifest");
    manifest.path = addon.clone();
    let bundles = resolve_bundles(&[&manifest]).expect("bundles resolve");
    let roots: HashMap<String, PathBuf> = [("web".to_string(), addon)].into_iter().collect();

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
async fn a_changed_file_is_served_instead_of_the_cached_bundle() {
    let fixture = Fixture::new("stale");
    // one router, so the same asset cache answers both requests
    let app = app(&fixture);
    let (_, _, first) = get(app.clone(), "/web/assets/web.assets_backend.js").await;
    assert!(first.contains("const a = 1;"));

    // a deploy replaces a file without restarting the server
    fixture.write("demo/static/src/a.js", "const a = 99;");
    let (_, _, second) = get(app, "/web/assets/web.assets_backend.js").await;
    assert!(
        second.contains("const a = 99;") && !second.contains("const a = 1;"),
        "yesterday's client is still being served: {second}"
    );
}

/// A bundle carries files that define and files that use: variables and
/// mixins compile to nothing of their own. Serving their source instead
/// is what broke the real client — the browser meets `@mixin` and `//`,
/// which are not CSS, and drops the stylesheet from there on.
#[tokio::test]
async fn a_scss_that_only_defines_contributes_no_source() {
    let fixture = Fixture::new("defs");
    let manifest_source = r#"{
        'name': 'Defs',
        'assets': {'web.assets_backend': [
            'defs/static/src/_mixins.scss',
            'defs/static/src/app.scss',
        ]},
    }"#;
    let addon = fixture.root.join("defs");
    fixture.write("defs/__manifest__.py", manifest_source);
    fixture.write(
        "defs/static/src/_mixins.scss",
        "// only definitions, no rules\n$brand: #336699;\n@mixin padded { padding: 4px; }\n",
    );
    fixture.write(
        "defs/static/src/app.scss",
        "@import \"mixins\";\n.o_form { color: $brand; @include padded; }\n",
    );
    let mut manifest = parse_manifest(manifest_source, "defs").expect("manifest");
    manifest.path = addon.clone();
    let bundles = resolve_bundles(&[&manifest]).expect("bundles resolve");
    let roots: HashMap<String, PathBuf> = [("defs".to_string(), addon)].into_iter().collect();
    let url = std::env::var("RUSDOO_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:rusdoo@localhost:55432/postgres".into());
    let service = OrmService::insecure(
        Arc::new(Registry::new()),
        rusdoo_orm::db::lazy_pool(&url).unwrap(),
    )
    .with_assets(AssetHub::new(bundles, roots));

    let (status, _, css) = get(router(service), "/web/assets/web.assets_backend.css").await;
    assert_eq!(status, StatusCode::OK);

    // what it uses is there, compiled
    assert!(css.contains("color: #336699"), "{css}");
    assert!(css.contains("padding: 4px"), "{css}");
    // and no line of Sass reached the browser
    for sass_only in ["@mixin", "$brand", "//", "@import"] {
        assert!(
            !css.contains(sass_only),
            "{sass_only:?} was served as CSS:\n{css}"
        );
    }
}

#[tokio::test]
async fn web_serves_the_client_when_an_addon_ships_one() {
    let fixture = Fixture::new("shell");
    let (status, headers, body) = get(app(&fixture), "/web").await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers["content-type"].starts_with("text/html"));
    assert!(body.contains("/web/assets/web.assets_backend.js"), "{body}");
    assert!(body.contains("/web/assets/web.assets_backend.css"), "{body}");
    assert!(body.contains("id=\"rusdoo-app\""), "{body}");
    // the shell carries no record data: it is safe before authentication
    assert!(!body.contains("secret"), "{body}");
}

#[tokio::test]
async fn the_shell_declares_the_odoo_global_before_the_bundle() {
    // Odoo's `web.layout` opens the head with an inline script creating
    // `var odoo`, then pours the bundles into it. Every transpiled module
    // calls `odoo.define`, so a bundle reached first is a ReferenceError.
    let fixture = Fixture::new("global");
    let (_, _, body) = get(app(&fixture), "/web").await;
    let global = body
        .find("var odoo")
        .unwrap_or_else(|| panic!("no odoo global: {body}"));
    let bundle = body
        .find("/web/assets/web.assets_backend.js")
        .expect("no bundle tag");
    assert!(global < bundle, "the bundle runs before `odoo` exists: {body}");
    assert!(body.contains("csrf_token"), "{body}");
}

#[tokio::test]
async fn the_shell_carries_the_session_the_client_boots_from() {
    // `session.js` reads `odoo.__session_info__` at module level and
    // `menu_service` awaits `odoo.loadMenusPromise`: both are set by the
    // layout, not fetched by the client.
    let fixture = Fixture::new("boot");
    let (_, _, body) = get(app(&fixture), "/web").await;
    assert!(body.contains("odoo.__session_info__ ="), "{body}");
    assert!(body.contains("\"uid\":null"), "anonymous session: {body}");
    assert!(body.contains("odoo.loadMenusPromise"), "{body}");
    assert!(
        body.contains("/web/webclient/load_menus"),
        "the menus are never fetched: {body}"
    );
}

#[tokio::test]
async fn the_client_bundle_is_assets_web_when_an_addon_ships_it() {
    // `web.webclient_bootstrap` calls `web.assets_web`, which includes
    // `web.assets_backend` plus the client's entry points. Serving the
    // contribution bundle leaves out `main.js`/`start.js`.
    let fixture = Fixture::new("assets-web");
    let (_, _, body) = get(web_addon_app(&fixture), "/web").await;
    assert!(body.contains("/web/assets/web.assets_web.js"), "{body}");
    assert!(
        !body.contains("/web/assets/web.assets_backend.js"),
        "both bundles served, so every module loads twice: {body}"
    );
}

#[tokio::test]
async fn without_a_client_bundle_web_keeps_the_server_rendered_index() {
    let url = std::env::var("RUSDOO_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:rusdoo@localhost:55432/postgres".into());
    let service = OrmService::insecure(
        Arc::new(Registry::new()),
        rusdoo_orm::db::lazy_pool(&url).unwrap(),
    );
    let (_, _, body) = get(router(service), "/web").await;
    assert!(!body.contains("rusdoo-app"), "{body}");
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

/// An addon shipping SCSS: nesting, a variable, and an `@import` of a
/// partial next to it — the three things every Odoo stylesheet uses.
fn scss_app(fixture: &Fixture) -> axum::Router {
    let manifest_source = r#"{
        'name': 'Sassy',
        'assets': {'web.assets_backend': [
            'sassy/static/src/theme.scss',
            'sassy/static/src/plain.css',
        ]},
    }"#;
    let addon = fixture.root.join("sassy");
    fixture.write("sassy/__manifest__.py", manifest_source);
    fixture.write(
        "sassy/static/src/_variables.scss",
        "$brand: #336699;\n",
    );
    fixture.write(
        "sassy/static/src/theme.scss",
        r#"@import "variables";

.o_form {
    color: $brand;
    .o_field { padding: 4px; }
}
"#,
    );
    fixture.write("sassy/static/src/plain.css", ".plain { margin: 0; }");

    let mut manifest = parse_manifest(manifest_source, "sassy").expect("manifest");
    manifest.path = addon.clone();
    let bundles = resolve_bundles(&[&manifest]).expect("bundles resolve");
    let roots: HashMap<String, PathBuf> = [("sassy".to_string(), addon)].into_iter().collect();

    let url = std::env::var("RUSDOO_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:rusdoo@localhost:55432/postgres".into());
    let service = OrmService::insecure(
        Arc::new(Registry::new()),
        rusdoo_orm::db::lazy_pool(&url).unwrap(),
    )
    .with_assets(AssetHub::new(bundles, roots));
    router(service)
}

/// A `.scss` in a bundle is compiled, not handed to the browser as it
/// was written.
///
/// The bundle is served as `text/css`, so shipping Sass syntax inside it
/// is not "unsupported" — it is a stylesheet the browser drops on the
/// floor, silently, and a client that looks unstyled for no visible
/// reason. Every Odoo addon ships `.scss`: the `web` addon alone has 190
/// of them.
#[tokio::test]
async fn scss_in_a_bundle_is_compiled_to_css() {
    let fixture = Fixture::new("scss");
    let (status, headers, body) = get(scss_app(&fixture), "/web/assets/web.assets_backend.css").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/css; charset=utf-8");

    // the variable resolved and the nesting was flattened
    assert!(
        body.contains("#336699"),
        "the @import'd variable resolved: {body}"
    );
    assert!(
        body.contains(".o_form .o_field"),
        "the nesting was flattened: {body}"
    );
    // and nothing Sass-shaped survived into what the browser is told is CSS
    assert!(!body.contains("$brand"), "a variable reached the client: {body}");
    assert!(!body.contains("@import"), "an @import reached the client: {body}");

    // a plain .css file still goes through untouched, and after the scss:
    // declaration order is cascade order
    let compiled = body.find(".o_form").expect("the scss is in the bundle");
    let plain = body.find(".plain").expect("the css is in the bundle");
    assert!(compiled < plain, "declaration order held: {body}");
}

/// A stylesheet that does not compile fails the bundle instead of
/// serving the half of it that did.
#[tokio::test]
async fn broken_scss_does_not_serve_half_a_stylesheet() {
    let fixture = Fixture::new("scss-broken");
    let app = scss_app(&fixture);
    fixture.write(
        "sassy/static/src/theme.scss",
        ".o_form { color: $never_declared; }",
    );
    let (status, _, _) = get(app, "/web/assets/web.assets_backend.css").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a bundle that cannot be built is not served truncated"
    );
}

/// A file that says it is an ES6 module comes out as one; a file that
/// says it is not comes out untouched.
///
/// The second half is the one worth a test. Odoo's rule is that
/// everything under `static/src` is a module, and this port's own client
/// lives there while being plain IIFEs that install themselves on
/// `window` at load. Wrapped in an `odoo.define`, their bodies would run
/// only when something required them — and nothing does, so the client
/// would go blank with no error anywhere.
#[tokio::test]
async fn a_bundle_transpiles_modules_and_leaves_the_rest_alone() {
    let fixture = Fixture::new("transpile");
    let manifest_source = r#"{
        'name': 'Mixed',
        'assets': {'web.assets_backend': [
            'mixed/static/src/legacy.js',
            'mixed/static/src/modern.js',
        ]},
    }"#;
    let addon = fixture.root.join("mixed");
    fixture.write("mixed/__manifest__.py", manifest_source);
    fixture.write(
        "mixed/static/src/legacy.js",
        "/** @odoo-module ignore **/\n(function (app) { app.ready = true; })(window.app = {});",
    );
    fixture.write(
        "mixed/static/src/modern.js",
        "import { Component } from \"@odoo/owl\";\nexport class Thing extends Component {}\n",
    );

    let mut manifest = parse_manifest(manifest_source, "mixed").expect("manifest");
    manifest.path = addon.clone();
    let bundles = resolve_bundles(&[&manifest]).expect("bundles resolve");
    let roots: HashMap<String, PathBuf> = [("mixed".to_string(), addon)].into_iter().collect();
    let url = std::env::var("RUSDOO_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:rusdoo@localhost:55432/postgres".into());
    let service = OrmService::insecure(
        Arc::new(Registry::new()),
        rusdoo_orm::db::lazy_pool(&url).unwrap(),
    )
    .with_assets(AssetHub::new(bundles, roots));

    let (status, _, body) = get(router(service), "/web/assets/web.assets_backend.js").await;
    assert_eq!(status, StatusCode::OK);

    // the module was wrapped, its import became a require, and its export
    // became a slot on __exports
    assert!(
        body.contains("odoo.define('@mixed/modern', ['@odoo/owl']"),
        "the module was defined with what it depends on: {body}"
    );
    assert!(
        body.contains("const { Component } = require(\"@odoo/owl\")"),
        "the import became a require: {body}"
    );
    assert!(
        body.contains("const Thing = __exports.Thing = class Thing"),
        "the export became a slot: {body}"
    );

    // and the one that opted out is byte for byte what was on disk —
    // still an IIFE, still running the moment the bundle loads
    assert!(
        body.contains("(function (app) { app.ready = true; })(window.app = {});"),
        "the IIFE survived: {body}"
    );
    assert!(
        !body.contains("odoo.define('@mixed/legacy'"),
        "and was not wrapped: {body}"
    );
}

/// A `@include` whose semicolon somebody forgot still compiles.
///
/// Not indulgence: libsass accepts it, Odoo is built on libsass, and
/// eight lines of Odoo's own stylesheets are written that way. A port
/// that refused them would be correct and useless — the same trade the
/// JS transpiler makes by copying Odoo's regexes rather than improving
/// on them.
#[tokio::test]
async fn a_missing_semicolon_after_an_include_does_not_fail_the_bundle() {
    let fixture = Fixture::new("scss-lenient");
    let manifest_source = r#"{
        'name': 'Lenient',
        'assets': {'web.assets_backend': ['lenient/static/src/theme.scss']},
    }"#;
    let addon = fixture.root.join("lenient");
    fixture.write("lenient/__manifest__.py", manifest_source);
    fixture.write(
        "lenient/static/src/theme.scss",
        // the first @include is missing its `;`, exactly as Odoo's
        // `iframe_wrapper_field.scss` is; the second takes a block and
        // must not have one inserted
        r#"@mixin tint($c) { color: $c; }
@mixin framed { border: 1px solid; @content; }

.o_a {
    @include tint(#336699)
}

.o_b {
    @include framed
    {
        padding: 2px;
    }
}
"#,
    );

    let mut manifest = parse_manifest(manifest_source, "lenient").expect("manifest");
    manifest.path = addon.clone();
    let bundles = resolve_bundles(&[&manifest]).expect("bundles resolve");
    let roots: HashMap<String, PathBuf> = [("lenient".to_string(), addon)].into_iter().collect();
    let url = std::env::var("RUSDOO_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:rusdoo@localhost:55432/postgres".into());
    let service = OrmService::insecure(
        Arc::new(Registry::new()),
        rusdoo_orm::db::lazy_pool(&url).unwrap(),
    )
    .with_assets(AssetHub::new(bundles, roots));

    let (status, _, body) = get(router(service), "/web/assets/web.assets_backend.css").await;
    assert_eq!(status, StatusCode::OK, "the bundle compiled: {body}");
    assert!(body.contains("#336699"), "the mixin ran: {body}");
    assert!(
        body.contains("padding: 2px"),
        "and the block form still took its block: {body}"
    );
}
