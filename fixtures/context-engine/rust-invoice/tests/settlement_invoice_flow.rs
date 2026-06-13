use muzen_context_rust_invoice::api::invoices::capture_invoice;

#[test]
fn invoice_capture_reserves_stock_and_settles_payment() {
    let settlement = capture_invoice("customer-123", "invoice-456", 10_000);

    assert_eq!(settlement.invoice_id, "invoice-456");
    assert_eq!(settlement.total_cents, 11_000);
    assert!(settlement.captured);
}
