{
    'name': 'Passos de onboarding',
    'version': '19.0.1.0',
    'category': 'Hidden',
    'summary': 'A lista de configuração que cada empresa percorre uma vez',
    'depends': ['base', 'web'],
    'data': [
        'security/ir.model.access.csv',
        'views/onboarding_views.xml',
        'views/menus.xml',
    ],
    'installable': True,
}
