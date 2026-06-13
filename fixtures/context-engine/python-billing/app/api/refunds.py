from app.payments.refunds import refund_invoice


def create_refund(payload, gateway):
    return refund_invoice(payload["invoice"], gateway)
