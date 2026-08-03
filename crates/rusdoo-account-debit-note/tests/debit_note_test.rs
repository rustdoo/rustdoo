//! The debit note end to end: the dialog that opens over a posted
//! invoice, the lines that come (or do not) with it, and what the model
//! recusa a debitar.

use rusdoo_orm::methods::{MethodCtx, MethodRegistry};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use sqlx::PgPool;

/// Each test in its own schema: the suite runs in parallel and all
/// criam as mesmas tabelas.
fn pool(url: &str, schema: &'static str) -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
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
        .unwrap()
}

fn registry() -> Registry {
    let mut reg = rusdoo_base::registry().unwrap();
    rusdoo_product::extend(&mut reg).unwrap();
    rusdoo_account::extend(&mut reg).unwrap();
    rusdoo_account_debit_note::extend(&mut reg).unwrap();
    reg
}

fn methods() -> MethodRegistry {
    let mut methods = MethodRegistry::new();
    rusdoo_account::extend_methods(&mut methods).unwrap();
    rusdoo_account_debit_note::extend_methods(&mut methods).unwrap();
    methods
}

struct Fixture {
    registry: Registry,
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
            .unwrap_or_else(|| panic!("{method} deveria estar registrado em {model}"));
        let args: Vec<Value> = Vec::new();
        let kwargs = Map::new();
        let ctx = MethodCtx::new(&self.registry, &self.pool, 1, model, ids);
        (entry.func)(ctx, &args, &kwargs)
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
            .expect("o registro existe")
    }

    /// A customer invoice with two lines, posted.
    async fn a_posted_invoice(&self) -> i64 {
        let invoice = self
            .registry
            .create(
                &self.pool,
                "account.move",
                vec![
                    ("partner_id", json!(self.partner)),
                    ("move_type", json!("out_invoice")),
                    ("invoice_date", json!("2026-03-10")),
                    (
                        "line_ids",
                        json!([
                            [0, 0, {"product_id": self.product, "name": "Mesa",
                                    "quantity": 2, "price_unit": 1250}],
                            [0, 0, {"name": "Montagem", "quantity": 1, "price_unit": 300}],
                        ]),
                    ),
                ],
            )
            .await
            .unwrap();
        self.call("account.move", "action_post", vec![invoice])
            .await
            .expect("a fatura lança");
        invoice
    }

    /// Write the dialog's answers onto the wizard.
    async fn answer(&self, wizard: i64, values: Vec<(&str, Value)>) {
        self.registry
            .write_as(&self.pool, 1, "account.debit.note", &[wizard], values)
            .await
            .unwrap();
    }
}

async fn fixture(schema: &'static str) -> Option<Fixture> {
    let url = std::env::var("RUSDOO_TEST_DATABASE_URL").ok()?;
    let pool = pool(&url, schema);
    let registry = registry();
    for table in [
        "account_move_debit_move",
        "account_debit_note",
        "account_move_line",
        "account_move",
        "product_product",
        "res_partner",
        "res_company",
        "res_country",
        "res_users",
        "res_groups",
        "ir_sequence",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}" CASCADE"#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for model in [
        "ir.sequence",
        "res.country",
        "res.company",
        "res.partner",
        // `account.move`'s company default reads the creating user
        "res.groups",
        "res.users",
        "product.product",
        "account.move",
        "account.move.line",
        "account.debit.note",
    ] {
        registry
            .get(model)
            .unwrap()
            .init_table(&pool)
            .await
            .unwrap();
    }
    // the sequences both addons ship in their data files
    for (code, prefix) in [("account.move", "FAT/"), ("account.move.debit", "FAT/D")] {
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
            vec![("name", json!("Mesa")), ("list_price", json!(1250))],
        )
        .await
        .unwrap();
    Some(Fixture {
        registry,
        methods: methods(),
        pool,
        partner,
        product,
    })
}

