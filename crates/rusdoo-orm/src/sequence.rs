//! Port of `ir.sequence` (`odoo/addons/base/models/ir_sequence.py`): the
//! numbers documents carry.
//!
//! A number is drawn inside the create's own transaction, with the
//! sequence row locked while it moves. That is the whole point: two
//! people creating an order at the same moment must not both be handed
//! `SO00007`, and a create that rolls back must not leave a hole nobody
//! can explain — Odoo accepts the hole (the sequence does not roll back
//! in `no_gap` mode either), and so does this, but never a duplicate.

use crate::registry::Registry;
use crate::sql::quote_ident;
use rusdoo_core::RusdooError;
use sqlx::{PgConnection, Row};

/// The model a sequence lives in. Registered by `base`; a database
/// without it simply has no sequences, and a field asking for one says
/// so instead of inventing a number.
pub const SEQUENCE_MODEL: &str = "ir.sequence";

/// A drawn number, formatted the way the sequence describes it.
///
/// `prefix` and `suffix` are taken literally — Odoo interpolates dates
/// into them (`%(year)s`), which is not ported: a prefix that looks like
/// a format string would come out as itself, and pretending to
/// understand it would be worse.
fn format_number(prefix: &str, suffix: &str, padding: i64, number: i64) -> String {
    let padded = if padding > 0 {
        format!("{number:0width$}", width = padding as usize)
    } else {
        number.to_string()
    };
    format!("{prefix}{padded}{suffix}")
}

/// Draw the next number of `code`, moving the sequence forward.
///
/// `None` when no sequence carries that code: the caller decides whether
/// that is a missing configuration or a field that simply keeps its
/// default.
pub async fn next_by_code(
    conn: &mut PgConnection,
    registry: &Registry,
    code: &str,
) -> Result<Option<String>, RusdooError> {
    let Some(model) = registry.get(SEQUENCE_MODEL) else {
        return Ok(None);
    };
    let table = quote_ident(&model.meta.table)?;
    // one statement: the UPDATE takes the row lock and hands back what
    // the number was before it moved. A SELECT-then-UPDATE would let two
    // transactions read the same value.
    let sql = format!(
        r#"UPDATE {table} SET "number_next" = COALESCE("number_next", 1) + GREATEST(COALESCE("number_increment", 1), 1)
           WHERE "id" = (SELECT "id" FROM {table} WHERE "code" = $1 ORDER BY "id" LIMIT 1)
           RETURNING COALESCE("number_next", 1) - GREATEST(COALESCE("number_increment", 1), 1) AS "drawn",
                     COALESCE("prefix", '') AS "prefix",
                     COALESCE("suffix", '') AS "suffix",
                     COALESCE("padding", 0) AS "padding""#
    );
    let row = sqlx::query(&sql)
        .bind(code)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|error| RusdooError::Database(error.to_string()))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let drawn: i32 = row.try_get("drawn").map_err(db_err)?;
    let prefix: String = row.try_get("prefix").map_err(db_err)?;
    let suffix: String = row.try_get("suffix").map_err(db_err)?;
    let padding: i32 = row.try_get("padding").map_err(db_err)?;
    Ok(Some(format_number(
        &prefix,
        &suffix,
        i64::from(padding),
        i64::from(drawn),
    )))
}

fn db_err(error: sqlx::Error) -> RusdooError {
    RusdooError::Database(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_number_wears_its_prefix_and_padding() {
        assert_eq!(format_number("SO", "", 5, 7), "SO00007");
        assert_eq!(format_number("WH/OUT/", "", 5, 42), "WH/OUT/00042");
        // no padding means the number as it is
        assert_eq!(format_number("FAT-", "", 0, 108), "FAT-108");
        // and a number wider than its padding is not truncated
        assert_eq!(format_number("SO", "", 3, 12345), "SO12345");
        assert_eq!(format_number("", "/2026", 4, 9), "0009/2026");
    }
}
