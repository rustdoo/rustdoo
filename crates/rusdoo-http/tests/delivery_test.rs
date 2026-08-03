//! Delivering a sales order: the picking a confirmed order produces,
//! and what validating it records.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rusdoo_http::dispatch::OrmService;
use rusdoo_http::routes::router;
use rusdoo_orm::methods::MethodRegistry;
use rusdoo_orm::registry::Registry;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

fn pool(url: &str, schema: &'static str) -> sqlx::PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |conn, _meta| {
            Box::pin(async move {
                sqlx::Executor::execute(
                    &mut *conn,
                    &format!("CREATE SCHEMA IF NOT EXISTS {schema}; SET search_path TO {schema}")
                        as &str,
                )
                .await?;
                Ok(())
            })
        })
        .connect_lazy(url)
        .unwrap()
}

fn registry() -> Registry {
    let mut registry = rusdoo_base::registry().unwrap();
    rusdoo_product::extend(&mut registry).unwrap();
    rusdoo_account::extend(&mut registry).unwrap();
    rusdoo_stock::extend(&mut registry).unwrap();
    rusdoo_sale::extend(&mut registry).unwrap();
    registry
}

async fn fixture(url: &str, schema: &'static str) -> OrmService {
    let pool = pool(url, schema);
    let registry = registry();
    for table in [
        "sale_order_line",
        "sale_order",
        "stock_move",
        "stock_picking",
        "stock_location",
        "account_move_line",
        "account_move",
        "product_product",
        "res_partner",
        "res_company",
        "res_users",
        "res_groups",
        "res_country",
        "ir_sequence",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}" CASCADE"#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for model in [
        // every registered model gets its table, like the real boot —
        // a document numbered by a sequence needs the sequence table
        "ir.sequence",
        "res.country",
        "res.company",
        "res.partner",
        // o default de empresa lê o usuário que está criando
        "res.groups",
        "res.users",
        "product.product",
        "account.move",
        "account.move.line",
        "stock.location",
        "stock.picking",
        "stock.move",
        "sale.order",
        "sale.order.line",
    ] {
        registry.get(model).unwrap().init_table(&pool).await.unwrap();
    }
    // the sequences these modules ship, like their data files load
    registry
        .create(
            &pool,
            "ir.sequence",
            vec![
                ("name", json!("sale.order")),
                ("code", json!("sale.order")),
                ("prefix", json!("SO")),
                ("padding", json!(5)),
                ("number_next", json!(1)),
            ],
        )
        .await
        .unwrap();
    registry
        .create(
            &pool,
            "ir.sequence",
            vec![
                ("name", json!("account.move")),
                ("code", json!("account.move")),
                ("prefix", json!("FAT/")),
                ("padding", json!(5)),
                ("number_next", json!(1)),
            ],
        )
        .await
        .unwrap();
    registry
        .create(
            &pool,
            "ir.sequence",
            vec![
                ("name", json!("stock.picking.out")),
                ("code", json!("stock.picking.out")),
                ("prefix", json!("WH/OUT/")),
                ("padding", json!(5)),
                ("number_next", json!(1)),
            ],
        )
        .await
        .unwrap();
    let mut methods = MethodRegistry::new();
    rusdoo_account::extend_methods(&mut methods).unwrap();
    rusdoo_stock::extend_methods(&mut methods).unwrap();
    rusdoo_sale::extend_methods(&mut methods).unwrap();
    OrmService::insecure(Arc::new(registry), pool).with_methods(methods)
}

