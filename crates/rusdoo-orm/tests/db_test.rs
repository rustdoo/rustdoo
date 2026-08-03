//! Live-database tests for the execution layer. They require PostgreSQL;
//! without RUSDOO_TEST_DATABASE_URL set they skip (with a note on stderr).

use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::{parse_domain, Domain};
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
    // a schema of this run: these tests create tables directly, and
    // without it two runs touch the same ones
    rusdoo_testing::pool_in("rusdoo_db_test_test_pool")
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
    // many2one reads as [id, display_name] (name_get)
    assert_eq!(rows[0]["person_id"], json!([bob_person, "Bob"]));

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
    // person_id reads as [id, name] now; take the id
    let old_person = rows[0]["person_id"][0].as_i64().unwrap();
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

#[tokio::test]
async fn x2many_read_returns_ids_live() {
    use rusdoo_orm::registry::Registry;

    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.grp".into(),
            table: "rusdoo_test_grp".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.usr".into(),
            table: "rusdoo_test_usr".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "group_ids",
                FieldType::Many2many {
                    comodel: "rusdoo.test.grp".into(),
                    relation: "rusdoo_test_usr_grp_rel".into(),
                    column1: "usr_id".into(),
                    column2: "grp_id".into(),
                },
            ),
            Field::new(
                "post_ids",
                FieldType::One2many {
                    comodel: "rusdoo.test.post".into(),
                    inverse: "author_id".into(),
                },
            ),
        ],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.post".into(),
            table: "rusdoo_test_post".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("title", FieldType::Char { size: None }),
            Field::new(
                "author_id",
                FieldType::Many2one {
                    comodel: "rusdoo.test.usr".into(),
                },
            ),
        ],
    ))
    .unwrap();

    for t in [
        "rusdoo_test_usr_grp_rel",
        "rusdoo_test_post",
        "rusdoo_test_usr",
        "rusdoo_test_grp",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for m in ["rusdoo.test.grp", "rusdoo.test.usr", "rusdoo.test.post"] {
        reg.get(m).unwrap().init_table(&pool).await.unwrap();
    }

    let g1 = reg
        .get("rusdoo.test.grp")
        .unwrap()
        .create(&pool, vec![("name", json!("admins"))])
        .await
        .unwrap();
    let g2 = reg
        .get("rusdoo.test.grp")
        .unwrap()
        .create(&pool, vec![("name", json!("users"))])
        .await
        .unwrap();
    let ana = reg
        .get("rusdoo.test.usr")
        .unwrap()
        .create(&pool, vec![("name", json!("Ana"))])
        .await
        .unwrap();
    // link ana to both groups via the relation table
    for g in [g1, g2] {
        sqlx::query(r#"INSERT INTO "rusdoo_test_usr_grp_rel" VALUES ($1, $2)"#)
            .bind(ana)
            .bind(g)
            .execute(&pool)
            .await
            .unwrap();
    }
    let p1 = reg
        .get("rusdoo.test.post")
        .unwrap()
        .create(
            &pool,
            vec![("title", json!("Oi")), ("author_id", json!(ana))],
        )
        .await
        .unwrap();

    // read m2m + o2m: both come back as arrays of ids
    let rows = reg
        .read(
            &pool,
            "rusdoo.test.usr",
            &[ana],
            &["name", "group_ids", "post_ids"],
        )
        .await
        .unwrap();
    assert_eq!(rows[0]["name"], json!("Ana"));
    let mut groups: Vec<i64> = rows[0]["group_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    groups.sort();
    assert_eq!(groups, vec![g1, g2]);
    assert_eq!(rows[0]["post_ids"], json!([p1]));
}

#[tokio::test]
async fn x2many_write_commands_live() {
    use rusdoo_orm::registry::Registry;

    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.tag2".into(),
            table: "rusdoo_test_tag2".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.doc2".into(),
            table: "rusdoo_test_doc2".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "tag_ids",
                FieldType::Many2many {
                    comodel: "rusdoo.test.tag2".into(),
                    relation: "rusdoo_test_doc2_tag_rel".into(),
                    column1: "doc_id".into(),
                    column2: "tag_id".into(),
                },
            ),
        ],
    ))
    .unwrap();
    for t in [
        "rusdoo_test_doc2_tag_rel",
        "rusdoo_test_doc2",
        "rusdoo_test_tag2",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for m in ["rusdoo.test.tag2", "rusdoo.test.doc2"] {
        reg.get(m).unwrap().init_table(&pool).await.unwrap();
    }
    let a = reg
        .get("rusdoo.test.tag2")
        .unwrap()
        .create(&pool, vec![("name", json!("a"))])
        .await
        .unwrap();
    let b = reg
        .get("rusdoo.test.tag2")
        .unwrap()
        .create(&pool, vec![("name", json!("b"))])
        .await
        .unwrap();
    let c = reg
        .get("rusdoo.test.tag2")
        .unwrap()
        .create(&pool, vec![("name", json!("c"))])
        .await
        .unwrap();

    // create with Command.link tuples: [[4, a, 0], [4, b, 0]]
    let doc = reg
        .create(
            &pool,
            "rusdoo.test.doc2",
            vec![
                ("name", json!("doc")),
                ("tag_ids", json!([[4, a, 0], [4, b, 0]])),
            ],
        )
        .await
        .unwrap();
    let mut tags: Vec<i64> = reg
        .read(&pool, "rusdoo.test.doc2", &[doc], &["tag_ids"])
        .await
        .unwrap()[0]["tag_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    tags.sort();
    assert_eq!(tags, vec![a, b]);

    // write set([c]) replaces the whole set
    reg.write(
        &pool,
        "rusdoo.test.doc2",
        &[doc],
        vec![("tag_ids", json!([[6, 0, [c]]]))],
    )
    .await
    .unwrap();
    let tags: Vec<i64> = reg
        .read(&pool, "rusdoo.test.doc2", &[doc], &["tag_ids"])
        .await
        .unwrap()[0]["tag_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    assert_eq!(tags, vec![c]);

    // write unlink(c) empties it
    reg.write(
        &pool,
        "rusdoo.test.doc2",
        &[doc],
        vec![("tag_ids", json!([[3, c, 0]]))],
    )
    .await
    .unwrap();
    let tags = reg
        .read(&pool, "rusdoo.test.doc2", &[doc], &["tag_ids"])
        .await
        .unwrap()[0]["tag_ids"]
        .clone();
    assert_eq!(tags, json!([]));
}

#[tokio::test]
async fn o2m_unlink_is_scoped_to_owner() {
    use rusdoo_orm::registry::Registry;

    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.node2".into(),
            table: "rusdoo_test_node2".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "parent_id",
                FieldType::Many2one {
                    comodel: "rusdoo.test.node2".into(),
                },
            ),
            Field::new(
                "child_ids",
                FieldType::One2many {
                    comodel: "rusdoo.test.node2".into(),
                    inverse: "parent_id".into(),
                },
            ),
        ],
    ))
    .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_node2""#)
        .execute(&pool)
        .await
        .unwrap();
    reg.get("rusdoo.test.node2")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();

    let p1 = reg
        .create(&pool, "rusdoo.test.node2", vec![("name", json!("p1"))])
        .await
        .unwrap();
    let p2 = reg
        .create(&pool, "rusdoo.test.node2", vec![("name", json!("p2"))])
        .await
        .unwrap();
    // c belongs to p2
    let c = reg
        .create(
            &pool,
            "rusdoo.test.node2",
            vec![("name", json!("c")), ("parent_id", json!(p2))],
        )
        .await
        .unwrap();

    // p1 tries to unlink c (which it does NOT own) — must be a no-op
    reg.write(
        &pool,
        "rusdoo.test.node2",
        &[p1],
        vec![("child_ids", json!([[3, c, 0]]))],
    )
    .await
    .unwrap();

    // c is still linked to p2 (not severed)
    let rows = reg
        .read(&pool, "rusdoo.test.node2", &[c], &["parent_id"])
        .await
        .unwrap();
    assert_eq!(
        rows[0]["parent_id"],
        json!([p2, "p2"]),
        "cross-owner unlink must not touch c"
    );

    // a bare command tuple (missing outer list) is rejected, not applied
    let err = reg
        .write(
            &pool,
            "rusdoo.test.node2",
            &[p2],
            vec![("child_ids", json!([3, c, 0]))],
        )
        .await;
    assert!(err.is_err(), "bare command tuple must be rejected");
    // and c is STILL linked to p2 (the rejected write changed nothing)
    let rows = reg
        .read(&pool, "rusdoo.test.node2", &[c], &["parent_id"])
        .await
        .unwrap();
    assert_eq!(rows[0]["parent_id"], json!([p2, "p2"]));
}

