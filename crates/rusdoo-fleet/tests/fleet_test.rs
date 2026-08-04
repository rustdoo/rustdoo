//! The fleet against a real database: what a vehicle is called, what its
//! odometer refuses, who drove it, and when a contract turns overdue.
//!
//! Every case builds its own schema through `rusdoo_testing::pool_in`,
//! so two of them — or two runs of the suite — never share a table.

use rusdoo_core::RusdooError;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::methods::{MethodCtx, MethodRegistry};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use std::sync::Arc;

/// A case: its own schema, the base models plus the fleet's, and every
/// table created. `None` means there is no test database configured, and
/// the caller returns instead of failing.
async fn fixture(case: &str) -> Option<(Arc<Registry>, PgPool)> {
    let Some(pool) = rusdoo_testing::pool_in(&format!("rusdoo_fleet_{case}")) else {
        eprintln!("skipped: {} not set", rusdoo_testing::DATABASE_ENV);
        return None;
    };
    // schemas left behind by runs that crashed are dropped here; a test
    // database that grows a dozen schemas per run is nobody's idea of a
    // clean one
    rusdoo_testing::sweep_stale_schemas(&pool).await;
    let mut registry = rusdoo_base::registry().expect("base registers");
    rusdoo_fleet::extend(&mut registry).expect("fleet registers");
    registry
        .init_tables(&pool)
        .await
        .expect("creating the models' tables");
    // the superuser every call is made as; a record stamped with an
    // author who is not in the table is a reference the database refuses
    sqlx::query(
        r#"INSERT INTO "res_users" ("id", "login", "name", "active")
           VALUES (1, 'admin', 'Administrator', true)
           ON CONFLICT ("id") DO NOTHING"#,
    )
    .execute(&pool)
    .await
    .expect("creating the case's superuser");
    Some((Arc::new(registry), pool))
}

