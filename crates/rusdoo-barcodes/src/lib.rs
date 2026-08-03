//! rusdoo-barcodes — port de `odoo/addons/barcodes/models/`: o que
//! acontece entre o bipe do leitor e o sistema saber o que foi lido.
//!
//! Uma *nomenclatura* é um conjunto de *regras*; cada regra diz "um
//! código com esta cara é um produto" — e, quando a balança embutiu peso
//! ou preço no código, onde esse número está. Decodificar é passar as
//! regras na ordem até uma casar.
//!
//! Três coisas do módulo original não vieram, e é melhor dizer do que
//! fingir: o mixin de eventos de teclado (`barcode.events`), que é
//! JavaScript de cliente e não tem modelo nem tabela; a nomenclatura
//! escolhida por empresa, que o Odoo enxerta em `res.company` com
//! `_inherit` de outro módulo; e a trava que impede apagar a
//! nomenclatura padrão, que precisaria de um gancho de exclusão.

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

/// O que a nomenclatura sabe fazer: ler um código.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    // decodificar não muda nada; quem só lê o estoque também pode bipar
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
            // Guardada, não aplicada. No Odoo 19 a conversão nunca chega
            // a acontecer: a codificação é conferida contra o código
            // original, e um EAN-13 convertido de UPC-A começa em zero —
            // o que, por definição, não é um EAN-13. Manter o campo é
            // manter a configuração de quem migra; escrever o branch
            // seria escrever código que não roda.
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

/// O padrão de uma regra precisa ser aplicável antes de ser gravado.
fn pattern_is_usable(record: &Map<String, Value>) -> Result<(), String> {
    let pattern = record.get("pattern").and_then(Value::as_str).unwrap_or("");
    pattern::check_pattern(pattern)
}

/// `barcode.rule` — uma cara de código e o que ela significa.
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
            // menor primeiro: é o desempate entre duas regras que casam o
            // mesmo código
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

/// `parse_barcode` — o que este código lido é?
///
/// A resposta é um objeto (`type`, `code`, `base_code`, `value`) quando
/// o código passou pelas regras, e uma *lista* quando era uma URI de
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
        // os argumentos do método vêm depois do conjunto de registros
        let barcode = ctx
            .rest
            .first()
            .or_else(|| kwargs.get("barcode"))
            .and_then(Value::as_str)
            .ok_or_else(|| RusdooError::Validation("give the barcode that was scanned".into()))?;

        // uma URI de RFID não passa pelas regras: ela já diz o que carrega
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
/// A ordem é o que decide qual regra ganha quando duas casam o mesmo
/// código, então ela vem do `search` (`sequence`, e o id para desempatar)
/// e não da ordem em que o banco devolveu as linhas — `read` não promete
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
        // uma regra sem padrão não filtra nada
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
