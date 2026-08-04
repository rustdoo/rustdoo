# probe — o instrumento que abre a tela

O cliente web do Odoo só diz o que está faltando **enquanto roda**. Nenhum
dos bloqueios que este harness já derrubou aparecia na suíte: um
`session_info` incompleto, uma empresa que não existia, uma rota que
recusava o hash na URL. Cada um deles era um servidor que respondia 200 a
tudo e uma tela em branco.

`probe` é um addon de uma página só, **servido pela própria origem** (o que
evita CORS e permite usar o cookie de sessão). Ele:

1. autentica por `fetch` em `/web/session/authenticate`;
2. põe a resposta em `odoo.__session_info__` — é o que o shell faz;
3. carrega os menus e injeta `/web/assets/web.assets_web.js`;
4. espera `odoo.isReady`, então **faz o que um usuário faz**: clica numa
   linha da lista, espera o formulário, troca um campo e salva;
5. escreve num `<div>` a cada 500 ms o estado do loader, os erros com toda
   a cadeia de `cause`, o console, as requisições que não deram 200, e o
   texto da navbar e do gerenciador de actions.

O passo 5 é o que importa: o `--dump-dom` do chromium tira uma foto do DOM
e nada mais — sem console, sem rede. Escrever o estado no próprio DOM é o
que transforma o dump num relatório.

## Rodando

```sh
# 1. um addons path SEM o `web` deste port: ele sombreia o `web` do Odoo,
#    e o layout real carrega `web.assets_web`, que só o do Odoo declara
mkdir -p /tmp/rusdoo-probe/addons
for a in addons/*/; do
  [ "$(basename "$a")" = web ] && continue
  ln -sfn "$PWD/$a" "/tmp/rusdoo-probe/addons/$(basename "$a")"
done

# 2. instalar num banco novo (leva minutos: compila os assets do `web`)
createdb rusdoo_probe
RUSDOO_INSTALL="web,probe,product,sale,mail,hr" \
RUSDOO_DATABASE_URL="postgres:///rusdoo_probe?host=/run/postgresql" \
RUSDOO_ADDONS_PATH="/tmp/rusdoo-probe/addons,odoo/addons,odoo/odoo/addons,tools/probe" \
  ./target/debug/rusdoo --init

# 3. subir o servidor NUMA PORTA QUE NÃO SEJA A 8069 (o Odoo de verdade
#    mora lá) e matar com `pkill -x rusdoo` — um `pkill -f` casa a própria
#    linha de comando e mata o shell
RUSDOO_INSTALL="web,probe,product,sale,mail,hr" \
RUSDOO_DATABASE_URL="postgres:///rusdoo_probe?host=/run/postgresql" \
RUSDOO_ADDONS_PATH="/tmp/rusdoo-probe/addons,odoo/addons,odoo/odoo/addons,tools/probe" \
RUSDOO_ADDR=127.0.0.1:8169 RUSDOO_INSECURE_COOKIES=1 ./target/debug/rusdoo &

# 4. o relatório
chromium --headless --no-sandbox --disable-gpu --virtual-time-budget=90000 \
  --dump-dom http://127.0.0.1:8169/probe/static/boot.html
```

O MCP do playwright **não serve** aqui: ele procura um executável "chrome"
que esta máquina não tem.

## O que já provou

Em 2026-08-04: navbar com empresa e usuário, o menu de apps, a action de um
menu, a lista de contatos com colunas e paginação, o formulário aberto num
clique, um campo trocado e **salvo no Postgres**.
