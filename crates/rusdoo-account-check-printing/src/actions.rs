//! The buttons: numbering a check when the payment is posted, printing a
//! batch of them, and voiding one that came out wrong.

use crate::models::{CHECK_PRINTING, LAYOUT_DISABLED, STATE_CANCELED, STATE_DRAFT, STATE_IN_PROCESS};
use crate::stub;
use crate::{numbering, words};
use rusdoo_core::RusdooError;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::methods::{MethodCtx, MethodFuture};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// The `ir.sequence` code a journal's check numbering lives under.
///
/// Odoo points at the sequence by id and draws with `next_by_id()`; this
/// ORM only draws by code, so every journal's sequence carries one of its
/// own. It is derived from the journal id rather than stored separately
/// so that the two can never drift apart.
fn sequence_code(journal_id: i64) -> String {
    format!("account.check.printing.{journal_id}")
}

/// The width Odoo gives a fresh check sequence.
const DEFAULT_PADDING: i64 = 5;

/// The id inside a many2one, which reads back as `[id, name]`.
fn first_id(value: Option<&Value>) -> Option<i64> {
    match value {
        Some(Value::Array(items)) => items.first().and_then(Value::as_i64),
        Some(Value::Number(number)) => number.as_i64(),
        _ => None,
    }
}

fn text<'a>(record: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    record
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn number(record: &Map<String, Value>, name: &str) -> f64 {
    record
        .get(name)
        .and_then(|value| match value {
            Value::Number(n) => n.as_f64(),
            Value::String(raw) => raw.parse().ok(),
            _ => None,
        })
        .unwrap_or(0.0)
}

