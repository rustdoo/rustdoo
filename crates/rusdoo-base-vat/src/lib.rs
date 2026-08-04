//! rusdoo-base-vat — port of `odoo/addons/base_vat/`: a tax id that is
//! refused when it cannot be one.
//!
//! A VAT number carries its own check digit, so a typo in it is a typo
//! anyone can catch — before it reaches an invoice, a tax return and an
//! auditor. That is the whole module: `res.partner.vat` is validated
//! against the partner's country, and a number that fails its country's
//! arithmetic is refused at the write.
//!
//! Odoo reaches for the `stdnum` library and falls back to its own
//! `check_vat_xx` methods; [`vat::eu`] is the port of the twenty-seven
//! member states plus the United Kingdom, each function named after the
//! `stdnum` module it came from.
//!
//! **What is deliberately not here:** the VIES check. Odoo can ask the
//! European Commission's web service whether a number is *registered*,
//! which is a different question from whether it is *well formed* — and
//! answering it means a network call inside a write. A saved record that
//! depends on a remote service being up is not a saved record. The
//! arithmetic never needs the network and never gives a different answer
//! twice.

use rusdoo_core::RusdooError;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{Map, Value};

pub mod vat;

/// Odoo's convention for "this partner has no VAT number, and that is
/// deliberate" (`base_vat`'s own message says to use it).
pub const NO_VAT: &str = "/";

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(checked_partner())?;
    Ok(())
}

/// Split a number into its country prefix and the rest, port of
/// `_split_vat`: `"BE0477472701"` is Belgian, `"0477472701"` says
/// nothing about where it is from.
pub fn split(vat: &str) -> (String, String) {
    let prefix: String = vat.chars().take(2).collect::<String>().to_uppercase();
    if prefix.chars().count() < 2 || !prefix.chars().all(|c| c.is_alphabetic()) {
        return (String::new(), vat.to_string());
    }
    let rest: String = vat.chars().skip(2).filter(|c| *c != ' ').collect();
    (prefix, rest)
}

/// The number as the database should keep it, port of
/// `_format_vat_number`: the separators a human types are not part of
/// the number, and some countries write theirs back with their own.
pub fn compact(country_code: &str, vat: &str) -> String {
    match vat::compact_for(country_code) {
        Some(compact) => compact(vat),
        None => vat.to_string(),
    }
}

/// Is `vat` a number `country_code` could have issued?
///
/// A country this build has no arithmetic for answers **yes**, like
/// Odoo's `_check_vat_number` does when neither `stdnum` nor `base_vat`
/// knows it: refusing what we cannot check would make the module a wall
/// in front of every country it does not cover yet.
pub fn is_valid(country_code: &str, vat: &str) -> bool {
    match vat::check_for(country_code) {
        Some(check) => check(vat),
        None => true,
    }
}

/// The sentence a refused number gets, port of
/// `_build_vat_error_message` minus the per-country `vat_label` (which
/// lives on `res.country` in Odoo and not yet here).
fn refusal(country_code: &str, vat: &str) -> String {
    let expected = vat::example_for(country_code)
        .map(|example| format!(" The expected format is {example}."))
        .unwrap_or_default();
    format!("the tax id [{vat}] does not look like a {country_code} number.{expected}")
}

/// The check both `res.partner` and `res.company` run.
///
/// The order is Odoo's, and each step of it is a decision the test file
/// of `base_vat` states outright
/// (`tests/test_vat_numbers.py::test_vat_syntactic_validation`):
///
/// 1. no number, or the `/` that means "none on purpose", is nothing to
///    check;
/// 2. **a record with no country is never checked** — not even when the
///    number starts with a country code. A database that has not filled
///    in addresses is not a database that should stop saving contacts;
/// 3. a number that names a country this build can check is checked as
///    that country, whatever the record's own country says: a Belgian
///    number on a French customer is a Belgian number;
/// 4. otherwise it is checked as the record's country — and a country
///    with no arithmetic here accepts everything.
fn vat_is_a_number_its_country_could_have_issued(
    record: &Map<String, Value>,
) -> Result<(), String> {
    let Some(number) = record.get("vat").and_then(Value::as_str) else {
        return Ok(());
    };
    let number = number.trim();
    if number.is_empty() || number == NO_VAT {
        return Ok(());
    }
    let country = record
        .get("country_code")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if country.is_empty() {
        return Ok(());
    }
    // the prefix wins, but only when it names a country whose arithmetic
    // this build has: `EU528003646` names none, and Odoo then falls back
    // to the record's country exactly like this
    let (prefix, _) = split(number);
    let checked_as = if vat::check_for(&prefix).is_some() {
        prefix
    } else {
        country.to_string()
    };
    if is_valid(&checked_as, number) {
        return Ok(());
    }
    Err(refusal(&checked_as, number))
}

