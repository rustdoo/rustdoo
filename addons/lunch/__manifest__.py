{
    'name': 'Almoço',
    'version': '19.0.1.0',
    'category': 'Recursos Humanos',
    'summary': 'Pedidos de almoço, fornecedores e a carteira de cada um',
    'description': """
Quem pede, o que pede, de quem, e quanto já gastou. Um pedido passa de
`novo` a `pedido` quando o escritório manda a lista ao fornecedor, e a
carteira de cada pessoa é a soma dos lançamentos — o que ela pediu de um
lado, o que ela pagou do outro.
""",
    'depends': ['base'],
    'data': [
        'security/ir.model.access.csv',
        'views/lunch_views.xml',
        'views/menus.xml',
    ],
    'installable': True,
}
