//! rusdoo-stock-picking-batch — port of
//! `odoo/addons/stock_picking_batch/`: one trip through the warehouse
//! that carries several transfers.
//!
//! A batch transfer groups pickings so that a picker walks the aisles
//! once for all of them instead of once per document. A *wave* is the
//! same record with `is_wave` set: Odoo keeps one model for both because
//! everything about them — the numbering, the states, the merge — is the
//! same, and only what is grouped differs. A batch groups whole
//! transfers; a wave groups the work inside them.
//!
//! What the port has that Odoo has:
//!
//! * `stock.picking.batch`, its states and its numbering (`picking.batch`
//!   and `picking.wave`, two series so that a wave never eats a batch's
//!   number);
//! * `batch_id` on `stock.picking`, which is the link the whole module
//!   exists for;
//! * the sanity check (`_sanity_check` + `_compute_allowed_picking_ids`):
//!   a transfer only joins a batch of its own operation type, and only
//!   while its state allows it;
//! * confirm / cancel / validate / merge, and the two wizards that put
//!   transfers into a batch or into a wave;
//! * `@api.ondelete`: a validated batch is not deleted.
//!
//! What it deliberately does **not** have, and why — the long version is
//! in the report, the short one here:
//!
//! * **automatic batching and automatic waving** (`_find_auto_batch`,
//!   `_auto_wave`). Every criterion they group by (`batch_group_by_partner`,
//!   `wave_group_by_category`, `batch_max_lines`, …) is a field Odoo adds
//!   to `stock.picking.type`, and this port's `stock` has no
//!   `stock.picking.type` model at all — a picking's operation type is a
//!   selection on the picking. Inventing the model here would put a core
//!   `stock` table in an optional addon.
//! * **waves that split a transfer**. Odoo's wave moves *move lines*
//!   between pickings; there is no `stock.move.line` here yet, so a wave
//!   in this port groups whole transfers, exactly like a batch, and is
//!   told apart by `is_wave` and by its own numbering.
//! * packages, the reception report, label layouts and the batch report —
//!   each needs a model or a report engine the port does not have.

use rusdoo_core::RusdooError;
use rusdoo_orm::access::Operation;
use rusdoo_orm::defaults;
use rusdoo_orm::fields::{Field, FieldType, OnDelete};
use rusdoo_orm::methods::{MethodCtx, MethodFuture, MethodRegistry};
use rusdoo_orm::model::{Model, ModelMeta};
use rusdoo_orm::registry::Registry;
use rusdoo_orm::unlink::UnlinkCtx;
use serde_json::{json, Map, Value};

/// The series a batch is numbered from (`data/stock_picking_batch_data.xml`).
pub const BATCH_SEQUENCE: &str = "picking.batch";
/// The series a wave is numbered from. Two series and not one: whoever
/// audits a month's batches should not find holes where the waves were.
pub const WAVE_SEQUENCE: &str = "picking.wave";

const DRAFT: &str = "draft";
const IN_PROGRESS: &str = "in_progress";
const DONE: &str = "done";
const CANCEL: &str = "cancel";

/// The state a transfer must be in to join a batch, port of
/// `_compute_allowed_picking_ids`.
///
/// Odoo's list is `waiting, confirmed, assigned`, plus `draft` while the
/// batch itself is a draft — the three middle states of a picking that
/// this port's `stock` collapses into `confirmed`. The rule that
/// survives is the one that matters: a transfer that is over (done or
/// cancelled) never joins anything, and a draft one only joins a batch
/// that has not started either.
fn allowed_transfer_states(batch_state: &str) -> &'static [&'static str] {
    if batch_state == DRAFT {
        &["draft", "confirmed"]
    } else {
        &["confirmed"]
    }
}

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

