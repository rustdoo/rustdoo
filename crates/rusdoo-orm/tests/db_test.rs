//! Live-database tests for the execution layer. They require PostgreSQL;
//! without RUSDOO_TEST_DATABASE_URL set they skip (with a note on stderr).

use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::db::connect;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use serde_json::json;
use sqlx::PgPool;

fn test_model() -> Model {
    Model::new(
        ModelMeta {
            name: "rusdoo.test.partner".into(),
            table: "rusdoo_test_partner".into(),
            inherit: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }).required(),
            Field::new("active", FieldType::Boolean),
            Field::new("color", FieldType::Integer),
        ],
    )
}

async fn test_pool() -> Option<PgPool> {
    let url = std::env::var("RUSDOO_TEST_DATABASE_URL").ok()?;
    Some(
        connect(&url)
            .await
            .expect("failed to connect to test database"),
    )
}

#[tokio::test]
async fn full_crud_roundtrip_against_postgres() {
    // Arrange: live database + fresh table
    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let model = test_model();
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_partner""#)
        .execute(&pool)
        .await
        .unwrap();
    model.init_table(&pool).await.unwrap();

    // Act + Assert: create returns sequential ids
    let id = model
        .create(
            &pool,
            vec![
                ("name", json!("Gemini")),
                ("color", json!(7)),
                ("active", json!(true)),
            ],
        )
        .await
        .unwrap();
    assert!(id >= 1);
    let id2 = model
        .create(
            &pool,
            vec![("name", json!("Zed")), ("active", json!(false))],
        )
        .await
        .unwrap();

    // search: comparison domain
    let dom = parse_domain(&json!([["color", ">", 3]])).unwrap();
    let found = model
        .search(&pool, &dom, &SearchOptions::default())
        .await
        .unwrap();
    assert_eq!(found, vec![id]);

    // search: falsy semantics live — active = false matches false AND unset
    let dom = parse_domain(&json!([["active", "=", false]])).unwrap();
    let found = model
        .search(&pool, &dom, &SearchOptions::default())
        .await
        .unwrap();
    assert_eq!(found, vec![id2]);

    // read
    let rows = model
        .read(&pool, &[id], &["name", "color", "active"])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], json!("Gemini"));
    assert_eq!(rows[0]["color"], json!(7));
    assert_eq!(rows[0]["active"], json!(true));

    // read: unset column comes back as JSON null, not an error
    let rows = model.read(&pool, &[id2], &["color"]).await.unwrap();
    assert_eq!(rows[0]["color"], serde_json::Value::Null);

    // write
    let updated = model
        .write(&pool, &[id], vec![("color", json!(9))])
        .await
        .unwrap();
    assert_eq!(updated, 1);
    let rows = model.read(&pool, &[id], &["color"]).await.unwrap();
    assert_eq!(rows[0]["color"], json!(9));

    // unlink
    let deleted = model.unlink(&pool, &[id, id2]).await.unwrap();
    assert_eq!(deleted, 2);
    let dom = parse_domain(&json!([])).unwrap();
    let remaining = model
        .search(&pool, &dom, &SearchOptions::default())
        .await
        .unwrap();
    assert!(remaining.is_empty());
}