fn extension(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![name.to_string()],
        inherits: vec![],
    }
}

/// The country's ISO code, reached through the link — Odoo declares the
/// same field on `res.partner` and reads the check from it.
fn country_code() -> Field {
    Field::new("country_code", FieldType::Char { size: None }).related("country_id.code")
}

/// `res.partner` extended (`_inherit`): the field was always there, the
/// arithmetic is what this module adds.
fn checked_partner() -> Model {
    Model::new(extension("res.partner", "res_partner"), vec![country_code()]).constrained_with(
        "a tax id is a number its country could have issued",
        // what makes the check run again
        vec!["vat".into(), "country_id".into()],
        // what it reads to answer
        vec!["vat".into(), "country_code".into()],
        rusdoo_orm::model::ConstraintFn::Native(vat_is_a_number_its_country_could_have_issued),
    )
}

// `res.company` is **not** checked here, and that is a gap named rather
// than a check that lies. In Odoo a company *is* a partner
// (`_inherits`), so `company.vat` is the partner's field and the
// partner's country decides it. In this port the two models are separate
// and `res.company` carries no country at all — a constraint over it
// would find nothing to check and pass every number, which reads like
// validation and is not. When the company delegates to a partner, as it
// does in Odoo, it inherits this check with nothing further to write.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(pairs: Value) -> Map<String, Value> {
        pairs.as_object().expect("an object").clone()
    }

    fn record_for(vat: &str, country: &str) -> Map<String, Value> {
        record(json!({"vat": vat, "country_code": country}))
    }

    #[test]
    fn a_number_says_where_it_is_from_or_says_nothing() {
        assert_eq!(split("BE0477472701"), ("BE".into(), "0477472701".into()));
        assert_eq!(split("0477472701"), (String::new(), "0477472701".into()));
        // lowercase is a prefix too, and the spaces of a human are not
        assert_eq!(split("be 0477 472 701"), ("BE".into(), "0477472701".into()));
    }

    #[test]
    fn a_country_without_arithmetic_is_not_a_country_that_refuses() {
        assert!(is_valid("BR", "qualquer coisa"));
        assert!(is_valid("BE", "BE0477472701"));
        assert!(!is_valid("BE", "BE0477472702"));
    }

    #[test]
    fn the_check_runs_over_the_country_of_the_record() {
        let refused = vat_is_a_number_its_country_could_have_issued(&record(
            json!({"vat": "0477472702", "country_code": "BE"}),
        ));
        assert!(refused.is_err(), "{refused:?}");
        assert!(vat_is_a_number_its_country_could_have_issued(&record(
            json!({"vat": "0477472701", "country_code": "BE"})
        ))
        .is_ok());
    }

    /// The four cases `base_vat`'s own test file spells out
    /// (`test_vat_syntactic_validation`), answered the same way.
    #[test]
    fn the_prefix_wins_over_the_country_but_only_when_there_is_a_country() {
        let check = |vat: &str, country: &str| {
            vat_is_a_number_its_country_could_have_issued(&record_for(vat, country))
        };
        // a Belgian number on a French customer is a Belgian number
        assert!(check("BE0477472701", "FR").is_ok());
        // and a French number wearing a Belgian prefix is refused
        assert!(check("BE23334175221", "FR").is_err());
        // no prefix: the customer's country decides
        assert!(check("0477472701", "BE").is_ok());
        assert!(check("42", "BE").is_err());
        // no country on the record: nothing is checked at all, prefix or
        // not — this is the case that surprises, and Odoo states it
        assert!(check("BE42", "").is_ok());
        assert!(check("BE0477472702", "").is_ok());
        // a prefix that names no country we can check falls back to the
        // record's: `EU...` on a Canadian customer is accepted (nothing
        // to check), on a Belgian one it is read as Belgian and refused
        assert!(check("EU528003646", "CA").is_ok());
        assert!(check("EU528003646", "BE").is_err());
    }

    #[test]
    fn a_partner_may_say_it_has_none() {
        assert!(vat_is_a_number_its_country_could_have_issued(&record_for(NO_VAT, "BE")).is_ok());
        assert!(vat_is_a_number_its_country_could_have_issued(&record(json!({"country_code": "BE"})))
            .is_ok());
        assert!(vat_is_a_number_its_country_could_have_issued(&record_for("   ", "BE")).is_ok());
    }

    #[test]
    fn the_refusal_says_what_was_expected() {
        let message = refusal("BE", "BE0477472702");
        assert!(message.contains("BE0477472702"), "{message}");
        assert!(message.contains("expected format"), "{message}");
    }
}
