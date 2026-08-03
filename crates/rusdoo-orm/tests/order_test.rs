//! `_order`: a search that asks for no order gets the model's, not the
//! one the
//! PostgreSQL quiser dar.

use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::Domain;
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

fn char(name: &str) -> Field {
    Field::new(name, FieldType::Char { size: None })
}

async fn pool() -> Option<PgPool> {
    // a schema of this run: these tests create tables directly, and
    // without it two runs touch the same ones
    rusdoo_testing::pool_in("rusdoo_order_test_pool")
}

#[test]
fn a_model_without_an_order_falls_back_to_id() {
    let model = Model::new(meta("x.y", "x_y"), vec![char("name")]);
    assert_eq!(model.order(), "id");
    assert_eq!(model.ordered("name, id").order(), "name, id");
}

#[test]
fn an_inherit_child_keeps_the_order_its_parent_chose() {
    let mut reg = Registry::new();
    reg.register(Model::new(meta("x.y", "x_y"), vec![char("name")]).ordered("name desc, id"))
        .unwrap();
    // a module that only adds a field does not change the list's order
    reg.register(Model::new(
        ModelMeta {
            inherit: vec!["x.y".into()],
            ..meta("x.y", "x_y")
        },
        vec![char("apelido")],
    ))
    .unwrap();
    assert_eq!(reg.get("x.y").unwrap().order(), "name desc, id");

    // mas um que declara a sua ganha
    reg.register(Model::new(
        ModelMeta {
            inherit: vec!["x.y".into()],
            ..meta("x.y", "x_y")
        },
        vec![char("outro")],
    ))
    .unwrap();
    let mut reg2 = Registry::new();
    reg2.register(Model::new(meta("x.y", "x_y"), vec![char("name")]).ordered("name desc, id"))
        .unwrap();
    reg2.register(
        Model::new(
            ModelMeta {
                inherit: vec!["x.y".into()],
                ..meta("x.y", "x_y")
            },
            vec![char("apelido")],
        )
        .ordered("id desc"),
    )
    .unwrap();
    assert_eq!(reg2.get("x.y").unwrap().order(), "id desc");
}

#[test]
fn an_order_over_a_field_that_does_not_exist_is_refused_when_it_runs() {
    let model = Model::new(meta("x.y", "x_y"), vec![char("name")]).ordered("inexistente desc");
    let error = model
        .search_sql(&Domain::True, &SearchOptions::default())
        .expect_err("um _order só pode nomear campos do modelo");
    assert!(error.to_string().contains("inexistente"), "{error}");
}

#[tokio::test]
async fn a_search_with_no_order_comes_back_in_the_models_order_live() {
    let Some(pool) = pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let table = "rusdoo_test_order";
    sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}""#))
        .execute(&pool)
        .await
        .unwrap();

    let mut reg = Registry::new();
    reg.register(Model::new(meta("x.order", table), vec![char("name")]).ordered("name, id"))
        .unwrap();
    reg.get("x.order").unwrap().init_table(&pool).await.unwrap();

    for name in ["Clara", "Ana", "Beto"] {
        reg.create(&pool, "x.order", vec![("name", json!(name))])
            .await
            .unwrap();
    }
    // the insertion order is not the model's order, and insertion is
    // what the database would hand back if nobody said anything
    let ids = reg
        .search(&pool, "x.order", &Domain::True, &SearchOptions::default())
        .await
        .unwrap();
    let names: Vec<String> = reg
        .read(&pool, "x.order", &ids, &["name"])
        .await
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["Ana", "Beto", "Clara"], "a ordem é a do modelo");

    // e quem pede uma ordem continua mandando
    let ids = reg
        .search(
            &pool,
            "x.order",
            &Domain::True,
            &SearchOptions {
                order: Some("name desc".into()),
                ..SearchOptions::default()
            },
        )
        .await
        .unwrap();
    let names: Vec<String> = reg
        .read(&pool, "x.order", &ids, &["name"])
        .await
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["Clara", "Beto", "Ana"], "o pedido explícito vence");

    sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}""#))
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn the_lines_of_a_record_come_back_in_the_comodels_order_live() {
    let Some(pool) = pool().await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    for table in ["rusdoo_test_order_line", "rusdoo_test_order_doc"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}""#))
            .execute(&pool)
            .await
            .unwrap();
    }

    let mut reg = Registry::new();
    reg.register(Model::new(
        meta("x.doc", "rusdoo_test_order_doc"),
        vec![
            char("name"),
            Field::new(
                "line_ids",
                FieldType::One2many {
                    comodel: "x.doc.line".into(),
                    inverse: "doc_id".into(),
                },
            ),
        ],
    ))
    .unwrap();
    reg.register(
        Model::new(
            meta("x.doc.line", "rusdoo_test_order_line"),
            vec![
                char("name"),
                Field::new("sequence", FieldType::Integer),
                Field::new(
                    "doc_id",
                    FieldType::Many2one {
                        comodel: "x.doc".into(),
                    },
                ),
            ],
        )
        .ordered("sequence, id"),
    )
    .unwrap();
    for model in reg.models() {
        model.init_table(&pool).await.unwrap();
    }

    // the lines go in out of order, as they do when somebody drags
    // uma linha nova para o meio do documento
    let doc = reg
        .create(
            &pool,
            "x.doc",
            vec![
                ("name", json!("pedido")),
                (
                    "line_ids",
                    json!([
                        [0, 0, {"name": "terceira", "sequence": 30}],
                        [0, 0, {"name": "primeira", "sequence": 10}],
                        [0, 0, {"name": "segunda", "sequence": 20}],
                    ]),
                ),
            ],
        )
        .await
        .unwrap();

    let rows = reg
        .read(&pool, "x.doc", &[doc], &["line_ids"])
        .await
        .unwrap();
    let line_ids: Vec<i64> = rows[0]["line_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    let names: Vec<String> = reg
        .read(&pool, "x.doc.line", &line_ids, &["name"])
        .await
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        vec!["primeira", "segunda", "terceira"],
        "as linhas vêm na ordem do comodelo, não na de criação"
    );

    for table in ["rusdoo_test_order_line", "rusdoo_test_order_doc"] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}""#))
            .execute(&pool)
            .await
            .unwrap();
    }
}
