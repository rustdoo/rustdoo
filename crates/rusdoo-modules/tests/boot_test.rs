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

/// A pool bound to a schema of this test's own. Every boot test installs
/// modules that create the same system tables (ir_model_data and the
/// fixture models), so sharing `public` makes them collide on concurrent
/// DDL — isolation belongs in the fixture, not in a --test-threads=1 rule
/// the runner has to remember.
async fn schema_pool(url: &str, schema: &'static str) -> sqlx::PgPool {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |conn, _meta| {
            Box::pin(async move {
                sqlx::Executor::execute(
                    conn,
                    &*format!("CREATE SCHEMA IF NOT EXISTS {schema}; SET search_path TO {schema}"),
                )
                .await?;
                Ok(())
            })
        })
        .connect_lazy(url)
        .unwrap();
    // start from an empty schema: these tests assert on what an install
    // creates, not on what a previous run left behind
    sqlx::query(&format!(
        "DROP SCHEMA {schema} CASCADE; CREATE SCHEMA {schema}"
    ))
    .execute(&pool)
    .await
    .ok();
    pool
}

#[tokio::test]
async fn boots_fixture_addons_in_dependency_order() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = schema_pool(&url, "rusdoo_boot_order").await;
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
    let pool = schema_pool(&url, "rusdoo_boot_models").await;
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

#[tokio::test]
async fn ir_model_access_csv_loads_into_acl() {
    use rusdoo_orm::access::Operation;

    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = schema_pool(&url, "rusdoo_boot_acl").await;
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "res.groups".into(),
            table: "rusdoo_test_acl_groups".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "x_demo.doc".into(),
            table: "rusdoo_test_acl_doc".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("title", FieldType::Char { size: None }),
            Field::new("owner_id", FieldType::Integer),
        ],
    ))
    .unwrap();
    for t in ["rusdoo_test_acl_groups", "rusdoo_test_acl_doc"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    XmlIds::load(&pool).await.unwrap();
    sqlx::query(r#"DELETE FROM "ir_model_data" WHERE "module" = 'demo_acl'"#)
        .execute(&pool)
        .await
        .unwrap();
    let mut xml_ids = XmlIds::new();
    let report = install_modules(
        &pool,
        &mut reg,
        &[Path::new("tests/fixtures/addons_acl")],
        &mut xml_ids,
    )
    .await
    .unwrap();

    let group_id = xml_ids.get("demo_acl.group_reader").unwrap().1;

    let acl = &report.access;
    assert!(acl
        .check("x_demo.doc", Operation::Read, &[group_id], false)
        .is_ok());
    assert!(acl
        .check("x_demo.doc", Operation::Write, &[group_id], false)
        .is_err());
    assert!(acl
        .check("x_demo.doc", Operation::Read, &[], false)
        .is_err());
    assert!(acl.check("x_demo.doc", Operation::Write, &[], true).is_ok());

    // the ir.rule in the same addon becomes a record rule: it constrains
    // the group it names, for the operations it declares, and nobody else
    let rules = &report.rules;
    assert!(rules.covers("x_demo.doc"));
    let domain = rules
        .domain_for("x_demo.doc", Operation::Read, 42, &[group_id], false)
        .unwrap()
        .expect("the reader group is constrained");
    let (sql, params) = reg
        .search_sql("x_demo.doc", &domain, &SearchOptions::default())
        .unwrap();
    assert!(sql.contains(r#""owner_id" = $1"#), "{sql}");
    assert_eq!(
        params,
        vec![json!(42)],
        "\"user.id\" resolves to the acting user"
    );
    // ...not the operations it left out, nor users outside the group
    assert!(rules
        .domain_for("x_demo.doc", Operation::Unlink, 42, &[group_id], false)
        .unwrap()
        .is_none());
    assert!(rules
        .domain_for("x_demo.doc", Operation::Read, 42, &[], false)
        .unwrap()
        .is_none());
    assert!(rules
        .domain_for("x_demo.doc", Operation::Read, 1, &[group_id], true)
        .unwrap()
        .is_none());

    // the install wrote them down: a server that boots later without
    // re-installing reads the same ACL and the same rules out of the
    // database, instead of coming up locked to the superuser
    let reloaded = rusdoo_orm::access::AccessControl::load(&pool).await.unwrap();
    assert!(reloaded
        .check("x_demo.doc", Operation::Read, &[group_id], false)
        .is_ok());
    assert!(reloaded
        .check("x_demo.doc", Operation::Write, &[group_id], false)
        .is_err());
    let reloaded_rules = rusdoo_orm::rules::RecordRules::load(&pool).await.unwrap();
    assert!(reloaded_rules
        .domain_for("x_demo.doc", Operation::Read, 42, &[group_id], false)
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn command_link_loads_m2m_end_to_end() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = schema_pool(&url, "rusdoo_boot_m2m").await;
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "res.groups".into(),
            table: "rusdoo_test_impl_groups".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "implied_ids",
                FieldType::Many2many {
                    comodel: "res.groups".into(),
                    relation: "rusdoo_test_impl_rel".into(),
                    column1: "gid".into(),
                    column2: "hid".into(),
                },
            ),
        ],
    ))
    .unwrap();
    for t in ["rusdoo_test_impl_rel", "rusdoo_test_impl_groups"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    XmlIds::load(&pool).await.unwrap();
    sqlx::query(r#"DELETE FROM "ir_model_data" WHERE "module" = 'demo_impl'"#)
        .execute(&pool)
        .await
        .unwrap();

    let mut xml_ids = XmlIds::new();
    install_modules(
        &pool,
        &mut reg,
        &[Path::new("tests/fixtures/addons_impl")],
        &mut xml_ids,
    )
    .await
    .unwrap();

    // the whole chain worked: eval Command.link(ref('group_a')) became a
    // relation row linking group_b -> group_a
    let a = xml_ids.get("demo_impl.group_a").unwrap().1;
    let b = xml_ids.get("demo_impl.group_b").unwrap().1;
    let rows = reg
        .read(&pool, "res.groups", &[b], &["implied_ids"])
        .await
        .unwrap();
    assert_eq!(rows[0]["implied_ids"], json!([a]));
}
