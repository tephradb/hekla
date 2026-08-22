# Schedules a reminder for a user. It has no invariants, so it omits `query` and
# `fold`; its point is to show `now()`, which is available only in `handle` and is
# pinned once per request. The wall clock becomes domain data (`due_at`), not a
# restatement of the envelope's append timestamp.

load("events/user.star", "reminder_scheduled")

input = schema(
    user_id = uuid(),
)

def handle(input, state):
    return reminder_scheduled(
        user_id = input.user_id,
        due_at = now(),
    )
