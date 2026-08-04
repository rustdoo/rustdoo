//! rusdoo-calendar — port of `odoo/addons/calendar/`: meetings, the
//! people at them, the reminders they carry and the rules that repeat
//! them.
//!
//! Six models and one relationship that runs through all of them. A
//! `calendar.event` is a meeting; a `calendar.attendee` is one person's
//! place at it, kept as a record of its own and not as a link row because
//! it holds an answer — yes, no, maybe — that a link cannot. A
//! `calendar.recurrence` is the rule a repeating meeting follows, and the
//! occurrences it produces are ordinary events with a pointer back at it,
//! which is what lets somebody move one of them without moving the rest.
//!
//! Everything about expanding a rule into dates lives in [`rrule`], away
//! from the database, because that is where a calendar is wrong in ways
//! nobody notices for a month.
//!
//! ## What is deliberately not here
//!
//! * **Invitation and reminder emails.** They are most of
//!   `calendar_attendee.py` and all of `calendar_alarm_manager.py`, and
//!   every line of them needs `mail.template` and the rendering behind
//!   it, which the port does not have. `calendar.alarm` is here, with its
//!   lead time as a real column, so the scheduler has something to ask;
//!   what it would send is missing. Same for `mail_template_id` on the
//!   alarm.
//! * **The activity bridge.** `calendar.event.activity_ids` and the
//!   `mail.activity` a meeting creates on the record it was scheduled
//!   from: there is no `mail.activity` model in the port.
//! * **Privacy as an access rule.** `privacy` is stored and the field is
//!   here, but Odoo enforces it by rewriting reads (`_fetch_query`
//!   replaces a private event's name with "Busy") and by an `ir.rule`
//!   that reads `res.users.calendar_default_privacy`. Neither the read
//!   rewrite nor a rule with a subquery over user settings exists here,
//!   and half-enforced privacy is worse than none: the field is stored
//!   and honestly does nothing yet. See the report.
//! * **Discuss video calls.** `videocall_channel_id` points at
//!   `discuss.channel`, which the port does not have; `videocall_location`
//!   is kept as the plain URL somebody pasted, which is Odoo's `custom`
//!   source.
//! * **The delete popover and the provider-configuration wizards.** Both
//!   are `mail.composer.mixin` transients whose whole job is choosing a
//!   template to send.
//! * **The guest list as a side effect of `partner_ids`.** In Odoo,
//!   writing `partner_ids` creates the `calendar.attendee` rows inside
//!   `create`/`write`; a model here cannot hook either, so the two are
//!   done together in `action_join_meeting` instead. Everything the
//!   port itself calls goes through that method — but a client that
//!   writes `partner_ids` straight onto the form, as Odoo's own does,
//!   puts somebody on the guest list with nowhere to answer from. The
//!   piece that closes it is a create/write hook in the ORM, not
//!   anything in this addon.

pub mod methods;
pub mod models;
pub mod rrule;

pub use models::extend;

use rusdoo_core::RusdooError;
use rusdoo_orm::methods::MethodRegistry;

/// Attach the calendar's methods.
pub fn extend_methods(m: &mut MethodRegistry) -> Result<(), RusdooError> {
    methods::extend_methods(m)
}

/// The models that carry a chatter, for a server wiring `mail` up.
///
/// `calendar.event` is `_inherit = ['mail.thread']` in Odoo, and the
/// answers to an invitation are posted on that thread. This crate does
/// not attach the thread itself — `mail` owns that, and a module cannot
/// know whether it is installed — it only says which of its models wants
/// one.
pub const THREAD_MODELS: [&str; 1] = ["calendar.event"];
