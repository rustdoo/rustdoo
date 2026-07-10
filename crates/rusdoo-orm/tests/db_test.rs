//! Live-database tests for the execution layer. They require PostgreSQL;
//! without RUSDOO_TEST_DATABASE_URL set they skip (with a note on stderr).

use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::db::connect;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use serde_json::json;
use sqlx::PgPool;

fn test_model() -> Model {
    Model::new(
        ModelMeta {
            name: "rusdoo.test.partner".into(),
            table: "rusdoo_test_partner".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }).required(),
            Field::new("active", FieldType::Boolean),
            Field::new("color", FieldType::Integer),
        ],
    )
}

async fn test_pool() -> Option<PgPool> {
    let url = std::env::var("RUSDOO_TEST_DATABASE_URL").ok()?;
    Some(
        connect(&url)
            .await
            .expect("failed to connect to test database"),
    )
}

#[tokio::test]
async fn full_crud_roundtrip_against_postgres() {
    // Arrange: live database + fresh table
    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let model = test_model();
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_partner""#)
        .execute(&pool)
        .await
        .unwrap();
    model.init_table(&pool).await.unwrap();

    // Act + Assert: create returns sequential ids
    let id = model
        .create(
            &pool,
            vec![
                ("name", json!("Gemini")),
                ("color", json!(7)),
                ("active", json!(true)),
            ],
        )
        .await
        .unwrap();
    assert!(id >= 1);
    let id2 = model
        .create(
            &pool,
            vec![("name", json!("Zed")), ("active", json!(false))],
        )
        .await
        .unwrap();

    // search: comparison domain
    let dom = parse_domain(&json!([["color", ">", 3]])).unwrap();
    let found = model
        .search(&pool, &dom, &SearchOptions::default())
        .await
        .unwrap();
    assert_eq!(found, vec![id]);

    // search: falsy semantics live — active = false matches false AND unset
    let dom = parse_domain(&json!([["active", "=", false]])).unwrap();
    let found = model
        .search(&pool, &dom, &SearchOptions::default())
        .await
        .unwrap();
    assert_eq!(found, vec![id2]);

    // read
    let rows = model
        .read(&pool, &[id], &["name", "color", "active"])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], json!("Gemini"));
    assert_eq!(rows[0]["color"], json!(7));
    assert_eq!(rows[0]["active"], json!(true));

    // read: unset column comes back as JSON null, not an error
    let rows = model.read(&pool, &[id2], &["color"]).await.unwrap();
    assert_eq!(rows[0]["color"], serde_json::Value::Null);

    // write
    let updated = model
        .write(&pool, &[id], vec![("color", json!(9))])
        .await
        .unwrap();
    assert_eq!(updated, 1);
    let rows = model.read(&pool, &[id], &["color"]).await.unwrap();
    assert_eq!(rows[0]["color"], json!(9));

    // unlink
    let deleted = model.unlink(&pool, &[id, id2]).await.unwrap();
    assert_eq!(deleted, 2);
    let dom = parse_domain(&json!([])).unwrap();
    let remaining = model
        .search(&pool, &dom, &SearchOptions::default())
        .await
        .unwrap();
    assert!(remaining.is_empty());
}

