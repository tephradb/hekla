# An internal command: invokable by effects, never HTTP-routed. It records that a
# welcome was delivered, so nobody can POST a fabricated `user.welcomed`. It has
# no invariants, so it omits `query` and `fold`.

load("events/user.star", "user_welcomed")

input = schema(
    user_id = uuid(),
)

def handle(input, state):
    return emit(user_welcomed(user_id = input.user_id))