/// The id inside a many2one, which reads back as `[id, name]`.
fn first_id(value: &Value) -> Option<i64> {
    match value {
        Value::Array(items) => items.first().and_then(Value::as_i64),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

/// The ids inside an x2many, which reads back as a flat list.
fn id_list(row: &Map<String, Value>, field: &str) -> Vec<i64> {
    row.get(field)
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

fn text<'a>(row: &'a Map<String, Value>, field: &str) -> &'a str {
    row.get(field).and_then(Value::as_str).unwrap_or_default()
}

pub fn extend(reg: &mut Registry) -> Result<(), RusdooError> {
    reg.register(batch())?;
    reg.register(batched_picking())?;
    reg.register(to_batch_wizard())?;
    reg.register(add_to_wave_wizard())?;
    Ok(())
}

/// The buttons of a batch, of a transfer, and of the two dialogs.
pub fn extend_methods(methods: &mut MethodRegistry) -> Result<(), RusdooError> {
    methods.register(
        "stock.picking.batch",
        "action_confirm",
        Operation::Write,
        action_confirm,
    )?;
    methods.register(
        "stock.picking.batch",
        "action_cancel",
        Operation::Write,
        action_cancel,
    )?;
    methods.register(
        "stock.picking.batch",
        "action_done",
        Operation::Write,
        action_done,
    )?;
    methods.register(
        "stock.picking.batch",
        "action_merge",
        Operation::Write,
        action_merge,
    )?;
    // opening a dialog writes the wizard record, not the transfers — but
    // the wizard is what will move them, so it is not a Read grant: a
    // user who may not batch must not get as far as the form
    methods.register(
        "stock.picking",
        "action_add_to_batch",
        Operation::Write,
        action_add_to_batch,
    )?;
    methods.register(
        "stock.picking",
        "action_add_to_wave",
        Operation::Write,
        action_add_to_wave,
    )?;
    methods.register(
        "stock.picking",
        "action_view_batch",
        Operation::Read,
        action_view_batch,
    )?;
    methods.register(
        "stock.picking.to.batch",
        "attach_pickings",
        Operation::Write,
        attach_to_batch,
    )?;
    methods.register(
        "stock.add.to.wave",
        "attach_pickings",
        Operation::Write,
        attach_to_wave,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------

/// `stock.picking.batch` — the trip, and the transfers it carries.
fn batch() -> Model {
    Model::new(
        meta("stock.picking.batch", "stock_picking_batch"),
        vec![
            // Odoo builds the name in `create` so that a wave draws from
            // `picking.wave` and a batch from `picking.batch`. There is
            // no create override here, so the field carries the batch
            // series and whatever creates a wave (the wave wizard, and
            // only it) draws the wave number itself and passes it in.
            char("name").required().from_sequence(BATCH_SEQUENCE),
            char("description"),
            m2o("user_id", "res.users"),
            m2o("company_id", "res.company")
                .required()
                .default_from(defaults::USER_COMPANY),
            Field::new(
                "picking_ids",
                FieldType::One2many {
                    comodel: "stock.picking".into(),
                    inverse: "batch_id".into(),
                },
            ),
            // Odoo's `picking_type_id`. This port's `stock` has no
            // `stock.picking.type` model — the operation type is a
            // selection on the picking — so the batch mirrors the same
            // selection. The behaviour that depends on it is the one
            // that matters and it is intact: a batch holds transfers of a
            // single operation type.
            Field::new(
                "picking_type",
                FieldType::Selection(vec![
                    ("outgoing".into(), "Delivery".into()),
                    ("incoming".into(), "Receipt".into()),
                    ("internal".into(), "Internal transfer".into()),
                ]),
            ),
            // Not a stored compute, unlike Odoo. A stored compute here is
            // readonly to callers and only reruns when the *batch* is
            // written, so `action_confirm` could not set `in_progress`
            // and a picking validated on its own would never move it.
            // The state is written by the batch's own actions, which run
            // `derived_state` for the "all its transfers are over" part.
            Field::new(
                "state",
                FieldType::Selection(vec![
                    (DRAFT.into(), "Draft".into()),
                    (IN_PROGRESS.into(), "In progress".into()),
                    (DONE.into(), "Done".into()),
                    (CANCEL.into(), "Cancelled".into()),
                ]),
            )
            .required()
            .default_value(json!(DRAFT)),
            // Odoo computes this as the earliest of its transfers' dates
            // and lets it be written over. Same reason as `state`: it is
            // a plain column the actions keep current.
            Field::new("scheduled_date", FieldType::Datetime),
            Field::new("is_wave", FieldType::Boolean).default_value(json!(false)),
            // how many transfers the list shows without opening the batch
            Field::new("picking_count", FieldType::Integer)
                .computed(&["picking_ids"], picking_count),
        ],
    )
    // Odoo's `_order`: the newest number first, which for a
    // zero-padded series is the newest batch
    .ordered("name desc")
    // `_unlink_if_not_done`: a validated batch is the record of a trip
    // that happened
    .on_unlink("no done batch transfer", refuse_done_batch)
}

/// `picking_count` — the number on the batch's card.
fn picking_count(record: &Map<String, Value>) -> Value {
    json!(record
        .get("picking_ids")
        .and_then(Value::as_array)
        .map_or(0, Vec::len))
}

/// `stock.picking` extended (`_inherit`): which batch carries it.
fn batched_picking() -> Model {
    Model::new(
        ModelMeta {
            name: "stock.picking".into(),
            table: "stock_picking".into(),
            inherit: vec!["stock.picking".into()],
            inherits: vec![],
        },
        vec![
            // SetNull and not Cascade: deleting a batch must never delete
            // the deliveries it was going to carry
            m2o("batch_id", "stock.picking.batch").ondelete(OnDelete::SetNull),
            // the order the picker walks them in
            Field::new("batch_sequence", FieldType::Integer).default_value(json!(0)),
        ],
    )
}

/// `stock.picking.to.batch` — the dialog that puts transfers in a batch.
fn to_batch_wizard() -> Model {
    Model::new(
        meta("stock.picking.to.batch", "stock_picking_to_batch"),
        vec![
            // Odoo reads the selection out of the context (`active_ids`);
            // the port's wizard is born pointing at it, like the debit
            // note's. A dialog that opens pointing at nothing is a dialog
            // whose button cannot work.
            Field::new(
                "picking_ids",
                FieldType::Many2many {
                    comodel: "stock.picking".into(),
                    relation: "stock_picking_to_batch_picking_rel".into(),
                    column1: "wizard_id".into(),
                    column2: "picking_id".into(),
                },
            ),
            m2o("batch_id", "stock.picking.batch"),
            Field::new(
                "mode",
                FieldType::Selection(vec![
                    ("existing".into(), "an existing batch transfer".into()),
                    ("new".into(), "a new batch transfer".into()),
                ]),
            )
            .default_value(json!("new")),
            m2o("user_id", "res.users"),
            // "leave it in draft": the batch is prepared now and confirmed
            // when the shift starts
            Field::new("is_create_draft", FieldType::Boolean).default_value(json!(false)),
            char("description"),
        ],
    )
    .transient()
}

/// `stock.add.to.wave` — the same dialog, for a wave.
fn add_to_wave_wizard() -> Model {
    Model::new(
        meta("stock.add.to.wave", "stock_add_to_wave"),
        vec![
            Field::new(
                "picking_ids",
                FieldType::Many2many {
                    comodel: "stock.picking".into(),
                    relation: "stock_add_to_wave_picking_rel".into(),
                    column1: "wizard_id".into(),
                    column2: "picking_id".into(),
                },
            ),
            m2o("wave_id", "stock.picking.batch"),
            Field::new(
                "mode",
                FieldType::Selection(vec![
                    ("existing".into(), "an existing wave transfer".into()),
                    ("new".into(), "a new wave transfer".into()),
                ]),
            )
            // Odoo's default: a wave usually joins the one already open
            .default_value(json!("existing")),
            m2o("user_id", "res.users"),
        ],
    )
    .transient()
}

/// `_unlink_if_not_done`.
fn refuse_done_batch(
    mut ctx: UnlinkCtx<'_>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), RusdooError>> + Send + '_>> {
    Box::pin(async move {
        for record in ctx.read(&["name", "state"]).await? {
            if text(&record, "state") != DONE {
                continue;
            }
            let name = record.get("name").and_then(Value::as_str).unwrap_or("?");
            return Err(RusdooError::Validation(format!(
                "batch transfer {name} is done: a trip that happened is not deleted"
            )));
        }
        Ok(())
    })
}

// ---------------------------------------------------------------------
// The rules, as plain functions
// ---------------------------------------------------------------------

/// A transfer, in the terms the batch reasons about it.
#[derive(Debug, Clone, PartialEq)]
struct Transfer {
    id: i64,
    name: String,
    state: String,
    picking_type: String,
    company: Option<i64>,
    moves: Vec<i64>,
}

impl Transfer {
    fn from_row(row: &Map<String, Value>) -> Transfer {
        Transfer {
            id: row.get("id").and_then(Value::as_i64).unwrap_or_default(),
            name: text(row, "name").to_string(),
            state: text(row, "state").to_string(),
            picking_type: text(row, "picking_type").to_string(),
            company: row.get("company_id").and_then(first_id),
            moves: id_list(row, "move_ids"),
        }
    }

    fn is_over(&self) -> bool {
        self.state == DONE || self.state == CANCEL
    }
}

/// The fields a `Transfer` is built from — one list, so a read and the
/// struct can never drift apart.
const TRANSFER_FIELDS: [&str; 5] = ["name", "state", "picking_type", "company_id", "move_ids"];

/// Port of `_sanity_check`: which of these transfers this batch cannot
/// take, and why.
///
/// Odoo phrases the refusal as one message naming every offender rather
/// than failing on the first: whoever selected forty deliveries wants to
/// know which three are wrong, not to fix them one round-trip at a time.
fn refuse_incompatible(
    batch_name: &str,
    batch_state: &str,
    batch_type: Option<&str>,
    transfers: &[Transfer],
) -> Result<(), String> {
    if batch_state == DONE || batch_state == CANCEL {
        return Err(format!(
            "batch transfer {batch_name} is {batch_state}: it takes no more transfers"
        ));
    }
    let allowed = allowed_transfer_states(batch_state);
    let wrong: Vec<&str> = transfers
        .iter()
        .filter(|transfer| {
            !allowed.contains(&transfer.state.as_str())
                || batch_type.is_some_and(|wanted| wanted != transfer.picking_type)
        })
        .map(|transfer| transfer.name.as_str())
        .collect();
    if !wrong.is_empty() {
        return Err(format!(
            "the following transfers cannot be added to batch transfer {batch_name}. \
             Please check their states and operation types.\n\nIncompatibilities: {}",
            wrong.join(", ")
        ));
    }
    Ok(())
}

/// Port of `_compute_state`: the state its transfers imply, or `None`
/// when they imply nothing and the written one stands.
///
/// Odoo only ever moves the batch *forward* this way — a batch already
/// done or cancelled is left alone, and so is one whose transfers are
/// still running.
fn derived_state(current: &str, transfer_states: &[String]) -> Option<&'static str> {
    if current == DONE || current == CANCEL || transfer_states.is_empty() {
        return None;
    }
    if transfer_states.iter().all(|state| state == CANCEL) {
        return Some(CANCEL);
    }
    if transfer_states
        .iter()
        .all(|state| state == CANCEL || state == DONE)
    {
        return Some(DONE);
    }
    None
}

/// A batch, in the terms `action_merge` reasons about it.
#[derive(Debug, Clone, PartialEq)]
struct Candidate {
    id: i64,
    state: String,
    is_wave: bool,
    picking_type: Option<String>,
    scheduled_date: Option<String>,
}

/// Port of the guards at the top of `action_merge`.
fn refuse_unmergeable(candidates: &[Candidate]) -> Result<(), String> {
    if candidates.len() < 2 {
        return Err("please select at least two batch/wave transfers to merge".into());
    }
    let first = &candidates[0];
    if candidates
        .iter()
        .any(|other| other.picking_type != first.picking_type)
    {
        return Err("batch/wave transfers with different operation types cannot be merged".into());
    }
    if candidates
        .iter()
        .any(|other| other.is_wave != first.is_wave)
    {
        return Err("batch transfers cannot be merged with wave transfers and vice versa".into());
    }
    if candidates.iter().any(|other| other.state != first.state) {
        return Err("batch/wave transfers with different states cannot be merged".into());
    }
    if first.state == DONE || first.state == CANCEL {
        return Err("you cannot merge done or cancelled batch/wave transfers".into());
    }
    Ok(())
}

/// Which of the merged batches the surviving one takes its values from:
/// the one due first, because a merge must not push a trip's date back.
///
/// Odoo's `self.filtered('scheduled_date').sorted(...)[0]` raises when no
/// batch has a date at all; here the target keeps its own values, which
/// is what that expression would have meant.
fn earliest<'a>(candidates: &'a [Candidate], target: &'a Candidate) -> &'a Candidate {
    candidates
        .iter()
        .filter(|candidate| candidate.scheduled_date.is_some())
        .min_by(|left, right| left.scheduled_date.cmp(&right.scheduled_date))
        .unwrap_or(target)
}

// ---------------------------------------------------------------------
// The pieces the methods share
// ---------------------------------------------------------------------

/// The single record a button was pressed on.
fn only_one(ids: &[i64], complaint: &str) -> Result<i64, RusdooError> {
    match ids {
        [id] => Ok(*id),
        _ => Err(RusdooError::Validation(complaint.to_string())),
    }
}

fn gone(model: &str, id: i64) -> RusdooError {
    RusdooError::Validation(format!("{model} {id} does not exist"))
}

/// The fields every batch action reads.
const BATCH_FIELDS: [&str; 9] = [
    "name",
    "state",
    "picking_type",
    "is_wave",
    "company_id",
    "picking_ids",
    "scheduled_date",
    "user_id",
    "description",
];

async fn read_batch(ctx: &MethodCtx<'_>, id: i64) -> Result<Map<String, Value>, RusdooError> {
    ctx.registry
        .read(ctx.pool, "stock.picking.batch", &[id], &BATCH_FIELDS)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| gone("batch transfer", id))
}

async fn read_transfers(ctx: &MethodCtx<'_>, ids: &[i64]) -> Result<Vec<Transfer>, RusdooError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = ctx
        .registry
        .read(ctx.pool, "stock.picking", ids, &TRANSFER_FIELDS)
        .await?;
    Ok(rows.iter().map(Transfer::from_row).collect())
}

/// Confirm the transfers of a batch, port of what `action_confirm` means
/// by `self.picking_ids.action_confirm()`.
///
/// The body duplicates `rusdoo_stock`'s own `action_confirm` — the empty
/// check and the draft → confirmed move — because a method here has no
/// way to call a method registered there: `MethodCtx` carries the model
/// registry, not the method one. That is the framework gap this addon
/// hit hardest; see the report.
async fn confirm_transfers(ctx: &MethodCtx<'_>, transfers: &[Transfer]) -> Result<(), RusdooError> {
    let to_confirm: Vec<i64> = transfers
        .iter()
        .filter(|transfer| transfer.state == DRAFT)
        .map(|transfer| transfer.id)
        .collect();
    if to_confirm.is_empty() {
        return Ok(());
    }
    for transfer in transfers.iter().filter(|t| t.state == DRAFT) {
        if transfer.moves.is_empty() {
            return Err(RusdooError::Validation(format!(
                "transfer {} has no lines: there is nothing to move",
                transfer.name
            )));
        }
    }
    ctx.registry
        .write_as(
            ctx.pool,
            ctx.uid,
            "stock.picking",
            &to_confirm,
            vec![("state", json!("confirmed"))],
        )
        .await
}

/// Confirm a batch: its transfers, then itself.
async fn confirm_batch(ctx: &MethodCtx<'_>, batch_id: i64) -> Result<(), RusdooError> {
    let batch = read_batch(ctx, batch_id).await?;
    let transfers = read_transfers(ctx, &id_list(&batch, "picking_ids")).await?;
    if transfers.is_empty() {
        return Err(RusdooError::Validation(
            "you have to set some transfers on the batch before confirming it".into(),
        ));
    }
    let batch_type = batch.get("picking_type").and_then(Value::as_str);
    refuse_incompatible(
        text(&batch, "name"),
        text(&batch, "state"),
        batch_type,
        &transfers,
    )
    .map_err(RusdooError::Validation)?;
    confirm_transfers(ctx, &transfers).await?;
    ctx.registry
        .write_as(
            ctx.pool,
            ctx.uid,
            "stock.picking.batch",
            &[batch_id],
            vec![("state", json!(IN_PROGRESS))],
        )
        .await
}

/// The act_window that opens a batch — what every button here answers
/// with, so the user lands on what they just acted on.
fn open_batch(id: i64, is_wave: bool) -> Value {
    json!({
        "type": "ir.actions.act_window",
        "name": if is_wave { "Wave transfer" } else { "Batch transfer" },
        "res_model": "stock.picking.batch",
        "res_id": id,
        "views": [[false, "form"]],
        "target": "current",
    })
}

// ---------------------------------------------------------------------
// The batch's buttons
// ---------------------------------------------------------------------

/// `action_confirm` — sanity check, confirm the transfers, start.
fn action_confirm<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let id = only_one(&ctx.ids, "confirm one batch transfer at a time")?;
        confirm_batch(&ctx, id).await?;
        Ok(json!(true))
    })
}

