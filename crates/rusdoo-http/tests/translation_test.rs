//! Campos traduzíveis: um valor por idioma na própria coluna, como o
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

    // nasce em inglês
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

    // sem tradução, o português cai no idioma de origem: uma tela pela
    // metade em inglês é melhor que uma tela pela metade em branco
    assert_eq!(name_of(&service, id, "pt_BR").await, "Oak table");

    // traduzir é escrever com o contexto no idioma
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
    // e um terceiro idioma sem tradução continua caindo na origem
    assert_eq!(name_of(&service, id, "es_ES").await, "Oak table");

    // o valor está na própria linha, não numa tabela lateral
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

    // criado com o cliente em português: o texto vira o valor de origem
    // *e* o valor em português, como o `convert_to_column_insert` do Odoo
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

    // e é isso que faz a diferença: traduzir o inglês depois não
    // arrasta o português junto
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
    // expressão SQL: se fosse colado cru, isto seria uma consulta
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

    // buscar pelo nome traduzido acha, e buscar pelo original também —
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

    // e procurar o texto do outro idioma não acha, que é o que um
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

    // o `like` da barra de busca também
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
