# A running count of registered users, kept with get() + put(). Each event reads
# the current total first, so a batch of N registrations reads through its own
# uncommitted writes and still lands the right count. Query it at
# `GET /read/user-stats/totals/all`.

totals = entity(
    key = "id",
    fields = {
        "id": text(),
        "count": i64_(),
    },
)

source = events(types = ["user.registered"])

def handle(event):
    row = get(totals, "all")
    count = (row["count"] if row else 0) + 1
    return [put(totals, {"id": "all", "count": count})]
