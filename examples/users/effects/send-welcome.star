# Reacts to a registration by sending a welcome email, then records delivery
# through the internal command. The durable-execution builtins (`http`,
# `invoke_command`) land in a later phase; this file exists to exercise effect
# loading and source validation, and to document the shape.

source = events(types = ["user.registered"])

def handle(event):
    http.post(
        url = "https://example.test/welcome",
        body = {"email": event.data["email"]},
    )
    invoke_command("record-welcome", {"user_id": event.data["user_id"]})
    return []
