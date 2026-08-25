# A read model of orders by id. The customer's personal columns are subject-scoped,
# so they mirror the event's `subject = "customer_id"` declaration: the projector
# stores the ciphertext (it only ever handles the opaque handle), and the read API
# decrypts on the way out. Filter by the plaintext `customer_id` (a subject-encrypted
# column cannot be indexed, since a filter arrives as plaintext).
#
# `GET /read/orders/orders/{order_id}` returns one order with the personal fields
# decrypted; after `hekla erase customer_id <id>` those fields read back as absent.

load("events/order.star", "order_placed")

orders = entity(
    key = "order_id",
    fields = {
        "order_id": uuid(),
        "customer_id": uint(),
        "email": str(subject = "customer_id", max_length = 200),
        "shipping_address": str(subject = "customer_id", max_length = 200),
    },
    indexes = [index("by_customer", ["customer_id"])],
)

handle = {
    order_placed(): lambda event: [put(orders, {
        "order_id": event.data.order_id,
        "customer_id": event.data.customer_id,
        "email": event.data.email,
        "shipping_address": event.data.shipping_address,
    })],
}
