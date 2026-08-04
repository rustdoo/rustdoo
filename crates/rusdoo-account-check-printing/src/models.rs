//! The models check printing hangs off.
//!
//! **Three of them do not belong here.** `account.journal`,
//! `account.payment` and `account.payment.method` are `account`'s models
//! in Odoo, and `account_check_printing` only extends them with
//! `_inherit`. This port's `rusdoo-account` has the invoice and nothing
//! else — no journal, no payment — so there was nothing to extend, and an
//! addon about the numbering of checks cannot be written without the
//! record a check *is*.
//!
//! They are declared here as new models, cut down to what check printing
//! actually touches, and marked so the integrator can lift them into
//! `rusdoo-account` unchanged; this crate then goes back to what Odoo
//! writes, an `_inherit` that adds five fields. The report says the same
//! thing in more detail.

use crate::numbering;
use crate::words;
use rusdoo_orm::defaults;
use rusdoo_orm::fields::{Field, FieldType, OnDelete};
use rusdoo_orm::model::{Model, ModelMeta};
use serde_json::{json, Map, Value};

/// The `code` of the payment method this module ships
/// (`account_payment_method_check`). Everything keys off it: a payment is
/// a check when its method says so, and not because of anything on the
/// journal.
pub const CHECK_PRINTING: &str = "check_printing";

/// The company selection every country-specific check module adds its
/// layout to with `selection_add`. Alone, it holds only "None" — which is
/// why this module prints nothing on its own, exactly like Odoo's.
pub const LAYOUT_DISABLED: &str = "disabled";

/// A payment is what this module numbers, and only these states are
/// numbered or printed (Odoo 19's `account.payment`).
pub const STATE_DRAFT: &str = "draft";
pub const STATE_IN_PROCESS: &str = "in_process";
pub const STATE_CANCELED: &str = "canceled";

/// Money, at the precision the rest of the port keeps it in.
const AMOUNT: FieldType = FieldType::Float {
    digits: Some((16, 2)),
};

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

fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![],
        inherits: vec![],
    }
}

fn extending(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![name.to_string()],
        inherits: vec![],
    }
}

/// The first value of a dependency read through a many2one — the ORM
/// hands those back as a one-element list, the same shape as an x2many.
fn through_m2o<'a>(record: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
    match record.get(path) {
        Some(Value::Array(items)) => items.first(),
        other => other,
    }
}

fn dotted_i64(record: &Map<String, Value>, path: &str) -> Option<i64> {
    through_m2o(record, path).and_then(Value::as_i64)
}

fn dotted_str<'a>(record: &'a Map<String, Value>, path: &str) -> Option<&'a str> {
    through_m2o(record, path).and_then(Value::as_str)
}

/// A numeric field's value, whatever shape the driver decoded it in.
fn number(record: &Map<String, Value>, name: &str) -> f64 {
    record
        .get(name)
        .and_then(|value| match value {
            Value::Number(n) => n.as_f64(),
            Value::String(text) => text.parse().ok(),
            _ => None,
        })
        .unwrap_or(0.0)
}

/// `check_next_number` — the number the journal's sequence would hand out
/// next, formatted the way it will be printed.
///
/// Odoo reads `sequence.get_next_char(sequence.number_next_actual)`,
/// which is the number *without* consuming it: this is a preview shown on
/// the journal's form, not a draw.
pub fn check_next_number(record: &Map<String, Value>) -> Value {
    let next = dotted_i64(record, "check_sequence_id.number_next").unwrap_or(1);
    let padding = dotted_i64(record, "check_sequence_id.padding").unwrap_or(0);
    json!(numbering::padded(next, padding))
}

/// `check_amount_in_words` — the legal amount line.
pub fn check_amount_in_words(record: &Map<String, Value>) -> Value {
    json!(words::amount_in_words(number(record, "amount")))
}

