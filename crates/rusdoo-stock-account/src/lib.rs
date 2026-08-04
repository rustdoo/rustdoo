//! rusdoo-stock-account — port of `odoo/addons/stock_account/`: the
//! accounting side of a move.
//!
//! `stock` says a product went from one place to another. This module
//! says what that was *worth*: the delivery that left the warehouse this
//! morning took a number out of the company's assets, and which number it
//! took depends on a policy — the product's cost, the weighted average,
//! or the price of the oldest receipt still in stock. That policy is the
//! whole of this addon, and it is in `valuation.rs`, as functions over
//! numbers that can be read and checked without a database.
//!
//! What is here:
//!
//! * the valuation policy, on the company and on the product
//!   (`cost_method`, `valuation`);
//! * `stock.move.value` — what a move was worth, decided when the
//!   transfer is valued and stored, because a value recomputed from
//!   today's prices is not what happened;
//! * `is_in` / `is_out` — which way a move crossed the company's
//!   boundary, which is the question every cost method starts from;
//! * `product.value` — the trail of hand adjustments, Odoo's own record
//!   of a decision that a recomputation would otherwise erase;
//! * `product.total_value` / `avg_cost` — what the stock is worth.
//!
//! What is deliberately **not** here, and why:
//!
//! * **The journal entry.** `_create_account_move` posts a debit and a
//!   credit between the stock valuation account and the location's. This
//!   ORM has no `account.account`, no `account.journal`, and no
//!   `debit`/`credit` on `account.move.line` — so an entry written here
//!   would be two lines pointing at nothing. `stock.move.account_move_id`
//!   and `account.move.stock_move_ids` are ported so that the link is
//!   waiting when the accounts land; nothing writes them yet. The same
//!   goes for `_action_close_stock_valuation` and its cron, for the COGS
//!   lines on a customer invoice, and for
//!   `stock.location.valuation_account_id`.
//! * **Valuation by lot.** `lot_valuated` and everything under it needs
//!   `stock.lot`, which this port does not have.
//! * **Landed costs, analytic lines, the AVCO audit report and the
//!   valuation report**, which are screens over data the port does not
//!   produce yet.
//! * **The policy on the category.** Odoo keeps `cost_method` and
//!   `valuation` on `product.category`, as company-dependent properties
//!   with a fallback to the company. `product.category` exists in this
//!   port now, but a *company-dependent* field does not — one column
//!   holding a different value per company is a piece of the ORM, not of
//!   this addon. So the two fields sit on the product itself with the
//!   company's defaults, which is the same effective answer Odoo reaches
//!   for a category that says nothing, and the answer stops being the
//!   same the day a database needs two policies at once.

pub mod methods;
pub mod models;
pub mod valuation;

pub use methods::extend_methods;
pub use models::extend;

#[cfg(test)]
mod tests {
    use rusdoo_orm::methods::MethodRegistry;

    fn registry() -> rusdoo_orm::registry::Registry {
        let mut reg = rusdoo_base::registry().unwrap();
        rusdoo_product::extend(&mut reg).unwrap();
        rusdoo_account::extend(&mut reg).unwrap();
        rusdoo_stock::extend(&mut reg).unwrap();
        super::extend(&mut reg).unwrap();
        reg
    }

    #[test]
    fn the_models_extend_stock_and_account_without_losing_them() {
        let reg = registry();

        // the extensions add; what `stock` brought stays
        let mv = reg.get("stock.move").expect("stock.move survives");
        assert_eq!(mv.meta.table, "stock_move", "and on the same table");
        assert!(mv.field("product_uom_qty").is_some(), "stock's own field");
        assert!(mv.field("value").is_some());
        assert!(mv.field("is_in").is_some());
        assert_eq!(
            mv.constraints().len(),
            1,
            "stock's positive-quantity rule survives"
        );

        let product = reg.get("product.product").unwrap();
        // through the delegation: since `product.template` landed, the
        // sales price belongs to the template and the variant reaches it
        // by delegating — which is what a caller sees either way
        assert!(
            reg.field_of(product, "list_price").is_some(),
            "product's own field"
        );
        assert!(product.field("cost_method").is_some());
        // the totals are not materialized: they move when a *move* is
        // written, and a stored column would go stale
        assert!(!product.field("total_value").unwrap().stored);

        let company = reg.get("res.company").unwrap();
        assert!(company.field("name").unwrap().required, "base's own field");
        assert!(company.field("cost_method").unwrap().required);

        let account_move = reg.get("account.move").unwrap();
        assert!(
            account_move.field("amount_total").unwrap().stored,
            "account's stored total survives"
        );
        assert!(account_move.field("stock_move_ids").is_some());
    }

    #[test]
    fn the_adjustment_trail_is_a_model_of_its_own() {
        let reg = registry();
        let adjustment = reg.get("product.value").expect("registered");
        assert!(adjustment.field("value").unwrap().required);
        assert!(adjustment.field("user_id").unwrap().required);
        assert!(!adjustment.is_transient(), "a decision is kept, not swept");
        assert_eq!(adjustment.order(), "date desc, id desc");
    }

    #[test]
    fn the_valuation_has_its_four_buttons() {
        let mut methods = MethodRegistry::new();
        rusdoo_stock::extend_methods(&mut methods).unwrap();
        rusdoo_account::extend_methods(&mut methods).unwrap();
        super::extend_methods(&mut methods).unwrap();
        assert_eq!(methods.names_for("product.value"), vec!["action_apply"]);
        assert_eq!(
            methods.names_for("stock.move"),
            vec!["action_adjust_valuation"]
        );
        assert_eq!(
            methods.names_for("product.product"),
            vec!["action_change_standard_price"]
        );
        // and `stock`'s own buttons are still the ones it registered
        assert_eq!(
            methods.names_for("stock.picking"),
            vec![
                "action_cancel",
                "action_confirm",
                "action_done",
                "action_valuate"
            ]
        );
    }
}
