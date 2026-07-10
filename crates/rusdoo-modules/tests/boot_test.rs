//! Integrated module boot: discover -> dependency order -> schema ->
//! data loading, the Rust side of `odoo/modules/loading.py`.

use rusdoo_modules::installer::{install_modules, XmlIds};
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::json;
use std::path::Path;

fn boot_registry() -> Registry {
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.bootco".into(),
            table: "rusdoo_test_bootco".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.bootpartner".into(),
            table: "rusdoo_test_bootpartner".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "company_id",
                FieldType::Many2one {
                    comodel: "rusdoo.test.bootco".into(),
                },
            ),
        ],
    ))
    .unwrap();
    reg
}

#[tokio::test]
async fn boots_fixture_addons_in_dependency_order() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = rusdoo_orm::db::connect(&url).await.unwrap();
    for table in ["rusdoo_test_bootpartner", "rusdoo_test_bootco"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    let mut reg = boot_registry();
    // ensure the persistence table exists, then isolate this test's modules
    XmlIds::load(&pool).await.unwrap();
    sqlx::query(r#"DELETE FROM "ir_model_data" WHERE "module" IN ('demo_a', 'demo_b')"#)
        .execute(&pool)
        .await
        .unwrap();
    let mut xml_ids = XmlIds::new();

    // Act: full boot — schema + both fixture modules (b depends on a)
    let report = install_modules(
        &pool,
        &mut reg,
        &[Path::new("tests/fixtures/addons")],
        &mut xml_ids,
    )
    .await
    .unwrap();

    // Assert: modules installed in dependency order with their stats
    let names: Vec<&str> = report.modules.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["demo_a", "demo_b"]);
    assert_eq!(report.modules[0].1.created, 1); // acme
    assert_eq!(report.modules[1].1.created, 2); // ana (xml) + bia (csv)

    // cross-module ref resolved: both partners point at demo_a.acme
    let acme = xml_ids.get("demo_a.acme").unwrap().1;
    let dom = parse_domain(&json!([["company_id", "=", acme]])).unwrap();
    let found = reg
        .search(
            &pool,
            "rusdoo.test.bootpartner",
            &dom,
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(found.len(), 2);

    // external ids are PERSISTED (ir.model.data): a fresh process
    // reloads them and a re-boot creates no duplicates
    let mut reloaded = XmlIds::load(&pool).await.unwrap();
    assert!(
        reloaded.get("demo_a.acme").is_some(),
        "external ids survive restarts"
    );
    let report = install_modules(
        &pool,
        &mut reg,
        &[Path::new("tests/fixtures/addons")],
        &mut reloaded,
    )
    .await
    .unwrap();
    let total_created: usize = report.modules.iter().map(|(_, s)| s.created).sum();
    assert_eq!(total_created, 0, "no duplicate records on re-boot");
    let dom = parse_domain(&json!([])).unwrap();
    let all = reg
        .search(
            &pool,
            "rusdoo.test.bootpartner",
            &dom,
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(all.len(), 2, "still exactly ana and bia");
}

#[tokio::test]
async fn addon_defined_models_are_registered_and_loaded() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = rusdoo_orm::db::connect(&url).await.unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "x_lib_livro""#)
        .execute(&pool)
        .await
        .unwrap();
    XmlIds::load(&pool).await.unwrap();
    sqlx::query(r#"DELETE FROM "ir_model_data" WHERE "module" = 'demo_models'"#)
        .execute(&pool)
        .await
        .unwrap();

    // Act: an EMPTY registry — the addon defines its own model
    let mut reg = Registry::new();
    let mut xml_ids = XmlIds::new();
    let report = install_modules(
        &pool,
        &mut reg,
        &[Path::new("tests/fixtures/addons_models")],
        &mut xml_ids,
    )
    .await
    .unwrap();

    assert_eq!(report.modules.len(), 1);
    let model = reg
        .get("x_lib.livro")
        .expect("model registered from ir.model records");
    assert_eq!(model.meta.table, "x_lib_livro");
    assert!(model.field("titulo").unwrap().required);

    // the data records for the addon-defined model loaded and are queryable
    let dom = parse_domain(&json!([["paginas", ">", 100]])).unwrap();
    let found = reg
        .search(&pool, "x_lib.livro", &dom, &SearchOptions::default())
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    let rows = reg
        .read(&pool, "x_lib.livro", &found, &["titulo"])
        .await
        .unwrap();
    assert_eq!(rows[0]["titulo"], json!("Dom Casmurro"));
}
