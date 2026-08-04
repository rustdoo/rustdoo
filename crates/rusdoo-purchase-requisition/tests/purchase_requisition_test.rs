//! The addon end to end: an agreement being signed, drawn down and
//! closed, and a call for tender being decided.

use rusdoo_orm::methods::{MethodCtx, MethodRegistry};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use std::sync::Arc;

/// Every case in a schema of its own: the suite runs in parallel and
/// every one of these builds the same tables.
fn pool_in(url: &str, schema: &'static str) -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .after_connect(move |conn, _meta| {
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
        .connect_lazy(url)
        .expect("connecting to the test database")
}

fn registry() -> Registry {
    let mut reg = rusdoo_base::registry().expect("the base models register");
    rusdoo_product::extend(&mut reg).unwrap();
    rusdoo_uom::extend(&mut reg).unwrap();
    rusdoo_account::extend(&mut reg).unwrap();
    rusdoo_stock::extend(&mut reg).unwrap();
    rusdoo_purchase::extend(&mut reg).unwrap();
    rusdoo_purchase_requisition::extend(&mut reg).unwrap();
    reg
}

fn methods() -> MethodRegistry {
    let mut methods = MethodRegistry::new();
    rusdoo_purchase::extend_methods(&mut methods).unwrap();
    rusdoo_purchase_requisition::extend_methods(&mut methods).unwrap();
    methods
}

struct Fixture {
    registry: Arc<Registry>,
    methods: MethodRegistry,
    pool: PgPool,
    schema: &'static str,
    vendor: i64,
    rival: i64,
    third: i64,
    table: i64,
    chair: i64,
}

impl Fixture {
    /// Call a model method the way the dispatch would.
    async fn call(&self, model: &str, method: &str, ids: Vec<i64>) -> Result<Value, String> {
        self.dispatch(model, method, ids.clone(), vec![json!(ids)], Vec::new())
            .await
    }

    /// `create`, whose first argument is the values and not a recordset.
    async fn create_agreement(&self, values: Value) -> Result<i64, String> {
        let args = vec![values];
        let answer = self
            .dispatch(
                "purchase.requisition",
                "create",
                Vec::new(),
                args.clone(),
                args,
            )
            .await?;
        Ok(answer.as_i64().expect("create answers an id"))
    }

    /// `write`, whose arguments are the recordset and then the values.
    async fn write_agreement(&self, ids: Vec<i64>, values: Value) -> Result<Value, String> {
        self.dispatch(
            "purchase.requisition",
            "write",
            ids.clone(),
            vec![json!(ids), values.clone()],
            vec![values],
        )
        .await
    }

    async fn dispatch(
        &self,
        model: &str,
        method: &str,
        ids: Vec<i64>,
        args: Vec<Value>,
        rest: Vec<Value>,
    ) -> Result<Value, String> {
        let entry = self
            .methods
            .get(model, method)
            .unwrap_or_else(|| panic!("{method} should be registered on {model}"));
        let kwargs = Map::new();
        let ctx = MethodCtx::new(Arc::clone(&self.registry), &self.pool, 1, model, ids).with_rest(rest);
        entry.call(ctx, &args, &kwargs)
            .await
            .map_err(|error| error.to_string())
    }

    async fn read(&self, model: &str, id: i64, fields: &[&str]) -> Map<String, Value> {
        self.registry
            .read(&self.pool, model, &[id], fields)
            .await
            .expect("the record reads")
            .into_iter()
            .next()
            .expect("the record exists")
    }

    /// A blanket order for two products, still a draft.
    async fn a_blanket_order(&self) -> i64 {
        self.create_agreement(json!({
            "vendor_id": self.vendor,
            "requisition_type": "blanket_order",
            "date_start": "2026-01-01",
            "date_end": "2026-12-31",
            "description": "Payment at 30 days.",
            "line_ids": [
                [0, 0, {"product_id": self.table, "product_qty": 10, "price_unit": 900}],
                [0, 0, {"product_id": self.chair, "product_qty": 40, "price_unit": 120}],
            ],
        }))
        .await
        .expect("the agreement is created")
    }

    /// The same, confirmed.
    async fn a_confirmed_blanket_order(&self) -> i64 {
        let agreement = self.a_blanket_order().await;
        self.call("purchase.requisition", "action_confirm", vec![agreement])
            .await
            .expect("the agreement confirms");
        agreement
    }

