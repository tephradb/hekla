# Shared event definitions. Commands import these and emit them; the runtime
# validates each payload against the fields and derives a tag from every indexed
# field automatically (opt a field out with indexed = False).

user_registered = event(
    type = "user.registered",
    fields = {
        "user_id": uuid(),
        "email": str(),
        "name": str(),
    },
)

user_renamed = event(
    type = "user.renamed",
    fields = {
        "user_id": uuid(),
        "name": str(),
    },
)

user_welcomed = event(
    type = "user.welcomed",
    fields = {
        "user_id": uuid(),
    },
)

reminder_scheduled = event(
    type = "reminder.scheduled",
    fields = {
        "user_id": uuid(),
        # Domain time, set from the request's pinned clock by the command.
        "due_at": timestamp(),
    },
)
