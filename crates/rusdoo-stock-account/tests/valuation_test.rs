//! The valuation end to end: goods come in at a price, go out at
//! whatever the cost method says they are worth, and the product's cost
//! follows.
//!
//! Every test builds its own schema, so the suite can run twice at once
//! without the two runs dropping each other's tables.

use rusdoo_orm::methods::{MethodCtx, MethodRegistry};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use std::sync::Arc;

fn registry() -> Registry {
    let mut reg = rusdoo_base::registry().unwrap();
    rusdoo_product::extend(&mut reg).unwrap();
    rusdoo_account::extend(&mut reg).unwrap();
    rusdoo_stock::extend(&mut reg).unwrap();
    rusdoo_stock_account::extend(&mut reg).unwrap();
    reg
}

fn methods() -> MethodRegistry {
    let mut methods = MethodRegistry::new();
    rusdoo_stock::extend_methods(&mut methods).unwrap();
    rusdoo_account::extend_methods(&mut methods).unwrap();
    rusdoo_stock_account::extend_methods(&mut methods).unwrap();
    methods
}

struct Fixture {
    registry: Arc<Registry>,
    methods: MethodRegistry,
    pool: PgPool,
    schema: String,
    /// where goods come from, where they live, where they go
    vendor: i64,
    stock: i64,
    customer: i64,
}

impl Fixture {
    /// Call a model method the way the dispatch would.
    async fn call(&self, model: &str, method: &str, ids: Vec<i64>) -> Result<Value, String> {
        self.call_with(model, method, ids, Vec::new(), Map::new())
            .await
    }

    async fn call_with(
        &self,
        model: &str,
        method: &str,
        ids: Vec<i64>,
        rest: Vec<Value>,
        kwargs: Map<String, Value>,
    ) -> Result<Value, String> {
        let entry = self
            .methods
            .get(model, method)
            .unwrap_or_else(|| panic!("{method} should be registered on {model}"));
        let args: Vec<Value> = Vec::new();
        let ctx = MethodCtx::new(Arc::clone(&self.registry), &self.pool, 1, model, ids)
            .with_rest(rest);
        entry
            .call(ctx, &args, &kwargs)
            .await
            .map_err(|error| error.to_string())
    }

    async fn read(&self, model: &str, id: i64, fields: &[&str]) -> Map<String, Value> {
        self.registry
            .read(&self.pool, model, &[id], fields)
            .await
            .unwrap()
            .into_iter()
            .next()
            .expect("the record exists")
    }

    /// A money field, whatever shape the driver decoded the numeric in.
    async fn money(&self, model: &str, id: i64, field: &str) -> f64 {
        let row = self.read(model, id, &[field]).await;
        match row.get(field) {
            Some(Value::Number(number)) => number.as_f64().unwrap_or_default(),
            Some(Value::String(text)) => text.parse().unwrap_or_default(),
            other => panic!("{model}.{field} came back as {other:?}"),
        }
    }

    async fn a_product(&self, name: &str, cost_method: &str, standard_price: f64) -> i64 {
        self.registry
            .create(
                &self.pool,
                "product.product",
                vec![
                    ("name", json!(name)),
                    ("cost_method", json!(cost_method)),
                    ("standard_price", json!(standard_price)),
                ],
            )
            .await
            .unwrap()
    }

    /// A transfer with one line, validated: the goods really moved.
    async fn a_transfer(
        &self,
        source: i64,
        destination: i64,
        product: i64,
        quantity: f64,
        price_unit: f64,
    ) -> i64 {
        let picking = self
            .registry
            .create(
                &self.pool,
                "stock.picking",
                vec![
                    ("location_id", json!(source)),
                    ("location_dest_id", json!(destination)),
                    (
                        "move_ids",
                        json!([[0, 0, {
                            "product_id": product,
                            "product_uom_qty": quantity,
                            "quantity_done": quantity,
                            "price_unit": price_unit,
                        }]]),
                    ),
                ],
            )
            .await
            .unwrap();
        self.call("stock.picking", "action_confirm", vec![picking])
            .await
            .expect("the transfer confirms");
        self.call("stock.picking", "action_done", vec![picking])
            .await
            .expect("the transfer validates");
        picking
    }

    /// A receipt, valued.
    async fn received(&self, product: i64, quantity: f64, price_unit: f64) -> i64 {
        let picking = self
            .a_transfer(self.vendor, self.stock, product, quantity, price_unit)
            .await;
        self.call("stock.picking", "action_valuate", vec![picking])
            .await
            .expect("the receipt is valued");
        self.only_move(picking).await
    }

    /// A delivery, valued.
    async fn delivered(&self, product: i64, quantity: f64) -> i64 {
        let picking = self
            .a_transfer(self.stock, self.customer, product, quantity, 0.0)
            .await;
        self.call("stock.picking", "action_valuate", vec![picking])
            .await
            .expect("the delivery is valued");
        self.only_move(picking).await
    }