fn ids_of(record: &Map<String, Value>, name: &str) -> Vec<i64> {
    record
        .get(name)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

/// A payment, in the terms this module cares about.
struct Check {
    id: i64,
    name: String,
    state: String,
    journal_id: Option<i64>,
    method_code: Option<String>,
    is_sent: bool,
    check_number: Option<String>,
}

impl Check {
    fn is_check(&self) -> bool {
        self.method_code.as_deref() == Some(CHECK_PRINTING)
    }
}

const PAYMENT_FIELDS: [&str; 6] = [
    "name",
    "state",
    "journal_id",
    "payment_method_id",
    "is_sent",
    "check_number",
];

/// Read `ids` as checks, resolving the payment method's code — which is
/// two reads and not a join, because this ORM does not read dotted paths.
async fn load_checks(ctx: &MethodCtx<'_>, ids: &[i64]) -> Result<Vec<Check>, RusdooError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = ctx
        .registry
        .read(ctx.pool, "account.payment", ids, &PAYMENT_FIELDS)
        .await?;
    let mut method_ids: Vec<i64> = rows
        .iter()
        .filter_map(|row| first_id(row.get("payment_method_id")))
        .collect();
    method_ids.sort_unstable();
    method_ids.dedup();
    let mut codes: HashMap<i64, String> = HashMap::new();
    if !method_ids.is_empty() {
        for method in ctx
            .registry
            .read(ctx.pool, "account.payment.method", &method_ids, &["code"])
            .await?
        {
            let (Some(id), Some(code)) = (method.get("id").and_then(Value::as_i64), text(&method, "code"))
            else {
                continue;
            };
            codes.insert(id, code.to_string());
        }
    }
    Ok(rows
        .iter()
        .filter_map(|row| {
            let id = row.get("id").and_then(Value::as_i64)?;
            Some(Check {
                id,
                name: text(row, "name").unwrap_or("").to_string(),
                state: text(row, "state").unwrap_or(STATE_DRAFT).to_string(),
                journal_id: first_id(row.get("journal_id")),
                method_code: first_id(row.get("payment_method_id"))
                    .and_then(|method| codes.get(&method).cloned()),
                is_sent: row.get("is_sent").and_then(Value::as_bool).unwrap_or(false),
                check_number: text(row, "check_number").map(str::to_string),
            })
        })
        .collect())
}

/// The journal's check sequence, created on first use.
///
/// Odoo creates it in `account.journal.create` and, for databases that
/// predate the module, in the `post_init_hook`. This ORM has no create
/// hook and the port has no module installation hooks, so the sequence is
/// made the first time a check is numbered — which covers both cases at
/// once and cannot leave a journal without one.
async fn check_sequence(ctx: &MethodCtx<'_>, journal_id: i64) -> Result<String, RusdooError> {
    let rows = ctx
        .registry
        .read(
            ctx.pool,
            "account.journal",
            &[journal_id],
            &["name", "check_sequence_id"],
        )
        .await?;
    let journal = rows
        .first()
        .ok_or_else(|| RusdooError::Validation(format!("journal {journal_id} is gone")))?;
    if let Some(sequence) = first_id(journal.get("check_sequence_id")) {
        let found = ctx
            .registry
            .read(ctx.pool, "ir.sequence", &[sequence], &["code"])
            .await?;
        if let Some(code) = found.first().and_then(|row| text(row, "code")) {
            return Ok(code.to_string());
        }
    }
    let code = sequence_code(journal_id);
    let name = format!(
        "{}: Check Number Sequence",
        text(journal, "name").unwrap_or("Bank")
    );
    let sequence = ctx
        .registry
        .create_as(
            ctx.pool,
            ctx.uid,
            "ir.sequence",
            vec![
                ("name", json!(name)),
                ("code", json!(code)),
                ("padding", json!(DEFAULT_PADDING)),
                ("number_increment", json!(1)),
                ("number_next", json!(1)),
            ],
        )
        .await?;
    ctx.registry
        .write_as(
            ctx.pool,
            ctx.uid,
            "account.journal",
            &[journal_id],
            vec![("check_sequence_id", json!(sequence))],
        )
        .await?;
    Ok(code)
}

/// Every check number already used in `journal`, by the payments that
/// count — port of `_constrains_check_number_unique`, which only looks at
/// posted moves: a number on a cancelled payment is free again, because
/// that check was voided and never left the building.
async fn numbers_in_journal(
    ctx: &MethodCtx<'_>,
    journal_id: i64,
) -> Result<Vec<(i64, String)>, RusdooError> {
    let domain = parse_domain(&json!([
        ["journal_id", "=", journal_id],
        ["state", "in", [STATE_IN_PROCESS, "paid"]]
    ]))?;
    let ids = ctx
        .registry
        .search(ctx.pool, "account.payment", &domain, &SearchOptions::default())
        .await?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(ctx
        .registry
        .read(ctx.pool, "account.payment", &ids, &["check_number"])
        .await?
        .iter()
        .filter_map(|row| {
            let id = row.get("id").and_then(Value::as_i64)?;
            Some((id, text(row, "check_number")?.to_string()))
        })
        .collect())
}

/// Refuse a number the journal has already spent.
///
/// `'0042'` and `'42'` are the same number to a bank, so the comparison
/// is numeric — which is what Odoo's `check_number::BIGINT` is doing.
async fn refuse_duplicates(
    ctx: &MethodCtx<'_>,
    journal_id: i64,
    proposed: &[(i64, String)],
) -> Result<(), RusdooError> {
    if proposed.is_empty() {
        return Ok(());
    }
    let mut taken: HashMap<i64, i64> = HashMap::new();
    for (id, used) in numbers_in_journal(ctx, journal_id).await? {
        if let Some(value) = numbering::numeric(&used) {
            taken.insert(value, id);
        }
    }
    for (id, wanted) in proposed {
        let Some(value) = numbering::numeric(wanted) else {
            continue;
        };
        match taken.get(&value) {
            Some(other) if other != id => {
                return Err(RusdooError::Validation(format!(
                    "The following numbers are already used:\n{wanted} in this journal"
                )))
            }
            _ => {
                taken.insert(value, *id);
            }
        }
    }
    Ok(())
}

/// Move the drafts among `checks` into `in_process`, drawing a check
/// number first for the ones the journal numbers itself.
///
/// This is the whole of Odoo's `action_post` override: draw
/// `sequence.next_by_id()` for every check payment on a manually
/// sequenced journal, then let the payment be posted.
async fn post_checks(ctx: &MethodCtx<'_>, checks: &[&Check]) -> Result<(), RusdooError> {
    let drafts: Vec<&&Check> = checks
        .iter()
        .filter(|check| check.state == STATE_DRAFT)
        .collect();
    if drafts.is_empty() {
        return Ok(());
    }
    // which journals number their own checks
    let mut journal_ids: Vec<i64> = drafts.iter().filter_map(|check| check.journal_id).collect();
    journal_ids.sort_unstable();
    journal_ids.dedup();
    let mut manual: HashMap<i64, bool> = HashMap::new();
    if !journal_ids.is_empty() {
        for row in ctx
            .registry
            .read(
                ctx.pool,
                "account.journal",
                &journal_ids,
                &["check_manual_sequencing"],
            )
            .await?
        {
            let Some(id) = row.get("id").and_then(Value::as_i64) else {
                continue;
            };
            manual.insert(
                id,
                row.get("check_manual_sequencing")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            );
        }
    }

    let mut drawn: HashMap<i64, Vec<(i64, String)>> = HashMap::new();
    for check in &drafts {
        let Some(journal_id) = check.journal_id else {
            continue;
        };
        if !check.is_check() || !manual.get(&journal_id).copied().unwrap_or(false) {
            continue;
        }
        // a number already written by hand onto the payment is kept: the
        // sequence is a convenience, not an override of what the user said
        let number = match &check.check_number {
            Some(existing) => existing.clone(),
            None => {
                let code = check_sequence(ctx, journal_id).await?;
                ctx.registry
                    .next_sequence(ctx.pool, &code)
                    .await?
                    .ok_or_else(|| {
                        RusdooError::Validation(format!(
                            "the check sequence of journal {journal_id} is missing"
                        ))
                    })?
            }
        };
        drawn.entry(journal_id).or_default().push((check.id, number));
    }

    // every number is checked against the journal before any of them is
    // written: a batch that stops halfway would leave some checks
    // numbered and the error on screen, with nobody able to say which
    for (journal_id, proposed) in &drawn {
        refuse_duplicates(ctx, *journal_id, proposed).await?;
    }
    for proposed in drawn.values() {
        for (id, number) in proposed {
            ctx.registry
                .write_as(
                    ctx.pool,
                    ctx.uid,
                    "account.payment",
                    &[*id],
                    vec![("check_number", json!(number))],
                )
                .await?;
        }
    }
    let draft_ids: Vec<i64> = drafts.iter().map(|check| check.id).collect();
    ctx.registry
        .write_as(
            ctx.pool,
            ctx.uid,
            "account.payment",
            &draft_ids,
            vec![("state", json!(STATE_IN_PROCESS))],
        )
        .await
}

/// `action_post` — the payment leaves, and the check gets its number.
pub fn action_post<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation(
                "the action needs at least one payment".into(),
            ));
        }
        let checks = load_checks(&ctx, &ctx.ids).await?;
        for check in &checks {
            if check.state != STATE_DRAFT {
                return Err(RusdooError::Validation(format!(
                    "payment {} is {:?} and cannot be posted",
                    check.name, check.state
                )));
            }
        }
        let all: Vec<&Check> = checks.iter().collect();
        post_checks(&ctx, &all).await?;
        Ok(json!(true))
    })
}

