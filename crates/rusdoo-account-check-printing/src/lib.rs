//! rusdoo-account-check-printing — port of
//! `odoo/addons/account_check_printing/`: paying by check.
//!
//! The module is small and its subject is narrow: a check is a piece of
//! paper with a number on it, and the number is the whole problem. Either
//! the paper is blank and the system numbers it — in which case two
//! checks must never get the same number and none may be skipped — or the
//! paper is pre-printed and already numbered, in which case the system is
//! being *told* what the numbers are and has to write them down against
//! the right payments, in the order the sheets go through the printer.
//! Everything here follows from that one distinction, which Odoo calls
//! `check_manual_sequencing`.
//!
//! Odoo says of itself that this module "must be used as a dependency for
//! modules that provide country-specific check templates", and it means
//! it: on its own the check layout selection holds only "None", so
//! printing always refuses. That is ported as it stands rather than
//! quietly given a default layout — a check printed on the wrong paper is
//! a check the bank sends back.

pub mod actions;
pub mod models;
pub mod numbering;
pub mod stub;
pub mod words;

pub use models::CHECK_PRINTING;
pub use stub::INV_LINES_PER_STUB;

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::methods::MethodRegistry;
use rusdoo_orm::registry::Registry;

/// The models of the addon.
///
/// Three of them (`account.payment.method`, `account.journal`,
/// `account.payment`) are `account`'s in Odoo and are declared here only
/// because this port's `rusdoo-account` does not have them yet; see
/// [`models`] for what that means and what the integrator should do about
/// it. `res.company` is a real `_inherit`, so `base` must be registered
/// first.
pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(models::payment_method())?;
    reg.register(models::journal())?;
    reg.register(models::payment())?;
    reg.register(models::company())?;
    reg.register(models::prenumbered_wizard())?;
    Ok(())
}

/// The buttons of a payment, of a journal and of the printing dialog.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    methods.register(
        "account.payment",
        "action_post",
        Operation::Write,
        actions::action_post,
    )?;
    methods.register(
        "account.payment",
        "action_cancel",
        Operation::Write,
        actions::action_cancel,
    )?;
    methods.register(
        "account.payment",
        "action_void_check",
        Operation::Write,
        actions::action_void_check,
    )?;
    methods.register(
        "account.payment",
        "unmark_as_sent",
        Operation::Write,
        actions::unmark_as_sent,
    )?;
    // printing writes: it marks the checks sent, and may post them first
    methods.register(
        "account.payment",
        "print_checks",
        Operation::Write,
        actions::print_checks,
    )?;
    methods.register(
        "account.payment",
        "do_print_checks",
        Operation::Write,
        actions::do_print_checks,
    )?;
    // assembling the pages only reads
    methods.register(
        "account.payment",
        "check_get_pages",
        Operation::Read,
        actions::check_get_pages,
    )?;
    methods.register(
        "account.journal",
        "set_check_next_number",
        Operation::Write,
        actions::set_check_next_number,
    )?;
    methods.register(
        "account.journal",
        "action_checks_to_print",
        Operation::Read,
        actions::action_checks_to_print,
    )?;
    methods.register(
        "print.prenumbered.checks",
        "print_checks",
        Operation::Write,
        actions::wizard_print_checks,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        let mut reg = rusdoo_base::registry().unwrap();
        rusdoo_product::extend(&mut reg).unwrap();
        rusdoo_account::extend(&mut reg).unwrap();
        extend(&mut reg).unwrap();
        reg
    }

    #[test]
    fn the_models_register_on_top_of_account() {
        let reg = registry();
        for name in [
            "account.payment.method",
            "account.journal",
            "account.payment",
            "print.prenumbered.checks",
        ] {
            assert!(reg.get(name).is_some(), "{name} must be registered");
        }
        // the dialog is not stored data
        assert!(reg.get("print.prenumbered.checks").unwrap().is_transient());
    }

    #[test]
    fn the_company_is_extended_and_not_replaced() {
        let reg = registry();
        let company = reg.get("res.company").unwrap();
        assert!(company.field("account_check_printing_layout").is_some());
        assert!(company.field("account_check_printing_margin_top").is_some());
        // what `base` put there stays
        assert!(company.field("name").is_some());
        assert!(company.field("vat").is_some());
        assert_eq!(company.meta.table, "res_company");
    }

    #[test]
    fn the_amount_in_words_is_a_column_the_report_can_read() {
        let reg = registry();
        let words = reg
            .get("account.payment")
            .unwrap()
            .field("check_amount_in_words")
            .unwrap();
        // stored, like Odoo's: the report reads it once per check and the
        // amount rarely changes
        assert!(words.stored);
        assert!(words.compute.is_some());
    }

    #[test]
    fn every_button_is_registered_once() {
        let mut methods = MethodRegistry::new();
        extend_methods(&mut methods).unwrap();
        assert_eq!(
            methods.names_for("account.payment"),
            vec![
                "action_cancel",
                "action_post",
                "action_void_check",
                "check_get_pages",
                "do_print_checks",
                "print_checks",
                "unmark_as_sent",
            ]
        );
        assert_eq!(
            methods.names_for("account.journal"),
            vec!["action_checks_to_print", "set_check_next_number"]
        );
        assert!(methods.get("print.prenumbered.checks", "print_checks").is_some());
    }
}
