//! rusdoo-utm — port de `odoo/addons/utm/models/`: de onde veio quem
//! chegou.
//!
//! Cinco modelos pequenos que outros módulos apontam — campanha, meio,
//! origem, estágio e etiqueta. O `utm.mixin`, que enfia `campaign_id`,
//! `source_id` e `medium_id` em modelos de outros módulos, ficou de fora:
//! ele é `_inherit` entre módulos, que o port ainda não tem. O que
//! sobrou é o que existe por si — o cadastro que os links rastreiam — e
//! os dois caminhos pelos quais um link cria um registro sem pedir nada
//! a ninguém.

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};
use std::collections::HashSet;

/// Os modelos que um link rastreia (`_tracking_models` do `utm.mixin`).
/// Sem o mixin para reuni-los, cada um responde por si — e são estes
/// três que ganham os métodos de criação por nome.
const TRACKING_MODELS: [&str; 3] = ["utm.campaign", "utm.medium", "utm.source"];

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

fn meta(name: &str, table: &str) -> ModelMeta {
    ModelMeta {
        name: name.to_string(),
        table: table.to_string(),
        inherit: vec![],
        inherits: vec![],
    }
}

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    // estágio e etiqueta antes da campanha, que aponta para os dois
    reg.register(stage())?;
    reg.register(tag())?;
    reg.register(medium())?;
    reg.register(source())?;
    reg.register(campaign())?;
    Ok(())
}

/// Os dois modos de um link virar registro: `name_create`, quando alguém
/// digita um nome numa lista suspensa, e `find_or_create`, quando um
/// clique traz um `utm_campaign=...` que talvez já exista.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    for model in TRACKING_MODELS {
        methods.register(model, "name_create", Operation::Create, name_create)?;
        methods.register(model, "find_or_create", Operation::Create, find_or_create)?;
    }
    Ok(())
}

/// Um nome só de espaços passa pelo `required` do banco e não identifica
/// nada — quem procurar a origem depois não acha coluna nenhuma vazia.
fn name_is_filled(record: &Map<String, Value>) -> Result<(), String> {
    if is_blank(record, "name") {
        return Err("the name cannot be left blank".into());
    }
    Ok(())
}

/// A campanha guarda o nome em `title`; `name` é o identificador
/// derivado dele, e não adianta cobrar o segundo.
fn title_is_filled(record: &Map<String, Value>) -> Result<(), String> {
    if is_blank(record, "title") {
        return Err("give the campaign a name".into());
    }
    Ok(())
}

fn is_blank(record: &Map<String, Value>, field: &str) -> bool {
    record
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .is_empty()
}

/// `utm.stage` — em que ponto a campanha está.
fn stage() -> Model {
    Model::new(
        meta("utm.stage", "utm_stage"),
        vec![
            char("name").required(),
            // a ordem das colunas do quadro; o Odoo ordena o modelo por ela
            Field::new("sequence", FieldType::Integer).default_value(json!(1)),
        ],
    )
    .constrained("nome preenchido", &["name"], name_is_filled)
}

/// `utm.tag` — como o marketing separa as próprias campanhas.
fn tag() -> Model {
    Model::new(
        meta("utm.tag", "utm_tag"),
        vec![
            char("name").required(),
            // o Odoo sorteia uma cor; aqui a etiqueta nasce sem nenhuma,
            // que é o que "sem cor" significa no quadro: nada a destacar
            Field::new("color", FieldType::Integer).default_value(json!(0)),
        ],
    )
    .constrained("nome preenchido", &["name"], name_is_filled)
}

/// `utm.medium` — por onde o visitante veio (e-mail, banner, telefone).
fn medium() -> Model {
    Model::new(
        meta("utm.medium", "utm_medium"),
        vec![
            char("name").required(),
            // arquivar em vez de apagar: um meio some das listas sem
            // levar junto o histórico que aponta para ele
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
        ],
    )
    .constrained("nome preenchido", &["name"], name_is_filled)
}