/// `action_cancel` — the payment never happened.
pub fn action_cancel<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation(
                "the action needs at least one payment".into(),
            ));
        }
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "account.payment",
                &ctx.ids,
                vec![("state", json!(STATE_CANCELED))],
            )
            .await?;
        Ok(json!(true))
    })
}

/// `action_void_check` — the check came out wrong and is destroyed.
///
/// Odoo writes this as `action_draft()` then `action_cancel()`, which is
/// two state changes to reach one state; the result is the same and the
/// number stays on the payment, so the voided check can still be
/// accounted for when the bank statement arrives.
pub fn action_void_check<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation(
                "the action needs at least one payment".into(),
            ));
        }
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "account.payment",
                &ctx.ids,
                vec![("state", json!(STATE_CANCELED))],
            )
            .await?;
        Ok(json!(true))
    })
}

/// `unmark_as_sent` — the check did not come out of the printer after
/// all, so it may be printed again.
pub fn unmark_as_sent<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation(
                "the action needs at least one payment".into(),
            ));
        }
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "account.payment",
                &ctx.ids,
                vec![("is_sent", json!(false))],
            )
            .await?;
        Ok(json!(true))
    })
}

/// `print_checks` — the button on the payment and on the list.
pub fn print_checks<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let checks = load_checks(&ctx, &ctx.ids).await?;
        // the button can be pressed over a whole list, so what arrives is
        // not necessarily what this module can print
        let valid: Vec<&Check> = checks
            .iter()
            .filter(|check| check.is_check() && !check.is_sent)
            .collect();
        if valid.is_empty() {
            return Err(RusdooError::Validation(
                "Payments to print as a checks must have 'Check' selected as payment method and \
                 not have already been reconciled"
                    .into(),
            ));
        }
        let journal_id = valid[0].journal_id.ok_or_else(|| {
            RusdooError::Validation("a check has to be drawn on a bank journal".into())
        })?;
        if valid.iter().any(|check| check.journal_id != Some(journal_id)) {
            return Err(RusdooError::Validation(
                "In order to print multiple checks at once, they must belong to the same bank \
                 journal."
                    .into(),
            ));
        }
        let manual = ctx
            .registry
            .read(
                ctx.pool,
                "account.journal",
                &[journal_id],
                &["check_manual_sequencing"],
            )
            .await?
            .first()
            .and_then(|row| row.get("check_manual_sequencing").and_then(Value::as_bool))
            .unwrap_or(false);
        let valid_ids: Vec<i64> = valid.iter().map(|check| check.id).collect();

        if manual {
            post_checks(&ctx, &valid).await?;
            return do_print(&ctx, &valid_ids, false).await;
        }

        // pre-printed paper: the numbers are on the sheets already, so the
        // system asks which one is on top instead of deciding
        let last = numbers_in_journal(&ctx, journal_id)
            .await?
            .into_iter()
            .filter_map(|(_, used)| numbering::numeric(&used).map(|value| (value, used)))
            .max_by_key(|(value, _)| *value)
            .map(|(_, used)| used);
        let wizard = ctx
            .registry
            .create_as(
                ctx.pool,
                ctx.uid,
                "print.prenumbered.checks",
                vec![
                    ("next_check_number", json!(numbering::next_after(last.as_deref()))),
                    ("payment_ids", json!([[6, 0, valid_ids]])),
                ],
            )
            .await?;
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": "Print Pre-numbered Checks",
            "res_model": "print.prenumbered.checks",
            "res_id": wizard,
            "views": [[false, "form"]],
            "target": "new",
        }))
    })
}

