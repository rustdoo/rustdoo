//! Mirrors `odoo.models.BaseModel` metadata and its field registry.

use crate::fields::Field;

#[derive(Debug, Clone)]
pub struct ModelMeta {
    /// Odoo model name, e.g. `res.partner`
    pub name: String,
    /// PostgreSQL table, e.g. `res_partner`
    pub table: String,
    /// `_inherit` chain
    pub inherit: Vec<String>,
    /// `_inherits` delegation: (parent model, many2one link field)
    pub inherits: Vec<(String, String)>,
}

/// A check whose behaviour needs state of its own — the one an addon
/// wrote in Python, which has to know *which* check it is.
///
/// It answers with a [`RusdooError`] rather than a string because a
/// Python check raises, and which exception it raised is part of the
/// answer: `ValidationError` and `AccessError` are different things for
/// a client to show.
pub trait DynConstraint: Send + Sync {
    fn check(
        &self,
        row: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), rusdoo_core::RusdooError>;
}

/// How a record is checked.
#[derive(Clone)]
pub enum ConstraintFn {
    /// compiled into the server; `Err(reason)` is the text the user reads
    Native(fn(&serde_json::Map<String, serde_json::Value>) -> Result<(), String>),
    /// written somewhere the compiler cannot see — an addon's Python
    Dynamic(std::sync::Arc<dyn DynConstraint>),
}

impl ConstraintFn {
    pub fn check(
        &self,
        row: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), rusdoo_core::RusdooError> {
        match self {
            ConstraintFn::Native(check) => check(row).map_err(rusdoo_core::RusdooError::Validation),
            ConstraintFn::Dynamic(check) => check.check(row),
        }
    }
}

/// A rule a record must satisfy, port of `@api.constrains`.
///
/// It watches fields and answers with what is wrong, not with a bool: a
/// record refused without a reason is a screen the user cannot fix.
#[derive(Clone)]
pub struct Constraint {
    /// what it is called, for the log and for a message with no text
    pub name: String,
    /// the fields whose change makes it worth checking again
    pub fields: Vec<String>,
    /// the fields read into the row the check is handed.
    ///
    /// Not the same list as `fields`, and Odoo is the reason:
    /// `@api.constrains` says *when* to check, while `self` inside the
    /// check is an ordinary record the rule may read anything from. A
    /// Rust check declares both at once because it reads exactly what it
    /// watches; a bridged one asks for the record's own columns, because
    /// its message almost always names a field other than the one that
    /// changed.
    pub reads: Vec<String>,
    /// `Ok` when the record is fine, an error saying why when it is not
    pub check: ConstraintFn,
}

impl std::fmt::Debug for Constraint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Constraint")
            .field("name", &self.name)
            .field("fields", &self.fields)
            .finish_non_exhaustive()
    }
}

/// What an `@api.onchange` body does: values in, the values it changed
/// out.
///
/// It works on what the form holds, not on what the database holds —
/// the record being edited may never have been saved, and the whole
/// point of an onchange is to answer before it is.
pub trait DynOnchange: Send + Sync {
    fn call(
        &self,
        values: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Map<String, serde_json::Value>, rusdoo_core::RusdooError>;
}

/// A form's reaction to an edit, port of `@api.onchange`.
///
/// Unlike a constraint, this refuses nothing and writes nothing: it
/// answers what *else* changes when the user touches a field, so the
/// form can show it before anything is saved.
#[derive(Clone)]
pub struct Onchange {
    /// what it is called, for the log
    pub name: String,
    /// the fields whose edit triggers it
    pub fields: Vec<String>,
    pub apply: std::sync::Arc<dyn DynOnchange>,
}

impl std::fmt::Debug for Onchange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Onchange")
            .field("name", &self.name)
            .field("fields", &self.fields)
            .finish_non_exhaustive()
    }
}

