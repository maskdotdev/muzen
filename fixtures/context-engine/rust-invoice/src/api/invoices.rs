use crate::inventory::reserve::reserve_invoice_stock;
use crate::payments::settlement::{settle_invoice, Settlement};

pub fn capture_invoice(customer_id: &str, invoice_id: &str, amount_cents: u64) -> Settlement {
    reserve_invoice_stock(invoice_id);
    settle_invoice(customer_id, invoice_id, amount_cents)
}
