//! A product is two records, as in Odoo: the template the catalogue
//! describes and the variant a warehouse counts. Reference:
//! `odoo/addons/product/models/product_template.py` +
//! `product_product.py` (`_inherits = {'product.template': 'product_tmpl_id'}`).

use rusdoo_http::dispatch::OrmService;
use rusdoo_testing::TransactionCase;
use serde_json::{json, Value};

const MODULES: [&str; 2] = ["base", "product"];

async fn call(service: &OrmService, model: &str, method: &str, args: Value) -> Value {
    service
        .call_kw(1, model, method, args.as_array().unwrap(), &Default::default())
        .await
        .unwrap_or_else(|e| panic!("{model}.{method}: {}", e.message))
}

/// The whole point of the delegation: a caller that only knows about
/// variants still reads and writes one value, not two that drift apart.
#[tokio::test]
async fn a_product_written_through_the_variant_is_one_value_live() {
    let Some(case) = TransactionCase::open("product_tmpl", &MODULES).await else {
        return;
    };
    let service = OrmService::insecure(case.registry(), case.pool());

    // created through the variant, as every caller does it
    let id = call(
        &service,
        "product.product",
        "create",
        json!([{"name": "Cadeira", "list_price": 250.0, "standard_price": 90.0}]),
    )
    .await;
    let id = id.as_i64().expect("an id");

    // the name and the price come back through the variant...
    let read = call(
        &service,
        "product.product",
        "read",
        json!([[id], ["name", "list_price", "standard_price", "product_tmpl_id"]]),
    )
    .await;
    let row = &read[0];
    assert_eq!(row["name"], json!("Cadeira"));
    assert_eq!(row["list_price"], json!(250.0));
    assert_eq!(row["standard_price"], json!(90.0));

    // ...and the template is a record of its own, holding the name
    let tmpl = row["product_tmpl_id"][0].as_i64().expect("a template link");
    let read = call(
        &service,
        "product.template",
        "read",
        json!([[tmpl], ["name", "list_price"]]),
    )
    .await;
    assert_eq!(read[0]["name"], json!("Cadeira"));
    assert_eq!(read[0]["list_price"], json!(250.0));

    // writing the name through the variant writes the template's row:
    // one value, not two that drift apart
    call(
        &service,
        "product.product",
        "write",
        json!([[id], {"name": "Cadeira de escritório"}]),
    )
    .await;
    let read = call(
        &service,
        "product.template",
        "read",
        json!([[tmpl], ["name"]]),
    )
    .await;
    assert_eq!(read[0]["name"], json!("Cadeira de escritório"));
}

#[tokio::test]
async fn the_cost_is_the_variants_own_live() {
    let Some(case) = TransactionCase::open("product_cost", &MODULES).await else {
        return;
    };
    let service = OrmService::insecure(case.registry(), case.pool());

    // one template, two variants: what they share is priced once, what
    // they do not is priced apart
    let first = call(
        &service,
        "product.product",
        "create",
        json!([{"name": "Camiseta", "list_price": 60.0, "standard_price": 20.0}]),
    )
    .await;
    let first = first.as_i64().unwrap();
    let tmpl = call(
        &service,
        "product.product",
        "read",
        json!([[first], ["product_tmpl_id"]]),
    )
    .await[0]["product_tmpl_id"][0]
        .as_i64()
        .unwrap();

    let second = call(
        &service,
        "product.product",
        "create",
        json!([{"product_tmpl_id": tmpl, "standard_price": 35.0, "default_code": "CAM-G"}]),
    )
    .await;
    let second = second.as_i64().unwrap();

    let read = call(
        &service,
        "product.product",
        "read",
        json!([[first, second], ["name", "list_price", "standard_price"]]),
    )
    .await;
    let rows = read.as_array().unwrap();
    // the name and the sales price are the template's, so both variants
    // answer the same
    for row in rows {
        assert_eq!(row["name"], json!("Camiseta"), "{row}");
        assert_eq!(row["list_price"], json!(60.0), "{row}");
    }
    // the cost is not
    let costs: Vec<f64> = rows
        .iter()
        .map(|r| r["standard_price"].as_f64().unwrap())
        .collect();
    assert!(costs.contains(&20.0) && costs.contains(&35.0), "{costs:?}");
}

#[tokio::test]
async fn a_negative_price_is_refused_on_either_record_live() {
    let Some(case) = TransactionCase::open("product_neg", &MODULES).await else {
        return;
    };
    let service = OrmService::insecure(case.registry(), case.pool());

    // the sales price belongs to the template and is checked there
    let refused = service
        .call_kw(
            1,
            "product.template",
            "create",
            &[json!({"name": "Errado", "list_price": -1.0})],
            &Default::default(),
        )
        .await;
    assert!(refused.is_err(), "a negative sales price was accepted");

    // the cost belongs to the variant and is checked there
    let refused = service
        .call_kw(
            1,
            "product.product",
            "create",
            &[json!({"name": "Errado", "standard_price": -1.0})],
            &Default::default(),
        )
        .await;
    assert!(refused.is_err(), "a negative cost was accepted");
}