/// `show_check_number` — whether the form shows the number at all.
///
/// Odoo's own compute, kept because it is the reason the field is not
/// simply always visible: a payment made by transfer has no check number,
/// and an empty "Check Number" box on its form is a question the user
/// cannot answer.
pub fn show_check_number(record: &Map<String, Value>) -> Value {
    let is_check = dotted_str(record, "payment_method_id.code") == Some(CHECK_PRINTING);
    let numbered = record
        .get("check_number")
        .and_then(Value::as_str)
        .is_some_and(|number| !number.is_empty());
    json!(is_check && numbered)
}

/// Port of `_constrains_check_number`.
fn digits_only(record: &Map<String, Value>) -> Result<(), String> {
    match record.get("check_number").and_then(Value::as_str) {
        // no number is not a wrong number: most payments have none
        None | Some("") => Ok(()),
        Some(number) if numbering::is_check_number(number) => Ok(()),
        Some(_) => Err("Check numbers can only consist of digits".into()),
    }
}

/// Port of the wizard's `_check_next_check_number`.
fn wizard_digits_only(record: &Map<String, Value>) -> Result<(), String> {
    match record.get("next_check_number").and_then(Value::as_str) {
        None | Some("") => Ok(()),
        Some(number) if numbering::is_check_number(number) => Ok(()),
        Some(_) => Err("Next Check Number should only contains numbers.".into()),
    }
}

/// `account.payment.method` — how a payment leaves the company.
///
/// *Belongs in `rusdoo-account`.* Odoo's model carries the registry of
/// method information (`_get_payment_method_information`, which this
/// module adds `check_printing` to); here the row itself is the whole
/// declaration, shipped by the addon's data file.
pub fn payment_method() -> Model {
    Model::new(
        meta("account.payment.method", "account_payment_method"),
        vec![
            char("name").required().translatable(),
            char("code").required(),
            Field::new(
                "payment_type",
                FieldType::Selection(vec![
                    ("inbound".into(), "Inbound".into()),
                    ("outbound".into(), "Outbound".into()),
                ]),
            )
            .required(),
        ],
    )
    // Odoo's `name_code_unique`: a code names one method per direction,
    // and the whole module keys off the code
    .sql_constrained(
        "account_payment_method_code_uniq",
        r#"UNIQUE ("code", "payment_type")"#,
        "a payment method with that code already exists for that direction",
    )
    .ordered("name, id")
}

/// `account.journal` — the bank account the checks are drawn on.
///
/// *Belongs in `rusdoo-account`, except for the four check fields.* Those
/// four are what `account_check_printing/models/account_journal.py` adds:
/// whether the checks are numbered by hand, the sequence that numbers
/// them, the next number, and which layout this journal prints on.
pub fn journal() -> Model {
    Model::new(
        meta("account.journal", "account_journal"),
        vec![
            char("name").required(),
            char("code"),
            Field::new(
                "type",
                FieldType::Selection(vec![
                    ("bank".into(), "Bank".into()),
                    ("cash".into(), "Cash".into()),
                    ("sale".into(), "Sales".into()),
                    ("purchase".into(), "Purchase".into()),
                    ("general".into(), "Miscellaneous".into()),
                ]),
            )
            .required()
            .default_value(json!("bank")),
            m2o("company_id", "res.company").default_from(defaults::USER_COMPANY),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            // --- what account_check_printing adds ---
            // "Check this option if your pre-printed checks are not
            // numbered": the numbers then come from the journal's own
            // sequence instead of being read off the paper.
            Field::new("check_manual_sequencing", FieldType::Boolean).default_value(json!(false)),
            // readonly in Odoo too: the sequence is the module's, not
            // something a user picks from a dropdown
            m2o("check_sequence_id", "ir.sequence").ondelete(OnDelete::SetNull),
            Field::new("check_next_number", FieldType::Char { size: None }).computed(
                &[
                    "check_sequence_id.number_next",
                    "check_sequence_id.padding",
                ],
                check_next_number,
            ),
            // the selection is empty until a country-specific module adds
            // its layout, which is exactly Odoo's state here
            Field::new("bank_check_printing_layout", FieldType::Selection(vec![])),
        ],
    )
    .ordered("name, id")
}

