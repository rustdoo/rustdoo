/** @odoo-module ignore **/
// Não é um módulo ES6: este cliente é IIFE e se instala em
// `window.rusdoo` ao carregar. Envolvê-lo num `odoo.define` faria
// o corpo só rodar quando alguém o requisitasse, e ninguém requisita.
/**
 * Utilitários do cliente: construção de DOM e formatação de valores.
 *
 * Todo elemento é criado por `el()` — nada de innerHTML com dado vindo do
 * servidor. É o que faz um registro chamado `<script>` ser texto na tela
 * em vez de código executado nela.
 */
(function (rusdoo) {
    "use strict";

    /**
     * Cria um elemento. `attrs` vira atributo (ou propriedade, quando o
     * nome começa com `on`), e `children` aceita nós, strings e null.
     */
    function el(tag, attrs, children) {
        const node = document.createElement(tag);
        for (const [name, value] of Object.entries(attrs || {})) {
            if (value === null || value === undefined || value === false) {
                continue;
            }
            if (name.startsWith("on") && typeof value === "function") {
                node.addEventListener(name.slice(2).toLowerCase(), value);
            } else if (name === "value") {
                node.value = value;
            } else if (name === "checked" || name === "disabled" || name === "selected") {
                node[name] = Boolean(value);
            } else {
                node.setAttribute(name, value);
            }
        }
        append(node, children);
        return node;
    }

    function append(node, children) {
        if (children === null || children === undefined || children === false) {
            return node;
        }
        if (Array.isArray(children)) {
            children.forEach((child) => append(node, child));
            return node;
        }
        node.append(children instanceof Node ? children : document.createTextNode(String(children)));
        return node;
    }

    /** Substitui o conteúdo de um nó pelos filhos dados. */
    function fill(node, children) {
        node.replaceChildren();
        return append(node, children);
    }

    /** Rótulo legível de um campo, com o `string` do arch tendo prioridade. */
    function fieldLabel(name, meta, archLabel) {
        return archLabel || (meta && meta.string) || name;
    }

    /**
     * Valor de um campo como texto de leitura. Um many2one chega como
     * `{id, display_name}`; um selection mostra o rótulo, não a chave.
     */
    function formatValue(value, meta) {
        if (value === null || value === undefined || value === false) {
            // `false` é o "vazio" do Odoo em qualquer tipo menos booleano
            return meta && meta.type === "boolean" ? "não" : "";
        }
        switch (meta ? meta.type : "char") {
            case "boolean":
                return value ? "sim" : "não";
            case "many2one":
                return value.display_name || (value.id ? "#" + value.id : "");
            case "selection": {
                const option = (meta.selection || []).find((pair) => pair[0] === value);
                return option ? option[1] : String(value);
            }
            case "one2many":
            case "many2many":
                return Array.isArray(value) ? String(value.length) + " registro(s)" : "";
            case "float":
            case "monetary":
                return typeof value === "number" ? value.toFixed(2) : String(value);
            case "datetime":
            case "date":
                return String(value).replace("T", " ");
            default:
                return String(value);
        }
    }

    /**
     * Converte o que o usuário digitou no valor que o servidor espera.
     * Um número inválido vira erro em vez de `0` silencioso — salvar o
     * valor errado é pior que recusar o salvamento.
     */
    function parseInput(raw, meta) {
        const type = meta ? meta.type : "char";
        if (type === "boolean") {
            return Boolean(raw);
        }
        if (raw === "" || raw === null || raw === undefined) {
            return false;
        }
        if (type === "integer") {
            const parsed = Number(raw);
            if (!Number.isInteger(parsed)) {
                throw new Error("valor inteiro inválido: " + raw);
            }
            return parsed;
        }
        if (type === "float" || type === "monetary") {
            const parsed = Number(String(raw).replace(",", "."));
            if (Number.isNaN(parsed)) {
                throw new Error("valor numérico inválido: " + raw);
            }
            return parsed;
        }
        return raw;
    }

    /** Adia `fn` até `delay` ms sem novas chamadas (busca enquanto digita). */
    function debounce(fn, delay) {
        let timer = null;
        return function (...args) {
            window.clearTimeout(timer);
            timer = window.setTimeout(() => fn.apply(this, args), delay);
        };
    }

    /** Faz o parse de um arch, tratando XML inválido como erro claro. */
    function parseArch(arch) {
        const doc = new DOMParser().parseFromString(arch || "", "text/xml");
        const failure = doc.querySelector("parsererror");
        if (failure || !doc.documentElement) {
            throw new Error("arch inválido: o servidor devolveu XML malformado");
        }
        return doc.documentElement;
    }

    /** Os `<field>` de um arch, na ordem em que aparecem. */
    function archFields(root) {
        return Array.from(root.getElementsByTagName("field")).map((node) => ({
            name: node.getAttribute("name"),
            label: node.getAttribute("string"),
            widget: node.getAttribute("widget"),
            readonly: node.getAttribute("readonly") === "1",
            node: node,
        }));
    }

    rusdoo.utils = {
        el: el,
        fill: fill,
        append: append,
        fieldLabel: fieldLabel,
        formatValue: formatValue,
        parseInput: parseInput,
        debounce: debounce,
        parseArch: parseArch,
        archFields: archFields,
    };
})((window.rusdoo = window.rusdoo || {}));