/// `do_print_checks` — mark the checks sent and hand back the report.
pub fn do_print_checks<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let ids = ctx.ids.clone();
        do_print(&ctx, &ids, false).await
    })
}

/// The layout to print on, and the refusal when there is none.
///
/// Odoo raises a `RedirectWarning` that offers to open the settings; this
/// ORM has no such error, so the sentence is the same and the offer is
/// gone. It is the message that matters — "you have to choose a check
/// layout" is actionable, a missing report id is not.
async fn do_print(
    ctx: &MethodCtx<'_>,
    ids: &[i64],
    close_on_download: bool,
) -> Result<Value, RusdooError> {
    if ids.is_empty() {
        return Err(RusdooError::Validation("there is no check to print".into()));
    }
    let payments = ctx
        .registry
        .read(ctx.pool, "account.payment", ids, &["journal_id", "company_id"])
        .await?;
    let first = payments
        .first()
        .ok_or_else(|| RusdooError::Validation("the payments are gone".into()))?;
    let mut layout = None;
    if let Some(journal_id) = first_id(first.get("journal_id")) {
        layout = ctx
            .registry
            .read(
                ctx.pool,
                "account.journal",
                &[journal_id],
                &["bank_check_printing_layout"],
            )
            .await?
            .first()
            .and_then(|row| text(row, "bank_check_printing_layout").map(str::to_string));
    }
    if layout.is_none() {
        if let Some(company_id) = first_id(first.get("company_id")) {
            layout = ctx
                .registry
                .read(
                    ctx.pool,
                    "res.company",
                    &[company_id],
                    &["account_check_printing_layout"],
                )
                .await?
                .first()
                .and_then(|row| text(row, "account_check_printing_layout").map(str::to_string));
        }
    }
    let layout = match layout {
        Some(layout) if layout != LAYOUT_DISABLED => layout,
        _ => {
            return Err(RusdooError::Validation(
                "You have to choose a check layout. For this, go in Invoicing/Accounting \
                 Settings, search for 'Checks layout' and set one."
                    .into(),
            ))
        }
    };
    ctx.registry
        .write_as(
            ctx.pool,
            ctx.uid,
            "account.payment",
            ids,
            vec![("is_sent", json!(true))],
        )
        .await?;
    let mut action = json!({
        "type": "ir.actions.report",
        "report_type": "qweb-pdf",
        "report_name": layout,
        "res_model": "account.payment",
        "res_ids": ids,
    });
    if close_on_download {
        action["close_on_report_download"] = json!(true);
    }
    Ok(action)
}

