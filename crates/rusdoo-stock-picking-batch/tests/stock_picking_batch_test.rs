//! The batch end to end: the dialog opened over a selection of
//! transfers, the batch it builds, the trip it validates, and everything
//! it refuses to group.

use rusdoo_orm::methods::{MethodCtx, MethodRegistry};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use std::sync::Arc;

/// The ids of an x2many, sorted.
///
/// A one2many comes back in the *comodel's* order — `stock.picking` is
/// ordered by date and then by id descending — so a test that cares
/// about membership must not spell an order it never promised.
fn ids(value: &Value) -> Vec<i64> {
    let mut ids: Vec<i64> = value
        .as_array()
        .expect("an x2many reads as a list")
        .iter()
        .filter_map(Value::as_i64)
        .collect();
    ids.sort_unstable();
    ids
}

fn registry() -> Registry {
    let mut reg = rusdoo_base::registry().unwrap();
    rusdoo_product::extend(&mut reg).unwrap();
    rusdoo_stock::extend(&mut reg).unwrap();
    rusdoo_stock_picking_batch::extend(&mut reg).unwrap();
    reg
}

fn methods() -> MethodRegistry {
    let mut methods = MethodRegistry::new();
    rusdoo_stock::extend_methods(&mut methods).unwrap();
    rusdoo_stock_picking_batch::extend_methods(&mut methods).unwrap();
    methods
}

struct Fixture {
    registry: Arc<Registry>,
    methods: MethodRegistry,
    pool: PgPool,
    partner: i64,
    product: i64,
}

