"""`odoo.models` — the base classes an addon subclasses.

The metaclass is the whole trick: when Python finishes building a class
that names `_name`, it hands the model's shape to the Rust registry. An
addon does not call anything; it declares a class the way it always did,
and by the time the import returns the model exists on the Rust side.

What crosses is the *declaration*, not behaviour. Methods stay in Python
and are called back later; this file is only about getting the model and
its fields into a registry the Rust ORM can serve.
"""

import _rusdoo

from . import fields as fields_module


#: model name -> the class that defines it.
#:
#: A recordset needs it to find the methods an addon wrote, and the Rust
#: dispatcher needs it for the same reason. Keeping it on the Python side
#: means the class is never handed across as an object — Rust asks for a
#: model and a method by name, which is all `call_kw` ever knows anyway.
MODEL_CLASSES = {}


class MetaModel(type):
    """Registers a model with the Rust side as soon as it is defined."""

    def __new__(mcs, name, bases, namespace):
        cls = super().__new__(mcs, name, bases, namespace)
        inherit = _as_list(namespace.get("_inherit"))
        # `_inherit` with no `_name` extends the model it names, in
        # place: the class *is* that model, with more on it. Odoo's own
        # rule, and the shape almost every addon uses to touch somebody
        # else's model.
        model_name = namespace.get("_name") or (inherit[0] if len(inherit) == 1 else None)
        if not model_name:
            # an abstract base of the addon's own, not a model
            return cls
        declared = []
        for attr, value in namespace.items():
            if isinstance(value, fields_module.Field):
                spec = value.declare(attr)
                if spec.get("compute"):
                    spec["depends"] = _depends_of(cls, namespace, model_name, spec)
                declared.append(spec)
        # `_name` and `_inherit` are read off the class body: inheriting
        # them from a base would make every subclass re-register its
        # parent's model. `_transient` and `_order` are read off the class,
        # because that is where they come from — `TransientModel` sets
        # `_transient` on itself, and Odoo lets a subclass inherit an
        # `_order` it did not restate.
        # the methods an addon wrote, for the dispatcher to reach. A
        # leading underscore means private in Odoo, and `call_kw` refuses
        # those there too — the convention is the access rule.
        methods = sorted(
            attr
            for attr, value in namespace.items()
            if callable(value) and not attr.startswith("_")
        )
        # a class extending a model already registered adds to it rather
        # than replacing it, the same way `_inherit` adds fields
        previous = MODEL_CLASSES.get(model_name)
        if previous is not None and previous is not cls:
            for attr, value in namespace.items():
                if not attr.startswith("__"):
                    setattr(previous, attr, value)
            cls = previous
        else:
            MODEL_CLASSES[model_name] = cls
        # the rules the model's records must satisfy. Unlike a compute,
        # which a field points at, a constraint is found by looking at the
        # methods: `@api.constrains` is the whole declaration.
        constraints = [
            {"method": attr, "fields": list(value._constrains)}
            for attr, value in sorted(namespace.items())
            if callable(value) and getattr(value, "_constrains", None)
        ]
        _rusdoo.declare_model(
            {
                "name": model_name,
                "methods": methods,
                "constraints": constraints,
                # Odoo's own rule: the table is the model with its dots
                # turned into underscores, unless the model says otherwise
                "table": getattr(cls, "_table", None) or model_name.replace(".", "_"),
                "inherit": inherit,
                "order": getattr(cls, "_order", None),
                "transient": bool(getattr(cls, "_transient", False)),
                "fields": declared,
            }
        )
        return cls


def _depends_of(cls, namespace, model_name, spec):
    """What a computed field reads, from its method's `@api.depends`.

    The field declaration cannot know: `fields.Float(compute="_compute_x")`
    names a method that does not exist yet when the field is built. The
    metaclass runs after the whole class body, so by here it does.

    A compute with no `@api.depends` is refused rather than registered
    with an empty list. The ORM reads a computed field by first reading
    what it depends on, so a compute that declared nothing would be
    handed an empty row and answer the same wrong value for every
    record — the kind of wrong that looks right until a report is off.
    """
    method_name = spec["compute"]
    method = namespace.get(method_name) or getattr(cls, method_name, None)
    if not callable(method):
        raise TypeError(
            "%s.%s: compute=%r names no method on the model"
            % (model_name, spec["name"], method_name)
        )
    depends = list(getattr(method, "_depends", ()) or ())
    if not depends:
        raise TypeError(
            "%s.%s: %s has no @api.depends — a compute that declares "
            "nothing is read against an empty record"
            % (model_name, spec["name"], method_name)
        )
    return depends


