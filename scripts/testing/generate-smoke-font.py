#!/usr/bin/env python3
"""Generate a tiny TrueType font for SyncMyFonts smoke tests.

The font is intentionally created at test time so the repo does not need to
vendor a third-party font binary. It contains a single .notdef glyph and enough
metadata for platform font loaders to accept it as a real TrueType font.
"""

from __future__ import annotations

import argparse
import struct
from pathlib import Path


def pad4(data: bytes) -> bytes:
    return data + (b"\0" * ((4 - len(data) % 4) % 4))


def u16(value: int) -> bytes:
    return struct.pack(">H", value)


def i16(value: int) -> bytes:
    return struct.pack(">h", value)


def u32(value: int) -> bytes:
    return struct.pack(">I", value & 0xFFFFFFFF)


def i32(value: int) -> bytes:
    return struct.pack(">i", value)


def table_checksum(data: bytes) -> int:
    padded = pad4(data)
    return sum(struct.unpack(f">{len(padded) // 4}I", padded)) & 0xFFFFFFFF


def name_table() -> bytes:
    strings = [
        (1, "SyncMyFonts Smoke"),
        (2, "Regular"),
        (4, "SyncMyFonts Smoke Regular"),
        (6, "SyncMyFontsSmoke-Regular"),
    ]
    records: list[tuple[int, int, int, int, int, int]] = []
    string_data = b""
    for name_id, value in strings:
        raw = value.encode("utf-16-be")
        records.append((3, 1, 0x0409, name_id, len(raw), len(string_data)))
        string_data += raw

    header = u16(0) + u16(len(records)) + u16(6 + 12 * len(records))
    return header + b"".join(struct.pack(">HHHHHH", *record) for record in records) + string_data


def os2_table() -> bytes:
    data = b""
    data += u16(4) + i16(400) + u16(5) + u16(0) + u16(400)
    data += u16(80) + u16(200) + u16(0) + u16(200) + u16(0)
    data += b"".join(i16(0) for _ in range(10))
    data += b"SMFS"
    data += u32(0) + u32(0) + u32(0) + u32(0)
    data += b"\0" * 10
    data += u16(32) + u16(32)
    data += i16(800) + i16(-200) + i16(200) + u16(800) + u16(200)
    data += u32(0) + u32(0)
    data += u16(0) + u16(0) + u16(0) + u16(0) + u16(0) + u16(0)
    return data


def cmap_table() -> bytes:
    seg_count = 2
    search_range = 2 * (1 << (seg_count.bit_length() - 1))
    entry_selector = seg_count.bit_length() - 1
    range_shift = 2 * seg_count - search_range
    subtable = (
        u16(4)
        + u16(32)
        + u16(0)
        + u16(seg_count * 2)
        + u16(search_range)
        + u16(entry_selector)
        + u16(range_shift)
        + u16(0x0041)
        + u16(0xFFFF)
        + u16(0)
        + u16(0x0041)
        + u16(0xFFFF)
        + u16((1 - 0x0041) & 0xFFFF)
        + u16(1)
        + u16(0)
        + u16(0)
    )
    return u16(0) + u16(1) + u16(3) + u16(1) + u32(12) + subtable


def simple_glyph(points: list[tuple[int, int]]) -> bytes:
    if not points:
        return i16(0) + i16(0) + i16(0) + i16(0) + i16(0)

    x_values = [point[0] for point in points]
    y_values = [point[1] for point in points]
    x_deltas = [x_values[0]] + [x_values[index] - x_values[index - 1] for index in range(1, len(x_values))]
    y_deltas = [y_values[0]] + [y_values[index] - y_values[index - 1] for index in range(1, len(y_values))]
    return (
        i16(1)
        + i16(min(x_values))
        + i16(min(y_values))
        + i16(max(x_values))
        + i16(max(y_values))
        + u16(len(points) - 1)
        + u16(0)
        + bytes([0x01] * len(points))
        + b"".join(i16(delta) for delta in x_deltas)
        + b"".join(i16(delta) for delta in y_deltas)
    )


def build_font() -> bytes:
    notdef_glyph = simple_glyph([])
    a_glyph = simple_glyph([(50, 0), (300, 700), (550, 0), (430, 0), (300, 420), (170, 0)])
    glyf = pad4(notdef_glyph) + a_glyph
    maxp_fields = [
        6,  # maxPoints
        1,  # maxContours
        0,  # maxCompositePoints
        0,  # maxCompositeContours
        1,  # maxZones
        0,  # maxTwilightPoints
        0,  # maxStorage
        0,  # maxFunctionDefs
        0,  # maxInstructionDefs
        0,  # maxStackElements
        0,  # maxSizeOfInstructions
        0,  # maxComponentElements
        0,  # maxComponentDepth
    ]
    tables: dict[str, bytes] = {
        "cmap": cmap_table(),
        "glyf": glyf,
        "head": (
            u32(0x00010000)
            + u32(0x00010000)
            + u32(0)
            + u32(0x5F0F3CF5)
            + u16(0)
            + u16(1000)
            + (b"\0" * 16)
            + i16(0)
            + i16(0)
            + i16(600)
            + i16(800)
            + u16(0)
            + u16(8)
            + i16(2)
            + i16(0)
            + i16(0)
        ),
        "hhea": (
            u32(0x00010000)
            + i16(800)
            + i16(-200)
            + i16(200)
            + u16(600)
            + i16(0)
            + i16(0)
            + i16(550)
            + i16(1)
            + i16(0)
            + b"".join(i16(0) for _ in range(4))
            + i16(0)
            + u16(2)
        ),
        "hmtx": u16(600) + i16(0) + u16(600) + i16(0),
        "loca": u16(0) + u16(len(pad4(notdef_glyph)) // 2) + u16(len(glyf) // 2),
        "maxp": u32(0x00010000) + u16(2) + b"".join(u16(value) for value in maxp_fields),
        "name": name_table(),
        "OS/2": os2_table(),
        "post": u32(0x00030000) + i32(0) + u32(0) + u32(0) + u32(0) + u32(0) + u32(0) + u32(0),
    }

    tags = sorted(tables)
    table_count = len(tags)
    max_power = 1 << (table_count.bit_length() - 1)
    search_range = max_power * 16
    entry_selector = max_power.bit_length() - 1
    range_shift = table_count * 16 - search_range

    offset = 12 + table_count * 16
    records: list[tuple[str, int, int, int]] = []
    body = b""
    for tag in tags:
        data = tables[tag]
        records.append((tag, table_checksum(data), offset, len(data)))
        body += pad4(data)
        offset += len(pad4(data))

    font = u32(0x00010000) + u16(table_count) + u16(search_range) + u16(entry_selector) + u16(range_shift)
    for tag, checksum, table_offset, length in records:
        font += tag.encode("ascii") + u32(checksum) + u32(table_offset) + u32(length)
    font += body

    head_offset = {tag: table_offset for tag, _, table_offset, _ in records}["head"]
    adjustment = (0xB1B0AFBA - table_checksum(font)) & 0xFFFFFFFF
    return font[: head_offset + 8] + u32(adjustment) + font[head_offset + 12 :]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_bytes(build_font())


if __name__ == "__main__":
    main()
