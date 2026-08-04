//! What can be ordered, and from whom: locations, vendors, categories,
//! products and the extras that go on them.
//!
//! Port of `lunch_location.py`, `lunch_supplier.py`,
//! `lunch_product_category.py`, `lunch_product.py` and `lunch_topping.py`.

use crate::schedule;
use crate::{char, first_id, m2o, meta};
use rusdoo_core::RusdooError;
use rusdoo_orm::fields::{Field, FieldType, OnDelete};
use rusdoo_orm::methods::{MethodCtx, MethodFuture};
use rusdoo_orm::model::Model;
use serde_json::{json, Map, Value};

/// The three extras groups a vendor may offer. Odoo numbers them 1, 2, 3
/// and hangs a label and a quantity rule off each — "Extras", "Drinks",
/// and one the vendor names itself.
pub const TOPPING_GROUPS: [i64; 3] = [1, 2, 3];

/// What a vendor requires of one extras group
/// (`lunch_supplier.py::topping_quantity_*`).
fn topping_quantity(name: &str) -> Field {
    Field::new(
        name,
        FieldType::Selection(vec![
            ("0_more".into(), "None or More".into()),
            ("1_more".into(), "One or More".into()),
            ("1".into(), "Only One".into()),
        ]),
    )
    .required()
    .default_value(json!("0_more"))
}

/// `lunch.location` — where the food is delivered.
pub fn location() -> Model {
    Model::new(
        meta("lunch.location", "lunch_location"),
        vec![
            char("name").required(),
            Field::new("address", FieldType::Text),
            m2o("company_id", "res.company").default_from(rusdoo_orm::defaults::USER_COMPANY),
        ],
    )
    .ordered("name, id")
}

/// `lunch.product.category` — pizza, sandwich, burger.
pub fn product_category() -> Model {
    Model::new(
        meta("lunch.product.category", "lunch_product_category"),
        vec![
            char("name").required().translatable(),
            m2o("company_id", "res.company"),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            // Odoo counts with a `_read_group`; the port counts the
            // records the relation points at, which needs the relation to
            // be declared. It is the same number and one fewer concept.
            Field::new(
                "product_ids",
                FieldType::One2many {
                    comodel: "lunch.product".into(),
                    inverse: "category_id".into(),
                },
            ),
            Field::new("product_count", FieldType::Integer)
                .computed(&["product_ids"], product_count),
        ],
    )
    .ordered("name, id")
}

/// `product_count` — how many products hang off this category.
fn product_count(record: &Map<String, Value>) -> Value {
    json!(record
        .get("product_ids")
        .and_then(Value::as_array)
        .map_or(0, Vec::len))
}