impl Fixture {
    /// Call a model method the way the dispatch would.
    async fn call(&self, model: &str, method: &str, ids: Vec<i64>) -> Result<Value, String> {
        let entry = self
            .methods
            .get(model, method)
            .unwrap_or_else(|| panic!("{method} should be registered on {model}"));
        let args: Vec<Value> = Vec::new();
        let kwargs = Map::new();
        let ctx = MethodCtx::new(Arc::clone(&self.registry), &self.pool, 1, model, ids);
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

    async fn write(&self, model: &str, id: i64, values: Vec<(&str, Value)>) {
        self.registry
            .write_as(&self.pool, 1, model, &[id], values)
            .await
            .unwrap();
    }

    /// A transfer with one line, still a draft.
    async fn a_draft_transfer(&self, kind: &str) -> i64 {
        self.registry
            .create(
                &self.pool,
                "stock.picking",
                vec![
                    ("partner_id", json!(self.partner)),
                    ("picking_type", json!(kind)),
                    (
                        "move_ids",
                        json!([[0, 0, {"product_id": self.product, "name": "Table",
                                       "product_uom_qty": 3.0}]]),
                    ),
                ],
            )
            .await
            .unwrap()
    }

    /// The same, confirmed — what the warehouse actually batches.
    async fn a_transfer(&self, kind: &str) -> i64 {
        let picking = self.a_draft_transfer(kind).await;
        self.call("stock.picking", "action_confirm", vec![picking])
            .await
            .expect("the transfer confirms");
        picking
    }

    /// Open one of the two dialogs over `pickings` and answer its form.
    async fn dialog(&self, button: &str, pickings: Vec<i64>) -> Result<i64, String> {
        let action = self.call("stock.picking", button, pickings).await?;
        assert_eq!(action["target"], "new", "a dialog, not a screen");
        Ok(action["res_id"].as_i64().expect("the dialog was born"))
    }

    /// The batch a dialog built, out of the action it answered with.
    async fn attach(&self, model: &str, wizard: i64) -> Result<i64, String> {
        let action = self.call(model, "attach_pickings", vec![wizard]).await?;
        assert_eq!(action["res_model"], "stock.picking.batch");
        Ok(action["res_id"].as_i64().expect("the batch has an id"))
    }
}

async fn fixture(case: &str) -> Option<Fixture> {
    let pool = rusdoo_testing::pool_in(case)?;
    let registry = registry();
    registry.init_tables(&pool).await.unwrap();
    // the superuser every call is made as, like a real boot leaves it
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
    let company = registry
        .create(&pool, "res.company", vec![("name", json!("Warehouse SA"))])
        .await
        .unwrap();
    // the batch's company default reads it off the acting user
    registry
        .write(
            &pool,
            "res.users",
            &[1],
            vec![("company_id", json!(company))],
        )
        .await
        .unwrap();
    // the series the two addons' data files load
    for (code, prefix) in [
        ("stock.picking.out", "WH/OUT/"),
        ("stock.picking.in", "WH/IN/"),
        (rusdoo_stock_picking_batch::BATCH_SEQUENCE, "BATCH/"),
        (rusdoo_stock_picking_batch::WAVE_SEQUENCE, "WAVE/"),
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
            .unwrap();
    }
    let partner = registry
        .create(&pool, "res.partner", vec![("name", json!("Ana"))])
        .await
        .unwrap();
    let product = registry
        .create(
            &pool,
            "product.product",
            vec![("name", json!("Table")), ("list_price", json!(1250))],
        )
        .await
        .unwrap();
    Some(Fixture {
        registry: Arc::new(registry),
        methods: methods(),
        pool,
        partner,
        product,
    })
}

#[tokio::test]
async fn two_transfers_travel_as_one_batch_live() {
    let Some(fx) = fixture("rusdoo_stock_picking_batch_flow").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let first = fx.a_transfer("outgoing").await;
    let second = fx.a_transfer("outgoing").await;

    let wizard = fx
        .dialog("action_add_to_batch", vec![first, second])
        .await
        .expect("the dialog opens over the selection");
    // it already knows what it is batching
    let opened = fx
        .read("stock.picking.to.batch", wizard, &["picking_ids", "mode"])
        .await;
    assert_eq!(opened["picking_ids"], json!([first, second]));
    assert_eq!(opened["mode"], "new", "a new batch, unless told otherwise");
    fx.write(
        "stock.picking.to.batch",
        wizard,
        vec![("description", json!("Morning round"))],
    )
    .await;

    let batch = fx
        .attach("stock.picking.to.batch", wizard)
        .await
        .expect("the batch is built");
    let row = fx
        .read(
            "stock.picking.batch",
            batch,
            &[
                "name",
                "state",
                "picking_type",
                "is_wave",
                "picking_ids",
                "picking_count",
                "description",
                "company_id",
            ],
        )
        .await;
    // its own series, and the operation type of what it carries
    assert_eq!(row["name"], "BATCH/00001");
    assert_eq!(row["picking_type"], "outgoing");
    assert_eq!(row["is_wave"], json!(false));
    assert_eq!(row["description"], "Morning round");
    assert!(row["company_id"].is_array(), "the batch has a company");
    // a batch created from the dialog starts right away
    assert_eq!(row["state"], "in_progress");
    assert_eq!(ids(&row["picking_ids"]), vec![first, second]);
    assert_eq!(row["picking_count"], json!(2));
    // and each transfer knows which trip carries it
    for picking in [first, second] {
        let carried = fx.read("stock.picking", picking, &["batch_id"]).await;
        assert_eq!(carried["batch_id"][0], json!(batch));
    }

    // one click validates the whole trip
    fx.call("stock.picking.batch", "action_done", vec![batch])
        .await
        .expect("the batch is validated");
    assert_eq!(
        fx.read("stock.picking.batch", batch, &["state"]).await["state"],
        "done"
    );
    for picking in [first, second] {
        let row = fx
            .read("stock.picking", picking, &["state", "move_ids"])
            .await;
        assert_eq!(row["state"], "done");
        // what left is what was planned, like validating the transfer
        // on its own would have recorded
        let move_id = row["move_ids"][0].as_i64().unwrap();
        let line = fx.read("stock.move", move_id, &["quantity_done"]).await;
        assert_eq!(line["quantity_done"], json!(3.0));
    }

    // a trip that happened is not deleted
    let error = fx
        .registry
        .unlink_as(&fx.pool, 1, "stock.picking.batch", &[batch])
        .await
        .expect_err("a done batch stays");
    assert!(error.to_string().contains("is done"), "{error}");
}

#[tokio::test]
async fn a_wave_is_numbered_from_its_own_series_live() {
    let Some(fx) = fixture("rusdoo_stock_picking_batch_wave").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let first = fx.a_transfer("outgoing").await;
    let second = fx.a_transfer("outgoing").await;

    let wizard = fx
        .dialog("action_add_to_wave", vec![first, second])
        .await
        .unwrap();
    let opened = fx.read("stock.add.to.wave", wizard, &["mode"]).await;
    assert_eq!(opened["mode"], "existing", "a wave usually already exists");
    // there is none yet, so this one is new
    fx.write("stock.add.to.wave", wizard, vec![("mode", json!("new"))])
        .await;

    let wave = fx.attach("stock.add.to.wave", wizard).await.unwrap();
    let row = fx
        .read(
            "stock.picking.batch",
            wave,
            &["name", "is_wave", "state", "picking_count"],
        )
        .await;
    // WAVE/, not BATCH/: a wave never eats a batch's number
    assert_eq!(row["name"], "WAVE/00001");
    assert_eq!(row["is_wave"], json!(true));
    assert_eq!(row["state"], "in_progress");
    assert_eq!(row["picking_count"], json!(2));

    // a third transfer joins the wave that is already open
    let third = fx.a_transfer("outgoing").await;
    let wizard = fx.dialog("action_add_to_wave", vec![third]).await.unwrap();
    fx.write("stock.add.to.wave", wizard, vec![("wave_id", json!(wave))])
        .await;
    let same = fx.attach("stock.add.to.wave", wizard).await.unwrap();
    assert_eq!(same, wave, "it joined, it did not start another one");
    assert_eq!(
        fx.read("stock.picking.batch", wave, &["picking_count"])
            .await["picking_count"],
        json!(3)
    );
}

#[tokio::test]
async fn a_receipt_does_not_travel_with_a_delivery_live() {
    let Some(fx) = fixture("rusdoo_stock_picking_batch_types").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let delivery = fx.a_transfer("outgoing").await;
    let receipt = fx.a_transfer("incoming").await;

    // the dialog does not even open over a mixed selection
    let error = fx
        .call(
            "stock.picking",
            "action_add_to_batch",
            vec![delivery, receipt],
        )
        .await
        .expect_err("one trip, one operation type");
    assert!(error.contains("same operation type"), "{error}");

    // and a receipt cannot be added to a batch of deliveries either
    let wizard = fx
        .dialog("action_add_to_batch", vec![delivery])
        .await
        .unwrap();
    let batch = fx.attach("stock.picking.to.batch", wizard).await.unwrap();
    let wizard = fx
        .dialog("action_add_to_batch", vec![receipt])
        .await
        .unwrap();
    fx.write(
        "stock.picking.to.batch",
        wizard,
        vec![("mode", json!("existing")), ("batch_id", json!(batch))],
    )
    .await;
    let error = fx
        .attach("stock.picking.to.batch", wizard)
        .await
        .expect_err("a receipt is not a delivery");
    assert!(error.contains("WH/OUT/00002"), "{error}");
    assert!(error.contains("operation types"), "{error}");
    // the refused transfer stayed out of the batch
    let row = fx.read("stock.picking", receipt, &["batch_id"]).await;
    assert!(row["batch_id"].is_null(), "{row:?}");
}

#[tokio::test]
async fn a_finished_transfer_is_not_batched_live() {
    let Some(fx) = fixture("rusdoo_stock_picking_batch_states").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let done = fx.a_transfer("outgoing").await;
    fx.call("stock.picking", "action_done", vec![done])
        .await
        .unwrap();
    let open = fx.a_transfer("outgoing").await;

    let wizard = fx
        .dialog("action_add_to_batch", vec![done, open])
        .await
        .unwrap();
    let error = fx
        .attach("stock.picking.to.batch", wizard)
        .await
        .expect_err("what has already left does not travel again");
    assert!(error.contains("WH/OUT/00001"), "{error}");
    assert!(error.contains("check their states"), "{error}");
}

#[tokio::test]
async fn a_draft_batch_waits_for_the_shift_live() {
    let Some(fx) = fixture("rusdoo_stock_picking_batch_draft").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    // drafts, because a draft transfer only joins a batch that has not
    // started either
    let first = fx.a_draft_transfer("outgoing").await;
    let second = fx.a_draft_transfer("outgoing").await;

    let wizard = fx
        .dialog("action_add_to_batch", vec![first, second])
        .await
        .unwrap();
    fx.write(
        "stock.picking.to.batch",
        wizard,
        vec![("is_create_draft", json!(true))],
    )
    .await;
    let batch = fx.attach("stock.picking.to.batch", wizard).await.unwrap();
    assert_eq!(
        fx.read("stock.picking.batch", batch, &["state"]).await["state"],
        "draft",
        "prepared now, started later"
    );
    assert_eq!(
        fx.read("stock.picking", first, &["state"]).await["state"],
        "draft",
        "and its transfers were not confirmed behind the user's back"
    );

    // the shift starts
    fx.call("stock.picking.batch", "action_confirm", vec![batch])
        .await
        .expect("the batch starts");
    assert_eq!(
        fx.read("stock.picking.batch", batch, &["state"]).await["state"],
        "in_progress"
    );
    for picking in [first, second] {
        assert_eq!(
            fx.read("stock.picking", picking, &["state"]).await["state"],
            "confirmed",
            "confirming the batch confirms what it carries"
        );
    }
}

#[tokio::test]
async fn an_empty_batch_is_not_confirmed_live() {
    let Some(fx) = fixture("rusdoo_stock_picking_batch_empty").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let batch = fx
        .registry
        .create(
            &fx.pool,
            "stock.picking.batch",
            vec![("picking_type", json!("outgoing"))],
        )
        .await
        .unwrap();
    for button in ["action_confirm", "action_done"] {
        let error = fx
            .call("stock.picking.batch", button, vec![batch])
            .await
            .expect_err("a trip with nothing to carry is not a trip");
        assert!(error.contains("you have to set some transfers"), "{error}");
    }
}

#[tokio::test]
async fn a_cancelled_batch_lets_its_transfers_go_live() {
    let Some(fx) = fixture("rusdoo_stock_picking_batch_cancel").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let first = fx.a_transfer("outgoing").await;
    let second = fx.a_transfer("outgoing").await;
    let wizard = fx
        .dialog("action_add_to_batch", vec![first, second])
        .await
        .unwrap();
    let batch = fx.attach("stock.picking.to.batch", wizard).await.unwrap();

    fx.call("stock.picking.batch", "action_cancel", vec![batch])
        .await
        .expect("the trip is off");
    let row = fx
        .read("stock.picking.batch", batch, &["state", "picking_count"])
        .await;
    assert_eq!(row["state"], "cancel");
    assert_eq!(row["picking_count"], json!(0));
    // cancelling the grouping does not cancel the deliveries: the
    // customers are still waiting for them
    for picking in [first, second] {
        let carried = fx
            .read("stock.picking", picking, &["state", "batch_id"])
            .await;
        assert_eq!(carried["state"], "confirmed");
        assert!(carried["batch_id"].is_null(), "{carried:?}");
    }
    // and a cancelled batch takes nothing more
    let wizard = fx.dialog("action_add_to_batch", vec![first]).await.unwrap();
    fx.write(
        "stock.picking.to.batch",
        wizard,
        vec![("mode", json!("existing")), ("batch_id", json!(batch))],
    )
    .await;
    let error = fx
        .attach("stock.picking.to.batch", wizard)
        .await
        .expect_err("a cancelled trip takes no passengers");
    assert!(error.contains("no more transfers"), "{error}");
}

#[tokio::test]
async fn two_batches_merge_into_the_one_due_first_live() {
    let Some(fx) = fixture("rusdoo_stock_picking_batch_merge").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let first = fx.a_transfer("outgoing").await;
    let second = fx.a_transfer("outgoing").await;
    let wizard = fx.dialog("action_add_to_batch", vec![first]).await.unwrap();
    let target = fx.attach("stock.picking.to.batch", wizard).await.unwrap();
    let wizard = fx
        .dialog("action_add_to_batch", vec![second])
        .await
        .unwrap();
    fx.write(
        "stock.picking.to.batch",
        wizard,
        vec![("description", json!("Afternoon round"))],
    )
    .await;
    let other = fx.attach("stock.picking.to.batch", wizard).await.unwrap();

    fx.write(
        "stock.picking.batch",
        target,
        vec![("scheduled_date", json!("2026-08-10 08:00:00"))],
    )
    .await;
    fx.write(
        "stock.picking.batch",
        other,
        vec![("scheduled_date", json!("2026-08-04 08:00:00"))],
    )
    .await;

    let action = fx
        .call("stock.picking.batch", "action_merge", vec![target, other])
        .await
        .expect("two batches of one shape merge");
    assert_eq!(action["res_id"], json!(target), "the first one survives");
    let row = fx
        .read(
            "stock.picking.batch",
            target,
            &[
                "picking_count",
                "picking_ids",
                "scheduled_date",
                "description",
            ],
        )
        .await;
    assert_eq!(row["picking_count"], json!(2));
    assert_eq!(ids(&row["picking_ids"]), vec![first, second]);
    // the values come from the batch that was due first: a merge must
    // not push a trip's date back
    assert_eq!(row["scheduled_date"], "2026-08-04 08:00:00");
    assert_eq!(row["description"], "Afternoon round");
    // and the emptied batch is gone
    let left = fx
        .registry
        .read(&fx.pool, "stock.picking.batch", &[other], &["name"])
        .await
        .unwrap();
    assert!(left.is_empty(), "{left:?}");
}

#[tokio::test]
async fn a_batch_does_not_merge_with_a_wave_live() {
    let Some(fx) = fixture("rusdoo_stock_picking_batch_unmergeable").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let first = fx.a_transfer("outgoing").await;
    let second = fx.a_transfer("outgoing").await;
    let wizard = fx.dialog("action_add_to_batch", vec![first]).await.unwrap();
    let batch = fx.attach("stock.picking.to.batch", wizard).await.unwrap();
    let wizard = fx.dialog("action_add_to_wave", vec![second]).await.unwrap();
    fx.write("stock.add.to.wave", wizard, vec![("mode", json!("new"))])
        .await;
    let wave = fx.attach("stock.add.to.wave", wizard).await.unwrap();

    let error = fx
        .call("stock.picking.batch", "action_merge", vec![batch, wave])
        .await
        .expect_err("a wave is not a batch");
    assert!(error.contains("wave transfers"), "{error}");
    // one alone is not a merge either
    let error = fx
        .call("stock.picking.batch", "action_merge", vec![batch])
        .await
        .expect_err("a merge needs two");
    assert!(error.contains("at least two"), "{error}");
}

#[tokio::test]
async fn a_transfer_says_which_batch_carries_it_live() {
    let Some(fx) = fixture("rusdoo_stock_picking_batch_view").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let alone = fx.a_transfer("outgoing").await;
    let error = fx
        .call("stock.picking", "action_view_batch", vec![alone])
        .await
        .expect_err("this one travels by itself");
    assert!(error.contains("not in a batch"), "{error}");

    let wizard = fx.dialog("action_add_to_batch", vec![alone]).await.unwrap();
    let batch = fx.attach("stock.picking.to.batch", wizard).await.unwrap();
    let action = fx
        .call("stock.picking", "action_view_batch", vec![alone])
        .await
        .expect("the batch opens");
    assert_eq!(action["res_model"], "stock.picking.batch");
    assert_eq!(action["res_id"], json!(batch));
    assert_eq!(action["target"], "current");
}
