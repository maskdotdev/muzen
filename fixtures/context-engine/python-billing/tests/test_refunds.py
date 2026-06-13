from app.api.refunds import create_refund


def test_create_refund_delegates_to_gateway():
    class Gateway:
        def refund(self, account_id, amount):
            return {"account_id": account_id, "amount": amount}

    result = create_refund({"invoice": {"account_id": "acct-1", "total": "3.00"}}, Gateway())
    assert result["account_id"] == "acct-1"
