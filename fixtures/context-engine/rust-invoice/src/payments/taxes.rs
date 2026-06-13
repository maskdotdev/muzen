pub fn apply_invoice_tax(_customer_id: &str, amount_cents: u64) -> u64 {
    amount_cents + (amount_cents / 10)
}
