//! Saved searches: the smallest piece of an ERP that belongs to a
//! person, and the rule about who else sees it.

use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_testing::TransactionCase;
use serde_json::{json, Value};

const MODULES: [&str; 1] = ["base"];

#[tokio::test]
async fn a_saved_search_survives_the_session_live() {
    let Some(case) = TransactionCase::open("ir_filters", &MODULES).await else {
        return;
    };
    let ana = case
        .models()
        .create(
            &case.pool(),
            "res.users",
            vec![("login", json!("ana")), ("name", json!("Ana"))],
        )
        .await
        .expect("the user saves");

    let mine = case
        .models()
        .create(
            &case.pool(),
            "ir.filters",
            vec![
                ("name", json!("Meus contatos do Recife")),
                ("model_id", json!("res.partner")),
                ("domain", json!(r#"[["city","=","Recife"]]"#)),
                ("context", json!(r#"{"group_by":["country_id"]}"#)),
                ("sort", json!(r#"["name asc"]"#)),
                ("user_ids", json!([[6, 0, [ana]]])),
                ("is_default", json!(true)),
            ],
        )
        .await
        .expect("the filter saves");

    let rows = case
        .models()
        .read(
            &case.pool(),
            "ir.filters",
            &[mine],
            &["name", "model_id", "domain", "context", "sort", "user_ids", "is_default"],
        )
        .await
        .expect("the filter reads");
    let row = &rows[0];
    assert_eq!(row["model_id"], json!("res.partner"), "{row:?}");
    assert_eq!(row["is_default"], json!(true), "{row:?}");
    assert_eq!(
        row["user_ids"].as_array().map(Vec::len),
        Some(1),
        "pertence a quem salvou: {row:?}"
    );

    // and the domain comes back as the client wrote it, ready to be a
    // search again
    let domain: Value =
        serde_json::from_str(row["domain"].as_str().expect("text")).expect("JSON");
    let parsed = parse_domain(&domain).expect("it is still a domain");
    let found = case
        .models()
        .search(&case.pool(), "res.partner", &parsed, &SearchOptions::default())
        .await;
    assert!(found.is_ok(), "o domínio salvo não roda: {found:?}");

    // a filter with no users is everybody's — Odoo's rule, and the
    // surprising case is the empty one
    let shared = case
        .models()
        .create(
            &case.pool(),
            "ir.filters",
            vec![
                ("name", json!("Todos os fornecedores")),
                ("model_id", json!("res.partner")),
                ("domain", json!("[]")),
            ],
        )
        .await
        .expect("the shared filter saves");
    let rows = case
        .models()
        .read(&case.pool(), "ir.filters", &[shared], &["user_ids", "sort"])
        .await
        .unwrap();
    assert_eq!(
        rows[0]["user_ids"].as_array().map(Vec::len),
        Some(0),
        "{:?}",
        rows[0]
    );
    assert_eq!(rows[0]["sort"], json!("[]"), "o padrão é uma ordem vazia");

    case.close().await;
}

#[tokio::test]
async fn a_filter_that_is_not_json_is_refused_live() {
    let Some(case) = TransactionCase::open("ir_filters_json", &MODULES).await else {
        return;
    };
    // text nobody parsed is how a saved search becomes a screen that will
    // not open, months later, for one person
    let refused = case
        .models()
        .create(
            &case.pool(),
            "ir.filters",
            vec![
                ("name", json!("Quebrado")),
                ("model_id", json!("res.partner")),
                ("domain", json!("[['city','=','Recife'")),
            ],
        )
        .await;
    assert!(refused.is_err(), "domínio inválido aceito");

    case.close().await;
}
