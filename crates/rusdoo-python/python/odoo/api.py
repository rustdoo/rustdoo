"""`odoo.api` — the environment and the decorators an addon writes.

The environment is what `self.env` is: something you index by model name
to get an empty recordset of it. Everything else an addon does starts
from there.

The decorators record what they are told and hand it back unchanged: the
metaclass reads the mark off the method afterwards, which is the only
moment both the field declaration and the method it names exist.

`@api.depends` and `@api.constrains` are acted on — a computed field is
computed by the method it points at, and a rule refuses the write that
breaks it. `@api.onchange` and `@api.ondelete` are still only recorded;
they are kept rather than dropped because a decorator that vanished
would turn a rule into silence, and that is the kind of wrong that looks
right for months.
"""

import _rusdoo


class Environment:
    """`self.env`: the models, and who is asking."""

    def __getitem__(self, model_name):
        from .models import RecordSet

        return RecordSet(model_name, [], self)

    @property
    def uid(self):
        return _rusdoo.uid()

    @property
    def user(self):
        return self["res.users"].browse(self.uid)

    @property
    def company(self):
        """The company the acting user works for.

        Odoo's `env.company` is the *active* company, which the client
        picks out of the ones the user is allowed. There is no such
        switch here yet, so it is the user's own — the same answer for
        every user who has one company, which is most of them.
        """
        company = self.user.company_id
        if not company:
            raise ValueError("user %s belongs to no company" % self.uid)
        return company

    def ref(self, xml_id, raise_if_not_found=True):
        """The record an external id names, as Odoo's `env.ref`."""
        from .models import RecordSet

        found = _rusdoo.ref(xml_id)
        if found is None:
            if raise_if_not_found:
                raise ValueError("no record has the external id %r" % xml_id)
            return None
        model_name, res_id = found
        return RecordSet(model_name, [res_id], self)


def _tag(**marks):
    """A decorator that records what it was told, and changes nothing."""

    def decorate(method):
        for name, value in marks.items():
            setattr(method, name, value)
        return method

    return decorate


def depends(*fields):
    return _tag(_depends=fields)


def constrains(*fields):
    return _tag(_constrains=fields)


def onchange(*fields):
    return _tag(_onchange=fields)


def ondelete(at_uninstall=False):
    return _tag(_ondelete=True, _ondelete_at_uninstall=at_uninstall)


def model(method):
    return _tag(_api_model=True)(method)


def model_create_multi(method):
    return _tag(_api_model_create_multi=True)(method)


def autovacuum(method):
    return _tag(_autovacuum=True)(method)
