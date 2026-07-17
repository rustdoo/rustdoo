//! The Odoo 19 web client's read path: `web_read` / `web_search_read`
//! (`odoo/addons/web/models/models.py`). A *specification* maps each field
//! to a sub-spec that shapes relational values: many2one becomes an
//! `{id, display_name, ...}` object, x2many a list of nested records.

use crate::dispatch::{OrmService, RpcError};
use rusdoo_orm::crud::SearchOptions;
use rusdoo_orm::domain::{parse_domain, Domain};
use rusdoo_orm::fields::FieldType;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;

/// The spec is client-controlled; nesting deeper than this is refused.
const MAX_WEB_SPEC_DEPTH: usize = 8;

/// Total field entries a spec may carry. Every relational entry triggers a
/// recursive read, so an uncapped tree is a query-amplification DoS: a few
/// mutually-related models repeated at each level multiply into hundreds of
/// thousands of sequential SQL round trips from one request. The depth cap
/// alone does not bound that — width does.
const MAX_WEB_SPEC_NODES: usize = 200;

/// One field's entry in a `web_read` specification.
#[derive(Debug, Clone, Default)]
pub(crate) struct SubSpec {
    /// Nested specification for a relational field. `None` (no `fields`
    /// key) leaves the value raw: the plain id for many2one, the id list
    /// for x2many.
    fields: Option<WebSpec>,
    /// Per-record cap on how many x2many records are read in full; ids
    /// beyond it come back as `{id}` stubs.
    limit: Option<usize>,
}

pub(crate) type WebSpec = HashMap<String, SubSpec>;

/// Parse the `specification` argument. Absent means "just the ids".
pub(crate) fn parse_web_spec(raw: Option<&Value>) -> Result<WebSpec, RpcError> {
    let mut nodes = 0;
    parse_spec_level(raw, 0, &mut nodes)
}

fn parse_spec_level(
    raw: Option<&Value>,
    depth: usize,
    nodes: &mut usize,
) -> Result<WebSpec, RpcError> {
    if depth > MAX_WEB_SPEC_DEPTH {
        return Err(RpcError::invalid_params(format!(
            "specification nests deeper than {MAX_WEB_SPEC_DEPTH} levels"
        )));
    }
    match raw {
        None => Ok(WebSpec::new()),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(name, sub)| {
                *nodes += 1;
                if *nodes > MAX_WEB_SPEC_NODES {
                    return Err(RpcError::invalid_params(format!(
                        "specification carries more than {MAX_WEB_SPEC_NODES} fields"
                    )));
                }
                Ok((name.clone(), parse_sub_spec(sub, depth, nodes)?))
            })
            .collect(),
        Some(_) => Err(RpcError::invalid_params("specification must be an object")),
    }
}

fn parse_sub_spec(raw: &Value, depth: usize, nodes: &mut usize) -> Result<SubSpec, RpcError> {
    let Value::Object(map) = raw else {
        return Err(RpcError::invalid_params(
            "each field specification must be an object",
        ));
    };
    let mut spec = SubSpec::default();
    for (key, value) in map {
        match key.as_str() {
            "fields" => spec.fields = Some(parse_spec_level(Some(value), depth + 1, nodes)?),
            "limit" => {
                spec.limit = Some(value.as_u64().ok_or_else(|| {
                    RpcError::invalid_params("specification limit must be a positive integer")
                })? as usize);
            }
            // `context` switches language/records and `order` re-sorts
            // them: silently ignoring either would return different data
            // than the client asked for, so refuse until implemented
            other => {
                return Err(RpcError::invalid_params(format!(
                    "web specification key {other:?} is not supported yet"
                )))
            }
        }
    }
    Ok(spec)
}

/// Distinct ids in first-seen order.
fn ordered_ids(iter: impl Iterator<Item = i64>) -> Vec<i64> {
    let mut seen = HashSet::new();
    iter.filter(|id| seen.insert(*id)).collect()
}

/// The `[id, display_name]` pair the ORM read produces for a many2one.
fn m2o_pair(value: Option<&Value>) -> Option<(i64, String)> {
    let arr = value?.as_array()?;
    Some((
        arr.first()?.as_i64()?,
        arr.get(1)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    ))
}

