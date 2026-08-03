//! rusdoo-account-debit-note — port de `odoo/addons/account_debit_note/`:
//! the debit note.
//!
//! A debit note charges extra on an invoice that is already posted. It
//! is not a loose invoice: it keeps the link to the document it
//! corrected, and it is by that link that somebody, months later,
//! understands why the same
//! cliente foi cobrado duas vezes. Cancelar a fatura e emitir outra
//! would erase that history — which is exactly what the note exists to
//! avoid.
//!
//! The path is Odoo's: a wizard takes the chosen invoices,
//! pergunta a data, o motivo e se as linhas devem vir junto, e devolve a
//! action that opens what it created.

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};

/// The debit notes' own series (`ir.sequence`), so that a note does not
/// consume a number from the invoices' series: whoever checks a month's
/// numbering should not find gaps because there were corrections. It is
/// what Odoo does by prefixing "D" to the number when the journal is
/// marked `debit_sequence` — the default for
/// venda e de compra.
const DEBIT_SEQUENCE: &str = "account.move.debit";

/// What can be debited. Odoo also accepts credit notes (`out_refund`,
/// `in_refund`), types the port's `account.move` does not have yet.
const DEBITABLE: [&str; 2] = ["out_invoice", "in_invoice"];

fn char(name: &str) -> Field {
    Field::new(name, FieldType::Char { size: None })
}

fn m2o(name: &str, comodel: &str) -> Field {
    Field::new(
        name,
        FieldType::Many2one {
            comodel: comodel.to_string(),
        },
    )
}

/// The id inside a many2one, which reads as `[id, name]`.
fn first_id(value: &Value) -> Option<i64> {
    match value {
        Value::Array(items) => items.first().and_then(Value::as_i64),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

/// Hoje, como a data viaja no protocolo (`YYYY-MM-DD`).
///
/// Odoo uses the user's timezone (`fields.Date.context_today`); there
/// is no timezone on `res.users` here yet, so this is UTC — and whoever
/// opens the wizard sees the date and can change it.
fn today() -> String {
    chrono::Utc::now().date_naive().to_string()
}

/// `debit_note_count` — how many debit notes came out of this invoice.
fn debit_note_count(record: &Map<String, Value>) -> Value {
    let notes = record
        .get("debit_note_ids")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    json!(notes)
}

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(debited_move())?;
    reg.register(debit_note_wizard())?;
    Ok(())
}

/// The debit note's buttons: the two on the invoice and the dialog's.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    // opening a dialog and listing do not touch the invoice: they read
    methods.register(
        "account.move",
        "action_debit_note",
        Operation::Read,
        action_debit_note,
    )?;
    methods.register(
        "account.move",
        "action_view_debit_notes",
        Operation::Read,
        action_view_debit_notes,
    )?;
    methods.register(
        "account.debit.note",
        "create_debit",
        Operation::Write,
        create_debit,
    )?;
    Ok(())
}

/// `account.move` estendida (`_inherit`): a fatura ganha para onde
/// aponta e o que saiu dela.
fn debited_move() -> Model {
    Model::new(
        ModelMeta {
            name: "account.move".into(),
            table: "account_move".into(),
            inherit: vec!["account.move".into()],
            inherits: vec![],
        },
        vec![
            // not `readonly` here, unlike Odoo: in this ORM `readonly`
            // forbids the write, and it is the wizard that fills the
            // link in. What protects the field on screen is the form's
            // arch.
            m2o("debit_origin_id", "account.move"),
            Field::new(
                "debit_note_ids",
                FieldType::One2many {
                    comodel: "account.move".into(),
                    inverse: "debit_origin_id".into(),
                },
            ),
            // not materialised: the count changes when ANOTHER record
            // writes its `debit_origin_id`, and the recompute only
            // follows the fields of whatever is being written — a column
            // here
            // envelheceria calada
            Field::new("debit_note_count", FieldType::Integer)
                .computed(&["debit_note_ids"], debit_note_count),
        ],
    )
}

