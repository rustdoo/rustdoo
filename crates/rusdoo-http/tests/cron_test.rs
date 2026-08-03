//! The scheduler: what it claims, what it runs, and what it refuses to
//! sweep.
//!
//! On `TransactionCase`: the case brings its own schema with the modules
//! installed, and drops it at the end.

use rusdoo_http::dispatch::OrmService;
use rusdoo_testing::TransactionCase;
use serde_json::json;

/// What a scheduled job needs to have something to sweep.
const MODULES: [&str; 6] = ["base", "mail", "product", "account", "stock", "sale"];

fn service(case: &TransactionCase) -> OrmService {
    OrmService::insecure(case.registry(), case.pool()).with_methods(case.methods())
}

/// A job due right now.
async fn a_due_job(pool: &sqlx::PgPool, name: &str, model: &str, code: &str) -> i64 {
    let row: (i32,) = sqlx::query_as(
        r#"INSERT INTO "ir_cron" ("name", "model", "code", "interval_number",
                                  "interval_type", "nextcall", "active")
           VALUES ($1, $2, $3, 1, 'days', CURRENT_TIMESTAMP - interval '1 minute', true)
           RETURNING "id""#,
    )
    .bind(name)
    .bind(model)
    .bind(code)
    .fetch_one(pool)
    .await
    .unwrap();
    i64::from(row.0)
}

#[tokio::test]
async fn a_due_job_runs_once_and_is_pushed_forward_live() {
    let Some(case) = TransactionCase::open("cron", &MODULES).await else {
        return;
    };
    let (service, pool) = (service(&case), case.pool());
    let job = a_due_job(&pool, "vacuum", "ir.autovacuum", "power_on").await;

    assert_eq!(rusdoo_http::cron::run_due(&service).await, 1);
    // the next tick finds nothing: claiming rescheduled it, so a slow
    // job never becomes a stampede
    assert_eq!(rusdoo_http::cron::run_due(&service).await, 0);

    let (nextcall, lastcall): (Option<chrono::NaiveDateTime>, Option<chrono::NaiveDateTime>) =
        sqlx::query_as(r#"SELECT "nextcall", "lastcall" FROM "ir_cron" WHERE "id" = $1"#)
            .bind(job as i32)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(lastcall.is_some(), "it recorded when it ran");
    let next = nextcall.expect("rescheduled");
    let now: chrono::NaiveDateTime = sqlx::query_scalar("SELECT CURRENT_TIMESTAMP::timestamp")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(next > now, "the next run is in the future: {next} vs {now}");
    case.close().await;
}

#[tokio::test]
async fn the_vacuum_sweeps_old_dialogs_and_spares_everything_else_live() {
    let Some(case) = TransactionCase::open("cron_vacuum", &MODULES).await else {
        return;
    };
    let (service, pool) = (service(&case), case.pool());
    let registry = case.models();
    let partner = registry
        .create(&pool, "res.partner", vec![("name", json!("Ana"))])
        .await
        .unwrap();
    let order = registry
        .create(
            &pool,
            "sale.order",
            vec![
                ("name", json!("SO-CRON")),
                ("partner_id", json!(partner)),
            ],
        )
        .await
        .unwrap();
    // two dialogs: one left open long ago, one from a minute ago
    let old = registry
        .create(
            &pool,
            "sale.order.cancel",
            vec![("order_id", json!(order)), ("reason", json!("antigo"))],
        )
        .await
        .unwrap();
    let fresh = registry
        .create(
            &pool,
            "sale.order.cancel",
            vec![("order_id", json!(order)), ("reason", json!("recente"))],
        )
        .await
        .unwrap();
    sqlx::query(
        r#"UPDATE "sale_order_cancel" SET "create_date" = CURRENT_TIMESTAMP - interval '2 days'
           WHERE "id" = $1"#,
    )
    .bind(old as i32)
    .execute(&pool)
    .await
    .unwrap();

    a_due_job(&pool, "vacuum", "ir.autovacuum", "power_on").await;
    assert_eq!(rusdoo_http::cron::run_due(&service).await, 1);

    let left: Vec<(i32,)> = sqlx::query_as(r#"SELECT "id" FROM "sale_order_cancel" ORDER BY "id""#)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        left.iter().map(|(id,)| i64::from(*id)).collect::<Vec<_>>(),
        vec![fresh],
        "the old dialog went, the recent one stayed"
    );

    // and nothing stored was touched: a vacuum that could reach an order
    // would be a delete nobody asked for, scheduled
    let orders: i64 = sqlx::query_scalar(r#"SELECT count(*) FROM "sale_order""#)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(orders, 1);
    case.close().await;
}

#[tokio::test]
async fn a_job_naming_code_that_does_not_exist_is_skipped_live() {
    let Some(case) = TransactionCase::open("cron_missing", &MODULES).await else {
        return;
    };
    let (service, pool) = (service(&case), case.pool());
    a_due_job(&pool, "fantasma", "sale.order", "nao_existe").await;
    a_due_job(&pool, "vacuum", "ir.autovacuum", "power_on").await;

    // the good job still runs: a row naming code that is not there is a
    // configuration problem, not a reason to stop the scheduler
    assert_eq!(rusdoo_http::cron::run_due(&service).await, 1);
    case.close().await;
}

#[tokio::test]
async fn an_inactive_or_future_job_is_left_alone_live() {
    let Some(case) = TransactionCase::open("cron_idle", &MODULES).await else {
        return;
    };
    let (service, pool) = (service(&case), case.pool());
    sqlx::query(
        r#"INSERT INTO "ir_cron" ("name", "model", "code", "interval_number", "interval_type",
                                  "nextcall", "active")
           VALUES ('desligado', 'ir.autovacuum', 'power_on', 1, 'days',
                   CURRENT_TIMESTAMP - interval '1 hour', false),
                  ('amanhã', 'ir.autovacuum', 'power_on', 1, 'days',
                   CURRENT_TIMESTAMP + interval '1 day', true)"#,
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(rusdoo_http::cron::run_due(&service).await, 0);
    case.close().await;
}
