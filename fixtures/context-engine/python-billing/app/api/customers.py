def update_customer_profile(payload, store):
    store.save_profile(payload["account_id"], payload)
    return {"ok": True}
