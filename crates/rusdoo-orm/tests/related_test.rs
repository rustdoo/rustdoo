//! Related fields (`odoo/orm/fields.py`'s `related`): a name for a value
//! that lives on another record, reached by following many2one hops.

use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::json;
use sqlx::PgPool;

fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.into(),
        table: table.into(),
        inherit: vec![],
        inherits: vec![],
    }
}

/// partner -> company -> country, so a path can be two hops long.
fn related_registry() -> Registry {
    let mut reg = Registry::new();
    reg.register(Model::new(
        meta("res.country", "rusdoo_test_rel_country"),
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("code", FieldType::Char { size: None }),
        ],
    ))
    .unwrap();
    reg.register(Model::new(
        meta("res.company", "rusdoo_test_rel_company"),
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "country_id",
                FieldType::Many2one {
                    comodel: "res.country".into(),
                },
            ),
        ],
    ))
    .unwrap();
    reg.register(Model::new(
        meta("res.partner", "rusdoo_test_rel_partner"),
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "company_id",
                FieldType::Many2one {
                    comodel: "res.company".into(),
                },
            ),
            Field::new("company_name", FieldType::Char { size: None }).related("company_id.name"),
            Field::new("country_code", FieldType::Char { size: None })
                .related("company_id.country_id.code"),
        ],
    ))
    .unwrap();
    reg
}

#[test]
fn a_related_field_is_not_stored_and_is_readonly() {
    let reg = related_registry();
    let partner = reg.get("res.partner").unwrap();
    let field = partner.field("company_name").unwrap();
    assert_eq!(field.related.as_deref(), Some("company_id.name"));
    assert!(!field.stored, "it has no column of its own");
    assert!(field.readonly, "writing it means writing the target");
}

#[test]
fn a_domain_on_a_related_field_becomes_its_path() {
    let reg = related_registry();
    let domain = parse_domain(&json!([["company_name", "=", "Acme"]])).unwrap();
    let (sql, params) = reg
        .search_sql("res.partner", &domain, &SearchOptions::default())
        .unwrap();
    // exactly what ["company_id.name", "=", "Acme"] renders to
    let (direct, direct_params) = reg
        .search_sql(
            "res.partner",
            &parse_domain(&json!([["company_id.name", "=", "Acme"]])).unwrap(),
            &SearchOptions::default(),
        )
        .unwrap();
    assert_eq!(sql, direct);
    assert_eq!(params, direct_params);
    assert!(sql.contains("rusdoo_test_rel_company"), "{sql}");

    // two hops resolve just as well
    let domain = parse_domain(&json!([["country_code", "=", "BR"]])).unwrap();
    let (sql, _) = reg
        .search_sql("res.partner", &domain, &SearchOptions::default())
        .unwrap();
    assert!(sql.contains("rusdoo_test_rel_country"), "{sql}");
}

#[test]
fn a_related_field_cannot_be_written_or_ordered_by() {
    let reg = related_registry();
    let partner = reg.get("res.partner").unwrap();

    let err = partner
        .insert_sql(1, vec![("company_name", json!("Acme"))])
        .unwrap_err()
        .to_string();
    assert!(err.contains("related"), "{err}");
    assert!(err.contains("write the target"), "{err}");

    let err = partner
        .update_sql(1, &[1], vec![("company_name", json!("Acme"))])
        .unwrap_err()
        .to_string();
    assert!(err.contains("related"), "{err}");

    // ORDER BY happens in SQL, where the field has no column
    let opts = SearchOptions {
        order: Some("company_name asc".into()),
        ..SearchOptions::default()
    };
    let err = partner
        .search_sql(&parse_domain(&json!([])).unwrap(), &opts)
        .unwrap_err()
        .to_string();
    assert!(err.contains("not stored"), "{err}");
}

async fn test_pool() -> Option<PgPool> {
    // a schema of this run: these tests create tables directly, and
    // without it two runs touch the same ones
    rusdoo_testing::pool_in("rusdoo_related_test_test_pool")
}

#[tokio::test]
async fn related_fields_read_through_the_path_live() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let reg = related_registry();
    for t in [
        "rusdoo_test_rel_partner",
        "rusdoo_test_rel_company",
        "rusdoo_test_rel_country",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for m in ["res.country", "res.company", "res.partner"] {
        reg.get(m).unwrap().init_table(&pool).await.unwrap();
    }
    let br = reg
        .create(
            &pool,
            "res.country",
            vec![("name", json!("Brasil")), ("code", json!("BR"))],
        )
        .await
        .unwrap();
    let acme = reg
        .create(
            &pool,
            "res.company",
            vec![("name", json!("Acme")), ("country_id", json!(br))],
        )
        .await
        .unwrap();
    let loose = reg
        .create(&pool, "res.company", vec![("name", json!("Sem país"))])
        .await
        .unwrap();
    let ana = reg
        .create(
            &pool,
            "res.partner",
            vec![("name", json!("Ana")), ("company_id", json!(acme))],
        )
        .await
        .unwrap();
    let bob = reg
        .create(
            &pool,
            "res.partner",
            vec![("name", json!("Bob")), ("company_id", json!(loose))],
        )
        .await
        .unwrap();
    let solo = reg
        .create(&pool, "res.partner", vec![("name", json!("Solo"))])
        .await
        .unwrap();

    let rows = reg
        .read(
            &pool,
            "res.partner",
            &[ana, bob, solo],
            &["name", "company_name", "country_code"],
        )
        .await
        .unwrap();
    let by_id: std::collections::HashMap<i64, _> = rows
        .into_iter()
        .map(|r| (r["id"].as_i64().unwrap(), r))
        .collect();
    assert_eq!(by_id[&ana]["company_name"], json!("Acme"));
    assert_eq!(by_id[&ana]["country_code"], json!("BR"), "two hops");
    // a hop that leads nowhere is null, not an error
    assert_eq!(by_id[&bob]["company_name"], json!("Sem país"));
    assert_eq!(by_id[&bob]["country_code"], json!(null));
    assert_eq!(by_id[&solo]["company_name"], json!(null));
    // the record's own fields still read normally alongside
    assert_eq!(by_id[&solo]["name"], json!("Solo"));

    // and a search through the related name finds the same records as
    // the path it stands for
    let found = reg
        .search(
            &pool,
            "res.partner",
            &parse_domain(&json!([["company_name", "=", "Acme"]])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(found, vec![ana]);
    let found = reg
        .search(
            &pool,
            "res.partner",
            &parse_domain(&json!([["country_code", "=", "BR"]])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(found, vec![ana]);
}
