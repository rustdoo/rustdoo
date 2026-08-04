//! The order: one person, one product, one day.
//!
//! Port of `odoo/addons/lunch/models/lunch_order.py`.
//!
//! Two deviations, both in the report:
//!
//! * Odoo splits the extras into `topping_ids_1/2/3`, three many2many
//!   fields **over one relation table**, each filtered by
//!   `topping_category`. Its own code says what that costs: `write` has
//!   to pop all three, merge them, write only the first, and invalidate
//!   the cache of the other two — with a `TODO` admitting the result is
//!   wrong for more than one order at a time. Here there is one
//!   `topping_ids`, and the group a topping belongs to is read off the
//!   topping, where it already lives. Nothing a user does changes: the
//!   three widgets are three filtered views of one field.
//! * merging a repeated order is `add_to_cart`, not an override of
//!   `create`. Odoo's `create` looks for a line that matches and bumps
//!   its quantity instead of writing a second row; this ORM has no
//!   create hook, and a create that silently returned somebody else's id
//!   would be worse than a method that says what it does.

use crate::money;
use crate::{char, first_id, ids_of, m2o, meta, today};
use rusdoo_core::RusdooError;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::methods::{MethodCtx, MethodFuture};
use rusdoo_orm::model::Model;
use serde_json::{json, Map, Value};

/// The states an order passes through (`lunch_order.py::state`).
const STATES: [(&str, &str); 5] = [
    ("new", "To Order"),
    // ordered internally: the office knows, the vendor does not
    ("ordered", "Ordered"),
    ("sent", "Sent"),
    ("confirmed", "Received"),
    ("cancelled", "Cancelled"),
];

/// The states in which the vendor already has the order, so changing it
/// changes nothing that has not already been cooked
/// (`update_quantity`'s filter).
const LOCKED_STATES: [&str; 2] = ["sent", "confirmed"];

/// `lunch.order` — one line of somebody's lunch.
pub fn order() -> Model {
    Model::new(
        meta("lunch.order", "lunch_order"),
        vec![
            m2o("product_id", "lunch.product").required(),
            char("name").related("product_id.name"),
            // one field, not Odoo's three: see the module doc
            Field::new(
                "topping_ids",
                FieldType::Many2many {
                    comodel: "lunch.topping".into(),
                    relation: "lunch_order_topping".into(),
                    column1: "order_id".into(),
                    column2: "topping_id".into(),
                },
            ),
            // stored, because the manager's screens group orders by
            // vendor and by category — and an unstored value has no
            // column to group on. Odoo stores them for the same reason,
            // as `related=..., store=True`; this ORM has no writable
            // related field, so it is a compute over the same path.
            m2o("category_id", "lunch.product.category")
                .computed(&["product_id.category_id"], product_category)
                .store(),
            m2o("supplier_id", "lunch.supplier")
                .computed(&["product_id.supplier_id"], product_supplier)
                .store(),
            Field::new("date", FieldType::Date)
                .required()
                .default_from(rusdoo_orm::defaults::TODAY),
            m2o("user_id", "res.users").default_from(rusdoo_orm::defaults::CURRENT_USER),
            m2o("lunch_location_id", "lunch.location"),
            Field::new("note", FieldType::Text),
            Field::new("quantity", FieldType::Float { digits: Some((16, 2)) })
                .required()
                .default_value(json!(1.0)),
            // what the wallet is charged: the product plus its extras,
            // times the quantity. Stored, because the balance sums it
            // over a person's whole history.
            Field::new("price", FieldType::Float { digits: Some((16, 2)) })
                .computed(
                    &["quantity", "product_id.price", "topping_ids.price"],
                    total_price,
                )
                .store(),
            Field::new("display_toppings", FieldType::Text)
                .computed(&["topping_ids.name"], display_toppings)
                .store(),
            Field::new(
                "state",
                FieldType::Selection(
                    STATES
                        .iter()
                        .map(|(key, label)| ((*key).to_string(), (*label).to_string()))
                        .collect(),
                ),
            )
            .default_value(json!("new")),
            // whether the "your lunch is here" notice already went out;
            // it is what keeps a second click from telling everybody
            // twice
            Field::new("notified", FieldType::Boolean).default_value(json!(false)),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
            m2o("company_id", "res.company").default_from(rusdoo_orm::defaults::USER_COMPANY),
        ],
    )
    // Odoo's `_order`: the newest line first, because the screen a user
    // opens is "what did I order"
    .ordered("id desc")
}

