# Sends an order confirmation. Acting on the customer's email means crossing the
# decrypt boundary explicitly with reveal(): only an effect has it, and every call is
# traced. If the customer was erased before this ran, reveal() fails terminally (the
# address is gone; no retry can recover it) rather than wedging forever.
#
# State comes from the log, not from a projector: `query` scopes a boundary to this
# customer and `fold` counts it. That fold is a function of the log prefix and this
# event's position, so every retry and every replay reproduces it exactly.

load("events/order.star", "order_placed")

def query(event):
    return [order_placed(customer_id = event.data.customer_id)]

initial = {"orders": 0}

def count_order(state, event):
    return {"orders": state["orders"] + 1}

fold = {order_placed(): count_order}

# The boundary is folded up to and including this event, so a customer's first order
# leaves the count at one.
def notify(event, state):
    email = reveal(event.data.email)
    response = http.post(
        url = "https://mail.example/confirm",
        body = {
            "to": email,
            "order_id": event.data.order_id,
            "first_order": state["orders"] == 1,
        },
    )
    if response.status >= 400:
        log("confirmation rejected with status " + str(response.status))

handle = {order_placed(): notify}
