//! rusdoo-account-debit-note — port de `odoo/addons/account_debit_note/`:
//! a nota de débito.
//!
//! Uma nota de débito cobra a mais sobre uma fatura já lançada. Ela não
//! é uma fatura solta: guarda o vínculo com o documento que corrigiu, e
//! é por esse vínculo que alguém, meses depois, entende por que o mesmo
//! cliente foi cobrado duas vezes. Cancelar a fatura e emitir outra
//! apagaria essa história — é justamente o que a nota existe para evitar.
//!
//! O caminho é o do Odoo: um assistente recebe as faturas escolhidas,
//! pergunta a data, o motivo e se as linhas devem vir junto, e devolve a
//! ação que abre o que criou.

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::fields::{Field, FieldType};
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use serde_json::{json, Map, Value};

/// A série própria das notas de débito (`ir.sequence`), para que uma
/// nota não consuma um número da série das faturas: quem confere a
/// numeração de um mês não deve encontrar buracos porque houve
/// correções. É o que o Odoo faz prefixando "D" no número quando o
/// diário está marcado com `debit_sequence` — o padrão dos diários de
/// venda e de compra.
const DEBIT_SEQUENCE: &str = "account.move.debit";

/// O que se debita. O Odoo aceita também nota de crédito (`out_refund`,
/// `in_refund`), tipos que o `account.move` do port ainda não tem.
const DEBITABLE: [&str; 2] = ["out_invoice", "in_invoice"];

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

/// O id dentro de um many2one, que se lê como `[id, nome]`.
fn first_id(value: &Value) -> Option<i64> {
    match value {
        Value::Array(items) => items.first().and_then(Value::as_i64),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

/// Hoje, como a data viaja no protocolo (`YYYY-MM-DD`).
///
/// O Odoo usa o fuso do usuário (`fields.Date.context_today`); o port
/// ainda não tem contexto, então é UTC — e quem abre o assistente vê a
/// data e pode trocá-la.
fn today() -> String {
    chrono::Utc::now().date_naive().to_string()
}

/// `debit_note_count` — quantas notas de débito saíram desta fatura.
fn debit_note_count(record: &Map<String, Value>) -> Value {
    let notes = record
        .get("debit_note_ids")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    json!(notes)
}

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(debited_move())?;
    reg.register(debit_note_wizard())?;
    Ok(())
}

/// Os botões da nota de débito: os dois na fatura e o do diálogo.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    // abrir um diálogo e listar não mexem na fatura: pedem leitura
    methods.register(
        "account.move",
        "action_debit_note",
        Operation::Read,
        action_debit_note,
    )?;
    methods.register(
        "account.move",
        "action_view_debit_notes",
        Operation::Read,
        action_view_debit_notes,
    )?;
    methods.register(
        "account.debit.note",
        "create_debit",
        Operation::Write,
        create_debit,
    )?;
    Ok(())
}

/// `account.move` estendida (`_inherit`): a fatura ganha para onde
/// aponta e o que saiu dela.
fn debited_move() -> Model {
    Model::new(
        ModelMeta {
            name: "account.move".into(),
            table: "account_move".into(),
            inherit: vec!["account.move".into()],
            inherits: vec![],
        },
        vec![
            // não é `readonly` aqui, ao contrário do Odoo: neste ORM
            // `readonly` proíbe a escrita, e é o assistente que preenche
            // o vínculo. Quem protege o campo na tela é o arch do form.
            m2o("debit_origin_id", "account.move"),
            Field::new(
                "debit_note_ids",
                FieldType::One2many {
                    comodel: "account.move".into(),
                    inverse: "debit_origin_id".into(),
                },
            ),
            // não materializado: a contagem muda quando OUTRO registro
            // grava seu `debit_origin_id`, e o recompute só acompanha os
            // campos de quem está sendo gravado — uma coluna aqui
            // envelheceria calada
            Field::new("debit_note_count", FieldType::Integer)
                .computed(&["debit_note_ids"], debit_note_count),
        ],
    )
}

