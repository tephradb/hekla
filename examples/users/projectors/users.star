# A read model of users by id, with a secondary index on email. Entities are
# collected implicitly from module scope; the table is named after this binding.
# Read one by key at `GET /read/users/users/{user_id}`, or filter on the indexed
# email at `GET /read/users/users?email=alice@example.com`.

load("events/user.star", "user_registered", "user_renamed")

users = entity(
    key = "user_id",
    fields = {
        "user_id": uuid(),
        "email": str(),
        "name": str(),
    },
    indexes = [index("by_email", ["email"])],
)

# The keys are the subscription: they say which events to read and what to do with
# each, so there is no second list to keep in step with them. Every arm whose clause
# matches runs, in declaration order.
handle = {
    user_registered(): lambda event: [put(users, {
        "user_id": event.data.user_id,
        "email": event.data.email,
        "name": event.data.name,
    })],
    user_renamed(): lambda event: [
        patch(users, event.data.user_id, {"name": event.data.name}),
    ],
}
