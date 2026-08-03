{
    'name': 'Analytic Accounting',
    'version': '19.0.1.0',
    'category': 'Accounting/Accounting',
    'summary': 'Analytic plans, accounts and distribution: what the money was for',
    # `uom` because an analytic line carries a unit of measure. Odoo also
    # depends on `mail`, for the chatter on the analytic account; there is
    # no `mail.thread` on these models here, so the dependency would buy
    # nothing.
    'depends': ['base', 'web', 'uom'],
    'data': [
        'security/analytic_groups.xml',
        'security/ir.model.access.csv',
        'data/analytic_data.xml',
        'views/analytic_plan_views.xml',
        'views/analytic_account_views.xml',
        'views/analytic_line_views.xml',
        'views/analytic_distribution_model_views.xml',
        'views/menus.xml',
    ],
    'installable': True,
}