    async fn only_move(&self, picking: i64) -> i64 {
        let row = self.read("stock.picking", picking, &["move_ids"]).await;
        row["move_ids"]
            .as_array()
            .and_then(|ids| ids.first())
            .and_then(Value::as_i64)
            .expect("the transfer has a line")
    }

    async fn close(self) {
        sqlx::query(&format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema))
            .execute(&self.pool)
            .await
            .ok();
    }
}

/// A schema of this test's own, with every model's table in it.
async fn fixture(case: &str) -> Option<Fixture> {
    let pool = rusdoo_testing::pool_in(case)?;
    let schema = rusdoo_testing::schema_for(case).to_string();
    // a schema left behind by a run that panicked is dropped, not reused
    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(&pool)
        .await
        .expect("dropping the case schema");
    sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS {schema}"))
        .execute(&pool)
        .await
        .expect("creating the case schema");

    let registry = registry();
    registry
        .init_tables(&pool)
        .await
        .expect("creating the tables");
    sqlx::query(
        r#"INSERT INTO "res_users" ("id", "login", "name", "active")
           VALUES (1, 'admin', 'Administrator', true)
           ON CONFLICT ("id") DO NOTHING"#,
    )
    .execute(&pool)
    .await
    .expect("creating the superuser");
    sqlx::query(
        r#"SELECT setval('res_users_id_seq', GREATEST(1, (SELECT MAX("id") FROM "res_users")))"#,
    )
    .execute(&pool)
    .await
    .expect("moving the res.users sequence forward");

    for (code, prefix) in [("stock.picking.out", "WH/"), ("account.move", "INV/")] {
        registry
            .create(
                &pool,
                "ir.sequence",
                vec![
                    ("name", json!(code)),
                    ("code", json!(code)),
                    ("prefix", json!(prefix)),
                    ("padding", json!(5)),
                    ("number_next", json!(1)),
                ],
            )
            .await
            .unwrap();
    }

    let place = |name: &'static str, usage: &'static str| {
        let registry = &registry;
        let pool = &pool;
        async move {
            registry
                .create(
                    pool,
                    "stock.location",
                    vec![("name", json!(name)), ("usage", json!(usage))],
                )
                .await
                .unwrap()
        }
    };
    let vendor = place("Vendors", "supplier").await;
    let stock = place("Stock", "internal").await;
    let customer = place("Customers", "customer").await;

    Some(Fixture {
        registry: Arc::new(registry),
        methods: methods(),
        pool,
        schema,
        vendor,
        stock,
        customer,
    })
}

/// The suite is skipped, never silently passed, when there is no test
/// database configured.
macro_rules! case {
    ($name:expr) => {
        match fixture($name).await {
            Some(fixture) => fixture,
            None => {
                eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
                return;
            }
        }
    };
}

#[tokio::test]
async fn a_receipt_is_worth_the_price_it_came_in_at_live() {
    let fx = case!("rusdoo_stock_account_receipt");
    let product = fx.a_product("Chair", "fifo", 5.0).await;
    let receipt = fx.received(product, 10.0, 12.0).await;

    let row = fx
        .read("stock.move", receipt, &["is_in", "is_out", "is_valued"])
        .await;
    assert_eq!(row["is_in"], json!(true), "vendor to stock comes in");
    assert_eq!(row["is_out"], json!(false));
    assert_eq!(row["is_valued"], json!(true));
    assert_eq!(fx.money("stock.move", receipt, "value").await, 120.0);

    // and the product's cost followed the receipt, not the 5 it was
    // created with
    assert_eq!(
        fx.money("product.product", product, "standard_price").await,
        12.0
    );
    assert_eq!(
        fx.money("product.product", product, "total_value").await,
        120.0
    );
    assert_eq!(fx.money("product.product", product, "avg_cost").await, 12.0);
    fx.close().await;
}

#[tokio::test]
async fn a_fifo_delivery_eats_the_oldest_receipt_first_live() {
    let fx = case!("rusdoo_stock_account_fifo");
    let product = fx.a_product("Chair", "fifo", 5.0).await;
    fx.received(product, 10.0, 12.0).await;
    fx.received(product, 5.0, 20.0).await;

    let delivery = fx.delivered(product, 12.0).await;
    let row = fx.read("stock.move", delivery, &["is_out"]).await;
    assert_eq!(row["is_out"], json!(true), "stock to customer goes out");
    // all ten at 12, then two at 20 — not twelve at the latest price and
    // not twelve at the average
    assert_eq!(fx.money("stock.move", delivery, "value").await, 160.0);

    // three units of the second receipt are left, so the stock is worth
    // 60 and the cost is 20
    assert_eq!(
        fx.money("product.product", product, "total_value").await,
        60.0
    );
    assert_eq!(
        fx.money("product.product", product, "standard_price").await,
        20.0
    );
    fx.close().await;
}

