#!/usr/bin/env python3
"""Report upstream xrizer changes the fork has not taken or triaged.

Run from the fork checkout with `gh` authenticated. Exits 1 when something
needs attention, so the scheduled workflow fails and GitHub sends an email.

Checks:
  * upstream main commits missing from the fork (by ancestry, then patch id),
    unless their PR is listed in CHANGES.md or the commit is acked
  * upstream PRs (open, or closed after the baseline date) not listed in
    CHANGES.md
  * PRs marked `merged` in CHANGES.md whose upstream head gained commits
    the fork lacks

Acks go in .github/upstream-watch.ack, one per line: `commit <sha>` or
`pr <number> <head sha>`. Anything after `#` is a comment.
"""

import json
import os
import re
import subprocess
import sys

UPSTREAM = os.environ.get("UPSTREAM_REPO", "Supreeeme/xrizer")
UPSTREAM_URL = f"https://github.com/{UPSTREAM}.git"
FORK_REF = os.environ.get("FORK_REF", "HEAD")
ROOT = subprocess.run(
    ["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=True
).stdout.strip()


def git(*args, check=True):
    return subprocess.run(
        ["git", *args], cwd=ROOT, capture_output=True, text=True, check=check
    )


def gh_api(path):
    out = subprocess.run(
        ["gh", "api", "--paginate", "--slurp", path],
        capture_output=True, text=True, check=True,
    ).stdout
    pages = json.loads(out)
    return [item for page in pages for item in page] if pages and isinstance(pages[0], list) else pages


def is_ancestor(sha, ref):
    return git("merge-base", "--is-ancestor", sha, ref, check=False).returncode == 0


def missing_by_patch(upstream, head, limit):
    """Commits in limit..head with no patch-equivalent in upstream."""
    out = git("cherry", upstream, head, limit).stdout
    return [line.split()[1] for line in out.splitlines() if line.startswith("+")]


def read_changes():
    text = open(os.path.join(ROOT, "CHANGES.md")).read()
    status = {}
    for m in re.finditer(r"^\| (\d+) \| [^|]*\| ([^|]+?) \|", text, re.M):
        status[int(m.group(1))] = m.group(2).strip()
    baseline = re.search(r"^Baseline: upstream `main` at `([0-9a-f]+)` \((\d{4}-\d{2}-\d{2})\)", text, re.M)
    if not baseline:
        sys.exit("CHANGES.md: no `Baseline: upstream `main` at `<sha>` (<date>)` line")
    return status, baseline.group(1), baseline.group(2)


def read_acks():
    commits, prs = set(), set()
    try:
        lines = open(os.path.join(ROOT, ".github/upstream-watch.ack")).read().splitlines()
    except FileNotFoundError:
        return commits, prs
    for line in lines:
        parts = line.split("#", 1)[0].split()
        if parts[:1] == ["commit"] and len(parts) == 2:
            commits.add(parts[1])
        elif parts[:1] == ["pr"] and len(parts) == 3:
            prs.add((int(parts[1]), parts[2]))
    return commits, prs


def acked(sha, acks):
    return any(sha.startswith(a) or a.startswith(sha) for a in acks)


def main():
    status, baseline_sha, baseline_date = read_changes()
    ack_commits, ack_prs = read_acks()
    git("fetch", "--quiet", "--no-tags", UPSTREAM_URL, "+refs/heads/main:refs/upstream-watch/main")
    upstream_main = "refs/upstream-watch/main"
    findings = []

    # Upstream main commits missing from the fork.
    for sha in missing_by_patch(FORK_REF, upstream_main, baseline_sha):
        if acked(sha, ack_commits):
            continue
        prs = [p["number"] for p in gh_api(f"repos/{UPSTREAM}/commits/{sha}/pulls")]
        if any(p in status for p in prs):
            continue
        subject = git("log", "-1", "--format=%s", sha).stdout.strip()
        ref = ", ".join(f"#{p}" for p in prs) or "no PR"
        findings.append(f"upstream main commit `{sha[:10]}` ({ref}) not in fork: {subject}")

    # Untracked PRs and moved heads of merged ones.
    pulls = gh_api(f"repos/{UPSTREAM}/pulls?state=all&sort=updated&direction=desc&per_page=100")
    heads = {}
    for p in pulls:
        n = p["number"]
        if n not in status:
            if p["state"] == "open":
                draft = " (draft)" if p["draft"] else ""
                findings.append(f"open PR #{n} not in CHANGES.md{draft}: {p['title']}")
            elif (p["closed_at"] or "") >= baseline_date and not p["merged_at"]:
                findings.append(f"PR #{n} closed unmerged {p['closed_at'][:10]}, not in CHANGES.md: {p['title']}")
            # Merged upstream PRs surface through the main commit check.
        elif status[n] == "merged" and not p["merged_at"]:
            heads[n] = (p["head"]["sha"], p["title"])

    if heads:
        git("fetch", "--quiet", "--no-tags", UPSTREAM_URL,
            *[f"+refs/pull/{n}/head:refs/upstream-watch/pr/{n}" for n in heads])
    for n, (sha, title) in sorted(heads.items()):
        if (n, sha) in ack_prs or any(n == a and sha.startswith(s) for a, s in ack_prs):
            continue
        if is_ancestor(sha, FORK_REF):
            continue
        new = missing_by_patch(FORK_REF, sha, upstream_main)
        if new:
            findings.append(f"merged PR #{n} has {len(new)} upstream commit(s) not in fork, head `{sha[:10]}`: {title}")

    git("update-ref", "-d", upstream_main)
    for n in heads:
        git("update-ref", "-d", f"refs/upstream-watch/pr/{n}", check=False)

    report = [f"# Upstream watch: {UPSTREAM}", ""]
    if findings:
        report += [f"- {f}" for f in findings]
        report += ["", "Triage by adding a row to CHANGES.md (any status) or an entry in `.github/upstream-watch.ack`."]
    else:
        report.append("Nothing new.")
    text = "\n".join(report) + "\n"
    print(text, end="")
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as f:
            f.write(text)
    return 1 if findings else 0


if __name__ == "__main__":
    sys.exit(main())
