/**
 * O boot do cliente: sessão, menus, e o despacho de uma ação para a view
 * que a desenha. É o último arquivo do bundle — quando ele roda, o resto
 * já existe.
 */
(function (rusdoo) {
    "use strict";

    const { el, fill } = rusdoo.utils;
    const api = rusdoo.rpc;

    /** As views que uma ação pode pedir e este cliente sabe desenhar. */
    const SUPPORTED_VIEWS = ["list", "form"];

    /** A view de busca não é pedida pela ação: acompanha a lista. */
    const SEARCH_VIEW = [false, "search"];

    class WebClient {
        constructor(target) {
            this.target = target;
            this.session = null;
            this.menus = {};
            this.currentApp = null;
            this.notification = el("div", { class: "o_notification" });
            this.content = el("div", { class: "o_content" });
        }

        /** Mostra um erro sem derrubar a tela que o usuário estava vendo. */
        notify(error) {
            const message = error && error.message ? error.message : String(error);
            fill(this.notification, [
                el("div", { class: "o_error" }, [
                    message,
                    el(
                        "button",
                        { class: "btn btn-ghost", onclick: () => fill(this.notification, null) },
                        "×"
                    ),
                ]),
            ]);
            if (error && error.isSessionExpired) {
                this.session = null;
                this.renderLogin();
            }
        }

        async start() {
            try {
                this.session = await api.sessionInfo();
            } catch (error) {
                this.notify(error);
                this.session = null;
            }
            if (!this.session || this.session.uid === null || this.session.uid === undefined) {
                this.renderLogin();
                return;
            }
            try {
                this.menus = await api.loadMenus();
            } catch (error) {
                this.menus = {};
                this.notify(error);
            }
            this.renderApp();
            const apps = this.appMenus();
            if (apps.length) {
                this.openMenu(apps[0]);
            }
        }

        /** Os menus de primeiro nível — os "apps" da barra superior. */
        appMenus() {
            const root = this.menus.root;
            if (!root || !Array.isArray(root.children)) {
                return [];
            }
            return root.children.map((id) => this.menus[String(id)]).filter(Boolean);
        }

        /** Os filhos de um menu, na ordem em que o servidor os mandou. */
        childrenOf(menu) {
            return (menu.children || []).map((id) => this.menus[String(id)]).filter(Boolean);
        }

        renderLogin() {
            const login = el("input", { type: "text", name: "login", placeholder: "Usuário", autofocus: "autofocus" });
            const password = el("input", { type: "password", name: "password", placeholder: "Senha" });
            const form = el(
                "form",
                {
                    class: "o_login_form",
                    onsubmit: async (event) => {
                        event.preventDefault();
                        try {
                            await api.authenticate(login.value, password.value);
                            // a sessão nova traz menus novos: recomeça o boot
                            await this.start();
                        } catch (error) {
                            this.notify(error);
                        }
                    },
                },
                [
                    el("h1", {}, "rusdoo"),
                    login,
                    password,
                    el("button", { class: "btn btn-primary", type: "submit" }, "Entrar"),
                ]
            );
            fill(this.target, [this.notification, el("div", { class: "o_login" }, form)]);
        }

        renderApp() {
            fill(this.target, [
                this.renderNavbar(),
                this.notification,
                el("div", { class: "o_main" }, [this.renderSidebar(), this.content]),
            ]);
        }

        renderNavbar() {
            const apps = this.appMenus().map((menu) =>
                el(
                    "button",
                    {
                        class: this.currentApp && this.currentApp.id === menu.id
                            ? "o_app o_active"
                            : "o_app",
                        onclick: () => this.openMenu(menu),
                    },
                    menu.name
                )
            );
            return el("nav", { class: "o_navbar" }, [
                el("span", { class: "o_brand" }, "rusdoo"),
                el("div", { class: "o_apps" }, apps),
                el("div", { class: "o_user" }, [
                    el("span", {}, this.session ? this.session.username || "" : ""),
                    el(
                        "button",
                        {
                            class: "btn btn-ghost",
                            onclick: async () => {
                                try {
                                    await api.destroy();
                                } finally {
                                    window.location.reload();
                                }
                            },
                        },
                        "Sair"
                    ),
                ]),
            ]);
        }

        renderSidebar() {
            if (!this.currentApp) {
                return el("aside", { class: "o_sidebar" });
            }
            const entries = [];
            const walk = (menu, depth) => {
                for (const child of this.childrenOf(menu)) {
                    entries.push(
                        el(
                            "button",
                            {
                                class: "o_menu_item o_depth_" + Math.min(depth, 3),
                                onclick: () => this.openMenu(child),
                            },
                            child.name
                        )
                    );
                    walk(child, depth + 1);
                }
            };
            walk(this.currentApp, 0);
            return el("aside", { class: "o_sidebar" }, entries);
        }

        /** Clique num menu: carrega a ação que ele aponta. */
        async openMenu(menu) {
            const app = this.menus[String(menu.appID)] || menu;
            this.currentApp = app;
            this.renderApp();
            if (!menu.actionID) {
                fill(this.content, el("div", { class: "o_nocontent" }, "Este menu não abre nenhuma ação."));
                return;
            }
            try {
                await this.doAction(menu.actionID);
            } catch (error) {
                this.notify(error);
            }
        }

        /** Carrega uma ação e desenha a primeira view que sabemos fazer. */
        async doAction(actionId) {
            const action = await api.loadAction(actionId);
            const wanted = (action.views || [])
                .map((pair) => pair[1])
                .filter((kind) => SUPPORTED_VIEWS.includes(kind));
            if (!wanted.length) {
                throw new Error("ação sem view suportada: " + (action.view_mode || ""));
            }
            const payload = await api.callKw(action.res_model, "get_views", [], {
                views: wanted.map((kind) => [false, kind]).concat([SEARCH_VIEW]),
            });
            this.action = {
                action: action,
                fields: payload.models[action.res_model].fields,
                views: payload.views,
                types: wanted,
            };
            if (wanted.includes("list")) {
                this.showList();
            } else {
                this.showForm(null);
            }
        }

        showList() {
            const { action, fields, views } = this.action;
            const view = new rusdoo.ListView({
                model: action.res_model,
                arch: views.list.arch,
                fields: fields,
                domain: action.domain || [],
                title: action.name || action.res_model,
                searchArch: views.search ? views.search.arch : null,
                onOpen: (id) => this.showForm(id),
                onCreate: views.form ? () => this.showForm(null) : null,
                onError: (error) => this.notify(error),
            });
            fill(this.content, view.root);
            view.refresh();
        }

        /**
         * Abrir o que uma ação aponta. Uma ação com `res_id` abre aquele
         * registro; sem ele, a lista do modelo.
         */
        async openAction(action) {
            try {
                const wanted = ["list", "form"];
                const payload = await api.callKw(action.res_model, "get_views", [], {
                    views: wanted.map((kind) => [false, kind]).concat([SEARCH_VIEW]),
                });
                this.action = {
                    action: {
                        res_model: action.res_model,
                        name: action.name || action.res_model,
                        domain: action.domain || [],
                    },
                    fields: payload.models[action.res_model].fields,
                    views: payload.views,
                    types: wanted,
                };
                if (action.res_id) {
                    await this.showForm(action.res_id);
                } else {
                    this.showList();
                }
            } catch (error) {
                this.notify(error);
            }
        }

        async showForm(resId) {
            const { action, fields, views } = this.action;
            if (!views.form) {
                this.notify(new Error("esta ação não tem view de formulário"));
                return;
            }
            const view = new rusdoo.FormView({
                model: action.res_model,
                arch: views.form.arch,
                fields: fields,
                resId: resId,
                title: action.name || action.res_model,
                onBack: () => this.showList(),
                onSaved: () => this.notifySaved(),
                onAction: (action) => this.openAction(action),
                onError: (error) => this.notify(error),
            });
            try {
                await view.load();
            } catch (error) {
                this.notify(error);
                return;
            }
            // o formulário aberto fica alcançável (depuração e testes)
            this.form = view;
            fill(this.content, view.render());
        }

        notifySaved() {
            fill(this.notification, [el("div", { class: "o_saved" }, "Registro salvo.")]);
            window.setTimeout(() => fill(this.notification, null), 2000);
        }
    }

    function boot() {
        const target = document.getElementById("rusdoo-app");
        if (!target) {
            return;
        }
        const client = new WebClient(target);
        rusdoo.client = client;
        client.start();
    }

    rusdoo.WebClient = WebClient;
    if (document.readyState === "loading") {
        document.addEventListener("DOMContentLoaded", boot);
    } else {
        boot();
    }
})((window.rusdoo = window.rusdoo || {}));