async fn call(app: axum::Router, params: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "call", "params": params});
    let response = app
        .oneshot(
            Request::post("/web/dataset/call_kw")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn create(service: &OrmService, model: &str, values: Value) -> i64 {
    call(
        router(service.clone()),
        json!({"model": model, "method": "create", "args": [values], "kwargs": {}}),
    )
    .await["result"]
        .as_i64()
        .expect("created")
}

/// An order for one storable product and one service, confirmed.
async fn a_confirmed_order(service: &OrmService, name: &str) -> i64 {
    let partner = create(service, "res.partner", json!({"name": "Cliente"})).await;
    let table = create(
        service,
        "product.product",
        json!({"name": "Mesa", "type": "consu", "list_price": 1250}),
    )
    .await;
    let setup = create(
        service,
        "product.product",
        json!({"name": "Montagem", "type": "service", "list_price": 300}),
    )
    .await;
    let order = create(
        service,
        "sale.order",
        json!({"name": name, "partner_id": partner, "order_line": [
            [0, 0, {"product_id": table, "name": "Mesa", "product_uom_qty": 2, "price_unit": 1250}],
            [0, 0, {"product_id": setup, "name": "Montagem", "product_uom_qty": 1, "price_unit": 300}],
        ]}),
    )
    .await;
    call(
        router(service.clone()),
        json!({"model": "sale.order", "method": "action_confirm",
               "args": [[order]], "kwargs": {}}),
    )
    .await;
    order
}

#[tokio::test]
async fn a_confirmed_order_produces_a_delivery_of_its_goods_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let service = fixture(&url, rusdoo_testing::schema_for("rusdoo_delivery_test")).await;
    let order = a_confirmed_order(&service, "SO-DEL").await;

    let answer = call(
        router(service.clone()),
        json!({"model": "sale.order", "method": "action_create_delivery",
               "args": [[order]], "kwargs": {}}),
    )
    .await;
    let action = &answer["result"];
    assert_eq!(action["res_model"], "stock.picking", "{answer}");
    let picking = action["res_id"].as_i64().expect("a picking id");

    let rows = call(
        router(service.clone()),
        json!({"model": "stock.picking", "method": "read", "args": [[picking]],
               "kwargs": {"fields": ["state", "picking_type", "origin", "move_ids"]}}),
    )
    .await;
    let row = &rows["result"][0];
    assert_eq!(row["state"], "draft");
    assert_eq!(row["picking_type"], "outgoing");
    assert_eq!(row["origin"], "SO-DEL");
    // the service line stayed behind: nobody puts an installation in a box
    let moves = row["move_ids"].as_array().expect("moves");
    assert_eq!(moves.len(), 1, "only the storable line travels: {row}");

    // delivering the same order twice would ship it twice
    let answer = call(
        router(service.clone()),
        json!({"model": "sale.order", "method": "action_create_delivery",
               "args": [[order]], "kwargs": {}}),
    )
    .await;
    assert!(answer.get("result").is_none(), "{answer}");
    assert!(answer["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("already has a delivery"));

    // confirm, then validate: what was planned becomes what was shipped
    call(
        router(service.clone()),
        json!({"model": "stock.picking", "method": "action_confirm",
               "args": [[picking]], "kwargs": {}}),
    )
    .await;
    let answer = call(
        router(service.clone()),
        json!({"model": "stock.picking", "method": "action_done",
               "args": [[picking]], "kwargs": {}}),
    )
    .await;
    assert_eq!(answer["result"], json!(true), "{answer}");

    let rows = call(
        router(service.clone()),
        json!({"model": "stock.picking", "method": "read", "args": [[picking]],
               "kwargs": {"fields": ["state"]}}),
    )
    .await;
    assert_eq!(rows["result"][0]["state"], "done");
    let move_id = moves[0].as_i64().unwrap();
    let rows = call(
        router(service),
        json!({"model": "stock.move", "method": "read", "args": [[move_id]],
               "kwargs": {"fields": ["product_uom_qty", "quantity_done"]}}),
    )
    .await;
    assert_eq!(
        rows["result"][0]["quantity_done"], rows["result"][0]["product_uom_qty"],
        "validating without touching anything ships what was planned"
    );
}

#[tokio::test]
async fn an_order_of_services_only_has_nothing_to_deliver_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let service = fixture(&url, rusdoo_testing::schema_for("rusdoo_delivery_service_test")).await;
    let partner = create(&service, "res.partner", json!({"name": "Cliente"})).await;
    let consulting = create(
        &service,
        "product.product",
        json!({"name": "Consultoria", "type": "service"}),
    )
    .await;
    let order = create(
        &service,
        "sale.order",
        json!({"name": "SO-SRV", "partner_id": partner, "order_line": [
            [0, 0, {"product_id": consulting, "product_uom_qty": 4, "price_unit": 200}],
        ]}),
    )
    .await;
    call(
        router(service.clone()),
        json!({"model": "sale.order", "method": "action_confirm",
               "args": [[order]], "kwargs": {}}),
    )
    .await;
    let answer = call(
        router(service),
        json!({"model": "sale.order", "method": "action_create_delivery",
               "args": [[order]], "kwargs": {}}),
    )
    .await;
    assert!(answer.get("result").is_none(), "{answer}");
    assert!(answer["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("services only"));
}

#[tokio::test]
async fn a_picking_without_moves_is_not_confirmed_live() {
    let Ok(url) = std::env::var("RUSDOO_TEST_DATABASE_URL") else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let service = fixture(&url, rusdoo_testing::schema_for("rusdoo_delivery_empty_test")).await;
    let picking = create(&service, "stock.picking", json!({"name": "WH/OUT/0001"})).await;
    let answer = call(
        router(service),
        json!({"model": "stock.picking", "method": "action_confirm",
               "args": [[picking]], "kwargs": {}}),
    )
    .await;
    assert!(answer.get("result").is_none(), "{answer}");
    assert!(answer["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("has no lines"));
}