/// `action_cancel` — the trip is off.
///
/// Odoo empties `picking_ids` and leaves the transfers alone: cancelling
/// the batch cancels the *grouping*, not forty deliveries the customers
/// are still waiting for. Whoever wants those cancelled cancels them.
fn action_cancel<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        if ctx.ids.is_empty() {
            return Err(RusdooError::Validation(
                "the action needs at least one batch transfer".into(),
            ));
        }
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "stock.picking.batch",
                &ctx.ids,
                vec![
                    ("state", json!(CANCEL)),
                    ("picking_ids", json!([[5, 0, 0]])),
                ],
            )
            .await?;
        Ok(json!(true))
    })
}

/// `action_done` — everything in the batch went out.
///
/// Odoo runs each transfer's `button_validate` and drops the empty ones
/// from the batch first. There is no per-move `picked` flag in this
/// port's `stock` (a move carries a planned quantity and a done one), so
/// "empty" cannot arise: validating fills the done quantity from the
/// planned one, exactly as `rusdoo_stock`'s own validate does.
fn action_done<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let id = only_one(&ctx.ids, "validate one batch transfer at a time")?;
        let batch = read_batch(&ctx, id).await?;
        let state = text(&batch, "state").to_string();
        if state == DONE || state == CANCEL {
            return Err(RusdooError::Validation(format!(
                "batch transfer {} is {state}: there is nothing left to validate",
                text(&batch, "name")
            )));
        }
        let transfers = read_transfers(&ctx, &id_list(&batch, "picking_ids")).await?;
        if transfers.is_empty() {
            return Err(RusdooError::Validation(
                "you have to set some transfers on the batch before validating it".into(),
            ));
        }
        for transfer in transfers.iter().filter(|transfer| !transfer.is_over()) {
            validate_transfer(&ctx, transfer).await?;
        }
        // what the transfers now say, so the batch's own state follows
        // from them and not from the button
        let after: Vec<String> = transfers
            .iter()
            .map(|transfer| {
                if transfer.state == CANCEL {
                    CANCEL.to_string()
                } else {
                    DONE.to_string()
                }
            })
            .collect();
        if let Some(reached) = derived_state(&state, &after) {
            ctx.registry
                .write_as(
                    ctx.pool,
                    ctx.uid,
                    "stock.picking.batch",
                    &[id],
                    vec![("state", json!(reached))],
                )
                .await?;
        }
        Ok(json!(true))
    })
}

