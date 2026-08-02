//! Computes that depend on the records a relation points at — Odoo's
//! `@api.depends('order_line.price_subtotal')`. Order totals are the
//! reason this exists: the number lives on the parent, the values that
//! make it live on the lines.

use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use sqlx::PgPool;

/// A line's own total, from its own columns.
fn subtotal(record: &Map<String, Value>) -> Value {
    let qty = record.get("qty").and_then(Value::as_i64).unwrap_or(0);
    let price = record.get("price").and_then(Value::as_i64).unwrap_or(0);
    Value::from(qty * price)
}

/// The parent's total: the sum of what the lines answer.
fn amount_total(record: &Map<String, Value>) -> Value {
    let lines = record
        .get("line_ids.subtotal")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Value::from(lines.iter().filter_map(Value::as_i64).sum::<i64>())
}

/// A compute reading through a many2one: one value, not a list.
fn partner_label(record: &Map<String, Value>) -> Value {
    let names = record
        .get("partner_id.name")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let name = names
        .first()
        .and_then(Value::as_str)
        .unwrap_or("sem parceiro");
    Value::from(format!("Pedido de {name}"))
}

fn order_registry(suffix: &str) -> Registry {
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.cl.partner".into(),
            table: format!("rusdoo_test_cl_{suffix}_partner"),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.cl.order".into(),
            table: format!("rusdoo_test_cl_{suffix}_order"),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "partner_id",
                FieldType::Many2one {
                    comodel: "rusdoo.test.cl.partner".into(),
                },
            ),
            Field::new(
                "line_ids",
                FieldType::One2many {
                    comodel: "rusdoo.test.cl.line".into(),
                    inverse: "order_id".into(),
                },
            ),
            Field::new("amount_total", FieldType::Integer)
                .computed(&["line_ids.subtotal"], amount_total),
            // the same mechanism through a many2one hop
            Field::new("label", FieldType::Char { size: None })
                .computed(&["partner_id.name"], partner_label),
            // and stored, which is what a total on a big table wants
            Field::new("amount_stored", FieldType::Integer)
                .computed(&["line_ids.subtotal"], amount_total)
                .store(),
        ],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.cl.line".into(),
            table: format!("rusdoo_test_cl_{suffix}_line"),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new(
                "order_id",
                FieldType::Many2one {
                    comodel: "rusdoo.test.cl.order".into(),
                },
            ),
            Field::new("qty", FieldType::Integer),
            Field::new("price", FieldType::Integer),
            Field::new("subtotal", FieldType::Integer).computed(&["qty", "price"], subtotal),
        ],
    ))
    .unwrap();
    reg
}

/// Each test builds its own tables: the suite runs them in parallel and
/// concurrent DDL on the same names deadlocks in the catalog.
async fn fixture(suffix: &str) -> Option<(Registry, PgPool)> {
    let url = std::env::var("RUSDOO_TEST_DATABASE_URL").ok()?;
    let pool = rusdoo_orm::db::connect(&url).await.expect("test database");
    let reg = order_registry(suffix);
    for table in [
        format!("rusdoo_test_cl_{suffix}_line"),
        format!("rusdoo_test_cl_{suffix}_order"),
        format!("rusdoo_test_cl_{suffix}_partner"),
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}" CASCADE"#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for model in [
        "rusdoo.test.cl.partner",
        "rusdoo.test.cl.order",
        "rusdoo.test.cl.line",
    ] {
        reg.get(model).unwrap().init_table(&pool).await.unwrap();
    }
    Some((reg, pool))
}

#[tokio::test]
async fn a_total_sums_the_lines_live() {
    let Some((reg, pool)) = fixture("sum").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let partner = reg
        .create(&pool, "rusdoo.test.cl.partner", vec![("name", json!("Ana"))])
        .await
        .unwrap();
    // two lines through the parent's x2many commands, like a form save
    let order = reg
        .create(
            &pool,
            "rusdoo.test.cl.order",
            vec![
                ("name", json!("SO001")),
                ("partner_id", json!(partner)),
                (
                    "line_ids",
                    json!([
                        [0, 0, {"qty": 2, "price": 50}],
                        [0, 0, {"qty": 1, "price": 30}],
                    ]),
                ),
            ],
        )
        .await
        .unwrap();

    let rows = reg
        .read(
            &pool,
            "rusdoo.test.cl.order",
            &[order],
            &["amount_total", "label", "amount_stored"],
        )
        .await
        .unwrap();
    assert_eq!(rows[0]["amount_total"], json!(130));
    // a many2one hop gives the compute one value, read the same way
    assert_eq!(rows[0]["label"], json!("Pedido de Ana"));
    // the stored twin was materialized by the create
    assert_eq!(rows[0]["amount_stored"], json!(130));

    // an order with no lines totals zero, not null
    let empty = reg
        .create(
            &pool,
            "rusdoo.test.cl.order",
            vec![("name", json!("SO002"))],
        )
        .await
        .unwrap();
    let rows = reg
        .read(
            &pool,
            "rusdoo.test.cl.order",
            &[empty],
            &["amount_total", "label"],
        )
        .await
        .unwrap();
    assert_eq!(rows[0]["amount_total"], json!(0));
    assert_eq!(rows[0]["label"], json!("Pedido de sem parceiro"));
}