/// `utm.source` — de qual lugar exatamente (a newsletter de maio, o
/// buscador, o site do parceiro).
fn source() -> Model {
    Model::new(
        meta("utm.source", "utm_source"),
        vec![char("name").required()],
    )
    .constrained("nome preenchido", &["name"], name_is_filled)
}

/// `name` — o identificador da campanha, que acompanha o nome dado a ela.
///
/// No Odoo ele também ganha um contador quando dois títulos coincidem;
/// isso pede uma busca no banco, e uma função de campo computado não
/// tem banco. Fica o espelho: uma campanha sempre tem identificador, e
/// é ele que aparece onde alguém a referencia.
fn campaign_name(record: &Map<String, Value>) -> Value {
    let title = record
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    json!(title)
}

/// `utm.campaign` — o esforço de marketing que se quer medir.
fn campaign() -> Model {
    Model::new(
        meta("utm.campaign", "utm_campaign"),
        vec![
            char("title").required(),
            // materializado: é por ele que a campanha aparece em toda
            // referência, e uma lista de campanhas lê uma coluna
            char("name").computed(&["title"], campaign_name).store(),
            m2o("user_id", "res.users").required(),
            m2o("stage_id", "utm.stage").required(),
            Field::new(
                "tag_ids",
                FieldType::Many2many {
                    comodel: "utm.tag".into(),
                    relation: "utm_tag_rel".into(),
                    // esta ponta primeiro, como o framework lê; o Odoo
                    // declara as duas trocadas por herança histórica
                    column1: "campaign_id".into(),
                    column2: "tag_id".into(),
                },
            ),
            // a campanha que um link criou sozinho, para o filtro separar
            // do que alguém sentou e planejou
            Field::new("is_auto_campaign", FieldType::Boolean).default_value(json!(false)),
            Field::new("color", FieldType::Integer).default_value(json!(0)),
            Field::new("active", FieldType::Boolean).default_value(json!(true)),
        ],
    )
    .constrained("nome preenchido", &["title"], title_is_filled)
}

/// Separa o nome do contador: `"Email [3]"` → `("Email", 3)`.
///
/// Um nome sem contador vale 1 — é assim que o Odoo o representa, e é o
/// que faz `"Email"` e `"Email [1]"` disputarem a mesma vaga.
fn split_name_and_count(name: &str) -> (&str, u32) {
    let Some(rest) = name.trim_end().strip_suffix(']') else {
        return (name, 1);
    };
    let Some((head, digits)) = rest.rsplit_once('[') else {
        return (name, 1);
    };
    // o colchete tem que vir depois de um espaço: em "A[2]" o número faz
    // parte do nome, não é contador
    if head.trim_end() == head {
        return (name, 1);
    }
    match digits.parse::<u32>() {
        Ok(count) => (head.trim_end(), count),
        Err(_) => (name, 1),
    }
}

/// O nome com que um registro pode nascer sem repetir um que já existe:
/// `"Email"` vira `"Email [2]"` quando `"Email"` está lá (`_get_unique_names`).
///
/// O contador preenche buracos em vez de sempre ir ao fim — com `"Email"`
/// e `"Email [3]"` ocupados, o próximo é `"Email [2]"`.
fn unique_name(wanted: &str, taken: &[String]) -> String {
    let (base, asked) = split_name_and_count(wanted);
    let used: HashSet<u32> = taken
        .iter()
        .filter_map(|name| {
            let (other, count) = split_name_and_count(name);
            // só a mesma família conta: "Email [2]" ocupa o 2 de "Email",
            // "Emails" não ocupa nada
            (other == base).then_some(count)
        })
        .collect();
    let count = if used.contains(&asked) {
        (1u32..).find(|c| !used.contains(c)).unwrap_or(asked)
    } else {
        asked
    };
    if count > 1 {
        format!("{base} [{count}]")
    } else {
        base.to_string()
    }
}

