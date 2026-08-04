{
    'name': 'CRM e Vendas',
    'version': '19.0.1.0',
    'category': 'Vendas',
    'summary': 'Uma oportunidade vira cotação, e o pedido aponta de volta',
    'description': """
Dois aplicativos construídos separados e usados juntos. Alguém trabalha uma
oportunidade até o cliente pedir preço; daí em diante o negócio é uma
cotação, e os dois registros precisam continuar sendo uma coisa só: o
pedido sabe de qual oportunidade veio, a oportunidade sabe quanto vale, e o
funil deixa de ser uma lista de palpites no momento em que há pedidos de
verdade atrás dele.
""",
    'depends': ['base', 'crm', 'sale'],
    'data': ['views/sale_crm_views.xml'],
    'installable': True,
    'auto_install': True,
}
