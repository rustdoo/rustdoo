//! Defaults the ORM runs rather than reads — Odoo's callable
//! `default=`.

use rusdoo_orm::defaults;
use rusdoo_orm::fields::{DefaultCtx, Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use rusdoo_core::RusdooError;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::future::Future;
use std::pin::Pin;

fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.into(),
        table: table.into(),
        inherit: vec![],
        inherits: vec![],
    }
}

/// A default that reads from the database: proof the function has the
/// create's own connection, not only the clock.
fn last_partner_name(
    ctx: DefaultCtx<'_>,
) -> Pin<Box<dyn Future<Output = Result<Value, RusdooError>> + Send + '_>> {
    Box::pin(async move {
        let found: Option<Option<String>> = sqlx::query_scalar(
            r#"SELECT "name" FROM "rusdoo_test_default_src" ORDER BY "id" DESC LIMIT 1"#,
        )
        .fetch_optional(&mut *ctx.conn)
        .await
        .ok()
        .flatten();
        Ok(match found.flatten() {
            Some(name) => Value::from(name),
            None => Value::Null,
        })
    })
}

async fn fixture(schema: &str) -> Option<(Registry, PgPool)> {
    let url = std::env::var("RUSDOO_TEST_DATABASE_URL").ok()?;
    let owned = schema.to_string();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .after_connect(move |conn, _meta| {
            let schema = owned.clone();
            Box::pin(async move {
                sqlx::Executor::execute(
                    &mut *conn,
                    &format!("CREATE SCHEMA IF NOT EXISTS {schema}; SET search_path TO {schema}")
                        as &str,
                )
                .await?;
                Ok(())
            })
        })
        .connect_lazy(&url)
        .expect("test database");
    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(&pool)
        .await
        .unwrap();

    let mut reg = Registry::new();
    reg.register(Model::new(
        meta("x.src", "rusdoo_test_default_src"),
        vec![Field::new("name", FieldType::Char { size: None })],
    ))
    .unwrap();
    reg.register(Model::new(
        meta("x.doc", "rusdoo_test_default_doc"),
        vec![
            Field::new("name", FieldType::Char { size: None }),
            Field::new("criado_em", FieldType::Datetime).default_from(defaults::NOW),
            Field::new("dia", FieldType::Date).default_from(defaults::TODAY),
            Field::new("autor", FieldType::Integer).default_from(defaults::CURRENT_USER),
            Field::new("herdado", FieldType::Char { size: None }).default_from(last_partner_name),
            // um default constante continua funcionando ao lado
            Field::new("estado", FieldType::Char { size: None }).default_value(json!("rascunho")),
        ],
    ))
    .unwrap();
    for model in reg.models() {
        model.init_table(&pool).await.unwrap();
    }
    Some((reg, pool))
}

#[tokio::test]
async fn a_dynamic_default_runs_at_create_time_live() {
    let schema = rusdoo_testing::schema_for("rusdoo_case_dyn_default");
    let Some((reg, pool)) = fixture(schema).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };

    reg.create(&pool, "x.src", vec![("name", json!("Marcenaria"))])
        .await
        .unwrap();

    let id = reg
        .create_as(&pool, 7, "x.doc", vec![("name", json!("um documento"))])
        .await
        .unwrap();
    let row = &reg
        .read(
            &pool,
            "x.doc",
            &[id],
            &["criado_em", "dia", "autor", "herdado", "estado"],
        )
        .await
        .unwrap()[0];

    let hoje = chrono::Utc::now().format("%Y-%m-%d").to_string();
    assert_eq!(row["dia"], json!(hoje), "a data de hoje, não vazio");
    assert!(
        row["criado_em"].as_str().unwrap().starts_with(&hoje),
        "o instante da criação: {}",
        row["criado_em"]
    );
    assert_eq!(row["autor"], json!(7), "o usuário que está criando");
    assert_eq!(
        row["herdado"], json!("Marcenaria"),
        "um default pode ler do banco"
    );
    assert_eq!(row["estado"], json!("rascunho"), "o default constante segue");

    // and what the caller passed always wins, an explicit null included
    let id = reg
        .create_as(
            &pool,
            7,
            "x.doc",
            vec![
                ("dia", json!("2020-01-01")),
                ("herdado", Value::Null),
                ("autor", json!(99)),
            ],
        )
        .await
        .unwrap();
    let row = &reg
        .read(&pool, "x.doc", &[id], &["dia", "herdado", "autor"])
        .await
        .unwrap()[0];
    assert_eq!(row["dia"], json!("2020-01-01"));
    assert_eq!(row["autor"], json!(99));
    assert_eq!(
        row["herdado"], json!(null),
        "não preenchido de propósito continua não preenchido"
    );

    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(&pool)
        .await
        .unwrap();
}

/// The default runs inside the create's transaction: what it reads is
/// what the record is about to be stored next to.
#[tokio::test]
async fn a_default_sees_what_the_same_call_just_wrote_live() {
    let schema = rusdoo_testing::schema_for("rusdoo_case_dyn_default_tx");
    let Some((reg, pool)) = fixture(schema).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };

    // with no source at all, the default has nothing to answer and
    // invents nothing
    let id = reg
        .create_as(&pool, 1, "x.doc", vec![("name", json!("sem origem"))])
        .await
        .unwrap();
    let row = &reg.read(&pool, "x.doc", &[id], &["herdado"]).await.unwrap()[0];
    assert_eq!(row["herdado"], json!(null));

    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(&pool)
        .await
        .unwrap();
}
