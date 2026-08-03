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
                declared.append(value.declare(attr))
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
        _rusdoo.declare_model(
            {
                "name": model_name,
                "methods": methods,
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
        return self._read_one()[name]

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
        answers a recordset of the comodel, so `order.mapped(
        'line_ids.product_id')` reads like it does there.
        """
        head, _, rest = path.partition(".")
        values = []
        for record in self:
            value = getattr(record, head)
            values.append(value)
        if not rest:
            return values
        # a relational value comes back as [id, name] or a list of ids
        return _flatten_relational(self._name, head, values, self._env).mapped(rest)

    def filtered(self, predicate):
        kept = [r.id for r in self if predicate(r)]
        return RecordSet(self._name, kept, self._env)

    def sorted(self, key=None, reverse=False):
        records = sorted(self, key=key, reverse=reverse)
        return RecordSet(self._name, [r.id for r in records], self._env)

    def exists(self):
        found = _rusdoo.search(self._name, [["id", "in", self._ids]], None, None)
        return RecordSet(self._name, found, self._env)


def _flatten_relational(model_name, field, values, env):
    """The comodel recordset behind a relational field's read values."""
    import _rusdoo as native

    comodel = native.comodel_of(model_name, field)
    ids = []
    for value in values:
        if value in (False, None):
            continue
        if isinstance(value, list) and value and isinstance(value[0], int):
            # a many2one reads as [id, name]; an x2many as a list of ids
            ids.extend(value[1:] and [value[0]] or value)
        elif isinstance(value, list):
            ids.extend(value)
        elif isinstance(value, int):
            ids.append(value)
    return RecordSet(comodel, ids, env)


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
