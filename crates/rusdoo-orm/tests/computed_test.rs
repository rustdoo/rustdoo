//! Computed fields: a value derived from others by a Rust function with
//! declared dependencies (`odoo/orm/fields.py`'s compute + @api.depends).

use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use sqlx::PgPool;

/// total = quantity * unit price, the shape of half the computed fields
/// in a real Odoo model.
fn total(record: &Map<String, Value>) -> Value {
    let qty = record.get("qty").and_then(Value::as_i64).unwrap_or(0);
    let price = record.get("price").and_then(Value::as_i64).unwrap_or(0);
    Value::from(qty * price)
}

/// A compute over another computed field, to prove the chain resolves.
fn with_tax(record: &Map<String, Value>) -> Value {
    let total = record.get("total").and_then(Value::as_i64).unwrap_or(0);
    Value::from(total * 2)
}

/// A compute reading a related field, which reads through a many2one.
fn label(record: &Map<String, Value>) -> Value {
    let name = record.get("name").and_then(Value::as_str).unwrap_or("");
    let company = record
        .get("company_name")
        .and_then(Value::as_str)
        .unwrap_or("sem empresa");
    Value::from(format!("{name} ({company})"))
}

fn line_registry() -> Registry {
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "res.company".into(),
            table: "rusdoo_test_cmp_company".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.line".into(),
            table: "rusdoo_test_cmp_line".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("qty", FieldType::Integer),
            Field::new("price", FieldType::Integer),
            Field::new(
                "company_id",
                FieldType::Many2one {
                    comodel: "res.company".into(),
                },
            ),
            Field::new("company_name", FieldType::Char { size: None }).related("company_id.name"),
            Field::new("total", FieldType::Integer).computed(&["qty", "price"], total),
            Field::new("with_tax", FieldType::Integer).computed(&["total"], with_tax),
            Field::new("label", FieldType::Char { size: None })
                .computed(&["name", "company_name"], label),
        ],
    ))
    .unwrap();
    reg
}

#[test]
fn a_computed_field_is_not_stored_and_is_readonly() {
    let reg = line_registry();
    let field = reg.get("rusdoo.test.line").unwrap().field("total").unwrap();
    assert!(!field.stored, "it has no column of its own");
    assert!(
        field.readonly,
        "computing and writing are opposite directions"
    );
    let compute = field.compute.as_ref().unwrap();
    assert_eq!(
        compute.depends,
        vec!["qty".to_string(), "price".to_string()]
    );
}

#[test]
fn a_computed_field_cannot_be_searched_written_or_ordered_by() {
    let reg = line_registry();
    let line = reg.get("rusdoo.test.line").unwrap();

    // no column means no WHERE clause: say what is missing instead of
    // naming a column that does not exist
    let domain = parse_domain(&json!([["total", ">", 10]])).unwrap();
    let err = reg
        .search_sql("rusdoo.test.line", &domain, &SearchOptions::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains("computed"), "{err}");
    assert!(err.contains("cannot be searched"), "{err}");

    let err = line
        .insert_sql(1, vec![("total", json!(5))])
        .unwrap_err()
        .to_string();
    assert!(err.contains("not stored"), "{err}");

    let opts = SearchOptions {
        order: Some("total desc".into()),
        ..SearchOptions::default()
    };
    let err = line
        .search_sql(&parse_domain(&json!([])).unwrap(), &opts)
        .unwrap_err()
        .to_string();
    assert!(err.contains("not stored"), "{err}");
}

async fn test_pool() -> Option<PgPool> {
    let url = std::env::var("RUSDOO_TEST_DATABASE_URL").ok()?;
    Some(rusdoo_orm::db::connect(&url).await.expect("test database"))
}

#[tokio::test]
async fn computed_fields_resolve_on_read_live() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let reg = line_registry();
    for t in ["rusdoo_test_cmp_line", "rusdoo_test_cmp_company"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for m in ["res.company", "rusdoo.test.line"] {
        reg.get(m).unwrap().init_table(&pool).await.unwrap();
    }
    let acme = reg
        .create(&pool, "res.company", vec![("name", json!("Acme"))])
        .await
        .unwrap();
    let a = reg
        .create(
            &pool,
            "rusdoo.test.line",
            vec![
                ("name", json!("cadeira")),
                ("qty", json!(3)),
                ("price", json!(50)),
                ("company_id", json!(acme)),
            ],
        )
        .await
        .unwrap();
    let b = reg
        .create(
            &pool,
            "rusdoo.test.line",
            vec![
                ("name", json!("mesa")),
                ("qty", json!(2)),
                ("price", json!(70)),
            ],
        )
        .await
        .unwrap();

    let rows = reg
        .read(
            &pool,
            "rusdoo.test.line",
            &[a, b],
            &["name", "total", "with_tax", "label"],
        )
        .await
        .unwrap();
    let by_id: std::collections::HashMap<i64, _> = rows
        .into_iter()
        .map(|r| (r["id"].as_i64().unwrap(), r))
        .collect();
    assert_eq!(by_id[&a]["total"], json!(150));
    assert_eq!(by_id[&b]["total"], json!(140));
    // a compute over another computed field resolves through the chain
    assert_eq!(by_id[&a]["with_tax"], json!(300));
    // ...and one reading a related field follows the many2one
    assert_eq!(by_id[&a]["label"], json!("cadeira (Acme)"));
    assert_eq!(by_id[&b]["label"], json!("mesa (sem empresa)"));
    // the record's own columns still read alongside
    assert_eq!(by_id[&a]["name"], json!("cadeira"));

    // a compute follows its dependencies: rewriting one changes it
    reg.write(&pool, "rusdoo.test.line", &[a], vec![("qty", json!(10))])
        .await
        .unwrap();
    let rows = reg
        .read(&pool, "rusdoo.test.line", &[a], &["total"])
        .await
        .unwrap();
    assert_eq!(rows[0]["total"], json!(500));

    // reading only the computed field works: its dependencies are read
    // for it, not required of the caller
    let rows = reg
        .read(&pool, "rusdoo.test.line", &[b], &["with_tax"])
        .await
        .unwrap();
    assert_eq!(rows[0]["with_tax"], json!(280));
    assert!(
        rows[0].get("qty").is_none(),
        "a dependency is not smuggled into the reply"
    );
}