#[tokio::test]
async fn many2one_reads_as_id_and_name() {
    use rusdoo_orm::registry::Registry;

    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.co3".into(),
            table: "rusdoo_test_co3".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.ct3".into(),
            table: "rusdoo_test_ct3".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "company_id",
                FieldType::Many2one {
                    comodel: "rusdoo.test.co3".into(),
                },
            ),
        ],
    ))
    .unwrap();
    for t in ["rusdoo_test_ct3", "rusdoo_test_co3"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    reg.get("rusdoo.test.co3")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();
    reg.get("rusdoo.test.ct3")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();

    let acme = reg
        .get("rusdoo.test.co3")
        .unwrap()
        .create(&pool, vec![("name", json!("Acme"))])
        .await
        .unwrap();
    let ana = reg
        .create(
            &pool,
            "rusdoo.test.ct3",
            vec![("name", json!("Ana")), ("company_id", json!(acme))],
        )
        .await
        .unwrap();
    let noco = reg
        .create(&pool, "rusdoo.test.ct3", vec![("name", json!("Sem"))])
        .await
        .unwrap();

    // many2one reads as [id, display_name], like Odoo
    let rows = reg
        .read(&pool, "rusdoo.test.ct3", &[ana], &["company_id"])
        .await
        .unwrap();
    assert_eq!(rows[0]["company_id"], json!([acme, "Acme"]));

    // an unset many2one stays null (Odoo returns false)
    let rows = reg
        .read(&pool, "rusdoo.test.ct3", &[noco], &["company_id"])
        .await
        .unwrap();
    assert_eq!(rows[0]["company_id"], serde_json::Value::Null);
}

