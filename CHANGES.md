# Changes

Upstreams merged into LVRR, and LVRR's own changes to them.

## Upstreams

Merged as git subtrees without `--squash`, which keeps upstream history and authors. `.github/scripts/upstream-watch.py` reports unmerged upstream commits every Monday.

| Name | Repo | Branch | Prefix | Local edits |
|------|------|--------|--------|-------------|
| xrizer | 360900/xrizer-edge | main | xrizer | no |
| wivrn | WiVRn/WiVRn | master | wivrn | yes |
| wayvr | wayvr-org/wayvr | main | wayvr | yes |

`xrizer/` takes no local edits. Changes go to xrizer-edge first, which tracks Supreeeme/xrizer.

Merging an upstream:

```sh
git fetch --no-tags https://github.com/WiVRn/WiVRn.git master
git subtree merge --prefix=wivrn <sha> -m "wivrn: merge WiVRn/WiVRn master at <sha>"
```

## LVRR changes

| Component | Change | Files |
|-----------|--------|-------|
| wivrn | app id `io.github.lvrr`, detected by the dashboard over adb | `build.gradle`, `dashboard/adb.cpp` |
