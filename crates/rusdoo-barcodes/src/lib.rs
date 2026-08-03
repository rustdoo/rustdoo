//! rusdoo-barcodes — port de `odoo/addons/barcodes/models/`: o que
//! acontece entre o bipe do leitor e o sistema saber o que foi lido.
//!
//! A *nomenclature* is a set of *rules*; each rule says "a code that
//! looks like this is a product" — and, when the scale embedded weight
//! or price in the code, where that number is. Decoding is walking the
//! rules in order until one matches.
//!
//! Three things from the original module did not come across, and it is
//! better to say so than to pretend: the keyboard-event mixin
//! (`barcode.events`), which is client JavaScript with no model and no
//! table; the nomenclature
//! escolhida por empresa, que o Odoo enxerta em `res.company` com
//! `_inherit` from another module; and the lock that stops the default
//! nomenclature from being deleted, which would need an unlink hook.

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

pub mod decode;
pub mod gtin;
pub mod pattern;

use decode::Rule;

fn char(name: &str) -> Field {
    Field::new(name, FieldType::Char { size: None })
}

fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![],
        inherits: vec![],
    }
}

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(nomenclature())?;
    reg.register(rule())?;
    Ok(())
}

/// What a nomenclature knows how to do: read a code.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    // decoding changes nothing; whoever only reads stock may scan too
    methods.register(
        "barcode.nomenclature",
        "parse_barcode",
        Operation::Read,
        parse_barcode,
    )?;
    Ok(())
}

/// `barcode.nomenclature` — o conjunto de regras de uma empresa.
fn nomenclature() -> Model {
    Model::new(
        meta("barcode.nomenclature", "barcode_nomenclature"),
        vec![
            char("name").required(),
            Field::new(
                "rule_ids",
                FieldType::One2many {
                    comodel: "barcode.rule".into(),
                    inverse: "barcode_nomenclature_id".into(),
                },
            ),
            // Stored, not applied. In Odoo 19 the conversion never
            // actually happens: the encoding is checked against the
            // original code, and an EAN-13 converted from a UPC-A starts
            // with zero — which, by definition, is not an EAN-13.
            // Keeping the field keeps the setting of whoever migrates;
            // writing the branch would be writing code that never runs.
            Field::new(
                "upc_ean_conv",
                FieldType::Selection(vec![
                    ("none".into(), "Never".into()),
                    ("ean2upc".into(), "EAN-13 para UPC-A".into()),
                    ("upc2ean".into(), "UPC-A para EAN-13".into()),
                    ("always".into(), "Always".into()),
                ]),
            )
            .required()
            .default_value(json!("always")),
        ],
    )
}

/// A rule's pattern has to be usable before it is stored.
fn pattern_is_usable(record: &Map<String, Value>) -> Result<(), String> {
    let pattern = record.get("pattern").and_then(Value::as_str).unwrap_or("");
    pattern::check_pattern(pattern)
}

/// `barcode.rule` — a shape of code and what it means.
fn rule() -> Model {
    Model::new(
        meta("barcode.rule", "barcode_rule"),
        vec![
            char("name").required(),
            Field::new(
                "barcode_nomenclature_id",
                FieldType::Many2one {
                    comodel: "barcode.nomenclature".into(),
                },
            ),
            // lowest first: it is the tie-break between two rules that
            // match the same code
            Field::new("sequence", FieldType::Integer).default_value(json!(10)),
            Field::new(
                "encoding",
                FieldType::Selection(vec![
                    ("any".into(), "Any".into()),
                    ("ean13".into(), "EAN-13".into()),
                    ("ean8".into(), "EAN-8".into()),
                    ("upca".into(), "UPC-A".into()),
                ]),
            )
            .required()
            .default_value(json!("any")),
            Field::new(
                "type",
                FieldType::Selection(vec![
                    ("alias".into(), "Alias".into()),
                    ("product".into(), "Product".into()),
                ]),
            )
            .required()
            .default_value(json!("product")),
            char("pattern").required().default_value(json!(".*")),
            char("alias").required().default_value(json!("0")),
        ],
    )
    .constrained("usable pattern", &["pattern"], pattern_is_usable)
}

/// `parse_barcode` — what is this scanned code?
///
/// The answer is an object (`type`, `code`, `base_code`, `value`) when
/// the code went through the rules, and a *list* when it was a URI
/// from
/// RFID: uma URI carrega o produto e o lote de uma vez.
fn parse_barcode<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [nomenclature] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "scan with one nomenclature at a time".into(),
            ));
        };
        // the method's arguments come after the recordset
        let barcode = ctx
            .rest
            .first()
            .or_else(|| kwargs.get("barcode"))
            .and_then(Value::as_str)
            .ok_or_else(|| RusdooError::Validation("give the barcode that was scanned".into()))?;

        // an RFID URI does not go through the rules: it already says
        // what it carries
        if let Some(parts) = decode::parse_uri(barcode) {
            return Ok(Value::Array(
                parts.iter().map(decode::Parsed::to_json).collect(),
            ));
        }
        let rules = rules_of(&ctx, nomenclature).await?;
        Ok(decode::parse_nomenclature(&rules, barcode).to_json())
    })
}

/// As regras da nomenclatura, na ordem em que valem.
///
/// The order is what decides which rule wins when two match the same
/// code, so it comes from the `search` (`sequence`, and the id to break
/// ties) and not from the order the database handed the rows back —
/// `read` promises
/// ordem nenhuma.
async fn rules_of(ctx: &MethodCtx<'_>, nomenclature: i64) -> Result<Vec<Rule>, RusdooError> {
    let ids = ctx
        .registry
        .search(
            ctx.pool,
            "barcode.rule",
            &parse_domain(&json!([["barcode_nomenclature_id", "=", nomenclature]]))?,
            &SearchOptions {
                order: Some("sequence asc, id asc".into()),
                ..SearchOptions::default()
            },
        )
        .await?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = ctx
        .registry
        .read(
            ctx.pool,
            "barcode.rule",
            &ids,
            &["encoding", "type", "pattern", "alias"],
        )
        .await?;
    let by_id: HashMap<i64, &Map<String, Value>> = rows
        .iter()
        .filter_map(|row| Some((row.get("id")?.as_i64()?, row)))
        .collect();
    Ok(ids
        .iter()
        .filter_map(|id| by_id.get(id))
        .map(|row| decode::rule_from(row))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_models_register_on_top_of_base() {
        let mut reg = rusdoo_base::registry().unwrap();
        extend(&mut reg).unwrap();
        for name in ["barcode.nomenclature", "barcode.rule"] {
            assert!(reg.get(name).is_some(), "{name} must be registered");
        }
        // a rule with no pattern filters nothing
        let pattern = reg.get("barcode.rule").unwrap().field("pattern").unwrap();
        assert!(pattern.required);
        assert_eq!(pattern.default, Some(json!(".*")));
    }

    #[test]
    fn the_nomenclature_knows_how_to_read_a_code() {
        let mut methods = MethodRegistry::new();
        extend_methods(&mut methods).unwrap();
        assert_eq!(
            methods.names_for("barcode.nomenclature"),
            vec!["parse_barcode"]
        );
    }

    #[test]
    fn the_constraint_reads_the_pattern_off_the_record() {
        let mut record = Map::new();
        record.insert("pattern".into(), json!("....{NN}{DD}"));
        assert!(pattern_is_usable(&record).is_err());
        record.insert("pattern".into(), json!("22......{NNDD}."));
        assert!(pattern_is_usable(&record).is_ok());
    }
}
