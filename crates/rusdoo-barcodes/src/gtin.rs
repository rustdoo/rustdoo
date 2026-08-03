//! The GTIN check digit, and what makes a code really be written in an
//! encoding — a port of `odoo/tools/barcode.py`.
//!
//! This is where a barcode's only hard truth lives: the digits the
//! scanner sent either add up or the code was misread. Everything else
//! (rules, patterns, embedded values) is convention
//! de cada empresa.

/// Each encoding's fixed length. `any` is not here: it does not
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

/// The check digit of a GTIN code (EAN-8, UPC-A, EAN-13, SSCC).
///
/// The code's last digit is ignored — it is precisely what is being
/// recomputed. The sum runs back to front so that the code's length does
/// not change each position's weight: it is the same algorithm for an
/// EAN-8 and for an eighteen-digit SSCC.
///
/// `None` when something that is not a digit turns up: there is no
/// checksum to verify then, and answering zero would let a broken code
/// pass for a good one.
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

/// Is the code really written in that encoding?
///
/// Right length, digits only, and the check digit adding up. The
/// exception is an EAN-13 starting with zero: that is a UPC-A with a
/// zero in front, and
/// quem quer UPC-A pede UPC-A.
pub fn check_encoding(barcode: &str, encoding: &str) -> bool {
    let encoding = encoding.to_ascii_lowercase();
    if encoding == "any" {
        return true;
    }
    // an encoding we do not know validates nothing: refusing is the
    // only way not to let through a code nobody checked
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

/// A valid EAN-13 from a prefix: leading zeros up to thirteen digits,
/// and the check digit recomputed.
///
/// It is what stores on the product the "base" code of a scan that
/// embedded weight or price — zeroing the numeric part spoils the check
/// digit, and without redoing it the stored code would not scan back.
pub fn sanitize_ean(ean: &str) -> String {
    let padded = format!("{:0>13}", ean.chars().take(13).collect::<String>());
    let body: String = padded
        .chars()
        .take(padded.chars().count().saturating_sub(1))
        .collect();
    // a non-numeric code has no check digit; zero is what Odoo puts
    // there too
    let digit = check_digit(&padded).unwrap_or(0);
    format!("{body}{digit}")
}

/// The same for UPC-A: it is an EAN-13 without the leading zero.
pub fn sanitize_upc(upc: &str) -> String {
    sanitize_ean(&format!("0{upc}")).chars().skip(1).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_check_digit_closes_the_code() {
        // the GS1 specification's examples, at each length
        assert_eq!(check_digit("12345670"), Some(0));
        assert_eq!(check_digit("02003405"), Some(5));
        assert_eq!(check_digit("1020034051259"), Some(9));
        assert_eq!(check_digit("012345678905"), Some(5));
        // the digit already there does not enter the sum: changing it
        // does not change the result, it is the one being checked
        assert_eq!(check_digit("12345671"), Some(0));
    }

    #[test]
    fn a_code_with_a_letter_has_no_check_digit() {
        assert_eq!(check_digit("12X45670"), None);
        assert_eq!(check_digit("ABC"), None);
        // the last position does not enter the sum — it is the check
        // digit's slot — which is why verifying a code also verifies
        // that digit
        assert_eq!(check_digit("1234567X"), Some(0));
        assert!(!check_encoding("1234567X", "ean8"));
    }

    #[test]
    fn an_encoding_is_size_digits_and_checksum() {
        assert!(check_encoding("12345670", "ean8"));
        // a wrong check digit is a misread code
        assert!(!check_encoding("12345678", "ean8"));
        // curto demais para ser um EAN-8
        assert!(!check_encoding("0002", "ean8"));
        assert!(check_encoding("1020034051259", "ean13"));
        assert!(check_encoding("012345678905", "upca"));
        // thirteen digits starting with zero are a UPC-A, not an EAN-13
        assert!(!check_encoding("0012345678905", "ean13"));
        // `any` verifies nothing; it is what the default rule uses
        assert!(check_encoding("qualquer coisa", "any"));
        // an invented encoding validates nothing
        assert!(!check_encoding("12345670", "ean42"));
    }

    #[test]
    fn sanitizing_refits_the_check_digit() {
        // the base code of a scan with embedded weight, zeroed and
        // fechado de novo
        assert_eq!(sanitize_ean("1020034050009"), "1020034050009");
        assert_eq!(sanitize_ean("2212345600009"), "2212345600007");
        // um prefixo curto vira um EAN-13 completo
        assert_eq!(sanitize_ean("123"), "0000000000123");
        assert!(check_encoding(&sanitize_ean("2212345600009"), "ean13"));
        // a UPC-A has twelve digits and adds up too
        let upc = sanitize_upc("01234567890");
        assert_eq!(upc.chars().count(), 12);
        assert!(check_encoding(&upc, "upca"));
    }
}
