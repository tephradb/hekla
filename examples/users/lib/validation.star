# Shared pure helpers, importable from commands, projectors and effects.

def is_blank(value):
    return value == None or value.strip() == ""