/// `account.payment` — the money going out, and the check that carries
/// it.
///
/// *Belongs in `rusdoo-account`, except for the check fields.* What is
/// missing next to Odoo's is everything about accounting: there is no
/// move, no reconciliation and no outstanding account here, so a payment
/// records what was paid and against which bills, and nothing is posted
/// to a ledger. `invoice_ids` is a plain many2many for that reason —
/// Odoo derives it from the reconciliation, which does not exist.
pub fn payment() -> Model {
    Model::new(
        meta("account.payment", "account_payment"),
        vec![
            char("name").from_sequence("account.payment"),
            m2o("partner_id", "res.partner"),
            m2o("company_id", "res.company").default_from(defaults::USER_COMPANY),
            m2o("journal_id", "account.journal").required(),
            m2o("payment_method_id", "account.payment.method"),
            Field::new("date", FieldType::Date).default_from(defaults::TODAY),
            Field::new("amount", AMOUNT).default_value(json!(0.0)),
            char("memo"),
            Field::new(
                "payment_type",
                FieldType::Selection(vec![
                    ("inbound".into(), "Receive".into()),
                    ("outbound".into(), "Send".into()),
                ]),
            )
            .default_value(json!("outbound")),
            Field::new(
                "state",
                FieldType::Selection(vec![
                    (STATE_DRAFT.into(), "Draft".into()),
                    (STATE_IN_PROCESS.into(), "In Process".into()),
                    ("paid".into(), "Paid".into()),
                    (STATE_CANCELED.into(), "Canceled".into()),
                    ("rejected".into(), "Rejected".into()),
                ]),
            )
            .default_value(json!(STATE_DRAFT)),
            // the bills this payment settles, for the stub beside the
            // check
            Field::new(
                "invoice_ids",
                FieldType::Many2many {
                    comodel: "account.move".into(),
                    relation: "account_payment_invoice_rel".into(),
                    column1: "payment_id".into(),
                    column2: "invoice_id".into(),
                },
            ),
            // --- what account_check_printing adds ---
            // "sent" means the check has been printed: it is what keeps
            // the same check from coming out of the printer twice
            Field::new("is_sent", FieldType::Boolean).default_value(json!(false)),
            // not computed here, unlike Odoo. Odoo's compute peeks at the
            // journal's sequence so a draft shows the number it *would*
            // get, and an inverse writes the padding back; this ORM has
            // no inverse, and a stored compute that draws from a sequence
            // would consume a number every time a dependency moved. The
            // number is written when the payment is posted, which is when
            // Odoo's `action_post` draws it for real.
            char("check_number"),
            Field::new("check_amount_in_words", FieldType::Char { size: None })
                .computed(&["amount"], check_amount_in_words)
                .store(),
            Field::new("check_manual_sequencing", FieldType::Boolean)
                .related("journal_id.check_manual_sequencing"),
            Field::new("payment_method_code", FieldType::Char { size: None })
                .related("payment_method_id.code"),
            Field::new("show_check_number", FieldType::Boolean).computed(
                &["payment_method_id.code", "check_number"],
                show_check_number,
            ),
        ],
    )
    .constrained("check number is digits", &["check_number"], digits_only)
    // Odoo's `_order` for payments is newest first, and a list of checks
    // to print is read from the top
    .ordered("date desc, id desc")
}

