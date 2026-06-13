from decimal import Decimal


def tax_for_region(region: str, subtotal: Decimal) -> Decimal:
    return subtotal * (Decimal("0.0825") if region == "tx" else Decimal("0"))
