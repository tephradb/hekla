load("events/order.star", "order_placed")

input = schema(
    order_id = uuid(),
    customer_id = uint(),
    shop_id = uint(),
    email = str(),
    shipping_address = str(),
    order_total = money(),
    notes = str(),
)

# One order per email address (a launch-day, one-per-person rule). Constraining only
# the `unique` field resolves to its global-key tag, which matches across customers;
# a per-customer scoped tag could not. The runtime encrypts the filter value the same
# way it encrypted the stored tag, so the boundary actually matches.
def query(input):
    return order_placed(email = input.email)

initial = {"taken": False}

fold = {order_placed(): lambda state, event: dict(state, taken = True)}

def handle(input, state):
    if state["taken"]:
        return reject("email_taken", "that email has already placed an order")
    return order_placed(
        order_id = input.order_id,
        customer_id = input.customer_id,
        shop_id = input.shop_id,
        email = input.email,
        shipping_address = input.shipping_address,
        order_total = input.order_total,
        notes = input.notes,
    )