/// `print_checks` on the wizard — the numbers on the paper are written
/// onto the payments, then the batch is printed.
pub fn wizard_print_checks<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [wizard_id] = ctx.ids[..] else {
            return Err(RusdooError::Validation("the wizard is gone".into()));
        };
        let rows = ctx
            .registry
            .read(
                ctx.pool,
                "print.prenumbered.checks",
                &[wizard_id],
                &["next_check_number", "payment_ids"],
            )
            .await?;
        let wizard = rows
            .first()
            .ok_or_else(|| RusdooError::Validation("the wizard is gone".into()))?;
        let entered = text(wizard, "next_check_number").ok_or_else(|| {
            RusdooError::Validation("say which number is on the first sheet".into())
        })?;
        if !numbering::is_check_number(entered) {
            return Err(RusdooError::Validation(
                "Next Check Number should only contains numbers.".into(),
            ));
        }
        let width = entered.len() as i64;
        let start: i64 = entered
            .parse()
            .map_err(|_| RusdooError::Validation("that is not a check number".into()))?;
        let payment_ids = ids_of(wizard, "payment_ids");
        if payment_ids.is_empty() {
            return Err(RusdooError::Validation(
                "the wizard points at no payment: there is nothing to print".into(),
            ));
        }
        let checks = load_checks(&ctx, &payment_ids).await?;
        let all: Vec<&Check> = checks.iter().collect();
        post_checks(&ctx, &all).await?;

        // the sheets go through the printer in order, so the numbers do
        // too: payment n gets the nth sheet
        let journal_id = checks
            .first()
            .and_then(|check| check.journal_id)
            .ok_or_else(|| RusdooError::Validation("the payments have no journal".into()))?;
        let proposed: Vec<(i64, String)> = checks
            .iter()
            .enumerate()
            .map(|(index, check)| (check.id, numbering::padded(start + index as i64, width)))
            .collect();
        refuse_duplicates(&ctx, journal_id, &proposed).await?;
        for (id, number) in &proposed {
            ctx.registry
                .write_as(
                    ctx.pool,
                    ctx.uid,
                    "account.payment",
                    &[*id],
                    vec![("check_number", json!(number))],
                )
                .await?;
        }
        do_print(&ctx, &payment_ids, true).await
    })
}

