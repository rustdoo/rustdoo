//! The models: `rating.rating`, and what a rating adds to a message.

use crate::data;
use rusdoo_core::RusdooError;
use rusdoo_orm::fields::{DefaultCtx, Field, FieldType, OnDelete};
use rusdoo_orm::model::{Model, ModelMeta};
use serde_json::{json, Map, Value};
use std::future::Future;
use std::pin::Pin;

/// The model every other addon points at.
pub const RATING: &str = "rating.rating";
/// The message a rating shows up in, from `rusdoo-mail`.
pub const MESSAGE: &str = "mail.message";

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

fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![],
        inherits: vec![],
    }
}

/// The rating value of a record, as the ORM hands it over.
fn rating_of(record: &Map<String, Value>) -> f64 {
    record.get("rating").and_then(Value::as_f64).unwrap_or(0.0)
}

/// `_compute_rating_text`.
fn rating_text(record: &Map<String, Value>) -> Value {
    json!(data::rating_to_text(rating_of(record)))
}

/// `_compute_rating_image` — the URL half of it. The bytes half
/// (`rating_image`, a `Binary` read off the addon's own PNG) is left out:
/// see the crate documentation.
fn rating_image_url(record: &Map<String, Value>) -> Value {
    json!(data::rating_image_url(rating_of(record)))
}

/// `_default_access_token` — `uuid.uuid4().hex`.
///
/// The token is the whole security of the customer flow: it travels in a
/// mail, in a URL, and it is the only thing standing between a stranger
/// and somebody else's satisfaction survey. A random v4 is what Odoo
/// uses, and the reason it is not a counter or a hash of the record.
pub fn new_access_token(
    _ctx: DefaultCtx<'_>,
) -> Pin<Box<dyn Future<Output = Result<Value, RusdooError>> + Send + '_>> {
    Box::pin(async move { Ok(json!(uuid::Uuid::new_v4().simple().to_string())) })
}

/// `rating.rating` — one opinion about one record.
///
/// The relation to what was rated is the pair `(res_model, res_id)`, and
/// that pair is the whole point of the addon: any model at all can be
/// rated without knowing this one exists.
///
/// Deviation from Odoo: `res_model_id` / `parent_res_model_id` (many2ones
/// to `ir.model`) are not here — this port has no `ir.model` — so the
/// technical name in `res_model` is the only handle, as `data_recycle`
/// already does for the same reason. `resource_ref` and `parent_ref`
/// (`Reference` fields) go with them: the ORM has no reference type.
pub fn rating() -> Model {
    Model::new(
        meta(RATING, "rating_rating"),
        vec![
            // what was rated
            char("res_model").required(),
            Field::new("res_id", FieldType::Integer).required(),
            // the target's name, written down when the rating is created
            // rather than computed on every read. Odoo recomputes it from
            // the record's `display_name`; a compute here cannot reach the
            // database, and the name at the time of the rating is in any
            // case what survives the record being deleted.
            char("res_name"),
            // the record the rated one hangs from — a team, a project —
            // filled by whoever creates the rating. Odoo finds it through
            // `_rating_get_parent_field_name`, a hook a mixin provides.
            char("parent_res_model"),
            Field::new("parent_res_id", FieldType::Integer),
            char("parent_res_name"),
            // who is being rated, and who is rating
            m2o("rated_partner_id", "res.partner"),
            m2o("partner_id", "res.partner"),
            Field::new("rating", FieldType::Float { digits: None }).default_value(json!(0)),
            // materialised: every list, filter and group-by in the addon's
            // own views works on this column, and a rating changes value
            // once in its life
            Field::new("rating_text", FieldType::Selection(selection()))
                .computed(&["rating"], rating_text)
                .store(),
            char("rating_image_url").computed(&["rating"], rating_image_url),
            Field::new("feedback", FieldType::Text),
            // the chatter message the rating was posted as. Cascade like
            // Odoo: deleting the message deletes the rating, because a
            // rating whose message is gone is a number nobody can place.
            m2o("message_id", MESSAGE).ondelete(OnDelete::Cascade),
            // Odoo relates this to `message_id.is_internal` and stores it;
            // `mail.message` here has no such field, so it is the plain
            // stored flag the related one materialises into.
            Field::new("is_internal", FieldType::Boolean).default_value(json!(false)),
            char("access_token").default_from(new_access_token),
            // false until somebody actually answered: a rating request
            // that was sent and ignored is not a rating of zero
            Field::new("consumed", FieldType::Boolean).default_value(json!(false)),
            Field::new("rated_on", FieldType::Datetime),
        ],
    )
    // `_order`: the most recently touched first, like Odoo
    .ordered("write_date desc, id desc")
    .sql_constrained(
        "rating_rating_rating_range",
        "CHECK (rating >= 0 AND rating <= 5)",
        "A rating is a value between 0 and 5",
    )
    // not in Odoo, which trusts uuid4 alone. The token is looked up with
    // no other condition, so two rows sharing one would mean a customer
    // answering somebody else's survey — cheap to make impossible, and
    // impossible to notice otherwise.
    .sql_constrained(
        "rating_rating_access_token_uniq",
        "UNIQUE (access_token)",
        "Two ratings cannot share one access token",
    )
}