/// `res.company` — the check layout and the printer's margins.
///
/// This one is a real `_inherit`, exactly as in Odoo: the fields belong
/// to the company, and every country-specific check module widens
/// `account_check_printing_layout` with its own report.
pub fn company() -> Model {
    Model::new(
        extending("res.company", "res_company"),
        vec![
            Field::new(
                "account_check_printing_layout",
                FieldType::Selection(vec![(LAYOUT_DISABLED.into(), "None".into())]),
            )
            .default_value(json!(LAYOUT_DISABLED)),
            // CPA asks for the date label on the check; a pre-printed pad
            // that already has one does not want a second
            Field::new("account_check_printing_date_label", FieldType::Boolean)
                .default_value(json!(true)),
            Field::new("account_check_printing_multi_stub", FieldType::Boolean)
                .default_value(json!(false)),
            // inches, and a quarter of one is where Odoo starts: printers
            // disagree about where the top of the page is
            Field::new("account_check_printing_margin_top", AMOUNT).default_value(json!(0.25)),
            Field::new("account_check_printing_margin_left", AMOUNT).default_value(json!(0.25)),
            Field::new("account_check_printing_margin_right", AMOUNT).default_value(json!(0.25)),
        ],
    )
}

/// `print.prenumbered.checks` — "which number is on the first sheet in
/// the printer?".
///
/// A dialog and nothing else: pre-printed checks already carry their
/// numbers, so the system is being told what they are rather than
/// deciding them.
pub fn prenumbered_wizard() -> Model {
    Model::new(
        meta("print.prenumbered.checks", "print_prenumbered_checks"),
        vec![
            char("next_check_number").required(),
            // Odoo carries the payments in the context (`payment_ids`);
            // they are a field here for the same reason the debit note's
            // wizard points at its invoices — a dialog that cannot say
            // what it is about cannot be reopened, and the context does
            // not survive a round trip through this ORM.
            Field::new(
                "payment_ids",
                FieldType::Many2many {
                    comodel: "account.payment".into(),
                    relation: "print_prenumbered_checks_payment_rel".into(),
                    column1: "wizard_id".into(),
                    column2: "payment_id".into(),
                },
            ),
        ],
    )
    .constrained(
        "next check number is digits",
        &["next_check_number"],
        wizard_digits_only,
    )
    .transient()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_next_number_is_shown_padded_without_being_drawn() {
        let mut record = Map::new();
        record.insert("check_sequence_id.number_next".into(), json!([42]));
        record.insert("check_sequence_id.padding".into(), json!([5]));
        assert_eq!(check_next_number(&record), json!("00042"));
    }

    #[test]
    fn a_journal_without_a_sequence_says_one() {
        // the read of a null many2one comes back as an empty list
        let mut record = Map::new();
        record.insert("check_sequence_id.number_next".into(), json!([]));
        record.insert("check_sequence_id.padding".into(), json!([]));
        assert_eq!(check_next_number(&record), json!("1"));
    }

    #[test]
    fn the_amount_in_words_follows_the_amount() {
        let mut record = Map::new();
        record.insert("amount".into(), json!(1234.5));
        assert_eq!(
            check_amount_in_words(&record),
            json!("One Thousand Two Hundred And Thirty-Four and 50/100")
        );
    }

    #[test]
    fn the_number_is_shown_only_for_a_numbered_check() {
        let mut record = Map::new();
        record.insert("payment_method_id.code".into(), json!(["check_printing"]));
        record.insert("check_number".into(), json!("00042"));
        assert_eq!(show_check_number(&record), json!(true));

        // a transfer has no check number to show
        record.insert("payment_method_id.code".into(), json!(["manual"]));
        assert_eq!(show_check_number(&record), json!(false));

        // and neither has a check that has not been posted yet
        record.insert("payment_method_id.code".into(), json!(["check_printing"]));
        record.insert("check_number".into(), Value::Null);
        assert_eq!(show_check_number(&record), json!(false));
    }

    #[test]
    fn a_check_number_with_letters_is_refused() {
        let mut record = Map::new();
        record.insert("check_number".into(), json!("F1234"));
        assert!(digits_only(&record).is_err());
        // and no number at all is fine: most payments are not checks
        assert!(digits_only(&Map::new()).is_ok());
    }
}
