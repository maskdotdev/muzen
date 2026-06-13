use crate::inventory::release::release_invoice_stock;

pub fn refund_invoice(invoice_id: &str) -> bool {
    release_invoice_stock(invoice_id)
}
