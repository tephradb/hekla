# Scenarios for register-user: events in, assert events out. Each case runs the
# real command against a store seeded with `given`, so `query` and the boundary
# guard are genuinely exercised.

load("events/user.star", "user_registered")

cases = [
    case(
        name = "registers a new email",
        command = "register-user",
        input = {
            "user_id": "11111111-1111-1111-1111-111111111111",
            "email": "alice@example.com",
            "name": "Alice",
        },
        expect = emit(user_registered(
            user_id = "11111111-1111-1111-1111-111111111111",
            email = "alice@example.com",
            name = "Alice",
        )),
    ),
    case(
        name = "rejects a taken email",
        command = "register-user",
        given = [user_registered(
            user_id = "22222222-2222-2222-2222-222222222222",
            email = "alice@example.com",
            name = "Alice",
        )],
        input = {
            "user_id": "33333333-3333-3333-3333-333333333333",
            "email": "alice@example.com",
            "name": "Alice Again",
        },
        expect = reject("email_taken", "that email is already registered"),
    ),
    case(
        name = "rejects a blank name",
        command = "register-user",
        input = {
            "user_id": "44444444-4444-4444-4444-444444444444",
            "email": "bob@example.com",
            "name": "   ",
        },
        expect = reject("invalid_name", "name must not be blank"),
    ),
]
