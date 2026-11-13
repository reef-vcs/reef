#!/usr/bin/env python3
"""Lists the tests nextest's `ci` profile had to retry (.config/nextest.toml).

Reads the JUnit report of the last `cargo nextest run` and removes it, writes a warning for each
test that failed and then passed, and adds them to the job summary. Never fails the job: a
missing or unreadable report is only said.

    scripts/ci-flaky-tests.py [LABEL] [REPORT]
"""

import os
import sys
import xml.etree.ElementTree as ET

label = sys.argv[1] if len(sys.argv) > 1 else "tests"
report = sys.argv[2] if len(sys.argv) > 2 else "target/nextest/ci/junit.xml"

try:
    root = ET.parse(report).getroot()
except (OSError, ET.ParseError) as error:
    print(f"{label}: no JUnit report to read ({error})")
    sys.exit(0)

# Read once: a later step, or the next run on a runner that keeps target/, must not list it again.
try:
    os.remove(report)
except OSError:
    pass

flaky = []
for case in root.iter("testcase"):
    tries = len(case.findall("flakyFailure")) + len(case.findall("flakyError"))
    if tries:
        flaky.append((case.get("classname", "?"), case.get("name", "?"), tries))

if not flaky:
    print(f"{label}: no test was retried")
    sys.exit(0)

lines = [f"### Flaky tests: {label}", "", "| Test | Failed tries |", "| --- | --- |"]
for binary, name, tries in sorted(flaky):
    print(f"::warning title=Flaky test::{binary} {name} failed {tries} time(s), then passed ({label})")
    lines.append(f"| `{binary}` `{name}` | {tries} |")
summary = os.environ.get("GITHUB_STEP_SUMMARY")
if summary:
    with open(summary, "a", encoding="utf-8") as out:
        out.write("\n".join(lines) + "\n\n")