#[tokio::test]
async fn an_average_priced_product_is_delivered_at_its_running_cost_live() {
    let fx = case!("rusdoo_stock_account_average");
    let product = fx.a_product("Chair", "average", 0.0).await;
    fx.received(product, 10.0, 10.0).await;
    // the second receipt at double the price moves the average to 15
    fx.received(product, 10.0, 20.0).await;
    assert_eq!(
        fx.money("product.product", product, "standard_price").await,
        15.0
    );

    let delivery = fx.delivered(product, 4.0).await;
    assert_eq!(fx.money("stock.move", delivery, "value").await, 60.0);
    // and taking stock out at the average leaves the average alone
    assert_eq!(
        fx.money("product.product", product, "standard_price").await,
        15.0
    );
    assert_eq!(
        fx.money("product.product", product, "total_value").await,
        240.0
    );
    fx.close().await;
}

#[tokio::test]
async fn a_standard_priced_product_ignores_what_it_was_bought_for_live() {
    let fx = case!("rusdoo_stock_account_standard");
    let product = fx.a_product("Chair", "standard", 9.0).await;
    // received at 30, which is not what a standard-priced product costs
    fx.received(product, 10.0, 30.0).await;
    assert_eq!(
        fx.money("product.product", product, "standard_price").await,
        9.0,
        "a standard price is decided by hand, not by a receipt"
    );

    let delivery = fx.delivered(product, 3.0).await;
    assert_eq!(fx.money("stock.move", delivery, "value").await, 27.0);
    fx.close().await;
}

#[tokio::test]
async fn an_internal_transfer_is_worth_nothing_to_the_accounting_live() {
    let fx = case!("rusdoo_stock_account_internal");
    let other = fx
        .registry
        .create(
            &fx.pool,
            "stock.location",
            vec![("name", json!("Shelf B")), ("usage", json!("internal"))],
        )
        .await
        .unwrap();
    let product = fx.a_product("Chair", "fifo", 5.0).await;
    fx.received(product, 10.0, 12.0).await;

    let picking = fx.a_transfer(fx.stock, other, product, 4.0, 0.0).await;
    let valued = fx
        .call("stock.picking", "action_valuate", vec![picking])
        .await
        .expect("valuing an internal transfer is not an error");
    assert_eq!(valued, json!(0), "nothing crossed the boundary");

    let mv = fx.only_move(picking).await;
    assert_eq!(fx.money("stock.move", mv, "value").await, 0.0);
    // and the stock is worth exactly what it was before the shelf change
    assert_eq!(
        fx.money("product.product", product, "total_value").await,
        120.0
    );
    fx.close().await;
}

#[tokio::test]
async fn a_transfer_that_has_not_happened_is_not_valued_live() {
    let fx = case!("rusdoo_stock_account_draft");
    let product = fx.a_product("Chair", "fifo", 5.0).await;
    let picking = fx
        .registry
        .create(
            &fx.pool,
            "stock.picking",
            vec![
                ("location_id", json!(fx.vendor)),
                ("location_dest_id", json!(fx.stock)),
                (
                    "move_ids",
                    json!([[0, 0, {"product_id": product, "product_uom_qty": 3, "price_unit": 10}]]),
                ),
            ],
        )
        .await
        .unwrap();

    let error = fx
        .call("stock.picking", "action_valuate", vec![picking])
        .await
        .expect_err("a draft transfer has moved nothing");
    assert!(error.contains("validate it before valuing it"), "{error}");
    let mv = fx.only_move(picking).await;
    assert_eq!(fx.money("stock.move", mv, "value").await, 0.0);
    fx.close().await;
}

#[tokio::test]
async fn a_hand_adjustment_beats_every_price_the_move_carried_live() {
    let fx = case!("rusdoo_stock_account_adjustment");
    let product = fx.a_product("Chair", "fifo", 5.0).await;
    let receipt = fx.received(product, 10.0, 12.0).await;
    assert_eq!(fx.money("stock.move", receipt, "value").await, 120.0);

    // the dialog opens over the move
    let action = fx
        .call("stock.move", "action_adjust_valuation", vec![receipt])
        .await
        .expect("the dialog opens");
    assert_eq!(action["res_model"], "product.value");
    assert_eq!(action["target"], "new", "it is a dialog, not a screen");
    assert_eq!(action["context"]["default_move_id"], json!(receipt));

    let adjustment = fx
        .registry
        .create_as(
            &fx.pool,
            1,
            "product.value",
            vec![
                ("move_id", json!(receipt)),
                ("product_id", json!(product)),
                ("value", json!(90.0)),
                ("description", json!("Freight was billed twice")),
            ],
        )
        .await
        .unwrap();
    fx.call("product.value", "action_apply", vec![adjustment])
        .await
        .expect("the adjustment applies");
    assert_eq!(fx.money("stock.move", receipt, "value").await, 90.0);

    // and the adjustment says who decided it and when
    let row = fx
        .read(
            "product.value",
            adjustment,
            &["user_id", "date", "description"],
        )
        .await;
    assert_eq!(row["user_id"][0], json!(1));
    assert!(row["date"].is_string(), "an adjustment is dated");
    assert_eq!(row["description"], "Freight was billed twice");

    // a delivery after the correction is priced off the corrected value
    let delivery = fx.delivered(product, 5.0).await;
    assert_eq!(fx.money("stock.move", delivery, "value").await, 45.0);
    fx.close().await;
}