/// `price` — port of `_compute_total_price`.
fn total_price(record: &Map<String, Value>) -> Value {
    let quantity = record
        .get("quantity")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let product = sum_values(record, "product_id.price");
    let extras = sum_values(record, "topping_ids.price");
    json!(money::round2(quantity * (product + extras)))
}

/// The numbers behind a dependency the ORM gathered over a relation.
fn sum_values(record: &Map<String, Value>, path: &str) -> f64 {
    record
        .get(path)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_f64)
        .sum()
}

/// `display_toppings` — port of `_compute_display_toppings`: what the
/// kitchen reads under the product's name.
fn display_toppings(record: &Map<String, Value>) -> Value {
    let names: Vec<&str> = record
        .get("topping_ids.name")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    json!(names.join(" + "))
}

/// The id at the end of a one-hop dependency, which arrives as a list of
/// one many2one pair.
fn hop_id(record: &Map<String, Value>, path: &str) -> Value {
    json!(record
        .get(path)
        .and_then(Value::as_array)
        .and_then(|values| values.first())
        .and_then(first_id))
}

fn product_category(record: &Map<String, Value>) -> Value {
    hop_id(record, "product_id.category_id")
}

fn product_supplier(record: &Map<String, Value>) -> Value {
    hop_id(record, "product_id.supplier_id")
}

// ---------------------------------------------------------------------
// The extras rule
// ---------------------------------------------------------------------

/// Port of `_check_topping_quantity`: what a vendor requires of one
/// extras group.
///
/// `available` is whether the vendor offers anything in that group at
/// all — a rule about a group with no toppings in it would refuse every
/// order for no reason anybody could act on.
pub fn check_topping_quantity(
    label: &str,
    rule: &str,
    chosen: usize,
    available: bool,
) -> Result<(), String> {
    if !available || rule == "0_more" {
        return Ok(());
    }
    match rule {
        "1" if chosen != 1 => Err(format!("You have to order one and only one {label}")),
        "1_more" if chosen == 0 => Err(format!("You should order at least one {label}")),
        _ => Ok(()),
    }
}

