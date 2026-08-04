//! The bridge end to end: a confirmed sale that raises a request for
//! quotation, the quantity that moves afterwards, and what each document
//! tells the other when one of them is cancelled.
//!
//! Every case owns a schema of this run (`rusdoo_testing::pool_in`), so
//! two tests — and two runs of the suite — build the same tables without
//! meeting.

use rusdoo_orm::methods::{MethodCtx, MethodRegistry};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use std::sync::Arc;

/// The modules a case installs, in dependency order.
fn registry() -> Registry {
    let mut reg = rusdoo_base::registry().unwrap();
    rusdoo_mail::extend(&mut reg).unwrap();
    rusdoo_product::extend(&mut reg).unwrap();
    rusdoo_sale::extend(&mut reg).unwrap();
    rusdoo_purchase::extend(&mut reg).unwrap();
    rusdoo_sale_purchase::extend(&mut reg).unwrap();
    reg
}

fn methods() -> MethodRegistry {
    let mut methods = MethodRegistry::new();
    rusdoo_sale::extend_methods(&mut methods).unwrap();
    rusdoo_purchase::extend_methods(&mut methods).unwrap();
    rusdoo_sale_purchase::extend_methods(&mut methods).unwrap();
    methods
}

struct Fixture {
    registry: Arc<Registry>,
    methods: MethodRegistry,
    pool: PgPool,
    schema: String,
    customer: i64,
    vendor: i64,
}

impl Fixture {
    /// Call a model method the way the dispatch would.
    async fn call(&self, model: &str, method: &str, ids: Vec<i64>) -> Result<Value, String> {
        self.call_with(model, method, ids, Vec::new()).await
    }

    /// The same, with the positional arguments the call carried.
    async fn call_with(
        &self,
        model: &str,
        method: &str,
        ids: Vec<i64>,
        rest: Vec<Value>,
    ) -> Result<Value, String> {
        let entry = self
            .methods
            .get(model, method)
            .unwrap_or_else(|| panic!("{method} should be registered on {model}"));
        let args: Vec<Value> = Vec::new();
        let kwargs = Map::new();
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
            .unwrap_or_else(|| panic!("{model} {id} should exist"))
    }

    async fn search(&self, model: &str, domain: Value) -> Vec<i64> {
        self.registry
            .search(
                &self.pool,
                model,
                &rusdoo_orm::domain::parse_domain(&domain).unwrap(),
                &rusdoo_orm::crud::SearchOptions {
                    order: Some("id".into()),
                    ..rusdoo_orm::crud::SearchOptions::default()
                },
            )
            .await
            .unwrap()
    }

    /// A service somebody else performs, with the vendor who performs it.
    async fn a_subcontracted_service(&self, name: &str, price: f64) -> i64 {
        self.registry
            .create(
                &self.pool,
                "product.product",
                vec![
                    ("name", json!(name)),
                    ("type", json!("service")),
                    ("list_price", json!(180.0)),
                    ("service_to_purchase", json!(true)),
                    (
                        "seller_ids",
                        json!([[0, 0, {"partner_id": self.vendor, "price": price,
                                        "delay": 1, "min_qty": 0}]]),
                    ),
                ],
            )
            .await
            .unwrap()
    }

    /// A confirmed sale of `lines`, as `(product, quantity, price)`.
    async fn a_confirmed_sale(&self, lines: &[(i64, f64, f64)]) -> i64 {
        let commands: Vec<Value> = lines
            .iter()
            .map(|(product, quantity, price)| {
                json!([0, 0, {"product_id": product, "product_uom_qty": quantity,
                              "price_unit": price}])
            })
            .collect();
        let order = self
            .registry
            .create(
                &self.pool,
                "sale.order",
                vec![
                    ("partner_id", json!(self.customer)),
                    ("order_line", Value::Array(commands)),
                ],
            )
            .await
            .unwrap();
        self.call("sale.order", "action_confirm", vec![order])
            .await
            .expect("the order confirms");
        order
    }

    /// The purchase lines a sale order raised, oldest first.
    async fn purchase_lines_of(&self, order: i64) -> Vec<Map<String, Value>> {
        let sale = self.read("sale.order", order, &["order_line"]).await;
        let lines: Vec<i64> = sale["order_line"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_i64)
            .collect();
        let ids = self
            .search("purchase.order.line", json!([["sale_line_id", "in", lines]]))
            .await;
        if ids.is_empty() {
            return Vec::new();
        }
        self.registry
            .read(
                &self.pool,
                "purchase.order.line",
                &ids,
                &["order_id", "sale_line_id", "product_qty", "price_unit", "name"],
            )
            .await
            .unwrap()
    }

