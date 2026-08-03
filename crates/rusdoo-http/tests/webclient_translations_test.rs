//! `/web/webclient/translations` — the vocabulary the browser needs.
//!
//! Port of `web/controllers/webclient.py::translations`. The client's
//! localization service fetches this before it renders anything: a 404
//! here is a client that never mounts.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rusdoo_http::assets::AssetHub;
use rusdoo_http::dispatch::OrmService;
use rusdoo_http::routes::router;
use rusdoo_modules::assets::Bundles;
use rusdoo_orm::registry::Registry;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

/// An addon shipping one `.po` with a text for the client and a text for
/// the server — Odoo tells them apart by the `#. odoo-javascript` line.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let root = std::env::temp_dir().join(format!(
            "rusdoo-http-i18n-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("demo/i18n")).expect("fixture root");
        std::fs::write(
            root.join("demo/i18n/pt_BR.po"),
            r#"
msgid ""
msgstr "Content-Type: text/plain; charset=UTF-8\n"

#. module: demo
#. odoo-javascript
#: code:addons/demo/static/src/thing.js:0
msgid "Discard"
msgstr "Descartar"

#. module: demo
#. odoo-python
#: code:addons/demo/models/thing.py:0
msgid "Server side only"
msgstr "Só no servidor"

#. module: demo
#. odoo-javascript
#: code:addons/demo/static/src/thing.xml:0
msgid "Untranslated"
msgstr ""
"#,
        )
        .expect("write po");
        Fixture { root }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn app(fixture: &Fixture) -> axum::Router {
    let roots: HashMap<String, PathBuf> = [("demo".to_string(), fixture.root.join("demo"))]
        .into_iter()
        .collect();
    let url = std::env::var("RUSDOO_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:rusdoo@localhost:55432/postgres".into());
    let service = OrmService::insecure(
        Arc::new(Registry::new()),
        rusdoo_orm::db::lazy_pool(&url).unwrap(),
    )
    .with_assets(AssetHub::new(Bundles::default(), roots));
    router(service)
}

async fn get(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

#[tokio::test]
async fn the_client_gets_the_javascript_half_of_a_catalogue() {
    let fixture = Fixture::new("half");
    let (status, body) = get(app(&fixture), "/web/webclient/translations?lang=pt_BR").await;
    assert_eq!(status, StatusCode::OK);

    let messages = &body["modules"]["demo"]["messages"];
    let pairs: Vec<(&str, &str)> = messages
        .as_array()
        .unwrap_or_else(|| panic!("no messages: {body}"))
        .iter()
        .map(|m| (m["id"].as_str().unwrap(), m["string"].as_str().unwrap()))
        .collect();
    assert_eq!(pairs, vec![("Discard", "Descartar")], "{body}");
    // an untranslated text is not a translation: sending it empty would
    // draw a blank label where the original belongs
    assert!(!body.to_string().contains("Untranslated"), "{body}");
}

#[tokio::test]
async fn the_localization_the_client_formats_dates_with_is_there() {
    // `localization_service` reads these off the answer without checking:
    // a missing field is a TypeError before the first render
    let fixture = Fixture::new("locale");
    let (_, body) = get(app(&fixture), "/web/webclient/translations?lang=pt_BR").await;

    let params = &body["lang_parameters"];
    for key in [
        "date_format",
        "time_format",
        "decimal_point",
        "thousands_sep",
        "direction",
        "grouping",
        "week_start",
    ] {
        assert!(!params[key].is_null(), "{key} missing: {body}");
    }
    // the client does JSON.parse on it, so it travels as the text of a list
    assert!(params["grouping"].as_str().unwrap().starts_with('['), "{body}");
    assert!(params["week_start"].is_number(), "{body}");
}

#[tokio::test]
async fn an_unchanged_catalogue_is_answered_without_its_messages() {
    // Odoo answers `{lang, hash}` alone when the client's hash matches,
    // which is what keeps a page load from carrying the vocabulary twice
    let fixture = Fixture::new("hash");
    let (_, first) = get(app(&fixture), "/web/webclient/translations?lang=pt_BR").await;
    let hash = first["hash"].as_str().expect("a hash").to_string();
    assert!(!hash.is_empty());

    let (status, again) = get(
        app(&fixture),
        &format!("/web/webclient/translations?lang=pt_BR&hash={hash}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["hash"].as_str(), Some(hash.as_str()));
    assert!(again["modules"].is_null(), "{again}");
}

#[tokio::test]
async fn the_source_language_has_a_catalogue_too() {
    // en_US ships no `.po` — the texts are already English. The answer
    // still has to be a valid one, or the client stops on the language
    // it needs least.
    let fixture = Fixture::new("source");
    let (status, body) = get(app(&fixture), "/web/webclient/translations?lang=en_US").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["modules"]["demo"]["messages"].is_array(), "{body}");
    assert!(!body["lang_parameters"]["date_format"].is_null(), "{body}");
}