    /// A plain request for quotation for `product`, outside any
    /// agreement.
    async fn a_quotation(&self, partner: i64, quantity: f64, price: f64) -> i64 {
        self.registry
            .create_as(
                &self.pool,
                1,
                "purchase.order",
                vec![
                    ("partner_id", json!(partner)),
                    (
                        "order_line",
                        json!([[0, 0, {
                            "product_id": self.table,
                            "name": "Table",
                            "product_qty": quantity,
                            "price_unit": price,
                        }]]),
                    ),
                ],
            )
            .await
            .expect("the quotation is created")
    }

    async fn close(self) {
        sqlx::query(&format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema))
            .execute(&self.pool)
            .await
            .expect("dropping the case schema");
    }
}

async fn fixture(case: &str) -> Option<Fixture> {
    let url = std::env::var(rusdoo_testing::DATABASE_ENV).ok()?;
    let schema = rusdoo_testing::schema_for(&format!("rusdoo_purchase_requisition_{case}"));
    let pool = pool_in(&url, schema);
    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(&pool)
        .await
        .expect("dropping a schema left behind by a crashed run");
    sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS {schema}"))
        .execute(&pool)
        .await
        .expect("creating the case schema");

    let registry = registry();
    registry
        .init_tables(&pool)
        .await
        .expect("creating the models' tables");
    // the superuser every call is made as: `user_id` points at it, and a
    // reference to a row that is not there is one the database refuses
    sqlx::query(
        r#"INSERT INTO "res_users" ("id", "login", "name", "active")
           VALUES (1, 'admin', 'Administrator', true) ON CONFLICT ("id") DO NOTHING"#,
    )
    .execute(&pool)
    .await
    .expect("creating the case's superuser");

    // the sequences the data files of `purchase` and of this addon load
    for (code, prefix) in [
        ("purchase.order", "PO"),
        ("purchase.requisition.blanket.order", "BO"),
        ("purchase.requisition.purchase.template", "PT"),
    ] {
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
            .expect("loading the module's sequence");
    }

    let mut partners = Vec::new();
    for name in ["Móveis Silva", "Marcenaria Rocha", "Indústria Prado"] {
        partners.push(
            registry
                .create(&pool, "res.partner", vec![("name", json!(name))])
                .await
                .expect("the vendor is created"),
        );
    }
    let table = registry
        .create(
            &pool,
            "product.product",
            vec![("name", json!("Table")), ("standard_price", json!(900))],
        )
        .await
        .unwrap();
    let chair = registry
        .create(
            &pool,
            "product.product",
            vec![("name", json!("Chair")), ("standard_price", json!(120))],
        )
        .await
        .unwrap();

    Some(Fixture {
        registry: Arc::new(registry),
        methods: methods(),
        pool,
        schema,
        vendor: partners[0],
        rival: partners[1],
        third: partners[2],
        table,
        chair,
    })
}

/// The whole suite skips, loudly, when there is no database.
macro_rules! case {
    ($name:expr) => {
        match fixture($name).await {
            Some(fixture) => fixture,
            None => {
                eprintln!("skipped: {} not set", rusdoo_testing::DATABASE_ENV);
                return;
            }
        }
    };
}

#[tokio::test]
async fn each_kind_of_agreement_is_numbered_from_its_own_series_live() {
    let fx = case!("numbering");
    let blanket = fx.a_blanket_order().await;
    assert_eq!(fx.read("purchase.requisition", blanket, &["name"]).await["name"], "BO00001");

    let template = fx
        .create_agreement(json!({
            "requisition_type": "purchase_template",
            "vendor_id": fx.vendor,
            "line_ids": [[0, 0, {"product_id": fx.table, "product_qty": 3, "price_unit": 880}]],
        }))
        .await
        .unwrap();
    assert_eq!(
        fx.read("purchase.requisition", template, &["name"]).await["name"],
        "PT00001",
        "a template does not consume a blanket order's number"
    );

    // changing the kind renumbers the agreement and drops its validity:
    // a template is a shape, not a deal for a period
    fx.write_agreement(
        vec![blanket],
        json!({"requisition_type": "purchase_template"}),
    )
    .await
    .expect("a draft agreement changes kind");
    let row = fx
        .read(
            "purchase.requisition",
            blanket,
            &["name", "requisition_type", "date_start", "date_end"],
        )
        .await;
    assert_eq!(row["name"], "PT00002");
    assert_eq!(row["requisition_type"], "purchase_template");
    assert_eq!(row["date_start"], Value::Null);
    assert_eq!(row["date_end"], Value::Null);

    // and a name the caller passed is not the one the record gets
    let named = fx
        .create_agreement(json!({
            "name": "MY OWN NUMBER",
            "vendor_id": fx.vendor,
            "line_ids": [[0, 0, {"product_id": fx.table, "product_qty": 1, "price_unit": 10}]],
        }))
        .await
        .unwrap();
    assert_eq!(
        fx.read("purchase.requisition", named, &["name"]).await["name"],
        "BO00002"
    );
    fx.close().await;
}

