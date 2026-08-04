//! The check where it actually runs: on the write, through the ORM.
//!
//! The cases are the ones `base_vat`'s own test file states
//! (`odoo/addons/base_vat/tests/test_vat_numbers.py`), asked of this port
//! the same way — by writing a partner and seeing whether the database
//! keeps it.

use rusdoo_testing::TransactionCase;
use serde_json::{json, Value};

const MODULES: [&str; 2] = ["base", "base_vat"];

/// The id of a country, created with the code the check reads.
async fn a_country(case: &TransactionCase, name: &str, code: &str) -> i64 {
    case.models()
        .create(
            &case.pool(),
            "res.country",
            vec![("name", json!(name)), ("code", json!(code))],
        )
        .await
        .expect("the country saves")
}

async fn write_vat(case: &TransactionCase, partner: i64, vat: &str, country: Option<i64>) -> bool {
    let mut values: Vec<(&str, Value)> = vec![("vat", json!(vat))];
    values.push(("country_id", country.map_or(Value::Null, Value::from)));
    case.models()
        .write(&case.pool(), "res.partner", &[partner], values)
        .await
        .is_ok()
}

#[tokio::test]
async fn a_tax_id_is_refused_when_it_cannot_be_one_live() {
    let Some(case) = TransactionCase::open("vat_constraint", &MODULES).await else {
        return;
    };
    let belgium = a_country(&case, "Bélgica", "BE").await;
    let france = a_country(&case, "França", "FR").await;
    let partner = case
        .models()
        .create(&case.pool(), "res.partner", vec![("name", json!("John Dex"))])
        .await
        .expect("the partner saves");

    // the number names its country, and that country decides — even when
    // the customer is somewhere else
    assert!(write_vat(&case, partner, "BE0477472701", Some(france)).await);
    assert!(
        !write_vat(&case, partner, "BE23334175221", Some(france)).await,
        "a French number wearing a Belgian prefix is not a Belgian number"
    );

    // no prefix: the customer's country decides
    assert!(write_vat(&case, partner, "0477472701", Some(belgium)).await);
    assert!(!write_vat(&case, partner, "42", Some(belgium)).await);

    // and a partner with no country is never checked, prefix or not —
    // the case that surprises, and the one Odoo states outright
    assert!(write_vat(&case, partner, "BE42", None).await);
    assert!(write_vat(&case, partner, "BE0477472702", None).await);

    // `/` is how a partner says it has none on purpose
    assert!(write_vat(&case, partner, "/", Some(belgium)).await);

    case.close().await;
}

#[tokio::test]
async fn a_country_this_build_cannot_check_is_a_country_that_accepts_live() {
    let Some(case) = TransactionCase::open("vat_uncovered", &MODULES).await else {
        return;
    };
    let brazil = a_country(&case, "Brasil", "BR").await;
    let partner = case
        .models()
        .create(&case.pool(), "res.partner", vec![("name", json!("Loja"))])
        .await
        .expect("the partner saves");

    // no arithmetic for BR here: refusing what cannot be checked would
    // make the module a wall in front of every country it does not cover
    assert!(write_vat(&case, partner, "12.345.678/0001-95", Some(brazil)).await);

    case.close().await;
}

#[tokio::test]
async fn the_number_is_refused_at_the_create_too_live() {
    let Some(case) = TransactionCase::open("vat_create", &MODULES).await else {
        return;
    };
    let belgium = a_country(&case, "Bélgica", "BE").await;

    let refused = case
        .models()
        .create(
            &case.pool(),
            "res.partner",
            vec![
                ("name", json!("Errado")),
                ("country_id", json!(belgium)),
                ("vat", json!("BE0477472702")),
            ],
        )
        .await;
    let error = refused.expect_err("a wrong check digit was saved");
    assert!(
        error.to_string().contains("BE0477472702"),
        "the refusal says which number: {error}"
    );

    // and the record is not there: a constraint that ran after the insert
    // still has to leave nothing behind
    let found = case
        .models()
        .search(
            &case.pool(),
            "res.partner",
            &rusdoo_orm::domain::parse_domain(&json!([["name", "=", "Errado"]])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .expect("the search runs");
    assert!(found.is_empty(), "the refused partner stayed: {found:?}");

    case.close().await;
}
