use muzen_context_rust_invoice::payments::refunds::refund_invoice;

#[test]
fn refund_releases_reserved_stock() {
    assert!(refund_invoice("invoice-456"));
}
