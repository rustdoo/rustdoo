//! rusdoo-sale — port of `odoo/addons/sale/models/` (and the slice of
//! `product` it needs): the first business module of the port.
//!
//! It is here to be a real one, not a demo: an order with lines, line
//! subtotals and an order total that the database keeps current — the
//! shape every other business module repeats.

use rusdoo_core::RusdooError;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};

/// Money and quantities: two decimals, like Odoo's default precision.
const PRICE: FieldType = FieldType::Float {
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

/// `price_subtotal` — what one line is worth.
fn price_subtotal(record: &Map<String, Value>) -> Value {
    let total = number(record, "product_uom_qty") * number(record, "price_unit");
    // rounded to the currency's precision, not left with the binary tail
    // of the multiplication
    json!((total * 100.0).round() / 100.0)
}

/// `amount_total` — the order is worth what its lines are worth.
fn amount_total(record: &Map<String, Value>) -> Value {
    let lines = record
        .get("order_line.price_subtotal")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let total: f64 = lines
        .iter()
        .filter_map(|value| match value {
            Value::Number(n) => n.as_f64(),
            Value::String(text) => text.parse().ok(),
            _ => None,
        })
        .sum();
    json!((total * 100.0).round() / 100.0)
}

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    for model in models() {
        reg.register(model)?;
    }
    Ok(())
}

fn models() -> Vec<Model> {
    vec![product(), order(), order_line()]
}

/// `product.product` — what is being sold.
fn product() -> Model {
    Model::new(
        meta("product.product", "product_product"),
        vec![
            char("name").required(),
            // the internal reference a warehouse actually says out loud
            char("default_code"),
            Field::new("list_price", PRICE).default_value(json!(0.0)),
            Field::new("standard_price", PRICE).default_value(json!(0.0)),
            Field::new(
                "type",
                FieldType::Selection(vec![
                    ("consu".into(), "Produto".into()),
                    ("service".into(), "Serviço".into()),
                ]),
            )
            .default_value(json!("consu")),
            Field::new("description", FieldType::Text),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
        ],
    )
}

/// `sale.order` — the order itself.
fn order() -> Model {
    Model::new(
        meta("sale.order", "sale_order"),
        vec![
            char("name").required().default_value(json!("Novo")),
            m2o("partner_id", "res.partner").required(),
            m2o("company_id", "res.company"),
            Field::new("date_order", FieldType::Datetime),
            Field::new(
                "state",
                FieldType::Selection(vec![
                    ("draft".into(), "Orçamento".into()),
                    ("sale".into(), "Pedido confirmado".into()),
                    ("cancel".into(), "Cancelado".into()),
                ]),
            )
            .default_value(json!("draft")),
            Field::new(
                "order_line",
                FieldType::One2many {
                    comodel: "sale.order.line".into(),
                    inverse: "order_id".into(),
                },
            ),
            // stored: an order list showing totals must not compute one
            // per row, and a total is asked for far more often than the
            // lines change
            Field::new("amount_total", PRICE)
                .computed(&["order_line.price_subtotal"], amount_total)
                .store(),
            Field::new("note", FieldType::Text),
        ],
    )
}

/// `sale.order.line` — one thing sold, at one price.
fn order_line() -> Model {
    Model::new(
        meta("sale.order.line", "sale_order_line"),
        vec![
            m2o("order_id", "sale.order").required(),
            m2o("product_id", "product.product"),
            char("name"),
            Field::new("product_uom_qty", PRICE).default_value(json!(1.0)),
            Field::new("price_unit", PRICE).default_value(json!(0.0)),
            Field::new("sequence", FieldType::Integer).default_value(json!(10)),
            Field::new("price_subtotal", PRICE)
                .computed(&["product_uom_qty", "price_unit"], price_subtotal)
                .store(),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_worth_quantity_times_price() {
        let mut record = Map::new();
        record.insert("product_uom_qty".into(), json!(3));
        record.insert("price_unit".into(), json!(19.9));
        assert_eq!(price_subtotal(&record), json!(59.7));
    }

    #[test]
    fn an_order_is_worth_the_sum_of_its_lines() {
        let mut record = Map::new();
        record.insert("order_line.price_subtotal".into(), json!([59.7, 10.3]));
        assert_eq!(amount_total(&record), json!(70.0));
        // and an order with no lines is worth zero, not null
        assert_eq!(amount_total(&Map::new()), json!(0.0));
    }

    #[test]
    fn the_models_register_on_top_of_base() {
        let mut reg = rusdoo_base::registry().unwrap();
        extend(&mut reg).unwrap();
        for name in ["product.product", "sale.order", "sale.order.line"] {
            assert!(reg.get(name).is_some(), "{name} must be registered");
        }
        // the total is materialized: a list of orders reads a column
        let total = reg.get("sale.order").unwrap().field("amount_total").unwrap();
        assert!(total.stored);
    }
}
