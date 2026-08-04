//! What lunch adds to the models it did not write.
//!
//! Port of `res_company.py` and `res_users.py`. Both are `_inherit`
//! extensions in place: the model keeps its table and everything the
//! other modules put on it, and gains the handful of columns this addon
//! needs.

use crate::m2o;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::model::{Model, ModelMeta};
use serde_json::json;

/// The same model, extended rather than replaced.
fn extending(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![name.to_string()],
        inherits: vec![],
    }
}

/// `res.company` — how far a wallet may go under, and what people are
/// told when their food arrives.
pub fn company() -> Model {
    Model::new(
        extending("res.company", "res_company"),
        vec![
            // an allowance, not a balance: the company lets somebody
            // order while they owe up to this much
            Field::new(
                "lunch_minimum_threshold",
                FieldType::Float { digits: Some((16, 2)) },
            )
            .default_value(json!(0.0)),
            Field::new("lunch_notify_message", FieldType::Html)
                .translatable()
                .default_value(json!("Your lunch has been delivered.\nEnjoy your meal!")),
        ],
    )
}

/// `res.users` — where somebody eats, and what they always order.
pub fn users() -> Model {
    Model::new(
        extending("res.users", "res_users"),
        vec![
            m2o("last_lunch_location_id", "lunch.location"),
            // the other side of `lunch.product.favorite_user_ids`: one
            // relation table read from both ends, like Odoo's
            Field::new(
                "favorite_lunch_product_ids",
                FieldType::Many2many {
                    comodel: "lunch.product".into(),
                    relation: "lunch_product_favorite_user_rel".into(),
                    column1: "user_id".into(),
                    column2: "product_id".into(),
                },
            ),
        ],
    )
}