/// `set_check_next_number` — the journal's "Next Check Number" box.
///
/// A method and not a writable field: Odoo's `check_next_number` is a
/// compute with an `inverse=` that writes the sequence, and this ORM has
/// no inverse. The number arrives as the call's first positional
/// argument.
pub fn set_check_next_number<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [journal_id] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "set the next check number of one journal at a time".into(),
            ));
        };
        let entered = ctx
            .rest
            .first()
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| ctx.rest.first().and_then(Value::as_i64).map(|n| n.to_string()))
            .ok_or_else(|| {
                RusdooError::Validation("say which number the next check carries".into())
            })?;
        let code = check_sequence(&ctx, journal_id).await?;
        let sequence_ids = ctx
            .registry
            .search(
                ctx.pool,
                "ir.sequence",
                &parse_domain(&json!([["code", "=", code]]))?,
                &SearchOptions::default(),
            )
            .await?;
        let [sequence_id] = sequence_ids[..] else {
            return Err(RusdooError::Validation(
                "the journal's check sequence is missing".into(),
            ));
        };
        let current = ctx
            .registry
            .read(ctx.pool, "ir.sequence", &[sequence_id], &["number_next"])
            .await?
            .first()
            .and_then(|row| row.get("number_next").and_then(Value::as_i64))
            .unwrap_or(1);
        let next = numbering::accept_next_number(&entered, current)?;
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "ir.sequence",
                &[sequence_id],
                vec![
                    ("number_next", json!(next)),
                    // the padding follows what was typed, so a journal
                    // told "00042" keeps printing five digits
                    ("padding", json!(entered.len() as i64)),
                ],
            )
            .await?;
        Ok(json!(true))
    })
}

/// `action_checks_to_print` — the journal dashboard's link.
pub fn action_checks_to_print<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [journal_id] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "open the checks of one journal at a time".into(),
            ));
        };
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": "Checks to Print",
            "res_model": "account.payment",
            "view_mode": "list,form",
            "domain": [
                ["journal_id", "=", journal_id],
                ["payment_method_id.code", "=", CHECK_PRINTING],
                ["state", "=", STATE_IN_PROCESS],
                ["is_sent", "=", false],
            ],
            "target": "current",
        }))
    })
}

/// Money on a check, with no currency to format it by.
///
/// Odoo runs this through `formatLang(..., currency_obj=...)`, which
/// knows the symbol, its side and the separators. The port has no
/// `res.currency`, so the figure is printed plainly rather than with a
/// symbol nobody declared.
fn money(value: f64) -> String {
    format!("{value:.2}")
}

/// The type groups Odoo puts headers on, in its order.
const TYPE_GROUPS: [(&[&str], &str); 2] = [
    (&["in_invoice", "in_receipt"], "Bills"),
    (&["out_refund"], "Refunds"),
];

/// One stub line, port of `prepare_vals`.
fn stub_line(invoice: &Map<String, Value>, sign: f64, paid: f64) -> Value {
    let name = text(invoice, "name").unwrap_or("/");
    // the reference goes next to the number: an invoice number alone
    // tells the supplier nothing about which of their documents it is
    let document = match text(invoice, "ref") {
        Some(reference) => format!("{name} - {reference}"),
        None => name.to_string(),
    };
    let total = number(invoice, "amount_total");
    let residual = total - paid;
    json!({
        "due_date": text(invoice, "invoice_date_due")
            .or_else(|| text(invoice, "invoice_date"))
            .unwrap_or(""),
        "number": document,
        "amount_total": money(sign * total),
        // an invoice paid in full says so with a dash, not with a zero
        // the reader has to interpret
        "amount_residual": if residual.abs() < 0.005 { "-".to_string() } else { money(sign * residual) },
        "amount_paid": money(sign * paid),
    })
}