/// One transfer validated: what left is what was planned, unless the
/// warehouse said otherwise.
async fn validate_transfer(ctx: &MethodCtx<'_>, transfer: &Transfer) -> Result<(), RusdooError> {
    if !transfer.moves.is_empty() {
        let moves = ctx
            .registry
            .read(
                ctx.pool,
                "stock.move",
                &transfer.moves,
                &["product_uom_qty", "quantity_done"],
            )
            .await?;
        for row in &moves {
            if number(row, "quantity_done") > 0.0 {
                continue;
            }
            let planned = number(row, "product_uom_qty");
            let move_id = row.get("id").and_then(Value::as_i64).unwrap_or_default();
            ctx.registry
                .write_as(
                    ctx.pool,
                    ctx.uid,
                    "stock.move",
                    &[move_id],
                    vec![("quantity_done", json!(planned))],
                )
                .await?;
        }
    }
    ctx.registry
        .write_as(
            ctx.pool,
            ctx.uid,
            "stock.picking",
            &[transfer.id],
            vec![("state", json!(DONE))],
        )
        .await
}

/// `action_merge` — several batches become one.
fn action_merge<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let rows = ctx
            .registry
            .read(ctx.pool, "stock.picking.batch", &ctx.ids, &BATCH_FIELDS)
            .await?;
        if rows.len() != ctx.ids.len() {
            return Err(RusdooError::Validation(
                "one of the chosen batch transfers is gone: reload the list".into(),
            ));
        }
        // the read comes back in the model's own order; the merge target
        // is the first of what the *caller* selected, like Odoo's `self[:1]`
        let mut candidates: Vec<Candidate> = Vec::with_capacity(rows.len());
        for id in &ctx.ids {
            let row = rows
                .iter()
                .find(|row| row.get("id").and_then(Value::as_i64) == Some(*id))
                .ok_or_else(|| gone("batch transfer", *id))?;
            candidates.push(Candidate {
                id: *id,
                state: text(row, "state").to_string(),
                is_wave: row.get("is_wave").and_then(Value::as_bool).unwrap_or(false),
                picking_type: row
                    .get("picking_type")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                scheduled_date: row
                    .get("scheduled_date")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
        refuse_unmergeable(&candidates).map_err(RusdooError::Validation)?;

        let (target, others) = candidates.split_first().expect("checked above");
        let source = earliest(&candidates, target).clone();
        let moving: Vec<i64> = {
            let mut ids = Vec::new();
            for other in others {
                let row = rows
                    .iter()
                    .find(|row| row.get("id").and_then(Value::as_i64) == Some(other.id))
                    .ok_or_else(|| gone("batch transfer", other.id))?;
                ids.extend(id_list(row, "picking_ids"));
            }
            ids
        };
        if !moving.is_empty() {
            ctx.registry
                .write_as(
                    ctx.pool,
                    ctx.uid,
                    "stock.picking",
                    &moving,
                    vec![("batch_id", json!(target.id))],
                )
                .await?;
        }
        // `_get_merged_batch_vals`: responsible, description and date
        // come from the batch that was due first
        let source_row = rows
            .iter()
            .find(|row| row.get("id").and_then(Value::as_i64) == Some(source.id))
            .ok_or_else(|| gone("batch transfer", source.id))?;
        ctx.registry
            .write_as(
                ctx.pool,
                ctx.uid,
                "stock.picking.batch",
                &[target.id],
                vec![
                    (
                        "user_id",
                        json!(source_row.get("user_id").and_then(first_id)),
                    ),
                    (
                        "description",
                        source_row
                            .get("description")
                            .cloned()
                            .unwrap_or(Value::Null),
                    ),
                    ("scheduled_date", json!(source.scheduled_date.clone())),
                ],
            )
            .await?;
        let emptied: Vec<i64> = others.iter().map(|other| other.id).collect();
        ctx.registry
            .unlink_as(ctx.pool, ctx.uid, "stock.picking.batch", &emptied)
            .await?;
        // Odoo answers with a notification carrying a link; the port
        // opens what survived, which is the same destination without a
        // client-side widget behind it
        Ok(open_batch(target.id, target.is_wave))
    })
}

