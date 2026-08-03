//! rusdoo-onboarding — port de `odoo/addons/onboarding/models/`: a
//! lista de passos que um banco recém-instalado mostra até estar
//! configurado.
//!
//! São quatro modelos e uma separação só: o painel
//! (`onboarding.onboarding`) e seus passos são a definição, igual para
//! todo mundo e instalada com o módulo; o progresso
//! (`onboarding.progress` e `onboarding.progress.step`) é o que cada
//! empresa já fez. É essa separação que deixa duas empresas no mesmo
//! banco terem o mesmo checklist em pontos diferentes — apagar o
//! progresso não apaga o painel, e instalar o painel de novo não zera
//! ninguém.
//!
//! Desvio do Odoo: não há `Environment` com contexto aqui, então "a
//! empresa atual" é a do usuário que está chamando — que é de onde o
//! Odoo tira o padrão de `self.env.company`. Fica de fora o painel em
//! si: a rota `/onboarding/<route_name>` e o template que a desenha são
//! um controller e um asset de website, e nada disso existe no port.

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::parse_domain;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};

/// A tabela de relação entre um painel e seus passos. As duas pontas
/// precisam nomear a mesma tabela, senão cada lado enxerga um vínculo
/// diferente do outro.
const PANEL_STEP_REL: &str = "onboarding_onboarding_step_rel";

/// Idem para o progresso: um registro de progresso de passo pode servir
/// a mais de um painel, porque o mesmo passo pode estar em vários.
const PROGRESS_STEP_REL: &str = "onboarding_progress_step_rel";

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

/// Os três estados por que um passo passa.
///
/// `just_done` não é enfeite: é o que deixa o painel comemorar uma vez
/// só. Quem lê o painel consolida o `just_done` em `done`, então a
/// animação de "acabou de ficar pronto" não volta a cada carregamento.
fn progress_states() -> FieldType {
    FieldType::Selection(vec![
        ("not_done".into(), "Pending".into()),
        ("just_done".into(), "Just done".into()),
        ("done".into(), "Done".into()),
    ])
}

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    // o painel antes do passo: o passo aponta de volta para ele
    reg.register(onboarding())?;
    reg.register(step())?;
    reg.register(progress())?;
    reg.register(progress_step())?;
    Ok(())
}

/// O que se clica num onboarding: fechar o painel, marcar um passo, e
/// ler o estado de tudo.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    methods.register(
        "onboarding.onboarding",
        "action_open_progress",
        Operation::Write,
        action_open_progress,
    )?;
    methods.register(
        "onboarding.onboarding",
        "action_close",
        Operation::Write,
        action_close,
    )?;
    methods.register(
        "onboarding.onboarding",
        "action_toggle_visibility",
        Operation::Write,
        action_toggle_visibility,
    )?;
    methods.register(
        "onboarding.onboarding.step",
        "action_set_just_done",
        Operation::Write,
        action_set_just_done,
    )?;
    // consolida o `just_done` enquanto responde, e por isso escreve
    methods.register(
        "onboarding.progress",
        "action_step_states",
        Operation::Write,
        action_step_states,
    )?;
    Ok(())
}

// ---------------------------------------------------------------- modelos

/// Um painel é "por empresa" quando algum passo dele é.
///
/// O Odoo também olha o progresso já criado e, uma vez por empresa,
/// nunca mais volta atrás — assim ele não precisa fundir progressos
/// existentes. Aqui a resposta segue só os passos, o que é mais simples
/// de explicar; o preço é que tornar um passo comum a todas as empresas
/// deixa o progresso antigo, gravado com dono, órfão.
fn is_per_company(record: &Map<String, Value>) -> Value {
    let per_company = record
        .get("step_ids.is_per_company")
        .and_then(Value::as_array)
        .is_some_and(|flags| flags.iter().any(|flag| flag.as_bool() == Some(true)));
    json!(per_company)
}