#[tokio::test]
async fn a_confirmed_agreement_does_not_change_kind_live() {
    let fx = case!("kind_locked");
    let agreement = fx.a_confirmed_blanket_order().await;
    let error = fx
        .write_agreement(
            vec![agreement],
            json!({"requisition_type": "purchase_template"}),
        )
        .await
        .expect_err("a confirmed agreement keeps its kind");
    assert!(error.contains("only be changed while it is a draft"), "{error}");
    // and the record was not touched on the way to the refusal
    let row = fx
        .read("purchase.requisition", agreement, &["requisition_type", "name"])
        .await;
    assert_eq!(row["requisition_type"], "blanket_order");
    assert_eq!(row["name"], "BO00001");
    fx.close().await;
}

#[tokio::test]
async fn a_blanket_order_is_not_confirmed_with_a_line_missing_a_price_live() {
    let fx = case!("confirm_guards");
    let empty = fx
        .create_agreement(json!({"vendor_id": fx.vendor}))
        .await
        .unwrap();
    let error = fx
        .call("purchase.requisition", "action_confirm", vec![empty])
        .await
        .expect_err("an agreement about nothing is not confirmed");
    assert!(error.contains("no product lines"), "{error}");

    let unpriced = fx
        .create_agreement(json!({
            "vendor_id": fx.vendor,
            "line_ids": [[0, 0, {"product_id": fx.table, "product_qty": 5, "price_unit": 0}]],
        }))
        .await
        .unwrap();
    let error = fx
        .call("purchase.requisition", "action_confirm", vec![unpriced])
        .await
        .expect_err("a blanket order is a promise about a price");
    assert!(error.contains("missing a price"), "{error}");

    // a purchase template is not a promise, so an unpriced line is fine
    let template = fx
        .create_agreement(json!({
            "requisition_type": "purchase_template",
            "vendor_id": fx.vendor,
            "line_ids": [[0, 0, {"product_id": fx.table, "product_qty": 5, "price_unit": 0}]],
        }))
        .await
        .unwrap();
    fx.call("purchase.requisition", "action_confirm", vec![template])
        .await
        .expect("a template confirms without prices");
    fx.close().await;
}

#[tokio::test]
async fn an_agreement_may_not_end_before_it_starts_live() {
    let fx = case!("validity");
    let error = fx
        .create_agreement(json!({
            "vendor_id": fx.vendor,
            "date_start": "2026-06-01",
            "date_end": "2026-05-01",
        }))
        .await
        .expect_err("the dates are backwards");
    assert!(error.contains("cannot come before"), "{error}");
    fx.close().await;
}

#[tokio::test]
async fn a_confirmed_agreement_produces_a_quotation_at_the_agreed_price_live() {
    let fx = case!("rfq");
    let agreement = fx.a_confirmed_blanket_order().await;

    let action = fx
        .call("purchase.requisition", "action_create_rfq", vec![agreement])
        .await
        .expect("the quotation comes out");
    assert_eq!(action["res_model"], "purchase.order");
    let order = action["res_id"].as_i64().expect("the order has an id");

    let row = fx
        .read(
            "purchase.order",
            order,
            &["name", "state", "partner_id", "requisition_id", "notes", "amount_total"],
        )
        .await;
    assert_eq!(row["state"], "draft", "a quotation is not an order yet");
    assert_eq!(row["partner_id"][0], json!(fx.vendor));
    assert_eq!(row["requisition_id"][0], json!(agreement));
    assert_eq!(row["notes"], "Payment at 30 days.", "the terms travel with it");
    // 10 tables at 900 and 40 chairs at 120: the agreed prices, applied
    assert_eq!(row["amount_total"], json!(13800.0));

    // the agreement knows it produced one, and the type reads through
    let seen = fx
        .read("purchase.requisition", agreement, &["order_count"])
        .await;
    assert_eq!(seen["order_count"], json!(1));
    let related = fx
        .read("purchase.order", order, &["requisition_type"])
        .await;
    assert_eq!(related["requisition_type"], "blanket_order");

    // nothing is ordered while the quotation is still a quotation
    let ordered = fx
        .call(
            "purchase.requisition",
            "get_ordered_quantities",
            vec![agreement],
        )
        .await
        .unwrap();
    assert!(
        ordered.as_object().unwrap().values().all(|q| q == &json!(0.0)),
        "{ordered}"
    );

    fx.call("purchase.order", "button_confirm", vec![order])
        .await
        .expect("the quotation is confirmed");
    let ordered = fx
        .call(
            "purchase.requisition",
            "get_ordered_quantities",
            vec![agreement],
        )
        .await
        .unwrap();
    let quantities: Vec<f64> = ordered
        .as_object()
        .unwrap()
        .values()
        .filter_map(Value::as_f64)
        .collect();
    assert!(quantities.contains(&10.0), "the tables were ordered: {ordered}");
    assert!(quantities.contains(&40.0), "the chairs were ordered: {ordered}");
    fx.close().await;
}

