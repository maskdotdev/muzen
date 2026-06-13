from app.payments.settlement import settle_invoice
from app.payments.taxes import tax_for_region


def create_invoice(payload, gateway, ledger):
    subtotal = payload["subtotal"] + tax_for_region(payload["region"], payload["subtotal"])
    invoice = {
        "id": payload["invoice_id"],
        "account_id": payload["account_id"],
        "subtotal": subtotal,
        "credit": payload.get("credit", "0"),
    }
    settlement = settle_invoice(invoice, gateway, ledger)
    return {
        "invoice_id": settlement.invoice_id,
        "charged": str(settlement.charged),
        "ledger_entry": settlement.ledger_entry,
    }
