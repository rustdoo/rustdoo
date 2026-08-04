//! rusdoo-sale-purchase — port of `odoo/addons/sale_purchase/`: a sale
//! that raises a purchase.
//!
//! A product marked "Subcontract Service" is something the company sells
//! and somebody else performs. Selling it has to buy it: confirming the
//! sales order raises a request for quotation to the vendor who provides
//! the service, and the two documents stay tied together by the link
//! between a sale line and the purchase lines it produced. That link is
//! the whole module — it is what turns a change on one side into a
//! warning on the other instead of a silent divergence nobody notices
//! until the vendor invoices for work that was cancelled.
//!
//! Two shapes of the port differ from Odoo, and they explain most of the
//! rest:
//!
//! * **Nothing hooks a write.** Odoo generates the purchase from inside
//!   `sale.order._action_confirm` and reacts to a quantity change from
//!   inside `sale.order.line.write`. This framework has no way to extend
//!   a method another module registered, and no create/write hook, so
//!   each of those steps is a method of its own here
//!   (`action_generate_purchase_orders`, `action_update_service_qty`,
//!   and the two cancellation notices). Each one checks the state it
//!   depends on, so calling it out of turn refuses instead of lying.
//! * **There is no `mail.activity`.** Odoo schedules a to-do on the
//!   other document's salesperson; the port posts a message on that
//!   document's thread, with the sentence Odoo's template writes.
//!
//! `product.supplierinfo` is registered here as well. It belongs to
//! `product`, which does not have it yet, and without it there is no
//! answer to "from whom, at what price, in how many days" — the three
//! things the generated purchase is made of. It is deliberately the
//! smallest version of Odoo's model that this module reads.

mod actions;
mod cancel;
mod generation;
mod notices;
mod shared;

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::fields::{Field, FieldType, OnDelete};
use rusdoo_orm::methods::MethodRegistry;
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use rusdoo_product::PRICE;
use serde_json::{json, Map, Value};

use shared::{deduplicated, first_id};

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

/// A model of this module's own.
fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![],
        inherits: vec![],
    }
}

/// `_inherit = '<name>'`: fields added to a model another module owns,
/// in its own table.
fn extension(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![name.to_string()],
        inherits: vec![],
    }
}

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(supplier_info())?;
    reg.register(purchasable_product())?;
    // the purchase side first: the sale line's one2many is declared over
    // the column this adds
    reg.register(purchase_order_line())?;
    reg.register(purchase_order())?;
    reg.register(sale_order_line())?;
    reg.register(sale_order())?;
    Ok(())
}

