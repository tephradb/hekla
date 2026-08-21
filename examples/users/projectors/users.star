# A read model of users by id, with a secondary index on email. Entities are
# collected implicitly from module scope; the table is named after this binding.

users = entity(
    key = "user_id",
    fields = {
        "user_id": uuid(),
        "email": text(),
        "name": text(),
    },
    indexes = [index("by_email", ["email"])],
)

source = events(types = ["user.registered", "user.renamed"])

def handle(event):
    if event.type == "user.registered":
        return [put(users, {
            "user_id": event.data["user_id"],
            "email": event.data["email"],
            "name": event.data["name"],
        })]
    return [patch(users, event.data["user_id"], {"name": event.data["name"]})]
