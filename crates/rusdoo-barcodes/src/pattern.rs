//! O padrão de uma regra: uma expressão regular que casa o começo do
//! código, com um trecho entre chaves onde a balança ou a etiquetadora
//! escreveram um número.
//!
//! `2.....{NNNDD}` lê "os códigos que começam em 2, e do sétimo dígito
//! em diante vem um valor com três inteiros e duas casas". O que sobra
//! quando esse valor é zerado é o código gravado no produto.

use regex::Regex;

/// O que casar um padrão contra um código revelou.
pub struct PatternMatch {
    pub matched: bool,
    /// o número embutido no código, zero quando o padrão não embute nenhum
    pub value: f64,
    /// o código com o trecho numérico zerado — é este que está no produto
    pub base_code: String,
}

/// Onde fica o trecho numérico do padrão: o primeiro `{N…D…}`.
///
/// Devolve as posições em caracteres, abrindo no `{` e fechando depois
/// do `}`. Sem N nem D também conta (`{}`), porque quem recusa isso é a
/// constraint da regra, e não este scanner.
fn numeric_span(pattern: &str) -> Option<(usize, usize)> {
    let chars: Vec<char> = pattern.chars().collect();
    for start in 0..chars.len() {
        if chars[start] != '{' {
            continue;
        }
        let mut cursor = start + 1;
        while chars.get(cursor) == Some(&'N') {
            cursor += 1;
        }
        while chars.get(cursor) == Some(&'D') {
            cursor += 1;
        }
        if chars.get(cursor) == Some(&'}') {
            return Some((start, cursor + 1));
        }
    }
    None
}

/// A expressão regular que o padrão realmente vira: o trecho numérico
/// trocado pelos zeros que ele ocupa no código base.
///
/// O casamento é sempre contra o código já zerado, então é este texto —
/// e não o padrão como foi escrito — que precisa compilar. É por isso
/// que a constraint da regra valida exatamente ele.
fn effective_regex(pattern: &str) -> String {
    let Some((start, end)) = numeric_span(pattern) else {
        return pattern.to_string();
    };
    let chars: Vec<char> = pattern.chars().collect();
    let head: String = chars[..start].iter().collect();
    let tail: String = chars[end..].iter().collect();
    format!("{head}{}{tail}", "0".repeat(end - start - 2))
}

/// Casa um código contra um padrão e extrai o número que ele embutia.
///
/// O padrão casa um *prefixo* do código (é assim no Odoo: `re.match`),
/// e o código é cortado no comprimento do padrão antes da comparação.
pub fn match_pattern(barcode: &str, pattern: &str) -> PatternMatch {
    let code: Vec<char> = barcode.chars().collect();
    let mut value = 0.0;
    let mut base_code = barcode.to_string();

    if let Some((start, end)) = numeric_span(pattern) {
        let inside: Vec<char> = pattern
            .chars()
            .skip(start + 1)
            .take(end - start - 2)
            .collect();
        let decimals = inside.iter().filter(|c| **c == 'D').count();
        let whole_len = inside.len() - decimals;
        // o trecho do código que fica sob as chaves; um código mais curto
        // que o padrão simplesmente entrega menos dígitos
        let digits: Vec<char> = code
            .iter()
            .skip(start)
            .take(inside.len())
            .copied()
            .collect();
        let whole: String = digits.iter().take(whole_len).collect();
        let decimal: String = digits.iter().skip(whole_len).collect();
        let whole = if whole.is_empty() {
            "0".to_string()
        } else {
            whole
        };
        // se ali não havia número, não há valor a extrair nem código base
        // a montar: a regra ainda pode não casar, e é o regex que decide
        if whole.chars().all(|c| c.is_ascii_digit()) {
            let integer: f64 = whole.parse().unwrap_or(0.0);
            let fraction: f64 = format!("0.{decimal}").parse().unwrap_or(0.0);
            value = integer + fraction;
            let head: String = code.iter().take(start).collect();
            let tail: String = code.iter().skip(start + inside.len()).collect();
            base_code = format!("{head}{}{tail}", "0".repeat(inside.len()));
        }
    }

    let effective = effective_regex(pattern);
    let subject: String = base_code
        .chars()
        .take(effective.chars().count())
        .collect::<String>();
    // um padrão que não compila não casa com nada; a constraint da regra
    // impede que um desses chegue até aqui
    let matched = Regex::new(&format!("^(?:{effective})"))
        .map(|regex| regex.is_match(&subject))
        .unwrap_or(false);

    PatternMatch {
        matched,
        value,
        base_code,
    }
}

