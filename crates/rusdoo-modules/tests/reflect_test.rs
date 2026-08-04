//! The registry describing itself: the rows, the external ids, and the
//! property that lets it run on every boot.

use rusdoo_modules::installer::XmlIds;
use rusdoo_modules::reflect;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::registry::Registry;
use serde_json::{json, Value};

async fn field_of(
    registry: &Registry,
    pool: &sqlx::PgPool,
    model: &str,
    name: &str,
) -> Value {
    let ids = registry
        .search(
            pool,
            "ir.model.fields",
            &parse_domain(&json!([["model", "=", model], ["name", "=", name]])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .expect("the search runs");
    assert_eq!(ids.len(), 1, "one row for {model}.{name}: {ids:?}");
    Value::Object(
        registry
            .read(
                pool,
                "ir.model.fields",
                &ids,
                &["ttype", "relation", "relation_field", "required", "store", "model_id"],
            )
            .await
            .expect("the field reads")
            .into_iter()
            .next()
            .unwrap(),
    )
}

async fn count(registry: &Registry, pool: &sqlx::PgPool, model: &str) -> usize {
    registry
        .search(pool, model, &parse_domain(&json!([])).unwrap(), &SearchOptions::default())
        .await
        .expect("the search runs")
        .len()
}

#[tokio::test]
async fn the_registry_writes_down_what_it_is_live() {
    let Some(pool) = rusdoo_testing::pool_in("rusdoo_reflect") else {
        return;
    };
    let registry = rusdoo_base::registry().expect("base registers");
    for table in ["ir_model_fields", "ir_model", "ir_model_data"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}" CASCADE"#))
            .execute(&pool)
            .await
            .unwrap();
    }
    registry.init_tables(&pool).await.expect("the tables are made");
    let mut xml_ids = XmlIds::load(&pool).await.expect("the external ids load");

    let described = reflect::reflect(&registry, &pool, &mut xml_ids)
        .await
        .expect("reflection runs");
    assert!(described > 10, "described {described} models");

    // a many2one says what it points at; a one2many says through what
    let partner = field_of(&registry, &pool, "res.users", "company_id").await;
    assert_eq!(partner["ttype"], json!("many2one"), "{partner}");
    assert_eq!(partner["relation"], json!("res.company"), "{partner}");
    assert!(partner["model_id"][0].as_i64().is_some(), "{partner}");

    // and every field row belongs to the model row it describes
    let module = field_of(&registry, &pool, "ir.module.module", "dependencies_id").await;
    assert_eq!(module["ttype"], json!("one2many"), "{module}");
    assert_eq!(module["relation_field"], json!("module_id"), "{module}");

    // the external ids Odoo's own data points at
    let partner_model = xml_ids
        .get("base.model_res_partner")
        .expect("the model id was published");
    assert_eq!(partner_model.0, "ir.model");
    assert!(xml_ids
        .get("base.field_res_partner__name")
        .is_some_and(|(model, _)| model == "ir.model.fields"));

    // ── the property that lets this run on every boot ────────────────
    let models = count(&registry, &pool, "ir.model").await;
    let fields = count(&registry, &pool, "ir.model.fields").await;
    reflect::reflect(&registry, &pool, &mut xml_ids)
        .await
        .expect("the second run works");
    assert_eq!(count(&registry, &pool, "ir.model").await, models, "duplicou modelos");
    assert_eq!(count(&registry, &pool, "ir.model.fields").await, fields, "duplicou campos");

    // and it survives a restart: the ids are rows, not memory
    let reloaded = XmlIds::load(&pool).await.expect("the external ids load again");
    assert!(reloaded.get("base.model_res_partner").is_some());

    sqlx::query("DROP SCHEMA IF EXISTS rusdoo_reflect CASCADE")
        .execute(&pool)
        .await
        .ok();
}

/// A model the binary lost stops being described — otherwise a screen
/// would offer a model nothing can answer for.
#[tokio::test]
async fn a_model_the_registry_no_longer_has_loses_its_row_live() {
    let Some(pool) = rusdoo_testing::pool_in("rusdoo_reflect_gone") else {
        return;
    };
    let registry = rusdoo_base::registry().expect("base registers");
    for table in ["ir_model_fields", "ir_model", "ir_model_data"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}" CASCADE"#))
            .execute(&pool)
            .await
            .unwrap();
    }
    registry.init_tables(&pool).await.unwrap();
    let mut xml_ids = XmlIds::load(&pool).await.unwrap();
    reflect::reflect(&registry, &pool, &mut xml_ids).await.unwrap();

    // a row for a model nobody compiled: what an older binary would have
    // left behind
    registry
        .create(
            &pool,
            "ir.model",
            vec![
                ("model", json!("x_from_an_older_binary")),
                ("name", json!("Fantasma")),
            ],
        )
        .await
        .unwrap();
    reflect::reflect(&registry, &pool, &mut xml_ids).await.unwrap();

    let left = registry
        .search(
            &pool,
            "ir.model",
            &parse_domain(&json!([["model", "=", "x_from_an_older_binary"]])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    assert!(left.is_empty(), "o modelo fantasma ficou: {left:?}");

    sqlx::query("DROP SCHEMA IF EXISTS rusdoo_reflect_gone CASCADE")
        .execute(&pool)
        .await
        .ok();
}
