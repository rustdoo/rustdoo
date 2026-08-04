{
    'name': 'Valoração de estoque',
    'version': '19.0.1.0',
    'category': 'Contabilidade',
    'summary': 'Quanto vale o que entrou e o que saiu do estoque',
    'description': """
O `stock` diz que um produto foi de um lugar para outro. Este módulo diz
quanto isso *valia*: a entrega que saiu do armazém hoje de manhã tirou um
número do ativo da empresa, e qual número depende de uma política — o custo
do produto, a média ponderada, ou o preço do recebimento mais antigo ainda
em estoque.

O valor de um movimento é decidido quando a transferência é valorada e
guardado ali, porque um valor recalculado com os preços de hoje não é o que
aconteceu.

O lançamento contábil em si ainda não é escrito: falta `account.account`,
`account.journal` e o par débito/crédito na linha. O vínculo
(`stock.move.account_move_id`) já existe, esperando por eles.
""",
    'depends': ['base', 'product', 'stock', 'account'],
    'data': [
        'security/ir.model.access.csv',
        'views/stock_account_views.xml',
    ],
    'installable': True,
}
