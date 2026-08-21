load("events/user.star", "user_renamed")
load("lib/validation.star", "is_blank")

input = schema(
    user_id = uuid(),
    name = text(),
)

# Both event types carry a `user_id` tag, so the boundary sees this user's whole
# history and can tell whether they exist before renaming.
def query(input):
    return events(
        types = ["user.registered", "user.renamed"],
        tags = {"user_id": input.user_id},
    )

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
    return emit(user_renamed(user_id = input.user_id, name = input.name))
