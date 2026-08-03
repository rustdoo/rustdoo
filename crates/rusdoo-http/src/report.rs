//! Printing a record, port of `ir.actions.report` (`odoo/addons/base/
//! models/ir_actions_report.py`) minus the PDF step.
//!
//! Odoo renders a QWeb template to HTML and hands it to wkhtmltopdf. The
//! engine and the templates are here; the PDF converter is not, so what
//! this serves is the HTML — the same document, printed by the browser
//! instead of by a binary the server would have to ship. When a PDF
//! renderer lands, it renders exactly this.

use crate::dispatch::{OrmService, RpcError};
use crate::session::Session;
use rusdoo_orm::fields::{Field, FieldType};
use serde_json::{json, Map, Value};

/// How many x2many lines a printed document expands. A report is a page
/// somebody reads, not an export.
const MAX_REPORT_LINES: usize = 500;

/// The audit columns (LOG_ACCESS). A printed document says what was
/// agreed, not who typed it into the database — and resolving the two
/// user names would cost a query per report for something no template
/// asks for.
const AUDIT_FIELDS: [&str; 4] = ["create_uid", "create_date", "write_uid", "write_date"];

fn is_printable(field: &Field) -> bool {
    field.exposed && !AUDIT_FIELDS.contains(&field.name.as_str())
}

impl OrmService {
    /// Render report `xml_id` for `res_id`: the record, its lines and
    /// its display names, fed to the QWeb template the report names.
    ///
    /// Every read on the way goes through the caller's access: printing
    /// is a way of reading, and a report must not become the one place
    /// where the ACL does not apply.
    pub async fn render_report(
        &self,
        xml_id: &str,
        res_id: i64,
        session: Option<&Session>,
    ) -> Result<String, RpcError> {
        let (module, name) = xml_id
            .split_once('.')
            .ok_or_else(|| RpcError::invalid_params("a report's external id is module.name"))?;
        let report_id = self
            .resolve_report_id(module, name)
            .await
            .ok_or_else(|| RpcError::invalid_params(format!("report {xml_id} does not exist")))?;
        let reports = self
            .registry
            .read(
                &self.pool,
                "ir.actions.report",
                &[report_id],
                &["name", "model", "report_name"],
            )
            .await?;
        let report = reports
            .first()
            .ok_or_else(|| RpcError::invalid_params("the report is gone"))?;
        let model = report
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
            .ok_or_else(|| RpcError::invalid_params("the report does not say which model it is about"))?;
        let template = report
            .get("report_name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| RpcError::invalid_params("the report names no template"))?;

        if let Some(session) = session {
            self.check_access(model, "read", session)?;
            self.check_access("ir.ui.view", "read", session)?;
        }
        // the record itself is subject to the record rules, like any read
        let uid = session
            .map(|session| session.uid)
            .unwrap_or(crate::session::SUPERUSER_ID);
        self.check_records(uid, model, rusdoo_orm::access::Operation::Read, &[res_id])
            .await?;

        let record = self.report_record(model, res_id, session).await?;
        let mut ctx = Map::new();
        ctx.insert(
            "title".into(),
            report.get("name").cloned().unwrap_or(Value::Null),
        );
        ctx.insert("doc".into(), Value::Object(record));
        // a report reads its own record; `docs` is the list form Odoo's
        // templates iterate over, with the single document in it
        ctx.insert(
            "docs".into(),
            Value::Array(vec![ctx["doc"].clone()]),
        );

        let arch = self.view_arch(template, session).await?;
        let templates = self.collect_templates(&arch, session).await?;
        rusdoo_qweb::render_with(&arch, &Value::Object(ctx), &templates)
            .map_err(RpcError::from)
    }

    /// The record as a template sees it: scalars, resolved many2one
    /// names, and each x2many expanded into its own records.
    async fn report_record(
        &self,
        model: &str,
        res_id: i64,
        session: Option<&Session>,
    ) -> Result<Map<String, Value>, RpcError> {
        let m = self
            .registry
            .get(model)
            .ok_or_else(|| RpcError::invalid_params(format!("modelo desconhecido: {model}")))?;
        let scalars: Vec<&str> = m
            .fields()
            .iter()
            .filter(|f| is_printable(f) && !matches!(f.ty, FieldType::One2many { .. }))
            .map(|f| f.name.as_str())
            .collect();
        let rows = self
            .registry
            .read(&self.pool, model, &[res_id], &scalars)
            .await?;
        let mut record = rows
            .into_iter()
            .next()
            .ok_or_else(|| RpcError::invalid_params(format!("record {res_id} does not exist")))?;

        for field in m.fields() {
            humanize_field(field, &mut record);
        }

        // the lines of each one2many, in their own order
        for field in m.fields() {
            let FieldType::One2many { comodel, .. } = &field.ty else {
                continue;
            };
            if let Some(session) = session {
                // reading the lines is reading their model
                self.check_access(comodel, "read", session)?;
            }
            let ids: Vec<i64> = self
                .registry
                .read(&self.pool, model, &[res_id], &[field.name.as_str()])
                .await?
                .first()
                .and_then(|row| row.get(&field.name))
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(Value::as_i64)
                        .take(MAX_REPORT_LINES)
                        .collect()
                })
                .unwrap_or_default();
            if ids.is_empty() {
                record.insert(field.name.clone(), json!([]));
                continue;
            }
            let line_model = self.registry.get(comodel).ok_or_else(|| {
                RpcError::invalid_params(format!("comodelo desconhecido: {comodel}"))
            })?;
            let line_fields: Vec<&str> = line_model
                .fields()
                .iter()
                .filter(|f| {
                    is_printable(f)
                        && !matches!(
                            f.ty,
                            FieldType::One2many { .. } | FieldType::Many2many { .. }
                        )
                })
                .map(|f| f.name.as_str())
                .collect();
            let mut lines = self
                .registry
                .read(&self.pool, comodel, &ids, &line_fields)
                .await?;
            for line in &mut lines {
                for line_field in line_model.fields() {
                    humanize_field(line_field, line);
                }
            }
            record.insert(field.name.clone(), json!(lines));
        }
        Ok(record)
    }

    /// External id -> `ir.actions.report` id.
    async fn resolve_report_id(&self, module: &str, name: &str) -> Option<i64> {
        let row: Option<(i32,)> = sqlx::query_as(
            r#"SELECT "res_id" FROM "ir_model_data"
               WHERE "module" = $1 AND "name" = $2 AND "model" = 'ir.actions.report'"#,
        )
        .bind(module)
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .ok()?;
        row.map(|(id,)| i64::from(id))
    }
}

