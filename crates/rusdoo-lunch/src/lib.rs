//! rusdoo-lunch — port of `odoo/addons/lunch/models/`: the office lunch
//! order.
//!
//! A small application with a real spine. Somebody puts a product in a
//! cart; the office places the day's orders with the vendors; the food
//! arrives and everyone is told. Behind it a wallet, made of two things
//! that never meet in one table: the money people paid in
//! (`lunch.cashmove`) and the orders that spent it.
//!
//! What was ported, and what was not:
//!
//! * **Ported**: locations, vendors with their delivery days and
//!   cut-off, product categories, products, extras, orders with their
//!   full state machine, cashmoves, the wallet balance, alerts, and the
//!   two columns lunch adds to `res.company` and `res.users`.
//! * **Not ported**: the per-record `ir.cron` an Odoo vendor and alert
//!   own (see `alerts.rs` and `catalog.rs` — the port's cron runs a named
//!   method, not a snippet of Python held in a column); the outgoing
//!   order email (`suppliers.rs`); the images on products and
//!   categories, which are Odoo's `image.mixin` and not this addon's;
//!   and `lunch.cashmove.report`, an SQL view whose *answer* is computed
//!   in `money.rs` instead.
//!
//! Timezones are the one place this port is knowingly coarser than Odoo.
//! A vendor and an alert both carry a `tz`, and Odoo decides "today" and
//! "is the cut-off past" in it. There is no timezone conversion in this
//! ORM yet — `rusdoo_orm::defaults::today` says the same — so every day
//! and every hour below is UTC's. The field is kept because it is the
//! vendor's answer and dropping it would lose it; what it cannot yet do
//! is change the arithmetic.

pub mod alerts;
pub mod catalog;
pub mod money;
pub mod orders;
pub mod people;
pub mod schedule;
pub mod suppliers;

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::methods::MethodRegistry;
use rusdoo_orm::model::ModelMeta;
use rusdoo_orm::registry::Registry;
use serde_json::Value;

pub(crate) fn char(name: &str) -> Field {
    Field::new(name, FieldType::Char { size: None })
}

pub(crate) fn m2o(name: &str, comodel: &str) -> Field {
    Field::new(
        name,
        FieldType::Many2one {
            comodel: comodel.to_string(),
        },
    )
}

pub(crate) fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![],
        inherits: vec![],
    }
}

/// The id out of a many2one value, which reads as `[id, name]`.
pub(crate) fn first_id(value: &Value) -> Option<i64> {
    match value {
        Value::Array(items) => items.first().and_then(Value::as_i64),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

/// The ids out of an x2many value, which reads as a plain list.
pub(crate) fn ids_of(value: Option<&Value>) -> Vec<i64> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_i64)
        .collect()
}

/// Today, as a date travels on the wire. UTC's today — see the module
/// note on timezones.
pub(crate) fn today() -> String {
    chrono::Utc::now()
        .format(rusdoo_orm::defaults::DATE_FORMAT)
        .to_string()
}

/// Every model of this addon, in dependency order: a many2one may only
/// name a model that is already registered, and the two `_inherit`
/// extensions come last because they point at `lunch.location` and
/// `lunch.product`.
pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(catalog::location())?;
    reg.register(catalog::product_category())?;
    reg.register(catalog::supplier())?;
    reg.register(catalog::topping())?;
    reg.register(catalog::product())?;
    reg.register(orders::order())?;
    reg.register(money::cashmove())?;
    reg.register(alerts::alert())?;
    reg.register(people::company())?;
    reg.register(people::users())?;
    Ok(())
}

