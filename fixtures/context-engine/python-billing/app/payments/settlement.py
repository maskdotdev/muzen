from dataclasses import dataclass
from decimal import Decimal


@dataclass(frozen=True)
class SettlementResult:
    invoice_id: str
    charged: Decimal
    ledger_entry: str


def settle_invoice(invoice, gateway, ledger) -> SettlementResult:
    subtotal = Decimal(str(invoice["subtotal"]))
    credit = Decimal(str(invoice.get("credit", "0")))
    surcharge = gateway.surcharge_for(invoice["account_id"])
    amount = subtotal + surcharge - credit
    if amount < Decimal("0"):
        amount = Decimal("0")

    charge = gateway.capture(
        account_id=invoice["account_id"],
        amount=amount,
        idempotency_key=f"invoice:{invoice['id']}",
    )
    entry = ledger.record_invoice_settlement(
        invoice_id=invoice["id"],
        charge_id=charge["id"],
        amount=amount,
    )
    return SettlementResult(
        invoice_id=invoice["id"],
        charged=amount,
        ledger_entry=entry["id"],
    )