#[tokio::test]
async fn changing_a_line_through_the_parent_moves_the_total_live() {
    let Some((reg, pool)) = fixture("edit").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let order = reg
        .create(
            &pool,
            "rusdoo.test.cl.order",
            vec![
                ("name", json!("SO003")),
                ("line_ids", json!([[0, 0, {"qty": 2, "price": 50}]])),
            ],
        )
        .await
        .unwrap();
    let lines = reg
        .read(&pool, "rusdoo.test.cl.order", &[order], &["line_ids"])
        .await
        .unwrap();
    let line_id = lines[0]["line_ids"][0].as_i64().unwrap();

    // command 1: update the line through the order, as a form save does
    reg.write(
        &pool,
        "rusdoo.test.cl.order",
        &[order],
        vec![("line_ids", json!([[1, line_id, {"qty": 3}]]))],
    )
    .await
    .unwrap();
    let rows = reg
        .read(
            &pool,
            "rusdoo.test.cl.order",
            &[order],
            &["amount_total", "amount_stored"],
        )
        .await
        .unwrap();
    assert_eq!(rows[0]["amount_total"], json!(150));
    assert_eq!(
        rows[0]["amount_stored"],
        json!(150),
        "the stored total is recomputed by the write that changed the line"
    );

    // command 2: drop the line and the total goes with it
    reg.write(
        &pool,
        "rusdoo.test.cl.order",
        &[order],
        vec![("line_ids", json!([[2, line_id, 0]]))],
    )
    .await
    .unwrap();
    let rows = reg
        .read(
            &pool,
            "rusdoo.test.cl.order",
            &[order],
            &["amount_total", "amount_stored"],
        )
        .await
        .unwrap();
    assert_eq!(rows[0]["amount_total"], json!(0));
    assert_eq!(rows[0]["amount_stored"], json!(0));
}

#[tokio::test]
async fn a_dependency_on_a_non_relational_field_is_an_error_live() {
    let Some((_reg, pool)) = fixture("broken").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.cl.broken".into(),
            table: "rusdoo_test_cl_broken".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            // `name` is a char: there is nothing to walk into
            Field::new("bad", FieldType::Integer).computed(&["name.thing"], amount_total),
        ],
    ))
    .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_cl_broken""#)
        .execute(&pool)
        .await
        .unwrap();
    reg.get("rusdoo.test.cl.broken")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();
    let id = reg
        .create(&pool, "rusdoo.test.cl.broken", vec![("name", json!("x"))])
        .await
        .unwrap();
    let error = reg
        .read(&pool, "rusdoo.test.cl.broken", &[id], &["bad"])
        .await
        .expect_err("a path through a char is refused");
    assert!(
        error.to_string().contains("not a relational field"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn a_batch_mixing_integer_and_decimal_prices_stores_both_live() {
    let Some((_reg, pool)) = fixture("money").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    // fixed-precision columns: the rows of one create share a prepared
    // statement, so `1250` in the first line and `890.5` in the second
    // must bind the same way or the second is read through the wrong
    // decoder and overflows the column
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.cl.price".into(),
            table: "rusdoo_test_cl_price".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "price",
                FieldType::Float {
                    digits: Some((16, 2)),
                },
            ),
        ],
    ))
    .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_cl_price""#)
        .execute(&pool)
        .await
        .unwrap();
    reg.get("rusdoo.test.cl.price")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();

    let mut ids = Vec::new();
    for (name, price) in [("inteiro", json!(1250)), ("decimal", json!(890.5))] {
        ids.push(
            reg.create(
                &pool,
                "rusdoo.test.cl.price",
                vec![("name", json!(name)), ("price", price)],
            )
            .await
            .unwrap(),
        );
    }
    let rows = reg
        .read(&pool, "rusdoo.test.cl.price", &ids, &["name", "price"])
        .await
        .unwrap();
    assert_eq!(rows[0]["price"], json!(1250.0));
    assert_eq!(rows[1]["price"], json!(890.5));
}