/// Call a registered method the way the dispatch does.
async fn call(
    registry: &Arc<Registry>,
    pool: &PgPool,
    model: &str,
    name: &str,
    ids: &[i64],
    kwargs: Value,
) -> Result<Value, RusdooError> {
    let mut methods = MethodRegistry::new();
    rusdoo_fleet::extend_methods(&mut methods).unwrap();
    let method = methods.get(model, name).expect("a registered method");
    let kwargs: Map<String, Value> = match kwargs {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    // the registry travels by handle now: a method may hand it to
    // another one that outlives this call
    let ctx = MethodCtx::new(Arc::clone(registry), pool, 1, model, ids.to_vec());
    method.call(ctx, &[], &kwargs).await
}

async fn create(
    registry: &Registry,
    pool: &PgPool,
    model: &str,
    values: Value,
) -> Result<i64, RusdooError> {
    let values: Vec<(&str, Value)> = values
        .as_object()
        .expect("an object of values")
        .iter()
        .map(|(key, value)| (key.as_str(), value.clone()))
        .collect();
    registry.create(pool, model, values).await
}

async fn field(
    registry: &Registry,
    pool: &PgPool,
    model: &str,
    id: i64,
    name: &str,
) -> Value {
    registry
        .read(pool, model, &[id], &[name])
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap_or_default()
        .get(name)
        .cloned()
        .unwrap_or(Value::Null)
}

async fn find(registry: &Registry, pool: &PgPool, model: &str, domain: Value) -> Vec<i64> {
    registry
        .search(
            pool,
            model,
            &parse_domain(&domain).unwrap(),
            &SearchOptions::default(),
        )
        .await
        .unwrap()
}

/// A date `days` from today, as a column reads back.
fn in_days(days: i64) -> String {
    (chrono::Utc::now().date_naive() + chrono::Duration::days(days)).to_string()
}

/// A brand, a model and a vehicle — the three records nothing else works
/// without.
async fn a_vehicle(registry: &Registry, pool: &PgPool, plate: &str) -> i64 {
    let brand = create(registry, pool, "fleet.vehicle.model.brand", json!({"name": "Audi"}))
        .await
        .unwrap();
    let model = create(
        registry,
        pool,
        "fleet.vehicle.model",
        json!({"name": "A3", "brand_id": brand}),
    )
    .await
    .unwrap();
    create(
        registry,
        pool,
        "fleet.vehicle",
        json!({"model_id": model, "license_plate": plate}),
    )
    .await
    .unwrap()
}

async fn a_driver(registry: &Registry, pool: &PgPool, name: &str) -> i64 {
    create(registry, pool, "res.partner", json!({"name": name}))
        .await
        .unwrap()
}

#[tokio::test]
async fn a_vehicle_is_named_after_its_brand_model_and_plate_live() {
    let Some((registry, pool)) = fixture("naming").await else {
        return;
    };
    let vehicle = a_vehicle(&registry, &pool, "ABC-1234").await;
    assert_eq!(
        field(&registry, &pool, "fleet.vehicle", vehicle, "name").await,
        json!("Audi/A3/ABC-1234")
    );

    // the name is a column, so renaming the plate rewrites it — and a
    // search by name finds the vehicle under its new one
    registry
        .write(
            &pool,
            "fleet.vehicle",
            &[vehicle],
            vec![("license_plate", json!("XYZ-9999"))],
        )
        .await
        .unwrap();
    assert_eq!(
        field(&registry, &pool, "fleet.vehicle", vehicle, "name").await,
        json!("Audi/A3/XYZ-9999")
    );
    let found = find(
        &registry,
        &pool,
        "fleet.vehicle",
        json!([["name", "like", "XYZ"]]),
    )
    .await;
    assert_eq!(found, vec![vehicle], "the stored name is searchable");
}

#[tokio::test]
async fn a_vehicle_is_filled_in_from_its_model_without_losing_what_was_typed_live() {
    let Some((registry, pool)) = fixture("load_model").await else {
        return;
    };
    let brand = create(&registry, &pool, "fleet.vehicle.model.brand", json!({"name": "Audi"}))
        .await
        .unwrap();
    let model = create(
        &registry,
        &pool,
        "fleet.vehicle.model",
        json!({
            "name": "A3",
            "brand_id": brand,
            "seats": 5,
            "doors": 5,
            "default_fuel_type": "diesel",
            "default_co2": 118.0,
            "power": 110.0,
            // never set on the model: it must not blank the vehicle's
            "color": "",
        }),
    )
    .await
    .unwrap();
    let vehicle = create(
        &registry,
        &pool,
        "fleet.vehicle",
        json!({"model_id": model, "license_plate": "ABC-1234", "color": "Midnight blue", "seats": 2}),
    )
    .await
    .unwrap();

    call(&registry, &pool, "fleet.vehicle", "action_load_from_model", &[vehicle], json!({}))
        .await
        .expect("the model fills the vehicle in");

    let read = registry
        .read(
            &pool,
            "fleet.vehicle",
            &[vehicle],
            &["seats", "doors", "fuel_type", "co2", "power", "color"],
        )
        .await
        .unwrap();
    let vehicle_row = &read[0];
    assert_eq!(vehicle_row["seats"], json!(5), "the model's spec wins");
    assert_eq!(vehicle_row["doors"], json!(5));
    assert_eq!(vehicle_row["fuel_type"], json!("diesel"), "renamed on the way");
    assert_eq!(vehicle_row["co2"], json!(118.0), "renamed on the way");
    assert_eq!(vehicle_row["power"], json!(110.0));
    // the model said nothing about colour, so what was typed survives
    assert_eq!(vehicle_row["color"], json!("Midnight blue"));
}

#[tokio::test]
async fn an_odometer_never_goes_backwards_live() {
    let Some((registry, pool)) = fixture("odometer").await else {
        return;
    };
    let vehicle = a_vehicle(&registry, &pool, "ABC-1234").await;
    assert_eq!(
        field(&registry, &pool, "fleet.vehicle", vehicle, "odometer").await,
        json!(0.0),
        "a vehicle nobody read is at zero, not null"
    );

    for reading in [12_000.0, 45_000.0] {
        call(
            &registry,
            &pool,
            "fleet.vehicle",
            "update_odometer",
            &[vehicle],
            json!({"value": reading}),
        )
        .await
        .expect("a higher reading is accepted");
    }
    assert_eq!(
        field(&registry, &pool, "fleet.vehicle", vehicle, "odometer").await,
        json!(45_000.0)
    );
    assert_eq!(
        field(&registry, &pool, "fleet.vehicle", vehicle, "odometer_count").await,
        json!(2)
    );

    let error = call(
        &registry,
        &pool,
        "fleet.vehicle",
        "update_odometer",
        &[vehicle],
        json!({"value": 30_000.0}),
    )
    .await
    .expect_err("a lower reading is refused");
    assert!(
        error.to_string().contains("cannot be lower than the previous one"),
        "{error}"
    );

    // emptying it is refused too, like Odoo's `_set_odometer`
    let error = call(
        &registry,
        &pool,
        "fleet.vehicle",
        "update_odometer",
        &[vehicle],
        json!({"value": 0.0}),
    )
    .await
    .expect_err("a blank reading is refused");
    assert!(error.to_string().contains("emptying the odometer"), "{error}");

    // and nothing was recorded by either refusal
    assert_eq!(
        field(&registry, &pool, "fleet.vehicle", vehicle, "odometer_count").await,
        json!(2)
    );
    // the reading carries the vehicle and the day in its name
    let readings = find(&registry, &pool, "fleet.vehicle.odometer", json!([])).await;
    let name = field(
        &registry,
        &pool,
        "fleet.vehicle.odometer",
        readings[0],
        "name",
    )
    .await;
    assert!(
        name.as_str().unwrap().starts_with("Audi/A3/ABC-1234 / "),
        "{name}"
    );
}

#[tokio::test]
async fn handing_a_vehicle_over_closes_the_previous_driver_s_row_live() {
    let Some((registry, pool)) = fixture("history").await else {
        return;
    };
    let vehicle = a_vehicle(&registry, &pool, "ABC-1234").await;
    let ana = a_driver(&registry, &pool, "Ana").await;
    let beto = a_driver(&registry, &pool, "Beto").await;

    for driver in [ana, beto] {
        call(
            &registry,
            &pool,
            "fleet.vehicle",
            "action_assign_driver",
            &[vehicle],
            json!({"driver_id": driver}),
        )
        .await
        .expect("the vehicle changes hands");
    }

    assert_eq!(
        field(&registry, &pool, "fleet.vehicle", vehicle, "driver_id")
            .await
            .as_array()
            .and_then(|pair| pair.first().cloned()),
        Some(json!(beto))
    );
    assert_eq!(
        field(&registry, &pool, "fleet.vehicle", vehicle, "history_count").await,
        json!(2),
        "two drivers, two rows"
    );
    // exactly one row is open: a history where both look current cannot
    // answer who had the car in March
    let open = find(
        &registry,
        &pool,
        "fleet.vehicle.assignation.log",
        json!([["date_end", "=", null]]),
    )
    .await;
    assert_eq!(open.len(), 1);
    assert_eq!(
        field(
            &registry,
            &pool,
            "fleet.vehicle.assignation.log",
            open[0],
            "driver_id"
        )
        .await
        .as_array()
        .and_then(|pair| pair.first().cloned()),
        Some(json!(beto))
    );

    // handing it to the same person again changes nothing
    call(
        &registry,
        &pool,
        "fleet.vehicle",
        "action_assign_driver",
        &[vehicle],
        json!({"driver_id": beto}),
    )
    .await
    .unwrap();
    assert_eq!(
        field(&registry, &pool, "fleet.vehicle", vehicle, "history_count").await,
        json!(2),
        "the same driver twice is one assignment"
    );
}

#[tokio::test]
async fn a_queued_driver_releases_the_vehicle_they_already_had_live() {
    let Some((registry, pool)) = fixture("driver_change").await else {
        return;
    };
    let old_car = a_vehicle(&registry, &pool, "OLD-0001").await;
    let new_car = a_vehicle(&registry, &pool, "NEW-0001").await;
    let ana = a_driver(&registry, &pool, "Ana").await;

    call(
        &registry,
        &pool,
        "fleet.vehicle",
        "action_assign_driver",
        &[old_car],
        json!({"driver_id": ana}),
    )
    .await
    .unwrap();

    // queueing her for the new car flags the one she has today
    let flagged = call(
        &registry,
        &pool,
        "fleet.vehicle",
        "action_plan_driver_change",
        &[new_car],
        json!({"driver_id": ana}),
    )
    .await
    .unwrap();
    assert_eq!(flagged, json!(1));
    assert_eq!(
        field(&registry, &pool, "fleet.vehicle", old_car, "plan_to_change_car").await,
        json!(true)
    );

    call(
        &registry,
        &pool,
        "fleet.vehicle",
        "action_accept_driver_change",
        &[new_car],
        json!({}),
    )
    .await
    .expect("she takes the new car");

    // one person, one car of each type
    assert_eq!(
        field(&registry, &pool, "fleet.vehicle", old_car, "driver_id").await,
        Value::Null
    );
    assert_eq!(
        field(&registry, &pool, "fleet.vehicle", new_car, "driver_id")
            .await
            .as_array()
            .and_then(|pair| pair.first().cloned()),
        Some(json!(ana))
    );
    assert_eq!(
        field(&registry, &pool, "fleet.vehicle", new_car, "future_driver_id").await,
        Value::Null,
        "the queue is empty once it was honoured"
    );
    assert_eq!(
        field(&registry, &pool, "fleet.vehicle", old_car, "plan_to_change_car").await,
        json!(false)
    );
    // the old car's row was closed, the new one's opened
    let open = find(
        &registry,
        &pool,
        "fleet.vehicle.assignation.log",
        json!([["date_end", "=", null]]),
    )
    .await;
    assert_eq!(open.len(), 1, "only the current assignment is open");

    // and nobody is queued, so pressing it again says so instead of
    // silently clearing the driver
    let error = call(
        &registry,
        &pool,
        "fleet.vehicle",
        "action_accept_driver_change",
        &[new_car],
        json!({}),
    )
    .await
    .expect_err("there is nobody to accept");
    assert!(error.to_string().contains("no driver is queued"), "{error}");
}

#[tokio::test]
async fn a_contract_that_ran_out_makes_the_vehicle_overdue_live() {
    let Some((registry, pool)) = fixture("renewal").await else {
        return;
    };
    let vehicle = a_vehicle(&registry, &pool, "ABC-1234").await;
    create(
        &registry,
        &pool,
        "fleet.vehicle.log.contract",
        json!({"vehicle_id": vehicle, "expiration_date": in_days(-10), "state": "open"}),
    )
    .await
    .unwrap();

    let read = registry
        .read(
            &pool,
            "fleet.vehicle",
            &[vehicle],
            &[
                "contract_renewal_overdue",
                "contract_renewal_due_soon",
                "contract_state",
                "contract_count",
            ],
        )
        .await
        .unwrap();
    assert_eq!(read[0]["contract_renewal_overdue"], json!(true));
    assert_eq!(read[0]["contract_renewal_due_soon"], json!(false));
    assert_eq!(read[0]["contract_state"], json!("open"));
    assert_eq!(read[0]["contract_count"], json!(1));

    // Odoo's own case: a renewal on file clears the alarm
    create(
        &registry,
        &pool,
        "fleet.vehicle.log.contract",
        json!({"vehicle_id": vehicle, "expiration_date": in_days(365), "state": "open"}),
    )
    .await
    .unwrap();
    assert_eq!(
        field(&registry, &pool, "fleet.vehicle", vehicle, "contract_renewal_overdue").await,
        json!(false)
    );
}

#[tokio::test]
async fn a_contract_about_to_run_out_is_flagged_before_it_does_live() {
    let Some((registry, pool)) = fixture("due_soon").await else {
        return;
    };
    let vehicle = a_vehicle(&registry, &pool, "ABC-1234").await;
    let contract = create(
        &registry,
        &pool,
        "fleet.vehicle.log.contract",
        json!({"vehicle_id": vehicle, "expiration_date": in_days(10), "state": "open"}),
    )
    .await
    .unwrap();

    assert_eq!(
        field(&registry, &pool, "fleet.vehicle", vehicle, "contract_renewal_due_soon").await,
        json!(true)
    );
    let read = registry
        .read(
            &pool,
            "fleet.vehicle.log.contract",
            &[contract],
            &["days_left", "expires_today", "name"],
        )
        .await
        .unwrap();
    assert_eq!(read[0]["days_left"], json!(10));
    assert_eq!(read[0]["expires_today"], json!(false));
    assert_eq!(
        read[0]["name"], json!("Audi/A3/ABC-1234"),
        "a contract with no type is named after the vehicle"
    );

    // cancelling it takes the vehicle off the renewal list
    call(
        &registry,
        &pool,
        "fleet.vehicle.log.contract",
        "action_close",
        &[contract],
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(
        field(&registry, &pool, "fleet.vehicle", vehicle, "contract_renewal_due_soon").await,
        json!(false)
    );
    assert_eq!(
        field(&registry, &pool, "fleet.vehicle", vehicle, "contract_count").await,
        json!(0),
        "a cancelled contract is not a contract the button offers"
    );
}

#[tokio::test]
async fn the_nightly_job_moves_each_contract_to_where_the_calendar_puts_it_live() {
    let Some((registry, pool)) = fixture("scheduler").await else {
        return;
    };
    let vehicle = a_vehicle(&registry, &pool, "ABC-1234").await;
    let mut contracts = Vec::new();
    for (start, expiry, state) in [
        // ran out last week while it was still marked running
        (in_days(-400), in_days(-7), "open"),
        // starts next month, but somebody created it as running
        (in_days(30), in_days(400), "open"),
        // was queued and starts today
        (in_days(0), in_days(365), "futur"),
        // cancelled: the job must not touch it, however old it is
        (in_days(-400), in_days(-7), "closed"),
    ] {
        contracts.push(
            create(
                &registry,
                &pool,
                "fleet.vehicle.log.contract",
                json!({
                    "vehicle_id": vehicle,
                    "start_date": start,
                    "expiration_date": expiry,
                    "state": state,
                }),
            )
            .await
            .unwrap(),
        );
    }

    let moved = call(
        &registry,
        &pool,
        "fleet.vehicle.log.contract",
        "run_scheduler",
        &[],
        json!({}),
    )
    .await
    .expect("the cron runs with no selection");
    assert_eq!(moved["expired"], json!(1));
    assert_eq!(moved["queued"], json!(1));
    assert_eq!(moved["started"], json!(1));

    let states: Vec<Value> = registry
        .read(&pool, "fleet.vehicle.log.contract", &contracts, &["state"])
        .await
        .unwrap()
        .iter()
        .map(|row| row["state"].clone())
        .collect();
    assert_eq!(
        states,
        vec![json!("expired"), json!("futur"), json!("open"), json!("closed")]
    );

    // running it again changes nothing: the job is idempotent, which is
    // what makes it safe on a clock
    let again = call(
        &registry,
        &pool,
        "fleet.vehicle.log.contract",
        "run_scheduler",
        &[],
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(again, json!({"expired": 0, "queued": 0, "started": 0}));
}

#[tokio::test]
async fn archiving_a_vehicle_takes_its_logs_with_it_live() {
    let Some((registry, pool)) = fixture("archive").await else {
        return;
    };
    let vehicle = a_vehicle(&registry, &pool, "ABC-1234").await;
    let service_type = create(
        &registry,
        &pool,
        "fleet.service.type",
        json!({"name": "Oil change", "category": "service"}),
    )
    .await
    .unwrap();
    let contract = create(
        &registry,
        &pool,
        "fleet.vehicle.log.contract",
        json!({"vehicle_id": vehicle, "expiration_date": in_days(-1), "state": "open"}),
    )
    .await
    .unwrap();
    create(
        &registry,
        &pool,
        "fleet.vehicle.log.services",
        json!({"vehicle_id": vehicle, "service_type_id": service_type}),
    )
    .await
    .unwrap();

    call(&registry, &pool, "fleet.vehicle", "action_archive", &[vehicle], json!({}))
        .await
        .expect("the vehicle leaves the board");

    assert_eq!(
        field(&registry, &pool, "fleet.vehicle", vehicle, "active").await,
        json!(false)
    );
    // a contract whose vehicle is gone must not keep asking to be
    // renewed: a search finds neither
    assert!(find(&registry, &pool, "fleet.vehicle", json!([])).await.is_empty());
    assert!(find(&registry, &pool, "fleet.vehicle.log.contract", json!([]))
        .await
        .is_empty());
    assert!(find(&registry, &pool, "fleet.vehicle.log.services", json!([]))
        .await
        .is_empty());
    // and the contract is archived rather than deleted: it is what the
    // lease was signed under
    assert_eq!(
        field(&registry, &pool, "fleet.vehicle.log.contract", contract, "active").await,
        json!(false)
    );
}

#[tokio::test]
async fn a_service_log_mirrors_the_vehicle_it_is_about_live() {
    let Some((registry, pool)) = fixture("service").await else {
        return;
    };
    let vehicle = a_vehicle(&registry, &pool, "ABC-1234").await;
    let ana = a_driver(&registry, &pool, "Ana").await;
    call(
        &registry,
        &pool,
        "fleet.vehicle",
        "action_assign_driver",
        &[vehicle],
        json!({"driver_id": ana}),
    )
    .await
    .unwrap();
    let service_type = create(
        &registry,
        &pool,
        "fleet.service.type",
        json!({"name": "Oil change", "category": "service"}),
    )
    .await
    .unwrap();
    let reading = create(
        &registry,
        &pool,
        "fleet.vehicle.odometer",
        json!({"vehicle_id": vehicle, "value": 61_000.0, "date": in_days(0)}),
    )
    .await
    .unwrap();
    let service = create(
        &registry,
        &pool,
        "fleet.vehicle.log.services",
        json!({
            "vehicle_id": vehicle,
            "service_type_id": service_type,
            "odometer_id": reading,
            "amount": 250.0,
        }),
    )
    .await
    .unwrap();

    let read = registry
        .read(
            &pool,
            "fleet.vehicle.log.services",
            &[service],
            &["brand_id", "model_id", "odometer", "odometer_unit", "state"],
        )
        .await
        .unwrap();
    // the list shows the brand and model without a second lookup
    assert_eq!(
        read[0]["brand_id"].as_array().map(|pair| pair[1].clone()),
        Some(json!("Audi"))
    );
    assert_eq!(
        read[0]["model_id"].as_array().map(|pair| pair[1].clone()),
        Some(json!("A3"))
    );
    // the reading is followed, not copied: the two can never disagree
    assert_eq!(read[0]["odometer"], json!(61_000.0));
    assert_eq!(read[0]["odometer_unit"], json!("kilometers"));
    assert_eq!(read[0]["state"], json!("new"));

    assert_eq!(
        field(&registry, &pool, "fleet.vehicle", vehicle, "service_count").await,
        json!(1)
    );
}

#[tokio::test]
async fn a_state_column_and_a_tag_may_not_be_created_twice_live() {
    let Some((registry, pool)) = fixture("uniqueness").await else {
        return;
    };
    create(&registry, &pool, "fleet.vehicle.state", json!({"name": "Registered", "sequence": 7}))
        .await
        .unwrap();
    let error = create(
        &registry,
        &pool,
        "fleet.vehicle.state",
        json!({"name": "Registered", "sequence": 9}),
    )
    .await
    .expect_err("two columns with one name is a board nobody can read");
    assert!(error.to_string().contains("already exists"), "{error}");

    create(&registry, &pool, "fleet.vehicle.tag", json!({"name": "Pool car"}))
        .await
        .unwrap();
    let error = create(&registry, &pool, "fleet.vehicle.tag", json!({"name": "Pool car"}))
        .await
        .expect_err("the same tag twice");
    assert!(error.to_string().contains("already exists"), "{error}");
}

#[tokio::test]
async fn the_catalogue_counts_what_hangs_off_it_live() {
    let Some((registry, pool)) = fixture("catalogue").await else {
        return;
    };
    let brand = create(&registry, &pool, "fleet.vehicle.model.brand", json!({"name": "Audi"}))
        .await
        .unwrap();
    let model = create(
        &registry,
        &pool,
        "fleet.vehicle.model",
        json!({"name": "A3", "brand_id": brand, "range_unit": "mi"}),
    )
    .await
    .unwrap();
    for plate in ["ABC-1234", "DEF-5678"] {
        create(
            &registry,
            &pool,
            "fleet.vehicle",
            json!({"model_id": model, "license_plate": plate}),
        )
        .await
        .unwrap();
    }

    assert_eq!(
        field(&registry, &pool, "fleet.vehicle.model.brand", brand, "model_count").await,
        json!(1)
    );
    assert_eq!(
        field(&registry, &pool, "fleet.vehicle.model", model, "vehicle_count").await,
        json!(2)
    );
    // emissions follow the range unit, so the two numbers on the form are
    // in the same system
    assert_eq!(
        field(&registry, &pool, "fleet.vehicle.model", model, "co2_emission_unit").await,
        json!("g/mi")
    );

    // the smart button offers the list once there is one to offer
    let action = call(
        &registry,
        &pool,
        "fleet.vehicle.model",
        "action_model_vehicle",
        &[model],
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(action["res_model"], json!("fleet.vehicle"));
    assert_eq!(action["view_mode"], json!("kanban,list,form"));
    assert_eq!(action["domain"], json!([["model_id", "=", model]]));
}
