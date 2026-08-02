{
    'name': 'Faturamento',
    'version': '19.0.1.0',
    'category': 'Financeiro',
    'summary': 'Faturas de cliente: linhas, totais e lançamento',
    'depends': ['base', 'web', 'mail', 'product'],
    'data': [
        'data/sequences.xml',
        'security/account_groups.xml',
        'security/ir.model.access.csv',
        'views/account_move_views.xml',
        'views/menus.xml',
    ],
    'installable': True,
}
