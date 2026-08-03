//! Converting between units, against a real database: the chain of
//! reference units, the refusal across categories, and the rules a unit
//! of measure has to satisfy to be usable at all.

use rusdoo_orm::methods::{MethodCtx, MethodRegistry};
use rusdoo_orm::registry::Registry;
use std::sync::Arc;
use serde_json::{json, Map, Value};
use sqlx::PgPool;

/// Each test gets its own schema, so the suite is safe in parallel.
fn pool(url: &str, schema: &'static str) -> PgPool {
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

async fn fixture(schema: &'static str) -> Option<(Arc<Registry>, MethodRegistry, PgPool)> {
    let url = std::env::var("RUSDOO_TEST_DATABASE_URL").ok()?;
    let pool = pool(&url, schema);
    let mut registry = rusdoo_base::registry().unwrap();
    rusdoo_uom::extend(&mut registry).unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "uom_uom" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
    registry
        .get("uom.uom")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();
    let mut methods = MethodRegistry::new();
    rusdoo_uom::extend_methods(&mut methods).unwrap();
    Some((Arc::new(registry), methods, pool))
}

/// Create a unit; `reference` is `None` for the root of a category.
async fn unit(
    registry: &Arc<Registry>,
    pool: &PgPool,
    name: &str,
    relative_factor: f64,
    reference: Option<i64>,
) -> i64 {
    let mut values = vec![
        ("name", json!(name)),
        ("relative_factor", json!(relative_factor)),
    ];
    if let Some(reference) = reference {
        values.push(("relative_uom_id", json!(reference)));
    }
    registry.create(pool, "uom.uom", values).await.unwrap()
}

/// The weight chain Odoo ships: g → kg → t.
async fn weights(registry: &Arc<Registry>, pool: &PgPool) -> (i64, i64, i64) {
    let gram = unit(registry, pool, "g", 1.0, None).await;
    let kilo = unit(registry, pool, "kg", 1000.0, Some(gram)).await;
    let ton = unit(registry, pool, "t", 1000.0, Some(kilo)).await;
    (gram, kilo, ton)
}

async fn call(
    registry: &Arc<Registry>,
    methods: &MethodRegistry,
    pool: &PgPool,
    from: i64,
    name: &str,
    args: Vec<Value>,
    kwargs: Map<String, Value>,
) -> Result<Value, rusdoo_core::RusdooError> {
    let method = methods.get("uom.uom", name).expect("method registered");
    // a method's positional arguments live in `rest`: `args[0]` is the
    // recordset call_kw sent
    let ctx = MethodCtx::new(Arc::clone(registry), pool, 1, "uom.uom", vec![from]).with_rest(args.clone());
    method.call(ctx, &args, &kwargs).await
}

#[tokio::test]
async fn a_quantity_travels_the_whole_reference_chain_live() {
    let Some((registry, methods, pool)) = fixture(rusdoo_testing::schema_for("rusdoo_uom_convert")).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (gram, kilo, ton) = weights(&registry, &pool).await;

    // the cases odoo/addons/uom/tests/test_uom.py asserts
    let converted = call(
        &registry,
        &methods,
        &pool,
        gram,
        "convert_quantity",
        vec![json!(1_020_000), json!(ton)],
        Map::new(),
    )
    .await
    .unwrap();
    assert_eq!(converted, json!(1.02), "1_020_000 g é 1,02 t");

    // 1234 g is 1.234 kg, and converting rounds up: a conversion that
    // came out short would ship less than was asked for
    let converted = call(
        &registry,
        &methods,
        &pool,
        gram,
        "convert_quantity",
        vec![json!(1234), json!(kilo)],
        Map::new(),
    )
    .await
    .unwrap();
    assert_eq!(converted, json!(1.24));

    // asking for the raw number gives the ratio untouched
    let mut kwargs = Map::new();
    kwargs.insert("round".into(), json!(false));
    let raw = call(
        &registry,
        &methods,
        &pool,
        gram,
        "convert_quantity",
        vec![json!(1234), json!(kilo)],
        kwargs,
    )
    .await
    .unwrap();
    assert_eq!(raw, json!(1.234));

    // and the same unit on both sides is the identity, not a rounding
    let same = call(
        &registry,
        &methods,
        &pool,
        gram,
        "convert_quantity",
        vec![json!(22.437), json!(gram)],
        Map::new(),
    )
    .await
    .unwrap();
    assert_eq!(same, json!(22.437));
}

#[tokio::test]
async fn converting_across_categories_is_refused_live() {
    let Some((registry, methods, pool)) = fixture(rusdoo_testing::schema_for("rusdoo_uom_categories")).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (gram, _kilo, ton) = weights(&registry, &pool).await;
    let hour = unit(&registry, &pool, "Horas", 1.0, None).await;
    let day = unit(&registry, &pool, "Dias", 8.0, Some(hour)).await;

    // inside a category, however deep, the conversion happens
    let converted = call(
        &registry,
        &methods,
        &pool,
        day,
        "convert_quantity",
        vec![json!(2), json!(hour)],
        Map::new(),
    )
    .await
    .unwrap();
    assert_eq!(converted, json!(16.0));

    // across categories it does not, and the message names both roots
    let refusal = call(
        &registry,
        &methods,
        &pool,
        ton,
        "convert_quantity",
        vec![json!(1), json!(day)],
        Map::new(),
    )
    .await
    .expect_err("tonelada não vira dia");
    let message = refusal.to_string();
    assert!(message.contains("different categories"), "{message}");
    assert!(
        message.contains('g') && message.contains("Horas"),
        "{message}"
    );

    // and a unit that is not there is said out loud, not treated as "no
    // conversion needed"
    let missing = call(
        &registry,
        &methods,
        &pool,
        gram,
        "convert_quantity",
        vec![json!(1), json!(999_999)],
        Map::new(),
    )
    .await
    .expect_err("a unidade de destino não existe");
    assert!(missing.to_string().contains("does not exist"), "{missing}");
}

#[tokio::test]
async fn the_absolute_factor_follows_an_edit_up_the_chain_live() {
    let Some((registry, _methods, pool)) = fixture(rusdoo_testing::schema_for("rusdoo_uom_factor")).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (_gram, kilo, ton) = weights(&registry, &pool).await;

    let rows = registry
        .read(&pool, "uom.uom", &[ton], &["factor", "reference_name"])
        .await
        .unwrap();
    assert_eq!(rows[0]["factor"], json!(1_000_000.0));
    assert_eq!(rows[0]["reference_name"], json!("g"));

    // the point of not storing it: an edit two links up is visible on
    // the next read, instead of leaving a stale number in a column
    registry
        .write(
            &pool,
            "uom.uom",
            &[kilo],
            vec![("relative_factor", json!(500.0))],
        )
        .await
        .unwrap();
    let rows = registry
        .read(&pool, "uom.uom", &[ton], &["factor"])
        .await
        .unwrap();
    assert_eq!(rows[0]["factor"], json!(500_000.0));
}

#[tokio::test]
async fn a_price_converts_the_other_way_round_live() {
    let Some((registry, methods, pool)) = fixture(rusdoo_testing::schema_for("rusdoo_uom_price")).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (gram, _kilo, ton) = weights(&registry, &pool).await;

    // 2 per gram is 2 million per ton — the same money, said per ton
    let price = call(
        &registry,
        &methods,
        &pool,
        gram,
        "convert_price",
        vec![json!(2), json!(ton)],
        Map::new(),
    )
    .await
    .unwrap();
    assert_eq!(price, json!(2_000_000.0));

    // and back again, without rounding eating the cents
    let price = call(
        &registry,
        &methods,
        &pool,
        ton,
        "convert_price",
        vec![json!(2_000_000), json!(gram)],
        Map::new(),
    )
    .await
    .unwrap();
    assert_eq!(price, json!(2.0));
}

#[tokio::test]
async fn a_unit_that_cannot_convert_is_never_saved_live() {
    let Some((registry, _methods, pool)) = fixture(rusdoo_testing::schema_for("rusdoo_uom_rules")).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let gram = unit(&registry, &pool, "g", 1.0, None).await;

    // a factor of zero would make every conversion a division by zero
    let refusal = registry
        .create(
            &pool,
            "uom.uom",
            vec![
                ("name", json!("nada")),
                ("relative_factor", json!(0)),
                ("relative_uom_id", json!(gram)),
            ],
        )
        .await
        .expect_err("fator zero não passa");
    assert!(refusal.to_string().contains("greater than zero"), "{refusal}");

    // a unit with no reference is the root of its category: it cannot
    // claim to contain twelve of something it does not name
    let refusal = registry
        .create(
            &pool,
            "uom.uom",
            vec![("name", json!("solta")), ("relative_factor", json!(12))],
        )
        .await
        .expect_err("raiz com fator 12 não passa");
    assert!(
        refusal.to_string().contains("reference unit"),
        "{refusal}"
    );

    // and nothing is its own reference: that chain has no end
    let refusal = registry
        .write(
            &pool,
            "uom.uom",
            &[gram],
            vec![("relative_uom_id", json!(gram))],
        )
        .await
        .expect_err("auto-referência não passa");
    assert!(
        refusal
            .to_string()
            .contains("its own reference unit"),
        "{refusal}"
    );

    // the refusals left nothing behind
    let rows = registry
        .search(
            &pool,
            "uom.uom",
            &rusdoo_orm::domain::parse_domain(&json!([])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(rows, vec![gram], "só a unidade boa sobreviveu");
}

#[tokio::test]
async fn the_ordering_sequence_follows_the_factor_live() {
    let Some((registry, _methods, pool)) = fixture(rusdoo_testing::schema_for("rusdoo_uom_sequence")).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let (gram, kilo, _ton) = weights(&registry, &pool).await;

    // the smaller unit sorts first, without anybody ordering the list by
    // hand — and the sequence is a real column, so the database can
    let rows = registry
        .read(&pool, "uom.uom", &[gram, kilo], &["sequence"])
        .await
        .unwrap();
    assert_eq!(rows[0]["sequence"], json!(100));
    assert_eq!(rows[1]["sequence"], json!(1000), "capado no teto");
}
