load("events/user.star", "user_registered", "user_renamed")
load("lib/validation.star", "is_blank")

input = schema(
    user_id = uuid(),
    name = str(),
)

# Both event types carry `user_id`, so the boundary sees this user's whole history
# and can tell whether they exist before renaming. Each clause is one event type
# constrained on `user_id`; the clauses OR together.
def query(input):
    return [
        user_registered(user_id = input.user_id),
        user_renamed(user_id = input.user_id),
    ]

initial = {"exists": False}

# `user_renamed` is in the boundary so a concurrent rename conflicts, not because the
# decision needs it: `exists` is settled by the registration. The boundary is the
# append condition and the fold is the decision state, so they answer different
# questions and need not name the same types.
#
# Keys are the loaded definitions rather than type strings, so a typo fails at load.
# The arm returns the new state; `initial` is frozen, so mutating it would fail.
fold = {
    user_registered: lambda state, event: dict(state, exists = True),
}

def handle(input, state):
    if is_blank(input.name):
        return reject("invalid_name", "name must not be blank")
    if not state["exists"]:
        return reject("unknown_user", "no such user")
    return user_renamed(user_id = input.user_id, name = input.name)