// ---------------------------------------------------------------------
// The transfer's buttons
// ---------------------------------------------------------------------

/// `action_view_batch` — open the batch this transfer travels in.
fn action_view_batch<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let id = only_one(&ctx.ids, "open the batch of one transfer at a time")?;
        let rows = ctx
            .registry
            .read(ctx.pool, "stock.picking", &[id], &["batch_id"])
            .await?;
        let batch = rows
            .first()
            .ok_or_else(|| gone("transfer", id))?
            .get("batch_id")
            .and_then(first_id)
            .ok_or_else(|| {
                RusdooError::Validation("this transfer is not in a batch transfer".into())
            })?;
        let is_wave = ctx
            .registry
            .read(ctx.pool, "stock.picking.batch", &[batch], &["is_wave"])
            .await?
            .first()
            .and_then(|row| row.get("is_wave").and_then(Value::as_bool))
            .unwrap_or(false);
        Ok(open_batch(batch, is_wave))
    })
}

/// Open one of the two dialogs over the selected transfers.
///
/// Odoo hands the selection over in the context (`active_ids`) and reads
/// it in the wizard's `default_get`; the port creates the wizard already
/// pointing at them, like `account_debit_note`'s. The check that the
/// selection makes sense therefore happens here, before a form opens
/// that could not have worked.
async fn open_dialog(ctx: &MethodCtx<'_>, model: &str, title: &str) -> Result<Value, RusdooError> {
    if ctx.ids.is_empty() {
        return Err(RusdooError::Validation(
            "choose at least one transfer".into(),
        ));
    }
    let transfers = read_transfers(ctx, &ctx.ids).await?;
    if transfers.len() != ctx.ids.len() {
        return Err(RusdooError::Validation(
            "one of the chosen transfers is gone: reload the list".into(),
        ));
    }
    single_operation_type(&transfers)?;
    single_company(&transfers)?;
    let wizard = ctx
        .registry
        .create_as(
            ctx.pool,
            ctx.uid,
            model,
            vec![("picking_ids", json!([[6, 0, ctx.ids]]))],
        )
        .await?;
    Ok(json!({
        "type": "ir.actions.act_window",
        "name": title,
        "res_model": model,
        "res_id": wizard,
        "views": [[false, "form"]],
        "target": "new",
    }))
}

