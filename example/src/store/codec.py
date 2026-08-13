"""The tuple codec: the byte encoding a key is made of.

A key is a sequence of fields laid out back to back, each tagged with a marker
byte, so that a reader can tell what a field is — and skip it — without knowing
the schema it was written under.
"""

MARK_TERM = 0x00
MARK_INT = 0x20
MARK_STR = 0x30


class CodecError(Exception):
    """Raised when bytes are not a well-formed tuple."""


def encode_int(value):
    """One integer field: the marker, then eight bytes, sign bit flipped.

    Flipping the sign bit is what makes the unsigned byte order the signed
    numeric order, so a scan over the raw bytes visits -1 before 0.
    """
    return bytes([MARK_INT]) + (value ^ (1 << 63)).to_bytes(8, "big")


def encode_str(text):
    """One string field: the marker, the UTF-8 bytes, then the terminator."""
    return bytes([MARK_STR]) + text.encode("utf-8") + bytes([MARK_TERM])


def encode_key(fields):
    """A whole key: every field back to back, in the order given."""
    out = bytearray()
    for field in fields:
        if isinstance(field, int):
            out += encode_int(field)
        else:
            out += encode_str(field)
    return bytes(out)


def decode_int(data, at):
    """The integer field at `at`, and where the next field starts."""
    if data[at] != MARK_INT:
        raise CodecError(f"expected an integer field at {at}")
    return int.from_bytes(data[at + 1 : at + 9], "big") ^ (1 << 63), at + 9


def decode_str(data, at):
    """The string field at `at`, and where the next field starts."""
    if data[at] != MARK_STR:
        raise CodecError(f"expected a string field at {at}")
    end = data.find(MARK_TERM, at + 1)
    if end < 0:
        raise CodecError(f"unterminated string field at {at}")
    return data[at + 1 : end].decode("utf-8"), end + 1


def decode_key(data):
    """Every field of a key, in encoding order."""
    fields = []
    at = 0
    while at < len(data):
        if data[at] == MARK_INT:
            field, at = decode_int(data, at)
        elif data[at] == MARK_STR:
            field, at = decode_str(data, at)
        else:
            raise CodecError(f"unknown marker {data[at]:#x} at {at}")
        fields.append(field)
    return fields
