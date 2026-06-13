from app.api.customers import update_customer_profile


def test_update_customer_profile_saves_payload():
    saved = {}

    class Store:
        def save_profile(self, account_id, payload):
            saved[account_id] = payload

    update_customer_profile({"account_id": "acct-1"}, Store())
    assert "acct-1" in saved
