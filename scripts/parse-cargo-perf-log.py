#!/usr/bin/env python3
"""Print a compact comparison table for cargo-perf guest logs."""

from __future__ import annotations

import re
import sys
from pathlib import Path


COMMAND_RE = re.compile(
    r"\[cargo-perf\] cargo_(new|run_release) "
    r"start_uptime_s=([0-9.]+) end_uptime_s=([0-9.]+) status=(\d+)"
)
SECTION_RE = re.compile(r"^===== CARGO_PERF_(NEW|RUN)_(IO|PROBE)_(BEGIN|END) =====$")
MEM_RE = re.compile(
    r"^===== CARGO_PERF_(NEW|RUN)_(BEFORE|AFTER)_MEMINFO_(BEGIN|END) =====$"
)


def parse_log(path: Path) -> dict:
    result = {
        "path": path,
        "commands": {},
        "io": {"NEW": {}, "RUN": {}},
        "probes": {"NEW": {}, "RUN": {}},
        "mem": {},
    }
    active_kind = None
    active_phase = None
    active_category = None
    active_mem = None

    for raw_line in path.read_text(errors="replace").splitlines():
        line = raw_line.rstrip("\r")
        if match := COMMAND_RE.search(line):
            name, start, end, status = match.groups()
            result["commands"][name] = {
                "seconds": float(end) - float(start),
                "status": int(status),
            }
            continue

        if match := SECTION_RE.match(line):
            phase, kind, edge = match.groups()
            if edge == "BEGIN":
                active_phase, active_kind = phase, kind
            else:
                active_phase = active_kind = None
            active_category = None
            continue

        if match := MEM_RE.match(line):
            phase, when, edge = match.groups()
            if edge == "BEGIN":
                active_mem = f"{phase}_{when}"
                result["mem"][active_mem] = {}
            else:
                active_mem = None
            continue

        if active_mem:
            if ":" in line:
                key, value = line.split(":", 1)
                token = value.strip().split()[0] if value.strip() else ""
                if token.isdigit():
                    result["mem"][active_mem][key] = int(token)
            continue

        if active_kind == "IO":
            if line.endswith(":") and not line.startswith(" "):
                active_category = line[:-1]
                continue
            fields = line.split()
            if active_category and len(fields) == 2 and fields[1].isdigit():
                result["io"][active_phase][f"{active_category}.{fields[0]}"] = int(fields[1])
            continue

        if active_kind == "PROBE":
            fields = line.split()
            if len(fields) == 5 and all(field.isdigit() for field in fields[1:]):
                result["probes"][active_phase][fields[0]] = tuple(map(int, fields[1:]))

    return result


def value(data: dict, key: str, default: int = 0) -> int:
    return data["io"]["RUN"].get(key, default)


def probe(data: dict, name: str, index: int = 0) -> int:
    return data["probes"]["RUN"].get(name, (0, 0, 0, 0))[index]


def mem_delta(data: dict, key: str) -> int:
    before = data["mem"].get("RUN_BEFORE", {}).get(key, 0)
    after = data["mem"].get("RUN_AFTER", {}).get(key, 0)
    return after - before


def fmt_seconds(seconds: float | None) -> str:
    return "-" if seconds is None else f"{seconds:.2f}"


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(f"usage: {argv[0]} LOG [LOG ...]", file=sys.stderr)
        return 2

    logs = [parse_log(Path(arg)) for arg in argv[1:]]
    headers = [
        "log",
        "new_s",
        "run_s",
        "read_MiB",
        "I/O_wait_s",
        "batch calls/reqs",
        "read_many probes",
        "file faults",
        "file_fault_s",
        "fault ready/calls",
        "around map/pre",
        "commit_ms",
        "lookup_s",
        "frame cache h/m",
        "alloc lock kticks",
        "frame allocs",
    ]
    rows = []
    for data in logs:
        commands = data["commands"]
        read_bytes = value(data, "virtio_blk.read_bytes")
        batch_calls = value(data, "virtio_blk.read_many_calls")
        batch_reqs = value(data, "virtio_blk.read_many_reqs")
        lookup_us = value(data, "fs_meta.lookup_inode_follow_total_us")
        rows.append(
            [
                data["path"].stem,
                fmt_seconds(commands.get("new", {}).get("seconds")),
                fmt_seconds(commands.get("run_release", {}).get("seconds")),
                f"{read_bytes / 2**20:.2f}",
                f"{value(data, 'virtio_blk.task_wait_ns') / 1e9:.3f}",
                f"{batch_calls}/{batch_reqs}",
                str(probe(data, "virtio.read_blocks_many")),
                str(probe(data, "mmap.handle_file_page_fault")),
                f"{probe(data, 'mmap.handle_file_page_fault', 1) / 1e9:.3f}",
                f"{value(data, 'page_cache.fault_window_ready_hits')}/"
                f"{value(data, 'page_cache.fault_window_calls')}",
                f"{value(data, 'page_cache.fault_around_mapped_pages')}/"
                f"{value(data, 'page_cache.fault_around_leaf_preflights')}",
                f"{probe(data, 'mmap.file_fault.commit.around', 1) / 1e6:.1f}",
                f"{lookup_us / 1e6:.3f}",
                f"{mem_delta(data, 'FramePerCpuCacheHits')}/"
                f"{mem_delta(data, 'FramePerCpuCacheMisses')}",
                f"{mem_delta(data, 'FrameAllocatorLockWaitTicks') / 1e3:.1f}",
                str(mem_delta(data, "FrameAllocCalls")),
            ]
        )

    widths = [len(header) for header in headers]
    for row in rows:
        widths = [max(width, len(cell)) for width, cell in zip(widths, row)]
    print("  ".join(header.ljust(width) for header, width in zip(headers, widths)))
    print("  ".join("-" * width for width in widths))
    for row in rows:
        print("  ".join(cell.ljust(width) for cell, width in zip(row, widths)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