/// A rule the database enforces, port of Odoo's `_sql_constraints`.
///
/// The difference from a `Constraint` is not style: a Rust check runs
/// before the `INSERT` and two concurrent requests can both pass it and
/// both write. A `UNIQUE` in PostgreSQL cannot be raced past.
#[derive(Debug, Clone)]
pub struct SqlConstraint {
    /// the constraint's name in the database
    pub name: String,
    /// what follows `ADD CONSTRAINT <name>`, e.g. `UNIQUE ("key")`
    pub definition: String,
    /// what the user is told when the database refuses the write
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct Model {
    pub meta: ModelMeta,
    fields: Vec<Field>,
    constraints: Vec<Constraint>,
    /// a `TransientModel`: its records are a dialog somebody had open,
    /// not data the business keeps
    transient: bool,
    /// `_order`: how a search with no order of its own comes back
    order: Option<String>,
    /// `_sql_constraints`: the rules the database itself enforces
    sql_constraints: Vec<SqlConstraint>,
    /// `@api.ondelete`: what must be true before a record may be deleted
    unlink_hooks: Vec<crate::unlink::UnlinkHook>,
    hooks: Vec<crate::hooks::Hook>,
    /// `@api.onchange`: what else changes when the user edits a field
    onchanges: Vec<Onchange>,
}

impl Model {
    pub fn new(meta: ModelMeta, fields: Vec<Field>) -> Self {
        Model {
            meta,
            fields,
            constraints: Vec::new(),
            transient: false,
            order: None,
            sql_constraints: Vec::new(),
            unlink_hooks: Vec::new(),
            hooks: Vec::new(),
            onchanges: Vec::new(),
        }
    }

    /// React to an edit of `fields`, port of `@api.onchange`.
    pub fn on_change(
        mut self,
        name: &str,
        fields: Vec<String>,
        apply: std::sync::Arc<dyn DynOnchange>,
    ) -> Self {
        self.onchanges.push(Onchange {
            name: name.to_string(),
            fields,
            apply,
        });
        self
    }

    pub fn onchanges(&self) -> &[Onchange] {
        &self.onchanges
    }

    /// The onchanges of this model's `_inherit` parents, ahead of its own
    /// — a module that extends a model keeps the reactions it inherited.
    pub(crate) fn with_onchanges(mut self, mut inherited: Vec<Onchange>) -> Self {
        inherited.extend(self.onchanges.iter().cloned());
        self.onchanges = inherited;
        self
    }

    /// Refuse a delete when `check` says so, port of `@api.ondelete`.
    ///
    /// A foreign key answers "what happens to what points at this"; this
    /// answers "may this go at all", which only the business knows — a
    /// confirmed order is not deletable even when nothing references it.
    pub fn on_unlink(self, name: &str, check: crate::unlink::OnDeleteFn) -> Self {
        self.on_unlink_with(name, crate::unlink::UnlinkCheck::Native(check))
    }

    /// [`Model::on_unlink`] for a check the compiler cannot see — the one
    /// an addon wrote in Python.
    pub fn on_unlink_with(mut self, name: &str, check: crate::unlink::UnlinkCheck) -> Self {
        self.unlink_hooks.push(crate::unlink::UnlinkHook {
            name: name.to_string(),
            check,
        });
        self
    }

    pub fn unlink_hooks(&self) -> &[crate::unlink::UnlinkHook] {
        &self.unlink_hooks
    }

    /// React to this model's own create, port of Odoo's `create`
    /// override. The record already exists and its values are written:
    /// a hook reacts, it does not intercept.
    pub fn on_create(self, name: &str, run: crate::hooks::HookFn) -> Self {
        self.with_hook(name, crate::hooks::HookOn::Create, crate::hooks::HookBody::Native(run))
    }

    /// The same for a write. It runs for *every* write, so a hook that
    /// cares about one field asks `ctx.wrote(field)` first.
    pub fn on_write(self, name: &str, run: crate::hooks::HookFn) -> Self {
        self.with_hook(name, crate::hooks::HookOn::Write, crate::hooks::HookBody::Native(run))
    }

    /// [`Model::on_create`] / [`Model::on_write`] for a body the compiler
    /// cannot see — the one an addon wrote in Python.
    pub fn with_hook(
        mut self,
        name: &str,
        on: crate::hooks::HookOn,
        body: crate::hooks::HookBody,
    ) -> Self {
        self.hooks.push(crate::hooks::Hook {
            name: name.to_string(),
            on,
            body,
        });
        self
    }

    pub fn hooks(&self) -> &[crate::hooks::Hook] {
        &self.hooks
    }

