"""A query becomes a plan: which field narrows the scan, which one only filters."""

from store.keys import key_prefix, key_range


class Plan:
    """What to read: a predicate, the range to walk, and what to filter on the way."""

    def __init__(self, predicate, lo, hi, filters=()):
        self.predicate = predicate
        self.lo = lo
        self.hi = hi
        self.filters = tuple(filters)

    def describe(self):
        """The plan in one line — which is where its cost is visible."""
        shape = "scan" if self.lo == b"" else "seek"
        return f"{self.predicate} {shape} + {len(self.filters)} filter(s)"


def plan_scan(predicate):
    """Every row of a predicate: the widest plan there is."""
    return Plan(predicate, b"", None)


def plan_seek(predicate, text):
    """The rows whose leading string field starts with `text`."""
    lo, hi = key_range(key_prefix(text))
    return Plan(predicate, lo, hi)


def plan_point(predicate, key):
    """One row, named exactly."""
    return Plan(predicate, key, key + bytes([0x00]))


def plan_filter(plan, field, text):
    """The same plan, with a field the range cannot narrow filtered instead."""
    return Plan(plan.predicate, plan.lo, plan.hi, plan.filters + ((field, text),))
