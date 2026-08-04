{
    'name': 'Pagamento por cheque',
    'version': '19.0.1.0',
    'category': 'Contabilidade',
    'summary': 'Numerar, imprimir e conferir cheques',
    'description': """
Um cheque é um papel com um número, e o número é o problema inteiro. Ou o
papel está em branco e o sistema numera — e aí dois cheques nunca podem
receber o mesmo número, e nenhum pode ser pulado — ou o papel já vem
numerado e o sistema está sendo *informado* dos números, tendo de anotá-los
contra os pagamentos certos, na ordem em que as folhas passam pela
impressora.

Tudo aqui decorre dessa distinção, que o Odoo chama de
`check_manual_sequencing`.

O layout do cheque em si é de outro módulo: o Odoo diz que este "deve ser
usado como dependência de módulos que fornecem modelos de cheque por país",
e sozinho a lista de layouts traz só "Nenhum".
""",
    'depends': ['base', 'account'],
    'data': [
        'security/ir.model.access.csv',
        'data/account_check_printing_data.xml',
        'views/account_check_printing_views.xml',
    ],
    'installable': True,
}
