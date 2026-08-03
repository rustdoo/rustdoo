//! `ir.config_parameter` e a unicidade que o banco garante — as duas
//! andam juntas: o upsert do `set_param` precisa de uma chave para
//! conflitar.

use rusdoo_orm::registry::Registry;
use serde_json::json;
use sqlx::PgPool;

async fn fixture(schema: &str) -> Option<(Registry, PgPool)> {
    let url = std::env::var("RUSDOO_TEST_DATABASE_URL").ok()?;
    let owned = schema.to_string();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
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
    sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS {schema}"))
        .execute(&pool)
        .await
        .unwrap();

    let mut registry = Registry::new();
    rusdoo_base::extend(&mut registry).unwrap();
    for model in registry.models() {
        model.init_table(&pool).await.unwrap();
    }
    Some((registry, pool))
}

async fn drop_schema(pool: &PgPool, schema: &str) {
    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_parameter_is_read_written_and_forgotten_live() {
    let schema = rusdoo_testing::schema_for("rusdoo_case_config");
    let Some((reg, pool)) = fixture(schema).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };

    // a key nobody wrote is not an error: it is the module's default
    assert_eq!(reg.get_param(&pool, "sales_team.membership_multi").await.unwrap(), None);
    assert_eq!(
        reg.param_or(&pool, "web.base.url", "http://localhost:8069").await,
        "http://localhost:8069"
    );
    assert!(!reg.param_flag(&pool, "sales_team.membership_multi", false).await);

    // escrever devolve o que havia antes, como o set_param do Odoo
    assert_eq!(
        reg.set_param(&pool, "web.base.url", "https://erp.exemplo.com")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        reg.set_param(&pool, "web.base.url", "https://outro.exemplo.com")
            .await
            .unwrap()
            .as_deref(),
        Some("https://erp.exemplo.com")
    );
    assert_eq!(
        reg.get_param(&pool, "web.base.url").await.unwrap().as_deref(),
        Some("https://outro.exemplo.com")
    );

    // the several ways Odoo writes a boolean all count
    for truthy in ["True", "1", "yes", "  true  "] {
        reg.set_param(&pool, "x.flag", truthy).await.unwrap();
        assert!(
            reg.param_flag(&pool, "x.flag", false).await,
            "{truthy:?} deveria ser verdadeiro"
        );
    }
    reg.set_param(&pool, "x.flag", "False").await.unwrap();
    assert!(!reg.param_flag(&pool, "x.flag", true).await);

    assert!(reg.clear_param(&pool, "x.flag").await.unwrap());
    assert!(!reg.clear_param(&pool, "x.flag").await.unwrap());
    assert_eq!(reg.get_param(&pool, "x.flag").await.unwrap(), None);

    drop_schema(&pool, schema).await;
}

#[tokio::test]
async fn the_database_refuses_a_second_parameter_with_the_same_key_live() {
    let schema = rusdoo_testing::schema_for("rusdoo_case_config_uniq");
    let Some((reg, pool)) = fixture(schema).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };

    reg.create(
        &pool,
        "ir.config_parameter",
        vec![("key", json!("database.uuid")), ("value", json!("um"))],
    )
    .await
    .unwrap();
    let error = reg
        .create(
            &pool,
            "ir.config_parameter",
            vec![("key", json!("database.uuid")), ("value", json!("outro"))],
        )
        .await
        .expect_err("a chave é única");
    // and the user reads the sentence the model declared, not the
    // driver's
    assert!(
        error.to_string().contains("parameter with that key already exists"),
        "mensagem inesperada: {error}"
    );

    drop_schema(&pool, schema).await;
}

/// The reason the uniqueness lives in the database and not in a Rust
/// check.
#[tokio::test]
async fn concurrent_writers_cannot_both_create_the_same_key_live() {
    let schema = rusdoo_testing::schema_for("rusdoo_case_config_race");
    let Some((reg, pool)) = fixture(schema).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let reg = std::sync::Arc::new(reg);

    // sixteen simultaneous attempts at the same key: a check before the
    // INSERT would let several through, because they all read "it does
    // not exist" before any of them writes
    let mut tasks = Vec::new();
    for n in 0..16 {
        let reg = std::sync::Arc::clone(&reg);
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            reg.create(
                &pool,
                "ir.config_parameter",
                vec![("key", json!("database.secret")), ("value", json!(n))],
            )
            .await
        }));
    }
    let mut created = 0;
    for task in tasks {
        if task.await.unwrap().is_ok() {
            created += 1;
        }
    }
    assert_eq!(created, 1, "exatamente um vencedor, não {created}");

    let rows: i64 =
        sqlx::query_scalar(r#"SELECT COUNT(*) FROM "ir_config_parameter" WHERE "key" = $1"#)
            .bind("database.secret")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 1);

    // and a concurrent set_param converges on a single row too
    let mut tasks = Vec::new();
    for n in 0..16 {
        let reg = std::sync::Arc::clone(&reg);
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            reg.set_param(&pool, "database.uuid", &format!("valor-{n}"))
                .await
        }));
    }
    for task in tasks {
        task.await.unwrap().expect("um upsert não briga com outro");
    }
    let rows: i64 =
        sqlx::query_scalar(r#"SELECT COUNT(*) FROM "ir_config_parameter" WHERE "key" = $1"#)
            .bind("database.uuid")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 1);

    drop_schema(&pool, schema).await;
}

/// A repeated boot must not fail because of a constraint that is
/// already there — that is the normal case, not the exception.
#[tokio::test]
async fn adding_a_constraint_that_already_exists_is_a_no_op_live() {
    let schema = rusdoo_testing::schema_for("rusdoo_case_config_idempotent");
    let Some((reg, pool)) = fixture(schema).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };

    for _ in 0..3 {
        for model in reg.models() {
            model.init_table(&pool).await.expect("o boot se repete");
        }
    }
    // scoped to the case's schema: the other cases in this file have
    // the same constraint, under the same name, in theirs
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_constraint
         WHERE conname = 'ir_config_parameter_key_uniq'
           AND conrelid = to_regclass('ir_config_parameter')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "uma restrição, não três");

    drop_schema(&pool, schema).await;
}