/// `default_get`'s refusal: "The selected transfers should belong to the
/// same operation type".
fn single_operation_type(transfers: &[Transfer]) -> Result<String, RusdooError> {
    let mut kinds: Vec<&str> = transfers
        .iter()
        .map(|transfer| transfer.picking_type.as_str())
        .collect();
    kinds.sort_unstable();
    kinds.dedup();
    match kinds[..] {
        [kind] => Ok(kind.to_string()),
        _ => Err(RusdooError::Validation(
            "the selected transfers should belong to the same operation type".into(),
        )),
    }
}

/// "The selected pickings should belong to an unique company."
fn single_company(transfers: &[Transfer]) -> Result<Option<i64>, RusdooError> {
    let mut companies: Vec<Option<i64>> =
        transfers.iter().map(|transfer| transfer.company).collect();
    companies.sort_unstable();
    companies.dedup();
    match companies[..] {
        [company] => Ok(company),
        _ => Err(RusdooError::Validation(
            "the selected transfers should belong to a single company".into(),
        )),
    }
}

fn action_add_to_batch<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move { open_dialog(&ctx, "stock.picking.to.batch", "Add to batch").await })
}

fn action_add_to_wave<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move { open_dialog(&ctx, "stock.add.to.wave", "Add to wave").await })
}

// ---------------------------------------------------------------------
// The dialogs' button
// ---------------------------------------------------------------------

/// What both wizards do, differing only in what they build.
struct Grouping {
    /// the batch the transfers join, existing or freshly created
    batch: i64,
    is_wave: bool,
    /// whether the batch should be confirmed once the transfers are in
    confirm: bool,
}

/// The transfers a wizard points at, checked the way `default_get` and
/// `attach_pickings` check them.
async fn wizard_transfers(
    ctx: &MethodCtx<'_>,
    wizard: &Map<String, Value>,
) -> Result<Vec<Transfer>, RusdooError> {
    let transfers = read_transfers(ctx, &id_list(wizard, "picking_ids")).await?;
    if transfers.is_empty() {
        return Err(RusdooError::Validation(
            "the dialog points at no transfer: reopen it from the list".into(),
        ));
    }
    Ok(transfers)
}

/// Put the transfers in the batch and answer with what to open.
async fn attach(
    ctx: &MethodCtx<'_>,
    grouping: Grouping,
    transfers: &[Transfer],
) -> Result<Value, RusdooError> {
    let batch = read_batch(ctx, grouping.batch).await?;
    refuse_incompatible(
        text(&batch, "name"),
        text(&batch, "state"),
        batch.get("picking_type").and_then(Value::as_str),
        transfers,
    )
    .map_err(RusdooError::Validation)?;
    let ids: Vec<i64> = transfers.iter().map(|transfer| transfer.id).collect();
    ctx.registry
        .write_as(
            ctx.pool,
            ctx.uid,
            "stock.picking",
            &ids,
            vec![("batch_id", json!(grouping.batch))],
        )
        .await?;
    if grouping.confirm {
        confirm_batch(ctx, grouping.batch).await?;
    }
    Ok(open_batch(grouping.batch, grouping.is_wave))
}

/// `stock.picking.to.batch.attach_pickings`.
fn attach_to_batch<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let id = only_one(&ctx.ids, "the dialog is gone")?;
        let rows = ctx
            .registry
            .read(
                ctx.pool,
                "stock.picking.to.batch",
                &[id],
                &[
                    "picking_ids",
                    "mode",
                    "batch_id",
                    "user_id",
                    "is_create_draft",
                    "description",
                ],
            )
            .await?;
        let wizard = rows.first().ok_or_else(|| gone("dialog", id))?;
        let transfers = wizard_transfers(&ctx, wizard).await?;
        let draft = wizard
            .get("is_create_draft")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let grouping = if text(wizard, "mode") == "new" {
            let kind = single_operation_type(&transfers)?;
            let company = single_company(&transfers)?;
            let batch = ctx
                .registry
                .create_as(
                    ctx.pool,
                    ctx.uid,
                    "stock.picking.batch",
                    vec![
                        ("user_id", json!(wizard.get("user_id").and_then(first_id))),
                        ("company_id", json!(company)),
                        ("picking_type", json!(kind)),
                        (
                            "description",
                            wizard.get("description").cloned().unwrap_or(Value::Null),
                        ),
                        ("is_wave", json!(false)),
                    ],
                )
                .await?;
            Grouping {
                batch,
                is_wave: false,
                // "you have to set some pickings to batch before confirm
                // it": a new batch is confirmed only once its transfers
                // are in, which is why this happens after the write
                confirm: !draft,
            }
        } else {
            let batch = wizard.get("batch_id").and_then(first_id).ok_or_else(|| {
                RusdooError::Validation("choose the batch transfer to add the transfers to".into())
            })?;
            Grouping {
                batch,
                is_wave: false,
                confirm: false,
            }
        };
        attach(&ctx, grouping, &transfers).await
    })
}

/// `stock.add.to.wave.attach_pickings`.
///
/// Odoo's wizard takes move *lines* and splits the transfers they came
/// from; with no `stock.move.line` in this port a wave takes whole
/// transfers, which is the `picking_ids` branch of the same wizard.
fn attach_to_wave<'a>(
    ctx: MethodCtx<'a>,
    _args: &'a [Value],
    _kwargs: &'a Map<String, Value>,
) -> MethodFuture<'a> {
    Box::pin(async move {
        let id = only_one(&ctx.ids, "the dialog is gone")?;
        let rows = ctx
            .registry
            .read(
                ctx.pool,
                "stock.add.to.wave",
                &[id],
                &["picking_ids", "mode", "wave_id", "user_id"],
            )
            .await?;
        let wizard = rows.first().ok_or_else(|| gone("dialog", id))?;
        let transfers = wizard_transfers(&ctx, wizard).await?;

        let grouping = if text(wizard, "mode") == "new" {
            let kind = single_operation_type(&transfers)?;
            let company = single_company(&transfers)?;
            let mut values: Vec<(&str, Value)> = vec![
                ("user_id", json!(wizard.get("user_id").and_then(first_id))),
                ("company_id", json!(company)),
                ("picking_type", json!(kind)),
                ("is_wave", json!(true)),
            ];
            // the wave's own series; without it installed the field's
            // default draws a batch number rather than leaving the wave
            // unnamed
            if let Some(number) = ctx.registry.next_sequence(ctx.pool, WAVE_SEQUENCE).await? {
                values.push(("name", json!(number)));
            }
            let batch = ctx
                .registry
                .create_as(ctx.pool, ctx.uid, "stock.picking.batch", values)
                .await?;
            Grouping {
                batch,
                is_wave: true,
                // Odoo confirms when the operation type says
                // `batch_auto_confirm`, which is on by default and has
                // nowhere to live here
                confirm: true,
            }
        } else {
            let batch = wizard.get("wave_id").and_then(first_id).ok_or_else(|| {
                RusdooError::Validation("choose the wave transfer to add the transfers to".into())
            })?;
            Grouping {
                batch,
                is_wave: true,
                confirm: false,
            }
        };
        attach(&ctx, grouping, &transfers).await
    })
}