/// The id list the ORM read produces for an x2many.
fn x2many_ids(value: Option<&Value>) -> Vec<i64> {
    value
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

impl OrmService {
    /// Odoo's `name_search`: records whose display name matches `pattern`
    /// under `operator`, further restricted by `extra`, as
    /// `(id, display_name)` pairs. The display name is the record's
    /// `name`/`display_name` column (name_get precedence, resolved
    /// sudo-like as everywhere else), or the id when there is neither.
    pub(crate) async fn name_search_pairs(
        &self,
        model: &str,
        pattern: &str,
        extra: Option<&Value>,
        operator: &str,
        limit: u64,
    ) -> Result<Vec<(i64, String)>, RpcError> {
        let m = self
            .registry
            .get(model)
            .ok_or_else(|| RpcError::invalid_params(format!("unknown model: {model}")))?;
        let rec_name = if m.field("name").is_some() {
            Some("name")
        } else if m.field("display_name").is_some() {
            Some("display_name")
        } else {
            None
        };
        let name_domain = if pattern.is_empty() {
            // the dropdown's initial state: no pattern, everything matches
            Domain::True
        } else {
            let Some(col) = rec_name else {
                // nothing to match a non-empty pattern against
                return Ok(Vec::new());
            };
            // the operator is client-supplied; parse_domain validates it
            parse_domain(&json!([[col, operator, pattern]]))?
        };
        let extra_domain = match extra {
            None => Domain::True,
            Some(value) => parse_domain(value)?,
        };
        let opts = SearchOptions {
            limit: Some(limit),
            ..SearchOptions::default()
        };
        let ids = self
            .registry
            .search(
                &self.pool,
                model,
                &Domain::And(vec![name_domain, extra_domain]),
                &opts,
            )
            .await?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let Some(col) = rec_name else {
            return Ok(ids.iter().map(|id| (*id, id.to_string())).collect());
        };
        let rows = self.registry.read(&self.pool, model, &ids, &[col]).await?;
        let by_id: HashMap<i64, String> = rows
            .iter()
            .filter_map(|r| {
                Some((
                    r.get("id")?.as_i64()?,
                    r.get(col)
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ))
            })
            .collect();
        Ok(ids
            .iter()
            .map(|id| {
                (
                    *id,
                    by_id.get(id).cloned().unwrap_or_else(|| id.to_string()),
                )
            })
            .collect())
    }

    /// `web_read`: read `spec`'s fields on `ids` and shape relational
    /// values by their sub-spec. Rows come back in `ids` order (the search
    /// order when called from `web_search_read`).
    pub(crate) fn web_read_records<'a>(
        &'a self,
        ident: &'a crate::session::Session,
        model: &'a str,
        ids: &'a [i64],
        spec: &'a WebSpec,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Value>, RpcError>> + Send + 'a>> {
        Box::pin(async move {
            if ids.is_empty() {
                return Ok(Vec::new());
            }
            if spec.is_empty() {
                // ids only — no read on the model at all, like Odoo
                return Ok(ids.iter().map(|id| json!({ "id": id })).collect());
            }
            let m = self
                .registry
                .get(model)
                .ok_or_else(|| RpcError::invalid_params(format!("unknown model: {model}")))?;
            // `id` is not a registry field — every read returns it anyway
            let mut names: Vec<String> = spec.keys().filter(|n| *n != "id").cloned().collect();
            // display_name is computed on every Odoo model; when it is not
            // a real column here, synthesize it from the record's `name`
            // (name_get semantics — the id when there is no name either)
            let synth_display =
                spec.contains_key("display_name") && m.field("display_name").is_none();
            let mut borrowed_name = false;
            if synth_display {
                names.retain(|n| n != "display_name");
            }
            // exposure is checked on what the client asked for, BEFORE the
            // display_name synthesis borrows `name` below: display_name is
            // resolvable even from a private name (Odoo computes it with
            // sudo), but that borrow must never make an explicit request
            // for a private `name` fail — or pass
            self.ensure_exposed(model, &names)?;
            if synth_display && m.field("name").is_some() && !names.iter().any(|n| n == "name") {
                names.push("name".into());
                borrowed_name = true;
            }
            let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let rows = self
                .registry
                .read(&self.pool, model, ids, &name_refs)
                .await?;
            // the SQL IN gives no order guarantee; restore the ids order
            let mut by_id: HashMap<i64, Map<String, Value>> = rows
                .into_iter()
                .filter_map(|r| Some((r.get("id")?.as_i64()?, r)))
                .collect();
            let mut records: Vec<Map<String, Value>> =
                ids.iter().filter_map(|id| by_id.remove(id)).collect();
            if synth_display {
                for record in &mut records {
                    let display = record
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            record
                                .get("id")
                                .and_then(Value::as_i64)
                                .map(|id| id.to_string())
                                .unwrap_or_default()
                        });
                    if borrowed_name {
                        record.remove("name");
                    }
                    record.insert("display_name".into(), Value::from(display));
                }
            }
            for (name, sub) in spec {
                let Some(field) = m.field(name) else { continue };
                match &field.ty {
                    FieldType::Many2one { comodel } => {
                        self.shape_many2one(ident, comodel, name, sub, &mut records)
                            .await?;
                    }
                    FieldType::One2many { comodel, .. } | FieldType::Many2many { comodel, .. } => {
                        self.shape_x2many(ident, comodel, name, sub, &mut records)
                            .await?;
                    }
                    _ => {}
                }
            }
            Ok(records.into_iter().map(Value::Object).collect())
        })
    }

    /// Shape a many2one column in place. Without a `fields` sub-spec the
    /// value degrades to the raw id (`false` when empty), like Odoo's
    /// `read(load=None)`. With one, it becomes `{id, display_name, ...}`:
    /// display_name comes from the read's `[id, name]` pair (Odoo resolves
    /// it with sudo), while any other sub-field is a normal ACL-checked
    /// read on the comodel.
    async fn shape_many2one(
        &self,
        ident: &crate::session::Session,
        comodel: &str,
        name: &str,
        sub: &SubSpec,
        records: &mut [Map<String, Value>],
    ) -> Result<(), RpcError> {
        let Some(sub_fields) = &sub.fields else {
            for record in records.iter_mut() {
                let raw = m2o_pair(record.get(name));
                record.insert(
                    name.into(),
                    raw.map_or(Value::Bool(false), |(id, _)| json!(id)),
                );
            }
            return Ok(());
        };
        let mut extra = sub_fields.clone();
        extra.remove("id");
        let wants_display = extra.remove("display_name").is_some();
        let nested: HashMap<i64, Map<String, Value>> = if extra.is_empty() {
            HashMap::new()
        } else {
            self.check_access(comodel, "read", ident)?;
            let co_ids = ordered_ids(
                records
                    .iter()
                    .filter_map(|r| m2o_pair(r.get(name)))
                    .map(|(id, _)| id),
            );
            self.web_read_records(ident, comodel, &co_ids, &extra)
                .await?
                .into_iter()
                .filter_map(|v| match v {
                    Value::Object(m) => Some((m.get("id")?.as_i64()?, m)),
                    _ => None,
                })
                .collect()
        };
        for record in records.iter_mut() {
            let Some((id, display)) = m2o_pair(record.get(name)) else {
                record.insert(name.into(), Value::Bool(false));
                continue;
            };
            let mut obj = nested.get(&id).cloned().unwrap_or_default();
            obj.insert("id".into(), json!(id));
            if wants_display {
                obj.insert("display_name".into(), json!(display));
            }
            record.insert(name.into(), Value::Object(obj));
        }
        Ok(())
    }

    /// Shape an x2many column in place. Without a `fields` sub-spec the id
    /// list stays as-is. With one, ids become nested records; the
    /// per-record `limit` caps how many are read in full — ids beyond it
    /// stay `{id}` stubs, like Odoo.
    async fn shape_x2many(
        &self,
        ident: &crate::session::Session,
        comodel: &str,
        name: &str,
        sub: &SubSpec,
        records: &mut [Map<String, Value>],
    ) -> Result<(), RpcError> {
        let Some(sub_fields) = &sub.fields else {
            return Ok(());
        };
        // `fields: {}` (or only `id`) reads nothing from the comodel, so no
        // access on it is required — Odoo shapes the ids it already has
        // into `{id}` stubs even when the comodel itself is unreadable
        if !sub_fields.keys().any(|k| k != "id") {
            for record in records.iter_mut() {
                let stubs: Vec<Value> = x2many_ids(record.get(name))
                    .into_iter()
                    .map(|id| json!({ "id": id }))
                    .collect();
                record.insert(name.into(), Value::from(stubs));
            }
            return Ok(());
        }
        // reading real comodel fields requires read access on it
        self.check_access(comodel, "read", ident)?;
        let co_ids = ordered_ids(records.iter().flat_map(|r| {
            let ids = x2many_ids(r.get(name));
            match sub.limit {
                Some(limit) => ids.into_iter().take(limit).collect::<Vec<_>>(),
                None => ids,
            }
        }));
        let nested: HashMap<i64, Value> = self
            .web_read_records(ident, comodel, &co_ids, sub_fields)
            .await?
            .into_iter()
            .filter_map(|v| Some((v.get("id")?.as_i64()?, v)))
            .collect();
        for record in records.iter_mut() {
            let shaped: Vec<Value> = x2many_ids(record.get(name))
                .into_iter()
                .map(|id| {
                    nested
                        .get(&id)
                        .cloned()
                        .unwrap_or_else(|| json!({ "id": id }))
                })
                .collect();
            record.insert(name.into(), Value::from(shaped));
        }
        Ok(())
    }
}