/// Odoo stamps `create_uid`/`write_uid` with the acting user on every
/// create/write (LOG_ACCESS). The columns aren't exposed as ORM fields
/// yet, so we assert them straight from the row.
#[tokio::test]
async fn create_and_write_stamp_the_acting_user() {
    use rusdoo_orm::registry::Registry;

    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.audit".into(),
            table: "rusdoo_test_audit".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None }).required()],
    ))
    .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_audit""#)
        .execute(&pool)
        .await
        .unwrap();
    reg.get("rusdoo.test.audit")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();

    // create as user 7: both create_uid and write_uid start as 7
    let id = reg
        .create_as(&pool, 7, "rusdoo.test.audit", vec![("name", json!("row"))])
        .await
        .unwrap();
    let (cuid, wuid): (Option<i32>, Option<i32>) = sqlx::query_as(
        r#"SELECT "create_uid", "write_uid" FROM "rusdoo_test_audit" WHERE "id" = $1"#,
    )
    .bind(id as i32)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cuid, Some(7), "create_uid should record the creator");
    assert_eq!(wuid, Some(7), "write_uid on create should equal create_uid");

    // write as user 9: create_uid is preserved, write_uid moves to 9
    reg.write_as(
        &pool,
        9,
        "rusdoo.test.audit",
        &[id],
        vec![("name", json!("edited"))],
    )
    .await
    .unwrap();
    let (cuid, wuid): (Option<i32>, Option<i32>) = sqlx::query_as(
        r#"SELECT "create_uid", "write_uid" FROM "rusdoo_test_audit" WHERE "id" = $1"#,
    )
    .bind(id as i32)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cuid, Some(7), "create_uid must not change on write");
    assert_eq!(wuid, Some(9), "write_uid should record the last writer");
}

/// Linking a child through a one2many command writes the child's inverse
/// FK, so the child's write_uid/write_date must record the acting user —
/// the apply_x2many path bypasses write_conn, so this needs its own probe.
#[tokio::test]
async fn one2many_link_stamps_the_child_writer() {
    use rusdoo_orm::registry::Registry;

    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.o2m_audit".into(),
            table: "rusdoo_test_o2m_audit".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "parent_id",
                FieldType::Many2one {
                    comodel: "rusdoo.test.o2m_audit".into(),
                },
            ),
            Field::new(
                "child_ids",
                FieldType::One2many {
                    comodel: "rusdoo.test.o2m_audit".into(),
                    inverse: "parent_id".into(),
                },
            ),
        ],
    ))
    .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_o2m_audit""#)
        .execute(&pool)
        .await
        .unwrap();
    reg.get("rusdoo.test.o2m_audit")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();

    // parent and child both created by user 3
    let parent = reg
        .create_as(
            &pool,
            3,
            "rusdoo.test.o2m_audit",
            vec![("name", json!("p"))],
        )
        .await
        .unwrap();
    let child = reg
        .create_as(
            &pool,
            3,
            "rusdoo.test.o2m_audit",
            vec![("name", json!("c"))],
        )
        .await
        .unwrap();

    // user 8 links the child into the parent's one2many
    reg.write_as(
        &pool,
        8,
        "rusdoo.test.o2m_audit",
        &[parent],
        vec![("child_ids", json!([[4, child, 0]]))],
    )
    .await
    .unwrap();

    let (wuid,): (Option<i32>,) =
        sqlx::query_as(r#"SELECT "write_uid" FROM "rusdoo_test_o2m_audit" WHERE "id" = $1"#)
            .bind(child as i32)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(wuid, Some(8), "o2m link must stamp the child's write_uid");
}