/// The `rating_text` selection, from `RATING_TEXT`.
fn selection() -> Vec<(String, String)> {
    data::RATING_TEXT
        .iter()
        .map(|(value, label)| ((*value).to_string(), (*label).to_string()))
        .collect()
}

/// `_compute_rating_value` on `mail.message`: the value of the message's
/// rating, or zero.
///
/// The latest is the one created last, and the creation date is read
/// rather than inferred from the order the lines arrive in. The comodel's
/// `_order` is `write_date desc` (Odoo's own), which is a different
/// question: edit an old rating and it moves to the front while the newer
/// one stays behind. Odoo settles it explicitly with
/// `sorted("create_date", reverse=True)[:1]` (mail_message.py:20-21), and
/// so does this.
fn message_rating_value(record: &Map<String, Value>) -> Value {
    let values = list(record, "rating_ids.rating");
    let consumed = list(record, "rating_ids.consumed");
    let created = list(record, "rating_ids.create_date");
    let mut latest: Option<(&str, f64)> = None;
    for (index, value) in values.iter().enumerate() {
        if consumed.get(index).and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let rating = value.as_f64().unwrap_or(0.0);
        // the stamps are `YYYY-MM-DD HH:MM:SS`, so comparing them as
        // text compares them as moments
        let when = created.get(index).and_then(Value::as_str).unwrap_or("");
        match latest {
            Some((seen, _)) if seen >= when => {}
            _ => latest = Some((when, rating)),
        }
    }
    json!(latest.map(|(_, rating)| rating).unwrap_or(0.0))
}

fn list<'a>(record: &'a Map<String, Value>, name: &str) -> &'a [Value] {
    record
        .get(name)
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
}

/// `mail.message` extended (`_inherit`): a message can carry a rating.
pub fn message() -> Model {
    Model::new(
        ModelMeta {
            name: MESSAGE.into(),
            table: "mail_message".into(),
            inherit: vec![MESSAGE.into()],
            inherits: vec![],
        },
        vec![
            Field::new(
                "rating_ids",
                FieldType::One2many {
                    comodel: RATING.into(),
                    inverse: "message_id".into(),
                },
            ),
            // not materialised, exactly as in Odoo (`store=False`): the
            // value changes when the *rating* is written, and a column
            // here would only be refreshed by a write to the message
            Field::new("rating_value", FieldType::Float { digits: None }).computed(
                &[
                    "rating_ids.rating",
                    "rating_ids.consumed",
                    "rating_ids.create_date",
                ],
                message_rating_value,
            ),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_text_follows_the_value() {
        let mut record = Map::new();
        record.insert("rating".into(), json!(5));
        assert_eq!(rating_text(&record), json!("top"));
        assert_eq!(
            rating_image_url(&record),
            json!("/rating/static/src/img/rating_5.png")
        );
        // a rating nobody answered yet reads as "none", not as null
        assert_eq!(rating_text(&Map::new()), json!("none"));
    }

    #[test]
    fn a_message_shows_the_latest_answered_rating() {
        let mut record = Map::new();
        // newest first, as the one2many is read; the newest was not
        // answered, so the message shows the one that was
        record.insert("rating_ids.rating".into(), json!([0.0, 4.0, 1.0]));
        record.insert("rating_ids.consumed".into(), json!([false, true, true]));
        record.insert(
            "rating_ids.create_date".into(),
            json!(["2026-01-03 10:00:00", "2026-01-02 10:00:00", "2026-01-01 10:00:00"]),
        );
        assert_eq!(message_rating_value(&record), json!(4.0));
    }

    #[test]
    fn a_message_with_no_answered_rating_is_worth_zero() {
        let mut record = Map::new();
        record.insert("rating_ids.rating".into(), json!([0.0]));
        record.insert("rating_ids.consumed".into(), json!([false]));
        record.insert("rating_ids.create_date".into(), json!(["2026-01-01 10:00:00"]));
        assert_eq!(message_rating_value(&record), json!(0.0));
        assert_eq!(message_rating_value(&Map::new()), json!(0.0));
    }
}