#[tokio::test]
async fn a_move_that_crossed_nothing_has_no_valuation_to_adjust_live() {
    let fx = case!("rusdoo_stock_account_no_adjust");
    let product = fx.a_product("Chair", "fifo", 5.0).await;
    let picking = fx
        .registry
        .create(
            &fx.pool,
            "stock.picking",
            vec![
                ("location_id", json!(fx.stock)),
                ("location_dest_id", json!(fx.stock)),
                (
                    "move_ids",
                    json!([[0, 0, {"product_id": product, "product_uom_qty": 2}]]),
                ),
            ],
        )
        .await
        .unwrap();
    let mv = fx.only_move(picking).await;

    let error = fx
        .call("stock.move", "action_adjust_valuation", vec![mv])
        .await
        .expect_err("there is nothing to adjust");
    assert!(
        error.contains("never crossed the company's stock"),
        "{error}"
    );
    fx.close().await;
}

#[tokio::test]
async fn changing_a_cost_leaves_a_row_saying_who_changed_it_live() {
    let fx = case!("rusdoo_stock_account_price_change");
    let product = fx.a_product("Chair", "standard", 9.0).await;

    let mut kwargs = Map::new();
    kwargs.insert("price".into(), json!(11.5));
    let changed = fx
        .call_with(
            "product.product",
            "action_change_standard_price",
            vec![product],
            Vec::new(),
            kwargs.clone(),
        )
        .await
        .expect("the cost changes");
    assert_eq!(changed, json!(true));
    assert_eq!(
        fx.money("product.product", product, "standard_price").await,
        11.5
    );

    let trail = fx
        .registry
        .search(
            &fx.pool,
            "product.value",
            &rusdoo_orm::domain::parse_domain(&json!([["product_id", "=", product]])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(trail.len(), 1, "one change, one row");
    let row = fx
        .read("product.value", trail[0], &["description", "value"])
        .await;
    let description = row["description"].as_str().unwrap_or_default();
    assert!(description.contains("from 9 to 11.5"), "{description}");
    assert!(description.contains("Administrator"), "{description}");

    // asking for the price it already has changes nothing and records
    // nothing
    let again = fx
        .call_with(
            "product.product",
            "action_change_standard_price",
            vec![product],
            vec![json!(11.5)],
            Map::new(),
        )
        .await
        .expect("a no-op is not an error");
    assert_eq!(again, json!(false));
    fx.close().await;
}

#[tokio::test]
async fn a_fifo_product_records_no_price_history_because_nothing_uses_it_live() {
    let fx = case!("rusdoo_stock_account_fifo_price");
    let product = fx.a_product("Chair", "fifo", 5.0).await;
    fx.call_with(
        "product.product",
        "action_change_standard_price",
        vec![product],
        vec![json!(8.0)],
        Map::new(),
    )
    .await
    .expect("the cost changes");
    assert_eq!(
        fx.money("product.product", product, "standard_price").await,
        8.0
    );

    let trail = fx
        .registry
        .search(
            &fx.pool,
            "product.value",
            &rusdoo_orm::domain::parse_domain(&json!([])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .unwrap();
    assert!(
        trail.is_empty(),
        "FIFO prices what leaves from the receipts, so the hint is not history"
    );
    fx.close().await;
}

#[tokio::test]
async fn a_delivery_of_more_than_ever_arrived_is_still_worth_something_live() {
    let fx = case!("rusdoo_stock_account_oversold");
    let product = fx.a_product("Chair", "fifo", 5.0).await;
    fx.received(product, 4.0, 20.0).await;

    // six leave a warehouse that only ever received four: Odoo values the
    // extra two at the last price it saw rather than at nothing
    let delivery = fx.delivered(product, 6.0).await;
    assert_eq!(fx.money("stock.move", delivery, "value").await, 120.0);
    assert_eq!(
        fx.money("product.product", product, "total_value").await,
        -40.0,
        "the books say the stock is short, which is the truth"
    );
    fx.close().await;
}