/// `account.debit.note` — the dialog that issues the note.
///
/// A `TransientModel`: the row is the state of an open dialog, not
/// something the business keeps. The many2many is what allows debiting
/// lote de faturas de uma vez, como no Odoo.
fn debit_note_wizard() -> Model {
    Model::new(
        ModelMeta {
            name: "account.debit.note".into(),
            table: "account_debit_note".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new(
                "move_ids",
                FieldType::Many2many {
                    comodel: "account.move".into(),
                    relation: "account_move_debit_move".into(),
                    column1: "debit_id".into(),
                    column2: "move_id".into(),
                },
            ),
            Field::new("date", FieldType::Date).required(),
            char("reason"),
            // unticked by default: a note usually charges something new
            // (a forgotten freight), not a repeat of what was charged
            Field::new("copy_lines", FieldType::Boolean).default_value(json!(false)),
        ],
    )
    .transient()
}

/// Why this invoice does not become a debit note.
///
/// It holds at both ends: when the dialog opens, so nobody fills in a
/// form that is going to fail; and at create time, because between one
/// and
/// outra a fatura pode ter sido cancelada.
fn refuse_undebitable(row: &Map<String, Value>) -> Result<(), RusdooError> {
    let name = row.get("name").and_then(Value::as_str).unwrap_or("");
    let state = row.get("state").and_then(Value::as_str).unwrap_or("draft");
    if state != "posted" {
        return Err(RusdooError::Validation(format!(
            "invoice {name} is {state:?}: post it before issuing the debit note"
        )));
    }
    if row.get("debit_origin_id").and_then(first_id).is_some() {
        return Err(RusdooError::Validation(format!(
            "{name} is already a debit note: debit the source invoice, not the note"
        )));
    }
    let move_type = row.get("move_type").and_then(Value::as_str).unwrap_or("");
    if !DEBITABLE.contains(&move_type) {
        return Err(RusdooError::Validation(format!(
            "{name} is of type {move_type:?}: only a customer or vendor invoice becomes a debit note"
        )));
    }
    Ok(())
}

/// `action_debit_note` — opens the dialog already pointing at the
/// invoices.
///
/// Odoo passes the selection through the context (`active_ids`) and
/// reads it in
/// `default_get`; aqui o registro do assistente nasce apontando, como no
/// `sale`'s cancel wizard. It is the same decision taken where the
/// invoice already is, and the dialog cannot open pointing at nothing.
fn action_debit_note<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation(
                "choose at least one invoice to debit".into(),
            ));
        }
        let moves = ctx
            .registry
            .read(
                ctx.pool,
                "account.move",
                &ctx.ids,
                &["name", "state", "move_type", "debit_origin_id"],
            )
            .await?;
        for row in &moves {
            refuse_undebitable(row)?;
        }
        let wizard = ctx
            .registry
            .create_as(
                ctx.pool,
                ctx.uid,
                "account.debit.note",
                vec![
                    ("move_ids", json!([[6, 0, ctx.ids]])),
                    ("date", json!(today())),
                ],
            )
            .await?;
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": "Debit note",
            "res_model": "account.debit.note",
            "res_id": wizard,
            "views": [[false, "form"]],
            // a dialog about the invoice, not a screen replacing it
            "target": "new",
        }))
    })
}

/// `action_view_debit_notes` — the notes that came out of this invoice.
fn action_view_debit_notes<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [move_id] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "view the notes of one invoice at a time".into(),
            ));
        };
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": "Debit notes",
            "res_model": "account.move",
            "view_mode": "list,form",
            "domain": [["debit_origin_id", "=", move_id]],
            "target": "current",
        }))
    })
}

