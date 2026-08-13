"""Key ranges: turning a pattern into the bounds of a scan."""

from store.codec import encode_key, encode_str


def key_of(fields):
    """The key a fact with these fields is stored under."""
    return encode_key(fields)


def key_prefix(text):
    """The bytes every key whose leading string field starts with `text` begins with.

    The terminator is what it drops: with it the bytes are one whole string, and
    the scan would be an equality rather than a range.
    """
    return encode_str(text)[:-1]


def key_successor(prefix):
    """The first key that does not start with `prefix`, or None if there is none."""
    out = bytearray(prefix)
    while out and out[-1] == 0xFF:
        out.pop()
    if not out:
        return None
    out[-1] += 1
    return bytes(out)


def key_range(prefix):
    """The half-open range a prefix scan walks: every key starting with `prefix`."""
    return prefix, key_successor(prefix)