/// `lunch.supplier` — who cooks.
///
/// Two things Odoo has here are missing on purpose. The `cron_id`, and
/// with it `_sync_cron`: an Odoo vendor owns an `ir.cron` row that is
/// rewritten on every save so the order mail goes out at the right
/// minute, and the port's `ir.cron` runs a named method rather than a
/// snippet of code — one job per vendor has nothing to point at. And the
/// timezone: `tz` is kept, because it is the vendor's data and losing it
/// would be losing the answer, but every "today" below is UTC's, exactly
/// as `rusdoo_orm::defaults::today` already is.
pub fn supplier() -> Model {
    let mut fields = vec![
        m2o("partner_id", "res.partner").required(),
        // the vendor *is* the partner: the name and the address are read
        // there and edited there, like Odoo's related fields
        char("name").related("partner_id.name"),
        char("email").related("partner_id.email"),
        char("phone").related("partner_id.phone"),
        char("street").related("partner_id.street"),
        char("street2").related("partner_id.street2"),
        char("zip_code").related("partner_id.zip"),
        char("city").related("partner_id.city"),
        m2o("country_id", "res.country").related("partner_id.country_id"),
        m2o("company_id", "res.company").default_from(rusdoo_orm::defaults::USER_COMPANY),
        // whoever orders for everyone, and whose address the mail leaves
        // from
        m2o("responsible_id", "res.users").default_from(rusdoo_orm::defaults::CURRENT_USER),
        Field::new(
            "send_by",
            FieldType::Selection(vec![
                ("phone".into(), "Phone".into()),
                ("mail".into(), "Email".into()),
            ]),
        )
        .default_value(json!("phone")),
        Field::new("automatic_email_time", FieldType::Float { digits: None })
            .required()
            .default_value(json!(12.0)),
        Field::new(
            "moment",
            FieldType::Selection(vec![("am".into(), "AM".into()), ("pm".into(), "PM".into())]),
        )
        .required()
        .default_value(json!("am")),
        Field::new(
            "delivery",
            FieldType::Selection(vec![
                ("delivery".into(), "Delivery".into()),
                ("no_delivery".into(), "No Delivery".into()),
            ]),
        )
        .default_value(json!("no_delivery")),
        Field::new("recurrency_end_date", FieldType::Date),
        char("tz").required().default_value(json!("UTC")),
        Field::new("active", FieldType::Boolean).default_value(json!(true)),
        Field::new(
            "available_location_ids",
            FieldType::Many2many {
                comodel: "lunch.location".into(),
                relation: "lunch_supplier_location_rel".into(),
                column1: "supplier_id".into(),
                column2: "location_id".into(),
            },
        ),
        char("topping_label_1")
            .required()
            .default_value(json!("Extras")),
        char("topping_label_2")
            .required()
            .default_value(json!("Beverages")),
        char("topping_label_3")
            .required()
            .default_value(json!("Extra Label 3")),
        topping_quantity("topping_quantity_1"),
        topping_quantity("topping_quantity_2"),
        topping_quantity("topping_quantity_3"),
        Field::new(
            "topping_ids",
            FieldType::One2many {
                comodel: "lunch.topping".into(),
                inverse: "supplier_id".into(),
            },
        ),
        // not stored, and it could not be: what it answers changes at
        // midnight without anybody writing a row
        Field::new("available_today", FieldType::Boolean)
            .computed(&WEEKDAY_DEPENDS, available_today),
        Field::new("order_deadline_passed", FieldType::Boolean).computed(
            &[
                "mon",
                "tue",
                "wed",
                "thu",
                "fri",
                "sat",
                "sun",
                "recurrency_end_date",
                "send_by",
                "automatic_email_time",
                "moment",
            ],
            deadline_passed,
        ),
    ];
    // Monday to Friday out of the box, like Odoo
    for day in schedule::WEEKDAY_TO_NAME {
        let open = !matches!(day, "sat" | "sun");
        fields.push(Field::new(day, FieldType::Boolean).default_value(json!(open)));
    }
    Model::new(meta("lunch.supplier", "lunch_supplier"), fields)
        // the field is a number of hours before noon: 13.5 is not a time
        // anybody meant, and the mail would go out at half past one in
        // the morning
        .sql_constrained(
            "lunch_supplier_automatic_email_time_range",
            "CHECK(automatic_email_time >= 0 AND automatic_email_time <= 12)",
            "the order time must be between 0 and 12",
        )
}

/// The seven booleans plus the end date: what "is this vendor open" is
/// made of.
const WEEKDAY_DEPENDS: [&str; 8] = [
    "mon",
    "tue",
    "wed",
    "thu",
    "fri",
    "sat",
    "sun",
    "recurrency_end_date",
];

fn available_today(record: &Map<String, Value>) -> Value {
    json!(schedule::available_on_date(
        record,
        chrono::Utc::now().date_naive()
    ))
}

fn deadline_passed(record: &Map<String, Value>) -> Value {
    json!(schedule::order_deadline_passed(
        record,
        chrono::Utc::now().naive_utc()
    ))
}

/// `lunch.topping` — an extra that goes on a product.
///
/// It belongs to a vendor and to one of that vendor's three groups; the
/// group decides which widget it shows up under and which quantity rule
/// it answers to.
pub fn topping() -> Model {
    Model::new(
        meta("lunch.topping", "lunch_topping"),
        vec![
            char("name").required(),
            m2o("company_id", "res.company").default_from(rusdoo_orm::defaults::USER_COMPANY),
            Field::new("price", FieldType::Float { digits: Some((16, 2)) })
                .required()
                .default_value(json!(0.0)),
            // an extra has no life without its vendor
            m2o("supplier_id", "lunch.supplier").ondelete(OnDelete::Cascade),
            Field::new("topping_category", FieldType::Integer)
                .required()
                .default_value(json!(1)),
        ],
    )
    .constrained(
        "lunch_topping_group",
        &["topping_category"],
        is_a_real_group,
    )
    .ordered("name, id")
}

/// A vendor has three extras groups and no more: a topping filed under 4
/// shows up in no widget and is priced into no order.
fn is_a_real_group(record: &Map<String, Value>) -> Result<(), String> {
    let group = record
        .get("topping_category")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if !TOPPING_GROUPS.contains(&group) {
        return Err(format!(
            "extras group {group} does not exist: a vendor has groups 1, 2 and 3"
        ));
    }
    Ok(())
}