    /// The inherited hooks, ahead of this model's own: an `_inherit`
    /// adds behaviour, it does not silence the parent's.
    pub(crate) fn with_hooks(mut self, mut inherited: Vec<crate::hooks::Hook>) -> Self {
        inherited.extend(std::mem::take(&mut self.hooks));
        self.hooks = inherited;
        self
    }

    /// The inherited hooks, ahead of this model's own.
    pub(crate) fn with_unlink_hooks(
        mut self,
        mut inherited: Vec<crate::unlink::UnlinkHook>,
    ) -> Self {
        inherited.extend(self.unlink_hooks.iter().cloned());
        self.unlink_hooks = inherited;
        self
    }

    /// Attach a rule the database enforces, port of `_sql_constraints`.
    ///
    /// `definition` is the SQL that follows `ADD CONSTRAINT <name>`, and
    /// `message` is what the user reads when the write is refused — a
    /// raw `duplicate key value violates unique constraint
    /// "ir_config_parameter_key_uniq"` is not a sentence anyone can act
    /// on.
    pub fn sql_constrained(mut self, name: &str, definition: &str, message: &str) -> Self {
        self.sql_constraints.push(SqlConstraint {
            name: name.to_string(),
            definition: definition.to_string(),
            message: message.to_string(),
        });
        self
    }

    pub fn sql_constraints(&self) -> &[SqlConstraint] {
        &self.sql_constraints
    }

    /// Mark the model transient (Odoo's `TransientModel`): the rows are
    /// the state of a dialog, and old ones are swept away instead of
    /// being kept forever.
    pub fn transient(mut self) -> Self {
        self.transient = true;
        self
    }

    pub fn is_transient(&self) -> bool {
        self.transient
    }

    /// Attach a rule every record of this model must satisfy.
    pub fn constrained(
        self,
        name: &str,
        fields: &[&str],
        check: fn(&serde_json::Map<String, serde_json::Value>) -> Result<(), String>,
    ) -> Self {
        let fields: Vec<String> = fields.iter().map(|f| (*f).to_string()).collect();
        self.constrained_with(name, fields.clone(), fields, ConstraintFn::Native(check))
    }

    /// [`Model::constrained`] for a check the compiler cannot see — the
    /// one an addon wrote in Python, which watches one list and reads
    /// another. See [`Constraint::reads`].
    pub fn constrained_with(
        mut self,
        name: &str,
        fields: Vec<String>,
        reads: Vec<String>,
        check: ConstraintFn,
    ) -> Self {
        self.constraints.push(Constraint {
            name: name.to_string(),
            fields,
            reads,
            check,
        });
        self
    }

    pub fn constraints(&self) -> &[Constraint] {
        &self.constraints
    }

    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }

    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// Port of `_order`: how a search that names no order of its own
    /// comes back. `"date desc, id desc"` reads like Odoo's.
    ///
    /// Without it a `SELECT` with no `ORDER BY` may answer in any order
    /// PostgreSQL likes — which usually looks like insertion order right
    /// up to the first `UPDATE`, and then quietly stops.
    pub fn ordered(mut self, order: &str) -> Self {
        self.order = Some(order.to_string());
        self
    }

    /// What this model declared, if anything — what an `_inherit` child
    /// falls back to when it declares no order of its own.
    pub(crate) fn declared_order(&self) -> Option<&str> {
        self.order.as_deref()
    }

    /// The order to search by: the declared one, or `id` like Odoo.
    pub fn order(&self) -> &str {
        self.order.as_deref().unwrap_or("id")
    }

    /// The same model with its identity, fields and rules replaced —
    /// what the registry does after folding an `_inherit` chain.
    ///
    /// It rebuilds *from* the model instead of constructing a new one so
    /// that everything the caller declared and this function says nothing
    /// about is carried over untouched. The previous shape took the model
    /// apart and put a new one together, which meant every attribute
    /// added later had to be threaded through by hand — and the first one
    /// that was not, the constraints, was silently dropped for a while.
    pub(crate) fn rebuilt(
        mut self,
        meta: ModelMeta,
        fields: Vec<Field>,
        constraints: Vec<Constraint>,
    ) -> Self {
        self.meta = meta;
        self.fields = fields;
        self.constraints = constraints;
        self
    }
}
