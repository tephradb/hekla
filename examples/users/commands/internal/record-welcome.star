# An internal command: invokable by effects, never HTTP-routed. It records that a
# welcome was delivered, so nobody can POST a fabricated `user.welcomed`.
#
# It carries a DCB boundary on this user's `user.welcomed`, so it is idempotent
# under replay. An effect that crashes between appending this event and recording
# its journal entry re-invokes on restart; the deterministic idempotency key is
# cleared at startup (like every pending key), so the boundary is what makes the
# second append a no-op reject instead of a duplicate event. That is what makes
# invoke_command land the domain fact exactly once across replays.

load("events/user.star", "user_welcomed")

input = schema(
    user_id = uuid(),
)

def query(input):
    return events(types = ["user.welcomed"], tags = {"user_id": input.user_id})

def fold(state, event):
    return True

initial = False

def handle(input, state):
    if state:
        return reject("already_welcomed", "a welcome was already recorded for this user")
    return emit(user_welcomed(user_id = input.user_id))