/// `create_debit` — the dialog's button: issues each invoice's note.
fn create_debit<'a>(
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
                "account.debit.note",
                &[wizard_id],
                &["move_ids", "date", "reason", "copy_lines"],
            )
            .await?;
        let wizard = rows
            .first()
            .ok_or_else(|| RusdooError::Validation("the wizard is gone".into()))?;
        let move_ids: Vec<i64> = wizard
            .get("move_ids")
            .and_then(Value::as_array)
            .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        if move_ids.is_empty() {
            return Err(RusdooError::Validation(
                "the wizard points at no invoice: there is nothing to debit".into(),
            ));
        }
        // the note's date is the wizard's, not the debited invoice's:
        // the charge is born today, even when it corrects an old
        // document
        let date = wizard
            .get("date")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| RusdooError::Validation("say which date the note is issued on".into()))?;
        let reason = wizard
            .get("reason")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .map(str::to_string);
        let copy_lines = wizard
            .get("copy_lines")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let moves = ctx
            .registry
            .read(
                ctx.pool,
                "account.move",
                &move_ids,
                &[
                    "name",
                    "state",
                    "move_type",
                    "debit_origin_id",
                    "partner_id",
                    "company_id",
                    "line_ids",
                ],
            )
            .await?;
        // an invoice deleted while the dialog was open would vanish from
        // leitura, e o lote sairia menor do que quem clicou pediu
        if moves.len() != move_ids.len() {
            return Err(RusdooError::Validation(
                "one of the chosen invoices is gone: reopen the wizard".into(),
            ));
        }
        // tudo conferido antes de criar qualquer coisa: um lote que para
        // halfway leaves notes issued and an error on screen, and
        // nobody knows which ones came out
        for row in &moves {
            refuse_undebitable(row)?;
        }

        let mut created = Vec::with_capacity(moves.len());
        for row in &moves {
            created.push(emit_note(&ctx, row, &date, reason.as_deref(), copy_lines).await?);
        }

        // a single note opens straight away; a batch opens the list
        if let [only] = created[..] {
            return Ok(json!({
                "type": "ir.actions.act_window",
                "name": "Debit note",
                "res_model": "account.move",
                "res_id": only,
                "views": [[false, "form"]],
                "target": "current",
            }));
        }
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": "Debit notes",
            "res_model": "account.move",
            "view_mode": "list,form",
            "domain": [["id", "in", created]],
            "target": "current",
        }))
    })
}

/// One invoice's debit note, answering the id of what was created.
///
/// Ela nasce rascunho, como qualquer documento: quem emitiu ainda vai
/// look at what is being charged before posting.
async fn emit_note(
    ctx: &MethodCtx<'_>,
    origin: &Map<String, Value>,
    date: &str,
    reason: Option<&str>,
    copy_lines: bool,
) -> Result<i64, RusdooError> {
    let origin_id = origin
        .get("id")
        .and_then(Value::as_i64)
        .ok_or_else(|| RusdooError::Validation("the source invoice is gone".into()))?;
    let name = origin.get("name").and_then(Value::as_str).unwrap_or("");
    // the reason goes into the reference next to the source's number:
    // it is what shows on the customer's statement, and the number alone
    // explains nothing
    let reference = match reason {
        Some(reason) => format!("{name}, {reason}"),
        None => name.to_string(),
    };
    let lines = if copy_lines {
        copied_lines(ctx, origin).await?
    } else {
        Vec::new()
    };

    // mesmo tipo da origem: debitar uma fatura de fornecedor gera outra
    // a vendor bill, not a charge to the customer
    let move_type = origin
        .get("move_type")
        .cloned()
        .ok_or_else(|| RusdooError::Validation(format!("invoice {name} has no type")))?;
    let mut values: Vec<(&str, Value)> = vec![
        ("move_type", move_type),
        (
            "partner_id",
            json!(origin.get("partner_id").and_then(first_id)),
        ),
        (
            "company_id",
            json!(origin.get("company_id").and_then(first_id)),
        ),
        ("ref", json!(reference)),
        ("invoice_date", json!(date)),
        ("debit_origin_id", json!(origin_id)),
        ("line_ids", Value::Array(lines)),
    ];
    // without the notes' sequence installed, the document falls back to
    // the invoices' ordinary numbering instead of being born unnumbered
    if let Some(number) = ctx.registry.next_sequence(ctx.pool, DEBIT_SEQUENCE).await? {
        values.push(("name", json!(number)));
    }
    ctx.registry
        .create_as(ctx.pool, ctx.uid, "account.move", values)
        .await
}

