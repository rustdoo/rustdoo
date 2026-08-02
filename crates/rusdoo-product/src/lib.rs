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
use serde_json::json;

/// Money: two decimals, Odoo's default price precision.
pub const PRICE: FieldType = FieldType::Float {
    digits: Some((16, 2)),
};

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(product())?;
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
            Field::new("name", FieldType::Char { size: None }).required(),
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
            Field::new("description", FieldType::Text),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
        ],
    )
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
