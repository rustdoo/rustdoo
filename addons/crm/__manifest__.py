{
    'name': 'CRM',
    'version': '19.0.1.0',
    'category': 'Vendas',
    'summary': 'O funil, do nome anotado num papel ao negócio ganho ou perdido',
    'description': """
Um **lead** é um rascunho — um cartão de visita, um formulário preenchido.
Uma **oportunidade** é o mesmo registro depois que alguém decidiu que vale
trabalhar: um modelo só, o `type` diz qual, como no Odoo.

As **etapas** são as colunas do funil, e as duas pontas dele são a razão do
módulo existir: um negócio é ganho numa etapa marcada `is_won`, e perdido
**com um motivo** — "perdemos" sem o porquê é um número do qual ninguém
aprende nada.
""",
    'depends': ['base', 'mail', 'sales_team', 'utm'],
    'data': [
        'security/ir.model.access.csv',
        'data/crm_data.xml',
        'views/crm_views.xml',
        'views/menus.xml',
    ],
    'installable': True,
}
