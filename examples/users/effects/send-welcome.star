# Reacts to a registration by sending a welcome email, then recording delivery
# through the internal command. It exercises the durable-execution builtins: a
# journaled `http.post` whose result the handler inspects, `log()`, and
# `invoke_command`, which lands the internal command exactly once across replays.
#
# The runtime absorbs transport errors and 5xx (they never reach here), so a
# `status >= 400` is a real 4xx to decide on. A crash after the POST but before
# the journal write replays the POST; the `invoke_command` is exactly-once.

load("events/user.star", "user_registered")

source = [user_registered()]

def handle(event):
    response = http.post(
        url = "https://example.test/welcome",
        body = {"email": event.data.email},
    )
    if response["status"] >= 400:
        log("welcome email rejected with status " + str(response["status"]))
        return
    invoke_command("record-welcome", {"user_id": event.data.user_id})