/// Hold a choice of extras to the vendor's three rules.
async fn check_toppings(
    ctx: &MethodCtx<'_>,
    supplier: i64,
    chosen: &[i64],
) -> Result<(), RusdooError> {
    let vendor = crate::catalog::read_supplier(ctx, supplier).await?;
    let offered = ctx
        .registry
        .search(
            ctx.pool,
            "lunch.topping",
            &parse_domain(&json!([["supplier_id", "=", supplier]]))?,
            &SearchOptions::default(),
        )
        .await?;
    let offered_rows = if offered.is_empty() {
        Vec::new()
    } else {
        ctx.registry
            .read(ctx.pool, "lunch.topping", &offered, &["topping_category"])
            .await?
    };
    // an extra from another vendor is priced into the order and cooked by
    // nobody. Odoo leaves it to the form's domain; a method that any
    // client may call has to say so itself.
    for id in chosen {
        if !offered.contains(id) {
            return Err(RusdooError::Validation(format!(
                "extra {id} is not offered by {}",
                vendor.get("name").and_then(Value::as_str).unwrap_or("this vendor")
            )));
        }
    }
    for group in crate::catalog::TOPPING_GROUPS {
        let in_group = |rows: &[Map<String, Value>], only: Option<&[i64]>| {
            rows.iter()
                .filter(|row| row.get("topping_category").and_then(Value::as_i64) == Some(group))
                .filter(|row| match only {
                    None => true,
                    Some(ids) => row
                        .get("id")
                        .and_then(Value::as_i64)
                        .is_some_and(|id| ids.contains(&id)),
                })
                .count()
        };
        let available = in_group(&offered_rows, None) > 0;
        let picked = in_group(&offered_rows, Some(chosen));
        let label = vendor
            .get(&format!("topping_label_{group}"))
            .and_then(Value::as_str)
            .unwrap_or("extras");
        let rule = vendor
            .get(&format!("topping_quantity_{group}"))
            .and_then(Value::as_str)
            .unwrap_or("0_more");
        check_topping_quantity(label, rule, picked, available).map_err(RusdooError::Validation)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Methods
// ---------------------------------------------------------------------

/// What a client sends to put something in the cart. `kwargs` is the
/// values dict Odoo would have passed to `create`.
fn wanted(kwargs: &Map<String, Value>, rest: &[Value], name: &str) -> Option<Value> {
    kwargs
        .get(name)
        .or_else(|| {
            rest.first()
                .and_then(Value::as_object)
                .and_then(|values| values.get(name))
        })
        .cloned()
        .filter(|value| !value.is_null())
}

/// `add_to_cart` — order a product, or one more of what is already
/// there.
///
/// Port of `lunch_order.py::create` + `_find_matching_lines`: ordering
/// the same product, with the same note, extras and delivery point, on
/// the same day, is a second helping — not a second line. Only lines
/// still in `new` merge, because a line the office already ordered is a
/// line the vendor has been told about.
pub fn add_to_cart<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let product = wanted(kwargs, &ctx.rest, "product_id")
            .and_then(|value| value.as_i64())
            .ok_or_else(|| {
                RusdooError::Validation("say what to order: pass product_id".into())
            })?;
        let user = wanted(kwargs, &ctx.rest, "user_id")
            .and_then(|value| value.as_i64())
            .unwrap_or(ctx.uid);
        let date = wanted(kwargs, &ctx.rest, "date")
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_else(today);
        let note = wanted(kwargs, &ctx.rest, "note")
            .and_then(|value| value.as_str().map(str::to_string));
        let location = wanted(kwargs, &ctx.rest, "lunch_location_id").and_then(|v| v.as_i64());
        let quantity = wanted(kwargs, &ctx.rest, "quantity")
            .and_then(|value| value.as_f64())
            .unwrap_or(1.0);
        let mut toppings: Vec<i64> = wanted(kwargs, &ctx.rest, "topping_ids")
            .as_ref()
            .and_then(Value::as_array)
            .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        toppings.sort_unstable();
        toppings.dedup();

        let rows = ctx
            .registry
            .read(
                ctx.pool,
                "lunch.product",
                &[product],
                &["name", "active", "supplier_id"],
            )
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| RusdooError::Validation(format!("product {product} does not exist")))?;
        if row.get("active").and_then(Value::as_bool) == Some(false) {
            return Err(RusdooError::Validation(
                "that product is no longer available".into(),
            ));
        }
        let supplier = row.get("supplier_id").and_then(first_id).ok_or_else(|| {
            RusdooError::Validation("that product has no vendor: nobody would cook it".into())
        })?;
        check_toppings(&ctx, supplier, &toppings).await?;

        // the same line already in the cart takes the extra helping
        if let Some(existing) = matching_line(&ctx, user, product, &date, note.as_deref(), location, &toppings).await? {
            bump(&ctx, &[existing], quantity).await?;
            return Ok(json!(existing));
        }

        let mut values: Vec<(&str, Value)> = vec![
            ("product_id", json!(product)),
            ("user_id", json!(user)),
            ("date", json!(date)),
            ("quantity", json!(quantity)),
            ("topping_ids", json!([[6, 0, toppings]])),
        ];
        if let Some(note) = note {
            values.push(("note", json!(note)));
        }
        if let Some(location) = location {
            values.push(("lunch_location_id", json!(location)));
        }
        let id = ctx
            .registry
            .create_as(ctx.pool, ctx.uid, "lunch.order", values)
            .await?;
        Ok(json!(id))
    })
}

/// The `new` line this order would be a repeat of, if there is one.
///
/// Port of `_find_matching_lines`. The extras have to match exactly —
/// a pizza with olives is not the pizza without them.
#[allow(clippy::too_many_arguments)]
async fn matching_line(
    ctx: &MethodCtx<'_>,
    user: i64,
    product: i64,
    date: &str,
    note: Option<&str>,
    location: Option<i64>,
    toppings: &[i64],
) -> Result<Option<i64>, RusdooError> {
    let domain = json!([
        ["user_id", "=", user],
        ["product_id", "=", product],
        ["date", "=", date],
        ["state", "=", "new"],
        ["note", "=", note],
        ["lunch_location_id", "=", location],
    ]);
    let candidates = ctx
        .registry
        .search(
            ctx.pool,
            "lunch.order",
            &parse_domain(&domain)?,
            &SearchOptions::default(),
        )
        .await?;
    if candidates.is_empty() {
        return Ok(None);
    }
    let rows = ctx
        .registry
        .read(ctx.pool, "lunch.order", &candidates, &["topping_ids"])
        .await?;
    Ok(rows
        .iter()
        .find(|row| {
            let mut chosen = ids_of(row.get("topping_ids"));
            chosen.sort_unstable();
            chosen == toppings
        })
        .and_then(|row| row.get("id").and_then(Value::as_i64)))
}

