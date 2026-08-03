//! Decodificar um código lido: passar as regras da nomenclatura, na
//! ordem, até uma casar — e dizer o que aquilo é.
//!
//! Puro de propósito: a decisão de o que um código significa não toca o
//! banco, e por isso pode ser testada com o código de um pacote de
//! bolacha e nada mais.

use crate::gtin::{check_digit, check_encoding, sanitize_ean, sanitize_upc};
use crate::pattern::match_pattern;
use serde_json::{json, Map, Value};

/// Uma regra da nomenclatura, do jeito que a decodificação precisa dela.
pub struct Rule {
    pub encoding: String,
    /// o `type` do Odoo: `alias` (troca o código por outro) ou `product`
    pub kind: String,
    pub pattern: String,
    pub alias: String,
}

/// O que a leitura de um código virou.
pub struct Parsed {
    pub encoding: String,
    /// `error` quando nenhuma regra casou — o leitor bipou algo que esta
    /// nomenclatura não sabe ler
    pub kind: String,
    pub code: String,
    pub base_code: String,
    pub value: Value,
}

impl Parsed {
    /// A leitura que nenhuma regra reconheceu.
    fn unknown(barcode: &str) -> Self {
        Parsed {
            encoding: String::new(),
            kind: "error".into(),
            code: barcode.to_string(),
            base_code: barcode.to_string(),
            value: json!(0.0),
        }
    }

    /// A forma que o cliente recebe, igual à do Odoo.
    pub fn to_json(&self) -> Value {
        json!({
            "encoding": self.encoding,
            "type": self.kind,
            "code": self.code,
            "base_code": self.base_code,
            "value": self.value,
        })
    }
}

/// Passa `barcode` pelas regras, na ordem em que vieram, e devolve o que
/// a primeira que casar disser que ele é.
///
/// A ordem é a resposta: duas regras podem casar o mesmo código, e é a
/// de menor `sequence` que vale. Quem ordena é quem lê as regras do
/// banco.
pub fn parse_nomenclature(rules: &[Rule], barcode: &str) -> Parsed {
    let mut result = Parsed::unknown(barcode);
    let mut code = barcode.to_string();
    for rule in rules {
        if !check_encoding(&code, &rule.encoding) {
            continue;
        }
        let hit = match_pattern(&code, &rule.pattern);
        if !hit.matched {
            continue;
        }
        if rule.kind == "alias" {
            // um alias não classifica nada: troca o código lido por outro
            // e deixa as regras seguintes classificarem esse
            code = rule.alias.clone();
            result.code = code.clone();
            continue;
        }
        result.encoding = rule.encoding.clone();
        result.kind = rule.kind.clone();
        result.value = json!(hit.value);
        result.code = code.clone();
        // zerar o trecho numérico quebrou o dígito verificador; o código
        // gravado no produto é o que fecha de novo
        result.base_code = match rule.encoding.as_str() {
            "ean13" => sanitize_ean(&hit.base_code),
            "upca" => sanitize_upc(&hit.base_code),
            _ => hit.base_code,
        };
        return result;
    }
    result
}

/// As URIs EPC que uma antena RFID entrega, convertidas nos códigos que
/// elas carregam.
///
/// Uma URI traz mais de uma coisa — o produto *e* o lote —, por isso a
/// resposta é uma lista, e por isso ela não passa pelas regras: a URI já
/// diz o que cada pedaço é.
///
/// `None` quando não é uma URI que sabemos ler; aí o código volta para o
/// caminho normal em vez de virar um resultado inventado.
pub fn parse_uri(barcode: &str) -> Option<Vec<Parsed>> {
    let trimmed = barcode.trim();
    if !trimmed.starts_with("urn:") {
        return None;
    }
    let parts: Vec<&str> = trimmed.split(':').collect();
    let identifier = parts.get(parts.len().checked_sub(2)?)?.trim();
    let data: Vec<&str> = parts.last()?.trim().split('.').collect();
    match identifier {
        "lgtin" | "sgtin" => gtin_uri(trimmed, &data),
        // as versões etiquetadas trazem um filtro na frente que não é dado
        "sgtin-96" | "sgtin-198" => gtin_uri(trimmed, data.get(1..)?),
        "sscc" => sscc_uri(trimmed, &data),
        "sscc-96" => sscc_uri(trimmed, data.get(1..)?),
        _ => None,
    }
}

/// O verificador de um código que ainda não tem lugar para ele: o zero
/// no fim ocupa a casa que o algoritmo ignora.
fn closed(code: &str) -> Option<String> {
    let digit = check_digit(&format!("{code}0"))?;
    Some(format!("{code}{digit}"))
}

/// SGTIN/LGTIN: prefixo da empresa, referência do item com o indicador
/// na frente, e o lote ou número de série.
fn gtin_uri(base_code: &str, data: &[&str]) -> Option<Vec<Parsed>> {
    let [company_prefix, item_ref, tracking_number] = data else {
        return None;
    };
    let indicator = item_ref.chars().next()?;
    let item: String = item_ref.chars().skip(1).collect();
    let product = closed(&format!("{indicator}{company_prefix}{item}"))?;
    Some(vec![
        Parsed {
            encoding: String::new(),
            kind: "product".into(),
            code: product.clone(),
            base_code: base_code.to_string(),
            value: json!(product),
        },
        Parsed {
            encoding: String::new(),
            kind: "lot".into(),
            code: (*tracking_number).to_string(),
            base_code: base_code.to_string(),
            value: json!(tracking_number),
        },
    ])
}