/// `account.debit.note` — o diálogo que emite a nota.
///
/// Um `TransientModel`: a linha é o estado de um diálogo aberto, não
/// algo que o negócio guarda. O many2many é o que permite debitar um
/// lote de faturas de uma vez, como no Odoo.
fn debit_note_wizard() -> Model {
    Model::new(
        ModelMeta {
            name: "account.debit.note".into(),
            table: "account_debit_note".into(),
            inherit: vec![],
            inherits: vec![],
        },
        vec![
            Field::new(
                "move_ids",
                FieldType::Many2many {
                    comodel: "account.move".into(),
                    relation: "account_move_debit_move".into(),
                    column1: "debit_id".into(),
                    column2: "move_id".into(),
                },
            ),
            Field::new("date", FieldType::Date).required(),
            char("reason"),
            // desmarcado por padrão: a nota costuma cobrar um item novo
            // (um frete esquecido), não repetir o que já foi cobrado
            Field::new("copy_lines", FieldType::Boolean).default_value(json!(false)),
        ],
    )
    .transient()
}

/// Por que esta fatura não vira nota de débito.
///
/// Vale nas duas pontas: ao abrir o diálogo, para ninguém preencher um
/// formulário que vai falhar; e ao criar, porque entre uma coisa e
/// outra a fatura pode ter sido cancelada.
fn refuse_undebitable(row: &Map<String, Value>) -> Result<(), RusdooError> {
    let name = row.get("name").and_then(Value::as_str).unwrap_or("");
    let state = row.get("state").and_then(Value::as_str).unwrap_or("draft");
    if state != "posted" {
        return Err(RusdooError::Validation(format!(
            "a fatura {name} está em {state:?}: lance a fatura antes de emitir a nota de débito"
        )));
    }
    if row.get("debit_origin_id").and_then(first_id).is_some() {
        return Err(RusdooError::Validation(format!(
            "{name} já é uma nota de débito: debite a fatura de origem, não a nota"
        )));
    }
    let move_type = row.get("move_type").and_then(Value::as_str).unwrap_or("");
    if !DEBITABLE.contains(&move_type) {
        return Err(RusdooError::Validation(format!(
            "{name} é do tipo {move_type:?}: só fatura de cliente ou de fornecedor vira nota de débito"
        )));
    }
    Ok(())
}

/// `action_debit_note` — abre o diálogo já apontando as faturas.
///
/// O Odoo passa a seleção pelo contexto (`active_ids`) e a lê no
/// `default_get`; aqui o registro do assistente nasce apontando, como no
/// assistente de cancelamento de `sale`. É a mesma decisão tomada onde a
/// fatura já está, e o diálogo não tem como abrir apontando para nada.
fn action_debit_note<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation(
                "escolha ao menos uma fatura para debitar".into(),
            ));
        }
        let moves = ctx
            .registry
            .read(
                ctx.pool,
                "account.move",
                &ctx.ids,
                &["name", "state", "move_type", "debit_origin_id"],
            )
            .await?;
        for row in &moves {
            refuse_undebitable(row)?;
        }
        let wizard = ctx
            .registry
            .create_as(
                ctx.pool,
                ctx.uid,
                "account.debit.note",
                vec![
                    ("move_ids", json!([[6, 0, ctx.ids]])),
                    ("date", json!(today())),
                ],
            )
            .await?;
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": "Nota de débito",
            "res_model": "account.debit.note",
            "res_id": wizard,
            "views": [[false, "form"]],
            // um diálogo sobre a fatura, não uma tela que a substitui
            "target": "new",
        }))
    })
}

/// `action_view_debit_notes` — as notas que saíram desta fatura.
fn action_view_debit_notes<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [move_id] = ctx.ids[..] else {
            return Err(RusdooError::Validation(
                "veja as notas de uma fatura de cada vez".into(),
            ));
        };
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": "Notas de débito",
            "res_model": "account.move",
            "view_mode": "list,form",
            "domain": [["debit_origin_id", "=", move_id]],
            "target": "current",
        }))
    })
}

