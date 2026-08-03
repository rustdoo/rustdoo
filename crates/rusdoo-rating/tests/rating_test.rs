//! The rating as somebody lives it: a link goes out, an answer comes
//! back, the chatter shows it, and the averages move.
//!
//! The rated model is invented here on purpose. The whole point of
//! `rating.rating` is that it does not know what it points at, and a test
//! that rated a `sale.order` would be testing `sale` as much as this.

use rusdoo_core::RusdooError;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::{parse_domain, Domain};
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::methods::{MethodCtx, MethodRegistry};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use rusdoo_rating::RATING;
use std::sync::Arc;
use serde_json::{json, Map, Value};
use sqlx::PgPool;

/// The model under test: something a customer can be unhappy about.
const TICKET: &str = "rusdoo.test.ticket";
/// What the tickets hang from — the parent side of `rating.parent.mixin`.
const DESK: &str = "rusdoo.test.desk";

fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![],
        inherits: vec![],
    }
}

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

fn registry() -> Registry {
    let mut reg = rusdoo_base::registry().unwrap();
    rusdoo_mail::extend(&mut reg).unwrap();
    rusdoo_rating::extend(&mut reg).unwrap();
    reg.register(Model::new(
        meta(DESK, "rusdoo_test_desk"),
        vec![char("name")],
    ))
    .unwrap();
    reg.register(Model::new(
        meta(TICKET, "rusdoo_test_ticket"),
        vec![
            char("name"),
            // the two fields Odoo looks for on a rated record: who the
            // customer is, and who is answering them
            m2o("partner_id", "res.partner"),
            m2o("user_id", "res.users"),
            m2o("desk_id", DESK),
        ],
    ))
    .unwrap();
    reg
}

/// A database with the models of this addon and the two it rates.
async fn fixture(case: &str) -> Option<(Arc<Registry>, PgPool, MethodRegistry)> {
    let pool = rusdoo_testing::pool_in(case)?;
    let reg = registry();
    reg.init_tables(&pool).await.unwrap();
    // the superuser every call is made as; a create stamps `create_uid`,
    // and a row pointing at a user who is not there is a reference the
    // database refuses
    sqlx::query(
        r#"INSERT INTO "res_users" ("id", "login", "name", "active")
           VALUES (1, 'admin', 'Administrator', true)
           ON CONFLICT ("id") DO NOTHING"#,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        r#"SELECT setval('res_users_id_seq', GREATEST(1, (SELECT MAX("id") FROM "res_users")))"#,
    )
    .execute(&pool)
    .await
    .unwrap();

    let mut methods = MethodRegistry::new();
    rusdoo_rating::extend_methods(&mut methods).unwrap();
    rusdoo_rating::extend_rated_models(&mut methods, &[TICKET, DESK]).unwrap();
    Some((Arc::new(reg), pool, methods))
}

/// Call a registered method, as the dispatch would.
async fn call(
    reg: &Arc<Registry>,
    pool: &PgPool,
    methods: &MethodRegistry,
    model: &str,
    name: &str,
    ids: Vec<i64>,
    kwargs: Value,
) -> Result<Value, RusdooError> {
    let method = methods
        .get(model, name)
        .unwrap_or_else(|| panic!("{model}.{name} must be registered"));
    let kwargs = kwargs.as_object().cloned().unwrap_or_default();
    let ctx = MethodCtx::new(Arc::clone(reg), pool, 1, model, ids);
    method.call(ctx, &[], &kwargs).await
}

async fn a_ticket(reg: &Arc<Registry>, pool: &PgPool, name: &str, partner: Option<i64>) -> i64 {
    reg.create(
        pool,
        TICKET,
        vec![("name", json!(name)), ("partner_id", json!(partner))],
    )
    .await
    .unwrap()
}

async fn a_partner(reg: &Arc<Registry>, pool: &PgPool, name: &str) -> i64 {
    reg.create(pool, "res.partner", vec![("name", json!(name))])
        .await
        .unwrap()
}