/// SSCC: prefixo da empresa e a referência serial, com a extensão na
/// frente. Identifica um volume, não um produto.
fn sscc_uri(base_code: &str, data: &[&str]) -> Option<Vec<Parsed>> {
    let [company_prefix, serial_reference] = data else {
        return None;
    };
    let extension = serial_reference.chars().next()?;
    let serial: String = serial_reference.chars().skip(1).collect();
    let sscc = closed(&format!("{extension}{company_prefix}{serial}"))?;
    Some(vec![Parsed {
        encoding: String::new(),
        kind: "package".into(),
        code: sscc.clone(),
        base_code: base_code.to_string(),
        value: json!(sscc),
    }])
}

/// Uma regra lida do banco. Um campo que voltou nulo vira o vazio: a
/// decodificação não é lugar de descobrir que a regra está incompleta.
pub fn rule_from(row: &Map<String, Value>) -> Rule {
    let text = |name: &str| {
        row.get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Rule {
        encoding: text("encoding"),
        kind: text("type"),
        pattern: text("pattern"),
        alias: text("alias"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(encoding: &str, pattern: &str) -> Rule {
        Rule {
            encoding: encoding.into(),
            kind: "product".into(),
            pattern: pattern.into(),
            alias: "0".into(),
        }
    }

    #[test]
    fn a_code_that_is_not_an_ean8_is_not_read_as_one() {
        let rules = [rule("ean8", "........")];

        // curto demais
        let parsed = parse_nomenclature(&rules, "0002");
        assert_eq!(parsed.kind, "error");
        assert_eq!(parsed.encoding, "");
        assert_eq!(parsed.code, "0002");

        // oito dígitos, mas o verificador não fecha: foi lido errado
        let parsed = parse_nomenclature(&rules, "12345678");
        assert_eq!(parsed.kind, "error", "o checksum é o que reprova");

        // este fecha
        let parsed = parse_nomenclature(&rules, "12345670");
        assert_eq!(parsed.kind, "product");
        assert_eq!(parsed.encoding, "ean8");
        assert_eq!(parsed.base_code, "12345670");
        assert_eq!(parsed.value, json!(0.0));
    }

    #[test]
    fn the_first_rule_that_matches_wins() {
        let rules = [rule("ean8", "11.....{N}"), rule("ean8", "66{NN}....")];

        // só a segunda casa
        let parsed = parse_nomenclature(&rules, "66012344");
        assert_eq!(parsed.value, json!(1.0));
        assert_eq!(parsed.base_code, "66002344");

        // só a primeira casa
        let parsed = parse_nomenclature(&rules, "11012344");
        assert_eq!(parsed.value, json!(4.0));
        assert_eq!(parsed.base_code, "11012340");

        // nenhuma casa
        assert_eq!(parse_nomenclature(&rules, "16012344").kind, "error");
    }

    #[test]
    fn the_base_code_of_an_ean13_is_closed_again() {
        let rules = [rule("ean13", "1........{NND}.")];
        let parsed = parse_nomenclature(&rules, "1020034051259");
        assert_eq!(parsed.kind, "product");
        assert_eq!(parsed.value, json!(12.5), "só o trecho NND é valor");
        // zerar o valor quebraria o verificador; o base_code fecha
        assert_eq!(parsed.base_code, "1020034050009");
        assert!(check_encoding(&parsed.base_code, "ean13"));
    }

    #[test]
    fn an_alias_hands_the_code_to_the_next_rule() {
        let rules = [
            Rule {
                encoding: "any".into(),
                kind: "alias".into(),
                pattern: "^99$".into(),
                alias: "12345670".into(),
            },
            rule("ean8", "........"),
        ];
        let parsed = parse_nomenclature(&rules, "99");
        assert_eq!(parsed.kind, "product", "o alias virou um EAN-8 de verdade");
        assert_eq!(parsed.code, "12345670");
    }

    #[test]
    fn a_nomenclature_without_rules_reads_nothing() {
        let parsed = parse_nomenclature(&[], "12345670");
        assert_eq!(parsed.kind, "error");
        assert_eq!(parsed.base_code, "12345670", "o código volta como veio");
    }

    #[test]
    fn an_epc_uri_carries_the_product_and_its_lot() {
        let parts = parse_uri("urn:epc:class:lgtin : 4012345.012345.998877").expect("uma URI");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].kind, "product");
        assert_eq!(parts[0].value, json!("04012345123456"));
        assert_eq!(parts[1].kind, "lot");
        assert_eq!(parts[1].value, json!("998877"));

        let parts = parse_uri("urn:epc:id:sgtin:9521141.012345.4711").expect("uma URI");
        assert_eq!(parts[0].value, json!("09521141123454"));
        assert_eq!(parts[1].value, json!("4711"));

        // a forma etiquetada traz um filtro na frente, que não é dado
        let parts = parse_uri("urn:epc:tag:sgtin-96 : 1.358378.0728089.620776").expect("uma URI");
        assert_eq!(parts[0].value, json!("03583787280898"));
        assert_eq!(parts[1].value, json!("620776"));
    }

    #[test]
    fn an_sscc_uri_is_a_package() {
        let parts = parse_uri("urn:epc:id:sscc:952656789012.03456").expect("uma URI");
        assert_eq!(parts.len(), 1, "um volume, não um produto e um lote");
        assert_eq!(parts[0].kind, "package");
        assert_eq!(parts[0].value, json!("095265678901234568"));
    }

    #[test]
    fn what_is_not_a_uri_we_know_goes_back_to_the_rules() {
        // um código comum não é URI nenhuma
        assert!(parse_uri("12345670").is_none());
        // uma URN de outra coisa não vira resultado inventado
        assert!(parse_uri("urn:isbn:0451450523").is_none());
        // e uma que tem o identificador certo com dados de menos também não
        assert!(parse_uri("urn:epc:id:sgtin:9521141.012345").is_none());
    }
}