/// O nome de rota vira um pedaço de URL (`/onboarding/<route_name>`), e
/// um pedaço de URL com espaço não é um pedaço de URL.
fn route_name_is_one_word(record: &Map<String, Value>) -> Result<(), String> {
    let route = record
        .get("route_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if route.is_empty() {
        return Err("the dashboard needs a route name".into());
    }
    if route.split_whitespace().count() > 1 {
        return Err(format!(
            "route name {route:?} must be a single word, with no spaces"
        ));
    }
    Ok(())
}

/// `onboarding.onboarding` — o painel: um título, uma rota e os passos.
fn onboarding() -> Model {
    Model::new(
        meta("onboarding.onboarding", "onboarding_onboarding"),
        vec![
            char("name"),
            char("route_name").required(),
            Field::new(
                "step_ids",
                FieldType::Many2many {
                    comodel: "onboarding.onboarding.step".into(),
                    relation: PANEL_STEP_REL.into(),
                    column1: "onboarding_id".into(),
                    column2: "step_id".into(),
                },
            ),
            char("text_completed")
                .default_value(json!("Well done! The setup is ready.")),
            // o método que o painel chama ao ser fechado, nomeado pelo
            // módulo que o instalou
            char("panel_close_action_name"),
            Field::new(
                "progress_ids",
                FieldType::One2many {
                    comodel: "onboarding.progress".into(),
                    inverse: "onboarding_id".into(),
                },
            ),
            Field::new("is_per_company", FieldType::Boolean)
                .computed(&["step_ids.is_per_company"], is_per_company),
            Field::new("sequence", FieldType::Integer).default_value(json!(10)),
        ],
    )
    .constrained(
        "single-word route",
        &["route_name"],
        route_name_is_one_word,
    )
}

/// Um passo pendurado num painel sem opening action é um botão que
/// não leva a lugar nenhum.
fn step_on_panel_has_action(record: &Map<String, Value>) -> Result<(), String> {
    let on_panel = record
        .get("onboarding_ids")
        .and_then(Value::as_array)
        .is_some_and(|panels| !panels.is_empty());
    let has_action = record
        .get("panel_step_open_action_name")
        .and_then(Value::as_str)
        .is_some_and(|action| !action.trim().is_empty());
    if on_panel && !has_action {
        let title = record.get("title").and_then(Value::as_str).unwrap_or("");
        return Err(format!(
            "step {title:?} is on a dashboard and needs an opening action"
        ));
    }
    Ok(())
}

/// `onboarding.onboarding.step` — um passo, e como ele se apresenta.
fn step() -> Model {
    Model::new(
        meta("onboarding.onboarding.step", "onboarding_onboarding_step"),
        vec![
            Field::new(
                "onboarding_ids",
                FieldType::Many2many {
                    comodel: "onboarding.onboarding".into(),
                    relation: PANEL_STEP_REL.into(),
                    column1: "step_id".into(),
                    column2: "onboarding_id".into(),
                },
            ),
            char("title"),
            char("description"),
            char("button_text")
                .required()
                .default_value(json!("Let's go")),
            char("done_icon").default_value(json!("fa-star")),
            char("done_text").default_value(json!("Step done!")),
            // o método que o passo abre quando se clica nele
            char("panel_step_open_action_name"),
            Field::new(
                "progress_ids",
                FieldType::One2many {
                    comodel: "onboarding.progress.step".into(),
                    inverse: "step_id".into(),
                },
            ),
            // o padrão do Odoo: configurar é quase sempre configurar uma
            // empresa, não o banco inteiro
            Field::new("is_per_company", FieldType::Boolean).default_value(json!(true)),
            Field::new("sequence", FieldType::Integer).default_value(json!(10)),
        ],
    )
    .constrained(
        "a dashboard step needs an action",
        &["onboarding_ids", "panel_step_open_action_name", "title"],
        step_on_panel_has_action,
    )
}

