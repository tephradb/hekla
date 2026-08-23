# Sends an order confirmation. Acting on the customer's email means crossing the
# decrypt boundary explicitly with reveal(): only an effect has it, and every call is
# traced. If the customer was erased before this ran, reveal() fails terminally (the
# address is gone; no retry can recover it) rather than wedging forever.

load("events/order.star", "order_placed")

source = [order_placed()]

def handle(event):
    email = reveal(event.data.email)
    response = http.post(
        url = "https://mail.example/confirm",
        body = {"to": email, "order_id": event.data.order_id},
    )
    if response["status"] >= 400:
        log("confirmation rejected with status " + str(response["status"]))