/// One rating, read whole.
async fn rating_row(reg: &Arc<Registry>, pool: &PgPool, id: i64) -> Map<String, Value> {
    reg.read(
        pool,
        RATING,
        &[id],
        &[
            "res_model",
            "res_id",
            "res_name",
            "parent_res_model",
            "parent_res_id",
            "parent_res_name",
            "partner_id",
            "rated_partner_id",
            "rating",
            "rating_text",
            "rating_image_url",
            "feedback",
            "consumed",
            "rated_on",
            "access_token",
            "message_id",
        ],
    )
    .await
    .unwrap()
    .into_iter()
    .next()
    .expect("the rating exists")
}

/// The ratings of a record, whatever their state.
async fn ratings_of(reg: &Arc<Registry>, pool: &PgPool, model: &str, id: i64) -> Vec<i64> {
    reg.search(
        pool,
        RATING,
        &parse_domain(&json!([["res_model", "=", model], ["res_id", "=", id]])).unwrap(),
        &SearchOptions::default(),
    )
    .await
    .unwrap()
}

/// Ask for a token, answer with it, and hand back the rating's id.
async fn rate(
    reg: &Arc<Registry>,
    pool: &PgPool,
    methods: &MethodRegistry,
    ticket: i64,
    value: f64,
) -> i64 {
    let token = call(
        reg,
        pool,
        methods,
        TICKET,
        "rating_get_access_token",
        vec![ticket],
        json!({}),
    )
    .await
    .unwrap();
    call(
        reg,
        pool,
        methods,
        TICKET,
        "rating_apply",
        vec![ticket],
        json!({"rate": value, "token": token}),
    )
    .await
    .unwrap()
    .as_i64()
    .expect("rating_apply answers with the rating")
}