/// `update_quantity(increment=...)` — one more, or one fewer.
///
/// Port of `update_quantity`: a line whose quantity would reach zero is
/// archived rather than deleted, because the wallet's history is made of
/// these rows and a deleted row is a statement that stops adding up.
pub fn update_quantity<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let increment = kwargs
            .get("increment")
            .or_else(|| ctx.rest.first())
            .and_then(Value::as_f64)
            .ok_or_else(|| {
                RusdooError::Validation("say by how much: pass increment".into())
            })?;
        let ids = ctx.ids.clone();
        bump(&ctx, &ids, increment).await?;
        Ok(json!(true))
    })
}

/// Move the quantity of every line that is still the office's to move.
async fn bump(ctx: &MethodCtx<'_>, ids: &[i64], increment: f64) -> Result<(), RusdooError> {
    if ids.is_empty() {
        return Ok(());
    }
    let rows = ctx
        .registry
        .read(ctx.pool, "lunch.order", ids, &["quantity", "state", "user_id"])
        .await?;
    for row in &rows {
        let Some(id) = row.get("id").and_then(Value::as_i64) else {
            continue;
        };
        let state = row.get("state").and_then(Value::as_str).unwrap_or("new");
        if LOCKED_STATES.contains(&state) {
            continue;
        }
        let quantity = row.get("quantity").and_then(Value::as_f64).unwrap_or(0.0);
        let values = if quantity + increment <= 0.0 {
            // Odoo leaves a `TODO: maybe unlink the order?` here; archiving
            // is what it does today and what the ledger needs
            vec![("active", json!(false))]
        } else {
            vec![("quantity", json!(quantity + increment))]
        };
        ctx.registry
            .write_as(ctx.pool, ctx.uid, "lunch.order", &[id], values)
            .await?;
    }
    check_wallets(ctx, &rows, Charge::Already).await
}

/// Whether the lines under examination have already been charged to the
/// wallet, or are about to be.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Charge {
    /// their state already counts against the balance
    Already,
    /// they are about to move into a state that counts
    Pending,
}

/// Port of `_check_wallet`: nobody eats into a wallet that is already
/// empty.
///
/// Odoo writes first and raises after, letting the transaction take the
/// write back. Here every ORM call commits on its own, so a charge that
/// has not landed yet has to be subtracted by hand and asked about
/// *before* the write — the alternative is an order that went through
/// and an error message saying it did not.
async fn check_wallets(
    ctx: &MethodCtx<'_>,
    rows: &[Map<String, Value>],
    charge: Charge,
) -> Result<(), RusdooError> {
    let mut seen: Vec<i64> = Vec::new();
    for row in rows {
        let Some(user) = row.get("user_id").and_then(first_id) else {
            continue;
        };
        if seen.contains(&user) {
            continue;
        }
        seen.push(user);
        let pending: f64 = match charge {
            Charge::Already => 0.0,
            Charge::Pending => rows
                .iter()
                .filter(|other| other.get("user_id").and_then(first_id) == Some(user))
                .filter(|other| {
                    // a line already in a charged state is already in the
                    // balance: counting it twice would refuse an order
                    // that is perfectly affordable
                    !money::SPENDING_STATES
                        .contains(&other.get("state").and_then(Value::as_str).unwrap_or("new"))
                })
                .filter_map(|other| other.get("price").and_then(Value::as_f64))
                .sum(),
        };
        if money::wallet_balance(ctx, user).await? - pending < 0.0 {
            return Err(RusdooError::Validation(
                "Oh no! You don't have enough money in your wallet to order your selected \
                 lunch! Contact your lunch manager to add some money to your wallet."
                    .into(),
            ));
        }
    }
    Ok(())
}