/// Port of `_check_make_stub_pages` for a payment with no journal entry.
///
/// Odoo has two branches: one that decodes the reconciliation of the
/// payment's move, and one that walks the bills spending the payment's
/// amount as it goes. This port has no reconciliation, so only the second
/// exists — and it is the one that matters for a check, which is written
/// before anything is reconciled.
async fn stub_lines(
    ctx: &MethodCtx<'_>,
    invoice_ids: &[i64],
    mut remaining: f64,
) -> Result<Vec<Value>, RusdooError> {
    if invoice_ids.is_empty() {
        return Ok(Vec::new());
    }
    let invoices = ctx
        .registry
        .read(
            ctx.pool,
            "account.move",
            invoice_ids,
            &[
                "name",
                "ref",
                "move_type",
                "invoice_date",
                "invoice_date_due",
                "amount_total",
            ],
        )
        .await?;
    // grouped in Odoo's order, so the headers read the same way
    let mut grouped: Vec<(&str, Vec<&Map<String, Value>>)> = Vec::new();
    for (types, label) in TYPE_GROUPS {
        let mut group: Vec<&Map<String, Value>> = invoices
            .iter()
            .filter(|invoice| {
                text(invoice, "move_type").is_some_and(|kind| types.contains(&kind))
            })
            .collect();
        if group.is_empty() {
            continue;
        }
        // oldest due first: that is the order they will be paid in
        group.sort_by(|left, right| {
            let key = |invoice: &Map<String, Value>| {
                text(invoice, "invoice_date_due")
                    .or_else(|| text(invoice, "invoice_date"))
                    .unwrap_or("")
                    .to_string()
            };
            key(left).cmp(&key(right))
        });
        grouped.push((label, group));
    }

    let mut lines = Vec::new();
    let several = grouped.len() > 1;
    for (label, group) in grouped {
        // a single group needs no heading: the stub is obviously bills
        if several {
            lines.push(json!({ "header": true, "name": label }));
        }
        let sign = if label == "Bills" { 1.0 } else { -1.0 };
        for invoice in group {
            if remaining <= 0.0 {
                break;
            }
            let total = number(invoice, "amount_total");
            let paid = remaining.min(total);
            lines.push(stub_line(invoice, sign, paid));
            remaining -= paid;
        }
    }
    Ok(lines)
}

/// `_check_get_pages` — what the check template is handed.
pub fn check_get_pages<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [payment_id] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "a check is printed one payment at a time".into(),
            ));
        };
        let rows = ctx
            .registry
            .read(
                ctx.pool,
                "account.payment",
                &[payment_id],
                &[
                    "name",
                    "state",
                    "date",
                    "amount",
                    "memo",
                    "partner_id",
                    "company_id",
                    "journal_id",
                    "check_number",
                    "check_amount_in_words",
                    "invoice_ids",
                ],
            )
            .await?;
        let payment = rows
            .first()
            .ok_or_else(|| RusdooError::Validation("the payment is gone".into()))?;

        let multi_stub = match first_id(payment.get("company_id")) {
            Some(company_id) => ctx
                .registry
                .read(
                    ctx.pool,
                    "res.company",
                    &[company_id],
                    &["account_check_printing_multi_stub"],
                )
                .await?
                .first()
                .and_then(|row| {
                    row.get("account_check_printing_multi_stub")
                        .and_then(Value::as_bool)
                })
                .unwrap_or(false),
            None => false,
        };
        let manual = match first_id(payment.get("journal_id")) {
            Some(journal_id) => ctx
                .registry
                .read(
                    ctx.pool,
                    "account.journal",
                    &[journal_id],
                    &["check_manual_sequencing"],
                )
                .await?
                .first()
                .and_then(|row| row.get("check_manual_sequencing").and_then(Value::as_bool))
                .unwrap_or(false),
            None => false,
        };

        let amount = number(payment, "amount");
        let lines = stub_lines(&ctx, &ids_of(payment, "invoice_ids"), amount).await?;
        let pages = stub::paginate(lines.clone(), multi_stub);
        // "the list goes on" — Odoo tells the template so it can draw the
        // ellipsis line it left room for
        let cropped = !multi_stub && lines.len() > stub::INV_LINES_PER_STUB;

        let mut header = Map::new();
        header.insert(
            "sequence_number".into(),
            json!(text(payment, "check_number")),
        );
        header.insert("manual_sequencing".into(), json!(manual));
        header.insert("date".into(), json!(text(payment, "date")));
        header.insert(
            "partner_name".into(),
            json!(payment
                .get("partner_id")
                .and_then(Value::as_array)
                .and_then(|pair| pair.get(1))
                .and_then(Value::as_str)),
        );
        header.insert("state".into(), json!(text(payment, "state")));
        header.insert("amount".into(), json!(money(amount)));
        header.insert(
            "amount_in_word".into(),
            json!(words::fill_line(
                text(payment, "check_amount_in_words").unwrap_or("")
            )),
        );
        header.insert("memo".into(), json!(text(payment, "memo")));
        header.insert("stub_cropped".into(), json!(cropped));

        Ok(Value::Array(stub::build_pages(&header, pages)))
    })
}
