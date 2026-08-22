load("events/user.star", "user_registered")
load("lib/validation.star", "is_blank")

input = schema(
    user_id = uuid(),
    email = text(),
    name = text(),
)

# The consistency boundary: every prior registration for this email. A typed query
# clause is the event definition called with the fields to match; `email` is
# auto-indexed, so it is filterable.
def query(input):
    return user_registered(email = input.email)

def initial():
    return {"taken": False}

def fold(state, event):
    state["taken"] = True
    return state

def handle(input, state):
    if is_blank(input.name):
        return reject("invalid_name", "name must not be blank")
    if state["taken"]:
        return reject("email_taken", "that email is already registered")
    return user_registered(
        user_id = input.user_id,
        email = input.email,
        name = input.name,
    )