/// `name_create` — criar digitando um nome numa lista suspensa.
///
/// O Odoo numera o nome repetido dentro do `create`; aqui não há gancho
/// de create, então o contador mora neste caminho, que é por onde o
/// cliente cria um registro só com um nome.
fn name_create<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let wanted = wanted_name(&ctx.rest, kwargs)?;
        let id = create_named(&ctx, &wanted, false).await?;
        // a resposta do `name_create` do Odoo: o par que o cliente mostra
        Ok(json!([id, stored_name(&ctx, id).await?]))
    })
}

/// `find_or_create` — o registro que este nome já designa, ou um novo.
///
/// É o `find_or_create_record` do `utm.mixin`: um link chega com
/// `utm_source=Newsletter` e não pode criar uma segunda "Newsletter" a
/// cada clique.
fn find_or_create<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let wanted = wanted_name(&ctx.rest, kwargs)?;
        if let Some(id) = named_exactly(&ctx, &wanted).await? {
            return Ok(json!({"id": id, "name": stored_name(&ctx, id).await?}));
        }
        // criada pelo link, não por alguém: o Odoo marca a campanha assim
        let id = create_named(&ctx, &wanted, true).await?;
        Ok(json!({"id": id, "name": stored_name(&ctx, id).await?}))
    })
}

