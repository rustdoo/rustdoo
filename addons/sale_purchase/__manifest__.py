{
    'name': 'Venda que levanta compra',
    'version': '19.0.1.0',
    'category': 'Vendas',
    'summary': 'Um serviço vendido e executado por terceiro vira uma cotação de compra',
    'description': """
Um produto marcado como serviço subcontratado é algo que a empresa vende e
outra pessoa executa. Vender exige comprar: confirmar o pedido de venda
levanta uma cotação para o fornecedor do serviço, e os dois documentos
ficam amarrados pelo vínculo entre a linha de venda e as linhas de compra
que ela gerou.

Esse vínculo é o módulo inteiro — é o que transforma uma mudança de um lado
em aviso do outro, em vez de uma divergência silenciosa que ninguém percebe
até o fornecedor faturar um trabalho que foi cancelado.

`product.supplierinfo` (de quem, por quanto, em quantos dias) é registrado
aqui porque o `product` ainda não o tem.
""",
    'depends': ['base', 'product', 'sale', 'purchase'],
    'data': [
        'security/ir.model.access.csv',
        'views/sale_purchase_views.xml',
    ],
    'installable': True,
}