/// Quantos passos o painel deste progresso tem.
///
/// A dependência atravessa duas relações — o painel, e a lista de passos
/// dele — e o ORM devolve uma lista por registro percorrido. Daí a lista
/// dentro da lista.
fn step_count(record: &Map<String, Value>) -> usize {
    record
        .get("onboarding_id.step_ids")
        .and_then(Value::as_array)
        .map(|panels| {
            panels
                .iter()
                .map(|steps| steps.as_array().map_or(0, Vec::len))
                .sum()
        })
        .unwrap_or(0)
}

/// Quantos passos já saíram do "pendente".
fn done_count(record: &Map<String, Value>) -> usize {
    record
        .get("progress_step_ids.step_state")
        .and_then(Value::as_array)
        .map(|states| {
            states
                .iter()
                .filter(|state| matches!(state.as_str(), Some("just_done" | "done")))
                .count()
        })
        .unwrap_or(0)
}

/// O painel está pronto quando não sobrou passo pendente.
///
/// Um painel sem passo nenhum já nasce pronto: não há o que fazer. E a
/// comparação é `>=`, não `==`, porque um passo retirado do painel
/// depois de feito deixaria o progresso pendente para sempre.
fn onboarding_state(record: &Map<String, Value>) -> Value {
    if done_count(record) >= step_count(record) {
        json!("done")
    } else {
        json!("not_done")
    }
}

/// `onboarding.progress` — o que uma empresa já fez de um painel.
fn progress() -> Model {
    Model::new(
        meta("onboarding.progress", "onboarding_progress"),
        vec![
            m2o("onboarding_id", "onboarding.onboarding").required(),
            // vazio quando o painel vale para o banco inteiro
            m2o("company_id", "res.company"),
            Field::new("is_onboarding_closed", FieldType::Boolean).default_value(json!(false)),
            Field::new(
                "progress_step_ids",
                FieldType::Many2many {
                    comodel: "onboarding.progress.step".into(),
                    relation: PROGRESS_STEP_REL.into(),
                    column1: "progress_id".into(),
                    column2: "progress_step_id".into(),
                },
            ),
            // materializado: a lista de painéis mostra o estado de cada
            // um, e recalcular isso por linha seria uma consulta por
            // linha
            Field::new("onboarding_state", progress_states())
                .computed(
                    &["progress_step_ids.step_state", "onboarding_id.step_ids"],
                    onboarding_state,
                )
                .store(),
        ],
    )
}

/// `onboarding.progress.step` — o estado de um passo para uma empresa.
fn progress_step() -> Model {
    Model::new(
        meta("onboarding.progress.step", "onboarding_progress_step"),
        vec![
            m2o("step_id", "onboarding.onboarding.step").required(),
            m2o("company_id", "res.company"),
            Field::new("step_state", progress_states()).default_value(json!("not_done")),
            Field::new(
                "progress_ids",
                FieldType::Many2many {
                    comodel: "onboarding.progress".into(),
                    relation: PROGRESS_STEP_REL.into(),
                    column1: "progress_step_id".into(),
                    column2: "progress_id".into(),
                },
            ),
        ],
    )
}

// ---------------------------------------------------------------- métodos

