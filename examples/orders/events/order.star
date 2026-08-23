# `order.placed` carries data for two different subjects: the customer and the shop.
# Per-field subjects (rather than scoping the whole event to one) let each be erased
# independently: shredding the customer leaves the shop's order record intact, and
# shredding the shop never touches the customer's personal fields. This is why the
# scope is per-field.

order_placed = event(
    type = "order.placed",
    fields = {
        "order_id": uuid(),
        # Subject ids stay plaintext: they are how the runtime finds the key.
        "customer_id": uint(),
        "shop_id": uint(),
        # Personal, scoped to the customer. `email` is also `unique`, so a global-key
        # tag enforces one order per email across customers and survives erasure of
        # any one customer (see ARCHITECTURE.md section 15 for the tradeoff).
        "email": str(subject = "customer_id", unique = True, max_length = 200),
        "shipping_address": str(subject = "customer_id", max_length = 200),
        # The shop's commercial figure, scoped to the shop.
        "order_total": money(subject = "shop_id"),
        # Free text nobody queries: opt out of tagging (and of being a huge tag).
        "notes": str(indexed = False),
    },
)