#[tokio::test]
async fn a_draft_agreement_has_nothing_to_quote_live() {
    let fx = case!("rfq_guard");
    let agreement = fx.a_blanket_order().await;
    let error = fx
        .call("purchase.requisition", "action_create_rfq", vec![agreement])
        .await
        .expect_err("a draft agreement is not drawn down");
    assert!(error.contains("confirm it before"), "{error}");
    fx.close().await;
}

#[tokio::test]
async fn an_agreement_is_not_closed_while_a_quotation_is_open_live() {
    let fx = case!("closing");
    let agreement = fx.a_confirmed_blanket_order().await;
    let order = fx
        .call("purchase.requisition", "action_create_rfq", vec![agreement])
        .await
        .unwrap()["res_id"]
        .as_i64()
        .unwrap();

    let error = fx
        .call("purchase.requisition", "action_done", vec![agreement])
        .await
        .expect_err("an open quotation blocks the close");
    assert!(error.contains("still open requests"), "{error}");

    fx.call("purchase.order", "button_confirm", vec![order])
        .await
        .unwrap();
    fx.call("purchase.requisition", "action_done", vec![agreement])
        .await
        .expect("a decided agreement closes");
    assert_eq!(
        fx.read("purchase.requisition", agreement, &["state"]).await["state"],
        "done"
    );
    fx.close().await;
}

#[tokio::test]
async fn cancelling_an_agreement_cancels_the_quotations_under_it_live() {
    let fx = case!("cancelling");
    let agreement = fx.a_confirmed_blanket_order().await;
    let order = fx
        .call("purchase.requisition", "action_create_rfq", vec![agreement])
        .await
        .unwrap()["res_id"]
        .as_i64()
        .unwrap();

    fx.call("purchase.requisition", "action_cancel", vec![agreement])
        .await
        .expect("the agreement is cancelled");
    assert_eq!(
        fx.read("purchase.requisition", agreement, &["state"]).await["state"],
        "cancel"
    );
    assert_eq!(
        fx.read("purchase.order", order, &["state"]).await["state"],
        "cancel",
        "an order under a dead agreement must not be confirmable"
    );

    // and a cancelled agreement is the one that goes back to draft
    fx.call("purchase.requisition", "action_draft", vec![agreement])
        .await
        .expect("a cancelled agreement reopens");
    assert_eq!(
        fx.read("purchase.requisition", agreement, &["state"]).await["state"],
        "draft"
    );
    fx.close().await;
}

