#!/usr/bin/env python3
"""Report upstream commits LVRR has not merged.

Run from the LVRR checkout. Exits 1 when something needs attention, so the
scheduled workflow fails and GitHub sends an email.

Upstreams come from the `## Upstreams` table in CHANGES.md. Subtrees are
merged without --squash, so merged upstream commits are ancestors of HEAD and
anything upstream that HEAD can't reach is missing.

Checks, per upstream:
  * upstream commits not in HEAD, unless acked
  * for subtrees without local edits (xrizer/ changes go to xrizer-edge
    first): changes made in LVRR since the last merge

Acks go in .github/upstream-watch.ack, one per line:
  `commit <sha>`       a single upstream commit
  `seen <name> <sha>`  <sha> and everything before it on that upstream
Anything after `#` is a comment.
"""

import os
import re
import subprocess
import sys

HEAD = os.environ.get("LVRR_REF", "HEAD")
MAX_LISTED = 40
ROOT = subprocess.run(
    ["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=True
).stdout.strip()


def git(*args, check=True):
    return subprocess.run(
        ["git", *args], cwd=ROOT, capture_output=True, text=True, check=check
    )


def read_upstreams():
    text = open(os.path.join(ROOT, "CHANGES.md")).read()
    section = re.search(r"^## Upstreams\n(.*?)(?=^## |\Z)", text, re.M | re.S)
    if not section:
        sys.exit("CHANGES.md: no `## Upstreams` section")
    rows = re.findall(
        r"^\| (\w[\w-]*) \| ([\w.-]+/[\w.-]+) \| ([\w./-]+) \| ([\w./-]+) \| (yes|no) \|",
        section.group(1),
        re.M,
    )
    if not rows:
        sys.exit("CHANGES.md: no rows in the `## Upstreams` table")
    return rows


def read_acks():
    commits, seen = set(), {}
    try:
        lines = open(os.path.join(ROOT, ".github/upstream-watch.ack")).read().splitlines()
    except FileNotFoundError:
        return commits, seen
    for line in lines:
        parts = line.split("#", 1)[0].split()
        if parts[:1] == ["commit"] and len(parts) == 2:
            commits.add(parts[1])
        elif parts[:1] == ["seen"] and len(parts) == 3:
            seen.setdefault(parts[1], []).append(parts[2])
    return commits, seen


def acked(sha, acks):
    return any(sha.startswith(a) or a.startswith(sha) for a in acks)


def check(name, repo, branch, prefix, local_edits, ack_commits, seen):
    url = f"https://github.com/{repo}.git"
    ref = f"refs/upstream-watch/{name}"
    git("fetch", "--quiet", "--no-tags", url, f"+refs/heads/{branch}:{ref}")
    try:
        head = git("rev-parse", ref).stdout.strip()
        base = git("merge-base", HEAD, ref, check=False).stdout.strip()
        if not base:
            return [f"`{prefix}/` has never merged {repo} {branch}"]

        findings = []
        excludes = []
        for sha in seen.get(name, []):
            if git("cat-file", "-e", f"{sha}^{{commit}}", check=False).returncode:
                findings.append(f"ack `seen {name} {sha}`: no such commit upstream")
            else:
                excludes.append(f"^{sha}")
        new = git("rev-list", "--no-merges", "--reverse", ref, f"^{HEAD}", *excludes).stdout.split()
        new = [sha for sha in new if not acked(sha, ack_commits)]
        if new:
            lines = [f"{len(new)} upstream commit(s) not merged:"]
            for sha in new[:MAX_LISTED]:
                lines.append("  - " + git("log", "-1", "--format=`%h` %s (%an, %as)", sha).stdout.strip())
            if len(new) > MAX_LISTED:
                lines.append(f"  - ... and {len(new) - MAX_LISTED} more")
            lines += [
                "",
                "  ```",
                f"  git fetch --no-tags {url} {branch}",
                f'  git subtree merge --prefix={prefix} {head[:10]} -m "{name}: merge {repo} {branch} at {head[:10]}"',
                "  cargo xtask rebrand",
                "  ```",
            ]
            findings.append("\n".join(lines))

        if local_edits == "no":
            drift = git("diff", "--stat", base, f"{HEAD}:{prefix}").stdout.rstrip()
            if drift:
                findings.append(
                    f"`{prefix}/` differs from {repo} `{base[:10]}`; make these changes upstream:\n\n"
                    + "\n".join("      " + line for line in drift.splitlines())
                )
        return findings
    finally:
        git("update-ref", "-d", ref, check=False)


def main():
    ack_commits, seen = read_acks()
    report = ["# Upstream watch", ""]
    attention = False
    for name, repo, branch, prefix, local_edits in read_upstreams():
        findings = check(name, repo, branch, prefix, local_edits, ack_commits, seen)
        report.append(f"## {name}: {repo} `{branch}`")
        report.append("")
        if findings:
            attention = True
            report += [f"- {f}" for f in findings]
        else:
            report.append("Nothing new.")
        report.append("")
    if attention:
        report.append(
            "Merge, or ack in `.github/upstream-watch.ack` (`commit <sha>` or `seen <name> <sha>`)."
        )
    text = "\n".join(report).rstrip() + "\n"
    print(text, end="")
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as f:
            f.write(text)
    return 1 if attention else 0


if __name__ == "__main__":
    sys.exit(main())