#[tokio::test]
async fn a_posted_invoice_becomes_a_debit_note_live() {
    let Some(fx) = fixture(rusdoo_testing::schema_for("rusdoo_account_debit_note_flow")).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let invoice = fx.a_posted_invoice().await;

    let action = fx
        .call("account.move", "action_debit_note", vec![invoice])
        .await
        .expect("o diálogo abre");
    assert_eq!(action["res_model"], "account.debit.note");
    assert_eq!(action["target"], "new", "é um diálogo, não uma tela");
    let wizard = action["res_id"].as_i64().expect("o assistente nasceu");
    // the dialog already knows which invoice it is debiting
    let opened = fx
        .read("account.debit.note", wizard, &["move_ids", "date"])
        .await;
    assert_eq!(opened["move_ids"], json!([invoice]));
    assert!(opened["date"].is_string(), "a data vem preenchida");

    fx.answer(
        wizard,
        vec![
            ("reason", json!("Frete não cobrado")),
            ("copy_lines", json!(true)),
            ("date", json!("2026-04-01")),
        ],
    )
    .await;
    let action = fx
        .call("account.debit.note", "create_debit", vec![wizard])
        .await
        .expect("a nota sai");
    assert_eq!(action["res_model"], "account.move");
    let note = action["res_id"].as_i64().expect("a nota tem id");

    let row = fx
        .read(
            "account.move",
            note,
            &[
                "name",
                "state",
                "move_type",
                "ref",
                "invoice_date",
                "debit_origin_id",
                "amount_total",
            ],
        )
        .await;
    assert_eq!(row["state"], "draft", "a nota nasce rascunho");
    assert_eq!(row["move_type"], "out_invoice", "o tipo da origem");
    assert_eq!(row["invoice_date"], "2026-04-01", "a data do assistente");
    assert_eq!(row["debit_origin_id"][0], json!(invoice), "o vínculo ficou");
    // the reference says which document it came from and why
    assert_eq!(row["ref"], "FAT/00001, Frete não cobrado");
    // its own series: the note does not consume an invoice's number
    assert_eq!(row["name"], "FAT/D00001");
    // the lines came along, so the note is worth the same as the invoice
    assert_eq!(row["amount_total"], json!(2800.0));

    // e a fatura de origem sabe que foi debitada
    let origin = fx
        .read(
            "account.move",
            invoice,
            &["debit_note_count", "debit_note_ids"],
        )
        .await;
    assert_eq!(origin["debit_note_count"], json!(1));
    assert_eq!(origin["debit_note_ids"], json!([note]));

    let action = fx
        .call("account.move", "action_view_debit_notes", vec![invoice])
        .await
        .expect("a lista abre");
    assert_eq!(action["domain"], json!([["debit_origin_id", "=", invoice]]));
}

#[tokio::test]
async fn a_debit_note_without_copied_lines_is_born_empty_live() {
    let Some(fx) = fixture(rusdoo_testing::schema_for("rusdoo_account_debit_note_empty")).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let invoice = fx.a_posted_invoice().await;
    let action = fx
        .call("account.move", "action_debit_note", vec![invoice])
        .await
        .unwrap();
    let wizard = action["res_id"].as_i64().unwrap();

    // with no reason and no copy: the note is a blank document linked
    // to the invoice, for charging an item that was not there
    let action = fx
        .call("account.debit.note", "create_debit", vec![wizard])
        .await
        .expect("a nota sai");
    let note = action["res_id"].as_i64().unwrap();
    let row = fx
        .read("account.move", note, &["ref", "line_ids", "amount_total"])
        .await;
    assert_eq!(row["ref"], "FAT/00001", "só o número da origem");
    assert_eq!(row["line_ids"], json!([]));
    assert_eq!(row["amount_total"], json!(0.0));

    // and a note with no lines is not posted: there is nothing to charge
    let error = fx
        .call("account.move", "action_post", vec![note])
        .await
        .expect_err("uma nota vazia não se lança");
    assert!(error.contains("has no lines"), "{error}");
}

