"""Run a plan against a store, and print what it answers with."""

import store.engine
from query.plan import Plan, plan_filter, plan_scan, plan_seek
from store.engine import Store


def run_plan(store, plan):
    """Every row the plan matches, in key order."""
    if not isinstance(plan, Plan):
        raise TypeError("run_plan wants a Plan")
    if not isinstance(store, Store):
        raise TypeError("run_plan wants a Store")
    for fields, value in store.scan(plan.lo, plan.hi):
        if run_filters(plan, fields):
            yield fields, value


def run_filters(plan, fields):
    """Whether a row survives the filters the range could not narrow."""
    return all(str(fields[field]).startswith(text) for field, text in plan.filters)


def run_scan(store, predicate):
    """Every row of a predicate."""
    return run_plan(store, plan_scan(predicate))


def run_seek(store, predicate, text):
    """The rows of a predicate whose leading string field starts with `text`."""
    return run_plan(store, plan_seek(predicate, text))


def main():
    """Write a handful of declarations, then ask for them by prefix."""
    db = store.engine.store_open("declarations.db")
    db.put(["encode_int", 17], "def")
    db.put(["encode_str", 27], "def")
    db.put(["decode_int", 41], "def")

    for fields, value in run_seek(db, "decl", "encode"):
        print(fields, value)

    filtered = plan_filter(plan_scan("decl"), 0, "de")
    for fields, value in run_plan(db, filtered):
        print(fields, value)


if __name__ == "__main__":
    main()
