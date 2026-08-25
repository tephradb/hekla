# Scenarios covering all three module kinds over the same event. The projector case is
# the one that shows subject-scoped columns: `email` is stored as ciphertext and read
# back decrypted, so the assertion is plaintext.

load("events/order.star", "order_placed")

ALICE = "11111111-1111-1111-1111-111111111111"
BOB = "22222222-2222-2222-2222-222222222222"

cases = [
    case(
        name = "places a first order",
        command = "place-order",
        input = {
            "order_id": ALICE,
            "customer_id": 1,
            "shop_id": 7,
            "email": "alice@example.com",
            "shipping_address": "1 High St",
            "order_total": "19.99",
            "notes": "leave at door",
        },
        expect = order_placed(
            order_id = ALICE,
            customer_id = 1,
            shop_id = 7,
            email = "alice@example.com",
            shipping_address = "1 High St",
            order_total = "19.99",
            notes = "leave at door",
        ),
    ),
    case(
        name = "rejects a second order on the same email",
        command = "place-order",
        given = [order_placed(
            order_id = ALICE,
            customer_id = 1,
            shop_id = 7,
            email = "alice@example.com",
            shipping_address = "1 High St",
            order_total = "19.99",
            notes = "",
        )],
        input = {
            "order_id": BOB,
            "customer_id": 2,
            "shop_id": 7,
            "email": "alice@example.com",
            "shipping_address": "2 Low Rd",
            "order_total": "5.00",
            "notes": "",
        },
        expect = reject("email_taken", "that email has already placed an order"),
    ),
    case(
        name = "projects the order with its personal columns readable",
        projector = "customer-orders",
        given = [order_placed(
            order_id = ALICE,
            customer_id = 1,
            shop_id = 7,
            email = "alice@example.com",
            shipping_address = "1 High St",
            order_total = "19.99",
            notes = "",
        )],
        expect = {"orders": [{
            "order_id": ALICE,
            "customer_id": 1,
            "email": "alice@example.com",
            "shipping_address": "1 High St",
        }]},
    ),
    case(
        name = "confirms the order to the revealed address",
        effect = "notify-customer",
        given = [order_placed(
            order_id = ALICE,
            customer_id = 1,
            shop_id = 7,
            email = "alice@example.com",
            shipping_address = "1 High St",
            order_total = "19.99",
            notes = "",
        )],
        responds = [http_response(status = 200)],
        expect = [http_call(
            method = "POST",
            url = "https://mail.example/confirm",
            body = {"to": "alice@example.com", "order_id": ALICE, "first_order": True},
        )],
    ),
    # `given` is the effect's state as well as its trigger: the boundary folds the
    # same seeded log, so a repeat customer's second confirmation knows it is one.
    # Both orders fire, since the effect runs over every event its keys select.
    case(
        name = "a repeat customer's confirmation is not their first",
        effect = "notify-customer",
        given = [
            order_placed(
                order_id = ALICE,
                customer_id = 1,
                shop_id = 7,
                email = "alice@example.com",
                shipping_address = "1 High St",
                order_total = "19.99",
                notes = "",
            ),
            order_placed(
                order_id = BOB,
                customer_id = 1,
                shop_id = 7,
                email = "alice@example.com",
                shipping_address = "1 High St",
                order_total = "5.00",
                notes = "",
            ),
        ],
        responds = [http_response(status = 200), http_response(status = 200)],
        expect = [
            http_call(
                url = "https://mail.example/confirm",
                body = {"to": "alice@example.com", "order_id": ALICE, "first_order": True},
            ),
            http_call(
                url = "https://mail.example/confirm",
                body = {"to": "alice@example.com", "order_id": BOB, "first_order": False},
            ),
        ],
    ),
]
