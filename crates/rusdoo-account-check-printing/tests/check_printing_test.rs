//! Check printing end to end: the two ways a check gets its number, the
//! refusal to reuse one, the batch that comes off a pre-printed pad, and
//! the layout that has to be chosen before anything prints at all.

use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::methods::{MethodCtx, MethodRegistry};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use std::sync::Arc;

fn registry() -> Registry {
    let mut reg = rusdoo_base::registry().unwrap();
    rusdoo_product::extend(&mut reg).unwrap();
    rusdoo_account::extend(&mut reg).unwrap();
    rusdoo_account_check_printing::extend(&mut reg).unwrap();
    reg
}

fn methods() -> MethodRegistry {
    let mut methods = MethodRegistry::new();
    rusdoo_account::extend_methods(&mut methods).unwrap();
    rusdoo_account_check_printing::extend_methods(&mut methods).unwrap();
    methods
}

struct Fixture {
    registry: Arc<Registry>,
    methods: MethodRegistry,
    pool: PgPool,
    partner: i64,
    company: i64,
    journal: i64,
    check_method: i64,
    transfer_method: i64,
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
        let ctx = MethodCtx::new(Arc::clone(&self.registry), &self.pool, 1, model, ids).with_rest(rest);
        entry.call(ctx, &args, &kwargs)
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

    /// A draft outbound payment, by check unless told otherwise.
    async fn a_check_payment(&self, amount: f64) -> i64 {
        self.a_payment(amount, self.check_method).await
    }

    async fn a_payment(&self, amount: f64, method: i64) -> i64 {
        self.registry
            .create_as(
                &self.pool,
                1,
                "account.payment",
                vec![
                    ("partner_id", json!(self.partner)),
                    ("company_id", json!(self.company)),
                    ("journal_id", json!(self.journal)),
                    ("payment_method_id", json!(method)),
                    ("payment_type", json!("outbound")),
                    ("amount", json!(amount)),
                    ("memo", json!("Rent")),
                ],
            )
            .await
            .unwrap()
    }

    /// A posted vendor bill, for the stub beside the check.
    async fn a_bill(&self, amount: f64, due: &str) -> i64 {
        self.registry
            .create_as(
                &self.pool,
                1,
                "account.move",
                vec![
                    ("partner_id", json!(self.partner)),
                    ("company_id", json!(self.company)),
                    ("move_type", json!("in_invoice")),
                    ("invoice_date", json!("2026-01-01")),
                    ("invoice_date_due", json!(due)),
                    (
                        "line_ids",
                        json!([[0, 0, {"name": "Rent", "quantity": 1, "price_unit": amount}]]),
                    ),
                ],
            )
            .await
            .unwrap()
    }

    /// Turn the journal into one that numbers its own checks, starting at
    /// `first`.
    async fn number_checks_from(&self, first: &str) {
        self.write(
            "account.journal",
            self.journal,
            vec![("check_manual_sequencing", json!(true))],
        )
        .await;
        self.call_with(
            "account.journal",
            "set_check_next_number",
            vec![self.journal],
            vec![json!(first)],
        )
        .await
        .expect("the journal takes the number");
    }

    /// Give the company a layout, so printing can get past the refusal.
    ///
    /// The base module's selection holds only "None"; a country module
    /// would add its report here with `selection_add`, and nothing
    /// validates the value against the selection, which is what lets this
    /// stand in for one.
    async fn with_a_check_layout(&self) {
        self.write(
            "res.company",
            self.company,
            vec![(
                "account_check_printing_layout",
                json!("l10n_us_check_printing.action_print_check_top"),
            )],
        )
        .await;
    }

