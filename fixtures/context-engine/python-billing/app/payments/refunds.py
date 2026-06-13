from decimal import Decimal


def refund_invoice(invoice, gateway):
    return gateway.refund(invoice["account_id"], Decimal(str(invoice["total"])))