    /// The messages posted on a document's thread.
    async fn notices(&self, model: &str, res_id: i64) -> Vec<String> {
        let ids = self
            .search(
                "mail.message",
                json!([["model", "=", model], ["res_id", "=", res_id]]),
            )
            .await;
        if ids.is_empty() {
            return Vec::new();
        }
        self.registry
            .read(&self.pool, "mail.message", &ids, &["body"])
            .await
            .unwrap()
            .iter()
            .map(|row| row["body"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    async fn close(self) {
        sqlx::query(&format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema))
            .execute(&self.pool)
            .await
            .ok();
    }
}

/// A case with its own schema, its tables, its sequences and its people.
async fn fixture(case: &str) -> Option<Fixture> {
    let pool = rusdoo_testing::pool_in(case)?;
    // the schema name is this run's; dropping it first means a case that
    // panicked yesterday is not the reason today's assertions fail
    let schema = rusdoo_testing::schema_for(case).to_string();
    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(&pool)
        .await
        .unwrap();
    // `IF NOT EXISTS`: every connection of the pool creates the schema as
    // it opens, so the one that answers this may have made it already
    sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS {schema}"))
        .execute(&pool)
        .await
        .unwrap();

    let registry = registry();
    registry.init_tables(&pool).await.unwrap();
    // the superuser exists like it does after a boot: every call below is
    // made as uid 1, and a message stamped with an author who is not a
    // row is a reference the database refuses
    sqlx::query(
        r#"INSERT INTO "res_users" ("id", "login", "name", "active")
           VALUES (1, 'admin', 'Administrator', true) ON CONFLICT ("id") DO NOTHING"#,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        r#"SELECT setval('res_users_id_seq', GREATEST(1, (SELECT MAX("id") FROM "res_users")))"#,
    )
    .execute(&pool)
    .await
    .unwrap();
    // the sequences the two addons' data files load: without them a
    // document cannot be born with a number
    for (code, prefix) in [("sale.order", "SO"), ("purchase.order", "PO")] {
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
    let customer = registry
        .create(&pool, "res.partner", vec![("name", json!("Ana"))])
        .await
        .unwrap();
    let vendor = registry
        .create(
            &pool,
            "res.partner",
            vec![("name", json!("Super Service Supplier"))],
        )
        .await
        .unwrap();
    Some(Fixture {
        registry: Arc::new(registry),
        methods: methods(),
        pool,
        schema,
        customer,
        vendor,
    })
}

macro_rules! case {
    ($name:expr) => {
        match fixture($name).await {
            Some(fixture) => fixture,
            None => return,
        }
    };
}

#[tokio::test]
async fn a_confirmed_sale_raises_a_request_for_quotation_live() {
    let fx = case!("rusdoo_sale_purchase_raise");
    let service = fx.a_subcontracted_service("Out-sourced service", 100.0).await;
    let goods = fx
        .registry
        .create(
            &fx.pool,
            "product.product",
            vec![("name", json!("Table")), ("list_price", json!(1250.0))],
        )
        .await
        .unwrap();
    let order = fx
        .a_confirmed_sale(&[(service, 4.0, 180.0), (goods, 2.0, 1250.0)])
        .await;

    let raised = fx
        .call("sale.order", "action_generate_purchase_orders", vec![order])
        .await
        .expect("the confirmed sale raises its purchase");
    let purchases: Vec<i64> = raised["purchase_order_ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_i64)
        .collect();
    assert_eq!(purchases.len(), 1, "one vendor, one request for quotation");
    let purchase = purchases[0];

    let row = fx
        .read(
            "purchase.order",
            purchase,
            &["name", "state", "partner_id", "origin", "sale_order_count", "has_sale_order"],
        )
        .await;
    assert_eq!(row["state"], "draft", "it is a request, not an order");
    assert_eq!(row["partner_id"][0], json!(fx.vendor));
    let order_name = fx.read("sale.order", order, &["name"]).await["name"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(row["origin"], json!(order_name), "it says which sale asked");
    assert_eq!(row["sale_order_count"], json!(1));
    assert_eq!(row["has_sale_order"], json!(true));

    // only the service was bought: the table is delivered, not
    // subcontracted
    let lines = fx.purchase_lines_of(order).await;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["product_qty"], json!(4.0), "the quantity sold");
    assert_eq!(lines[0]["price_unit"], json!(100.0), "the vendor's price");
    assert_eq!(lines[0]["name"], "Out-sourced service");

    // and both documents can now find each other
    let sale = fx.read("sale.order", order, &["purchase_order_count"]).await;
    assert_eq!(sale["purchase_order_count"], json!(1));
    let action = fx
        .call("sale.order", "action_view_purchase_orders", vec![order])
        .await
        .expect("the stat button opens");
    assert_eq!(action["res_id"], json!(purchase));
    let action = fx
        .call("purchase.order", "action_view_sale_orders", vec![purchase])
        .await
        .expect("the stat button opens");
    assert_eq!(action["res_id"], json!(order));

    fx.close().await;
}

#[tokio::test]
async fn two_services_for_one_vendor_share_one_request_live() {
    let fx = case!("rusdoo_sale_purchase_share");
    let painting = fx.a_subcontracted_service("Painting", 100.0).await;
    let cleaning = fx.a_subcontracted_service("Cleaning", 40.0).await;
    let order = fx
        .a_confirmed_sale(&[(painting, 1.0, 200.0), (cleaning, 3.0, 60.0)])
        .await;

    fx.call("sale.order", "action_generate_purchase_orders", vec![order])
        .await
        .unwrap();

    let lines = fx.purchase_lines_of(order).await;
    assert_eq!(lines.len(), 2, "each sale line raised its own purchase line");
    let purchases: Vec<i64> = lines
        .iter()
        .map(|line| line["order_id"][0].as_i64().unwrap())
        .collect();
    assert_eq!(
        purchases[0], purchases[1],
        "the same vendor is not asked twice for one order"
    );
    // the stat button counts documents, not lines
    let sale = fx.read("sale.order", order, &["purchase_order_count"]).await;
    assert_eq!(sale["purchase_order_count"], json!(1));

    fx.close().await;
}

#[tokio::test]
async fn a_reconfirmed_sale_does_not_buy_the_service_twice_live() {
    let fx = case!("rusdoo_sale_purchase_reconfirm");
    let service = fx.a_subcontracted_service("Out-sourced service", 100.0).await;
    let order = fx.a_confirmed_sale(&[(service, 4.0, 180.0)]).await;
    fx.call("sale.order", "action_generate_purchase_orders", vec![order])
        .await
        .unwrap();
    assert_eq!(fx.purchase_lines_of(order).await.len(), 1);

    // cancelled, warned, and confirmed again — the request for quotation
    // that is already open is the one that stands
    fx.call("sale.order", "action_cancel", vec![order])
        .await
        .unwrap();
    fx.call(
        "sale.order",
        "action_notify_purchase_of_cancellation",
        vec![order],
    )
    .await
    .expect("the purchase is warned");
    fx.call("sale.order", "action_draft", vec![order])
        .await
        .unwrap();
    fx.call("sale.order", "action_confirm", vec![order])
        .await
        .unwrap();
    fx.call("sale.order", "action_generate_purchase_orders", vec![order])
        .await
        .unwrap();

    let lines = fx.purchase_lines_of(order).await;
    assert_eq!(lines.len(), 1, "still one purchase line: {lines:?}");
    assert_eq!(lines[0]["product_qty"], json!(4.0));

    fx.close().await;
}

#[tokio::test]
async fn a_service_nobody_sells_us_stops_the_generation_live() {
    let fx = case!("rusdoo_sale_purchase_no_vendor");
    let service = fx.a_subcontracted_service("Out-sourced service", 100.0).await;
    let order = fx.a_confirmed_sale(&[(service, 4.0, 180.0)]).await;
    // the vendor stopped selling it after the product was set up
    let sellers = fx
        .search("product.supplierinfo", json!([["product_id", "=", service]]))
        .await;
    fx.registry
        .unlink_as(&fx.pool, 1, "product.supplierinfo", &sellers)
        .await
        .unwrap();

    let error = fx
        .call("sale.order", "action_generate_purchase_orders", vec![order])
        .await
        .expect_err("a service nobody performs cannot be bought");
    assert!(error.contains("no vendor"), "{error}");
    assert!(
        fx.purchase_lines_of(order).await.is_empty(),
        "and nothing half-created was left behind"
    );

    fx.close().await;
}

#[tokio::test]
async fn a_draft_sale_raises_nothing_live() {
    let fx = case!("rusdoo_sale_purchase_draft");
    let service = fx.a_subcontracted_service("Out-sourced service", 100.0).await;
    let order = fx
        .registry
        .create(
            &fx.pool,
            "sale.order",
            vec![
                ("partner_id", json!(fx.customer)),
                (
                    "order_line",
                    json!([[0, 0, {"product_id": service, "product_uom_qty": 4,
                                   "price_unit": 180}]]),
                ),
            ],
        )
        .await
        .unwrap();

    let error = fx
        .call("sale.order", "action_generate_purchase_orders", vec![order])
        .await
        .expect_err("a quotation is not a promise to anyone yet");
    assert!(error.contains("confirm it before"), "{error}");

    fx.close().await;
}

#[tokio::test]
async fn selling_more_raises_the_open_request_live() {
    let fx = case!("rusdoo_sale_purchase_increase");
    let service = fx.a_subcontracted_service("Out-sourced service", 100.0).await;
    let order = fx.a_confirmed_sale(&[(service, 4.0, 180.0)]).await;
    fx.call("sale.order", "action_generate_purchase_orders", vec![order])
        .await
        .unwrap();
    let sale_line = fx.purchase_lines_of(order).await[0]["sale_line_id"][0]
        .as_i64()
        .unwrap();

    fx.call_with(
        "sale.order.line",
        "action_update_service_qty",
        vec![sale_line],
        vec![json!(16.0)],
    )
    .await
    .expect("the line takes the new quantity");

    let lines = fx.purchase_lines_of(order).await;
    assert_eq!(lines.len(), 1, "the open request is raised, not doubled");
    assert_eq!(lines[0]["product_qty"], json!(16.0));
    // and the sale line really did change
    let line = fx
        .read("sale.order.line", sale_line, &["product_uom_qty"])
        .await;
    assert_eq!(line["product_uom_qty"], json!(16.0));

    fx.close().await;
}

#[tokio::test]
async fn selling_more_after_the_purchase_is_confirmed_raises_a_second_one_live() {
    let fx = case!("rusdoo_sale_purchase_second");
    let service = fx.a_subcontracted_service("Out-sourced service", 100.0).await;
    let order = fx.a_confirmed_sale(&[(service, 4.0, 180.0)]).await;
    fx.call("sale.order", "action_generate_purchase_orders", vec![order])
        .await
        .unwrap();
    let first_line = fx.purchase_lines_of(order).await[0].clone();
    let sale_line = first_line["sale_line_id"][0].as_i64().unwrap();
    let first_purchase = first_line["order_id"][0].as_i64().unwrap();
    // the vendor accepted: what was ordered is now a promise
    fx.call("purchase.order", "action_confirm", vec![first_purchase])
        .await
        .unwrap();

    fx.call_with(
        "sale.order.line",
        "action_update_service_qty",
        vec![sale_line],
        vec![json!(12.0)],
    )
    .await
    .expect("the extra eight are bought again");

    let lines = fx.purchase_lines_of(order).await;
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(
        lines[0]["product_qty"],
        json!(4.0),
        "the confirmed order keeps what was promised"
    );
    assert_eq!(
        lines[1]["product_qty"],
        json!(8.0),
        "only the difference is bought"
    );
    let second_purchase = lines[1]["order_id"][0].as_i64().unwrap();
    assert_ne!(second_purchase, first_purchase);
    let second = fx.read("purchase.order", second_purchase, &["state"]).await;
    assert_eq!(second["state"], "draft");
    // the sale now points at two purchases
    let sale = fx.read("sale.order", order, &["purchase_order_count"]).await;
    assert_eq!(sale["purchase_order_count"], json!(2));

    fx.close().await;
}

#[tokio::test]
async fn selling_less_warns_the_buyer_and_touches_nothing_live() {
    let fx = case!("rusdoo_sale_purchase_decrease");
    let service = fx.a_subcontracted_service("Out-sourced service", 100.0).await;
    let order = fx.a_confirmed_sale(&[(service, 16.0, 180.0)]).await;
    fx.call("sale.order", "action_generate_purchase_orders", vec![order])
        .await
        .unwrap();
    let line = fx.purchase_lines_of(order).await[0].clone();
    let sale_line = line["sale_line_id"][0].as_i64().unwrap();
    let purchase = line["order_id"][0].as_i64().unwrap();

    fx.call_with(
        "sale.order.line",
        "action_update_service_qty",
        vec![sale_line],
        vec![json!(13.0)],
    )
    .await
    .expect("selling less is allowed");

    let lines = fx.purchase_lines_of(order).await;
    assert_eq!(
        lines[0]["product_qty"],
        json!(16.0),
        "the vendor may already have started: only a human trims the order"
    );
    let notices = fx.notices("purchase.order", purchase).await;
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("13 of Out-sourced service ordered instead of 16"),
        "{}",
        notices[0]
    );

    fx.close().await;
}

#[tokio::test]
async fn a_cancelled_sale_warns_the_purchase_it_raised_live() {
    let fx = case!("rusdoo_sale_purchase_cancel_sale");
    let service = fx.a_subcontracted_service("Out-sourced service", 100.0).await;
    let order = fx.a_confirmed_sale(&[(service, 4.0, 180.0)]).await;
    fx.call("sale.order", "action_generate_purchase_orders", vec![order])
        .await
        .unwrap();
    let purchase = fx.purchase_lines_of(order).await[0]["order_id"][0]
        .as_i64()
        .unwrap();

    // announcing a cancellation that did not happen is worse than none
    let error = fx
        .call(
            "sale.order",
            "action_notify_purchase_of_cancellation",
            vec![order],
        )
        .await
        .expect_err("the order is still confirmed");
    assert!(error.contains("not cancelled"), "{error}");

    fx.call("sale.order", "action_cancel", vec![order])
        .await
        .unwrap();
    let warned = fx
        .call(
            "sale.order",
            "action_notify_purchase_of_cancellation",
            vec![order],
        )
        .await
        .expect("the buyer is told");
    assert_eq!(warned["purchase_order_ids"], json!([purchase]));

    let notices = fx.notices("purchase.order", purchase).await;
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("4 of Out-sourced service cancelled"), "{}", notices[0]);

    fx.close().await;
}

#[tokio::test]
async fn a_cancelled_purchase_warns_the_sale_behind_it_live() {
    let fx = case!("rusdoo_sale_purchase_cancel_purchase");
    let service = fx.a_subcontracted_service("Out-sourced service", 100.0).await;
    let order = fx.a_confirmed_sale(&[(service, 4.0, 180.0)]).await;
    fx.call("sale.order", "action_generate_purchase_orders", vec![order])
        .await
        .unwrap();
    let purchase = fx.purchase_lines_of(order).await[0]["order_id"][0]
        .as_i64()
        .unwrap();

    fx.call("purchase.order", "action_cancel", vec![purchase])
        .await
        .unwrap();
    let warned = fx
        .call(
            "purchase.order",
            "action_notify_sale_of_cancellation",
            vec![purchase],
        )
        .await
        .expect("the salesperson is told");
    assert_eq!(warned["sale_order_ids"], json!([order]));

    let notices = fx.notices("sale.order", order).await;
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("purchase order(s): PO00001"), "{}", notices[0]);
    assert!(notices[0].contains("4 of Out-sourced service cancelled"), "{}", notices[0]);

    fx.close().await;
}

#[tokio::test]
async fn a_product_that_is_not_a_service_cannot_be_subcontracted_live() {
    let fx = case!("rusdoo_sale_purchase_product_rule");
    let error = fx
        .registry
        .create(
            &fx.pool,
            "product.product",
            vec![
                ("name", json!("Table")),
                ("type", json!("consu")),
                ("service_to_purchase", json!(true)),
                (
                    "seller_ids",
                    json!([[0, 0, {"partner_id": fx.vendor, "price": 100}]]),
                ),
            ],
        )
        .await
        .expect_err("goods are delivered, not subcontracted");
    assert!(error.to_string().contains("not a service"), "{error}");

    let error = fx
        .registry
        .create(
            &fx.pool,
            "product.product",
            vec![
                ("name", json!("Painting")),
                ("type", json!("service")),
                ("service_to_purchase", json!(true)),
            ],
        )
        .await
        .expect_err("somebody has to perform it");
    assert!(error.to_string().contains("define the vendor"), "{error}");

    fx.close().await;
}
