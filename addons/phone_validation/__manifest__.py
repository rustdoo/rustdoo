{
    'name': 'Phone Numbers Validation',
    'version': '19.0.1.0',
    'category': 'Hidden',
    'summary': 'Validate and format phone numbers, and keep a blacklist of them',
    'description': """
Phone Numbers Validation
========================

Reads a phone number the way its country writes it, and stores it in one
canonical form so that the same telephone is the same string everywhere.

On top of that it keeps a blacklist: the numbers this database has been
asked to stop contacting. The blacklist is matched string against string,
which is exactly why the formatting has to come first.
""",
    'depends': ['base', 'mail'],
    'data': [
        'security/ir.model.access.csv',
        'views/phone_blacklist_views.xml',
        'views/res_partner_views.xml',
        'wizard/phone_blacklist_remove_view.xml',
    ],
    'installable': True,
    'auto_install': True,
    'license': 'LGPL-3',
}