/// O id dentro de um many2one, que lê como `[id, nome]`.
fn first_id(value: &Value) -> Option<i64> {
    match value {
        Value::Array(items) => items.first().and_then(Value::as_i64),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

/// Os ids de um campo x2many.
fn ids_of(record: &Map<String, Value>, name: &str) -> Vec<i64> {
    record
        .get(name)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

/// A empresa de quem está chamando — o port de `self.env.company`.
///
/// Sem contexto na chamada, a empresa é a do usuário: é dela que o Odoo
/// parte quando ninguém disse outra coisa. Um usuário sem empresa
/// devolve `None`, e aí o progresso vale para o banco inteiro.
async fn current_company(ctx: &MethodCtx<'_>) -> Result<Option<i64>, RusdooError> {
    let rows = ctx
        .registry
        .read(ctx.pool, "res.users", &[ctx.uid], &["company_id"])
        .await?;
    Ok(rows
        .first()
        .and_then(|row| row.get("company_id"))
        .and_then(first_id))
}

/// Regra do Odoo: um registro de progresso serve à empresa atual quando
/// é dela, ou quando não é de ninguém (painel comum a todas).
fn serves_company(record: &Map<String, Value>, company: Option<i64>) -> bool {
    match record.get("company_id").and_then(first_id) {
        None => true,
        Some(owner) => Some(owner) == company,
    }
}

/// O progresso deste painel para `company`, criado se ainda não existe —
/// o `_search_or_create_progress` do Odoo.
async fn progress_for(
    ctx: &MethodCtx<'_>,
    onboarding: i64,
    company: Option<i64>,
) -> Result<i64, RusdooError> {
    let found = ctx
        .registry
        .search(
            ctx.pool,
            "onboarding.progress",
            &parse_domain(&json!([["onboarding_id", "=", onboarding]]))?,
            &SearchOptions::default(),
        )
        .await?;
    if !found.is_empty() {
        let rows = ctx
            .registry
            .read(ctx.pool, "onboarding.progress", &found, &["company_id"])
            .await?;
        let existing = rows
            .iter()
            .find(|row| serves_company(row, company))
            .and_then(|row| row.get("id"))
            .and_then(Value::as_i64);
        if let Some(id) = existing {
            return Ok(id);
        }
    }
    // um painel cujos passos valem para o banco inteiro tem um progresso
    // só: gravar a empresa nele partiria o mesmo checklist em dois
    let panel = ctx
        .registry
        .read(
            ctx.pool,
            "onboarding.onboarding",
            &[onboarding],
            &["is_per_company"],
        )
        .await?;
    let per_company = panel
        .first()
        .and_then(|row| row.get("is_per_company"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut values = vec![("onboarding_id", json!(onboarding))];
    if let Some(company) = company.filter(|_| per_company) {
        values.push(("company_id", json!(company)));
    }
    ctx.registry
        .create_as(ctx.pool, ctx.uid, "onboarding.progress", values)
        .await
}

/// O registro de progresso deste passo para `company`, com o estado em
/// que ele está. `None` quando ninguém encostou no passo ainda — o que é
/// diferente de um passo pendente e por isso não se inventa uma linha
/// para responder.
async fn find_progress_step(
    ctx: &MethodCtx<'_>,
    step: i64,
    company: Option<i64>,
) -> Result<Option<(i64, String)>, RusdooError> {
    let found = ctx
        .registry
        .search(
            ctx.pool,
            "onboarding.progress.step",
            &parse_domain(&json!([["step_id", "=", step]]))?,
            &SearchOptions::default(),
        )
        .await?;
    if found.is_empty() {
        return Ok(None);
    }
    let rows = ctx
        .registry
        .read(
            ctx.pool,
            "onboarding.progress.step",
            &found,
            &["company_id", "step_state"],
        )
        .await?;
    Ok(rows
        .iter()
        .find(|row| serves_company(row, company))
        .and_then(|row| {
            let id = row.get("id")?.as_i64()?;
            let state = row
                .get("step_state")
                .and_then(Value::as_str)
                .unwrap_or("not_done")
                .to_string();
            Some((id, state))
        }))
}

/// Recoloca em `progress` os registros de progresso dos passos do painel
/// — o `_recompute_progress_step_ids` do Odoo.
///
/// Escrever o m2m é também o que manda o ORM recalcular o estado do
/// painel: o estado depende dessa lista, e um estado gravado que ninguém
/// recalcula é pior do que nenhum.
async fn relink_progress_steps(
    ctx: &MethodCtx<'_>,
    progress: i64,
    onboarding: i64,
    company: Option<i64>,
) -> Result<(), RusdooError> {
    let panel = ctx
        .registry
        .read(
            ctx.pool,
            "onboarding.onboarding",
            &[onboarding],
            &["step_ids"],
        )
        .await?;
    let steps = panel
        .first()
        .map(|row| ids_of(row, "step_ids"))
        .unwrap_or_default();
    let mut linked: Vec<Value> = Vec::new();
    for step in steps {
        if let Some((id, _)) = find_progress_step(ctx, step, company).await? {
            linked.push(json!(id));
        }
    }
    ctx.registry
        .write_as(
            ctx.pool,
            ctx.uid,
            "onboarding.progress",
            &[progress],
            vec![("progress_step_ids", json!([[6, 0, linked]]))],
        )
        .await
}

/// `action_set_just_done` — o passo foi feito.
///
/// Marca um passo de cada vez porque é assim que o painel funciona: um
/// clique, um passo. Um passo já feito não é marcado de novo — dizer
/// "it was already done" é a resposta certa, não um erro.
fn action_set_just_done<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [step_id] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "mark one step at a time".into(),
            ));
        };
        let rows = ctx
            .registry
            .read(
                ctx.pool,
                "onboarding.onboarding.step",
                &[step_id],
                &["is_per_company", "onboarding_ids"],
            )
            .await?;
        let step = rows
            .first()
            .ok_or_else(|| RusdooError::Validation(format!("step {step_id} does not exist")))?;
        // um passo comum a todas as empresas tem um registro só, sem dono
        let per_company = step
            .get("is_per_company")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let company = if per_company {
            current_company(&ctx).await?
        } else {
            None
        };

        let existing = find_progress_step(&ctx, step_id, company).await?;
        let just_done = match &existing {
            Some((id, state)) if state == "not_done" => {
                ctx.registry
                    .write_as(
                        ctx.pool,
                        ctx.uid,
                        "onboarding.progress.step",
                        &[*id],
                        vec![("step_state", json!("just_done"))],
                    )
                    .await?;
                true
            }
            Some(_) => false,
            None => {
                let mut values = vec![
                    ("step_id", json!(step_id)),
                    ("step_state", json!("just_done")),
                ];
                if let Some(company) = company {
                    values.push(("company_id", json!(company)));
                }
                ctx.registry
                    .create_as(ctx.pool, ctx.uid, "onboarding.progress.step", values)
                    .await?;
                true
            }
        };

        // o progresso de cada painel que usa este passo passa a apontar
        // para o registro que acabou de existir
        for onboarding in ids_of(step, "onboarding_ids") {
            let progress = progress_for(&ctx, onboarding, company).await?;
            relink_progress_steps(&ctx, progress, onboarding, company).await?;
        }
        Ok(json!(if just_done { "just_done" } else { "was_done" }))
    })
}

