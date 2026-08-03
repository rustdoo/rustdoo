{
    'name': 'Customer Rating',
    'version': '19.0.1.0',
    'category': 'Productivity',
    'summary': 'Lets a customer rate any record, and counts what they said',
    'depends': ['base', 'web', 'mail'],
    'data': [
        'security/ir.model.access.csv',
        'views/rating_rating_views.xml',
        'views/menus.xml',
    ],
    'installable': True,
}