/// A write to a delegated (`_inherits`) field updates the parent row via
/// the inline delegated UPDATE; that path must stamp the parent's
/// write_uid with the acting user too.
#[tokio::test]
async fn delegated_write_stamps_the_parent_writer() {
    use rusdoo_orm::registry::Registry;

    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.dperson".into(),
            table: "rusdoo_test_dperson".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.daccount".into(),
            table: "rusdoo_test_daccount".into(),
            inherit: vec![],
            inherits: vec![("rusdoo.test.dperson".into(), "person_id".into())],
        },
        vec![
            Field::new("login", FieldType::Char { size: None }),
            Field::new(
                "person_id",
                FieldType::Many2one {
                    comodel: "rusdoo.test.dperson".into(),
                },
            ),
        ],
    ))
    .unwrap();
    for t in ["rusdoo_test_daccount", "rusdoo_test_dperson"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    reg.get("rusdoo.test.dperson")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();
    reg.get("rusdoo.test.daccount")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();

    // create as user 3 (auto-creates the delegated person parent)
    let acc = reg
        .create_as(
            &pool,
            3,
            "rusdoo.test.daccount",
            vec![("login", json!("bob")), ("name", json!("Bob"))],
        )
        .await
        .unwrap();
    // the delegated parent id, to inspect its row directly
    let (person_id,): (i32,) =
        sqlx::query_as(r#"SELECT "person_id" FROM "rusdoo_test_daccount" WHERE "id" = $1"#)
            .bind(acc as i32)
            .fetch_one(&pool)
            .await
            .unwrap();

    // user 8 writes the delegated field `name` (owned by the parent)
    reg.write_as(
        &pool,
        8,
        "rusdoo.test.daccount",
        &[acc],
        vec![("name", json!("Bob II"))],
    )
    .await
    .unwrap();

    let (wuid,): (Option<i32>,) =
        sqlx::query_as(r#"SELECT "write_uid" FROM "rusdoo_test_dperson" WHERE "id" = $1"#)
            .bind(person_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        wuid,
        Some(8),
        "delegated write must stamp the parent's write_uid"
    );
}

/// The LOG_ACCESS columns are exposed as ORM fields on every model:
/// create_uid/write_uid read back as many2one [id, name] to res.users,
/// create_date/write_date as Odoo datetime strings. They are readonly,
/// so a client write to them is rejected.
#[tokio::test]
async fn log_access_columns_are_readable_orm_fields() {
    use rusdoo_orm::registry::Registry;

    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "res.users".into(),
            table: "res_users".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.doc".into(),
            table: "rusdoo_test_doc".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    for t in ["res_users", "rusdoo_test_doc"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    reg.get("res.users")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();
    reg.get("rusdoo.test.doc")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();

    let alice = reg
        .create(&pool, "res.users", vec![("name", json!("Alice"))])
        .await
        .unwrap();
    let doc = reg
        .create_as(&pool, alice, "rusdoo.test.doc", vec![("name", json!("d"))])
        .await
        .unwrap();

    let rows = reg
        .read(
            &pool,
            "rusdoo.test.doc",
            &[doc],
            &["create_uid", "write_uid", "create_date", "write_date"],
        )
        .await
        .unwrap();
    // many2one audit fields resolve to [id, display_name], like name_get
    assert_eq!(rows[0]["create_uid"], json!([alice, "Alice"]));
    assert_eq!(rows[0]["write_uid"], json!([alice, "Alice"]));
    // datetimes come back as non-empty Odoo-format strings
    let cdate = rows[0]["create_date"].as_str().expect("create_date string");
    assert!(cdate.len() >= 19, "datetime string: {cdate:?}");
    assert!(rows[0]["write_date"].is_string());

    // the columns are readonly: a client write must be rejected
    let err = reg
        .write(
            &pool,
            "rusdoo.test.doc",
            &[doc],
            vec![("create_uid", json!(999))],
        )
        .await;
    assert!(
        err.is_err(),
        "writing a readonly LOG_ACCESS field must fail"
    );
}

/// Reading a many2one (here create_uid -> res.users) when the comodel is
/// not registered must degrade to id-only display, never hard-fail the
/// whole read.
#[tokio::test]
async fn m2o_read_without_comodel_registered_is_graceful() {
    use rusdoo_orm::registry::Registry;

    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    // note: res.users is deliberately NOT registered here
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.lonely".into(),
            table: "rusdoo_test_lonely".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_lonely""#)
        .execute(&pool)
        .await
        .unwrap();
    reg.get("rusdoo.test.lonely")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();

    let id = reg
        .create_as(&pool, 5, "rusdoo.test.lonely", vec![("name", json!("x"))])
        .await
        .unwrap();
    let rows = reg
        .read(&pool, "rusdoo.test.lonely", &[id], &["create_uid"])
        .await
        .expect("read must not fail when res.users is absent");
    // graceful fallback: [id, "id"] rather than a resolved display name
    assert_eq!(rows[0]["create_uid"], json!([5, "5"]));
}

/// A readonly field owned by an _inherits parent is write-protected on the
/// delegated path too, not only the local one.
#[tokio::test]
async fn readonly_delegated_field_write_is_rejected() {
    use rusdoo_orm::registry::Registry;

    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.rparent".into(),
            table: "rusdoo_test_rparent".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            // a non-magic readonly field, owned by the parent
            Field::new("code", FieldType::Char { size: None }).readonly(),
        ],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.rchild".into(),
            table: "rusdoo_test_rchild".into(),
            inherit: vec![],
            inherits: vec![("rusdoo.test.rparent".into(), "parent_id".into())],
        },
        vec![Field::new(
            "parent_id",
            FieldType::Many2one {
                comodel: "rusdoo.test.rparent".into(),
            },
        )],
    ))
    .unwrap();
    for t in ["rusdoo_test_rchild", "rusdoo_test_rparent"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    reg.get("rusdoo.test.rparent")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();
    reg.get("rusdoo.test.rchild")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();

    let child = reg
        .create(&pool, "rusdoo.test.rchild", vec![("name", json!("c"))])
        .await
        .unwrap();
    // writing the delegated readonly `code` must be rejected
    let err = reg
        .write(
            &pool,
            "rusdoo.test.rchild",
            &[child],
            vec![("code", json!("HACK"))],
        )
        .await;
    assert!(
        err.is_err(),
        "readonly field must be write-protected through _inherits delegation"
    );
}

