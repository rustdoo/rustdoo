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
    let pool = schema_pool(&url, rusdoo_testing::schema_for("rusdoo_boot_order")).await;
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
    let pool = schema_pool(&url, rusdoo_testing::schema_for("rusdoo_boot_models")).await;
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
    let pool = schema_pool(&url, rusdoo_testing::schema_for("rusdoo_boot_acl")).await;
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
    // the second row of the CSV names the model the way Odoo names it —
    // `model_id:id` pointing at an `ir.model` external id — and grants
    // create, which the first row does not
    assert!(acl
        .check("x_demo.doc", Operation::Create, &[group_id], false)
        .is_ok());
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
    // the same rule written the way Odoo writes it — the model named by
    // a `model_id` ref to an `ir.model` external id — reaches the same
    // model. It covers unlink, which the first rule does not.
    let domain = rules
        .domain_for("x_demo.doc", Operation::Unlink, 42, &[group_id], false)
        .unwrap()
        .expect("the rule named by model_id applies too");
    let (sql, _) = reg
        .search_sql("x_demo.doc", &domain, &SearchOptions::default())
        .unwrap();
    assert!(sql.contains(r#""owner_id" = $1"#), "{sql}");

    // ...not the operations it left out, nor users outside the group
    assert!(rules
        .domain_for("x_demo.doc", Operation::Create, 42, &[group_id], false)
        .unwrap()
        .is_none());
    // and the rule about a model this build does not have was skipped,
    // not loaded: there are no rows of it to leave open
    assert!(!rules.covers("x_demo.absent"));
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
    let pool = schema_pool(&url, rusdoo_testing::schema_for("rusdoo_boot_m2m")).await;
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

/// The install reads an addon's `models/*.py`.
///
/// This is what "an addon published tomorrow runs here" finally means:
/// nobody hands the server the source. The addon is a directory with a
/// manifest, a `models/` package and data files, and the install finds
/// all three — the models before the tables are made, so the data file
/// that follows has somewhere to land.
#[tokio::test(flavor = "multi_thread")]
async fn installs_an_addon_whose_models_are_python() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = schema_pool(&url, rusdoo_testing::schema_for("rusdoo_boot_python")).await;
    let mut registry = Registry::new();
    let mut methods = rusdoo_orm::methods::MethodRegistry::new();
    let mut xml_ids = XmlIds::new();

    // the addon's code, like a Rust module's crate would have done —
    // every boot, before the schema exists
    let declared = rusdoo_modules::installer::register_python_models(
        &[Path::new("tests/fixtures/addons_python")],
        &mut registry,
        &mut methods,
    )
    .expect("the addon's Python declares its models");
    assert_eq!(declared, 2, "both files of models/ ran");

    let report = install_modules(
        &pool,
        &mut registry,
        &[Path::new("tests/fixtures/addons_python")],
        &mut xml_ids,
    )
    .await
    .expect("the addon installs");
    assert_eq!(
        report.modules.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        vec!["demo_python"]
    );

    // the models the addon's Python declared are in the registry, in the
    // order its `models/__init__.py` asked for — the family first,
    // because the plant points at it
    let plant = registry.get("demo.plant").expect("demo.plant is registered");
    assert_eq!(plant.meta.table, "demo_plant");
    assert!(
        matches!(
            &plant.field("family_id").expect("family_id").ty,
            FieldType::Many2one { comodel } if comodel == "demo.plant.family"
        ),
        "the relation crossed"
    );

    // its data file loaded into the tables those models made
    let (name, height): (String, i32) = sqlx::query_as(
        r#"SELECT "name", "height_cm" FROM "demo_plant" ORDER BY "id" LIMIT 1"#,
    )
    .fetch_one(&pool)
    .await
    .expect("the data file's record is there");
    assert_eq!(name, "Bird's Nest Fern");
    assert_eq!(height, 45);

    // the compute the addon wrote runs
    let rows = registry
        .read(
            &pool,
            "demo.plant",
            &registry
                .search(
                    &pool,
                    "demo.plant",
                    &parse_domain(&json!([])).unwrap(),
                    &SearchOptions::default(),
                )
                .await
                .unwrap(),
            &["label"],
        )
        .await
        .unwrap();
    assert_eq!(rows[0]["label"], json!("Bird's Nest Fern (45cm)"));

    // its rule refuses the write that breaks it
    let ids = registry
        .search(
            &pool,
            "demo.plant",
            &parse_domain(&json!([])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    let error = registry
        .write_as(&pool, 1, "demo.plant", &ids, vec![("height_cm", json!(0))])
        .await
        .expect_err("a plant with no height is refused");
    assert!(
        error.to_string().contains("a plant has a height"),
        "the addon's own message: {error}"
    );

    // and its method is reachable by name, like a Rust module's
    let entry = methods
        .get("demo.plant", "action_prune")
        .expect("the addon's method is registered");
    let registry = std::sync::Arc::new(registry);
    let ctx = rusdoo_orm::methods::MethodCtx::new(
        std::sync::Arc::clone(&registry),
        &pool,
        1,
        "demo.plant",
        ids.clone(),
    )
    .with_rest(vec![json!(5)]);
    assert_eq!(
        entry.call(ctx, &[], &serde_json::Map::new()).await.unwrap(),
        json!(40),
        "the method ran and wrote"
    );
}

/// A `.po` puts its translations *on the records*, not only in the
/// catalogue of labels.
///
/// Half of what an addon ships in `i18n/` is not program text but
/// values: the names of countries, of payment methods, of menus, of
/// actions. Those live in columns, and a translation that only reached
/// the label catalogue would leave every one of them in English while
/// the frame around them was translated — which is exactly the mixed
/// screen issue #6 opens with.
#[tokio::test(flavor = "multi_thread")]
async fn a_po_translates_the_records_a_module_shipped() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let pool = schema_pool(&url, rusdoo_testing::schema_for("rusdoo_boot_i18n")).await;
    let mut registry = Registry::new();
    let mut methods = rusdoo_orm::methods::MethodRegistry::new();
    let mut xml_ids = XmlIds::new();
    let addons = Path::new("tests/fixtures/addons_i18n");

    rusdoo_modules::installer::register_python_models(&[addons], &mut registry, &mut methods)
        .expect("the addon's Python declares its models");
    let report = install_modules(&pool, &mut registry, &[addons], &mut xml_ids)
        .await
        .expect("the addon installs");
    assert_eq!(
        report.translations.len_for("pt_BR"),
        5,
        "the catalogue still holds every entry of the file"
    );

    let br = xml_ids
        .get("demo_i18n.country_br")
        .map(|(_, id)| *id)
        .expect("the record loaded");
    let de = xml_ids
        .get("demo_i18n.country_de")
        .map(|(_, id)| *id)
        .expect("the record loaded");

    // the same two records, read by two users in two languages
    let english = registry
        .read_lang(&pool, "demo.country", &[br, de], &["name", "code"], "en_US")
        .await
        .unwrap();
    assert_eq!(english[0]["name"], json!("Brazil"));
    assert_eq!(english[1]["name"], json!("Germany"));

    let portuguese = registry
        .read_lang(&pool, "demo.country", &[br, de], &["name", "code"], "pt_BR")
        .await
        .unwrap();
    assert_eq!(portuguese[0]["name"], json!("Brasil"));
    assert_eq!(portuguese[1]["name"], json!("Alemanha"));

    // the translation did not overwrite the source it was translated
    // from: both languages are on the record at once
    assert_eq!(
        english[0]["name"], json!("Brazil"),
        "the English name survived the Portuguese one landing"
    );

    // a field that is not translatable is left alone, whatever the file
    // says about it — the column holds one value and a `.po` is not
    // allowed to make it hold another
    assert_eq!(portuguese[0]["code"], json!("BR"));

    // and an entry naming a record that does not exist is skipped
    // rather than failing the install: a `.po` outlives the data file it
    // was written against, and a stale line is not a broken module
    assert!(
        xml_ids.get("demo_i18n.country_nonexistent").is_none(),
        "the fixture's stale entry really names nothing"
    );
}
