# Shared event definitions. Commands import these and emit them; the runtime
# validates each payload against the fields and derives tags from the tag fields.

user_registered = event(
    type = "user.registered",
    fields = {
        "user_id": uuid(),
        "email": text(),
        "name": text(),
    },
    tags = ["user_id", "email"],
)

user_renamed = event(
    type = "user.renamed",
    fields = {
        "user_id": uuid(),
        "name": text(),
    },
    tags = ["user_id"],
)

user_welcomed = event(
    type = "user.welcomed",
    fields = {
        "user_id": uuid(),
    },
    tags = ["user_id"],
)

reminder_scheduled = event(
    type = "reminder.scheduled",
    fields = {
        "user_id": uuid(),
        # Domain time, set from the request's pinned clock by the command.
        "due_at": timestamp(),
    },
    tags = ["user_id"],
)