/// What the list view does when a user clicks the "Produto" column, and
/// what the kanban does before it draws a single card. Both name a field
/// that belongs to the template, and both reach it from the variant's
/// own query.
#[tokio::test]
async fn a_search_orders_and_groups_by_the_templates_fields_live() {
    let Some(case) = TransactionCase::open("product_delegated_query", &MODULES).await else {
        return;
    };
    let service = OrmService::insecure(case.registry(), case.pool());

    for (name, kind, code) in [
        ("Cadeira", "consu", "MOB-2"),
        ("Armário", "consu", "MOB-1"),
        ("Montagem", "service", "SRV-1"),
    ] {
        call(
            &service,
            "product.product",
            "create",
            json!([{"name": name, "type": kind, "default_code": code}]),
        )
        .await;
    }

    // ordered by the name the catalogue shows, not by the variant's own
    // columns
    let rows = service
        .call_kw(
            1,
            "product.product",
            "search_read",
            &[json!([]), json!(["name"])],
            &serde_json::from_value(json!({"order": "name asc"})).unwrap(),
        )
        .await
        .expect("search_read ordered by a delegated field");
    let names: Vec<&str> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Armário", "Cadeira", "Montagem"]);

    // and grouped by the type the template carries
    let groups = service
        .call_kw(
            1,
            "product.product",
            "formatted_read_group",
            &[json!([]), json!(["type"]), json!(["__count"])],
            &Default::default(),
        )
        .await
        .expect("read_group by a delegated field");
    let counts: Vec<(String, i64)> = groups
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            (
                g["type"].as_str().unwrap_or_default().to_string(),
                g["__count"].as_i64().unwrap(),
            )
        })
        .collect();
    assert!(counts.contains(&("consu".into(), 2)), "{counts:?}");
    assert!(counts.contains(&("service".into(), 1)), "{counts:?}");

    case.close().await;
}

/// Writing through the delegation is a write on the record that stores
/// the value, and `ir.model.access` says so. A user who may edit variants
/// does not get to rename the catalogue for free.
#[tokio::test]
async fn the_templates_own_access_is_checked_through_the_delegation_live() {
    use rusdoo_orm::access::{AccessControl, Operation};

    let Some(case) = TransactionCase::open("product_delegated_acl", &MODULES).await else {
        return;
    };
    let registry = case.registry();
    let pool = case.pool();

    // a user who may read and write products, and only read templates
    let group = registry
        .create(&pool, "res.groups", vec![("name", json!("estoque"))])
        .await
        .unwrap();
    let uid = registry
        .create(
            &pool,
            "res.users",
            vec![
                ("name", json!("Bruna")),
                ("login", json!("bruna")),
                ("groups_id", json!([[4, group, 0]])),
            ],
        )
        .await
        .unwrap();
    let mut access = AccessControl::new();
    access.grant(
        "product.product",
        group,
        &[Operation::Read, Operation::Write, Operation::Create],
    );
    access.grant("product.template", group, &[Operation::Read]);
    let service = rusdoo_http::dispatch::OrmService::new(registry, pool).with_access(access);

    // seeded as the superuser, who bypasses the ACL like Odoo's uid 1
    let id = call(
        &service,
        "product.product",
        "create",
        json!([{"name": "Prateleira", "standard_price": 40.0}]),
    )
    .await
    .as_i64()
    .unwrap();

    // her own field: allowed
    service
        .call_kw(
            uid,
            "product.product",
            "write",
            &[json!([id]), json!({"standard_price": 45.0})],
            &Default::default(),
        )
        .await
        .expect("the cost is the variant's own");

    // the template's field through the same call: refused
    let refused = service
        .call_kw(
            uid,
            "product.product",
            "write",
            &[json!([id]), json!({"name": "Prateleira grande"})],
            &Default::default(),
        )
        .await;
    let message = refused.expect_err("renaming needs write on the template").message;
    assert!(
        message.contains("product.template"),
        "the refusal names the model that refused: {message}"
    );

    // and creating a variant creates a template, which she may not do
    let refused = service
        .call_kw(
            uid,
            "product.product",
            "create",
            &[json!({"name": "Banqueta"})],
            &Default::default(),
        )
        .await;
    let message = refused
        .expect_err("creating a product creates its template")
        .message;
    assert!(
        message.contains("product.template"),
        "the refusal names the model that refused: {message}"
    );

    case.close().await;
}
