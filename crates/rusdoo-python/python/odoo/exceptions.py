"""`odoo.exceptions` — the ways an addon refuses.

Odoo's hierarchy, and the reason it is a hierarchy rather than one
exception with a message: what the server does next differs. A
`UserError` is the business saying no and the user reading why; an
`AccessError` is a door that stays shut and must not describe what is
behind it; a `MissingError` is a record somebody else already deleted,
which a client can recover from by reloading.

Each of these maps onto the matching `RusdooError` variant when it
crosses back to Rust, so a refusal keeps its kind all the way to the
client. An exception that is *not* one of these — a `KeyError` in an
addon's own code — crosses as a validation error with its traceback,
because that is a bug and the traceback is the useful part.
"""


class RusdooException(Exception):
    """The base of every refusal, Odoo's `UserError` ancestor."""


class UserError(RusdooException):
    """The business says no, and the user reads why."""


class ValidationError(UserError):
    """A record does not satisfy a rule — `@api.constrains` raises this."""


class AccessError(UserError):
    """The acting user may not do this."""


class AccessDenied(AccessError):
    """Authentication failed. Deliberately without detail."""

    def __init__(self, message="Access denied"):
        super().__init__(message)


class MissingError(UserError):
    """The record is gone: somebody else deleted it first."""


class CacheMiss(RusdooException):
    """A field was read that was never loaded."""


#: Odoo's own alias, kept so an addon importing it still imports.
Warning = UserError

__all__ = [
    "RusdooException",
    "UserError",
    "ValidationError",
    "AccessError",
    "AccessDenied",
    "MissingError",
    "CacheMiss",
    "Warning",
]