/// O nome que a chamada pediu, sem os espaços das pontas.
fn wanted_name(args: &[Value], kwargs: &Map<String, Value>) -> Result<String, RusdooError> {
    let raw = args
        .first()
        .or_else(|| kwargs.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if raw.is_empty() {
        return Err(RusdooError::Validation(
            "say the name of the record to create".into(),
        ));
    }
    Ok(raw.to_string())
}

/// O registro cujo nome é exatamente este, sem olhar maiúsculas — e
/// contando os arquivados, que continuam ocupando o nome.
async fn named_exactly(ctx: &MethodCtx<'_>, name: &str) -> Result<Option<i64>, RusdooError> {
    let found = ctx
        .registry
        .search(
            ctx.pool,
            ctx.model,
            &parse_domain(&json!([["name", "=ilike", name]]))?,
            &SearchOptions {
                limit: Some(1),
                active_test: false,
                ..SearchOptions::default()
            },
        )
        .await?;
    Ok(found.first().copied())
}

/// Cria o registro a partir de um nome só, preenchendo o que o modelo
/// exige e o chamador não tem como saber.
async fn create_named(
    ctx: &MethodCtx<'_>,
    wanted: &str,
    from_link: bool,
) -> Result<i64, RusdooError> {
    let model = ctx.registry.get(ctx.model).ok_or_else(|| {
        RusdooError::Validation(format!("unknown model: {model}", model = ctx.model))
    })?;
    let mut values: Vec<(&str, Value)> = Vec::new();
    if model.field("title").is_some() {
        // a campanha guarda o nome em `title`; o identificador vem dele
        values.push(("title", json!(wanted)));
    } else {
        values.push(("name", json!(available_name(ctx, wanted).await?)));
    }
    if model.field("user_id").is_some() {
        // o responsável é quem está criando (`default=lambda: self.env.uid`)
        values.push(("user_id", json!(ctx.uid)));
    }
    if model.field("stage_id").is_some() {
        values.push(("stage_id", json!(first_stage(ctx).await?)));
    }
    if from_link && model.field("is_auto_campaign").is_some() {
        values.push(("is_auto_campaign", json!(true)));
    }
    ctx.registry
        .create_as(ctx.pool, ctx.uid, ctx.model, values)
        .await
}

/// O nome livre mais próximo do pedido, olhando os que já existem.
async fn available_name(ctx: &MethodCtx<'_>, wanted: &str) -> Result<String, RusdooError> {
    let (base, _) = split_name_and_count(wanted);
    let ids = ctx
        .registry
        .search(
            ctx.pool,
            ctx.model,
            &parse_domain(&json!([["name", "ilike", base]]))?,
            &SearchOptions {
                active_test: false,
                ..SearchOptions::default()
            },
        )
        .await?;
    if ids.is_empty() {
        return Ok(unique_name(wanted, &[]));
    }
    let taken: Vec<String> = ctx
        .registry
        .read(ctx.pool, ctx.model, &ids, &["name"])
        .await?
        .iter()
        .filter_map(|row| row.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    Ok(unique_name(wanted, &taken))
}

/// O primeiro estágio, que é o estágio em que uma campanha começa.
///
/// O Odoo resolve isso num `default`; os defaults daqui são valores
/// fixos, e um id não é um valor fixo — então a busca acontece aqui.
async fn first_stage(ctx: &MethodCtx<'_>) -> Result<i64, RusdooError> {
    let found = ctx
        .registry
        .search(
            ctx.pool,
            "utm.stage",
            &parse_domain(&json!([]))?,
            &SearchOptions {
                order: Some("sequence, id".into()),
                limit: Some(1),
                ..SearchOptions::default()
            },
        )
        .await?;
    found.first().copied().ok_or_else(|| {
        RusdooError::Validation("there is no stage at all: create one before creating campaigns".into())
    })
}

/// O nome gravado, que não é necessariamente o pedido: o contador pode
/// ter entrado, e o identificador da campanha é computado.
async fn stored_name(ctx: &MethodCtx<'_>, id: i64) -> Result<String, RusdooError> {
    let rows = ctx
        .registry
        .read(ctx.pool, ctx.model, &[id], &["name"])
        .await?;
    Ok(rows
        .first()
        .and_then(|row| row.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_counter_is_read_off_the_end_of_the_name() {
        assert_eq!(split_name_and_count("Email"), ("Email", 1));
        assert_eq!(split_name_and_count("Email [3]"), ("Email", 3));
        // sem espaço antes do colchete o número é parte do nome
        assert_eq!(split_name_and_count("Email[3]"), ("Email[3]", 1));
        assert_eq!(split_name_and_count("Email [x]"), ("Email [x]", 1));
        assert_eq!(split_name_and_count("[3]"), ("[3]", 1));
    }

    #[test]
    fn a_free_name_is_kept_as_it_was_asked() {
        assert_eq!(unique_name("Email", &[]), "Email");
        // outra família não ocupa vaga nenhuma
        assert_eq!(unique_name("Email", &["Emails".into()]), "Email");
    }

    #[test]
    fn a_taken_name_gets_the_first_free_counter() {
        let taken = vec!["Email".to_string(), "Email [3]".to_string()];
        assert_eq!(unique_name("Email", &taken), "Email [2]");
        // pedir o 3, que está ocupado, cai no primeiro buraco
        assert_eq!(unique_name("Email [3]", &taken), "Email [2]");
        // e pedir um contador livre é atendido como pedido
        assert_eq!(unique_name("Email [9]", &taken), "Email [9]");
    }

    #[test]
    fn the_models_register_on_top_of_base() {
        let mut reg = rusdoo_base::registry().expect("base registra");
        extend(&mut reg).expect("utm registra");
        for name in [
            "utm.stage",
            "utm.tag",
            "utm.medium",
            "utm.source",
            "utm.campaign",
        ] {
            assert!(reg.get(name).is_some(), "{name} must be registered");
        }
        // o identificador é materializado: uma lista de campanhas lê coluna
        let campaign = reg.get("utm.campaign").expect("campanha registrada");
        let name = campaign.field("name").expect("campanha tem identificador");
        assert!(name.stored && name.compute.is_some());
        // e é o `title` que alguém digita, então é ele que é obrigatório
        assert!(campaign.field("title").expect("has a title").required);
        assert!(!name.required, "the identifier is filled by the recompute");
    }

    #[test]
    fn the_three_tracked_models_can_be_created_from_a_name() {
        let mut methods = MethodRegistry::new();
        extend_methods(&mut methods).expect("methods register");
        for model in TRACKING_MODELS {
            assert_eq!(
                methods.names_for(model),
                vec!["find_or_create", "name_create"],
                "{model}"
            );
        }
        // a etiqueta e o estágio não são rastreados por link nenhum
        assert!(methods.names_for("utm.tag").is_empty());
    }
}
