//! Files next to a record: what an upload stores, what a download
//! serves, and what neither is allowed to do.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rusdoo_http::dispatch::OrmService;
use rusdoo_http::routes::router;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

fn pool(url: &str, schema: &'static str) -> sqlx::PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |conn, _meta| {
            Box::pin(async move {
                sqlx::Executor::execute(
                    &mut *conn,
                    &format!("CREATE SCHEMA IF NOT EXISTS {schema}; SET search_path TO {schema}")
                        as &str,
                )
                .await?;
                Ok(())
            })
        })
        .connect_lazy(url)
        .unwrap()
}

/// A filestore of this test's own — the service carries the path, so
/// tests running side by side never write over each other's files.
fn filestore(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rusdoo-filestore-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("filestore");
    dir
}

async fn fixture(url: &str, schema: &'static str, store: &std::path::Path) -> (OrmService, i64) {
    let pool = pool(url, schema);
    let registry = rusdoo_base::registry().unwrap();
    for table in [
        "ir_attachment",
        "res_partner",
        "res_company",
        "res_country",
        "ir_sequence",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}" CASCADE"#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for model in [
        "ir.sequence",
        "res.country",
        "res.company",
        "res.partner",
        "ir.attachment",
    ] {
        registry.get(model).unwrap().init_table(&pool).await.unwrap();
    }
    let partner = registry
        .create(&pool, "res.partner", vec![("name", json!("Ana"))])
        .await
        .unwrap();
    (
        OrmService::insecure(Arc::new(registry), pool).with_filestore(store),
        partner,
    )
}

/// A multipart body, hand-rolled: the test speaks the wire format a
/// browser would send.
fn multipart(model: &str, res_id: i64, file_name: &str, content: &[u8]) -> (String, Vec<u8>) {
    let boundary = "----rusdooTESTBOUNDARY";
    let mut body = Vec::new();
    for (name, value) in [("model", model.to_string()), ("id", res_id.to_string())] {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"ufile\"; \
             filename=\"{file_name}\"\r\nContent-Type: text/plain\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(content);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (
        format!("multipart/form-data; boundary={boundary}"),
        body,
    )
}

async fn upload(app: axum::Router, model: &str, res_id: i64, name: &str, bytes: &[u8]) -> Value {
    let (content_type, body) = multipart(model, res_id, name, bytes);
    let response = app
        .oneshot(
            Request::post("/web/binary/upload_attachment")
                .header("content-type", content_type)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn a_file_is_stored_and_served_back_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let store = filestore("roundtrip");
    let (service, partner) = fixture(&url, rusdoo_testing::schema_for("rusdoo_attachment_test"), &store).await;

    let answer = upload(
        router(service.clone()),
        "res.partner",
        partner,
        "contrato.txt",
        b"assinado em duas vias",
    )
    .await;
    let id = answer["attachments"][0]["id"]
        .as_i64()
        .expect("uploaded: {answer}");
    assert_eq!(answer["attachments"][0]["name"], "contrato.txt");

    // the bytes went to the filestore, not into the row
    assert!(store.join(id.to_string()).is_file(), "{store:?}");

    let response = router(service.clone())
        .oneshot(
            Request::get(format!("/web/content/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let disposition = response
        .headers()
        .get("content-disposition")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    // always an attachment: an uploaded file is not something this
    // origin should render in a tab
    assert!(disposition.starts_with("attachment;"), "{disposition}");
    assert!(disposition.contains("contrato.txt"), "{disposition}");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"assinado em duas vias");

    // and the row says what it is, next to the record it hangs from
    let rows = rusdoo_http::routes::router(service.clone())
        .oneshot(
            Request::post("/web/dataset/call_kw")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"jsonrpc": "2.0", "id": 1, "method": "call", "params": {
                        "model": "ir.attachment", "method": "search_read",
                        "args": [[["res_id", "=", partner]]],
                        "kwargs": {"fields": ["name", "res_model", "file_size", "mimetype"]}}})
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = rows.into_body().collect().await.unwrap().to_bytes();
    let answer: Value = serde_json::from_slice(&bytes).unwrap();
    let row = &answer["result"][0];
    assert_eq!(row["res_model"], "res.partner");
    assert_eq!(row["file_size"], json!(21));
    assert_eq!(row["mimetype"], "text/plain");
}

#[tokio::test]
async fn a_file_name_never_becomes_a_path_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let store = filestore("traversal");
    let (service, partner) = fixture(&url, rusdoo_testing::schema_for("rusdoo_attachment_path_test"), &store).await;
    let answer = upload(
        router(service),
        "res.partner",
        partner,
        "../../etc/passwd",
        b"nao sou o passwd",
    )
    .await;
    let id = answer["attachments"][0]["id"].as_i64().expect("uploaded");
    // the stored name is the id, and the label lost its directories
    assert_eq!(answer["attachments"][0]["name"], "passwd");
    // the bytes are under the id, and nothing in the filestore carries
    // a name the client chose
    assert!(store.join(id.to_string()).is_file(), "{store:?}");
    let entries: Vec<String> = std::fs::read_dir(&store)
        .unwrap()
        .filter_map(|entry| Some(entry.ok()?.file_name().to_string_lossy().into_owned()))
        .collect();
    assert!(
        entries.iter().all(|name| name.parse::<i64>().is_ok()),
        "the filestore names files by id: {entries:?}"
    );
}

#[tokio::test]
async fn an_upload_without_a_record_is_refused_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let store = filestore("norecord");
    let (service, partner) = fixture(&url, rusdoo_testing::schema_for("rusdoo_attachment_norecord_test"), &store).await;

    // a model nobody registered
    let answer = upload(
        router(service.clone()),
        "nao.existe",
        partner,
        "x.txt",
        b"conteudo",
    )
    .await;
    assert!(answer.get("attachments").is_none(), "{answer}");

    // and an empty file is not a file
    let answer = upload(
        router(service),
        "res.partner",
        partner,
        "vazio.txt",
        b"",
    )
    .await;
    assert!(answer.get("attachments").is_none(), "{answer}");
}

#[tokio::test]
async fn a_missing_attachment_is_a_404_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let store = filestore("missing");
    let (service, _partner) = fixture(&url, rusdoo_testing::schema_for("rusdoo_attachment_missing_test"), &store).await;
    let response = router(service)
        .oneshot(Request::get("/web/content/9999").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