/// A numeric field's value, whatever shape the driver decoded it in.
fn number(record: &Map<String, Value>, name: &str) -> f64 {
    record
        .get(name)
        .and_then(|value| match value {
            Value::Number(n) => n.as_f64(),
            Value::String(text) => text.parse().ok(),
            _ => None,
        })
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transfer(id: i64, name: &str, state: &str, kind: &str) -> Transfer {
        Transfer {
            id,
            name: name.to_string(),
            state: state.to_string(),
            picking_type: kind.to_string(),
            company: Some(1),
            moves: vec![id * 10],
        }
    }

    fn candidate(id: i64, state: &str, is_wave: bool, kind: &str, date: Option<&str>) -> Candidate {
        Candidate {
            id,
            state: state.to_string(),
            is_wave,
            picking_type: Some(kind.to_string()),
            scheduled_date: date.map(str::to_string),
        }
    }

    #[test]
    fn a_draft_transfer_only_joins_a_draft_batch() {
        assert!(allowed_transfer_states(DRAFT).contains(&"draft"));
        assert!(!allowed_transfer_states(IN_PROGRESS).contains(&"draft"));
        // and a confirmed one joins either
        for state in [DRAFT, IN_PROGRESS] {
            assert!(allowed_transfer_states(state).contains(&"confirmed"));
        }
    }

    #[test]
    fn a_batch_takes_confirmed_transfers_of_its_own_operation_type() {
        let transfers = vec![
            transfer(1, "WH/OUT/00001", "confirmed", "outgoing"),
            transfer(2, "WH/OUT/00002", "confirmed", "outgoing"),
        ];
        assert!(
            refuse_incompatible("BATCH/00001", IN_PROGRESS, Some("outgoing"), &transfers).is_ok()
        );
    }

    #[test]
    fn a_transfer_of_another_operation_type_is_named_in_the_refusal() {
        let transfers = vec![
            transfer(1, "WH/OUT/00001", "confirmed", "outgoing"),
            transfer(2, "WH/IN/00007", "confirmed", "incoming"),
        ];
        let error = refuse_incompatible("BATCH/00001", IN_PROGRESS, Some("outgoing"), &transfers)
            .expect_err("a receipt does not travel with a delivery");
        assert!(error.contains("WH/IN/00007"), "{error}");
        // and the one that was fine is not blamed
        assert!(!error.contains("WH/OUT/00001"), "{error}");
    }

    #[test]
    fn a_batch_with_no_operation_type_yet_takes_whatever_it_is_given() {
        let transfers = vec![transfer(1, "WH/OUT/00001", "confirmed", "outgoing")];
        assert!(refuse_incompatible("BATCH/00001", DRAFT, None, &transfers).is_ok());
    }

    #[test]
    fn a_finished_transfer_never_joins_a_batch() {
        for state in [DONE, CANCEL] {
            let transfers = vec![transfer(1, "WH/OUT/00001", state, "outgoing")];
            let error =
                refuse_incompatible("BATCH/00001", IN_PROGRESS, Some("outgoing"), &transfers)
                    .expect_err("what is over does not travel");
            assert!(error.contains("WH/OUT/00001"), "{error}");
        }
    }

    #[test]
    fn a_finished_batch_takes_nothing_more() {
        let transfers = vec![transfer(1, "WH/OUT/00001", "confirmed", "outgoing")];
        for state in [DONE, CANCEL] {
            let error = refuse_incompatible("BATCH/00001", state, Some("outgoing"), &transfers)
                .expect_err("a trip that is over takes no passengers");
            assert!(error.contains("no more transfers"), "{error}");
        }
    }

    #[test]
    fn a_batch_is_done_when_its_transfers_are() {
        let states = vec![DONE.to_string(), DONE.to_string()];
        assert_eq!(derived_state(IN_PROGRESS, &states), Some(DONE));
        // a cancelled transfer among done ones does not hold the batch back
        let mixed = vec![DONE.to_string(), CANCEL.to_string()];
        assert_eq!(derived_state(IN_PROGRESS, &mixed), Some(DONE));
    }

    #[test]
    fn a_batch_whose_transfers_were_all_cancelled_is_cancelled() {
        let states = vec![CANCEL.to_string(), CANCEL.to_string()];
        assert_eq!(derived_state(IN_PROGRESS, &states), Some(CANCEL));
    }

    #[test]
    fn a_running_batch_stays_where_it_was() {
        let states = vec![DONE.to_string(), "confirmed".to_string()];
        assert_eq!(derived_state(IN_PROGRESS, &states), None);
        // an empty batch says nothing about itself, as in Odoo
        assert_eq!(derived_state(IN_PROGRESS, &[]), None);
        // and one already over is never moved again
        assert_eq!(
            derived_state(DONE, &[CANCEL.to_string()]),
            None,
            "a done batch is not un-done by a late cancellation"
        );
    }

    #[test]
    fn one_batch_is_not_a_merge() {
        let error = refuse_unmergeable(&[candidate(1, DRAFT, false, "outgoing", None)])
            .expect_err("a merge needs two");
        assert!(error.contains("at least two"), "{error}");
    }

    #[test]
    fn batches_of_different_shapes_do_not_merge() {
        let base = candidate(1, DRAFT, false, "outgoing", None);
        for (other, expected) in [
            (
                candidate(2, DRAFT, false, "incoming", None),
                "operation types",
            ),
            (
                candidate(2, DRAFT, true, "outgoing", None),
                "wave transfers",
            ),
            (
                candidate(2, IN_PROGRESS, false, "outgoing", None),
                "different states",
            ),
        ] {
            let error = refuse_unmergeable(&[base.clone(), other])
                .expect_err("these two do not belong together");
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn finished_batches_do_not_merge() {
        for state in [DONE, CANCEL] {
            let error = refuse_unmergeable(&[
                candidate(1, state, false, "outgoing", None),
                candidate(2, state, false, "outgoing", None),
            ])
            .expect_err("what is over is not merged");
            assert!(error.contains("done or cancelled"), "{error}");
        }
    }

    #[test]
    fn two_waves_of_one_type_and_one_state_merge() {
        assert!(refuse_unmergeable(&[
            candidate(1, IN_PROGRESS, true, "outgoing", None),
            candidate(2, IN_PROGRESS, true, "outgoing", None),
        ])
        .is_ok());
    }

    #[test]
    fn the_merged_batch_keeps_the_earliest_date() {
        let candidates = vec![
            candidate(1, DRAFT, false, "outgoing", Some("2026-08-10 08:00:00")),
            candidate(2, DRAFT, false, "outgoing", Some("2026-08-04 08:00:00")),
        ];
        assert_eq!(earliest(&candidates, &candidates[0]).id, 2);
        // nobody has a date: the target's own values stand
        let undated = vec![
            candidate(1, DRAFT, false, "outgoing", None),
            candidate(2, DRAFT, false, "outgoing", None),
        ];
        assert_eq!(earliest(&undated, &undated[0]).id, 1);
    }

    #[test]
    fn a_selection_of_two_operation_types_is_refused() {
        let mixed = vec![
            transfer(1, "WH/OUT/00001", "confirmed", "outgoing"),
            transfer(2, "WH/IN/00001", "confirmed", "incoming"),
        ];
        let error = single_operation_type(&mixed).expect_err("one dialog, one operation type");
        assert!(error.to_string().contains("same operation type"));
        // one type, whatever the number of transfers
        let same = vec![
            transfer(1, "WH/OUT/00001", "confirmed", "outgoing"),
            transfer(2, "WH/OUT/00002", "confirmed", "outgoing"),
        ];
        assert_eq!(single_operation_type(&same).unwrap(), "outgoing");
    }

    #[test]
    fn a_selection_spanning_two_companies_is_refused() {
        let mut mixed = vec![
            transfer(1, "WH/OUT/00001", "confirmed", "outgoing"),
            transfer(2, "WH/OUT/00002", "confirmed", "outgoing"),
        ];
        mixed[1].company = Some(2);
        let error = single_company(&mixed).expect_err("a batch belongs to one company");
        assert!(error.to_string().contains("single company"));
    }

    #[test]
    fn a_batch_counts_the_transfers_it_carries() {
        let mut record = Map::new();
        record.insert("picking_ids".into(), json!([4, 5, 6]));
        assert_eq!(picking_count(&record), json!(3));
        // an empty batch counts zero, not null
        assert_eq!(picking_count(&Map::new()), json!(0));
    }

    #[test]
    fn the_models_register_on_top_of_stock() {
        let mut reg = rusdoo_base::registry().unwrap();
        rusdoo_product::extend(&mut reg).unwrap();
        rusdoo_stock::extend(&mut reg).unwrap();
        extend(&mut reg).unwrap();
        for name in [
            "stock.picking.batch",
            "stock.picking.to.batch",
            "stock.add.to.wave",
        ] {
            assert!(reg.get(name).is_some(), "{name} must be registered");
        }
        // the transfer gained the link without losing what stock gave it
        let picking = reg.get("stock.picking").unwrap();
        assert!(picking.field("batch_id").is_some());
        assert!(picking.field("batch_sequence").is_some());
        assert!(picking.field("move_ids").is_some(), "stock's fields stay");
        assert_eq!(picking.meta.table, "stock_picking");
        // both dialogs are dialogs, not stored data
        for name in ["stock.picking.to.batch", "stock.add.to.wave"] {
            assert!(reg.get(name).unwrap().is_transient(), "{name} is a wizard");
        }
        // a batch is numbered, and refuses to be nameless
        let batch = reg.get("stock.picking.batch").unwrap();
        assert!(batch.field("name").unwrap().required);
        assert_eq!(
            batch.field("name").unwrap().sequence.as_deref(),
            Some(BATCH_SEQUENCE)
        );
        assert_eq!(batch.order(), "name desc");
        assert_eq!(batch.unlink_hooks().len(), 1, "a done batch is not deleted");
    }

    #[test]
    fn every_button_the_addon_ships_is_registered() {
        let mut methods = MethodRegistry::new();
        extend_methods(&mut methods).unwrap();
        assert_eq!(
            methods.names_for("stock.picking.batch"),
            vec![
                "action_cancel",
                "action_confirm",
                "action_done",
                "action_merge"
            ]
        );
        assert_eq!(
            methods.names_for("stock.picking"),
            vec![
                "action_add_to_batch",
                "action_add_to_wave",
                "action_view_batch"
            ]
        );
        assert_eq!(
            methods.names_for("stock.picking.to.batch"),
            vec!["attach_pickings"]
        );
        assert_eq!(
            methods.names_for("stock.add.to.wave"),
            vec!["attach_pickings"]
        );
    }

    #[test]
    fn the_addon_does_not_take_over_stocks_own_buttons() {
        // registering twice is an error, so this is what proves the two
        // crates can be loaded side by side
        let mut methods = MethodRegistry::new();
        rusdoo_stock::extend_methods(&mut methods).unwrap();
        extend_methods(&mut methods).expect("the batch adds buttons, it does not replace any");
    }
}
