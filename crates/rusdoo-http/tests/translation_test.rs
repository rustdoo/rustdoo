//! Translatable fields: one value per language in the column itself, as
//! Odoo 19 faz desde que `ir.translation` deixou de existir.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rusdoo_http::dispatch::OrmService;
use rusdoo_http::routes::router;
use rusdoo_testing::TransactionCase;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn call(service: &OrmService, model: &str, method: &str, args: Value, kwargs: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "call",
        "params": {"model": model, "method": method, "args": args, "kwargs": kwargs}
    });
    let response = router(service.clone())
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

fn in_lang(lang: &str) -> Value {
    json!({"context": {"lang": lang}})
}

async fn name_of(service: &OrmService, id: i64, lang: &str) -> String {
    let answer = call(
        service,
        "product.product",
        "read",
        json!([[id], ["name"]]),
        in_lang(lang),
    )
    .await;
    answer["result"][0]["name"]
        .as_str()
        .unwrap_or_else(|| panic!("sem nome: {answer}"))
        .to_string()
}

#[tokio::test]
async fn a_value_per_language_lives_in_the_record_live() {
    let Some(case) = TransactionCase::open("translation", &["base", "product"]).await else {
        return;
    };
    let service = OrmService::insecure(case.registry(), case.pool());

    // born in English
    let id = call(
        &service,
        "product.product",
        "create",
        json!([{"name": "Oak table", "list_price": 100.0}]),
        in_lang("en_US"),
    )
    .await["result"]
        .as_i64()
        .unwrap();

    // with no translation, Portuguese falls back to the source
    // language: a screen half in English beats a screen half blank
    assert_eq!(name_of(&service, id, "pt_BR").await, "Oak table");

    // translating is writing with the context in that language
    call(
        &service,
        "product.product",
        "write",
        json!([[id], {"name": "Mesa de carvalho"}]),
        in_lang("pt_BR"),
    )
    .await;

    assert_eq!(name_of(&service, id, "pt_BR").await, "Mesa de carvalho");
    assert_eq!(
        name_of(&service, id, "en_US").await,
        "Oak table",
        "traduzir não apaga o original"
    );
    // and a third language with no translation still falls back
    assert_eq!(name_of(&service, id, "es_ES").await, "Oak table");

    // the value is in the row itself, not in a side table
    let stored: Value = sqlx::query_scalar(r#"SELECT "name" FROM "product_product" WHERE "id" = $1"#)
        .bind(id as i32)
        .fetch_one(&case.pool())
        .await
        .unwrap();
    assert_eq!(stored["en_US"], json!("Oak table"));
    assert_eq!(stored["pt_BR"], json!("Mesa de carvalho"));

    case.close().await;
}

#[tokio::test]
async fn a_record_born_in_a_language_keeps_that_language_apart_live() {
    let Some(case) = TransactionCase::open("translation_birth", &["base", "product"]).await else {
        return;
    };
    let service = OrmService::insecure(case.registry(), case.pool());

    // created with the client in Portuguese: the text becomes the source
    // value *and* the Portuguese value, like Odoo's
    // `convert_to_column_insert`
    let id = call(
        &service,
        "product.product",
        "create",
        json!([{"name": "Cadeira", "list_price": 10.0}]),
        in_lang("pt_BR"),
    )
    .await["result"]
        .as_i64()
        .unwrap();
    assert_eq!(name_of(&service, id, "pt_BR").await, "Cadeira");
    assert_eq!(name_of(&service, id, "en_US").await, "Cadeira");

    // and that is what makes the difference: translating the English
    // afterwards does not drag the Portuguese along
    call(
        &service,
        "product.product",
        "write",
        json!([[id], {"name": "Chair"}]),
        in_lang("en_US"),
    )
    .await;
    assert_eq!(name_of(&service, id, "en_US").await, "Chair");
    assert_eq!(
        name_of(&service, id, "pt_BR").await,
        "Cadeira",
        "o português foi guardado na criação, então sobrevive"
    );

    case.close().await;
}

#[tokio::test]
async fn a_language_from_the_client_cannot_be_a_query_live() {
    let Some(case) = TransactionCase::open("translation_injection", &["base", "product"]).await
    else {
        return;
    };
    let service = OrmService::insecure(case.registry(), case.pool());
    let id = call(
        &service,
        "product.product",
        "create",
        json!([{"name": "Mesa", "list_price": 1.0}]),
        json!({}),
    )
    .await["result"]
        .as_i64()
        .unwrap();

    // o `lang` vem de um contexto que o cliente controla, e entra numa
    // SQL expression: pasted raw, this would be a query of its own
    let answer = call(
        &service,
        "product.product",
        "read",
        json!([[id], ["name"]]),
        json!({"context": {"lang": "x' || (SELECT 'boom') || '"}}),
    )
    .await;
    assert_eq!(
        answer["result"][0]["name"],
        json!("Mesa"),
        "o idioma inventado não casa e cai na origem: {answer}"
    );

    case.close().await;
}

#[tokio::test]
async fn a_filter_matches_the_name_in_the_callers_language_live() {
    let Some(case) = TransactionCase::open("translation_search", &["base", "product"]).await else {
        return;
    };
    let service = OrmService::insecure(case.registry(), case.pool());
    let id = call(
        &service,
        "product.product",
        "create",
        json!([{"name": "Oak table", "list_price": 100.0}]),
        in_lang("en_US"),
    )
    .await["result"]
        .as_i64()
        .unwrap();
    call(
        &service,
        "product.product",
        "write",
        json!([[id], {"name": "Mesa de carvalho"}]),
        in_lang("pt_BR"),
    )
    .await;

    // searching by the translated name finds it, and by the original
    // too —
    // cada um no seu idioma
    for (lang, term) in [("pt_BR", "Mesa de carvalho"), ("en_US", "Oak table")] {
        let found = call(
            &service,
            "product.product",
            "search",
            json!([[["name", "=", term]]]),
            in_lang(lang),
        )
        .await;
        assert_eq!(found["result"], json!([id]), "{lang}/{term}: {found}");
    }

    // and looking for the other language's text finds nothing, which is
    // what a
    // filtro quer dizer
    let found = call(
        &service,
        "product.product",
        "search",
        json!([[["name", "=", "Mesa de carvalho"]]]),
        in_lang("en_US"),
    )
    .await;
    assert_eq!(found["result"], json!([]), "{found}");

    // the search bar's `like` as well
    let found = call(
        &service,
        "product.product",
        "name_search",
        json!(["carvalho"]),
        in_lang("pt_BR"),
    )
    .await;
    assert_eq!(
        found["result"],
        json!([[id, "Mesa de carvalho"]]),
        "o name_search casa e devolve o nome no idioma: {found}"
    );

    case.close().await;
}

/// The list is the most common screen there is, and it does not go
/// through `read`: it goes through `web_search_read`. Translating only
/// one of the two would leave the
/// outro respondendo no idioma de origem e parecendo certo.
#[tokio::test]
async fn the_list_view_answers_in_the_callers_language_live() {
    let Some(case) = TransactionCase::open("translation_list", &["base", "product"]).await else {
        return;
    };
    let service = OrmService::insecure(case.registry(), case.pool());
    let id = call(
        &service,
        "product.product",
        "create",
        json!([{"name": "Oak table", "list_price": 100.0}]),
        in_lang("en_US"),
    )
    .await["result"]
        .as_i64()
        .unwrap();
    call(
        &service,
        "product.product",
        "write",
        json!([[id], {"name": "Mesa de carvalho"}]),
        in_lang("pt_BR"),
    )
    .await;

    for (lang, expected) in [("pt_BR", "Mesa de carvalho"), ("en_US", "Oak table")] {
        let answer = call(
            &service,
            "product.product",
            "web_search_read",
            json!([[], {"name": {}}]),
            in_lang(lang),
        )
        .await;
        assert_eq!(
            answer["result"]["records"][0]["name"],
            json!(expected),
            "web_search_read em {lang}: {answer}"
        );
    }

    case.close().await;
}

/// A field's label is text of the program, not a value in the database:
/// it comes from the module's `.po` and is the same for every record.
#[tokio::test]
async fn a_field_label_comes_back_in_the_callers_language_live() {
    let Some(case) = TransactionCase::open("translation_labels", &["base", "product"]).await else {
        return;
    };
    let mut catalogue = rusdoo_orm::translations::Translations::new();
    catalogue.extend(
        "pt_BR",
        [
            ("Create Date".to_string(), "Criado em".to_string()),
            ("List Price".to_string(), "Preço de venda".to_string()),
        ],
    );
    let service =
        OrmService::insecure(case.registry(), case.pool()).with_translations(catalogue);

    let answer = call(
        &service,
        "product.product",
        "fields_get",
        json!([]),
        in_lang("pt_BR"),
    )
    .await;
    assert_eq!(
        answer["result"]["list_price"]["string"],
        json!("Preço de venda"),
        "{answer}"
    );
    assert_eq!(answer["result"]["create_date"]["string"], json!("Criado em"));
    // an untranslated label shows in the source language, never blank
    assert_eq!(
        answer["result"]["standard_price"]["string"],
        json!("Standard Price")
    );

    // and in English it stays English
    let answer = call(
        &service,
        "product.product",
        "fields_get",
        json!([]),
        in_lang("en_US"),
    )
    .await;
    assert_eq!(answer["result"]["list_price"]["string"], json!("List Price"));

    case.close().await;
}