/// Turn a stored value into what the printed page should say: a
/// many2one shows the record's name, a selection its label, and money
/// its decimals. A document that prints `draft` and `1250.0` is a
/// document nobody wants to send to a customer.
fn humanize_field(field: &Field, record: &mut Map<String, Value>) {
    match &field.ty {
        FieldType::Many2one { .. } => {
            if let Some(pair) = record.get(&field.name).and_then(Value::as_array).cloned() {
                let label = pair.get(1).cloned().unwrap_or(Value::Null);
                record.insert(field.name.clone(), label);
            }
        }
        FieldType::Selection(options) => {
            let label = record
                .get(&field.name)
                .and_then(Value::as_str)
                .and_then(|value| {
                    options
                        .iter()
                        .find(|(key, _)| key == value)
                        .map(|(_, label)| label.clone())
                });
            if let Some(label) = label {
                record.insert(field.name.clone(), Value::from(label));
            }
        }
        FieldType::Float {
            digits: Some((_, scale)),
        } => {
            if let Some(number) = record.get(&field.name).and_then(Value::as_f64) {
                let text = format!("{number:.*}", *scale as usize);
                record.insert(field.name.clone(), Value::from(text));
            }
        }
        FieldType::Monetary => {
            if let Some(number) = record.get(&field.name).and_then(Value::as_f64) {
                record.insert(field.name.clone(), Value::from(format!("{number:.2}")));
            }
        }
        _ => {}
    }
}