/// `action_open_progress` — abrir o progresso deste painel.
///
/// Criar o registro aqui é o que garante que a tela abre apontando para
/// alguma coisa, mesmo na primeira vez que alguém olha o painel.
fn action_open_progress<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let progress = progress_of_single_panel(&ctx).await?;
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": "Progresso",
            "res_model": "onboarding.progress",
            "res_id": progress,
            "views": [[false, "form"]],
            "target": "current",
        }))
    })
}

/// O progresso do painel em `ctx.ids`, para a empresa de quem chamou.
async fn progress_of_single_panel(ctx: &MethodCtx<'_>) -> Result<i64, RusdooError> {
    let [onboarding] = ctx.ids[..] else {
        return Err(RusdooError::Validation("open one dashboard at a time".into()));
    };
    let panel = ctx
        .registry
        .read(ctx.pool, "onboarding.onboarding", &[onboarding], &["name"])
        .await?;
    if panel.is_empty() {
        return Err(RusdooError::Validation(format!(
            "dashboard {onboarding} does not exist"
        )));
    }
    let company = current_company(ctx).await?;
    progress_for(ctx, onboarding, company).await
}

/// `action_close` — o usuário não quer mais ver este painel.
fn action_close<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let progress = progress_of_single_panel(&ctx).await?;
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "onboarding.progress",
                &[progress],
                vec![("is_onboarding_closed", json!(true))],
            )
            .await?;
        Ok(json!(true))
    })
}