/// A Json (jsonb) field round-trips its structured value through the ORM.
#[tokio::test]
async fn json_field_roundtrips() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let model = Model::new(
        ModelMeta {
            name: "rusdoo.test.jdoc".into(),
            table: "rusdoo_test_jdoc".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("data", FieldType::Json),
        ],
    );
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_jdoc""#)
        .execute(&pool)
        .await
        .unwrap();
    model.init_table(&pool).await.unwrap();

    let payload = json!({"k": "v", "n": 3, "nested": [1, 2]});
    let id = model
        .create(&pool, vec![("name", json!("j")), ("data", payload.clone())])
        .await
        .unwrap();
    let rows = model.read(&pool, &[id], &["data"]).await.unwrap();
    assert_eq!(rows[0]["data"], payload);
}

/// Registry with an order/line pair: the shape a form view saves through
/// `Command.CREATE/UPDATE/DELETE` on a one2many.
fn order_registry() -> rusdoo_orm::registry::Registry {
    use rusdoo_orm::registry::Registry;

    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.ordr.line".into(),
            table: "rusdoo_test_ordr_line".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("qty", FieldType::Integer),
            Field::new(
                "order_id",
                FieldType::Many2one {
                    comodel: "rusdoo.test.ordr".into(),
                },
            ),
        ],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.ordr".into(),
            table: "rusdoo_test_ordr".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "line_ids",
                FieldType::One2many {
                    comodel: "rusdoo.test.ordr.line".into(),
                    inverse: "order_id".into(),
                },
            ),
        ],
    ))
    .unwrap();
    reg
}