/// O padrão de uma regra é utilizável?
///
/// A recusa acontece na gravação da regra, não na leitura de um código:
/// um padrão quebrado descoberto no balcão é uma venda parada, e ninguém
/// ali vai saber que o problema é uma chave a mais numa tela de
/// configuração.
pub fn check_pattern(pattern: &str) -> Result<(), String> {
    // chaves escapadas fazem parte do texto a casar, não delimitam o
    // trecho numérico: saem da contagem
    let bare = pattern
        .replace("\\\\", "X")
        .replace("\\{", "X")
        .replace("\\}", "X");
    let braces = bare.chars().filter(|c| *c == '{' || *c == '}').count();
    if braces == 2 {
        if numeric_span(&bare).is_none() {
            return Err(format!(
                "o padrão {pattern:?} tem chaves com algo que não é N seguido de D; \
                 escreva os inteiros e depois os decimais, como {{NNNDD}}"
            ));
        }
        if bare.contains("{}") {
            return Err(format!(
                "o padrão {pattern:?} tem chaves vazias; diga quantos dígitos o valor \
                 ocupa, como {{NNN}}"
            ));
        }
    } else if braces != 0 {
        return Err(format!(
            "o padrão {pattern:?} tem mais de um par de chaves; uma regra embute um \
             valor só"
        ));
    } else if bare == "*" {
        return Err("'*' não é um padrão válido; para casar qualquer código escreva '.*'".into());
    }
    Regex::new(&effective_regex(&bare))
        .map_err(|_| format!("o padrão {pattern:?} não é uma expressão regular válida"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pattern_without_braces_only_matches() {
        let hit = match_pattern("12345670", "........");
        assert!(hit.matched);
        assert_eq!(hit.value, 0.0, "não há valor embutido");
        assert_eq!(hit.base_code, "12345670", "o código é ele mesmo");

        assert!(!match_pattern("16012344", "11......").matched);
    }

    #[test]
    fn the_whole_code_can_be_the_value() {
        let hit = match_pattern("12345670", "{NNNNNNNN}");
        assert!(hit.matched);
        assert_eq!(hit.value, 12345670.0);
        assert_eq!(hit.base_code, "00000000", "sobrou só o zero");
    }

    #[test]
    fn decimals_come_after_the_whole_part() {
        // 1020034051259: do décimo dígito em diante, 12 com uma casa
        let hit = match_pattern("1020034051259", "1........{NND}.");
        assert!(hit.matched);
        assert_eq!(hit.value, 12.5);
        assert_eq!(hit.base_code, "1020034050009");

        let hit = match_pattern("2212345610259", "22......{NNDD}.");
        assert!(hit.matched);
        assert_eq!(hit.value, 10.25);
        assert_eq!(hit.base_code, "2212345600009");
    }

    #[test]
    fn a_value_of_a_single_digit_still_works() {
        let hit = match_pattern("11012344", "11.....{N}");
        assert!(hit.matched);
        assert_eq!(hit.value, 4.0);
        assert_eq!(hit.base_code, "11012340");

        // o mesmo padrão contra um código de outro prefixo não casa,
        // mesmo tendo extraído um valor
        assert!(!match_pattern("66012344", "11.....{N}").matched);
    }

    #[test]
    fn a_leading_zero_in_the_value_is_still_a_number() {
        let hit = match_pattern("66012344", "66{NN}....");
        assert!(hit.matched);
        assert_eq!(hit.value, 1.0);
        assert_eq!(hit.base_code, "66002344");
    }

    #[test]
    fn a_pattern_the_server_can_apply_is_accepted() {
        for pattern in [".*", "........", "22......{NNDD}.", "..>>>{ND}", "{NNN}"] {
            assert!(check_pattern(pattern).is_ok(), "{pattern} devia passar");
        }
    }

    #[test]
    fn a_pattern_the_server_cannot_apply_is_refused() {
        // chaves vazias: quantos dígitos?
        assert!(check_pattern("......{}..")
            .unwrap_err()
            .contains("chaves vazias"));
        // decimal antes do inteiro
        assert!(check_pattern("......{DN}")
            .unwrap_err()
            .contains("N seguido de D"));
        // dois valores numa regra só
        assert!(check_pattern("....{NN}{DD}")
            .unwrap_err()
            .contains("mais de um par"));
        // '*' sozinho é o erro que todo mundo comete
        assert!(check_pattern("*").unwrap_err().contains("'.*'"));
        // e o que nem regex é
        assert!(check_pattern("**>>>{ND}")
            .unwrap_err()
            .contains("expressão regular"));
    }
}