/// `create_debit` — o botão do diálogo: emite a nota de cada fatura.
fn create_debit<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let [wizard_id] = ctx.ids[..] else {
            return Err(RusdooError::Validation("o assistente sumiu".into()));
        };
        let rows = ctx
            .registry
            .read(
                ctx.pool,
                "account.debit.note",
                &[wizard_id],
                &["move_ids", "date", "reason", "copy_lines"],
            )
            .await?;
        let wizard = rows
            .first()
            .ok_or_else(|| RusdooError::Validation("o assistente sumiu".into()))?;
        let move_ids: Vec<i64> = wizard
            .get("move_ids")
            .and_then(Value::as_array)
            .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        if move_ids.is_empty() {
            return Err(RusdooError::Validation(
                "o assistente não aponta nenhuma fatura: não há o que debitar".into(),
            ));
        }
        // a data da nota é a do assistente, não a da fatura debitada: a
        // cobrança nasce hoje, ainda que corrija um documento antigo
        let date = wizard
            .get("date")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| RusdooError::Validation("diga em que data a nota é emitida".into()))?;
        let reason = wizard
            .get("reason")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .map(str::to_string);
        let copy_lines = wizard
            .get("copy_lines")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let moves = ctx
            .registry
            .read(
                ctx.pool,
                "account.move",
                &move_ids,
                &[
                    "name",
                    "state",
                    "move_type",
                    "debit_origin_id",
                    "partner_id",
                    "company_id",
                    "line_ids",
                ],
            )
            .await?;
        // uma fatura apagada enquanto o diálogo estava aberto sumiria da
        // leitura, e o lote sairia menor do que quem clicou pediu
        if moves.len() != move_ids.len() {
            return Err(RusdooError::Validation(
                "uma das faturas escolhidas não existe mais: reabra o assistente".into(),
            ));
        }
        // tudo conferido antes de criar qualquer coisa: um lote que para
        // no meio deixa notas emitidas e um erro na tela, e ninguém sabe
        // quais saíram
        for row in &moves {
            refuse_undebitable(row)?;
        }

        let mut created = Vec::with_capacity(moves.len());
        for row in &moves {
            created.push(emit_note(&ctx, row, &date, reason.as_deref(), copy_lines).await?);
        }

        // uma só nota abre direto; um lote abre a lista do que saiu
        if let [only] = created[..] {
            return Ok(json!({
                "type": "ir.actions.act_window",
                "name": "Nota de débito",
                "res_model": "account.move",
                "res_id": only,
                "views": [[false, "form"]],
                "target": "current",
            }));
        }
        Ok(json!({
            "type": "ir.actions.act_window",
            "name": "Notas de débito",
            "res_model": "account.move",
            "view_mode": "list,form",
            "domain": [["id", "in", created]],
            "target": "current",
        }))
    })
}

/// A nota de débito de uma fatura, devolvendo o id do que foi criado.
///
/// Ela nasce rascunho, como qualquer documento: quem emitiu ainda vai
/// olhar o que está cobrando antes de lançar.
async fn emit_note(
    ctx: &MethodCtx<'_>,
    origin: &Map<String, Value>,
    date: &str,
    reason: Option<&str>,
    copy_lines: bool,
) -> Result<i64, RusdooError> {
    let origin_id = origin
        .get("id")
        .and_then(Value::as_i64)
        .ok_or_else(|| RusdooError::Validation("a fatura de origem sumiu".into()))?;
    let name = origin.get("name").and_then(Value::as_str).unwrap_or("");
    // o motivo entra na referência junto do número da origem: é o que
    // aparece no extrato do cliente, e sozinho o número não explica nada
    let reference = match reason {
        Some(reason) => format!("{name}, {reason}"),
        None => name.to_string(),
    };
    let lines = if copy_lines {
        copied_lines(ctx, origin).await?
    } else {
        Vec::new()
    };

    // mesmo tipo da origem: debitar uma fatura de fornecedor gera outra
    // fatura de fornecedor, não uma cobrança ao cliente
    let move_type = origin
        .get("move_type")
        .cloned()
        .ok_or_else(|| RusdooError::Validation(format!("a fatura {name} não tem tipo")))?;
    let mut values: Vec<(&str, Value)> = vec![
        ("move_type", move_type),
        (
            "partner_id",
            json!(origin.get("partner_id").and_then(first_id)),
        ),
        (
            "company_id",
            json!(origin.get("company_id").and_then(first_id)),
        ),
        ("ref", json!(reference)),
        ("invoice_date", json!(date)),
        ("debit_origin_id", json!(origin_id)),
        ("line_ids", Value::Array(lines)),
    ];
    // sem a sequência das notas instalada, o documento cai na numeração
    // normal das faturas em vez de nascer sem número
    if let Some(number) = ctx.registry.next_sequence(ctx.pool, DEBIT_SEQUENCE).await? {
        values.push(("name", json!(number)));
    }
    ctx.registry
        .create_as(ctx.pool, ctx.uid, "account.move", values)
        .await
}

