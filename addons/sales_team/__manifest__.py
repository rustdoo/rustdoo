{
    'name': 'Equipes de venda',
    'version': '19.0.1.0',
    'category': 'Vendas',
    'summary': 'Equipes de venda, seus vendedores e as etiquetas do CRM',
    'depends': ['base', 'web', 'mail'],
    'data': [
        'security/sales_team_groups.xml',
        'security/ir.model.access.csv',
        'data/crm_team_data.xml',
        'views/crm_team_views.xml',
        'views/crm_team_member_views.xml',
        'views/crm_tag_views.xml',
        'views/menus.xml',
    ],
    'installable': True,
}