#[tokio::test]
async fn a_draft_invoice_is_not_debited_live() {
    let Some(fx) = fixture(rusdoo_testing::schema_for("rusdoo_account_debit_note_draft")).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let invoice = fx
        .registry
        .create(
            &fx.pool,
            "account.move",
            vec![
                ("partner_id", json!(fx.partner)),
                ("move_type", json!("out_invoice")),
                (
                    "line_ids",
                    json!([[0, 0, {"quantity": 1, "price_unit": 10}]]),
                ),
            ],
        )
        .await
        .unwrap();

    let error = fx
        .call("account.move", "action_debit_note", vec![invoice])
        .await
        .expect_err("a draft is not debited");
    assert!(error.contains("post it before"), "{error}");
    // and no wizard was left behind
    let wizards = fx
        .registry
        .search(
            &fx.pool,
            "account.debit.note",
            &rusdoo_orm::domain::parse_domain(&json!([])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .unwrap();
    assert!(wizards.is_empty(), "o diálogo nem chegou a abrir");
}

#[tokio::test]
async fn a_debit_note_is_not_debited_again_live() {
    let Some(fx) = fixture(rusdoo_testing::schema_for("rusdoo_account_debit_note_chain")).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let invoice = fx.a_posted_invoice().await;
    let action = fx
        .call("account.move", "action_debit_note", vec![invoice])
        .await
        .unwrap();
    let wizard = action["res_id"].as_i64().unwrap();
    fx.answer(wizard, vec![("copy_lines", json!(true))]).await;
    let note = fx
        .call("account.debit.note", "create_debit", vec![wizard])
        .await
        .unwrap()["res_id"]
        .as_i64()
        .unwrap();
    fx.call("account.move", "action_post", vec![note])
        .await
        .expect("a nota lança");

    // a chain of debit notes does not say where the charge came from:
    // debita-se a fatura de origem
    let error = fx
        .call("account.move", "action_debit_note", vec![note])
        .await
        .expect_err("a note is not debited");
    assert!(error.contains("already a debit note"), "{error}");
}

#[tokio::test]
async fn a_plain_entry_is_not_debited_live() {
    let Some(fx) = fixture(rusdoo_testing::schema_for("rusdoo_account_debit_note_entry")).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let entry = fx
        .registry
        .create(
            &fx.pool,
            "account.move",
            vec![
                ("partner_id", json!(fx.partner)),
                ("move_type", json!("entry")),
                (
                    "line_ids",
                    json!([[0, 0, {"quantity": 1, "price_unit": 10}]]),
                ),
            ],
        )
        .await
        .unwrap();
    fx.call("account.move", "action_post", vec![entry])
        .await
        .unwrap();

    let error = fx
        .call("account.move", "action_debit_note", vec![entry])
        .await
        .expect_err("an entry is not debited");
    assert!(error.contains("customer or vendor invoice"), "{error}");
}

#[tokio::test]
async fn a_batch_of_invoices_is_debited_at_once_live() {
    let Some(fx) = fixture(rusdoo_testing::schema_for("rusdoo_account_debit_note_batch")).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let first = fx.a_posted_invoice().await;
    let second = fx.a_posted_invoice().await;

    let action = fx
        .call("account.move", "action_debit_note", vec![first, second])
        .await
        .expect("o diálogo abre sobre as duas");
    let wizard = action["res_id"].as_i64().unwrap();
    let action = fx
        .call("account.debit.note", "create_debit", vec![wizard])
        .await
        .expect("as notas saem");
    // a batch does not open a form: it opens the list of what was issued
    assert_eq!(action["view_mode"], "list,form");
    let created: Vec<i64> = action["domain"][0][2]
        .as_array()
        .expect("os ids emitidos")
        .iter()
        .filter_map(Value::as_i64)
        .collect();
    assert_eq!(created.len(), 2);
    for (note, origin) in created.iter().zip([first, second]) {
        let row = fx.read("account.move", *note, &["debit_origin_id"]).await;
        assert_eq!(row["debit_origin_id"][0], json!(origin));
    }
}

#[tokio::test]
async fn a_cancelled_invoice_between_the_dialog_and_the_button_is_refused_live() {
    let Some(fx) = fixture(rusdoo_testing::schema_for("rusdoo_account_debit_note_race")).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let invoice = fx.a_posted_invoice().await;
    let action = fx
        .call("account.move", "action_debit_note", vec![invoice])
        .await
        .unwrap();
    let wizard = action["res_id"].as_i64().unwrap();

    // the dialog stayed open while somebody else cancelled the invoice
    fx.call("account.move", "action_cancel", vec![invoice])
        .await
        .unwrap();

    let error = fx
        .call("account.debit.note", "create_debit", vec![wizard])
        .await
        .expect_err("não se debita uma fatura cancelada");
    assert!(error.contains("post it before"), "{error}");
    // e nenhuma nota meio-criada ficou no banco
    let notes = fx
        .registry
        .search(
            &fx.pool,
            "account.move",
            &rusdoo_orm::domain::parse_domain(&json!([["debit_origin_id", "!=", false]])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .unwrap();
    assert!(notes.is_empty(), "{notes:?}");
}
