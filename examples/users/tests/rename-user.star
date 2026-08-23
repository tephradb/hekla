# Scenarios for rename-user, whose boundary spans two event types and whose `fold`
# is a per-type map. The second case seeds a prior rename as well, so both arms run.

load("events/user.star", "user_registered", "user_renamed")

cases = [
    case(
        name = "renames an existing user",
        command = "rename-user",
        given = [user_registered(
            user_id = "55555555-5555-5555-5555-555555555555",
            email = "carol@example.com",
            name = "Carol",
        )],
        input = {
            "user_id": "55555555-5555-5555-5555-555555555555",
            "name": "Caroline",
        },
        expect = user_renamed(
            user_id = "55555555-5555-5555-5555-555555555555",
            name = "Caroline",
        ),
    ),
    case(
        name = "renames a user that was already renamed",
        command = "rename-user",
        given = [
            user_registered(
                user_id = "66666666-6666-6666-6666-666666666666",
                email = "dave@example.com",
                name = "Dave",
            ),
            user_renamed(
                user_id = "66666666-6666-6666-6666-666666666666",
                name = "David",
            ),
        ],
        input = {
            "user_id": "66666666-6666-6666-6666-666666666666",
            "name": "Davey",
        },
        expect = user_renamed(
            user_id = "66666666-6666-6666-6666-666666666666",
            name = "Davey",
        ),
    ),
    case(
        name = "rejects an unknown user",
        command = "rename-user",
        input = {
            "user_id": "77777777-7777-7777-7777-777777777777",
            "name": "Nobody",
        },
        expect = reject("unknown_user", "no such user"),
    ),
]
