//! rusdoo-product — port of `odoo/addons/product/models/`: what is
//! bought, sold and invoiced.
//!
//! Its own module because more than one thing needs it: a sales order
//! and an invoice both point at a product, and neither should have to
//! depend on the other to do it.

use rusdoo_core::RusdooError;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};

/// Money: two decimals, Odoo's default price precision.
pub const PRICE: FieldType = FieldType::Float {
    digits: Some((16, 2)),
};

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(product())?;
    Ok(())
}

/// A price below zero is not a discount, it is a typo.
fn prices_are_not_negative(record: &Map<String, Value>) -> Result<(), String> {
    for (field, label) in [("list_price", "preço de venda"), ("standard_price", "custo")] {
        let value = record
            .get(field)
            .and_then(Value::as_f64)
            .or_else(|| record.get(field).and_then(Value::as_i64).map(|n| n as f64))
            .unwrap_or(0.0);
        if value < 0.0 {
            return Err(format!("o {label} não pode ser negativo"));
        }
    }
    Ok(())
}

/// `product.product` — the thing itself.
fn product() -> Model {
    Model::new(
        ModelMeta {
            name: "product.product".into(),
            table: "product_product".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            // `translate=True` no Odoo: o catálogo é o que o cliente lê
            Field::new("name", FieldType::Char { size: None })
                .required()
                .translatable(),
            // the internal reference a warehouse actually says out loud
            Field::new("default_code", FieldType::Char { size: None }),
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
            Field::new("description", FieldType::Text).translatable(),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            // `image.mixin` do Odoo dá uma imagem a todo produto; aqui só
            // a original, sem os tamanhos derivados que ele materializa
            Field::new("image_1920", FieldType::Binary),
        ],
    )
    .constrained(
        "preços não negativos",
        &["list_price", "standard_price"],
        prices_are_not_negative,
    )
    // o Odoo ordena por `is_favorite desc, name`; sem os favoritos,
    // por nome
    .ordered("name, id")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_product_registers_on_top_of_base() {
        let mut reg = rusdoo_base::registry().unwrap();
        extend(&mut reg).unwrap();
        let product = reg.get("product.product").expect("registered");
        assert!(product.field("list_price").is_some());
        assert!(product.field("name").is_some_and(|f| f.required));
    }
}
