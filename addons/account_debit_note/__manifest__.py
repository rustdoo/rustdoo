{
    'name': 'Notas de débito',
    'version': '19.0.1.0',
    'category': 'Financeiro',
    'summary': 'Cobra a mais sobre uma fatura lançada, guardando o vínculo com ela',
    'depends': ['base', 'web', 'mail', 'product', 'account'],
    'data': [
        'data/sequences.xml',
        'security/ir.model.access.csv',
        'views/account_debit_note_views.xml',
        'views/account_move_views.xml',
        'views/menus.xml',
    ],
    'installable': True,
}
