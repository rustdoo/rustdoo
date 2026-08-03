//! O cadastro UTM contra um banco de verdade: o contador que evita o
//! nome repetido, o registro que um link reaproveita em vez de duplicar,
//! e o que o modelo recusa.

use rusdoo_orm::methods::{MethodCtx, MethodRegistry};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use sqlx::PgPool;

/// Each test in its own schema: the suite runs in parallel against one
/// database.
fn pool(url: &str, schema: &'static str) -> PgPool {
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

struct Fixture {
    registry: Registry,
    methods: MethodRegistry,
    pool: PgPool,
    /// who makes the calls, as a logged-in user would
    uid: i64,
}

impl Fixture {
    /// Call a model method the way the dispatch would.
    async fn call(&self, model: &str, method: &str, args: Vec<Value>) -> Result<Value, String> {
        let entry = self
            .methods
            .get(model, method)
            .unwrap_or_else(|| panic!("{model}.{method} registrado"));
        // a method with no records still gets its arguments, and they
        // vivem em `rest`
        let ctx = MethodCtx::new(&self.registry, &self.pool, self.uid, model, vec![])
            .with_rest(args.clone());
        let kwargs = Map::new();
        (entry.func)(ctx, &args, &kwargs)
            .await
            .map_err(|error| error.to_string())
    }

    /// The name that was created, whether from `name_create`'s pair or
    /// from the dictionary
    /// do `find_or_create`.
    fn created_name(answer: &Value) -> &str {
        answer
            .get(1)
            .or_else(|| answer.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("a resposta traz o nome: {answer}"))
    }

    fn created_id(answer: &Value) -> i64 {
        answer
            .get(0)
            .or_else(|| answer.get("id"))
            .and_then(Value::as_i64)
            .unwrap_or_else(|| panic!("a resposta traz o id: {answer}"))
    }

    async fn read(&self, model: &str, id: i64, fields: &[&str]) -> Map<String, Value> {
        self.registry
            .read(&self.pool, model, &[id], fields)
            .await
            .expect("leitura")
            .first()
            .cloned()
            .expect("o registro está lá")
    }
}

async fn fixture(schema: &'static str) -> Option<Fixture> {
    let url = std::env::var("RUSDOO_TEST_DATABASE_URL").ok()?;
    let pool = pool(&url, schema);
    let mut registry = rusdoo_base::registry().expect("base");
    rusdoo_utm::extend(&mut registry).expect("utm");
    for table in [
        "utm_tag_rel",
        "utm_campaign",
        "utm_tag",
        "utm_stage",
        "utm_medium",
        "utm_source",
        "res_users",
    ] {
        sqlx::query(&format!(r#"DROP TABLE IF EXISTS "{table}" CASCADE"#))
            .execute(&pool)
            .await
            .unwrap();
    }
    for model in [
        // res.users exists because a campaign has an owner, and reading
        // one
        // many2one vai buscar o nome do outro lado
        "res.users",
        "utm.stage",
        "utm.tag",
        "utm.medium",
        "utm.source",
        "utm.campaign",
    ] {
        registry
            .get(model)
            .unwrap()
            .init_table(&pool)
            .await
            .unwrap();
    }
    let uid = registry
        .create(
            &pool,
            "res.users",
            vec![("login", json!("marketing")), ("name", json!("Marketing"))],
        )
        .await
        .unwrap();
    let mut methods = MethodRegistry::new();
    rusdoo_utm::extend_methods(&mut methods).expect("métodos");
    Some(Fixture {
        registry,
        methods,
        pool,
        uid,
    })
}

#[tokio::test]
async fn the_same_medium_name_twice_gets_a_counter_live() {
    let Some(fx) = fixture(rusdoo_testing::schema_for("rusdoo_utm_medium")).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let first = fx
        .call("utm.medium", "name_create", vec![json!("Email")])
        .await
        .expect("o primeiro nasce com o nome pedido");
    assert_eq!(Fixture::created_name(&first), "Email");

    let second = fx
        .call("utm.medium", "name_create", vec![json!("Email")])
        .await
        .expect("o segundo nasce numerado, não recusado");
    assert_eq!(Fixture::created_name(&second), "Email [2]");
    assert_ne!(Fixture::created_id(&first), Fixture::created_id(&second));

    let third = fx
        .call("utm.medium", "name_create", vec![json!("Email")])
        .await
        .expect("e o terceiro segue a contagem");
    assert_eq!(Fixture::created_name(&third), "Email [3]");

    // um meio arquivado continua ocupando o nome: o contador o enxerga
    fx.registry
        .write(
            &fx.pool,
            "utm.medium",
            &[Fixture::created_id(&second)],
            vec![("active", json!(false))],
        )
        .await
        .unwrap();
    let fourth = fx
        .call("utm.medium", "name_create", vec![json!("Email")])
        .await
        .expect("criado");
    assert_eq!(Fixture::created_name(&fourth), "Email [4]");

    // and the gap left by a name that went away is reused
    fx.registry
        .get("utm.medium")
        .unwrap()
        .unlink(&fx.pool, &[Fixture::created_id(&third)])
        .await
        .unwrap();
    let fifth = fx
        .call("utm.medium", "name_create", vec![json!("Email")])
        .await
        .expect("criado");
    assert_eq!(Fixture::created_name(&fifth), "Email [3]");
}

#[tokio::test]
async fn a_link_reuses_the_source_that_is_already_there_live() {
    let Some(fx) = fixture(rusdoo_testing::schema_for("rusdoo_utm_source")).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let created = fx
        .call("utm.source", "find_or_create", vec![json!("Newsletter")])
        .await
        .expect("a origem nasce no primeiro clique");
    let id = Fixture::created_id(&created);

    // the same name in another case and with spaces left over is the
    // same source: a repeated click must not fill the records with
    // duplicates
    for asked in ["Newsletter", "  newsletter ", "NEWSLETTER"] {
        let again = fx
            .call("utm.source", "find_or_create", vec![json!(asked)])
            .await
            .expect("encontrada");
        assert_eq!(Fixture::created_id(&again), id, "pedindo {asked:?}");
        assert_eq!(Fixture::created_name(&again), "Newsletter");
    }

    // another name is another source
    let other = fx
        .call("utm.source", "find_or_create", vec![json!("Buscador")])
        .await
        .expect("criada");
    assert_ne!(Fixture::created_id(&other), id);

    // and a blank name creates nothing
    let refused = fx
        .call("utm.source", "find_or_create", vec![json!("   ")])
        .await
        .expect_err("sem nome não há o que procurar nem o que criar");
    assert!(refused.contains("say the name"), "{refused}");
}

#[tokio::test]
async fn a_campaign_is_born_with_an_identifier_a_stage_and_an_owner_live() {
    let Some(fx) = fixture(rusdoo_testing::schema_for("rusdoo_utm_campaign")).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    // with no stage at all a campaign has nowhere to start, and the
    // method says so
    let refused = fx
        .call("utm.campaign", "name_create", vec![json!("Black Friday")])
        .await
        .expect_err("faltou o estágio");
    assert!(refused.contains("create one before"), "{refused}");

    // the stages go in out of order: the first is the lowest sequence
    for (name, sequence) in [("Em andamento", 20), ("Novo", 10)] {
        fx.registry
            .create(
                &fx.pool,
                "utm.stage",
                vec![("name", json!(name)), ("sequence", json!(sequence))],
            )
            .await
            .unwrap();
    }

    let created = fx
        .call("utm.campaign", "name_create", vec![json!("Black Friday")])
        .await
        .expect("criada");
    let id = Fixture::created_id(&created);
    let row = fx
        .read(
            "utm.campaign",
            id,
            &["title", "name", "stage_id", "user_id", "is_auto_campaign"],
        )
        .await;
    assert_eq!(row["title"], "Black Friday");
    // the identifier is computed from the title and stored in the column
    assert_eq!(row["name"], "Black Friday");
    assert_eq!(
        row["stage_id"][1], "Novo",
        "a campanha começa no primeiro estágio"
    );
    assert_eq!(
        row["user_id"][0],
        json!(fx.uid),
        "o responsável é quem criou"
    );
    assert_eq!(
        row["is_auto_campaign"],
        json!(false),
        "esta foi criada por alguém"
    );

    // editing the title drags the identifier along
    fx.registry
        .write(
            &fx.pool,
            "utm.campaign",
            &[id],
            vec![("title", json!("Black Friday 2026"))],
        )
        .await
        .unwrap();
    let row = fx.read("utm.campaign", id, &["name"]).await;
    assert_eq!(row["name"], "Black Friday 2026");

    // the campaign a link creates comes marked as automatic
    let auto = fx
        .call("utm.campaign", "find_or_create", vec![json!("Natal")])
        .await
        .expect("criada pelo link");
    let row = fx
        .read(
            "utm.campaign",
            Fixture::created_id(&auto),
            &["is_auto_campaign", "name"],
        )
        .await;
    assert_eq!(row["is_auto_campaign"], json!(true));
    assert_eq!(row["name"], "Natal");

    // and the second click on the same link finds the one that exists
    let again = fx
        .call("utm.campaign", "find_or_create", vec![json!("natal")])
        .await
        .expect("encontrada");
    assert_eq!(
        Fixture::created_id(&again),
        Fixture::created_id(&auto),
        "o link não cria uma campanha por clique"
    );
}

#[tokio::test]
async fn a_record_without_a_real_name_is_refused_live() {
    let Some(fx) = fixture(rusdoo_testing::schema_for("rusdoo_utm_blank")).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    // the database's `required` accepts an empty string; the model's
    // rule does not
    for model in ["utm.medium", "utm.source", "utm.stage", "utm.tag"] {
        let error = fx
            .registry
            .create(&fx.pool, model, vec![("name", json!("   "))])
            .await
            .expect_err("um nome em branco não identifica nada");
        assert!(error.to_string().contains("cannot be left blank"), "{model}: {error}");
    }

    let stage = fx
        .registry
        .create(&fx.pool, "utm.stage", vec![("name", json!("Novo"))])
        .await
        .unwrap();
    let error = fx
        .registry
        .create(
            &fx.pool,
            "utm.campaign",
            vec![
                ("title", json!("")),
                ("stage_id", json!(stage)),
                ("user_id", json!(fx.uid)),
            ],
        )
        .await
        .expect_err("a campanha sem nome também é recusada");
    assert!(error.to_string().contains("give the campaign a name"), "{error}");

    // and none of that was left behind
    let left = fx
        .registry
        .search(
            &fx.pool,
            "utm.campaign",
            &rusdoo_orm::domain::parse_domain(&json!([])).unwrap(),
            &rusdoo_orm::crud::SearchOptions::default(),
        )
        .await
        .unwrap();
    assert!(left.is_empty(), "um registro recusado não sobrevive");
}

#[tokio::test]
async fn a_campaign_carries_its_tags_live() {
    let Some(fx) = fixture(rusdoo_testing::schema_for("rusdoo_utm_tags")).await else {
        eprintln!("skipped: RUSDOO_TEST_DATABASE_URL not set");
        return;
    };
    let stage = fx
        .registry
        .create(&fx.pool, "utm.stage", vec![("name", json!("Novo"))])
        .await
        .unwrap();
    let marketing = fx
        .registry
        .create(
            &fx.pool,
            "utm.tag",
            vec![("name", json!("Marketing")), ("color", json!(1))],
        )
        .await
        .unwrap();
    let newsletter = fx
        .registry
        .create(&fx.pool, "utm.tag", vec![("name", json!("Newsletter"))])
        .await
        .unwrap();

    let campaign = fx
        .registry
        .create(
            &fx.pool,
            "utm.campaign",
            vec![
                ("title", json!("Volta às aulas")),
                ("stage_id", json!(stage)),
                ("user_id", json!(fx.uid)),
                // os comandos x2many, como o cliente manda
                ("tag_ids", json!([[6, 0, [marketing, newsletter]]])),
            ],
        )
        .await
        .unwrap();

    let row = fx.read("utm.campaign", campaign, &["name", "tag_ids"]).await;
    assert_eq!(row["name"], "Volta às aulas");
    let mut tags: Vec<i64> = row["tag_ids"]
        .as_array()
        .expect("as etiquetas voltam como lista")
        .iter()
        .filter_map(Value::as_i64)
        .collect();
    tags.sort_unstable();
    assert_eq!(tags, vec![marketing, newsletter]);

    // taking a tag off the campaign does not delete the tag
    fx.registry
        .write(
            &fx.pool,
            "utm.campaign",
            &[campaign],
            vec![("tag_ids", json!([[3, marketing]]))],
        )
        .await
        .unwrap();
    let row = fx.read("utm.campaign", campaign, &["tag_ids"]).await;
    assert_eq!(row["tag_ids"], json!([newsletter]));
    let survived = fx.read("utm.tag", marketing, &["name"]).await;
    assert_eq!(survived["name"], "Marketing");
}