def _as_list(value):
    if not value:
        return []
    if isinstance(value, str):
        return [value]
    return list(value)


class BaseModel(metaclass=MetaModel):
    """What every model is, in Odoo's own hierarchy."""

    _name = None
    _inherit = None
    _description = None


class Model(BaseModel):
    """A model whose records the business keeps."""


class TransientModel(BaseModel):
    """A wizard: rows are the state of an open dialog."""

    _transient = True


class AbstractModel(BaseModel):
    """A mixin: no table of its own."""


class RecordSet:
    """A handle on records of one model — Odoo's `self`.

    Three things and no data: the model, the ids, and the environment.
    Fields are read when they are asked for, because a recordset is
    handed around long before anyone touches a value, and reading forty
    columns for a caller that wanted one is how an ORM gets slow.

    The read is cached for the life of the recordset, not longer: two
    reads of `self.name` in one method must not be two queries, and a
    recordset that outlived a write must not answer with what was true
    before it.
    """

    __slots__ = ("_name", "_ids", "_env", "_cache")

    def __init__(self, model_name, ids, env):
        object.__setattr__(self, "_name", model_name)
        object.__setattr__(self, "_ids", list(ids))
        object.__setattr__(self, "_env", env)
        object.__setattr__(self, "_cache", {})

    # -- the shape of a recordset ---------------------------------------

    @property
    def env(self):
        return self._env

    @property
    def ids(self):
        return list(self._ids)

    @property
    def id(self):
        """The single id, as Odoo's `record.id`."""
        if len(self._ids) != 1:
            raise ValueError(
                "expected one record, got %d: use .ids for a set" % len(self._ids)
            )
        return self._ids[0]

    def __len__(self):
        return len(self._ids)

    def __bool__(self):
        return bool(self._ids)

    def __iter__(self):
        for one in self._ids:
            yield RecordSet(self._name, [one], self._env)

    def __eq__(self, other):
        return (
            isinstance(other, RecordSet)
            and other._name == self._name
            and set(other._ids) == set(self._ids)
        )

    def __hash__(self):
        return hash((self._name, tuple(sorted(self._ids))))

    def __repr__(self):
        return "%s(%s)" % (self._name, ", ".join(str(i) for i in self._ids))

    # -- reading --------------------------------------------------------

    def __getattr__(self, name):
        # only reached when normal lookup failed, so a method or a
        # property on the model class always wins over a field
        if name.startswith("_"):
            raise AttributeError(name)
        # a method the addon wrote, bound to these records: this is what
        # makes `self.action_confirm()` inside a model mean what it means
        # in Odoo
        declared = getattr(MODEL_CLASSES.get(self._name), name, None)
        if callable(declared):
            return declared.__get__(self, type(self))
        if name not in _rusdoo.fields_of(self._name):
            raise AttributeError(
                "%s has no field %r" % (self._name, name)
            )
        if len(self._ids) != 1:
            raise ValueError(
                "reading %r wants one record, got %d" % (name, len(self._ids))
            )
        value = self._read_one()[name]
        # a relation answers records, not the ids the ORM read. Half of
        # every addon is written `order.partner_id.name`, and a pair
        # `[id, name]` has no `.name` — an addon that hit that would have
        # nothing to fix on its side.
        relation = _rusdoo.relation_of(self._name, name)
        if relation is not None:
            comodel, kind = relation
            return RecordSet(comodel, _ids_in(value, kind), self._env)
        return value

    def __setattr__(self, name, value):
        if name.startswith("_"):
            object.__setattr__(self, name, value)
            return
        self.write({name: value})

    def _read_one(self):
        if not self._cache:
            names = _rusdoo.fields_of(self._name)
            rows = _rusdoo.read(self._name, self._ids, names)
            if not rows:
                raise ValueError(
                    "%s record %s is gone" % (self._name, self._ids)
                )
            object.__setattr__(self, "_cache", rows[0])
        return self._cache

    def read(self, fields=None):
        """The records as dicts, like Odoo's `read`."""
        names = list(fields) if fields else _rusdoo.fields_of(self._name)
        return _rusdoo.read(self._name, self._ids, names)

    # -- the calls an addon makes ---------------------------------------

    def browse(self, ids):
        if isinstance(ids, int):
            ids = [ids]
        return RecordSet(self._name, ids, self._env)

    def search(self, domain=None, limit=None, order=None):
        found = _rusdoo.search(self._name, domain or [], limit, order)
        return RecordSet(self._name, found, self._env)

    def search_count(self, domain=None):
        return len(_rusdoo.search(self._name, domain or [], None, None))

    def create(self, values):
        new_id = _rusdoo.create(self._name, values)
        return RecordSet(self._name, [new_id], self._env)

    def write(self, values):
        _rusdoo.write(self._name, self._ids, values)
        # what was cached described the record before this write
        object.__setattr__(self, "_cache", {})
        return True

    def unlink(self):
        _rusdoo.unlink(self._name, self._ids)
        object.__setattr__(self, "_ids", [])
        object.__setattr__(self, "_cache", {})
        return True

    # -- the set operations addons lean on ------------------------------

    def mapped(self, path):
        """`records.mapped('name')` — the values, in order.

        A dotted path walks relations, as in Odoo. A relational hop
        answers a recordset of the comodel — with the records it reached
        deduplicated, as a set is — so `order.mapped('line_ids.
        product_id')` reads like it does there, and `.mapped('name')` on
        the last hop gives the plain values.
        """
        head, _, rest = path.partition(".")
        values = [getattr(record, head) for record in self]
        relation = _rusdoo.relation_of(self._name, head)
        if relation is None:
            if rest:
                raise ValueError(
                    "%s.%s is not a relation: %r cannot be walked through"
                    % (self._name, head, path)
                )
            return values
        reached = []
        for value in values:
            for one in value._ids:
                if one not in reached:
                    reached.append(one)
        hop = RecordSet(relation[0], reached, self._env)
        return hop.mapped(rest) if rest else hop

    def filtered(self, predicate):
        kept = [r.id for r in self if predicate(r)]
        return RecordSet(self._name, kept, self._env)

    def sorted(self, key=None, reverse=False):
        records = sorted(self, key=key, reverse=reverse)
        return RecordSet(self._name, [r.id for r in records], self._env)

    def exists(self):
        found = _rusdoo.search(self._name, [["id", "in", self._ids]], None, None)
        return RecordSet(self._name, found, self._env)


