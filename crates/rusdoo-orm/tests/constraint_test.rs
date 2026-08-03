//! Model constraints (`@api.constrains`): a rule every record must
//! satisfy, checked inside the transaction that wrote it.

use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use sqlx::PgPool;

fn number(record: &Map<String, Value>, name: &str) -> f64 {
    record
        .get(name)
        .and_then(Value::as_f64)
        .or_else(|| record.get(name).and_then(Value::as_i64).map(|n| n as f64))
        .unwrap_or(0.0)
}

/// A quantity nobody can sell.
fn positive_qty(record: &Map<String, Value>) -> Result<(), String> {
    if number(record, "qty") <= 0.0 {
        return Err("a quantidade precisa ser maior que zero".into());
    }
    Ok(())
}

/// A discount that would pay the customer to take it.
fn sane_discount(record: &Map<String, Value>) -> Result<(), String> {
    let discount = number(record, "discount");
    if !(0.0..=100.0).contains(&discount) {
        return Err(format!("desconto de {discount}% está fora de 0 a 100"));
    }
    Ok(())
}

fn registry(suffix: &str) -> Registry {
    let mut reg = Registry::new();
    reg.register(
        Model::new(
            ModelMeta {
                name: "rusdoo.test.line".into(),
                table: format!("rusdoo_test_constraint_{suffix}"),
                inherit: vec![],
                inherits: vec![],
            },
            vec![
                Field::new("name", FieldType::Char { size: None }),
                Field::new("qty", FieldType::Integer).default_value(json!(1)),
                Field::new("discount", FieldType::Integer).default_value(json!(0)),
            ],
        )
        .constrained("quantidade positiva", &["qty"], positive_qty)
        .constrained("desconto entre 0 e 100", &["discount"], sane_discount),
    )
    .unwrap();
    reg
}

async fn fixture(suffix: &str) -> Option<(Registry, PgPool)> {
    let pool = rusdoo_testing::pool_in("rusdoo_constraint_te_fixture").expect("test database");
    let reg = registry(suffix);
    sqlx::query(&format!(
        r#"DROP TABLE IF EXISTS "rusdoo_test_constraint_{suffix}""#
    ))
    .execute(&pool)
    .await
    .unwrap();
    reg.get("rusdoo.test.line")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();
    Some((reg, pool))
}

#[tokio::test]
async fn a_create_that_breaks_a_rule_leaves_nothing_behind_live() {
    let Some((reg, pool)) = fixture("create").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let error = reg
        .create(
            &pool,
            "rusdoo.test.line",
            vec![("name", json!("ruim")), ("qty", json!(0))],
        )
        .await
        .expect_err("a rule refused it");
    assert!(
        error.to_string().contains("maior que zero"),
        "the reason reaches the caller: {error}"
    );

    // and the row is not there: the check ran inside the transaction
    let count: i64 = sqlx::query_scalar(r#"SELECT count(*) FROM "rusdoo_test_constraint_create""#)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "a refused record must not survive");

    // a good one goes through
    let id = reg
        .create(
            &pool,
            "rusdoo.test.line",
            vec![("name", json!("boa")), ("qty", json!(3))],
        )
        .await
        .unwrap();
    assert!(id > 0);
}

#[tokio::test]
async fn a_write_that_breaks_a_rule_is_rolled_back_live() {
    let Some((reg, pool)) = fixture("write").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let id = reg
        .create(
            &pool,
            "rusdoo.test.line",
            vec![("name", json!("boa")), ("qty", json!(5))],
        )
        .await
        .unwrap();

    let error = reg
        .write(
            &pool,
            "rusdoo.test.line",
            &[id],
            vec![("name", json!("editada")), ("qty", json!(-2))],
        )
        .await
        .expect_err("a rule refused it");
    assert!(error.to_string().contains("maior que zero"), "{error}");

    // neither the quantity nor the name moved: the whole write went back
    let rows = reg
        .read(&pool, "rusdoo.test.line", &[id], &["name", "qty"])
        .await
        .unwrap();
    assert_eq!(rows[0]["qty"], json!(5));
    assert_eq!(rows[0]["name"], "boa", "the write was one transaction");
}

#[tokio::test]
async fn a_write_only_rechecks_what_it_touched_live() {
    let Some((reg, pool)) = fixture("scope").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    // a record whose discount is already impossible (written straight to
    // the column, as a migration would leave it)
    let id = reg
        .create(&pool, "rusdoo.test.line", vec![("qty", json!(1))])
        .await
        .unwrap();
    sqlx::query(r#"UPDATE "rusdoo_test_constraint_scope" SET "discount" = 300 WHERE "id" = $1"#)
        .bind(id as i32)
        .execute(&pool)
        .await
        .unwrap();

    // editing the name does not resurrect a rule about the discount:
    // Odoo rechecks the constraints the write touched, not all of them
    reg.write(
        &pool,
        "rusdoo.test.line",
        &[id],
        vec![("name", json!("outra"))],
    )
    .await
    .expect("editing the name is allowed");

    // touching the discount itself does check it
    let error = reg
        .write(
            &pool,
            "rusdoo.test.line",
            &[id],
            vec![("discount", json!(150))],
        )
        .await
        .expect_err("now the rule applies");
    assert!(error.to_string().contains("fora de 0 a 100"), "{error}");
}

#[tokio::test]
async fn every_record_of_a_batch_is_checked_live() {
    let Some((reg, pool)) = fixture("batch").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let mut ids = Vec::new();
    for qty in [1, 2, 3] {
        ids.push(
            reg.create(&pool, "rusdoo.test.line", vec![("qty", json!(qty))])
                .await
                .unwrap(),
        );
    }
    // one write, three records: the rule is about each of them
    let error = reg
        .write(&pool, "rusdoo.test.line", &ids, vec![("qty", json!(0))])
        .await
        .expect_err("all three are refused");
    assert!(error.to_string().contains("maior que zero"), "{error}");
    let rows = reg
        .read(&pool, "rusdoo.test.line", &ids, &["qty"])
        .await
        .unwrap();
    assert_eq!(rows[0]["qty"], json!(1), "nothing moved");
}
