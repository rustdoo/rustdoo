//! Lunch as somebody actually uses it: putting a product in the cart,
//! ordering it, paying for it, and being told it arrived.
//!
//! Every case builds its own schema, so the suite is safe to run in
//! parallel and safe to run twice at once.

use rusdoo_core::RusdooError;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::methods::{MethodCtx, MethodRegistry};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use std::sync::Arc;

/// A registry with base, mail and lunch, over a schema of this case's
/// own. `None` when there is no test database configured — the caller
/// returns and the runner shows the test as passed-because-skipped.
async fn fixture(case: &str) -> Option<(Arc<Registry>, PgPool)> {
    let Some(pool) = rusdoo_testing::pool_in(case) else {
        eprintln!("skipped: {} not set", rusdoo_testing::DATABASE_ENV);
        return None;
    };
    let mut registry = rusdoo_base::registry().expect("the base models");
    rusdoo_mail::extend(&mut registry).expect("the mail models");
    rusdoo_lunch::extend(&mut registry).expect("the lunch models");
    registry
        .init_tables(&pool)
        .await
        .expect("creating the models' tables");
    Some((Arc::new(registry), pool))
}

/// Call a registered method the way the dispatch does.
async fn call(
    registry: &Arc<Registry>,
    pool: &PgPool,
    uid: i64,
    model: &str,
    name: &str,
    ids: &[i64],
    kwargs: Value,
) -> Result<Value, RusdooError> {
    let mut methods = MethodRegistry::new();
    rusdoo_lunch::extend_methods(&mut methods).expect("the lunch methods");
    let method = methods.get(model, name).expect("a registered method");
    let kwargs: Map<String, Value> = match kwargs {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    // the registry travels by handle now
    let ctx = MethodCtx::new(Arc::clone(registry), pool, uid, model, ids.to_vec());
    method.call(ctx, &[], &kwargs).await
}

/// Everything a lunch needs: a company, a hungry person, a vendor open
/// every day, a category and a product.
struct Office {
    company: i64,
    user: i64,
    supplier: i64,
    category: i64,
    product: i64,
    location: i64,
}

async fn an_office(registry: &Registry, pool: &PgPool) -> Office {
    let company = registry
        .create(pool, "res.company", vec![("name", json!("Acme"))])
        .await
        .unwrap();
    let user = registry
        .create(
            pool,
            "res.users",
            vec![
                ("login", json!("ana")),
                ("name", json!("Ana")),
                ("company_id", json!(company)),
            ],
        )
        .await
        .unwrap();
    let location = registry
        .create(
            pool,
            "lunch.location",
            vec![("name", json!("HQ Office")), ("company_id", json!(company))],
        )
        .await
        .unwrap();
    let supplier = an_open_vendor(registry, pool, company, "Pizza Inn").await;
    let category = registry
        .create(
            pool,
            "lunch.product.category",
            vec![("name", json!("Pizza"))],
        )
        .await
        .unwrap();
    let product = registry
        .create(
            pool,
            "lunch.product",
            vec![
                ("name", json!("Pizza")),
                ("category_id", json!(category)),
                ("supplier_id", json!(supplier)),
                ("price", json!(9.0)),
            ],
        )
        .await
        .unwrap();
    Office {
        company,
        user,
        supplier,
        category,
        product,
        location,
    }
}

/// A vendor that delivers every day of the week, so a test does not
/// depend on what day it is run.
async fn an_open_vendor(registry: &Registry, pool: &PgPool, company: i64, name: &str) -> i64 {
    let partner = registry
        .create(pool, "res.partner", vec![("name", json!(name))])
        .await
        .unwrap();
    let mut values = vec![
        ("partner_id", json!(partner)),
        ("company_id", json!(company)),
        ("send_by", json!("phone")),
    ];
    for day in rusdoo_lunch::schedule::WEEKDAY_TO_NAME {
        values.push((day, json!(true)));
    }
    registry
        .create(pool, "lunch.supplier", values)
        .await
        .unwrap()
}

async fn read_one(registry: &Registry, pool: &PgPool, model: &str, id: i64, fields: &[&str]) -> Map<String, Value> {
    registry
        .read(pool, model, &[id], fields)
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("the record exists")
}

async fn pay_in(registry: &Registry, pool: &PgPool, user: i64, amount: f64) {
    registry
        .create(
            pool,
            "lunch.cashmove",
            vec![
                ("user_id", json!(user)),
                ("amount", json!(amount)),
                ("name", json!("Payment")),
            ],
        )
        .await
        .unwrap();
}

async fn wallet(registry: &Arc<Registry>, pool: &PgPool, user: i64) -> f64 {
    call(
        registry,
        pool,
        user,
        "lunch.cashmove",
        "get_wallet_balance",
        &[],
        json!({}),
    )
    .await
    .unwrap()
    .as_f64()
    .expect("a balance")
}

async fn orders_of(registry: &Registry, pool: &PgPool, user: i64) -> Vec<i64> {
    registry
        .search(
            pool,
            "lunch.order",
            &parse_domain(&json!([["user_id", "=", user]])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn ordering_the_same_lunch_twice_is_a_second_helping_live() {
    let Some((registry, pool)) = fixture("rusdoo_lunch_cart").await else {
        return;
    };
    let office = an_office(&registry, &pool).await;

    let first = call(
        &registry,
        &pool,
        office.user,
        "lunch.order",
        "add_to_cart",
        &[],
        json!({"product_id": office.product, "lunch_location_id": office.location}),
    )
    .await
    .expect("Ana orders a pizza")
    .as_i64()
    .unwrap();

    // the same pizza, the same day, the same delivery point: one line
    let again = call(
        &registry,
        &pool,
        office.user,
        "lunch.order",
        "add_to_cart",
        &[],
        json!({"product_id": office.product, "lunch_location_id": office.location}),
    )
    .await
    .expect("Ana is hungry")
    .as_i64()
    .unwrap();
    assert_eq!(again, first, "a second helping is not a second line");
    assert_eq!(orders_of(&registry, &pool, office.user).await.len(), 1);

    let row = read_one(&registry, &pool, "lunch.order", first, &["quantity", "price"]).await;
    assert_eq!(row["quantity"], json!(2.0));
    assert_eq!(row["price"], json!(18.0), "the price followed the quantity");
}

#[tokio::test]
async fn a_line_the_office_already_ordered_does_not_take_the_second_helping_live() {
    let Some((registry, pool)) = fixture("rusdoo_lunch_ordered_line").await else {
        return;
    };
    let office = an_office(&registry, &pool).await;
    pay_in(&registry, &pool, office.user, 100.0).await;

    let placed = call(
        &registry,
        &pool,
        office.user,
        "lunch.order",
        "add_to_cart",
        &[],
        json!({"product_id": office.product}),
    )
    .await
    .unwrap()
    .as_i64()
    .unwrap();
    call(
        &registry,
        &pool,
        office.user,
        "lunch.order",
        "action_order",
        &[placed],
        json!({}),
    )
    .await
    .expect("the office places it");

    // ordering the same thing again is a new line: the vendor has
    // already been told about the first one
    let second = call(
        &registry,
        &pool,
        office.user,
        "lunch.order",
        "add_to_cart",
        &[],
        json!({"product_id": office.product}),
    )
    .await
    .unwrap()
    .as_i64()
    .unwrap();
    assert_ne!(second, placed);
    assert_eq!(
        read_one(&registry, &pool, "lunch.order", placed, &["quantity"]).await["quantity"],
        json!(1.0),
        "the placed order was left alone"
    );
    assert_eq!(orders_of(&registry, &pool, office.user).await.len(), 2);
}

#[tokio::test]
async fn an_order_is_priced_from_its_product_and_its_extras_live() {
    let Some((registry, pool)) = fixture("rusdoo_lunch_price").await else {
        return;
    };
    let office = an_office(&registry, &pool).await;
    let olives = registry
        .create(
            &pool,
            "lunch.topping",
            vec![
                ("name", json!("Olives")),
                ("price", json!(0.3)),
                ("supplier_id", json!(office.supplier)),
                ("topping_category", json!(1)),
            ],
        )
        .await
        .unwrap();
    let cheese = registry
        .create(
            &pool,
            "lunch.topping",
            vec![
                ("name", json!("Extra cheese")),
                ("price", json!(0.7)),
                ("supplier_id", json!(office.supplier)),
                ("topping_category", json!(1)),
            ],
        )
        .await
        .unwrap();

    let order = call(
        &registry,
        &pool,
        office.user,
        "lunch.order",
        "add_to_cart",
        &[],
        json!({
            "product_id": office.product,
            "quantity": 2,
            "topping_ids": [olives, cheese],
        }),
    )
    .await
    .unwrap()
    .as_i64()
    .unwrap();

    let row = read_one(
        &registry,
        &pool,
        "lunch.order",
        order,
        &["price", "display_toppings", "supplier_id", "category_id", "name"],
    )
    .await;
    // 2 x (9.00 + 0.30 + 0.70)
    assert_eq!(row["price"], json!(20.0));
    assert_eq!(row["display_toppings"], json!("Olives + Extra cheese"));
    // the vendor and the category came from the product, into columns of
    // their own, so the manager's screens can group by them
    assert_eq!(row["supplier_id"][0], json!(office.supplier));
    assert_eq!(row["category_id"][0], json!(office.category));
    assert_eq!(row["name"], json!("Pizza"), "the product's name, mirrored");
}

#[tokio::test]
async fn an_extra_from_another_vendor_is_refused_live() {
    let Some((registry, pool)) = fixture("rusdoo_lunch_stray_extra").await else {
        return;
    };
    let office = an_office(&registry, &pool).await;
    let other = an_open_vendor(&registry, &pool, office.company, "Kothai").await;
    let wasabi = registry
        .create(
            &pool,
            "lunch.topping",
            vec![
                ("name", json!("Wasabi")),
                ("price", json!(0.5)),
                ("supplier_id", json!(other)),
                ("topping_category", json!(1)),
            ],
        )
        .await
        .unwrap();

    let error = call(
        &registry,
        &pool,
        office.user,
        "lunch.order",
        "add_to_cart",
        &[],
        json!({"product_id": office.product, "topping_ids": [wasabi]}),
    )
    .await
    .expect_err("Pizza Inn does not keep wasabi");
    assert!(error.to_string().contains("not offered by"), "{error}");
    assert!(orders_of(&registry, &pool, office.user).await.is_empty());
}

#[tokio::test]
async fn a_vendor_that_requires_exactly_one_drink_gets_exactly_one_live() {
    let Some((registry, pool)) = fixture("rusdoo_lunch_extras_rule").await else {
        return;
    };
    let office = an_office(&registry, &pool).await;
    registry
        .write(
            &pool,
            "lunch.supplier",
            &[office.supplier],
            vec![
                ("topping_quantity_2", json!("1")),
                ("topping_label_2", json!("Drinks")),
            ],
        )
        .await
        .unwrap();
    let mut drinks = Vec::new();
    for name in ["Coke", "Water"] {
        drinks.push(
            registry
                .create(
                    &pool,
                    "lunch.topping",
                    vec![
                        ("name", json!(name)),
                        ("price", json!(1.0)),
                        ("supplier_id", json!(office.supplier)),
                        ("topping_category", json!(2)),
                    ],
                )
                .await
                .unwrap(),
        );
    }

    let order_with = |toppings: Vec<i64>| {
        call(
            &registry,
            &pool,
            office.user,
            "lunch.order",
            "add_to_cart",
            &[],
            json!({"product_id": office.product, "topping_ids": toppings}),
        )
    };

    let error = order_with(vec![]).await.expect_err("no drink is not one drink");
    assert!(
        error.to_string().contains("one and only one Drinks"),
        "{error}"
    );
    let error = order_with(drinks.clone())
        .await
        .expect_err("two drinks is not one drink");
    assert!(error.to_string().contains("one and only one Drinks"), "{error}");
    order_with(vec![drinks[0]])
        .await
        .expect("one drink is what the vendor asked for");
}

#[tokio::test]
async fn a_wallet_is_what_was_paid_in_less_what_was_ordered_live() {
    let Some((registry, pool)) = fixture("rusdoo_lunch_wallet").await else {
        return;
    };
    let office = an_office(&registry, &pool).await;
    pay_in(&registry, &pool, office.user, 100.0).await;
    assert_eq!(wallet(&registry, &pool, office.user).await, 100.0);

    let order = call(
        &registry,
        &pool,
        office.user,
        "lunch.order",
        "add_to_cart",
        &[],
        json!({"product_id": office.product}),
    )
    .await
    .unwrap()
    .as_i64()
    .unwrap();
    // a line still in the cart has not been paid for
    assert_eq!(
        wallet(&registry, &pool, office.user).await,
        100.0,
        "a cart is not a debt"
    );

    call(
        &registry,
        &pool,
        office.user,
        "lunch.order",
        "action_order",
        &[order],
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(wallet(&registry, &pool, office.user).await, 91.0);

    // and a cancelled order gives the money back
    call(
        &registry,
        &pool,
        office.user,
        "lunch.order",
        "action_cancel",
        &[order],
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(wallet(&registry, &pool, office.user).await, 100.0);
}

#[tokio::test]
async fn an_empty_wallet_refuses_the_order_and_leaves_it_in_the_cart_live() {
    let Some((registry, pool)) = fixture("rusdoo_lunch_broke").await else {
        return;
    };
    let office = an_office(&registry, &pool).await;
    let order = call(
        &registry,
        &pool,
        office.user,
        "lunch.order",
        "add_to_cart",
        &[],
        json!({"product_id": office.product}),
    )
    .await
    .unwrap()
    .as_i64()
    .unwrap();

    let error = call(
        &registry,
        &pool,
        office.user,
        "lunch.order",
        "action_order",
        &[order],
        json!({}),
    )
    .await
    .expect_err("nobody paid anything in");
    assert!(error.to_string().contains("enough money"), "{error}");
    // the refusal left the line where it was: it was never placed
    assert_eq!(
        read_one(&registry, &pool, "lunch.order", order, &["state"]).await["state"],
        json!("new")
    );

    // the company's allowance is what lets somebody order while they owe
    registry
        .write(
            &pool,
            "res.company",
            &[office.company],
            vec![("lunch_minimum_threshold", json!(20.0))],
        )
        .await
        .unwrap();
    call(
        &registry,
        &pool,
        office.user,
        "lunch.order",
        "action_order",
        &[order],
        json!({}),
    )
    .await
    .expect("the allowance covers it");
    assert_eq!(wallet(&registry, &pool, office.user).await, 11.0);
}

#[tokio::test]
async fn a_vendor_closed_that_day_refuses_the_order_live() {
    let Some((registry, pool)) = fixture("rusdoo_lunch_closed").await else {
        return;
    };
    let office = an_office(&registry, &pool).await;
    pay_in(&registry, &pool, office.user, 100.0).await;
    let order = call(
        &registry,
        &pool,
        office.user,
        "lunch.order",
        "add_to_cart",
        &[],
        json!({"product_id": office.product}),
    )
    .await
    .unwrap()
    .as_i64()
    .unwrap();

    // the kitchen closes for good today
    let mut shut: Vec<(&str, Value)> = Vec::new();
    for day in rusdoo_lunch::schedule::WEEKDAY_TO_NAME {
        shut.push((day, json!(false)));
    }
    registry
        .write(&pool, "lunch.supplier", &[office.supplier], shut)
        .await
        .unwrap();
    assert_eq!(
        read_one(
            &registry,
            &pool,
            "lunch.supplier",
            office.supplier,
            &["available_today"]
        )
        .await["available_today"],
        json!(false)
    );

    let error = call(
        &registry,
        &pool,
        office.user,
        "lunch.order",
        "action_order",
        &[order],
        json!({}),
    )
    .await
    .expect_err("nobody is cooking");
    assert!(error.to_string().contains("does not deliver on"), "{error}");
}

#[tokio::test]
async fn the_days_orders_go_out_to_the_vendor_and_come_back_live() {
    let Some((registry, pool)) = fixture("rusdoo_lunch_round_trip").await else {
        return;
    };
    let office = an_office(&registry, &pool).await;
    pay_in(&registry, &pool, office.user, 100.0).await;
    let order = call(
        &registry,
        &pool,
        office.user,
        "lunch.order",
        "add_to_cart",
        &[],
        json!({"product_id": office.product}),
    )
    .await
    .unwrap()
    .as_i64()
    .unwrap();
    call(
        &registry,
        &pool,
        office.user,
        "lunch.order",
        "action_order",
        &[order],
        json!({}),
    )
    .await
    .unwrap();

    // the closure borrows handles of its own: an `async move` that ate
    // the registry left the rest of the test with nothing to read from
    let state = {
        let registry = Arc::clone(&registry);
        let pool = pool.clone();
        move |id: i64| {
            let registry = Arc::clone(&registry);
            let pool = pool.clone();
            async move {
                read_one(&registry, &pool, "lunch.order", id, &["state"]).await["state"].clone()
            }
        }
    };

    let answer = call(
        &registry,
        &pool,
        1,
        "lunch.supplier",
        "action_send_orders",
        &[office.supplier],
        json!({}),
    )
    .await
    .expect("the office tells the vendor");
    assert_eq!(answer["tag"], json!("display_notification"));
    assert_eq!(state(order).await, json!("sent"));

    call(
        &registry,
        &pool,
        1,
        "lunch.supplier",
        "action_confirm_orders",
        &[office.supplier],
        json!({}),
    )
    .await
    .expect("the food arrived");
    assert_eq!(state(order).await, json!("confirmed"));
}

#[tokio::test]
async fn people_are_told_their_lunch_arrived_exactly_once_live() {
    let Some((registry, pool)) = fixture("rusdoo_lunch_notify").await else {
        return;
    };
    let office = an_office(&registry, &pool).await;
    registry
        .write(
            &pool,
            "res.company",
            &[office.company],
            vec![("lunch_notify_message", json!("Your pizza is at the desk"))],
        )
        .await
        .unwrap();
    pay_in(&registry, &pool, office.user, 100.0).await;
    let order = call(
        &registry,
        &pool,
        office.user,
        "lunch.order",
        "add_to_cart",
        &[],
        json!({"product_id": office.product}),
    )
    .await
    .unwrap()
    .as_i64()
    .unwrap();

    let told = call(
        &registry,
        &pool,
        1,
        "lunch.order",
        "action_notify",
        &[order],
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(told, json!(1));

    let messages = registry
        .search(
            &pool,
            "mail.message",
            &parse_domain(&json!([["model", "=", "lunch.order"]])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(messages.len(), 1);
    let message = read_one(&registry, &pool, "mail.message", messages[0], &["body", "res_id"]).await;
    assert_eq!(message["body"], json!("Your pizza is at the desk"));
    assert_eq!(message["res_id"], json!(order));

    // pressing the button again tells nobody: the line is already
    // notified
    let told = call(
        &registry,
        &pool,
        1,
        "lunch.order",
        "action_notify",
        &[order],
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(told, json!(0));
    let messages = registry
        .search(
            &pool,
            "mail.message",
            &parse_domain(&json!([["model", "=", "lunch.order"]])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(messages.len(), 1, "the office is not told twice");
}

#[tokio::test]
async fn taking_the_last_helping_back_archives_the_line_live() {
    let Some((registry, pool)) = fixture("rusdoo_lunch_take_back").await else {
        return;
    };
    let office = an_office(&registry, &pool).await;
    let order = call(
        &registry,
        &pool,
        office.user,
        "lunch.order",
        "add_to_cart",
        &[],
        json!({"product_id": office.product}),
    )
    .await
    .unwrap()
    .as_i64()
    .unwrap();

    call(
        &registry,
        &pool,
        office.user,
        "lunch.order",
        "update_quantity",
        &[order],
        json!({"increment": 1}),
    )
    .await
    .unwrap();
    assert_eq!(
        read_one(&registry, &pool, "lunch.order", order, &["quantity"]).await["quantity"],
        json!(2.0)
    );

    for _ in 0..2 {
        call(
            &registry,
            &pool,
            office.user,
            "lunch.order",
            "update_quantity",
            &[order],
            json!({"increment": -1}),
        )
        .await
        .unwrap();
    }
    // the row is still there — the ledger is made of these — but it is
    // out of every search
    let row = read_one(&registry, &pool, "lunch.order", order, &["active"]).await;
    assert_eq!(row["active"], json!(false));
    assert!(
        orders_of(&registry, &pool, office.user).await.is_empty(),
        "an archived line is out of the cart"
    );
}

#[tokio::test]
async fn a_favourite_is_a_link_the_person_who_pressed_the_star_owns_live() {
    let Some((registry, pool)) = fixture("rusdoo_lunch_favourite").await else {
        return;
    };
    let office = an_office(&registry, &pool).await;
    let beto = registry
        .create(
            &pool,
            "res.users",
            vec![("login", json!("beto")), ("name", json!("Beto"))],
        )
        .await
        .unwrap();

    let starred = call(
        &registry,
        &pool,
        office.user,
        "lunch.product",
        "action_toggle_favorite",
        &[office.product],
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(starred, json!(true));
    call(
        &registry,
        &pool,
        beto,
        "lunch.product",
        "action_toggle_favorite",
        &[office.product],
        json!({}),
    )
    .await
    .unwrap();

    // the user side of the same relation table sees it too
    let ana_likes = read_one(
        &registry,
        &pool,
        "res.users",
        office.user,
        &["favorite_lunch_product_ids"],
    )
    .await;
    assert_eq!(ana_likes["favorite_lunch_product_ids"], json!([office.product]));

    // pressing it again unstars it — and only for whoever pressed
    let starred = call(
        &registry,
        &pool,
        office.user,
        "lunch.product",
        "action_toggle_favorite",
        &[office.product],
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(starred, json!(false));
    let row = read_one(
        &registry,
        &pool,
        "lunch.product",
        office.product,
        &["favorite_user_ids"],
    )
    .await;
    assert_eq!(row["favorite_user_ids"], json!([beto]));
}

#[tokio::test]
async fn an_extras_group_outside_the_vendors_three_is_refused_live() {
    let Some((registry, pool)) = fixture("rusdoo_lunch_group").await else {
        return;
    };
    let office = an_office(&registry, &pool).await;
    let error = registry
        .create(
            &pool,
            "lunch.topping",
            vec![
                ("name", json!("Ketchup")),
                ("price", json!(0.1)),
                ("supplier_id", json!(office.supplier)),
                ("topping_category", json!(4)),
            ],
        )
        .await
        .expect_err("there is no fourth group");
    assert!(error.to_string().contains("groups 1, 2 and 3"), "{error}");

    // and nothing was left behind by the refused create
    let toppings = registry
        .search(
            &pool,
            "lunch.topping",
            &parse_domain(&json!([])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    assert!(toppings.is_empty(), "a refused record must not survive");
}

#[tokio::test]
async fn a_category_knows_how_many_products_hang_off_it_live() {
    let Some((registry, pool)) = fixture("rusdoo_lunch_category").await else {
        return;
    };
    let office = an_office(&registry, &pool).await;
    assert_eq!(
        read_one(
            &registry,
            &pool,
            "lunch.product.category",
            office.category,
            &["product_count"]
        )
        .await["product_count"],
        json!(1)
    );

    registry
        .create(
            &pool,
            "lunch.product",
            vec![
                ("name", json!("Pizza Funghi")),
                ("category_id", json!(office.category)),
                ("supplier_id", json!(office.supplier)),
                ("price", json!(11.0)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        read_one(
            &registry,
            &pool,
            "lunch.product.category",
            office.category,
            &["product_count"]
        )
        .await["product_count"],
        json!(2)
    );
}
