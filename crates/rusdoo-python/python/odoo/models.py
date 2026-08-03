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
        _rusdoo.declare_model(
            {
                "name": model_name,
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