#[tokio::test]
async fn a_token_is_issued_once_and_answered_once_live() {
    let Some((reg, pool, methods)) = fixture("rusdoo_rating_token").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let customer = a_partner(&reg, &pool, "Ana").await;
    let ticket = a_ticket(&reg, &pool, "Printer on fire", Some(customer)).await;

    let token = call(
        &reg,
        &pool,
        &methods,
        TICKET,
        "rating_get_access_token",
        vec![ticket],
        json!({}),
    )
    .await
    .unwrap();
    let token = token.as_str().expect("a token is a string").to_string();
    assert_eq!(token.len(), 32, "uuid4 hex, like Odoo: {token}");

    // asking twice for the same record and the same customer does not
    // open a second survey
    let again = call(
        &reg,
        &pool,
        &methods,
        TICKET,
        "rating_get_access_token",
        vec![ticket],
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(again, json!(token));
    let pending = ratings_of(&reg, &pool, TICKET, ticket).await;
    assert_eq!(pending.len(), 1, "one request, one rating: {pending:?}");

    // the rating is born knowing what it is about and who was asked
    let row = rating_row(&reg, &pool, pending[0]).await;
    assert_eq!(row["res_model"], TICKET);
    assert_eq!(row["res_id"], json!(ticket));
    assert_eq!(row["res_name"], "Printer on fire");
    assert_eq!(row["partner_id"][0], json!(customer));
    assert_eq!(row["consumed"], json!(false));
    // and it is worth nothing until it is answered
    assert_eq!(row["rating"], json!(0.0));
    assert_eq!(row["rating_text"], "none");
    assert_eq!(row["rated_on"], Value::Null);

    let applied = call(
        &reg,
        &pool,
        &methods,
        TICKET,
        "rating_apply",
        vec![ticket],
        json!({"rate": 5, "token": token, "feedback": "solved in ten minutes"}),
    )
    .await
    .unwrap();
    assert_eq!(applied, json!(pending[0]));

    let row = rating_row(&reg, &pool, pending[0]).await;
    assert_eq!(row["rating"], json!(5.0));
    assert_eq!(
        row["rating_text"], "top",
        "the stored text follows the value"
    );
    assert_eq!(
        row["rating_image_url"],
        "/rating/static/src/img/rating_5.png"
    );
    assert_eq!(row["feedback"], "solved in ten minutes");
    assert_eq!(row["consumed"], json!(true));
    assert_ne!(row["rated_on"], Value::Null, "answering stamps the moment");

    // the answer is on the ticket's chatter, and the message carries it
    let message = row["message_id"][0].as_i64().expect("a message was posted");
    let posted = reg
        .read(
            &pool,
            "mail.message",
            &[message],
            &["model", "res_id", "body", "rating_value", "rating_ids"],
        )
        .await
        .unwrap();
    assert_eq!(posted[0]["model"], TICKET);
    assert_eq!(posted[0]["res_id"], json!(ticket));
    assert_eq!(
        posted[0]["body"],
        "Rating: 5/5 (Happy) — solved in ten minutes"
    );
    assert_eq!(posted[0]["rating_value"], json!(5.0));
    assert_eq!(posted[0]["rating_ids"], json!([pending[0]]));

    // a consumed rating is not handed out again: the next request is a
    // new survey, with a token of its own
    let next = call(
        &reg,
        &pool,
        &methods,
        TICKET,
        "rating_get_access_token",
        vec![ticket],
        json!({}),
    )
    .await
    .unwrap();
    assert_ne!(next, json!(token));
    assert_eq!(ratings_of(&reg, &pool, TICKET, ticket).await.len(), 2);
}

#[tokio::test]
async fn changing_the_answer_edits_the_message_instead_of_repeating_it_live() {
    let Some((reg, pool, methods)) = fixture("rusdoo_rating_second_thought").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let ticket = a_ticket(&reg, &pool, "Slow delivery", None).await;
    let token = call(
        &reg,
        &pool,
        &methods,
        TICKET,
        "rating_get_access_token",
        vec![ticket],
        json!({}),
    )
    .await
    .unwrap();
    call(
        &reg,
        &pool,
        &methods,
        TICKET,
        "rating_apply",
        vec![ticket],
        json!({"rate": 1, "token": token}),
    )
    .await
    .unwrap();
    // the same token again: the customer thought better of it
    let rating = call(
        &reg,
        &pool,
        &methods,
        TICKET,
        "rating_apply",
        vec![ticket],
        json!({"rate": 3, "token": token, "feedback": "it did arrive"}),
    )
    .await
    .unwrap()
    .as_i64()
    .unwrap();

    let row = rating_row(&reg, &pool, rating).await;
    assert_eq!(row["rating"], json!(3.0));
    assert_eq!(row["rating_text"], "ok");
    // one message, edited — not two, which would read as two customers
    let messages = reg
        .search(
            &pool,
            "mail.message",
            &parse_domain(&json!([["model", "=", TICKET], ["res_id", "=", ticket]])).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(messages.len(), 1, "{messages:?}");
    let body = reg
        .read(&pool, "mail.message", &messages, &["body"])
        .await
        .unwrap();
    assert_eq!(body[0]["body"], "Rating: 3/5 (Neutral) — it did arrive");
}

#[tokio::test]
async fn what_is_not_a_rating_is_refused_live() {
    let Some((reg, pool, methods)) = fixture("rusdoo_rating_refusals").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let ticket = a_ticket(&reg, &pool, "Broken chair", None).await;
    let other = a_ticket(&reg, &pool, "Another one", None).await;
    let token = call(
        &reg,
        &pool,
        &methods,
        TICKET,
        "rating_get_access_token",
        vec![ticket],
        json!({}),
    )
    .await
    .unwrap();

    // out of the scale
    let error = call(
        &reg,
        &pool,
        &methods,
        TICKET,
        "rating_apply",
        vec![ticket],
        json!({"rate": 7, "token": token}),
    )
    .await
    .expect_err("7 is not a rating");
    assert!(error.to_string().contains("between 0 and 5"), "{error}");

    // a token nobody issued says the same as one that expired
    let error = call(
        &reg,
        &pool,
        &methods,
        TICKET,
        "rating_apply",
        vec![ticket],
        json!({"rate": 5, "token": "not-a-token"}),
    )
    .await
    .expect_err("that token was never issued");
    assert!(error.to_string().contains("Invalid token"), "{error}");

    // and a real token applied to the wrong record does not move the
    // opinion from one ticket to the other
    let error = call(
        &reg,
        &pool,
        &methods,
        TICKET,
        "rating_apply",
        vec![other],
        json!({"rate": 5, "token": token}),
    )
    .await
    .expect_err("that rating is about another ticket");
    assert!(error.to_string().contains("another record"), "{error}");

    // nothing was written by any of the three
    let rating = ratings_of(&reg, &pool, TICKET, ticket).await[0];
    let row = rating_row(&reg, &pool, rating).await;
    assert_eq!(row["consumed"], json!(false));
    assert_eq!(row["rating"], json!(0.0));

    // and the database refuses the same value to a writer that never
    // came through the method
    let error = reg
        .create(
            &pool,
            RATING,
            vec![
                ("res_model", json!(TICKET)),
                ("res_id", json!(ticket)),
                ("rating", json!(9)),
            ],
        )
        .await
        .expect_err("the CHECK is the database's own");
    assert!(error.to_string().contains("between 0 and 5"), "{error}");
}

#[tokio::test]
async fn resetting_makes_the_rating_a_question_again_live() {
    let Some((reg, pool, methods)) = fixture("rusdoo_rating_reset").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let ticket = a_ticket(&reg, &pool, "Wrong colour", None).await;
    let rating = rate(&reg, &pool, &methods, ticket, 1.0).await;
    let before = rating_row(&reg, &pool, rating).await;

    call(
        &reg,
        &pool,
        &methods,
        RATING,
        "reset",
        vec![rating],
        json!({}),
    )
    .await
    .unwrap();

    let after = rating_row(&reg, &pool, rating).await;
    assert_eq!(after["rating"], json!(0.0));
    assert_eq!(after["rating_text"], "none");
    assert_eq!(after["feedback"], Value::Null);
    assert_eq!(after["consumed"], json!(false));
    // a new token: the old link is in somebody's mailbox, and it must
    // stop working
    assert_ne!(after["access_token"], before["access_token"]);
    // and the reopened survey is the one the next request hands out
    let token = call(
        &reg,
        &pool,
        &methods,
        TICKET,
        "rating_get_access_token",
        vec![ticket],
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(token, after["access_token"]);
}

#[tokio::test]
async fn the_statistics_count_only_what_was_answered_live() {
    let Some((reg, pool, methods)) = fixture("rusdoo_rating_stats").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let tickets: Vec<i64> = {
        let mut ids = Vec::new();
        for name in ["A", "B", "C", "D", "E"] {
            ids.push(a_ticket(&reg, &pool, name, None).await);
        }
        ids
    };
    for (ticket, value) in tickets.iter().zip([5.0, 5.0, 3.0, 1.0]) {
        rate(&reg, &pool, &methods, *ticket, value).await;
    }
    // the fifth was asked and never answered
    call(
        &reg,
        &pool,
        &methods,
        TICKET,
        "rating_get_access_token",
        vec![tickets[4]],
        json!({}),
    )
    .await
    .unwrap();

    let stats = call(
        &reg,
        &pool,
        &methods,
        TICKET,
        "rating_get_stats",
        tickets.clone(),
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(stats["total"], json!(4), "the silent one is not a zero");
    assert_eq!(stats["avg"], json!(3.5));
    assert_eq!(stats["avg_text"], "ok");
    assert_eq!(stats["percent"]["5"], json!(50.0));
    assert_eq!(stats["percent"]["3"], json!(25.0));
    assert_eq!(stats["percent"]["2"], json!(0.0), "the empty bar is drawn");
    assert_eq!(stats["percentage_satisfaction"], json!(50.0));

    let grades = call(
        &reg,
        &pool,
        &methods,
        TICKET,
        "rating_get_grades",
        tickets.clone(),
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(grades, json!({"great": 2, "okay": 1, "bad": 1}));

    // a record nobody rated has nothing to show, and says so with -1
    let empty = call(
        &reg,
        &pool,
        &methods,
        TICKET,
        "rating_get_stats",
        vec![tickets[4]],
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(empty["total"], json!(0));
    assert_eq!(empty["percentage_satisfaction"], json!(-1.0));

    // `_rating_satisfaction_days`: a year-old complaint is not what a
    // team's satisfaction is measured on today. `write_date` is the
    // framework's own column, so the test moves it the only way anybody
    // could — behind the ORM's back.
    let old = ratings_of(&reg, &pool, TICKET, tickets[3]).await[0];
    sqlx::query(
        r#"UPDATE "rating_rating" SET "write_date" = now() - interval '100 days' WHERE "id" = $1"#,
    )
    .bind(old as i32)
    .execute(&pool)
    .await
    .unwrap();
    let recent = call(
        &reg,
        &pool,
        &methods,
        TICKET,
        "rating_get_stats",
        tickets.clone(),
        json!({"days": 30}),
    )
    .await
    .unwrap();
    assert_eq!(recent["total"], json!(3), "the old unhappy one drops out");
    assert_eq!(recent["percentage_satisfaction"], json!(200.0 / 3.0));
}

#[tokio::test]
async fn a_holder_counts_the_ratings_of_what_hangs_from_it_live() {
    let Some((reg, pool, methods)) = fixture("rusdoo_rating_parent").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let desk = reg
        .create(&pool, DESK, vec![("name", json!("Support"))])
        .await
        .unwrap();
    let other_desk = reg
        .create(&pool, DESK, vec![("name", json!("Sales"))])
        .await
        .unwrap();
    let happy = a_ticket(&reg, &pool, "Happy one", None).await;
    let sad = a_ticket(&reg, &pool, "Sad one", None).await;
    let elsewhere = a_ticket(&reg, &pool, "Not ours", None).await;

    for (ticket, holder, value) in [
        (happy, desk, 5.0),
        (sad, desk, 1.0),
        (elsewhere, other_desk, 5.0),
    ] {
        let token = call(
            &reg,
            &pool,
            &methods,
            TICKET,
            "rating_get_access_token",
            vec![ticket],
            json!({"parent_res_model": DESK, "parent_res_id": holder}),
        )
        .await
        .unwrap();
        call(
            &reg,
            &pool,
            &methods,
            TICKET,
            "rating_apply",
            vec![ticket],
            json!({"rate": value, "token": token}),
        )
        .await
        .unwrap();
    }

    // the rating knows both ends, and the holder's name with it
    let rating = ratings_of(&reg, &pool, TICKET, happy).await[0];
    let row = rating_row(&reg, &pool, rating).await;
    assert_eq!(row["parent_res_model"], DESK);
    assert_eq!(row["parent_res_id"], json!(desk));
    assert_eq!(row["parent_res_name"], "Support");

    let stats = call(
        &reg,
        &pool,
        &methods,
        DESK,
        "rating_get_stats",
        vec![desk],
        json!({"parent": true}),
    )
    .await
    .unwrap();
    assert_eq!(stats["total"], json!(2), "the other desk's is not counted");
    assert_eq!(stats["avg"], json!(3.0));
    assert_eq!(stats["percentage_satisfaction"], json!(50.0));

    // and counted the ordinary way the desk has no rating of its own
    let direct = call(
        &reg,
        &pool,
        &methods,
        DESK,
        "rating_get_stats",
        vec![desk],
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(direct["total"], json!(0));
}

#[tokio::test]
async fn a_rating_points_back_at_what_it_is_about_live() {
    let Some((reg, pool, methods)) = fixture("rusdoo_rating_open").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let ticket = a_ticket(&reg, &pool, "Open me", None).await;
    let rating = rate(&reg, &pool, &methods, ticket, 5.0).await;

    let action = call(
        &reg,
        &pool,
        &methods,
        RATING,
        "action_open_rated_object",
        vec![rating],
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(action["type"], "ir.actions.act_window");
    assert_eq!(action["res_model"], TICKET);
    assert_eq!(action["res_id"], json!(ticket));
}

#[tokio::test]
async fn deleting_the_message_takes_its_rating_with_it_live() {
    let Some((reg, pool, methods)) = fixture("rusdoo_rating_cascade").await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let ticket = a_ticket(&reg, &pool, "Say it once", None).await;
    let rating = rate(&reg, &pool, &methods, ticket, 5.0).await;
    let message = rating_row(&reg, &pool, rating).await["message_id"][0]
        .as_i64()
        .expect("the answer was posted");

    reg.unlink_as(&pool, 1, "mail.message", &[message])
        .await
        .unwrap();

    // a rating whose message is gone is a number nobody could place, and
    // the database is what says so — not this crate
    let left = reg
        .search(&pool, RATING, &Domain::True, &SearchOptions::default())
        .await
        .unwrap();
    assert!(left.is_empty(), "the cascade took the rating: {left:?}");
}
