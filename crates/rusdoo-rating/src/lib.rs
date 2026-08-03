//! rusdoo-rating — port of `odoo/addons/rating/`: what the customer
//! thought of it.
//!
//! One model, `rating.rating`, pointing at any record at all through the
//! pair `(res_model, res_id)`. That generality is the whole addon: a
//! ticket, an order and a delivery are rated by the same table, and none
//! of them has to know this module exists. What a module *does* declare
//! is that its records can be rated — [`extend_rated_models`] — which is
//! the port of `_inherit = ['rating.mixin']`.
//!
//! The flow is Odoo's. A record hands out an access token
//! (`rating_get_access_token`); the token travels in a mail; the answer
//! comes back through `rating_apply`, which fills the rating in and posts
//! it on the record's chatter. A rating that was asked for and never
//! answered stays `consumed = false` and is worth nothing in every
//! statistic — silence is not a bad review.
//!
//! What is deliberately left out, and why:
//!
//! * `res_model_id` / `parent_res_model_id`, the many2ones to `ir.model`:
//!   there is no `ir.model` in this port. The technical name in
//!   `res_model` is the handle, as `data_recycle` already does.
//! * `resource_ref` / `parent_ref`, Odoo's `Reference` fields: the ORM
//!   has no reference type, and the two fields are a widget's convenience
//!   over the pair that *is* stored.
//! * `rating_image`, the `Binary` that base64s the addon's own PNG on
//!   every read. `rating_image_url` is here and the images are in the
//!   addon, so the client draws the face from a URL instead of from a
//!   column — which is what `rating_image_url` exists for.
//! * the HTTP controller (`/rate/<token>/<rate>`), the portal templates
//!   and the JavaScript: the port has no website/portal layer to hang
//!   them on. Everything they call is here.
//! * `rating_send_request`, which posts a `mail.template` on the record:
//!   there are no mail templates in this port yet.

mod apply;
mod data;
mod models;
mod stats;

pub use data::{
    rating_avg_to_text, rating_image_url, rating_to_grade, rating_to_text, rating_to_threshold,
    stats_of, Stats, RATING_TEXT,
};
pub use models::{MESSAGE, RATING};

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::methods::MethodRegistry;
use rusdoo_orm::registry::Registry;

/// Register `rating.rating` and the rating a message carries.
///
/// `mail` must already be registered: the message this extends and the
/// message a rating points at are the same model, and the addon depends
/// on it.
pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(models::rating())?;
    reg.register(models::message())?;
    Ok(())
}

/// The two buttons a rating carries on its own screens.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    // opening the rated record reads it; the dispatcher checks the rating,
    // and the act_window it answers with is checked again when the client
    // opens it
    methods.register(
        RATING,
        "action_open_rated_object",
        Operation::Read,
        apply::action_open_rated_object,
    )?;
    // resetting throws an answer away and reissues the link
    methods.register(RATING, "reset", Operation::Write, apply::reset)?;
    Ok(())
}

/// Declare that the records of `models` can be rated — the port of
/// `_inherit = ['rating.mixin']`.
///
/// There are no mixins in this ORM, so a module says the same thing by
/// naming its models here, exactly as `rusdoo-mail` does for the chatter.
///
/// Everything is a `Read`, and that is deliberate. Handing out a token
/// and answering with it are what a *customer* does, and a customer who
/// may see an order but not edit it must still be able to say what they
/// thought of it — the same reason posting on a chatter is a read in
/// `rusdoo-mail`, and the same access Odoo requires before it drops into
/// `sudo()`.
pub fn extend_rated_models(
    methods: &mut MethodRegistry,
    models: &[&str],
) -> Result<(), RusdooError> {
    for model in models {
        methods.register(
            model,
            "rating_get_access_token",
            Operation::Read,
            apply::rating_get_access_token,
        )?;
        methods.register(model, "rating_apply", Operation::Read, apply::rating_apply)?;
        methods.register(
            model,
            "rating_get_stats",
            Operation::Read,
            stats::rating_get_stats,
        )?;
        methods.register(
            model,
            "rating_get_grades",
            Operation::Read,
            stats::rating_get_grades,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusdoo_orm::fields::FieldType;

    fn registry() -> Registry {
        let mut reg = rusdoo_base::registry().expect("base registers");
        rusdoo_mail::extend(&mut reg).expect("mail registers");
        extend(&mut reg).expect("rating registers");
        reg
    }

    #[test]
    fn the_model_registers_on_top_of_mail() {
        let reg = registry();
        let rating = reg.get(RATING).expect("rating.rating is registered");
        // the pair that points at anything at all
        assert!(rating.field("res_model").expect("res_model").required);
        assert!(rating.field("res_id").expect("res_id").required);
        // the text is materialised, so a list can be grouped by it
        let text = rating.field("rating_text").expect("rating_text");
        assert!(text.stored && text.compute.is_some());
        // and the URL is not: it is a string built from the value
        let url = rating.field("rating_image_url").expect("rating_image_url");
        assert!(!url.stored && url.compute.is_some());
        // the range is the database's business, like Odoo's `_rating_range`
        assert!(rating
            .sql_constraints()
            .iter()
            .any(|constraint| constraint.definition.contains("rating <= 5")));
        assert_eq!(rating.order(), "write_date desc, id desc");
    }

    #[test]
    fn the_message_keeps_what_mail_gave_it() {
        let reg = registry();
        let message = reg.get(MESSAGE).expect("mail.message is registered");
        // the extension adds
        assert!(matches!(
            message.field("rating_ids").expect("rating_ids").ty,
            FieldType::One2many { ref comodel, .. } if comodel == RATING
        ));
        assert!(message.field("rating_value").is_some());
        // and does not take away
        assert!(message.field("body").is_some());
        assert!(message.field("author_id").is_some());
        assert_eq!(message.meta.table, "mail_message");
    }

    #[test]
    fn a_rating_needs_mail_before_it_can_register() {
        let mut reg = rusdoo_base::registry().expect("base registers");
        let error = extend(&mut reg).expect_err("mail.message is not there yet");
        assert!(error.to_string().contains("_inherit parent"), "{error}");
    }

    #[test]
    fn a_rated_model_gets_the_four_methods_and_the_rating_its_two() {
        let mut methods = MethodRegistry::new();
        extend_methods(&mut methods).expect("the rating's methods register");
        extend_rated_models(&mut methods, &["sale.order"]).expect("a rated model registers");
        assert_eq!(
            methods.names_for(RATING),
            vec!["action_open_rated_object", "reset"]
        );
        assert_eq!(
            methods.names_for("sale.order"),
            vec![
                "rating_apply",
                "rating_get_access_token",
                "rating_get_grades",
                "rating_get_stats"
            ]
        );
    }
}