/// `action_order` — the office orders it.
///
/// Port of `action_order`: the vendor has to be open on the day of the
/// order, the product has to still exist, and the wallet has to survive
/// the charge.
pub fn action_order<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let rows = ctx
            .registry
            .read(
                ctx.pool,
                "lunch.order",
                &ctx.ids,
                &["date", "supplier_id", "product_id", "user_id", "price", "state"],
            )
            .await?;
        for row in &rows {
            let date = crate::schedule::as_date(row.get("date")).ok_or_else(|| {
                RusdooError::Validation("an order without a date cannot be placed".into())
            })?;
            let supplier = row.get("supplier_id").and_then(first_id).ok_or_else(|| {
                RusdooError::Validation("that order has no vendor".into())
            })?;
            let vendor = crate::catalog::read_supplier(&ctx, supplier).await?;
            if !crate::schedule::available_on_date(&vendor, date) {
                return Err(RusdooError::Validation(format!(
                    "{} does not deliver on {date}",
                    vendor.get("name").and_then(Value::as_str).unwrap_or("the vendor")
                )));
            }
            let product = row.get("product_id").and_then(first_id).ok_or_else(|| {
                RusdooError::Validation("that order has no product".into())
            })?;
            let products = ctx
                .registry
                .read(ctx.pool, "lunch.product", &[product], &["active"])
                .await?;
            if products
                .first()
                .and_then(|p| p.get("active"))
                .and_then(Value::as_bool)
                != Some(true)
            {
                return Err(RusdooError::Validation(
                    "that product is no longer available".into(),
                ));
            }
        }
        // the charge lands the moment the state moves, so the wallet is
        // asked about it first: see `check_wallets`
        check_wallets(&ctx, &rows, Charge::Pending).await?;
        write_state(&ctx, "ordered").await
    })
}

/// `action_send` — the vendor has now been told.
pub fn action_send<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move { write_state(&ctx, "sent").await })
}

/// `action_confirm` — the food arrived.
pub fn action_confirm<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move { write_state(&ctx, "confirmed").await })
}

/// `action_cancel` — nobody is cooking this.
pub fn action_cancel<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move { write_state(&ctx, "cancelled").await })
}

/// `action_reset` — back to ordered, for the line that was cancelled by
/// mistake (Odoo writes `ordered`, not `new`: the office had already
/// placed it).
pub fn action_reset<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move { write_state(&ctx, "ordered").await })
}

async fn write_state(ctx: &MethodCtx<'_>, state: &str) -> Result<Value, RusdooError> {
    if ctx.ids.is_empty() {
        return Err(RusdooError::Validation("choose at least one order".into()));
    }
    ctx.registry
        .write_as(
            ctx.pool,
            ctx.uid,
            "lunch.order",
            &ctx.ids,
            vec![("state", json!(state))],
        )
        .await?;
    Ok(json!(true))
}

/// `action_notify` — tell the people whose lunch has arrived.
///
/// Port of `action_notify`. Odoo sends each user a chatter notification
/// with the company's `lunch_notify_message`; here the message is posted
/// on the order itself, which is the record the notice is about and the
/// one the port's `mail.message` can point at. A user is told once, and
/// a line already notified is skipped — clicking the button twice must
/// not tell the office twice.
pub fn action_notify<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.registry.get("mail.message").is_none() {
            return Err(RusdooError::Validation(
                "lunch needs the mail module installed to notify anybody".into(),
            ));
        }
        let rows = ctx
            .registry
            .read(
                ctx.pool,
                "lunch.order",
                &ctx.ids,
                &["notified", "user_id", "company_id"],
            )
            .await?;
        let mut told: Vec<i64> = Vec::new();
        let mut notified: Vec<i64> = Vec::new();
        for row in &rows {
            let Some(id) = row.get("id").and_then(Value::as_i64) else {
                continue;
            };
            if row.get("notified").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            notified.push(id);
            let Some(user) = row.get("user_id").and_then(first_id) else {
                continue;
            };
            if told.contains(&user) {
                continue;
            }
            told.push(user);
            let body = notify_message(&ctx, row.get("company_id").and_then(first_id)).await?;
            ctx.registry
                .create_as(
                    ctx.pool,
                    ctx.uid,
                    "mail.message",
                    vec![
                        ("model", json!("lunch.order")),
                        ("res_id", json!(id)),
                        ("subject", json!("Lunch notification")),
                        ("body", json!(body)),
                        ("message_type", json!("notification")),
                        ("author_id", json!(ctx.uid)),
                    ],
                )
                .await?;
        }
        if !notified.is_empty() {
            ctx.registry
                .write_as(
                    ctx.pool,
                    ctx.uid,
                    "lunch.order",
                    &notified,
                    vec![("notified", json!(true))],
                )
                .await?;
        }
        Ok(json!(notified.len()))
    })
}

