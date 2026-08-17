#!/usr/bin/env python3
"""Wrap a flat kernel binary in a legacy U-Boot image header."""

from __future__ import annotations

import argparse
import struct
import time
import zlib
from pathlib import Path


UIMAGE_MAGIC = 0x27051956
IH_OS_LINUX = 5
IH_ARCH_RISCV = 26
IH_TYPE_KERNEL = 2
IH_COMP_NONE = 0
HEADER_FORMAT = ">7I4B32s"


def parse_address(value: str) -> int:
    return int(value, 0)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("input", type=Path, help="flat kernel binary")
    parser.add_argument("output", type=Path, help="legacy uImage output")
    parser.add_argument("--load", type=parse_address, required=True)
    parser.add_argument("--entry", type=parse_address, required=True)
    parser.add_argument("--name", default="CosmOS VisionFive 2")
    args = parser.parse_args()

    payload = args.input.read_bytes()
    name = args.name.encode("ascii")[:32].ljust(32, b"\0")
    timestamp = int(time.time())
    data_crc = zlib.crc32(payload) & 0xFFFF_FFFF
    fields = (
        UIMAGE_MAGIC,
        0,
        timestamp,
        len(payload),
        args.load,
        args.entry,
        data_crc,
        IH_OS_LINUX,
        IH_ARCH_RISCV,
        IH_TYPE_KERNEL,
        IH_COMP_NONE,
        name,
    )
    header = struct.pack(HEADER_FORMAT, *fields)
    header_crc = zlib.crc32(header) & 0xFFFF_FFFF
    fields = (UIMAGE_MAGIC, header_crc, *fields[2:])
    args.output.write_bytes(struct.pack(HEADER_FORMAT, *fields) + payload)

    print(
        f"{args.output}: payload={len(payload)} load={args.load:#x} "
        f"entry={args.entry:#x} data_crc={data_crc:08x} header_crc={header_crc:08x}"
    )


if __name__ == "__main__":
    main()
