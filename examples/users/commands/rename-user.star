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

def initial():
    return {"exists": False}

def fold(state, event):
    state["exists"] = True
    return state

def handle(input, state):
    if is_blank(input.name):
        return reject("invalid_name", "name must not be blank")
    if not state["exists"]:
        return reject("unknown_user", "no such user")
    return user_renamed(user_id = input.user_id, name = input.name)
