"""The store itself: open one, write a fact, walk a range of them."""

from store.codec import CodecError, decode_key, encode_key
from store.keys import key_prefix, key_range

DEFAULT_PATH = "example.db"


class StoreError(Exception):
    """Raised when a store cannot be opened, or one of its rows cannot be read."""


class Store:
    """A sorted map from encoded key to value."""

    def __init__(self, path):
        self.path = path
        self.rows = {}

    def put(self, fields, value):
        """Write one fact, returning the key it went under."""
        key = encode_key(fields)
        self.rows[key] = value
        return key

    def get(self, fields):
        """The value stored under these fields, or None."""
        return self.rows.get(encode_key(fields))

    def scan(self, lo, hi):
        """Every row in the half-open range `[lo, hi)`, in key order."""
        for key in sorted(self.rows):
            if hi is not None and key >= hi:
                break
            if key >= lo:
                yield decode_key(key), self.rows[key]


def store_open(path=DEFAULT_PATH):
    """Open the store held at `path`."""
    if not path:
        raise StoreError("a store needs a path")
    return Store(path)


def store_close(store):
    """Drop everything the store holds."""
    store.rows.clear()


def store_flush(store):
    """Write every row the store holds to its path, as decoded tuples."""
    with open(store.path, "w", encoding="utf-8") as out:
        for key in sorted(store.rows):
            try:
                fields = decode_key(key)
            except CodecError as error:
                raise StoreError(f"{store.path}: {error}") from error
            print(fields, store.rows[key], file=out)


def store_scan(store, text):
    """Every row whose leading string field starts with `text`."""
    lo, hi = key_range(key_prefix(text))
    return store.scan(lo, hi)
