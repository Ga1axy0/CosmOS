import argparse
from pathlib import Path


DEFAULT_LTP_PATH = Path("../os/qemu-run.log")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Summarize LTP results from a qemu-run log."
    )
    parser.add_argument(
        "ltp_path",
        nargs="?",
        type=Path,
        default=DEFAULT_LTP_PATH,
        help=f"Path to the log file (default: {DEFAULT_LTP_PATH})",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()

    with args.ltp_path.open("r") as f:
        lines = f.readlines()

    passed_lines = [line for line in lines if line.startswith("passed")]
    failed_lines = [line for line in lines if line.startswith("failed")]
    broken_lines = [line for line in lines if line.startswith("broken")]
    skipped_lines = [line for line in lines if line.startswith("skipped")]
    warnings_lines = [line for line in lines if line.startswith("warnings")]

    total_passed = sum(int(line.split()[1]) for line in passed_lines)
    total_failed = sum(int(line.split()[1]) for line in failed_lines)
    total_broken = sum(int(line.split()[1]) for line in broken_lines)
    total_skipped = sum(int(line.split()[1]) for line in skipped_lines)
    total_warnings = sum(int(line.split()[1]) for line in warnings_lines)

    print(f"Total Passed: {total_passed}")
    print(f"Total Failed: {total_failed}")
    print(f"Total Broken: {total_broken}")
    print(f"Total Skipped: {total_skipped}")
    print(f"Total Warnings: {total_warnings}")
    print("-" * 30)
    print(
        f"Total Tests: {total_passed + total_failed + total_broken + total_skipped + total_warnings}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
