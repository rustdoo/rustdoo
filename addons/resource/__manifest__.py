{
    'name': 'Recursos',
    'version': '19.0.1.0',
    'category': 'Oculto',
    'summary': 'Quando as pessoas e as máquinas estão disponíveis',
    'description': """
Um **recurso** é qualquer coisa que se agenda: um desenvolvedor, um centro
de trabalho, uma sala. Uma **agenda de trabalho** (`resource.calendar`) diz
que horas de que dias ele está disponível, e as **ausências**
(`resource.calendar.leaves`) tiram horas de volta.

Todo módulo de planejamento do Odoo pergunta as mesmas quatro coisas a este
aqui: quantas horas cabem entre dois momentos, quantos dias são esses,
quando N horas terminam e quando N dias terminam.
""",
    'depends': ['base'],
    'data': [
        'security/ir.model.access.csv',
        'data/resource_data.xml',
        'views/resource_views.xml',
        'views/menus.xml',
    ],
    'installable': True,
}
