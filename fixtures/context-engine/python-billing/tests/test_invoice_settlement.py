from decimal import Decimal

from app.api.invoices import create_invoice


class Gateway:
    def surcharge_for(self, account_id):
        return Decimal("1.50")

    def capture(self, account_id, amount, idempotency_key):
        return {"id": f"charge:{idempotency_key}", "amount": amount}


class Ledger:
    def record_invoice_settlement(self, invoice_id, charge_id, amount):
        return {"id": f"ledger:{invoice_id}:{charge_id}"}


def test_create_invoice_settles_charge_and_records_ledger_entry():
    response = create_invoice(
        {
            "invoice_id": "inv-1",
            "account_id": "acct-1",
            "subtotal": Decimal("10.00"),
            "region": "none",
            "credit": "2.00",
        },
        Gateway(),
        Ledger(),
    )

    assert response["charged"] == "9.50"
    assert response["ledger_entry"].startswith("ledger:inv-1")