/// As linhas da fatura de origem, como comandos de criação.
///
/// São cópias, não vínculos: a nota é um documento próprio, e editar a
/// linha de uma não pode reescrever a outra.
async fn copied_lines(
    ctx: &MethodCtx<'_>,
    origin: &Map<String, Value>,
) -> Result<Vec<Value>, RusdooError> {
    let line_ids: Vec<i64> = origin
        .get("line_ids")
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    if line_ids.is_empty() {
        return Ok(Vec::new());
    }
    let lines = ctx
        .registry
        .read(
            ctx.pool,
            "account.move.line",
            &line_ids,
            &["product_id", "name", "quantity", "price_unit", "sequence"],
        )
        .await?;
    Ok(lines
        .iter()
        .map(|line| {
            json!([0, 0, {
                "product_id": line.get("product_id").and_then(first_id),
                "name": line.get("name").cloned().unwrap_or(Value::Null),
                "quantity": line.get("quantity").cloned().unwrap_or_else(|| json!(0)),
                "price_unit": line.get("price_unit").cloned().unwrap_or_else(|| json!(0)),
                "sequence": line.get("sequence").cloned().unwrap_or_else(|| json!(10)),
            }])
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn posted_invoice() -> Map<String, Value> {
        let mut record = Map::new();
        record.insert("name".into(), json!("FAT/00001"));
        record.insert("state".into(), json!("posted"));
        record.insert("move_type".into(), json!("out_invoice"));
        record
    }

    #[test]
    fn a_posted_invoice_may_be_debited() {
        assert!(refuse_undebitable(&posted_invoice()).is_ok());
    }

    #[test]
    fn a_draft_invoice_may_not_be_debited() {
        let mut record = posted_invoice();
        record.insert("state".into(), json!("draft"));
        let error = refuse_undebitable(&record).expect_err("um rascunho não se debita");
        assert!(error.to_string().contains("lance a fatura antes"));
    }

    #[test]
    fn a_debit_note_may_not_be_debited_again() {
        let mut record = posted_invoice();
        // como o many2one volta da leitura: [id, nome]
        record.insert("debit_origin_id".into(), json!([7, "FAT/00001"]));
        let error = refuse_undebitable(&record).expect_err("uma nota não se debita");
        assert!(error.to_string().contains("já é uma nota de débito"));
    }

    #[test]
    fn a_plain_entry_may_not_be_debited() {
        let mut record = posted_invoice();
        record.insert("move_type".into(), json!("entry"));
        let error = refuse_undebitable(&record).expect_err("um lançamento não se debita");
        assert!(error.to_string().contains("fatura de cliente"));
    }

    #[test]
    fn an_invoice_counts_the_notes_that_came_out_of_it() {
        let mut record = Map::new();
        record.insert("debit_note_ids".into(), json!([12, 13]));
        assert_eq!(debit_note_count(&record), json!(2));
        // e uma fatura que nunca foi debitada conta zero, não nulo
        assert_eq!(debit_note_count(&Map::new()), json!(0));
    }

    #[test]
    fn the_models_extend_the_invoice_without_losing_it() {
        let mut reg = rusdoo_base::registry().unwrap();
        rusdoo_product::extend(&mut reg).unwrap();
        rusdoo_account::extend(&mut reg).unwrap();
        extend(&mut reg).unwrap();

        let mv = reg.get("account.move").unwrap();
        assert!(mv.field("debit_origin_id").is_some());
        assert!(mv.field("debit_note_count").is_some());
        // a extensão adiciona; o que o módulo `account` trouxe continua lá
        assert!(mv.field("amount_total").unwrap().stored);
        assert_eq!(mv.constraints().len(), 1, "a regra do vencimento sobrevive");
        assert_eq!(mv.meta.table, "account_move", "e a tabela é a mesma");

        let wizard = reg.get("account.debit.note").unwrap();
        assert!(wizard.is_transient(), "o assistente não é dado guardado");
    }
}