/// The source invoice's lines, as create commands.
///
/// Copies, not links: the note is a document of its own, and editing
/// one's line must not rewrite the other's.
async fn copied_lines(
    ctx: &MethodCtx<'_>,
    origin: &Map<String, Value>,
) -> Result<Vec<Value>, RusdooError> {
    let line_ids: Vec<i64> = origin
        .get("line_ids")
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    if line_ids.is_empty() {
        return Ok(Vec::new());
    }
    let lines = ctx
        .registry
        .read(
            ctx.pool,
            "account.move.line",
            &line_ids,
            &["product_id", "name", "quantity", "price_unit", "sequence"],
        )
        .await?;
    Ok(lines
        .iter()
        .map(|line| {
            json!([0, 0, {
                "product_id": line.get("product_id").and_then(first_id),
                "name": line.get("name").cloned().unwrap_or(Value::Null),
                "quantity": line.get("quantity").cloned().unwrap_or_else(|| json!(0)),
                "price_unit": line.get("price_unit").cloned().unwrap_or_else(|| json!(0)),
                "sequence": line.get("sequence").cloned().unwrap_or_else(|| json!(10)),
            }])
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn posted_invoice() -> Map<String, Value> {
        let mut record = Map::new();
        record.insert("name".into(), json!("FAT/00001"));
        record.insert("state".into(), json!("posted"));
        record.insert("move_type".into(), json!("out_invoice"));
        record
    }

    #[test]
    fn a_posted_invoice_may_be_debited() {
        assert!(refuse_undebitable(&posted_invoice()).is_ok());
    }

    #[test]
    fn a_draft_invoice_may_not_be_debited() {
        let mut record = posted_invoice();
        record.insert("state".into(), json!("draft"));
        let error = refuse_undebitable(&record).expect_err("a draft is not debited");
        assert!(error.to_string().contains("post it before"));
    }

    #[test]
    fn a_debit_note_may_not_be_debited_again() {
        let mut record = posted_invoice();
        // como o many2one volta da leitura: [id, nome]
        record.insert("debit_origin_id".into(), json!([7, "FAT/00001"]));
        let error = refuse_undebitable(&record).expect_err("a note is not debited");
        assert!(error.to_string().contains("already a debit note"));
    }

    #[test]
    fn a_plain_entry_may_not_be_debited() {
        let mut record = posted_invoice();
        record.insert("move_type".into(), json!("entry"));
        let error = refuse_undebitable(&record).expect_err("an entry is not debited");
        assert!(error.to_string().contains("customer or vendor invoice"));
    }

    #[test]
    fn an_invoice_counts_the_notes_that_came_out_of_it() {
        let mut record = Map::new();
        record.insert("debit_note_ids".into(), json!([12, 13]));
        assert_eq!(debit_note_count(&record), json!(2));
        // and an invoice that was never debited counts zero, not null
        assert_eq!(debit_note_count(&Map::new()), json!(0));
    }

    #[test]
    fn the_models_extend_the_invoice_without_losing_it() {
        let mut reg = rusdoo_base::registry().unwrap();
        rusdoo_product::extend(&mut reg).unwrap();
        rusdoo_account::extend(&mut reg).unwrap();
        extend(&mut reg).unwrap();

        let mv = reg.get("account.move").unwrap();
        assert!(mv.field("debit_origin_id").is_some());
        assert!(mv.field("debit_note_count").is_some());
        // the extension adds; what the `account` module brought stays
        assert!(mv.field("amount_total").unwrap().stored);
        assert_eq!(mv.constraints().len(), 1, "a regra do vencimento sobrevive");
        assert_eq!(mv.meta.table, "account_move", "and the table is the same");

        let wizard = reg.get("account.debit.note").unwrap();
        assert!(wizard.is_transient(), "a wizard is not stored data");
    }
}