/// The buttons this bridge puts on the two documents.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    methods.register(
        "sale.order",
        "action_generate_purchase_orders",
        Operation::Write,
        generation::action_generate_purchase_orders,
    )?;
    methods.register(
        "sale.order",
        "action_view_purchase_orders",
        Operation::Read,
        actions::action_view_purchase_orders,
    )?;
    // warning the other document is a write on it: the notice lands on
    // its thread, and the salesperson who cancelled may not own it
    methods.register(
        "sale.order",
        "action_notify_purchase_of_cancellation",
        Operation::Write,
        cancel::action_notify_purchase_of_cancellation,
    )?;
    methods.register(
        "sale.order.line",
        "action_update_service_qty",
        Operation::Write,
        generation::action_update_service_qty,
    )?;
    methods.register(
        "purchase.order",
        "action_view_sale_orders",
        Operation::Read,
        actions::action_view_sale_orders,
    )?;
    methods.register(
        "purchase.order",
        "action_notify_sale_of_cancellation",
        Operation::Write,
        cancel::action_notify_sale_of_cancellation,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------

/// `product.supplierinfo` — from whom, at what price, in how many days.
///
/// Odoo's model lives in `product` and carries far more: validity dates,
/// a currency, a vendor's own code and name for the product, the company
/// it applies to. What is here is what `sale_purchase` reads to build a
/// request for quotation, and nothing else — a stand-in for the day
/// `product` brings the real one.
fn supplier_info() -> Model {
    Model::new(
        meta("product.supplierinfo", "product_supplierinfo"),
        vec![
            m2o("product_id", "product.product")
                .required()
                .ondelete(OnDelete::Cascade),
            m2o("partner_id", "res.partner").required(),
            m2o("company_id", "res.company"),
            Field::new("price", PRICE).default_value(json!(0.0)),
            // the smallest quantity this vendor sells at this price
            Field::new("min_qty", PRICE).default_value(json!(0.0)),
            // days between the order and the service being done
            Field::new("delay", FieldType::Integer).default_value(json!(1)),
            Field::new("sequence", FieldType::Integer).default_value(json!(1)),
        ],
    )
    // Odoo's `_order`: the chosen vendor is the first row that fits, so
    // the order *is* the choice
    .ordered("sequence, min_qty, price, id")
}

/// Why a product cannot be marked "Subcontract Service", port of
/// `_check_service_to_purchase` and of the create-time check next to it.
fn service_to_purchase_is_configured(record: &Map<String, Value>) -> Result<(), String> {
    let wanted = record
        .get("service_to_purchase")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !wanted {
        return Ok(());
    }
    if record.get("type").and_then(Value::as_str) != Some("service") {
        return Err("a product that is not a service cannot raise a request for quotation".into());
    }
    let sellers = record
        .get("seller_ids")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    if sellers == 0 {
        return Err(
            "define the vendor you want this service bought from: without one, selling it \
             could not raise anything"
                .into(),
        );
    }
    Ok(())
}

/// `product.product` extended: what is sold by buying it.
///
/// Odoo puts `service_to_purchase` on `product.template` and makes it
/// company-dependent — the same product may be subcontracted by one
/// company and performed in-house by another. This port has no
/// template and no company-dependent columns, so the flag is one value
/// for the whole database. What is lost is exactly Odoo's
/// multi-company case; what is kept is the decision itself.
fn purchasable_product() -> Model {
    Model::new(
        extension("product.product", "product_product"),
        vec![
            Field::new("service_to_purchase", FieldType::Boolean).default_value(json!(false)),
            Field::new(
                "seller_ids",
                FieldType::One2many {
                    comodel: "product.supplierinfo".into(),
                    inverse: "product_id".into(),
                },
            ),
        ],
    )
    .constrained(
        "a subcontracted service needs a vendor",
        &["service_to_purchase", "type", "seller_ids"],
        service_to_purchase_is_configured,
    )
}

/// `purchase.order.line` extended: which sale line asked for it.
fn purchase_order_line() -> Model {
    Model::new(
        extension("purchase.order.line", "purchase_order_line"),
        vec![
            // no `ondelete`, like Odoo: deleting a sale line must not
            // delete a purchase line the vendor may already have
            // answered, and it must not block the delete either — the
            // link is simply emptied
            m2o("sale_line_id", "sale.order.line"),
            m2o("sale_order_id", "sale.order").related("sale_line_id.order_id"),
        ],
    )
}

/// The distinct sale orders behind a purchase's lines.
fn sale_orders_behind(record: &Map<String, Value>) -> Vec<i64> {
    deduplicated(
        record
            .get("order_line.sale_order_id")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(first_id),
    )
}

/// `sale_order_count` — how many sales this purchase serves.
fn sale_order_count(record: &Map<String, Value>) -> Value {
    json!(sale_orders_behind(record).len())
}

/// `has_sale_order` — whether the stat button is drawn at all.
fn has_sale_order(record: &Map<String, Value>) -> Value {
    json!(!sale_orders_behind(record).is_empty())
}

/// `purchase.order` extended: where it came from.
fn purchase_order() -> Model {
    Model::new(
        extension("purchase.order", "purchase_order"),
        vec![
            // `origin` belongs to Odoo's `purchase`, which this port's
            // `purchase` does not have yet. It is declared here because
            // the generation writes it — it is how a buyer looking at a
            // request for quotation sees which sale asked for it, before
            // opening anything.
            char("origin"),
            // neither count is materialised: both change when ANOTHER
            // record writes its `sale_line_id`, and the recompute only
            // follows the fields of what is being written — a column
            // here would age silently
            Field::new("sale_order_count", FieldType::Integer)
                .computed(&["order_line.sale_order_id"], sale_order_count),
            Field::new("has_sale_order", FieldType::Boolean)
                .computed(&["order_line.sale_order_id"], has_sale_order),
        ],
    )
}

/// `purchase_line_count` — how many purchase lines this sale line
/// raised. It is what says whether the service was already bought.
fn purchase_line_count(record: &Map<String, Value>) -> Value {
    json!(record
        .get("purchase_line_ids")
        .and_then(Value::as_array)
        .map_or(0, Vec::len))
}

/// `purchase_order_ids` — the purchases this sale line landed in.
///
/// Not a field Odoo has: it exists because the count on the order is
/// about *orders*, not lines, and the ORM resolves one relational hop
/// per dependency. Two lines of the same request for quotation must
/// count once, and this is where the hop from line to order happens.
fn purchase_orders_of_line(record: &Map<String, Value>) -> Value {
    json!(deduplicated(
        record
            .get("purchase_line_ids.order_id")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(first_id),
    ))
}

/// `sale.order.line` extended: what buying it produced.
fn sale_order_line() -> Model {
    Model::new(
        extension("sale.order.line", "sale_order_line"),
        vec![
            Field::new(
                "purchase_line_ids",
                FieldType::One2many {
                    comodel: "purchase.order.line".into(),
                    inverse: "sale_line_id".into(),
                },
            ),
            Field::new("purchase_line_count", FieldType::Integer)
                .computed(&["purchase_line_ids"], purchase_line_count),
            Field::new("purchase_order_ids", FieldType::Json)
                .computed(&["purchase_line_ids.order_id"], purchase_orders_of_line),
        ],
    )
}

/// `purchase_order_count` — how many purchases this sale raised.
fn purchase_order_count(record: &Map<String, Value>) -> Value {
    let per_line = record
        .get("order_line.purchase_order_ids")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let orders = deduplicated(
        per_line
            .iter()
            .filter_map(Value::as_array)
            .flatten()
            .filter_map(Value::as_i64),
    );
    json!(orders.len())
}

/// `sale.order` extended: what it had bought for it.
fn sale_order() -> Model {
    Model::new(
        extension("sale.order", "sale_order"),
        vec![Field::new("purchase_order_count", FieldType::Integer)
            .computed(&["order_line.purchase_order_ids"], purchase_order_count)],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(pairs: Vec<(&str, Value)>) -> Map<String, Value> {
        pairs
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect()
    }

    #[test]
    fn a_sale_line_counts_the_purchase_lines_it_raised() {
        let line = record(vec![("purchase_line_ids", json!([7, 9]))]);
        assert_eq!(purchase_line_count(&line), json!(2));
        // a line nobody bought for counts zero, not null
        assert_eq!(purchase_line_count(&Map::new()), json!(0));
    }

    #[test]
    fn a_sale_line_names_each_purchase_once() {
        // two purchase lines in the same request for quotation
        let line = record(vec![(
            "purchase_line_ids.order_id",
            json!([[3, "PO00001"], [3, "PO00001"], [5, "PO00002"]]),
        )]);
        assert_eq!(purchase_orders_of_line(&line), json!([3, 5]));
    }

    #[test]
    fn an_order_counts_purchases_and_not_purchase_lines() {
        // both lines of the sale landed in the same request: one vendor,
        // one document, one number on the stat button
        let order = record(vec![(
            "order_line.purchase_order_ids",
            json!([[3], [3, 5]]),
        )]);
        assert_eq!(purchase_order_count(&order), json!(2));
        assert_eq!(purchase_order_count(&Map::new()), json!(0));
    }

    #[test]
    fn a_purchase_counts_the_sales_behind_it() {
        let purchase = record(vec![(
            "order_line.sale_order_id",
            json!([[11, "SO00001"], [11, "SO00001"], Value::Null]),
        )]);
        assert_eq!(sale_order_count(&purchase), json!(1));
        assert_eq!(has_sale_order(&purchase), json!(true));
        // a purchase nobody sold for draws no button at all
        assert_eq!(sale_order_count(&Map::new()), json!(0));
        assert_eq!(has_sale_order(&Map::new()), json!(false));
    }

    #[test]
    fn a_subcontracted_service_needs_to_be_a_service_and_to_have_a_vendor() {
        let ok = record(vec![
            ("service_to_purchase", json!(true)),
            ("type", json!("service")),
            ("seller_ids", json!([4])),
        ]);
        assert!(service_to_purchase_is_configured(&ok).is_ok());

        let goods = record(vec![
            ("service_to_purchase", json!(true)),
            ("type", json!("consu")),
            ("seller_ids", json!([4])),
        ]);
        let error = service_to_purchase_is_configured(&goods).expect_err("goods are not a service");
        assert!(error.contains("not a service"), "{error}");

        let no_vendor = record(vec![
            ("service_to_purchase", json!(true)),
            ("type", json!("service")),
            ("seller_ids", json!([])),
        ]);
        let error =
            service_to_purchase_is_configured(&no_vendor).expect_err("nobody performs it");
        assert!(error.contains("define the vendor"), "{error}");

        // and an ordinary product is not asked any of this
        let plain = record(vec![("type", json!("consu"))]);
        assert!(service_to_purchase_is_configured(&plain).is_ok());
    }

    #[test]
    fn the_models_extend_the_two_documents_without_losing_them() {
        let mut reg = rusdoo_base::registry().unwrap();
        rusdoo_product::extend(&mut reg).unwrap();
        rusdoo_sale::extend(&mut reg).unwrap();
        rusdoo_purchase::extend(&mut reg).unwrap();
        extend(&mut reg).unwrap();

        assert!(reg.get("product.supplierinfo").is_some());
        let product = reg.get("product.product").unwrap();
        assert!(product.field("service_to_purchase").is_some());
        // what `product` brought is still there, rules included. The
        // sales price is the template's since the delegation landed, so
        // the question a caller asks is the one asked here.
        assert!(reg.field_of(product, "list_price").is_some());
        assert_eq!(
            product.constraints().len(),
            2,
            "the variant's own rule (a cost is not negative) plus this \
             module's (a subcontracted service needs a vendor)"
        );

        let sale_line = reg.get("sale.order.line").unwrap();
        assert!(sale_line.field("purchase_line_ids").is_some());
        assert_eq!(sale_line.meta.table, "sale_order_line");

        let purchase_line = reg.get("purchase.order.line").unwrap();
        assert!(purchase_line.field("sale_line_id").is_some());
        assert_eq!(
            purchase_line.field("sale_order_id").unwrap().related,
            Some("sale_line_id.order_id".to_string())
        );

        // the sale order keeps its `_order` and its unlink rule
        let order = reg.get("sale.order").unwrap();
        assert!(order.field("purchase_order_count").is_some());
        assert_eq!(order.order(), "date_order desc, id desc");
        assert_eq!(order.unlink_hooks().len(), 1);
    }

    #[test]
    fn each_document_carries_its_new_buttons() {
        let mut methods = MethodRegistry::new();
        rusdoo_sale::extend_methods(&mut methods).unwrap();
        rusdoo_purchase::extend_methods(&mut methods).unwrap();
        extend_methods(&mut methods).unwrap();
        for (model, name) in [
            ("sale.order", "action_generate_purchase_orders"),
            ("sale.order", "action_view_purchase_orders"),
            ("sale.order", "action_notify_purchase_of_cancellation"),
            ("sale.order.line", "action_update_service_qty"),
            ("purchase.order", "action_view_sale_orders"),
            ("purchase.order", "action_notify_sale_of_cancellation"),
        ] {
            assert!(methods.get(model, name).is_some(), "{model}.{name}");
        }
        // and the buttons the two modules already had still answer
        assert!(methods.get("sale.order", "action_confirm").is_some());
        assert!(methods.get("purchase.order", "action_cancel").is_some());
    }
}
