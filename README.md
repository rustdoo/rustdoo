# Rusdoo — Port do Odoo para Rust

Port integral do [Odoo](https://github.com/odoo/odoo) para Rust. O código-fonte
original vive em `./odoo/` (clone shallow, branch `master`) e serve como
referência canônica durante o port.

## Escala do problema (medida em 2026-07-10)

| Métrica | Valor |
|---|---|
| Python (backend) | 8.549 arquivos, ~1,19M linhas |
| JavaScript (web client/Owl) | 5.824 arquivos, ~1,33M linhas |
| Módulos addon | 628 |
| Tamanho do clone shallow | 1,4 GB |

## Arquitetura do workspace

Cada crate espelha um subsistema do núcleo Python (`odoo/odoo/`):

| Crate | Origem no Odoo | Responsabilidade |
|---|---|---|
| `rusdoo-core` | `odoo/api.py`, `odoo/exceptions.py` | Environment, registry, erros |
| `rusdoo-orm` | `odoo/orm/` | Models, fields, domains → PostgreSQL (sqlx) |
| `rusdoo-http` | `odoo/http.py` | Servidor axum, JSON-RPC 2.0, sessões |
| `rusdoo-qweb` | `ir_qweb.py` | Engine de templates QWeb (XML) |
| `rusdoo-modules` | `odoo/modules/` | Manifests, grafo de dependências, loading, assets |
| `rusdoo-base` | `odoo/addons/base/models/` | Os modelos que todo addon usa |
| `rusdoo-server` | `odoo-bin` | Binário `rusdoo` (CLI + bootstrap) |

Um addon segue a mesma divisão do Odoo: **código** num crate, **dados**
num diretório de `addons/`.

| Addon | Conteúdo |
|---|---|
| `addons/base` | Grupos, `ir.model.access.csv`, views e menus dos modelos base |
| `addons/web` | O cliente web (JS/CSS servido pelo bundle `web.assets_backend`) |
| `addons/rusdoo_demo` | Dados de demonstração |

## Fases do port

O Odoo inteiro depende de ~5% do código (o framework). A ordem é ditada por isso:

1. **Fase 0 — Fundação** ✅: workspace compilando, tipos centrais.
2. **Fase 1 — ORM** ✅ no essencial: fields, domains → SQL, CRUD,
   `search`/`read`/`write`/`unlink`, herança (`_inherit`/`_inherits`),
   comandos x2many (0–6), `read_group`, defaults, `active_test`,
   LOG_ACCESS, campos computed (com dependências) e related, access
   rights (`ir.model.access`) e record rules (`ir.rule`) — ambos
   persistidos, lidos a cada boot.
   Falta: contexto/`Environment` completo, constraints SQL.
3. **Fase 2 — HTTP/RPC** ✅ no essencial: `/jsonrpc`,
   `/web/dataset/call_kw`, sessões, autenticação, e o caminho do web
   client — `fields_get`, `default_get`, `web_read`, `web_search_read`,
   `name_search`/`web_name_search`, `web_save`, `web_read_group`,
   `get_session_info`, `load_menus`, `onchange`, `/web/action/load`,
   e os assets: bundles resolvidos dos manifests e servidos em
   `/web/assets/<bundle>.js|css` + `/<módulo>/static/<arquivo>`.
   Falta: unfolding de grupos, upload de binários.
4. **Fase 3 — Módulos + dados** ✅ no essencial: parser de
   `__manifest__.py`, grafo de dependências, loader de XML/CSV
   (`ir.model.data`), instalação de addons.
5. **Fase 4 — QWeb + relatórios** ✅ no essencial: engine QWeb
   (`t-if`/`t-foreach`/`t-out`/`t-call`/`t-set`/`t-att*`), views
   renderizadas server-side. Falta: relatórios PDF.
6. **Fase 5 — Cliente web** ✅ no essencial: o addon `web` traz um
   cliente próprio (JS sem dependências) que fala o mesmo JSON-RPC do
   Odoo: login, apps e menus, view de lista (busca, ordenação, paginação)
   e view de formulário (criar, editar, excluir). Falta: kanban, linhas
   x2many editáveis, painel de filtros.
7. **Fase 6 — Addons de negócio** *(atual)*: port módulo a módulo em ordem
   do grafo de dependências (`base` → `web` → `mail` → `sale`/`account`/
   `stock` → …). O web client JS original (1,33M linhas) pode ser mantido
   como está — ele só fala JSON-RPC — ou substituído pelo cliente daqui.

## Build

```sh
cargo build          # compila todos os crates

createdb rusdoo
RUSDOO_DATABASE_URL=postgres:///rusdoo cargo run -p rusdoo-server -- --init
```

`--init` instala os addons de `addons/` (ou de `RUSDOO_ADDONS_PATH`) e
cria o usuário `admin` (senha `admin`) na primeira vez. Depois disso o
servidor sobe sem `--init`: ACL, regras de registro e bundles são lidos
a cada boot.

Abra <http://localhost:8069/web>. Variáveis úteis:

| Variável | Efeito |
|---|---|
| `RUSDOO_DATABASE_URL` | conexão PostgreSQL (obrigatória) |
| `RUSDOO_ADDR` | endereço de escuta (padrão `0.0.0.0:8069`) |
| `RUSDOO_ADDONS_PATH` | diretório de addons (padrão `addons`) |
| `RUSDOO_INSECURE_COOKIES` | cookies de sessão sem `Secure`, para HTTP local |

## Testes

A suíte cobre desde a construção de SQL até chamadas JSON-RPC ponta a
ponta. Os testes marcados `_live` precisam de um PostgreSQL: sem a
variável eles se auto-ignoram (com aviso no stderr).

```sh
createdb rusdoo_test
RUSDOO_TEST_DATABASE_URL="postgres:///rusdoo_test" cargo test --workspace
```

Testes que criam tabelas de sistema (`ir_model_data`, …) rodam cada um em
seu próprio schema, então a suíte é segura em paralelo.

## Licença

LGPL-3.0, herdada do Odoo (obrigatória para trabalho derivado).
