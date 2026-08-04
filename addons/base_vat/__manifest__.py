{
    'name': 'Validação de CNPJ/IVA',
    'version': '19.0.1.0',
    'category': 'Oculto',
    'summary': 'Recusa um número de identificação fiscal que não pode ser um',
    'description': """
Um número de IVA carrega o próprio dígito verificador, então um erro de
digitação nele é um erro que qualquer um pega — antes de chegar a uma
fatura, a uma apuração e a um auditor.

O módulo não tem dados: o que ele acrescenta é a aritmética de cada país
sobre o campo `vat` que o `base` já declara. A checagem VIES do Odoo, que
pergunta à Comissão Europeia se o número está *registrado*, fica de fora
de propósito: é uma chamada de rede dentro de uma gravação.
""",
    'depends': ['base'],
    'data': [],
    'installable': True,
    'auto_install': True,
}