#[tokio::test]
async fn many2one_path_search_live() {
    use rusdoo_orm::registry::Registry;

    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.company".into(),
            table: "rusdoo_test_company".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.contact".into(),
            table: "rusdoo_test_contact".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "company_id",
                FieldType::Many2one {
                    comodel: "rusdoo.test.company".into(),
                },
            ),
        ],
    ))
    .unwrap();

    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_contact""#)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_company""#)
        .execute(&pool)
        .await
        .unwrap();
    let company = reg.get("rusdoo.test.company").unwrap();
    let contact = reg.get("rusdoo.test.contact").unwrap();
    company.init_table(&pool).await.unwrap();
    contact.init_table(&pool).await.unwrap();

    let acme = company
        .create(&pool, vec![("name", json!("Acme"))])
        .await
        .unwrap();
    let globex = company
        .create(&pool, vec![("name", json!("Globex"))])
        .await
        .unwrap();
    let ana = contact
        .create(
            &pool,
            vec![("name", json!("Ana")), ("company_id", json!(acme))],
        )
        .await
        .unwrap();
    contact
        .create(
            &pool,
            vec![("name", json!("Bia")), ("company_id", json!(globex))],
        )
        .await
        .unwrap();
    contact
        .create(&pool, vec![("name", json!("Céu"))])
        .await
        .unwrap();

    // dotted path: contacts whose company is named Acme
    let dom = parse_domain(&json!([["company_id.name", "=", "Acme"]])).unwrap();
    let found = reg
        .search(
            &pool,
            "rusdoo.test.contact",
            &dom,
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(found, vec![ana]);

    // not any: company is not Acme, or no company at all
    let dom = parse_domain(&json!([["company_id", "not any", [["name", "=", "Acme"]]]])).unwrap();
    let found = reg
        .search(
            &pool,
            "rusdoo.test.contact",
            &dom,
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(found.len(), 2);
}

#[tokio::test]
async fn hierarchy_and_m2m_live() {
    use rusdoo_orm::registry::Registry;

    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.node".into(),
            table: "rusdoo_test_node".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "parent_id",
                FieldType::Many2one {
                    comodel: "rusdoo.test.node".into(),
                },
            ),
            Field::new(
                "tag_ids",
                FieldType::Many2many {
                    comodel: "rusdoo.test.tag".into(),
                    relation: "rusdoo_test_node_tag_rel".into(),
                    column1: "node_id".into(),
                    column2: "tag_id".into(),
                },
            ),
        ],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.tag".into(),
            table: "rusdoo_test_tag".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();

    for t in [
        "rusdoo_test_node_tag_rel",
        "rusdoo_test_node",
        "rusdoo_test_tag",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    let node = reg.get("rusdoo.test.node").unwrap();
    let tag = reg.get("rusdoo.test.tag").unwrap();
    node.init_table(&pool).await.unwrap();
    tag.init_table(&pool).await.unwrap();

    let a = node
        .create(&pool, vec![("name", json!("a"))])
        .await
        .unwrap();
    let b = node
        .create(&pool, vec![("name", json!("b")), ("parent_id", json!(a))])
        .await
        .unwrap();
    let c = node
        .create(&pool, vec![("name", json!("c")), ("parent_id", json!(b))])
        .await
        .unwrap();
    let d = node
        .create(&pool, vec![("name", json!("d"))])
        .await
        .unwrap();

    // child_of: the root and all its descendants
    let dom = parse_domain(&json!([["id", "child_of", a]])).unwrap();
    let mut found = reg
        .search(&pool, "rusdoo.test.node", &dom, &SearchOptions::default())
        .await
        .unwrap();
    found.sort();
    assert_eq!(found, vec![a, b, c]);

    // child_of on the self-referential m2o must behave like child_of on id
    let dom = parse_domain(&json!([["parent_id", "child_of", a]])).unwrap();
    let mut found = reg
        .search(&pool, "rusdoo.test.node", &dom, &SearchOptions::default())
        .await
        .unwrap();
    found.sort();
    assert_eq!(found, vec![a, b, c]);

    // parent_of: the leaf and all its ancestors
    let dom = parse_domain(&json!([["id", "parent_of", c]])).unwrap();
    let mut found = reg
        .search(&pool, "rusdoo.test.node", &dom, &SearchOptions::default())
        .await
        .unwrap();
    found.sort();
    assert_eq!(found, vec![a, b, c]);

    // m2m: init_table created the relation table; link d to a tag
    let vip = tag
        .create(&pool, vec![("name", json!("vip"))])
        .await
        .unwrap();
    sqlx::query(r#"INSERT INTO "rusdoo_test_node_tag_rel" VALUES ($1, $2)"#)
        .bind(d)
        .bind(vip)
        .execute(&pool)
        .await
        .unwrap();

    let dom = parse_domain(&json!([["tag_ids", "any", [["name", "=", "vip"]]]])).unwrap();
    let found = reg
        .search(&pool, "rusdoo.test.node", &dom, &SearchOptions::default())
        .await
        .unwrap();
    assert_eq!(found, vec![d]);

    let dom = parse_domain(&json!([["tag_ids", "in", [vip]]])).unwrap();
    let found = reg
        .search(&pool, "rusdoo.test.node", &dom, &SearchOptions::default())
        .await
        .unwrap();
    assert_eq!(found, vec![d]);
}

#[tokio::test]
async fn inherits_delegation_live() {
    use rusdoo_orm::registry::Registry;

    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.person".into(),
            table: "rusdoo_test_person".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("email", FieldType::Char { size: None }),
        ],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.account".into(),
            table: "rusdoo_test_account".into(),
            inherit: vec![],
            inherits: vec![("rusdoo.test.person".into(), "person_id".into())],
        },
        vec![
            Field::new("login", FieldType::Char { size: None }).required(),
            Field::new(
                "person_id",
                FieldType::Many2one {
                    comodel: "rusdoo.test.person".into(),
                },
            )
            .required(),
        ],
    ))
    .unwrap();

    for t in ["rusdoo_test_account", "rusdoo_test_person"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    reg.get("rusdoo.test.person")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();
    reg.get("rusdoo.test.account")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();

    // create with mixed values: the person row is created first
    let uid = reg
        .create(
            &pool,
            "rusdoo.test.account",
            vec![
                ("login", json!("ana")),
                ("name", json!("Ana")),
                ("email", json!("a@x")),
            ],
        )
        .await
        .unwrap();

    // read joins the parent transparently
    let rows = reg
        .read(
            &pool,
            "rusdoo.test.account",
            &[uid],
            &["login", "name", "email"],
        )
        .await
        .unwrap();
    assert_eq!(rows[0]["login"], json!("ana"));
    assert_eq!(rows[0]["name"], json!("Ana"));
    assert_eq!(rows[0]["email"], json!("a@x"));

    // search on a delegated field
    let dom = parse_domain(&json!([["name", "=", "Ana"]])).unwrap();
    let found = reg
        .search(
            &pool,
            "rusdoo.test.account",
            &dom,
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(found, vec![uid]);

    // write through delegation updates the parent row
    reg.write(
        &pool,
        "rusdoo.test.account",
        &[uid],
        vec![("name", json!("Ana Maria"))],
    )
    .await
    .unwrap();
    let rows = reg
        .read(&pool, "rusdoo.test.account", &[uid], &["name"])
        .await
        .unwrap();
    assert_eq!(rows[0]["name"], json!("Ana Maria"));

    // creating with an explicit link reuses that parent: no orphan row,
    // and delegated values land on the existing parent
    let bob_person = reg
        .create(&pool, "rusdoo.test.person", vec![("name", json!("Bob"))])
        .await
        .unwrap();
    let persons_before: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "rusdoo_test_person""#)
        .fetch_one(&pool)
        .await
        .unwrap();
    let uid2 = reg
        .create(
            &pool,
            "rusdoo.test.account",
            vec![
                ("login", json!("bob")),
                ("person_id", json!(bob_person)),
                ("email", json!("b@x")),
            ],
        )
        .await
        .unwrap();
    let persons_after: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "rusdoo_test_person""#)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(persons_before, persons_after, "no orphan parent row");
    let rows = reg
        .read(
            &pool,
            "rusdoo.test.account",
            &[uid2],
            &["email", "person_id"],
        )
        .await
        .unwrap();
    assert_eq!(rows[0]["email"], json!("b@x"));
    assert_eq!(rows[0]["person_id"], json!(bob_person));

    // a failed create rolls the parent rows back (login is required)
    let failed = reg
        .create(&pool, "rusdoo.test.account", vec![("name", json!("Ghost"))])
        .await;
    assert!(failed.is_err());
    let persons_final: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "rusdoo_test_person""#)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        persons_after, persons_final,
        "failed create left no orphans"
    );

    // reassigning the link and writing a delegated field in one call:
    // the delegated value goes to the parent linked BEFORE the call
    let rows = reg
        .read(&pool, "rusdoo.test.account", &[uid], &["person_id"])
        .await
        .unwrap();
    let old_person = rows[0]["person_id"].as_i64().unwrap();
    reg.write(
        &pool,
        "rusdoo.test.account",
        &[uid],
        vec![("person_id", json!(bob_person)), ("name", json!("Renamed"))],
    )
    .await
    .unwrap();
    let rows = reg
        .read(&pool, "rusdoo.test.person", &[old_person], &["name"])
        .await
        .unwrap();
    assert_eq!(
        rows[0]["name"],
        json!("Renamed"),
        "old parent got the delegated write"
    );
    let rows = reg
        .read(&pool, "rusdoo.test.account", &[uid], &["name"])
        .await
        .unwrap();
    assert_eq!(
        rows[0]["name"],
        json!("Bob"),
        "account now reads through the new parent"
    );
}
