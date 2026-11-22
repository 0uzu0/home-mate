#!/usr/bin/env python3
"""Source file line-count gate for Home Mate (`crates/**/*.rs`, `web/src/**/*.{ts,vue}`).

Per #226 (owner decision, 2026-11-21):
- Files exceeding 1,500 lines emit a soft warning (`WARN`, exit 0) so functional
  cohesion and lock-scope clarity take priority over artificial file splitting.
- Files exceeding 3,000 lines fail the gate (`FAIL`, exit 1).
"""

import argparse
import pathlib
import sys
import tempfile

WARN_MAX_LINES = 1500
HARD_MAX_LINES = 3000


def count_lines(path: pathlib.Path) -> int:
    with path.open("r", encoding="utf-8", errors="replace") as f:
        return sum(1 for _ in f)


def check_tree(root: pathlib.Path) -> tuple[int, list[str], list[str]]:
    checked = 0
    warnings: list[str] = []
    violations: list[str] = []
    targets = [
        (root / "crates", (".rs",)),
        (root / "web" / "src", (".ts", ".vue")),
    ]
    for base_dir, exts in targets:
        if not base_dir.exists():
            continue
        for path in sorted(base_dir.rglob("*")):
            if not path.is_file() or path.suffix not in exts:
                continue
            rel = path.relative_to(root).as_posix()
            lines = count_lines(path)
            checked += 1
            if lines > HARD_MAX_LINES:
                violations.append(
                    f"FAIL   {rel}: {lines} lines (hard limit {HARD_MAX_LINES})"
                )
            elif lines > WARN_MAX_LINES:
                warnings.append(
                    f"WARN   {rel}: {lines} lines (soft reminder > {WARN_MAX_LINES})"
                )
    return checked, warnings, violations


def run_self_test() -> int:
    with tempfile.TemporaryDirectory() as tmp:
        root = pathlib.Path(tmp)
        (root / "crates" / "hm-core" / "src").mkdir(parents=True)
        ok_file = root / "crates" / "hm-core" / "src" / "ok.rs"
        ok_file.write_text("fn a() {}\n" * 100, encoding="utf-8")
        checked, warnings, violations = check_tree(root)
        if checked != 1 or warnings or violations:
            print("FAIL   line_gate self-test: under-limit file failed", file=sys.stderr)
            return 1

        warn_file = root / "crates" / "hm-core" / "src" / "warn.rs"
        warn_file.write_text("fn w() {}\n" * (WARN_MAX_LINES + 1), encoding="utf-8")
        checked, warnings, violations = check_tree(root)
        if checked != 2 or len(warnings) != 1 or violations:
            print("FAIL   line_gate self-test: soft-warning file did not warn cleanly", file=sys.stderr)
            return 1

        bad_file = root / "crates" / "hm-core" / "src" / "over.rs"
        bad_file.write_text("fn b() {}\n" * (HARD_MAX_LINES + 1), encoding="utf-8")
        _, _, violations = check_tree(root)
        if len(violations) != 1:
            print("FAIL   line_gate self-test: over-limit file did not fail", file=sys.stderr)
            return 1

    print("PASS   line_gate self-test (under-limit passed, >1500 warned, >3000 rejected)")
    return 0


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description="Check source file line limits.")
    ap.add_argument(
        "--root",
        default=str(pathlib.Path(__file__).resolve().parent.parent),
        help="repository root directory",
    )
    ap.add_argument(
        "--self-test",
        action="store_true",
        help="run internal self-test verifying under-limit, soft-warn, and hard-limit behavior",
    )
    args = ap.parse_args(argv)

    if args.self_test:
        return run_self_test()

    root = pathlib.Path(args.root).resolve()
    checked, warnings, violations = check_tree(root)
    for w in warnings:
        print(w)
    if violations:
        for v in violations:
            print(v, file=sys.stderr)
        print(
            f"FAIL   {len(violations)} of {checked} source files exceeded hard line limit ({HARD_MAX_LINES})",
            file=sys.stderr,
        )
        return 1

    print(
        f"PASS   all {checked} source files within hard line limit (<={HARD_MAX_LINES}; {len(warnings)} soft warnings >{WARN_MAX_LINES})"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