    async fn payments_of_journal(&self) -> Vec<i64> {
        self.registry
            .search(
                &self.pool,
                "account.payment",
                &parse_domain(&json!([["journal_id", "=", self.journal]])).unwrap(),
                &SearchOptions::default(),
            )
            .await
            .unwrap()
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
    // the sequences the addons' data files load
    for (code, prefix) in [("account.move", "FAT/"), ("account.payment", "PAY/")] {
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
    let company = registry
        .create(&pool, "res.company", vec![("name", json!("Acme"))])
        .await
        .unwrap();
    let partner = registry
        .create(&pool, "res.partner", vec![("name", json!("Landlord"))])
        .await
        .unwrap();
    let journal = registry
        .create(
            &pool,
            "account.journal",
            vec![
                ("name", json!("Bank")),
                ("code", json!("BNK1")),
                ("type", json!("bank")),
                ("company_id", json!(company)),
            ],
        )
        .await
        .unwrap();
    // the addon's data file: the "Checks" outbound payment method
    let check_method = registry
        .create(
            &pool,
            "account.payment.method",
            vec![
                ("name", json!("Checks")),
                ("code", json!("check_printing")),
                ("payment_type", json!("outbound")),
            ],
        )
        .await
        .unwrap();
    let transfer_method = registry
        .create(
            &pool,
            "account.payment.method",
            vec![
                ("name", json!("Manual")),
                ("code", json!("manual")),
                ("payment_type", json!("outbound")),
            ],
        )
        .await
        .unwrap();
    Some(Fixture {
        registry: Arc::new(registry),
        methods: methods(),
        pool,
        partner,
        company,
        journal,
        check_method,
        transfer_method,
    })
}

/// Every test that needs a database, or a notice and a pass when there is
/// none.
macro_rules! fixture {
    ($case:expr) => {
        match fixture($case).await {
            Some(fx) => fx,
            None => {
                eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
                return;
            }
        }
    };
}

#[tokio::test]
async fn a_blank_check_is_numbered_when_the_payment_is_posted_live() {
    let fx = fixture!("rusdoo_account_check_printing_numbering");
    fx.number_checks_from("00042").await;

    // the journal shows the number it would hand out, without spending it
    let journal = fx
        .read("account.journal", fx.journal, &["check_next_number"])
        .await;
    assert_eq!(journal["check_next_number"], "00042");

    let payment = fx.a_check_payment(1234.5).await;
    // a draft is not numbered: the number is spent when the check leaves
    let row = fx
        .read(
            "account.payment",
            payment,
            &["check_number", "state", "check_amount_in_words", "name"],
        )
        .await;
    assert_eq!(row["check_number"], Value::Null);
    assert_eq!(row["state"], "draft");
    // the legal amount line is written the moment the amount is
    assert_eq!(
        row["check_amount_in_words"],
        "One Thousand Two Hundred And Thirty-Four and 50/100"
    );
    assert_eq!(row["name"], "PAY/00001", "a payment carries its own number");

    fx.call("account.payment", "action_post", vec![payment])
        .await
        .expect("the payment posts");
    let row = fx
        .read(
            "account.payment",
            payment,
            &["check_number", "state", "show_check_number"],
        )
        .await;
    assert_eq!(row["check_number"], "00042");
    assert_eq!(row["state"], "in_process");
    assert_eq!(row["show_check_number"], json!(true));

    // and the sequence moved on, keeping the width it was given
    let second = fx.a_check_payment(10.0).await;
    fx.call("account.payment", "action_post", vec![second])
        .await
        .unwrap();
    let row = fx.read("account.payment", second, &["check_number"]).await;
    assert_eq!(row["check_number"], "00043");
}

#[tokio::test]
async fn a_payment_that_is_not_a_check_is_never_numbered_live() {
    let fx = fixture!("rusdoo_account_check_printing_other_method");
    fx.number_checks_from("00042").await;

    let transfer = fx.a_payment(50.0, fx.transfer_method).await;
    fx.call("account.payment", "action_post", vec![transfer])
        .await
        .unwrap();
    let row = fx
        .read(
            "account.payment",
            transfer,
            &["check_number", "show_check_number"],
        )
        .await;
    assert_eq!(row["check_number"], Value::Null);
    assert_eq!(row["show_check_number"], json!(false));

    // the number it did not take is still there for the next check
    let check = fx.a_check_payment(50.0).await;
    fx.call("account.payment", "action_post", vec![check])
        .await
        .unwrap();
    assert_eq!(
        fx.read("account.payment", check, &["check_number"]).await["check_number"],
        "00042"
    );
}

#[tokio::test]
async fn the_journal_refuses_a_number_it_has_already_passed_live() {
    let fx = fixture!("rusdoo_account_check_printing_journal_number");
    fx.number_checks_from("100").await;

    let error = fx
        .call_with(
            "account.journal",
            "set_check_next_number",
            vec![fx.journal],
            vec![json!("99")],
        )
        .await
        .expect_err("the bank has seen 99 already");
    assert!(error.contains("The last check number was 100"), "{error}");

    let error = fx
        .call_with(
            "account.journal",
            "set_check_next_number",
            vec![fx.journal],
            vec![json!("F1234")],
        )
        .await
        .expect_err("letters are not a check number");
    assert!(error.contains("should only contains numbers"), "{error}");

    let error = fx
        .call_with(
            "account.journal",
            "set_check_next_number",
            vec![fx.journal],
            vec![json!("2147483648")],
        )
        .await
        .expect_err("the column does not hold it");
    assert!(error.contains("exceeds the maximum allowed value"), "{error}");

    // none of the refusals moved the sequence
    assert_eq!(
        fx.read("account.journal", fx.journal, &["check_next_number"])
            .await["check_next_number"],
        "100"
    );
}

#[tokio::test]
async fn a_check_number_with_letters_is_refused_by_the_model_live() {
    let fx = fixture!("rusdoo_account_check_printing_letters");
    let payment = fx.a_check_payment(10.0).await;
    let error = fx
        .registry
        .write_as(
            &fx.pool,
            1,
            "account.payment",
            &[payment],
            vec![("check_number", json!("F1234"))],
        )
        .await
        .expect_err("a check number is digits");
    assert!(
        error.to_string().contains("can only consist of digits"),
        "{error}"
    );
}

#[tokio::test]
async fn two_posted_checks_may_not_carry_the_same_number_live() {
    let fx = fixture!("rusdoo_account_check_printing_unique");
    fx.number_checks_from("00042").await;
    let first = fx.a_check_payment(10.0).await;
    fx.call("account.payment", "action_post", vec![first])
        .await
        .unwrap();

    // somebody types the number of a check that has already gone out —
    // and '42' is the same number to a bank as '00042'
    let second = fx.a_check_payment(20.0).await;
    fx.write("account.payment", second, vec![("check_number", json!("42"))])
        .await;
    let error = fx
        .call("account.payment", "action_post", vec![second])
        .await
        .expect_err("the number is spent");
    assert!(error.contains("already used"), "{error}");
    // and the payment stayed a draft: nothing was half-done
    assert_eq!(
        fx.read("account.payment", second, &["state"]).await["state"],
        "draft"
    );
}

#[tokio::test]
async fn a_voided_check_gives_its_number_back_live() {
    let fx = fixture!("rusdoo_account_check_printing_void");
    fx.number_checks_from("00042").await;
    let spoiled = fx.a_check_payment(10.0).await;
    fx.call("account.payment", "action_post", vec![spoiled])
        .await
        .unwrap();
    fx.call("account.payment", "action_void_check", vec![spoiled])
        .await
        .expect("the check is destroyed");

    let row = fx
        .read("account.payment", spoiled, &["state", "check_number"])
        .await;
    assert_eq!(row["state"], "canceled");
    // the number stays on the payment, so the bank statement can be
    // matched against it later
    assert_eq!(row["check_number"], "00042");

    // but it is free again: a voided check never left the building
    let replacement = fx.a_check_payment(10.0).await;
    fx.write(
        "account.payment",
        replacement,
        vec![("check_number", json!("00042"))],
    )
    .await;
    fx.call("account.payment", "action_post", vec![replacement])
        .await
        .expect("the number was given back");
}

#[tokio::test]
async fn printing_without_a_layout_says_which_setting_is_missing_live() {
    let fx = fixture!("rusdoo_account_check_printing_no_layout");
    fx.number_checks_from("00042").await;
    let payment = fx.a_check_payment(10.0).await;

    // the base module's selection holds only "None", so on its own it
    // prints nothing — which is what Odoo says of itself
    let error = fx
        .call("account.payment", "print_checks", vec![payment])
        .await
        .expect_err("there is no layout to print on");
    assert!(error.contains("choose a check layout"), "{error}");
    // and the check was not marked as sent by a print that never happened
    assert_eq!(
        fx.read("account.payment", payment, &["is_sent"]).await["is_sent"],
        json!(false)
    );
}

#[tokio::test]
async fn a_numbered_journal_prints_straight_away_live() {
    let fx = fixture!("rusdoo_account_check_printing_manual_print");
    fx.number_checks_from("00042").await;
    fx.with_a_check_layout().await;
    let payment = fx.a_check_payment(10.0).await;

    let action = fx
        .call("account.payment", "print_checks", vec![payment])
        .await
        .expect("the check prints");
    // no dialog: the system knows the numbers, so it just prints
    assert_eq!(action["type"], "ir.actions.report");
    assert_eq!(action["res_ids"], json!([payment]));

    let row = fx
        .read(
            "account.payment",
            payment,
            &["is_sent", "state", "check_number"],
        )
        .await;
    assert_eq!(row["is_sent"], json!(true));
    assert_eq!(row["state"], "in_process", "printing posts the draft");
    assert_eq!(row["check_number"], "00042");

    // a check that has already come out of the printer is not printed
    // again by the same button
    let error = fx
        .call("account.payment", "print_checks", vec![payment])
        .await
        .expect_err("it has been printed");
    assert!(error.contains("already been reconciled"), "{error}");

    // unless somebody says it did not actually come out
    fx.call("account.payment", "unmark_as_sent", vec![payment])
        .await
        .unwrap();
    assert_eq!(
        fx.read("account.payment", payment, &["is_sent"]).await["is_sent"],
        json!(false)
    );
}

#[tokio::test]
async fn a_pre_printed_pad_asks_which_sheet_is_on_top_live() {
    let fx = fixture!("rusdoo_account_check_printing_prenumbered");
    fx.with_a_check_layout().await;
    // the journal is left on its default: the paper carries the numbers
    let first = fx.a_check_payment(10.0).await;
    let second = fx.a_check_payment(20.0).await;

    let action = fx
        .call("account.payment", "print_checks", vec![first, second])
        .await
        .expect("the dialog opens");
    assert_eq!(action["res_model"], "print.prenumbered.checks");
    assert_eq!(action["target"], "new", "it is a dialog, not a screen");
    let wizard = action["res_id"].as_i64().expect("the wizard was born");

    let opened = fx
        .read(
            "print.prenumbered.checks",
            wizard,
            &["next_check_number", "payment_ids"],
        )
        .await;
    // nothing was ever printed on this journal, so it starts at one
    assert_eq!(opened["next_check_number"], "1");
    assert_eq!(opened["payment_ids"], json!([first, second]));

    // the operator reads the number off the top sheet
    fx.write(
        "print.prenumbered.checks",
        wizard,
        vec![("next_check_number", json!("00100"))],
    )
    .await;
    let action = fx
        .call("print.prenumbered.checks", "print_checks", vec![wizard])
        .await
        .expect("the batch prints");
    assert_eq!(action["type"], "ir.actions.report");
    // the dialog closes itself once the file is down
    assert_eq!(action["close_on_report_download"], json!(true));

    // the sheets go through in order, so the numbers do too
    for (payment, expected) in [(first, "00100"), (second, "00101")] {
        let row = fx
            .read(
                "account.payment",
                payment,
                &["check_number", "state", "is_sent"],
            )
            .await;
        assert_eq!(row["check_number"], expected);
        assert_eq!(row["state"], "in_process");
        assert_eq!(row["is_sent"], json!(true));
    }

    // and the next batch picks up where this one stopped
    let third = fx.a_check_payment(30.0).await;
    let action = fx
        .call("account.payment", "print_checks", vec![third])
        .await
        .unwrap();
    let wizard = action["res_id"].as_i64().unwrap();
    assert_eq!(
        fx.read("print.prenumbered.checks", wizard, &["next_check_number"])
            .await["next_check_number"],
        "00102"
    );
}

#[tokio::test]
async fn the_dialog_refuses_a_number_that_is_not_one_live() {
    let fx = fixture!("rusdoo_account_check_printing_wizard_refusal");
    fx.with_a_check_layout().await;
    let payment = fx.a_check_payment(10.0).await;
    let action = fx
        .call("account.payment", "print_checks", vec![payment])
        .await
        .unwrap();
    let wizard = action["res_id"].as_i64().unwrap();

    let error = fx
        .registry
        .write_as(
            &fx.pool,
            1,
            "print.prenumbered.checks",
            &[wizard],
            vec![("next_check_number", json!("A100"))],
        )
        .await
        .expect_err("letters are not a check number");
    assert!(
        error.to_string().contains("should only contains numbers"),
        "{error}"
    );
}

#[tokio::test]
async fn checks_from_two_journals_do_not_print_together_live() {
    let fx = fixture!("rusdoo_account_check_printing_two_journals");
    fx.with_a_check_layout().await;
    let other = fx
        .registry
        .create(
            &fx.pool,
            "account.journal",
            vec![
                ("name", json!("Second bank")),
                ("type", json!("bank")),
                ("company_id", json!(fx.company)),
            ],
        )
        .await
        .unwrap();
    let here = fx.a_check_payment(10.0).await;
    let there = fx.a_check_payment(10.0).await;
    fx.write("account.payment", there, vec![("journal_id", json!(other))])
        .await;

    let error = fx
        .call("account.payment", "print_checks", vec![here, there])
        .await
        .expect_err("one printer, one pad of paper");
    assert!(error.contains("same bank journal"), "{error}");
}

#[tokio::test]
async fn the_stub_lists_the_bills_the_check_pays_live() {
    let fx = fixture!("rusdoo_account_check_printing_stub");
    fx.number_checks_from("00042").await;
    let older = fx.a_bill(100.0, "2026-02-01").await;
    let newer = fx.a_bill(250.0, "2026-03-01").await;

    let payment = fx.a_check_payment(300.0).await;
    fx.write(
        "account.payment",
        payment,
        vec![("invoice_ids", json!([[6, 0, [newer, older]]]))],
    )
    .await;
    fx.call("account.payment", "action_post", vec![payment])
        .await
        .unwrap();

    let pages = fx
        .call("account.payment", "check_get_pages", vec![payment])
        .await
        .expect("the pages are built");
    let pages = pages.as_array().expect("a list of pages");
    assert_eq!(pages.len(), 1, "two bills fit beside one check");
    assert_eq!(pages[0]["sequence_number"], "00042");
    assert_eq!(pages[0]["manual_sequencing"], json!(true));
    assert_eq!(pages[0]["memo"], "Rent");
    assert!(
        pages[0]["amount_in_word"]
            .as_str()
            .unwrap()
            .starts_with("Three Hundred and 00/100 *"),
        "{}",
        pages[0]["amount_in_word"]
    );

    let lines = pages[0]["stub_lines"].as_array().expect("the stub lines");
    assert_eq!(lines.len(), 2);
    // oldest due first: that is the order the money goes out in
    assert_eq!(lines[0]["due_date"], "2026-02-01");
    assert_eq!(lines[0]["amount_total"], "100.00");
    assert_eq!(lines[0]["amount_paid"], "100.00");
    assert_eq!(lines[0]["amount_residual"], "-", "paid in full");
    // the check ran out on the second bill: 300 - 100 leaves 200 of 250
    assert_eq!(lines[1]["amount_paid"], "200.00");
    assert_eq!(lines[1]["amount_residual"], "50.00");
}

#[tokio::test]
async fn a_long_stub_is_cropped_or_spilled_as_the_company_asked_live() {
    let fx = fixture!("rusdoo_account_check_printing_stub_pages");
    fx.number_checks_from("00042").await;
    let count = rusdoo_account_check_printing::INV_LINES_PER_STUB + 1;
    let mut bills = Vec::with_capacity(count);
    for index in 0..count {
        bills.push(fx.a_bill(10.0, &format!("2026-02-{:02}", index + 1)).await);
    }
    let payment = fx.a_check_payment(10.0 * count as f64).await;
    fx.write(
        "account.payment",
        payment,
        vec![("invoice_ids", json!([[6, 0, bills]]))],
    )
    .await;
    fx.call("account.payment", "action_post", vec![payment])
        .await
        .unwrap();

    // cropped by default, with a line left for the ellipsis
    let pages = fx
        .call("account.payment", "check_get_pages", vec![payment])
        .await
        .unwrap();
    let pages = pages.as_array().unwrap();
    assert_eq!(pages.len(), 1);
    assert_eq!(
        pages[0]["stub_lines"].as_array().unwrap().len(),
        rusdoo_account_check_printing::INV_LINES_PER_STUB - 1
    );
    assert_eq!(pages[0]["stub_cropped"], json!(true));

    // or spilled over a second page, when the company wants the detail
    fx.write(
        "res.company",
        fx.company,
        vec![("account_check_printing_multi_stub", json!(true))],
    )
    .await;
    let pages = fx
        .call("account.payment", "check_get_pages", vec![payment])
        .await
        .unwrap();
    let pages = pages.as_array().unwrap();
    assert_eq!(pages.len(), 2);
    let printed: usize = pages
        .iter()
        .map(|page| page["stub_lines"].as_array().unwrap().len())
        .sum();
    assert_eq!(printed, count, "no bill is dropped");
    // only the first page is a check; the rest are stubs
    assert_eq!(pages[1]["amount"], "VOID");
    assert_eq!(pages[1]["amount_in_word"], "VOID");
    assert_eq!(pages[0]["stub_cropped"], json!(false));
}

#[tokio::test]
async fn the_dashboard_link_points_at_the_checks_still_to_print_live() {
    let fx = fixture!("rusdoo_account_check_printing_dashboard");
    fx.number_checks_from("00042").await;
    let payment = fx.a_check_payment(10.0).await;
    fx.call("account.payment", "action_post", vec![payment])
        .await
        .unwrap();

    let action = fx
        .call("account.journal", "action_checks_to_print", vec![fx.journal])
        .await
        .expect("the list opens");
    assert_eq!(action["res_model"], "account.payment");
    let domain = action["domain"].clone();
    // exactly what the dashboard counts: this journal's unprinted checks
    assert_eq!(
        domain,
        json!([
            ["journal_id", "=", fx.journal],
            ["payment_method_id.code", "=", "check_printing"],
            ["state", "=", "in_process"],
            ["is_sent", "=", false],
        ])
    );
    // and the domain finds it
    let found = fx
        .registry
        .search(
            &fx.pool,
            "account.payment",
            &parse_domain(&domain).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(found, vec![payment]);
    assert_eq!(fx.payments_of_journal().await, vec![payment]);
}
