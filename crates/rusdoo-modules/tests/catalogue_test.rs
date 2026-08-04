//! The catalogue against the database: what the Apps screen reads, and
//! the rule that keeps a command line from rewriting history.

use rusdoo_modules::catalogue;
use rusdoo_modules::manifest::Manifest;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::registry::Registry;
use serde_json::{json, Value};
use std::collections::HashSet;

fn manifest(name: &str, category: &str, depends: &[&str]) -> Manifest {
    Manifest {
        name: name.into(),
        display_name: format!("O módulo {name}"),
        version: "19.0.1.0".into(),
        category: category.into(),
        summary: format!("resumo de {name}"),
        depends: depends.iter().map(|d| (*d).to_string()).collect(),
        data: Vec::new(),
        assets: Vec::new(),
        installable: true,
        auto_install: false,
        path: Default::default(),
    }
}

fn set(names: &[&str]) -> HashSet<String> {
    names.iter().map(|n| (*n).to_string()).collect()
}

async fn row(registry: &Registry, pool: &sqlx::PgPool, name: &str) -> Value {
    let ids = registry
        .search(
            pool,
            "ir.module.module",
            &parse_domain(&json!([["name", "=", name]])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .expect("the search runs");
    assert_eq!(ids.len(), 1, "one row for {name}: {ids:?}");
    Value::Object(
        registry
            .read(
                pool,
                "ir.module.module",
                &ids,
                &["name", "shortdesc", "state", "has_code", "application", "dependencies_id"],
            )
            .await
            .expect("the module reads")
            .into_iter()
            .next()
            .unwrap(),
    )
}

#[tokio::test]
async fn the_catalogue_is_the_disk_and_the_database_together_live() {
    let Some(pool) = rusdoo_testing::pool_in("rusdoo_catalogue") else {
        return;
    };
    let registry = rusdoo_base::registry().expect("base registers");
    for table in ["ir_module_module_dependency", "ir_module_module"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}" CASCADE"#))
            .execute(&pool)
            .await
            .unwrap();
    }
    registry.init_tables(&pool).await.expect("the tables are made");

    let on_disk = vec![
        manifest("base", "Oculto", &[]),
        manifest("sale", "Vendas", &["base", "product"]),
        manifest("fleet", "Frota", &["base"]),
    ];
    // this boot loaded two of them, and the binary carries the code of
    // those two only
    catalogue::sync(
        &registry,
        &pool,
        &on_disk,
        &set(&["base", "sale"]),
        &set(&["base", "sale"]),
    )
    .await
    .expect("the catalogue syncs");

    let sale = row(&registry, &pool, "sale").await;
    assert_eq!(sale["state"], json!("installed"), "{sale}");
    assert_eq!(sale["shortdesc"], json!("O módulo sale"), "{sale}");
    assert_eq!(sale["application"], json!(true), "{sale}");
    assert_eq!(
        sale["dependencies_id"].as_array().map(Vec::len),
        Some(2),
        "as duas dependências viraram linhas: {sale}"
    );

    // on disk, never installed, and this build could not run it anyway —
    // which is what the screen has to say before somebody clicks
    let fleet = row(&registry, &pool, "fleet").await;
    assert_eq!(fleet["state"], json!("uninstalled"), "{fleet}");
    assert_eq!(fleet["has_code"], json!(false), "{fleet}");

    // a technical module is not an application
    let base = row(&registry, &pool, "base").await;
    assert_eq!(base["application"], json!(false), "{base}");

    // ── the rule that matters ────────────────────────────────────────
    // a narrower boot does not uninstall anything: what is in this
    // database is still in it, whatever this command line loaded
    catalogue::sync(&registry, &pool, &on_disk, &set(&["base"]), &set(&["base", "sale"]))
        .await
        .expect("the second sync runs");
    let sale = row(&registry, &pool, "sale").await;
    assert_eq!(
        sale["state"],
        json!("installed"),
        "um boot mais estreito reescreveu a história: {sale}"
    );

    // ── the button, and the boot that honours it ─────────────────────
    let ids = registry
        .search(
            &pool,
            "ir.module.module",
            &parse_domain(&json!([["name", "=", "fleet"]])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    registry
        .write(&pool, "ir.module.module", &ids, vec![("state", json!("to install"))])
        .await
        .expect("the request is recorded");

    // a boot that could not honour it yet leaves the request standing
    catalogue::sync(&registry, &pool, &on_disk, &set(&["base"]), &set(&["base", "sale"]))
        .await
        .unwrap();
    assert_eq!(
        row(&registry, &pool, "fleet").await["state"],
        json!("to install")
    );
    // and the boot reads it, which is how the request reaches the server
    assert_eq!(catalogue::wanted_modules(&pool).await, vec!["fleet".to_string()]);

    // the boot that loads it settles the matter
    catalogue::sync(
        &registry,
        &pool,
        &on_disk,
        &set(&["base", "fleet"]),
        &set(&["base", "sale", "fleet"]),
    )
    .await
    .unwrap();
    assert_eq!(
        row(&registry, &pool, "fleet").await["state"],
        json!("installed")
    );
    assert!(catalogue::wanted_modules(&pool).await.is_empty());

    // ── an addon that left the disk leaves the catalogue ─────────────
    catalogue::sync(
        &registry,
        &pool,
        &on_disk[..2],
        &set(&["base"]),
        &set(&["base", "sale"]),
    )
    .await
    .unwrap();
    let left = registry
        .search(
            &pool,
            "ir.module.module",
            &parse_domain(&json!([["name", "=", "fleet"]])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    assert!(left.is_empty(), "o módulo sumiu do disco e ficou na tela");

    sqlx::query("DROP SCHEMA IF EXISTS rusdoo_catalogue CASCADE")
        .execute(&pool)
        .await
        .ok();
}

/// The database with no catalogue at all — a first boot — wants nothing,
/// and asking is not the reason it fails.
#[tokio::test]
async fn a_first_boot_has_nothing_to_honour_live() {
    let Some(pool) = rusdoo_testing::pool_in("rusdoo_catalogue_empty") else {
        return;
    };
    sqlx::query(r#"DROP TABLE IF EXISTS "ir_module_module" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
    assert!(catalogue::wanted_modules(&pool).await.is_empty());
}
