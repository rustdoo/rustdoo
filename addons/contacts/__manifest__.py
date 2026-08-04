{
    'name': 'Contatos',
    'version': '19.0.1.0',
    'category': 'Vendas',
    'summary': 'A agenda de pessoas e empresas, como aplicativo',
    'description': """
O modelo `res.partner` é do `base`, porque metade do sistema aponta para
ele. O **aplicativo** — o menu na barra, a lista, o cartão — é este addon,
exatamente como o Odoo separa: quem instala só o `base` tem parceiros
gravados por outros módulos; quem quer uma agenda para abrir e mexer
instala `contacts`.
""",
    'depends': ['base', 'mail'],
    'data': [
        'views/contacts_views.xml',
        'views/menus.xml',
    ],
    'installable': True,
    'application': True,
}
