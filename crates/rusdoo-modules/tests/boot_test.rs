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
    let reg = boot_registry();
    let mut xml_ids = XmlIds::new();

    // Act: full boot — schema + both fixture modules (b depends on a)
    let report = install_modules(
        &pool,
        &reg,
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

    // schema init is idempotent: booting again must not fail on DDL
    let mut fresh_ids = XmlIds::new();
    let report = install_modules(
        &pool,
        &reg,
        &[Path::new("tests/fixtures/addons")],
        &mut fresh_ids,
    )
    .await
    .unwrap();
    assert_eq!(report.modules.len(), 2);
}
