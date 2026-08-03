//! What one interpreter costs, measured instead of assumed.
//!
//! Issue #10 leaves open whether the bridge needs one interpreter per
//! worker or subinterpreters, and says the choice belongs to a benchmark
//! rather than to an argument. This is that benchmark, and it starts by
//! asking the question that comes first: *what is actually serialized?*
//!
//! Two workloads, because they answer differently:
//!
//! - **cpu**: a method that only computes. Every instruction of it needs
//!   the GIL, so no arrangement of one process can make two of these run
//!   at once. This is the floor — whatever it measures is what a second
//!   interpreter would have to beat.
//! - **io**: a method that reads and writes through the ORM, which is
//!   what an addon's method almost always is. Almost none of its time is
//!   bytecode; nearly all of it is Postgres answering. Whether *this*
//!   serializes is not a fact about the GIL — it is a fact about whether
//!   the bridge lets go of the GIL while it waits.
//!
//! Run it against a scratch database:
//!
//! ```sh
//! RUSDOO_TEST_DATABASE_URL=postgres:///rusdoo_test \
//!     cargo run --release -p rusdoo-python --example gil_bench
//! ```

use rusdoo_orm::methods::{MethodCtx, MethodRegistry};
use rusdoo_orm::registry::Registry;
use serde_json::json;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Instant;

/// The addon under test: one method that only computes, one that talks
/// to the database. Both are ordinary Odoo Python.
const ADDON: &str = r#"
from odoo import models, fields


class Widget(models.Model):
    _name = "bench.widget"
    _order = "id"

    name = fields.Char(required=True)
    counter = fields.Integer(default=0)

    def burn(self, rounds=20000):
        """Pure bytecode: the GIL is held for every step of it."""
        total = 0
        for i in range(rounds):
            total = (total + i * 7) % 1000003
        return total

    def touch(self, rounds=4):
        """What an addon's method really is: mostly waiting on Postgres."""
        for _ in range(rounds):
            value = self.counter
            self.write({"counter": value + 1})
        return self.counter
"#;

/// The levels of concurrency worth looking at. Beyond the core count the
/// numbers say more about the scheduler than about the bridge.
const CONCURRENCY: [usize; 5] = [1, 2, 4, 8, 16];

/// Calls per level. Enough that one slow scheduling decision does not
/// become the measurement: at the fastest level this still runs for the
/// better part of a second, and a run measured in hundredths would be
/// reporting the scheduler.
const CALLS: usize = 4096;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("set RUSDOO_TEST_DATABASE_URL to a scratch database");
        std::process::exit(1);
    };
    // a pool wider than the highest concurrency, so what is being
    // measured is the interpreter and not a queue for connections
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(32)
        .connect(&url)
        .await?;
    sqlx::query("DROP TABLE IF EXISTS bench_widget CASCADE")
        .execute(&pool)
        .await?;

    let mut registry = Registry::new();
    let mut methods = MethodRegistry::new();
    rusdoo_python::load_python_module(&mut registry, &mut methods, "bench", ADDON)?;
    registry.init_tables(&pool).await?;
    let registry = Arc::new(registry);

    let mut ids = Vec::new();
    for n in 0..CONCURRENCY.into_iter().max().unwrap() {
        ids.push(
            registry
                .create_as(&pool, 1, "bench.widget", vec![("name", json!(format!("w{n}")))])
                .await?,
        );
    }

    println!("threads available: {}", std::thread::available_parallelism()?);
    println!(
        "python: GIL {}",
        if gil_enabled() { "enabled" } else { "disabled" }
    );
    println!();

    for workload in ["cpu", "io"] {
        println!("== {workload} ==");
        println!("{:>11}  {:>10}  {:>10}  {:>8}", "concurrency", "wall", "calls/s", "scaling");
        let mut baseline = 0.0_f64;
        for level in CONCURRENCY {
            let rate = measure(&registry, &methods, &pool, &ids, workload, level).await?;
            if level == 1 {
                baseline = rate;
            }
            println!(
                "{level:>11}  {:>9.2}s  {rate:>10.1}  {:>7.2}x",
                CALLS as f64 / rate,
                rate / baseline
            );
        }
        println!();
    }
    sqlx::query("DROP TABLE IF EXISTS bench_widget CASCADE")
        .execute(&pool)
        .await?;
    Ok(())
}

/// Calls per second for `CALLS` calls run `level` at a time.
async fn measure(
    registry: &Arc<Registry>,
    methods: &MethodRegistry,
    pool: &PgPool,
    ids: &[i64],
    workload: &str,
    level: usize,
) -> Result<f64, Box<dyn std::error::Error>> {
    let method = if workload == "cpu" { "burn" } else { "touch" };
    let entry = methods
        .get("bench.widget", method)
        .expect("the benchmark's method is registered");
    // warm: the first call through the bridge pays for the interpreter
    // waking up, and that cost belongs to no level in particular
    let ctx = MethodCtx::new(Arc::clone(registry), pool, 1, "bench.widget", vec![ids[0]]);
    entry.call(ctx, &[], &serde_json::Map::new()).await?;

    let started = Instant::now();
    let mut running = tokio::task::JoinSet::new();
    let per_task = CALLS / level;
    for slot in 0..level {
        // one record per task: two tasks writing the same row would be
        // measuring row contention in Postgres, not the interpreter
        let id = ids[slot % ids.len()];
        let registry = Arc::clone(registry);
        let pool = pool.clone();
        let entry = entry.clone();
        running.spawn(async move {
            for _ in 0..per_task {
                let ctx =
                    MethodCtx::new(Arc::clone(&registry), &pool, 1, "bench.widget", vec![id]);
                entry
                    .call(ctx, &[], &serde_json::Map::new())
                    .await
                    .expect("the benchmark's method runs");
            }
        });
    }
    while let Some(done) = running.join_next().await {
        done?;
    }
    let elapsed = started.elapsed().as_secs_f64();
    Ok((per_task * level) as f64 / elapsed)
}

/// Whether this CPython holds a global lock at all — a free-threaded
/// build does not, and every number here would mean something else.
fn gil_enabled() -> bool {
    use pyo3::prelude::PyAnyMethods;
    pyo3::Python::attach(|py| {
        (|| -> pyo3::PyResult<bool> {
            py.import("sys")?
                .getattr("_is_gil_enabled")?
                .call0()?
                .extract()
        })()
        .unwrap_or(true)
    })
}