#[tokio::test]
async fn a_confirmed_agreement_is_not_deleted_live() {
    let fx = case!("unlink");
    let agreement = fx.a_confirmed_blanket_order().await;
    let error = fx
        .registry
        .unlink_as(&fx.pool, 1, "purchase.requisition", &[agreement])
        .await
        .expect_err("a confirmed agreement is closed, not deleted");
    assert!(error.to_string().contains("only a draft or cancelled"), "{error}");

    let draft = fx.a_blanket_order().await;
    let lines = fx.read("purchase.requisition", draft, &["line_ids"]).await;
    let line_count = lines["line_ids"].as_array().unwrap().len();
    assert_eq!(line_count, 2);
    fx.registry
        .unlink_as(&fx.pool, 1, "purchase.requisition", &[draft])
        .await
        .expect("a draft agreement is deleted");
    // and its lines went with it, through the cascade the field declares
    let orphans = fx
        .registry
        .search(
            &fx.pool,
            "purchase.requisition.line",
            &rusdoo_orm::domain::parse_domain(&json!([["requisition_id", "=", draft]])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .unwrap();
    assert!(orphans.is_empty(), "{orphans:?}");
    fx.close().await;
}

#[tokio::test]
async fn alternatives_are_created_and_held_in_one_group_live() {
    let fx = case!("alternatives");
    let origin = fx.a_quotation(fx.vendor, 10.0, 900.0).await;

    let action = fx
        .call("purchase.order", "action_create_alternative", vec![origin])
        .await
        .expect("the dialog opens");
    assert_eq!(action["target"], "new", "it is a dialog, not a screen");
    let wizard = action["res_id"].as_i64().unwrap();
    fx.registry
        .write_as(
            &fx.pool,
            1,
            "purchase.requisition.create.alternative",
            &[wizard],
            vec![("partner_ids", json!([[6, 0, [fx.rival, fx.third]]]))],
        )
        .await
        .unwrap();

    let action = fx
        .call(
            "purchase.requisition.create.alternative",
            "action_create_alternative",
            vec![wizard],
        )
        .await
        .expect("the alternatives come out");
    let created: Vec<i64> = action["domain"][0][2]
        .as_array()
        .expect("two orders were created")
        .iter()
        .filter_map(Value::as_i64)
        .collect();
    assert_eq!(created.len(), 2);

    // the three of them are one another's alternatives, through one group
    let seen = fx.read("purchase.order", origin, &["alternative_po_ids"]).await;
    let mut members: Vec<i64> = seen["alternative_po_ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_i64)
        .collect();
    members.sort_unstable();
    let mut expected = created.clone();
    expected.push(origin);
    expected.sort_unstable();
    assert_eq!(members, expected);

    // the products were copied, but not the price: what the other vendor
    // charges is the whole question
    let copy = fx.read("purchase.order", created[0], &["order_line", "partner_id"]).await;
    assert_eq!(copy["partner_id"][0], json!(fx.rival));
    let line = copy["order_line"][0].as_i64().unwrap();
    let row = fx
        .read("purchase.order.line", line, &["product_id", "product_qty", "price_unit"])
        .await;
    assert_eq!(row["product_id"][0], json!(fx.table));
    assert_eq!(row["product_qty"], json!(10.0));
    assert_eq!(row["price_unit"], json!(0.0));
    fx.close().await;
}

#[tokio::test]
async fn confirming_one_offer_asks_what_to_do_with_the_others_live() {
    let fx = case!("tender_decision");
    let winner = fx.a_quotation(fx.vendor, 10.0, 880.0).await;
    let loser = fx.a_quotation(fx.rival, 10.0, 950.0).await;
    group(&fx, winner, &[loser]).await;

    let action = fx
        .call("purchase.order", "button_confirm", vec![winner])
        .await
        .expect("the confirm stops to ask");
    assert_eq!(
        action["res_model"], "purchase.requisition.alternative.warning",
        "a rival offer is still open, so it asks"
    );
    let wizard = action["res_id"].as_i64().unwrap();
    assert_eq!(
        fx.read(
            "purchase.requisition.alternative.warning",
            wizard,
            &["po_ids", "alternative_po_ids"]
        )
        .await["alternative_po_ids"],
        json!([loser])
    );
    // nothing was confirmed while the question was open
    assert_eq!(
        fx.read("purchase.order", winner, &["state"]).await["state"],
        "draft"
    );

    fx.call(
        "purchase.requisition.alternative.warning",
        "action_cancel_alternatives",
        vec![wizard],
    )
    .await
    .expect("the tender is decided");
    assert_eq!(
        fx.read("purchase.order", winner, &["state"]).await["state"],
        "purchase"
    );
    assert_eq!(
        fx.read("purchase.order", loser, &["state"]).await["state"],
        "cancel"
    );
    fx.close().await;
}

#[tokio::test]
async fn the_losing_offers_may_be_kept_live() {
    let fx = case!("tender_keep");
    let chosen = fx.a_quotation(fx.vendor, 10.0, 880.0).await;
    let other = fx.a_quotation(fx.rival, 10.0, 950.0).await;
    group(&fx, chosen, &[other]).await;

    let wizard = fx
        .call("purchase.order", "button_confirm", vec![chosen])
        .await
        .unwrap()["res_id"]
        .as_i64()
        .unwrap();
    fx.call(
        "purchase.requisition.alternative.warning",
        "action_keep_alternatives",
        vec![wizard],
    )
    .await
    .expect("the other offer stays on the table");
    assert_eq!(
        fx.read("purchase.order", chosen, &["state"]).await["state"],
        "purchase"
    );
    assert_eq!(
        fx.read("purchase.order", other, &["state"]).await["state"],
        "draft",
        "keeping means keeping"
    );
    fx.close().await;
}

#[tokio::test]
async fn an_offer_with_no_rivals_confirms_straight_away_live() {
    let fx = case!("no_rivals");
    let alone = fx.a_quotation(fx.vendor, 4.0, 800.0).await;
    let answer = fx
        .call("purchase.order", "button_confirm", vec![alone])
        .await
        .expect("there is nothing to ask about");
    assert_eq!(answer, json!(true));
    assert_eq!(
        fx.read("purchase.order", alone, &["state"]).await["state"],
        "purchase"
    );
    fx.close().await;
}

#[tokio::test]
async fn the_cheapest_offers_are_pointed_out_live() {
    let fx = case!("best_lines");
    // same product: 10 at 900 (9000), 10 at 880 (8800), 20 at 890 (17800)
    let dearest = fx.a_quotation(fx.vendor, 10.0, 900.0).await;
    let cheapest = fx.a_quotation(fx.rival, 10.0, 880.0).await;
    let bulk = fx.a_quotation(fx.third, 20.0, 890.0).await;
    group(&fx, dearest, &[cheapest, bulk]).await;

    let best = fx
        .call("purchase.order", "get_tender_best_lines", vec![dearest])
        .await
        .expect("the comparison answers");
    let cheapest_line = only_line(&fx, cheapest).await;
    // the smallest bill belongs to the cheapest offer for that quantity
    assert_eq!(best["best_price_ids"], json!([cheapest_line]));
    // and so does the smallest unit price, 880 against 890 and 900
    assert_eq!(best["best_price_unit_ids"], json!([cheapest_line]));

    // a cancelled offer is out of the running
    fx.registry
        .write_as(
            &fx.pool,
            1,
            "purchase.order",
            &[cheapest],
            vec![("state", json!("cancel"))],
        )
        .await
        .unwrap();
    let best = fx
        .call("purchase.order", "get_tender_best_lines", vec![dearest])
        .await
        .unwrap();
    let dearest_line = only_line(&fx, dearest).await;
    assert_eq!(
        best["best_price_ids"],
        json!([dearest_line]),
        "9000 beats 17800 on the total"
    );
    let bulk_line = only_line(&fx, bulk).await;
    assert_eq!(
        best["best_price_unit_ids"],
        json!([bulk_line]),
        "890 a unit beats 900 a unit"
    );
    fx.close().await;
}

#[tokio::test]
async fn leaving_a_tender_of_two_dissolves_the_group_live() {
    let fx = case!("leaving");
    let first = fx.a_quotation(fx.vendor, 5.0, 900.0).await;
    let second = fx.a_quotation(fx.rival, 5.0, 910.0).await;
    group(&fx, first, &[second]).await;
    let group_id = fx
        .read("purchase.order", first, &["purchase_group_id"])
        .await["purchase_group_id"][0]
        .as_i64()
        .expect("the group exists");

    fx.call("purchase.order", "action_remove_from_group", vec![second])
        .await
        .expect("the offer leaves the tender");
    // a group of one is not a comparison: it goes, and the order left
    // behind stops claiming to have alternatives
    let left = fx
        .registry
        .read(&fx.pool, "purchase.order.group", &[group_id], &["id"])
        .await
        .unwrap();
    assert!(left.is_empty(), "the group dissolved");
    let alone = fx
        .read("purchase.order", first, &["purchase_group_id", "alternative_po_ids"])
        .await;
    assert_eq!(alone["purchase_group_id"], Value::Null);
    assert_eq!(alone["alternative_po_ids"], Value::Null);
    fx.close().await;
}

/// The single line of a one-product quotation.
async fn only_line(fx: &Fixture, order: i64) -> i64 {
    fx.read("purchase.order", order, &["order_line"]).await["order_line"][0]
        .as_i64()
        .expect("the quotation has a line")
}

/// Put `others` into `origin`'s tender, the way the dialog does.
async fn group(fx: &Fixture, origin: i64, others: &[i64]) {
    let group = fx
        .registry
        .create_as(
            &fx.pool,
            1,
            "purchase.order.group",
            vec![("order_ids", json!([[6, 0, [origin]]]))],
        )
        .await
        .expect("the group is created");
    fx.registry
        .write_as(
            &fx.pool,
            1,
            "purchase.order",
            others,
            vec![("purchase_group_id", json!(group))],
        )
        .await
        .expect("the alternatives join it");
}
