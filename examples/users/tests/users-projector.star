# Scenarios for the `users` projector: events in, assert rows out. Each case projects
# `given` into a throwaway read model and reads it back through the read API, so the
# rows are what `GET /read/users/users/{user_id}` would return.

load("events/user.star", "user_registered", "user_renamed")

cases = [
    case(
        name = "a registration becomes a row",
        projector = "users",
        given = [user_registered(
            user_id = "11111111-1111-1111-1111-111111111111",
            email = "alice@example.com",
            name = "Alice",
        )],
        expect = {"users": [{
            "user_id": "11111111-1111-1111-1111-111111111111",
            "email": "alice@example.com",
            "name": "Alice",
        }]},
    ),
    case(
        name = "a rename patches the name and leaves the email",
        projector = "users",
        given = [
            user_registered(
                user_id = "22222222-2222-2222-2222-222222222222",
                email = "bob@example.com",
                name = "Bob",
            ),
            user_renamed(
                user_id = "22222222-2222-2222-2222-222222222222",
                name = "Robert",
            ),
        ],
        expect = {"users": [{
            "user_id": "22222222-2222-2222-2222-222222222222",
            "email": "bob@example.com",
            "name": "Robert",
        }]},
    ),
    case(
        name = "a rename with no registration is a no-op patch",
        projector = "users",
        given = [user_renamed(
            user_id = "33333333-3333-3333-3333-333333333333",
            name = "Nobody",
        )],
        expect = {"users": []},
    ),
]