#[tokio::test]
async fn a_broken_compute_declaration_is_an_error_live() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    fn zero(_: &Map<String, Value>) -> Value {
        Value::from(0)
    }
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.broken".into(),
            table: "rusdoo_test_cmp_broken".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("bad", FieldType::Integer).computed(&["nope"], zero),
            Field::new("empty", FieldType::Integer).computed(&[], zero),
        ],
    ))
    .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_cmp_broken""#)
        .execute(&pool)
        .await
        .unwrap();
    reg.get("rusdoo.test.broken")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();
    let id = reg
        .create(&pool, "rusdoo.test.broken", vec![("name", json!("x"))])
        .await
        .unwrap();

    // depending on a field that does not exist would compute the wrong
    // value for every record, silently
    let err = reg
        .read(&pool, "rusdoo.test.broken", &[id], &["bad"])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown field"), "{err}");
    // and a compute with no dependency can never be invalidated
    let err = reg
        .read(&pool, "rusdoo.test.broken", &[id], &["empty"])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no dependency"), "{err}");
}

/// The other half: a compute materialized into a real column, which is
/// what makes it indexable, orderable and groupable.
fn stored_registry() -> Registry {
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.stored".into(),
            table: "rusdoo_test_cmp_stored".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("qty", FieldType::Integer),
            Field::new("price", FieldType::Integer),
            Field::new("total", FieldType::Integer)
                .computed(&["qty", "price"], total)
                .store(),
        ],
    ))
    .unwrap();
    reg
}

#[tokio::test]
async fn stored_computes_are_materialized_and_recomputed_live() {
    use rusdoo_orm::group::{Aggregate, GroupBy, GroupOptions};

    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let reg = stored_registry();
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_cmp_stored""#)
        .execute(&pool)
        .await
        .unwrap();
    reg.get("rusdoo.test.stored")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();

    // the create computes it, inside the insert's own transaction
    let a = reg
        .create(
            &pool,
            "rusdoo.test.stored",
            vec![
                ("name", json!("a")),
                ("qty", json!(3)),
                ("price", json!(50)),
            ],
        )
        .await
        .unwrap();
    let b = reg
        .create(
            &pool,
            "rusdoo.test.stored",
            vec![
                ("name", json!("b")),
                ("qty", json!(2)),
                ("price", json!(70)),
            ],
        )
        .await
        .unwrap();
    // the value is really in the column, not derived on the way out
    let stored: i32 =
        sqlx::query_scalar(r#"SELECT "total" FROM "rusdoo_test_cmp_stored" WHERE "id" = $1"#)
            .bind(a as i32)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, 150);

    // writing a dependency recomputes it
    reg.write(&pool, "rusdoo.test.stored", &[a], vec![("qty", json!(10))])
        .await
        .unwrap();
    let stored: i32 =
        sqlx::query_scalar(r#"SELECT "total" FROM "rusdoo_test_cmp_stored" WHERE "id" = $1"#)
            .bind(a as i32)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, 500);

    // writing something else leaves it alone (no needless UPDATE)
    reg.write(
        &pool,
        "rusdoo.test.stored",
        &[a],
        vec![("name", json!("a renomeado"))],
    )
    .await
    .unwrap();
    let rows = reg
        .read(&pool, "rusdoo.test.stored", &[a], &["total"])
        .await
        .unwrap();
    assert_eq!(rows[0]["total"], json!(500));

    // the payoff: it can be searched, ordered and grouped by, which a
    // computed value that only exists on the way out cannot
    let found = reg
        .search(
            &pool,
            "rusdoo.test.stored",
            &parse_domain(&json!([["total", ">", 200]])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(found, vec![a]);
    let ordered = reg
        .search(
            &pool,
            "rusdoo.test.stored",
            &parse_domain(&json!([])).unwrap(),
            &SearchOptions {
                order: Some("total desc".into()),
                ..SearchOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(ordered, vec![a, b]);
    let model = reg.get("rusdoo.test.stored").unwrap();
    let groups = reg
        .read_group(
            &pool,
            "rusdoo.test.stored",
            &parse_domain(&json!([])).unwrap(),
            &[GroupBy::parse(model, "total").unwrap()],
            &[Aggregate::Count],
            &GroupOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0]["total"], json!(140));

    // and a client still cannot write it: only the recompute does
    let err = reg
        .write(&pool, "rusdoo.test.stored", &[a], vec![("total", json!(1))])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("readonly"), "{err}");
}