def _ids_in(value, kind):
    """The ids behind what the ORM read for a relational field.

    The two shapes are told apart by `kind` and never by looking at the
    value: a many2one reads back as the pair `[id, name]`, and a
    many2many holding two records reads back as `[id, id]`. Guessing
    from the shape would take the second for the first exactly when
    there are two of them.
    """
    if not value:
        return []
    if kind == "many2one":
        return [value[0] if isinstance(value, list) else value]
    return [one for one in value if isinstance(one, int)]


class RowRecord:
    """`self` inside a compute or a constraint: one record, and only
    what the decorator declared.

    Both run in the middle of something the ORM already has open — the
    read that asked for the value, the transaction about to commit the
    write — so neither can go back to the database for a field it feels
    like reading. Neither needs to: `@api.depends` and `@api.constrains`
    already named everything the method reads, and the ORM read exactly
    that before calling.

    So the record answers from that row and nothing else. A field the
    method reads without declaring is an error naming the field and the
    decorator to add it to, which is the whole difference between an
    addon somebody fixes in a minute and a total that is quietly wrong.

    Assignment does not write. `record.amount = 10` inside a compute is
    how Odoo spells "this is the value", not a write to the database —
    for a non-stored field there is no column to write to, and for a
    stored one the ORM writes it itself once the compute answers.
    """

    __slots__ = ("_model", "_row", "_pending", "_hint")

    def __init__(self, model_name, row, hint):
        object.__setattr__(self, "_model", model_name)
        object.__setattr__(self, "_row", row)
        object.__setattr__(self, "_pending", {})
        #: the decorator an undeclared read should have been added to
        object.__setattr__(self, "_hint", hint)

    # both are written `for record in self:`, and here `self` is the one
    # record the ORM is asking about
    def __iter__(self):
        yield self

    def __len__(self):
        return 1

    def __bool__(self):
        return True

    def __repr__(self):
        return "%s(%s)" % (self._model, self._row.get("id"))

    @property
    def id(self):
        return self._row.get("id")

    def __getattr__(self, name):
        if name.startswith("_"):
            raise AttributeError(name)
        declared = getattr(MODEL_CLASSES.get(self._model), name, None)
        if callable(declared):
            return declared.__get__(self, type(self))
        if name in self._pending:
            # a compute that assigns one field and then reads it back to
            # derive another reads what it just said, not what the column
            # still holds
            return self._pending[name]
        if name in self._row:
            return self._row[name]
        prefix = name + "."
        if any(key.startswith(prefix) for key in self._row):
            return DependHop(self._model, name, self._row, self._hint)
        raise AttributeError(
            "%s.%s is not readable here: add %r to its %s"
            % (self._model, name, name, self._hint)
        )

    def __setattr__(self, name, value):
        if name.startswith("_"):
            object.__setattr__(self, name, value)
            return
        self._pending[name] = value