/// What the company tells people when their lunch is on the table.
async fn notify_message(
    ctx: &MethodCtx<'_>,
    company: Option<i64>,
) -> Result<String, RusdooError> {
    let fallback = "Your lunch has been delivered.\nEnjoy your meal!".to_string();
    let Some(company) = company else {
        return Ok(fallback);
    };
    let rows = ctx
        .registry
        .read(
            ctx.pool,
            "res.company",
            &[company],
            &["lunch_notify_message"],
        )
        .await?;
    Ok(rows
        .first()
        .and_then(|row| row.get("lunch_notify_message"))
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .map_or(fallback, str::to_string))
}

/// Today's orders of a set of vendors, in one state.
///
/// Port of `lunch_supplier.py::_get_current_orders`, which lives here
/// because it is a query about orders.
pub async fn current_orders(
    ctx: &MethodCtx<'_>,
    suppliers: &[i64],
    state: &str,
) -> Result<Vec<i64>, RusdooError> {
    if suppliers.is_empty() {
        return Ok(Vec::new());
    }
    let domain = json!([
        ["supplier_id", "in", suppliers],
        ["state", "=", state],
        ["date", "=", today()],
    ]);
    ctx.registry
        .search(
            ctx.pool,
            "lunch.order",
            &parse_domain(&domain)?,
            &SearchOptions {
                order: Some("user_id, product_id, id".into()),
                ..SearchOptions::default()
            },
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_price_is_the_product_plus_its_extras_times_the_quantity() {
        let mut record = Map::new();
        record.insert("quantity".into(), json!(2.0));
        record.insert("product_id.price".into(), json!([9.0]));
        record.insert("topping_ids.price".into(), json!([0.3, 0.7]));
        assert_eq!(total_price(&record), json!(20.0));
        // an order of nothing costs nothing, and does not read as null
        assert_eq!(total_price(&Map::new()), json!(0.0));
    }

    #[test]
    fn the_extras_are_written_out_for_the_kitchen() {
        let mut record = Map::new();
        record.insert("topping_ids.name".into(), json!(["Olives", "Extra cheese"]));
        assert_eq!(display_toppings(&record), json!("Olives + Extra cheese"));
        assert_eq!(display_toppings(&Map::new()), json!(""));
    }

    #[test]
    fn the_order_takes_the_category_and_the_vendor_of_its_product() {
        let mut record = Map::new();
        record.insert("product_id.category_id".into(), json!([[4, "Pizza"]]));
        record.insert("product_id.supplier_id".into(), json!([[7, "Pizza Inn"]]));
        assert_eq!(product_category(&record), json!(4));
        assert_eq!(product_supplier(&record), json!(7));
        assert_eq!(product_category(&Map::new()), json!(null));
    }

    #[test]
    fn a_group_with_nothing_in_it_asks_nothing_of_the_order() {
        // the vendor requires exactly one drink, but offers no drinks:
        // refusing here would be refusing every order forever
        assert!(check_topping_quantity("Drinks", "1", 0, false).is_ok());
    }

    #[test]
    fn only_one_means_exactly_one() {
        assert!(check_topping_quantity("Drinks", "1", 1, true).is_ok());
        let error = check_topping_quantity("Drinks", "1", 0, true).expect_err("none is not one");
        assert_eq!(error, "You have to order one and only one Drinks");
        let error = check_topping_quantity("Drinks", "1", 2, true).expect_err("two is not one");
        assert!(error.contains("one and only one"), "{error}");
    }

    #[test]
    fn one_or_more_means_at_least_one() {
        let error =
            check_topping_quantity("Extras", "1_more", 0, true).expect_err("none is too few");
        assert_eq!(error, "You should order at least one Extras");
        assert!(check_topping_quantity("Extras", "1_more", 1, true).is_ok());
        assert!(check_topping_quantity("Extras", "1_more", 5, true).is_ok());
    }

    #[test]
    fn none_or_more_asks_nothing() {
        for chosen in [0, 1, 9] {
            assert!(check_topping_quantity("Extras", "0_more", chosen, true).is_ok());
        }
    }
}