async fn line_ids_of(reg: &rusdoo_orm::registry::Registry, pool: &PgPool, order: i64) -> Vec<i64> {
    let rows = reg
        .read(pool, "rusdoo.test.ordr", &[order], &["line_ids"])
        .await
        .unwrap();
    let mut ids: Vec<i64> = rows[0]["line_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn o2m_create_update_delete_commands_live() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let reg = order_registry();
    for t in ["rusdoo_test_ordr_line", "rusdoo_test_ordr"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for m in ["rusdoo.test.ordr", "rusdoo.test.ordr.line"] {
        reg.get(m).unwrap().init_table(&pool).await.unwrap();
    }

    // create(0): the lines are created with the order and linked to it,
    // stamped with the acting user like any other write
    let order = reg
        .create_as(
            &pool,
            7,
            "rusdoo.test.ordr",
            vec![
                ("name", json!("SO001")),
                (
                    "line_ids",
                    json!([[0, 0, {"name": "a", "qty": 1}], [0, 0, {"name": "b", "qty": 2}]]),
                ),
            ],
        )
        .await
        .unwrap();
    let lines = line_ids_of(&reg, &pool, order).await;
    assert_eq!(lines.len(), 2, "both lines must be created and linked");
    let rows = reg
        .read(
            &pool,
            "rusdoo.test.ordr.line",
            &lines,
            &["name", "qty", "create_uid"],
        )
        .await
        .unwrap();
    let mut named: Vec<(String, i64)> = rows
        .iter()
        .map(|r| {
            (
                r["name"].as_str().unwrap().to_string(),
                r["qty"].as_i64().unwrap(),
            )
        })
        .collect();
    named.sort();
    assert_eq!(named, vec![("a".into(), 1), ("b".into(), 2)]);
    assert_eq!(
        rows[0]["create_uid"][0],
        json!(7),
        "a line created through a command is stamped with the acting user"
    );

    // update(1): writes onto the linked line, leaving the link alone
    let (line_a, line_b) = (lines[0], lines[1]);
    reg.write(
        &pool,
        "rusdoo.test.ordr",
        &[order],
        vec![("line_ids", json!([[1, line_a, {"qty": 5}]]))],
    )
    .await
    .unwrap();
    let rows = reg
        .read(&pool, "rusdoo.test.ordr.line", &[line_a], &["qty"])
        .await
        .unwrap();
    assert_eq!(rows[0]["qty"], json!(5));
    assert_eq!(line_ids_of(&reg, &pool, order).await, lines);

    // delete(2): the line row itself is gone, not merely unlinked
    reg.write(
        &pool,
        "rusdoo.test.ordr",
        &[order],
        vec![("line_ids", json!([[2, line_b, 0]]))],
    )
    .await
    .unwrap();
    assert_eq!(line_ids_of(&reg, &pool, order).await, vec![line_a]);
    let left: i64 =
        sqlx::query_scalar(r#"SELECT count(*) FROM "rusdoo_test_ordr_line" WHERE "id" = $1"#)
            .bind(line_b as i32)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(left, 0, "delete(2) removes the record, not just the link");
}

#[tokio::test]
async fn o2m_create_command_cannot_reassign_the_link() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let reg = order_registry();
    // own tables: the sibling command test runs in parallel on the others
    for t in ["rusdoo_test_ordr_line2", "rusdoo_test_ordr2"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    let mut reg2 = rusdoo_orm::registry::Registry::new();
    for (name, table) in [
        ("rusdoo.test.ordr.line", "rusdoo_test_ordr_line2"),
        ("rusdoo.test.ordr", "rusdoo_test_ordr2"),
    ] {
        let model = reg.get(name).unwrap();
        reg2.register(Model::new(
            ModelMeta {
                name: name.into(),
                table: table.into(),
                inherit: vec![],
                inherits: vec![],
            },
            model
                .fields()
                .iter()
                .filter(|f| {
                    !["create_uid", "create_date", "write_uid", "write_date"]
                        .contains(&f.name.as_str())
                })
                .cloned()
                .collect(),
        ))
        .unwrap();
    }
    for m in ["rusdoo.test.ordr", "rusdoo.test.ordr.line"] {
        reg2.get(m).unwrap().init_table(&pool).await.unwrap();
    }

    let other = reg2
        .create(&pool, "rusdoo.test.ordr", vec![("name", json!("other"))])
        .await
        .unwrap();
    // the values carry a foreign link; the owner of the command wins
    let order = reg2
        .create(
            &pool,
            "rusdoo.test.ordr",
            vec![
                ("name", json!("mine")),
                (
                    "line_ids",
                    json!([[0, 0, {"name": "l", "order_id": other}]]),
                ),
            ],
        )
        .await
        .unwrap();
    let rows = reg2
        .read(&pool, "rusdoo.test.ordr", &[other], &["line_ids"])
        .await
        .unwrap();
    assert_eq!(
        rows[0]["line_ids"],
        json!([]),
        "a client-supplied inverse must not steal the new line"
    );
    let rows = reg2
        .read(&pool, "rusdoo.test.ordr", &[order], &["line_ids"])
        .await
        .unwrap();
    assert_eq!(rows[0]["line_ids"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn x2many_update_and_delete_are_scoped_to_the_owner() {
    use rusdoo_orm::registry::Registry;

    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.node3".into(),
            table: "rusdoo_test_node3".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "parent_id",
                FieldType::Many2one {
                    comodel: "rusdoo.test.node3".into(),
                },
            ),
            Field::new(
                "child_ids",
                FieldType::One2many {
                    comodel: "rusdoo.test.node3".into(),
                    inverse: "parent_id".into(),
                },
            ),
        ],
    ))
    .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_node3""#)
        .execute(&pool)
        .await
        .unwrap();
    reg.get("rusdoo.test.node3")
        .unwrap()
        .init_table(&pool)
        .await
        .unwrap();

    let p1 = reg
        .create(&pool, "rusdoo.test.node3", vec![("name", json!("p1"))])
        .await
        .unwrap();
    let p2 = reg
        .create(&pool, "rusdoo.test.node3", vec![("name", json!("p2"))])
        .await
        .unwrap();
    // c belongs to p2
    let c = reg
        .create(
            &pool,
            "rusdoo.test.node3",
            vec![("name", json!("c")), ("parent_id", json!(p2))],
        )
        .await
        .unwrap();

    // p1 tries to write onto a child it does not own
    let err = reg
        .write(
            &pool,
            "rusdoo.test.node3",
            &[p1],
            vec![("child_ids", json!([[1, c, {"name": "hijacked"}]]))],
        )
        .await;
    assert!(err.is_err(), "cross-owner update must be refused");
    // ...and p1 tries to delete it
    let err = reg
        .write(
            &pool,
            "rusdoo.test.node3",
            &[p1],
            vec![("child_ids", json!([[2, c, 0]]))],
        )
        .await;
    assert!(err.is_err(), "cross-owner delete must be refused");

    let rows = reg
        .read(&pool, "rusdoo.test.node3", &[c], &["name", "parent_id"])
        .await
        .unwrap();
    assert_eq!(rows[0]["name"], json!("c"), "c must be untouched");
    assert_eq!(rows[0]["parent_id"], json!([p2, "p2"]));

    // the owner itself may do both
    reg.write(
        &pool,
        "rusdoo.test.node3",
        &[p2],
        vec![("child_ids", json!([[1, c, {"name": "renamed"}]]))],
    )
    .await
    .unwrap();
    let rows = reg
        .read(&pool, "rusdoo.test.node3", &[c], &["name"])
        .await
        .unwrap();
    assert_eq!(rows[0]["name"], json!("renamed"));
    reg.write(
        &pool,
        "rusdoo.test.node3",
        &[p2],
        vec![("child_ids", json!([[2, c, 0]]))],
    )
    .await
    .unwrap();
    let left: i64 =
        sqlx::query_scalar(r#"SELECT count(*) FROM "rusdoo_test_node3" WHERE "id" = $1"#)
            .bind(c as i32)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test]
async fn m2m_create_and_delete_commands_live() {
    use rusdoo_orm::registry::Registry;

    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let mut reg = Registry::new();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.tag3".into(),
            table: "rusdoo_test_tag3".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    reg.register(Model::new(
        ModelMeta {
            name: "rusdoo.test.doc3".into(),
            table: "rusdoo_test_doc3".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new(
                "tag_ids",
                FieldType::Many2many {
                    comodel: "rusdoo.test.tag3".into(),
                    relation: "rusdoo_test_doc3_tag_rel".into(),
                    column1: "doc_id".into(),
                    column2: "tag_id".into(),
                },
            ),
        ],
    ))
    .unwrap();
    for t in [
        "rusdoo_test_doc3_tag_rel",
        "rusdoo_test_doc3",
        "rusdoo_test_tag3",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for m in ["rusdoo.test.tag3", "rusdoo.test.doc3"] {
        reg.get(m).unwrap().init_table(&pool).await.unwrap();
    }

    // create(0) on a many2many creates the record AND the relation row
    let doc = reg
        .create(
            &pool,
            "rusdoo.test.doc3",
            vec![
                ("name", json!("doc")),
                ("tag_ids", json!([[0, 0, {"name": "fresh"}]])),
            ],
        )
        .await
        .unwrap();
    let rows = reg
        .read(&pool, "rusdoo.test.doc3", &[doc], &["tag_ids"])
        .await
        .unwrap();
    let tags: Vec<i64> = rows[0]["tag_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    assert_eq!(tags.len(), 1);
    let rows = reg
        .read(&pool, "rusdoo.test.tag3", &tags, &["name"])
        .await
        .unwrap();
    assert_eq!(rows[0]["name"], json!("fresh"));

    // update(1) reaches the linked tag
    reg.write(
        &pool,
        "rusdoo.test.doc3",
        &[doc],
        vec![("tag_ids", json!([[1, tags[0], {"name": "renamed"}]]))],
    )
    .await
    .unwrap();
    let rows = reg
        .read(&pool, "rusdoo.test.tag3", &tags, &["name"])
        .await
        .unwrap();
    assert_eq!(rows[0]["name"], json!("renamed"));

    // delete(2) drops the relation row and the tag itself
    reg.write(
        &pool,
        "rusdoo.test.doc3",
        &[doc],
        vec![("tag_ids", json!([[2, tags[0], 0]]))],
    )
    .await
    .unwrap();
    let rows = reg
        .read(&pool, "rusdoo.test.doc3", &[doc], &["tag_ids"])
        .await
        .unwrap();
    assert_eq!(rows[0]["tag_ids"], json!([]));
    let left: i64 =
        sqlx::query_scalar(r#"SELECT count(*) FROM "rusdoo_test_tag3" WHERE "id" = $1"#)
            .bind(tags[0] as i32)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(left, 0, "delete(2) removes the tag record itself");
    let dangling: i64 = sqlx::query_scalar(
        r#"SELECT count(*) FROM "rusdoo_test_doc3_tag_rel" WHERE "tag_id" = $1"#,
    )
    .bind(tags[0] as i32)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(dangling, 0, "no dangling relation rows are left behind");
}

#[tokio::test]
async fn malformed_create_and_update_commands_are_refused() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let reg = order_registry();
    for t in ["rusdoo_test_ordr_line3", "rusdoo_test_ordr3"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{t}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
    let mut reg3 = rusdoo_orm::registry::Registry::new();
    for (name, table) in [
        ("rusdoo.test.ordr.line", "rusdoo_test_ordr_line3"),
        ("rusdoo.test.ordr", "rusdoo_test_ordr3"),
    ] {
        let model = reg.get(name).unwrap();
        reg3.register(Model::new(
            ModelMeta {
                name: name.into(),
                table: table.into(),
                inherit: vec![],
                inherits: vec![],
            },
            model
                .fields()
                .iter()
                .filter(|f| {
                    !["create_uid", "create_date", "write_uid", "write_date"]
                        .contains(&f.name.as_str())
                })
                .cloned()
                .collect(),
        ))
        .unwrap();
    }
    for m in ["rusdoo.test.ordr", "rusdoo.test.ordr.line"] {
        reg3.get(m).unwrap().init_table(&pool).await.unwrap();
    }

    // a create command whose third slot is not a values object
    let err = reg3
        .create(
            &pool,
            "rusdoo.test.ordr",
            vec![("name", json!("x")), ("line_ids", json!([[0, 0, 0]]))],
        )
        .await;
    assert!(err.is_err(), "create(0) without values must be refused");

    // an update command missing its record id
    let order = reg3
        .create(&pool, "rusdoo.test.ordr", vec![("name", json!("y"))])
        .await
        .unwrap();
    let err = reg3
        .write(
            &pool,
            "rusdoo.test.ordr",
            &[order],
            vec![("line_ids", json!([[1, null, {"qty": 1}]]))],
        )
        .await;
    assert!(err.is_err(), "update(1) without an id must be refused");
}

#[tokio::test]
async fn date_and_datetime_roundtrip_live() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let model = Model::new(
        ModelMeta {
            name: "rusdoo.test.dated".into(),
            table: "rusdoo_test_dated".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("day", FieldType::Date),
            Field::new("moment", FieldType::Datetime),
        ],
    );
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_dated""#)
        .execute(&pool)
        .await
        .unwrap();
    model.init_table(&pool).await.unwrap();

    // the web client sends dates as strings; they must land in the
    // date/timestamp columns and come back in the same wire format
    let id = model
        .create(
            &pool,
            vec![
                ("name", json!("a")),
                ("day", json!("2026-07-31")),
                ("moment", json!("2026-07-31 12:30:00")),
            ],
        )
        .await
        .unwrap();
    let rows = model.read(&pool, &[id], &["day", "moment"]).await.unwrap();
    assert_eq!(rows[0]["day"], json!("2026-07-31"));
    assert_eq!(rows[0]["moment"], json!("2026-07-31 12:30:00"));

    // and they are comparable in a domain
    let older = model
        .create(
            &pool,
            vec![("name", json!("b")), ("day", json!("2025-01-15"))],
        )
        .await
        .unwrap();
    let found = model
        .search(
            &pool,
            &parse_domain(&json!([["day", ">=", "2026-01-01"]])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(found, vec![id]);
    let found = model
        .search(
            &pool,
            &parse_domain(&json!([["day", "in", ["2025-01-15"]]])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(found, vec![older]);
    let unset = model
        .create(&pool, vec![("name", json!("c"))])
        .await
        .unwrap();
    let found = model
        .search(
            &pool,
            &parse_domain(&json!([["day", "=", false]])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(found, vec![unset]);

    // ...and writable after the fact
    model
        .write(&pool, &[id], vec![("day", json!("2026-08-02"))])
        .await
        .unwrap();
    let rows = model.read(&pool, &[id], &["day"]).await.unwrap();
    assert_eq!(rows[0]["day"], json!("2026-08-02"));
}

#[tokio::test]
async fn create_applies_declared_defaults_live() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let model = Model::new(
        ModelMeta {
            name: "rusdoo.test.defaulted".into(),
            table: "rusdoo_test_defaulted".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            Field::new("color", FieldType::Integer).default_value(json!(7)),
            Field::new("note", FieldType::Text),
        ],
    );
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_defaulted""#)
        .execute(&pool)
        .await
        .unwrap();
    model.init_table(&pool).await.unwrap();

    // a field the create leaves out gets its default
    let id = model
        .create(&pool, vec![("name", json!("a"))])
        .await
        .unwrap();
    let rows = model
        .read(&pool, &[id], &["active", "color", "note"])
        .await
        .unwrap();
    assert_eq!(rows[0]["active"], json!(true));
    assert_eq!(rows[0]["color"], json!(7));
    // a field without a default stays unset
    assert_eq!(rows[0]["note"], json!(null));

    // a value the caller passed always wins — including an explicit null,
    // which says "unset on purpose"
    let id = model
        .create(
            &pool,
            vec![
                ("name", json!("b")),
                ("color", json!(1)),
                ("active", json!(null)),
            ],
        )
        .await
        .unwrap();
    let rows = model
        .read(&pool, &[id], &["active", "color"])
        .await
        .unwrap();
    assert_eq!(rows[0]["color"], json!(1));
    // the read path returns unset columns as null (Odoo would say False)
    assert_eq!(
        rows[0]["active"],
        json!(null),
        "an explicit null is kept, not replaced by the default"
    );

    // a write never re-applies defaults
    model
        .write(&pool, &[id], vec![("name", json!("b2"))])
        .await
        .unwrap();
    let rows = model.read(&pool, &[id], &["color"]).await.unwrap();
    assert_eq!(rows[0]["color"], json!(1));
}

#[tokio::test]
async fn archived_records_stay_out_of_searches_live() {
    let Some(pool) = test_pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let model = Model::new(
        ModelMeta {
            name: "rusdoo.test.archivable".into(),
            table: "rusdoo_test_archivable".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
        ],
    );
    sqlx::query(r#"DROP TABLE IF EXISTS "rusdoo_test_archivable""#)
        .execute(&pool)
        .await
        .unwrap();
    model.init_table(&pool).await.unwrap();

    let live = model
        .create(&pool, vec![("name", json!("live"))])
        .await
        .unwrap();
    let archived = model
        .create(
            &pool,
            vec![("name", json!("archived")), ("active", json!(false))],
        )
        .await
        .unwrap();

    // the default search hides the archived record
    let found = model
        .search(&pool, &Domain::True, &SearchOptions::default())
        .await
        .unwrap();
    assert_eq!(found, vec![live]);

    // active_test off brings it back — the "Archived" filter of a list view
    let found = model
        .search(
            &pool,
            &Domain::True,
            &SearchOptions {
                active_test: false,
                ..SearchOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(found, vec![live, archived]);

    // a domain that already speaks about `active` decides for itself
    let found = model
        .search(
            &pool,
            &parse_domain(&json!([["active", "=", false]])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(found, vec![archived]);

    // ...even nested under an operator
    let found = model
        .search(
            &pool,
            &parse_domain(&json!(["|", ["active", "=", false], ["name", "=", "live"]])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(found, vec![live, archived]);

    // a model without the field is untouched by any of this
    let plain = Model::new(
        ModelMeta {
            name: "rusdoo.test.plain".into(),
            table: "rusdoo_test_plain".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![Field::new("name", FieldType::Char { size: None })],
    );
    let (sql, _) = plain
        .search_sql(&Domain::True, &SearchOptions::default())
        .unwrap();
    assert!(!sql.contains("active"), "{sql}");
}
