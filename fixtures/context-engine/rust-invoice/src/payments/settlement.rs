use crate::payments::taxes::apply_invoice_tax;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settlement {
    pub invoice_id: String,
    pub total_cents: u64,
    pub captured: bool,
}

pub fn settle_invoice(customer_id: &str, invoice_id: &str, amount_cents: u64) -> Settlement {
    let total_cents = apply_invoice_tax(customer_id, amount_cents);
    Settlement {
        invoice_id: invoice_id.to_string(),
        total_cents,
        captured: true,
    }
}
