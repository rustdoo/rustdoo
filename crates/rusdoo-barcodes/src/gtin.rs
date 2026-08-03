//! O dígito verificador do GTIN e o que faz um código estar mesmo
//! escrito numa codificação — port de `odoo/tools/barcode.py`.
//!
//! É aqui que mora a única verdade dura de um código de barras: os
//! dígitos que o leitor mandou ou fecham a conta ou o código foi lido
//! errado. Tudo o mais (regras, padrões, valores embutidos) é convenção
//! de cada empresa.

/// O tamanho fixo de cada codificação. `any` não aparece aqui: ela não
/// tem tamanho, aceita o que vier.
fn size_of(encoding: &str) -> Option<usize> {
    Some(match encoding {
        "ean8" => 8,
        "upca" => 12,
        "ean13" => 13,
        "gtin14" => 14,
        "sscc" => 18,
        _ => return None,
    })
}

/// O dígito verificador de um código GTIN (EAN-8, UPC-A, EAN-13, SSCC).
///
/// O último dígito do código é ignorado — ele é justamente o que se
/// recalcula. A conta é feita de trás para frente para que o
/// comprimento do código não mude o peso de cada posição: é o mesmo
/// algoritmo para um EAN-8 e para um SSCC de dezoito dígitos.
///
/// `None` quando aparece algo que não é dígito: aí não há checksum a
/// conferir, e devolver zero faria um código quebrado passar por bom.
pub fn check_digit(barcode: &str) -> Option<u32> {
    let digits = barcode
        .chars()
        .rev()
        .skip(1)
        .map(|c| c.to_digit(10))
        .collect::<Option<Vec<u32>>>()?;
    let total: u32 = digits
        .iter()
        .enumerate()
        .map(|(position, digit)| if position % 2 == 0 { digit * 3 } else { *digit })
        .sum();
    Some((10 - total % 10) % 10)
}

/// O código está mesmo escrito nessa codificação?
///
/// Tamanho certo, só dígitos, e o verificador batendo. A exceção é o
/// EAN-13 começando em zero: isso é um UPC-A com um zero na frente, e
/// quem quer UPC-A pede UPC-A.
pub fn check_encoding(barcode: &str, encoding: &str) -> bool {
    let encoding = encoding.to_ascii_lowercase();
    if encoding == "any" {
        return true;
    }
    // uma codificação que não conhecemos não valida nada: recusar é o
    // único jeito de não deixar passar um código que ninguém conferiu
    let Some(size) = size_of(&encoding) else {
        return false;
    };
    if barcode.chars().count() != size {
        return false;
    }
    if encoding == "ean13" && barcode.starts_with('0') {
        return false;
    }
    let Some(last) = barcode.chars().next_back().and_then(|c| c.to_digit(10)) else {
        return false;
    };
    check_digit(barcode) == Some(last)
}

/// Um EAN-13 válido a partir de um prefixo: zeros à esquerda até treze
/// dígitos, e o verificador recalculado.
///
/// É o que grava no produto o código "base" de uma leitura que embutia
/// peso ou preço — zerar o trecho numérico estraga o verificador, e sem
/// refazê-lo o código gravado não seria lido de volta.
pub fn sanitize_ean(ean: &str) -> String {
    let padded = format!("{:0>13}", ean.chars().take(13).collect::<String>());
    let body: String = padded
        .chars()
        .take(padded.chars().count().saturating_sub(1))
        .collect();
    // um código não numérico não tem verificador; o zero é o que o Odoo
    // também põe ali
    let digit = check_digit(&padded).unwrap_or(0);
    format!("{body}{digit}")
}

/// O mesmo para UPC-A: é um EAN-13 sem o zero da frente.
pub fn sanitize_upc(upc: &str) -> String {
    sanitize_ean(&format!("0{upc}")).chars().skip(1).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_check_digit_closes_the_code() {
        // os exemplos da especificação GS1, em cada tamanho
        assert_eq!(check_digit("12345670"), Some(0));
        assert_eq!(check_digit("02003405"), Some(5));
        assert_eq!(check_digit("1020034051259"), Some(9));
        assert_eq!(check_digit("012345678905"), Some(5));
        // o dígito que já está lá não entra na conta: trocá-lo não muda
        // o resultado, é ele que está sendo conferido
        assert_eq!(check_digit("12345671"), Some(0));
    }

    #[test]
    fn a_code_with_a_letter_has_no_check_digit() {
        assert_eq!(check_digit("12X45670"), None);
        assert_eq!(check_digit("ABC"), None);
        // a última posição não entra na conta — é a casa do verificador —,
        // e por isso quem confere um código também confere aquele dígito
        assert_eq!(check_digit("1234567X"), Some(0));
        assert!(!check_encoding("1234567X", "ean8"));
    }

    #[test]
    fn an_encoding_is_size_digits_and_checksum() {
        assert!(check_encoding("12345670", "ean8"));
        // um dígito verificador errado é um código lido errado
        assert!(!check_encoding("12345678", "ean8"));
        // curto demais para ser um EAN-8
        assert!(!check_encoding("0002", "ean8"));
        assert!(check_encoding("1020034051259", "ean13"));
        assert!(check_encoding("012345678905", "upca"));
        // treze dígitos começando em zero são um UPC-A, não um EAN-13
        assert!(!check_encoding("0012345678905", "ean13"));
        // `any` não confere nada, é o que a regra padrão usa
        assert!(check_encoding("qualquer coisa", "any"));
        // uma codificação inventada não valida nada
        assert!(!check_encoding("12345670", "ean42"));
    }

    #[test]
    fn sanitizing_refits_the_check_digit() {
        // o código base de uma leitura com peso embutido, zerado e
        // fechado de novo
        assert_eq!(sanitize_ean("1020034050009"), "1020034050009");
        assert_eq!(sanitize_ean("2212345600009"), "2212345600007");
        // um prefixo curto vira um EAN-13 completo
        assert_eq!(sanitize_ean("123"), "0000000000123");
        assert!(check_encoding(&sanitize_ean("2212345600009"), "ean13"));
        // UPC-A tem doze dígitos e também fecha
        let upc = sanitize_upc("01234567890");
        assert_eq!(upc.chars().count(), 12);
        assert!(check_encoding(&upc, "upca"));
    }
}
