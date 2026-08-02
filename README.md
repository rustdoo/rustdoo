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
| `rusdoo-modules` | `odoo/modules/` | Manifests, grafo de dependências, loading |
| `rusdoo-server` | `odoo-bin` | Binário `rusdoo` (CLI + bootstrap) |

## Fases do port

O Odoo inteiro depende de ~5% do código (o framework). A ordem é ditada por isso:

1. **Fase 0 — Fundação** ✅: workspace compilando, tipos centrais.
2. **Fase 1 — ORM** ✅ no essencial: fields, domains → SQL, CRUD,
   `search`/`read`/`write`/`unlink`, herança (`_inherit`/`_inherits`),
   comandos x2many (0–6), `read_group`, defaults, `active_test`,
   LOG_ACCESS e access rights (`ir.model.access`).
   Falta: campos computed/related, record rules, contexto/`Environment`.
3. **Fase 2 — HTTP/RPC** ✅ no essencial: `/jsonrpc`,
   `/web/dataset/call_kw`, sessões, autenticação, e o caminho do web
   client — `fields_get`, `default_get`, `web_read`, `web_search_read`,
   `name_search`/`web_name_search`, `web_save`, `web_read_group`,
   `get_session_info`, `load_menus`.
   Falta: `onchange`, unfolding de grupos, assets/bundles do client.
4. **Fase 3 — Módulos + dados** ✅ no essencial: parser de
   `__manifest__.py`, grafo de dependências, loader de XML/CSV
   (`ir.model.data`), instalação de addons.
5. **Fase 4 — QWeb + relatórios** ✅ no essencial: engine QWeb
   (`t-if`/`t-foreach`/`t-out`/`t-call`/`t-set`/`t-att*`), views
   renderizadas server-side. Falta: view types do client (list/form/kanban
   como arch interpretado pelo Owl) e relatórios PDF.
6. **Fase 5 — Addons de negócio** *(atual)*: port módulo a módulo em ordem
   do grafo de dependências (`base` → `web` → `mail` → `sale`/`account`/
   `stock` → …). O web client JS (1,33M linhas) pode ser mantido como está
   — ele só fala JSON-RPC — ou portado depois para WASM.

## Build

```sh
cargo build          # compila todos os crates
cargo run -p rusdoo-server
```

## Testes

A suíte cobre desde a construção de SQL até chamadas JSON-RPC ponta a
ponta. Os testes marcados `_live` precisam de um PostgreSQL: sem a
variável eles se auto-ignoram (com aviso no stderr).

```sh
createdb rusdoo_test
RUSDOO_TEST_DATABASE_URL="postgres:///rusdoo_test" \
  cargo test --workspace -- --test-threads=1
```

`--test-threads=1` porque alguns testes de instalação de módulo criam as
mesmas tabelas de sistema (`ir_model_data`, …) e colidem em paralelo.

## Licença

LGPL-3.0, herdada do Odoo (obrigatória para trabalho derivado).
