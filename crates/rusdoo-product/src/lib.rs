//! rusdoo-product — port of `odoo/addons/product/models/`: what is
//! bought, sold and invoiced.
//!
//! Its own module because more than one thing needs it: a sales order
//! and an invoice both point at a product, and neither should have to
//! depend on the other to do it.
//!
//! A product is two records, as it is in Odoo. The **template**
//! (`product.template`) is what the catalogue describes — the name, the
//! sales price, the picture. The **variant** (`product.product`) is what
//! a warehouse counts and a document points at, and it delegates to its
//! template through `_inherits = {'product.template': 'product_tmpl_id'}`.
//! Two shirts of different sizes are two variants of one template: they
//! are renamed once and repriced once, and each keeps its own cost and
//! its own internal reference.
//!
//! Every caller may keep writing to `product.product` alone — creating
//! one creates its template, and writing the name through it writes the
//! template's row. That is the whole point of the delegation, and it is
//! why the port can add the template without changing a single caller.

use rusdoo_core::RusdooError;
use rusdoo_orm::fields::{DefaultCtx, Field, FieldType, OnDelete};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use std::future::Future;
use std::pin::Pin;

/// Money: two decimals, Odoo's default price precision.
pub const PRICE: FieldType = FieldType::Float {
    digits: Some((16, 2)),
};

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    // order matters: the variant delegates to the template, and a
    // delegation parent has to be registered before its child
    reg.register(category())?;
    reg.register(template())?;
    reg.register(product())?;
    Ok(())
}

fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![],
        inherits: vec![],
    }
}

/// A price below zero is not a discount, it is a typo. The check reads
/// one field because each record owns one price: the sales price is the
/// template's, the cost is the variant's.
fn price_is_not_negative(
    record: &Map<String, Value>,
    field: &str,
    label: &str,
) -> Result<(), String> {
    let value = record
        .get(field)
        .and_then(Value::as_f64)
        .or_else(|| record.get(field).and_then(Value::as_i64).map(|n| n as f64))
        .unwrap_or(0.0);
    if value < 0.0 {
        return Err(format!("the {label} cannot be negative"));
    }
    Ok(())
}

fn sales_price_is_not_negative(record: &Map<String, Value>) -> Result<(), String> {
    price_is_not_negative(record, "list_price", "sales price")
}

fn cost_is_not_negative(record: &Map<String, Value>) -> Result<(), String> {
    price_is_not_negative(record, "standard_price", "cost")
}

/// The unit a product is measured in when nobody said otherwise, as an
/// external id: Odoo's `default=lambda self: self.env.ref('uom.product_uom_unit')`.
///
/// Resolved from `ir_model_data` on the create's own connection rather
/// than held as an id, because the id is whatever the database that
/// installed `uom` handed out. A database without `uom` installed gives
/// no default instead of an error — the field is optional, and a
/// catalogue of services has nothing to weigh.
const REFERENCE_UNIT: (&str, &str) = ("uom", "product_uom_unit");

type Answer<'a> = Pin<Box<dyn Future<Output = Result<Value, RusdooError>> + Send + 'a>>;