/// `lunch.product` — one thing on the menu.
pub fn product() -> Model {
    Model::new(
        meta("lunch.product", "lunch_product"),
        vec![
            char("name").required().translatable(),
            m2o("category_id", "lunch.product.category").required(),
            Field::new("description", FieldType::Html).translatable(),
            Field::new("price", FieldType::Float { digits: Some((16, 2)) })
                .required()
                .default_value(json!(0.0)),
            m2o("supplier_id", "lunch.supplier").required(),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            // Odoo mirrors the vendor's company and stores it so the
            // multi-company rule has a column to filter on; the port has
            // no writable related field, so this is the vendor's company
            // materialized by a compute — same column, same reason.
            m2o("company_id", "res.company")
                .computed(&["supplier_id.company_id"], supplier_company)
                .store(),
            Field::new("new_until", FieldType::Date),
            Field::new("is_new", FieldType::Boolean).computed(&["new_until"], is_new),
            // who put this on their list of favourites. A real relation
            // table, and `res.users` reads it from the other side.
            Field::new(
                "favorite_user_ids",
                FieldType::Many2many {
                    comodel: "res.users".into(),
                    relation: "lunch_product_favorite_user_rel".into(),
                    column1: "product_id".into(),
                    column2: "user_id".into(),
                },
            ),
        ],
    )
    .ordered("name, id")
}

/// `company_id` — the vendor's company, materialized.
fn supplier_company(record: &Map<String, Value>) -> Value {
    let company = record
        .get("supplier_id.company_id")
        .and_then(Value::as_array)
        .and_then(|values| values.first())
        .and_then(first_id);
    json!(company)
}

/// `is_new` — the badge a product carries until the date it was given.
fn is_new(record: &Map<String, Value>) -> Value {
    let until = schedule::as_date(record.get("new_until"));
    json!(until.is_some_and(|until| chrono::Utc::now().date_naive() <= until))
}

/// `action_toggle_favorite` — put this product on the caller's list, or
/// take it off.
///
/// Port of `lunch_product.py`'s `is_favorite` write, which pushes a
/// command onto `self.env.user.favorite_lunch_product_ids`. Whose
/// favourite it is comes from who is calling and never from the
/// arguments: a client that could name the user would be a client that
/// reorganizes somebody else's menu.
pub fn action_toggle_favorite<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [product] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "mark one product at a time".into(),
            ));
        };
        let rows = ctx
            .registry
            .read(ctx.pool, "lunch.product", &[product], &["favorite_user_ids"])
            .await?;
        let listed = rows
            .first()
            .ok_or_else(|| RusdooError::Validation(format!("product {product} does not exist")))?
            .get("favorite_user_ids")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_i64)
            .any(|id| id == ctx.uid);
        // command 3 unlinks the pair, command 4 links it
        let command = if listed { 3 } else { 4 };
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "lunch.product",
                &[product],
                vec![("favorite_user_ids", json!([[command, ctx.uid, 0]]))],
            )
            .await?;
        Ok(json!(!listed))
    })
}

/// One vendor, read with everything the order rules ask of it: which
/// days it is open, when it closes, and what it requires of each extras
/// group.
pub async fn read_supplier(
    ctx: &MethodCtx<'_>,
    id: i64,
) -> Result<Map<String, Value>, RusdooError> {
    let mut fields: Vec<&str> = vec![
        "name",
        "send_by",
        "automatic_email_time",
        "moment",
        "recurrency_end_date",
        "topping_label_1",
        "topping_label_2",
        "topping_label_3",
        "topping_quantity_1",
        "topping_quantity_2",
        "topping_quantity_3",
    ];
    fields.extend(schedule::WEEKDAY_TO_NAME);
    ctx.registry
        .read(ctx.pool, "lunch.supplier", &[id], &fields)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| RusdooError::Validation(format!("vendor {id} does not exist")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_topping_belongs_to_one_of_the_three_groups() {
        let mut record = Map::new();
        for group in TOPPING_GROUPS {
            record.insert("topping_category".into(), json!(group));
            assert!(is_a_real_group(&record).is_ok(), "{group} is a group");
        }
        record.insert("topping_category".into(), json!(4));
        let error = is_a_real_group(&record).expect_err("there is no fourth group");
        assert!(error.contains("groups 1, 2 and 3"), "{error}");
    }

    #[test]
    fn a_category_counts_the_products_that_point_at_it() {
        let mut record = Map::new();
        record.insert("product_ids".into(), json!([4, 9, 12]));
        assert_eq!(product_count(&record), json!(3));
        // and an empty category counts zero, not null
        assert_eq!(product_count(&Map::new()), json!(0));
    }

    #[test]
    fn a_product_without_a_new_until_date_is_never_new() {
        assert_eq!(is_new(&Map::new()), json!(false));
        let mut record = Map::new();
        record.insert("new_until".into(), json!("1999-01-01"));
        assert_eq!(is_new(&record), json!(false), "the badge expired");
        record.insert("new_until".into(), json!("2999-01-01"));
        assert_eq!(is_new(&record), json!(true));
    }

    #[test]
    fn a_product_takes_the_company_of_its_vendor() {
        let mut record = Map::new();
        // as the ORM hands a hop over: one value, and a m2o reads as a pair
        record.insert("supplier_id.company_id".into(), json!([[3, "Farm"]]));
        assert_eq!(supplier_company(&record), json!(3));
        assert_eq!(supplier_company(&Map::new()), json!(null));
    }
}