/// `action_toggle_visibility` — mostrar de novo o painel fechado, ou
/// fechar o aberto. É o botão de quem quer rever a configuração.
fn action_toggle_visibility<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let progress = progress_of_single_panel(&ctx).await?;
        let rows = ctx
            .registry
            .read(
                ctx.pool,
                "onboarding.progress",
                &[progress],
                &["is_onboarding_closed"],
            )
            .await?;
        let closed = rows
            .first()
            .and_then(|row| row.get("is_onboarding_closed"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "onboarding.progress",
                &[progress],
                vec![("is_onboarding_closed", json!(!closed))],
            )
            .await?;
        Ok(json!(!closed))
    })
}

/// `action_step_states` — o estado de cada passo, para desenhar o painel.
///
/// É o `_get_and_update_onboarding_state` do Odoo, e como lá ele
/// consolida os `just_done` enquanto responde: quem perguntou já viu a
/// comemoração, e ela não se repete na próxima leitura. Por isso o
/// método escreve, e declara que escreve.
fn action_step_states<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [progress_id] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "read one progress record at a time".into(),
            ));
        };
        let rows = ctx
            .registry
            .read(
                ctx.pool,
                "onboarding.progress",
                &[progress_id],
                &[
                    "onboarding_id",
                    "company_id",
                    "is_onboarding_closed",
                    "onboarding_state",
                ],
            )
            .await?;
        let progress = rows.first().ok_or_else(|| {
            RusdooError::Validation(format!("progress record {progress_id} does not exist"))
        })?;
        let onboarding = progress
            .get("onboarding_id")
            .and_then(first_id)
            .ok_or_else(|| {
                RusdooError::Validation("the progress record points at no dashboard".into())
            })?;
        // a empresa do próprio progresso; quando ele vale para o banco
        // inteiro, os passos por empresa ainda são lidos da empresa de
        // quem está olhando
        let company = match progress.get("company_id").and_then(first_id) {
            Some(company) => Some(company),
            None => current_company(&ctx).await?,
        };

        let panel = ctx
            .registry
            .read(
                ctx.pool,
                "onboarding.onboarding",
                &[onboarding],
                &["step_ids"],
            )
            .await?;
        let steps = panel
            .first()
            .map(|row| ids_of(row, "step_ids"))
            .unwrap_or_default();

        // percorre os passos do painel, e não o progresso: um passo que
        // ninguém começou não tem registro de progresso e mesmo assim
        // precisa aparecer como pendente
        let mut states = Map::new();
        let mut to_consolidate: Vec<i64> = Vec::new();
        for step in steps {
            let state = match find_progress_step(&ctx, step, company).await? {
                Some((id, state)) => {
                    if state == "just_done" {
                        to_consolidate.push(id);
                    }
                    state
                }
                None => "not_done".to_string(),
            };
            states.insert(step.to_string(), json!(state));
        }
        if !to_consolidate.is_empty() {
            ctx.registry
                .write_as(
                    ctx.pool,
                    ctx.uid,
                    "onboarding.progress.step",
                    &to_consolidate,
                    vec![("step_state", json!("done"))],
                )
                .await?;
        }

        let closed = progress
            .get("is_onboarding_closed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let done = progress.get("onboarding_state").and_then(Value::as_str) == Some("done");
        let panel_state = if closed {
            "closed"
        } else if done && !to_consolidate.is_empty() {
            // ficou pronto agora: o painel comemora uma vez
            "just_done"
        } else if done {
            "done"
        } else {
            "not_done"
        };
        Ok(json!({"onboarding_state": panel_state, "steps": states}))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panel_is_done_when_no_step_is_pending() {
        let mut record = Map::new();
        record.insert("onboarding_id.step_ids".into(), json!([[1, 2, 3]]));
        record.insert(
            "progress_step_ids.step_state".into(),
            json!(["done", "just_done"]),
        );
        assert_eq!(onboarding_state(&record), json!("not_done"));

        record.insert(
            "progress_step_ids.step_state".into(),
            json!(["done", "just_done", "not_done"]),
        );
        assert_eq!(
            onboarding_state(&record),
            json!("not_done"),
            "um passo pendente segura o painel inteiro"
        );

        record.insert(
            "progress_step_ids.step_state".into(),
            json!(["done", "done", "just_done"]),
        );
        assert_eq!(onboarding_state(&record), json!("done"));

        // um painel sem passo nenhum não tem o que esperar
        assert_eq!(onboarding_state(&Map::new()), json!("done"));
    }

    #[test]
    fn a_panel_follows_its_steps_on_being_per_company() {
        let mut record = Map::new();
        record.insert("step_ids.is_per_company".into(), json!([false, false]));
        assert_eq!(is_per_company(&record), json!(false));
        record.insert("step_ids.is_per_company".into(), json!([false, true]));
        assert_eq!(is_per_company(&record), json!(true));
        assert_eq!(is_per_company(&Map::new()), json!(false));
    }

    #[test]
    fn a_route_name_with_a_space_is_refused() {
        let mut record = Map::new();
        record.insert("route_name".into(), json!("sale"));
        assert!(route_name_is_one_word(&record).is_ok());
        record.insert("route_name".into(), json!("pedido de venda"));
        let error = route_name_is_one_word(&record).expect_err("two words do not make a route");
        assert!(error.contains("a single word"), "{error}");
        record.insert("route_name".into(), json!("   "));
        assert!(route_name_is_one_word(&record).is_err());
    }

    #[test]
    fn a_step_on_a_panel_needs_an_opening_action() {
        let mut record = Map::new();
        record.insert("title".into(), json!("Cadastrar a empresa"));
        record.insert("onboarding_ids".into(), json!([]));
        assert!(
            step_on_panel_has_action(&record).is_ok(),
            "outside a dashboard a step opens nothing"
        );
        record.insert("onboarding_ids".into(), json!([1]));
        let error = step_on_panel_has_action(&record).expect_err("a button with no target");
        assert!(error.contains("opening action"), "{error}");
        record.insert("panel_step_open_action_name".into(), json!("action_open"));
        assert!(step_on_panel_has_action(&record).is_ok());
    }

    #[test]
    fn the_models_register_on_top_of_base() {
        let mut reg = rusdoo_base::registry().unwrap();
        extend(&mut reg).unwrap();
        for name in [
            "onboarding.onboarding",
            "onboarding.onboarding.step",
            "onboarding.progress",
            "onboarding.progress.step",
        ] {
            assert!(reg.get(name).is_some(), "{name} must be registered");
        }
        // o estado do painel é uma coluna: uma lista de painéis não
        // recalcula um por linha
        let state = reg
            .get("onboarding.progress")
            .unwrap()
            .field("onboarding_state")
            .unwrap();
        assert!(state.stored);
        // um progresso sem painel não é progresso de nada
        assert!(
            reg.get("onboarding.progress")
                .unwrap()
                .field("onboarding_id")
                .unwrap()
                .required
        );
    }

    #[test]
    fn the_panel_has_its_buttons() {
        let mut methods = MethodRegistry::new();
        extend_methods(&mut methods).unwrap();
        assert_eq!(
            methods.names_for("onboarding.onboarding"),
            vec![
                "action_close",
                "action_open_progress",
                "action_toggle_visibility"
            ]
        );
        assert_eq!(
            methods.names_for("onboarding.onboarding.step"),
            vec!["action_set_just_done"]
        );
        assert_eq!(
            methods.names_for("onboarding.progress"),
            vec!["action_step_states"]
        );
    }
}
