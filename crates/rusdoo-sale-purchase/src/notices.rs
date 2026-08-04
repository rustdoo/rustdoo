//! What one document tells the other when they stop agreeing.
//!
//! Port of the three templates in `data/mail_templates.xml`. They are
//! plain sentences here rather than QWeb HTML: the port's chatter shows
//! `mail.message.body` as text, and a body full of `<a data-oe-model=...>`
//! would read as markup on the screen instead of as a link. The wording
//! is the templates' — it is what tells somebody months later why a
//! request for quotation stopped matching the order that raised it.

/// One line of a document that changed, as the notices spell it out.
pub(crate) struct Exception {
    /// the document the line belongs to, by its number
    pub document: String,
    pub product: String,
    pub quantity: f64,
}

/// The heading both cancellation notices share: which documents went
/// wrong, and that somebody has to look.
fn heading(kind: &str, documents: &[String]) -> String {
    let names = documents.join(", ");
    format!("Exception(s) occurred on the {kind}(s): {names}. Manual actions may be needed.\nException(s):")
}

/// The documents named by a batch of exceptions, each one once.
fn documents(exceptions: &[Exception]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for exception in exceptions {
        if !names.contains(&exception.document) {
            names.push(exception.document.clone());
        }
    }
    names
}

/// `exception_purchase_on_sale_cancellation` — told to the purchase when
/// the sale that raised it is cancelled.
pub(crate) fn sale_cancelled(exceptions: &[Exception]) -> String {
    let mut body = heading("sale order", &documents(exceptions));
    for exception in exceptions {
        body.push_str(&format!(
            "\n- {}: {} of {} cancelled",
            exception.document, exception.quantity, exception.product
        ));
    }
    body
}

/// `exception_sale_on_purchase_cancellation` — told to the sale when the
/// purchase it raised is cancelled.
pub(crate) fn purchase_cancelled(exceptions: &[Exception]) -> String {
    let mut body = heading("purchase order", &documents(exceptions));
    for exception in exceptions {
        body.push_str(&format!(
            "\n- {}: {} of {} cancelled",
            exception.document, exception.quantity, exception.product
        ));
    }
    body
}

/// `exception_purchase_on_sale_quantity_decreased` — told to the
/// purchase when the sale line asks for less than it used to.
///
/// The purchase is not touched: a vendor may already have started, and
/// only the buyer knows whether the order can still be trimmed. What the
/// notice carries is both quantities, which is what makes the decision
/// possible.
pub(crate) fn quantity_decreased(order: &str, product: &str, new_qty: f64, old_qty: f64) -> String {
    format!(
        "Exception(s) occurred on the sale order(s): {order}. Manual actions may be needed.\n\
         Exception(s):\n- {order}: {new_qty} of {product} ordered instead of {old_qty}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exception(document: &str, product: &str, quantity: f64) -> Exception {
        Exception {
            document: document.into(),
            product: product.into(),
            quantity,
        }
    }

    #[test]
    fn a_cancelled_sale_names_the_order_and_what_was_dropped() {
        let body = sale_cancelled(&[exception("SO00001", "Out-sourced service", 4.0)]);
        assert!(body.contains("sale order(s): SO00001"), "{body}");
        assert!(body.contains("- SO00001: 4 of Out-sourced service cancelled"), "{body}");
    }

    #[test]
    fn two_lines_of_the_same_order_name_it_once_in_the_heading() {
        let body = sale_cancelled(&[
            exception("SO00001", "Painting", 1.0),
            exception("SO00001", "Cleaning", 2.0),
        ]);
        // the heading lists documents, the list below it lists lines: an
        // order named twice at the top reads as two cancellations
        assert!(body.contains("sale order(s): SO00001."), "{body}");
        assert_eq!(body.matches("- SO00001").count(), 2, "{body}");
    }

    #[test]
    fn a_cancelled_purchase_speaks_of_the_purchase() {
        let body = purchase_cancelled(&[exception("PO00007", "Out-sourced service", 4.0)]);
        assert!(body.contains("purchase order(s): PO00007"), "{body}");
    }

    #[test]
    fn a_decrease_carries_both_quantities() {
        let body = quantity_decreased("SO00001", "Out-sourced service", 13.0, 16.0);
        assert!(body.contains("13 of Out-sourced service ordered instead of 16"), "{body}");
    }
}