/// What somebody can press.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    // ordering is a write on `lunch.order`, and every one of these is
    // registered as the operation it actually performs: the dispatch
    // cannot guess, and guessing wrong is either a hole or a locked door
    methods.register("lunch.order", "add_to_cart", Operation::Create, orders::add_to_cart)?;
    methods.register(
        "lunch.order",
        "update_quantity",
        Operation::Write,
        orders::update_quantity,
    )?;
    methods.register("lunch.order", "action_order", Operation::Write, orders::action_order)?;
    methods.register("lunch.order", "action_send", Operation::Write, orders::action_send)?;
    methods.register(
        "lunch.order",
        "action_confirm",
        Operation::Write,
        orders::action_confirm,
    )?;
    methods.register("lunch.order", "action_cancel", Operation::Write, orders::action_cancel)?;
    methods.register("lunch.order", "action_reset", Operation::Write, orders::action_reset)?;
    methods.register("lunch.order", "action_notify", Operation::Write, orders::action_notify)?;
    // the wallet is a question, not a change
    methods.register(
        "lunch.cashmove",
        "get_wallet_balance",
        Operation::Read,
        money::get_wallet_balance,
    )?;
    methods.register(
        "lunch.supplier",
        "action_send_orders",
        Operation::Write,
        suppliers::action_send_orders,
    )?;
    methods.register(
        "lunch.supplier",
        "action_confirm_orders",
        Operation::Write,
        suppliers::action_confirm_orders,
    )?;
    // marking a favourite touches the caller's own list and nothing
    // else, so a user who may see the menu may do it — the same decision
    // Odoo makes by writing through `self.env.user`
    methods.register(
        "lunch.product",
        "action_toggle_favorite",
        Operation::Read,
        catalog::action_toggle_favorite,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lunch_registry() -> Registry {
        let mut reg = rusdoo_base::registry().expect("the base models");
        extend(&mut reg).expect("the lunch models");
        reg
    }

    #[test]
    fn every_model_registers_on_top_of_base() {
        let reg = lunch_registry();
        for name in [
            "lunch.location",
            "lunch.product.category",
            "lunch.supplier",
            "lunch.topping",
            "lunch.product",
            "lunch.order",
            "lunch.cashmove",
            "lunch.alert",
        ] {
            assert!(reg.get(name).is_some(), "{name} must be registered");
        }
    }

    #[test]
    fn the_extensions_add_to_the_models_they_extend() {
        let reg = lunch_registry();
        let company = reg.get("res.company").expect("res.company");
        assert!(company.field("lunch_minimum_threshold").is_some());
        // what base put there survives the extension
        assert!(company.field("name").expect("name").required);
        assert_eq!(company.meta.table, "res_company", "same table");

        let users = reg.get("res.users").expect("res.users");
        assert!(users.field("last_lunch_location_id").is_some());
        assert!(users.field("groups_id").is_some(), "base's field is kept");
        // and the password is still the secret base declared it to be
        assert!(!users.field("password").expect("password").exposed);
    }

    #[test]
    fn the_two_sides_of_the_favourites_share_one_relation_table() {
        let reg = lunch_registry();
        let product = reg
            .get("lunch.product")
            .and_then(|model| model.field("favorite_user_ids"))
            .expect("the product's side");
        let user = reg
            .get("res.users")
            .and_then(|model| model.field("favorite_lunch_product_ids"))
            .expect("the user's side");
        let table = |field: &Field| match &field.ty {
            FieldType::Many2many {
                relation,
                column1,
                column2,
                ..
            } => (relation.clone(), column1.clone(), column2.clone()),
            other => panic!("expected a many2many, got {other:?}"),
        };
        let (product_rel, product_c1, product_c2) = table(product);
        let (user_rel, user_c1, user_c2) = table(user);
        assert_eq!(product_rel, user_rel, "one table, read from both ends");
        // and the columns are swapped, which is what makes it the *same*
        // link rather than two unrelated ones
        assert_eq!(product_c1, user_c2);
        assert_eq!(product_c2, user_c1);
    }

    #[test]
    fn the_totals_an_order_is_searched_by_have_a_column() {
        let reg = lunch_registry();
        let order = reg.get("lunch.order").expect("lunch.order");
        for name in ["price", "supplier_id", "category_id", "display_toppings"] {
            let field = order.field(name).expect(name);
            assert!(field.stored, "{name} is grouped and searched by");
            assert!(field.compute.is_some(), "{name} is derived, not typed in");
        }
        // and what changes at midnight is not stored anywhere
        let supplier = reg.get("lunch.supplier").expect("lunch.supplier");
        assert!(!supplier.field("available_today").expect("available_today").stored);
    }

    #[test]
    fn every_button_is_registered_once_with_the_access_it_needs() {
        let mut methods = MethodRegistry::new();
        extend_methods(&mut methods).expect("the lunch methods");
        assert_eq!(
            methods.names_for("lunch.order"),
            vec![
                "action_cancel",
                "action_confirm",
                "action_notify",
                "action_order",
                "action_reset",
                "action_send",
                "add_to_cart",
                "update_quantity",
            ]
        );
        assert_eq!(
            methods.names_for("lunch.supplier"),
            vec!["action_confirm_orders", "action_send_orders"]
        );
        // asking what is in a wallet never changes it
        let balance = methods
            .get("lunch.cashmove", "get_wallet_balance")
            .expect("get_wallet_balance");
        assert_eq!(balance.operation, Operation::Read);
        let order = methods
            .get("lunch.order", "action_order")
            .expect("action_order");
        assert_eq!(order.operation, Operation::Write);
    }
}