class DependHop:
    """What a relational field answers inside a compute.

    `@api.depends('line_ids.subtotal')` made the ORM read the subtotals
    of the lines before the compute ran, keyed by the parent. So
    `record.line_ids.mapped('subtotal')` is answered from that — the
    records themselves never come across, because the compute has no
    connection to fetch them with.
    """

    __slots__ = ("_model", "_field", "_row", "_hint")

    def __init__(self, model_name, field, row, hint):
        self._model = model_name
        self._field = field
        self._row = row
        self._hint = hint

    def _gathered(self, path):
        key = "%s.%s" % (self._field, path)
        if key not in self._row:
            raise AttributeError(
                "%s.%s is not readable here: add %r to its %s"
                % (self._model, key, key, self._hint)
            )
        values = self._row[key]
        return list(values) if isinstance(values, list) else [values]

    def mapped(self, path):
        return self._gathered(path)

    def __len__(self):
        """How many records the relation holds.

        Read off whichever dependency was gathered: they all have one
        value per record, so any of them counts the records.
        """
        prefix = self._field + "."
        for key, values in self._row.items():
            if key.startswith(prefix) and isinstance(values, list):
                return len(values)
        return 0

    def __bool__(self):
        return len(self) > 0

    def __repr__(self):
        return "%s.%s(%d)" % (self._model, self._field, len(self))


def dispatch_compute(model_name, method_name, field_name, row):
    """Run a computed field's method over one record, and hand back the
    value it assigned.

    The Rust side calls this the same way it calls a native compute: the
    row of everything `@api.depends` named goes in, one value comes out.
    """
    cls = MODEL_CLASSES.get(model_name)
    if cls is None:
        raise AttributeError("no Python model named %r" % model_name)
    method = getattr(cls, method_name, None)
    if not callable(method):
        raise AttributeError("%s has no compute %r" % (model_name, method_name))
    record = RowRecord(model_name, row, "@api.depends")
    method(record)
    pending = object.__getattribute__(record, "_pending")
    if field_name not in pending:
        # Odoo raises here too: a compute that returns without assigning
        # leaves the field with no value at all, and answering `False`
        # would hide the bug behind a plausible number
        raise ValueError(
            "%s.%s left %s unassigned" % (model_name, method_name, field_name)
        )
    return pending[field_name]


def dispatch_constraint(model_name, method_name, row):
    """Run an `@api.constrains` method over one record.

    It answers nothing: a constraint that is satisfied returns, and one
    that is not raises — which is the same thing Odoo's does, and what
    the Rust side turns back into a refused write.
    """
    cls = MODEL_CLASSES.get(model_name)
    if cls is None:
        raise AttributeError("no Python model named %r" % model_name)
    method = getattr(cls, method_name, None)
    if not callable(method):
        raise AttributeError("%s has no constraint %r" % (model_name, method_name))
    # the row already holds the record's own columns, not only the
    # watched ones: `@api.constrains` says *when* to check, not what the
    # check may read. A field still missing is one with no column —
    # another model's, reached through a relation — and naming it beats
    # a `None` the message would print as "None".
    method(RowRecord(model_name, row, "@api.constrains"))


def dispatch(model_name, method_name, ids, args, kwargs):
    """Call `model_name.method_name` on `ids`, as `call_kw` would.

    The entry point the Rust side uses. It builds the recordset the
    method expects as `self`, so an addon's method sees exactly what it
    sees in Odoo — `self.ids`, `self.env`, `self.name` — and never learns
    that its caller was not Python.
    """
    from .api import Environment

    cls = MODEL_CLASSES.get(model_name)
    if cls is None:
        raise AttributeError("no Python model named %r" % model_name)
    method = getattr(cls, method_name, None)
    if method is None or not callable(method):
        raise AttributeError("%s has no method %r" % (model_name, method_name))
    records = RecordSet(model_name, ids, Environment())
    return method(records, *(args or []), **(kwargs or {}))
