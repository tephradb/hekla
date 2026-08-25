# Scenarios for the `send-welcome` effect: events in, assert the external calls out.
# `responds` stubs the HTTP replies in the order the handler makes its calls, and
# `expect` is the sequence it should have produced.

load("events/user.star", "user_registered", "user_renamed")

cases = [
    case(
        name = "posts the welcome then records it",
        effect = "send-welcome",
        given = [user_registered(
            user_id = "11111111-1111-1111-1111-111111111111",
            email = "alice@example.com",
            name = "Alice",
        )],
        responds = [http_response(status = 200)],
        expect = [
            http_call(
                method = "POST",
                url = "https://example.test/welcome",
                body = {"email": "alice@example.com"},
            ),
            command_call("record-welcome", {"user_id": "11111111-1111-1111-1111-111111111111"}),
        ],
    ),
    case(
        name = "a 4xx stops before the internal command",
        effect = "send-welcome",
        given = [user_registered(
            user_id = "22222222-2222-2222-2222-222222222222",
            email = "bob@example.com",
            name = "Bob",
        )],
        responds = [http_response(status = 422)],
        expect = [http_call(url = "https://example.test/welcome")],
    ),
    case(
        name = "an event no arm selects does nothing",
        effect = "send-welcome",
        given = [user_renamed(
            user_id = "33333333-3333-3333-3333-333333333333",
            name = "Carol",
        )],
        expect = [],
    ),
]