fn default_unit(ctx: DefaultCtx<'_>) -> Answer<'_> {
    Box::pin(async move {
        let failed = |error: sqlx::Error| {
            RusdooError::Database(format!("default de unidade em {}: {error}", ctx.model))
        };
        // Asked first, and not for elegance: a query against a table that
        // does not exist aborts the transaction it runs in, and this one
        // is the create's. Every statement after it would then fail with
        // "current transaction is aborted" and the real cause would be
        // named nowhere. A database that never installed a module — a
        // test that builds its own tables — has no `ir_model_data` at
        // all, and that is not this create's problem.
        let registered: bool =
            sqlx::query_scalar("SELECT to_regclass('ir_model_data') IS NOT NULL")
                .fetch_one(&mut *ctx.conn)
                .await
                .map_err(failed)?;
        if !registered {
            return Ok(Value::Null);
        }
        let (module, name) = REFERENCE_UNIT;
        let found: Option<i32> = sqlx::query_scalar(
            r#"SELECT "res_id" FROM "ir_model_data"
               WHERE "module" = $1 AND "name" = $2 AND "model" = 'uom.uom'"#,
        )
        .bind(module)
        .bind(name)
        .fetch_optional(&mut *ctx.conn)
        .await
        .map_err(failed)?;
        Ok(match found {
            Some(id) => Value::from(i64::from(id)),
            None => Value::Null,
        })
    })
}

/// `product.category` — how the catalogue is filed.
///
/// Odoo keeps a `complete_name` ("Furniture / Chairs") and a
/// `parent_path` for its hierarchy queries; neither is needed to file a
/// product, and `child_of` already walks the tree through `parent_id`.
fn category() -> Model {
    Model::new(
        meta("product.category", "product_category"),
        vec![
            Field::new("name", FieldType::Char { size: None })
                .required()
                .translatable(),
            // a category whose parent goes takes its children with it,
            // like Odoo's `ondelete='cascade'`
            Field::new(
                "parent_id",
                FieldType::Many2one {
                    comodel: "product.category".into(),
                },
            )
            .ondelete(OnDelete::Cascade),
            Field::new(
                "child_id",
                FieldType::One2many {
                    comodel: "product.category".into(),
                    inverse: "parent_id".into(),
                },
            ),
        ],
    )
    .ordered("name, id")
}

/// `product.template` — what the catalogue describes.
fn template() -> Model {
    Model::new(
        meta("product.template", "product_template"),
        vec![
            // `translate=True` in Odoo: the catalogue is what the
            // customer reads
            Field::new("name", FieldType::Char { size: None })
                .required()
                .translatable(),
            Field::new("list_price", PRICE).default_value(json!(0.0)),
            Field::new(
                "type",
                FieldType::Selection(vec![
                    ("consu".into(), "Product".into()),
                    ("service".into(), "Service".into()),
                ]),
            )
            .default_value(json!("consu")),
            Field::new("description", FieldType::Text).translatable(),
            Field::new(
                "categ_id",
                FieldType::Many2one {
                    comodel: "product.category".into(),
                },
            ),
            // what a quantity of this product means: two of it is two
            // units, or two dozen, or two kilos
            Field::new(
                "uom_id",
                FieldType::Many2one {
                    comodel: "uom.uom".into(),
                },
            )
            .default_from(default_unit),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            // Odoo's `image.mixin` gives every product an image; here
            // only the original, without the sizes it materialises
            Field::new("image_1920", FieldType::Binary),
            Field::new(
                "product_variant_ids",
                FieldType::One2many {
                    comodel: "product.product".into(),
                    inverse: "product_tmpl_id".into(),
                },
            ),
        ],
    )
    .constrained(
        "non-negative sales price",
        &["list_price"],
        sales_price_is_not_negative,
    )
    // Odoo orders by `is_favorite desc, name`; without the favourites,
    // by name
    .ordered("name, id")
}

/// `product.product` — the variant a document points at.
fn product() -> Model {
    Model::new(
        ModelMeta {
            name: "product.product".into(),
            table: "product_product".into(),
            inherit: vec![],
            inherits: vec![("product.template".into(), "product_tmpl_id".into())],
        },
        vec![
            // the variant has no life without its template: Odoo's
            // `ondelete='cascade'` on the same field
            Field::new(
                "product_tmpl_id",
                FieldType::Many2one {
                    comodel: "product.template".into(),
                },
            )
            .required()
            .ondelete(OnDelete::Cascade),
            // the internal reference a warehouse actually says out loud,
            // and which two variants of one template do not share
            Field::new("default_code", FieldType::Char { size: None }),
            // what it cost *this* variant: a large shirt and a small one
            // are bought at different prices and sold at one
            Field::new("standard_price", PRICE).default_value(json!(0.0)),
            // declared here as well as on the template, like Odoo does:
            // a variant can be archived while its template stays on sale
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
        ],
    )
    .constrained(
        "non-negative cost",
        &["standard_price"],
        cost_is_not_negative,
    )
    // Odoo's own `_order`: the reference first, then the name the
    // template holds — a delegated column the search reaches through the
    // link
    .ordered("default_code, name, id")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_product_registers_on_top_of_base() {
        let mut reg = rusdoo_base::registry().unwrap();
        extend(&mut reg).unwrap();
        let product = reg.get("product.product").expect("registered");
        assert!(product.field("default_code").is_some());
        assert!(product.field("product_tmpl_id").is_some_and(|f| f.required));
    }

    #[test]
    fn the_catalogue_fields_live_on_the_template() {
        let mut reg = rusdoo_base::registry().unwrap();
        extend(&mut reg).unwrap();
        let template = reg.get("product.template").expect("registered");
        assert!(template.field("list_price").is_some());
        assert!(template.field("name").is_some_and(|f| f.required));
        // and the variant reaches them by delegating, not by owning them
        let product = reg.get("product.product").expect("registered");
        assert!(product.field("name").is_none());
        assert_eq!(
            product.meta.inherits,
            vec![("product.template".to_string(), "product_tmpl_id".to_string())]
        );
    }
}
