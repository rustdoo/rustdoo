//! Document numbers: drawn inside the create's own transaction, never
//! twice, and never over a number the caller brought.

use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::json;
use sqlx::PgPool;
use std::collections::HashSet;
use std::sync::Arc;

/// A registry with `ir.sequence` and a document numbered from it.
fn registry(suffix: &str) -> Registry {
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "ir.sequence".into(),
            table: format!("rusdoo_test_seq_{suffix}"),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("code", FieldType::Char { size: None }),
            Field::new("prefix", FieldType::Char { size: None }),
            Field::new("suffix", FieldType::Char { size: None }),
            Field::new("padding", FieldType::Integer).default_value(json!(0)),
            Field::new("number_next", FieldType::Integer).default_value(json!(1)),
            Field::new("number_increment", FieldType::Integer).default_value(json!(1)),
        ],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.doc".into(),
            table: format!("rusdoo_test_doc_{suffix}"),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }).from_sequence("test.doc"),
            Field::new("note", FieldType::Char { size: None }),
        ],
    ))
    .unwrap();
    reg
}

async fn fixture(suffix: &str) -> Option<(Registry, PgPool)> {
    let pool = rusdoo_testing::pool_in("rusdoo_sequence_test_fixture").expect("test database");
    let reg = registry(suffix);
    for table in [
        format!("rusdoo_test_doc_{suffix}"),
        format!("rusdoo_test_seq_{suffix}"),
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}" CASCADE"#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for model in ["ir.sequence", "rusdoo.test.doc"] {
        reg.get(model).unwrap().init_table(&pool).await.unwrap();
    }
    reg.create(
        &pool,
        "ir.sequence",
        vec![
            ("name", json!("Documento de teste")),
            ("code", json!("test.doc")),
            ("prefix", json!("DOC/")),
            ("padding", json!(4)),
            ("number_next", json!(1)),
        ],
    )
    .await
    .unwrap();
    Some((reg, pool))
}

async fn name_of(reg: &Registry, pool: &PgPool, id: i64) -> String {
    reg.read(pool, "rusdoo.test.doc", &[id], &["name"])
        .await
        .unwrap()[0]["name"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn a_document_is_numbered_by_its_sequence_live() {
    let Some((reg, pool)) = fixture("basic").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let mut names = Vec::new();
    for _ in 0..3 {
        let id = reg
            .create(&pool, "rusdoo.test.doc", vec![("note", json!("x"))])
            .await
            .unwrap();
        names.push(name_of(&reg, &pool, id).await);
    }
    assert_eq!(names, vec!["DOC/0001", "DOC/0002", "DOC/0003"]);

    // the sequence moved with them, and nothing else did
    let rows = reg
        .read(&pool, "ir.sequence", &[1], &["number_next"])
        .await
        .unwrap();
    assert_eq!(rows[0]["number_next"], json!(4));
}

#[tokio::test]
async fn a_number_the_caller_brought_is_kept_live() {
    let Some((reg, pool)) = fixture("given").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    // importing documents that already have a number must not renumber
    // them — and must not spend a number either
    let id = reg
        .create(
            &pool,
            "rusdoo.test.doc",
            vec![("name", json!("LEGADO-42"))],
        )
        .await
        .unwrap();
    assert_eq!(name_of(&reg, &pool, id).await, "LEGADO-42");
    let rows = reg
        .read(&pool, "ir.sequence", &[1], &["number_next"])
        .await
        .unwrap();
    assert_eq!(rows[0]["number_next"], json!(1), "no number was drawn");

    // an empty value is not a number: it draws one
    let id = reg
        .create(&pool, "rusdoo.test.doc", vec![("name", json!(false))])
        .await
        .unwrap();
    assert_eq!(name_of(&reg, &pool, id).await, "DOC/0001");
}

#[tokio::test]
async fn two_creates_at_once_never_share_a_number_live() {
    let Some((reg, pool)) = fixture("race").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    // the point of drawing inside the transaction: eight documents
    // created at the same moment are eight different documents
    let reg = Arc::new(reg);
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let reg = Arc::clone(&reg);
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            reg.create(&pool, "rusdoo.test.doc", vec![("note", json!("x"))])
                .await
                .expect("created")
        }));
    }
    let mut ids = Vec::new();
    for task in tasks {
        ids.push(task.await.expect("joined"));
    }

    let rows = reg
        .read(&pool, "rusdoo.test.doc", &ids, &["name"])
        .await
        .unwrap();
    let names: HashSet<String> = rows
        .iter()
        .map(|row| row["name"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(names.len(), 8, "a number was handed out twice: {names:?}");
    assert!(names.contains("DOC/0001"), "{names:?}");
    assert!(names.contains("DOC/0008"), "{names:?}");
}

#[tokio::test]
async fn a_field_whose_sequence_is_missing_says_so_and_creates_live() {
    let Some((reg, pool)) = fixture("missing").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    // a database whose sequence row was never loaded must still create
    // records — the field simply arrives empty, and the log says why
    let domain = parse_domain(&json!([["code", "=", "test.doc"]])).unwrap();
    let ids = reg
        .search(&pool, "ir.sequence", &domain, &SearchOptions::default())
        .await
        .unwrap();
    reg.get("ir.sequence").unwrap().unlink(&pool, &ids).await.unwrap();

    let id = reg
        .create(&pool, "rusdoo.test.doc", vec![("note", json!("sem número"))])
        .await
        .expect("the record is still created");
    assert_eq!(name_of(&reg, &pool, id).await, "");
}

#[tokio::test]
async fn a_required_field_without_its_sequence_says_which_one_live() {
    let Some((reg, pool)) = fixture("required").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    // a document that must be numbered and has no sequence is a missing
    // configuration; the error names it instead of arriving as a
    // not-null violation from the database
    let mut strict = Registry::new();
    strict
        .register(
            reg.get("ir.sequence")
                .expect("the sequence model")
                .clone(),
        )
        .unwrap();
    strict
        .register(Model::new(
            ModelMeta {
                name: "rusdoo.test.strict".into(),
                table: "rusdoo_test_doc_required_strict".into(),
                inherit: vec![],
                inherits: vec![],
            },
            vec![Field::new("name", FieldType::Char { size: None })
                .required()
                .from_sequence("nao.existe")],
        ))
        .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_doc_required_strict""#)
        .execute(&pool)
        .await
        .unwrap();
    strict
        .get("rusdoo.test.strict")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();

    let error = strict
        .create(&pool, "rusdoo.test.strict", vec![])
        .await
        .expect_err("a numbered document with no sequence is refused");
    let message = error.to_string();
    assert!(message.contains("nao.existe"), "{message}");
    assert!(message.contains("sequence"), "{message}");
}
