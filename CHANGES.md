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
cargo xtask rebrand
```

`cargo xtask rebrand` re-applies the LVRR name and logo. On a merge conflict in a rebranded line, take upstream's side and run it again; `cargo xtask rebrand --check` (also run by CI) reports rules that no longer match.

## LVRR changes

| Component | Change | Files |
|-----------|--------|-------|
| wivrn | app id `io.github.lvrr`, detected by the dashboard over adb | `build.gradle`, `dashboard/adb.cpp` |
| wivrn, wayvr | LVRR name in user-visible strings and translations, LVRR logo | applied by `cargo xtask rebrand`, see `xtask/src/rebrand.rs` |
| wayvr | LVRR assets (`lvrr-assets/`) looked up before upstream ones; language files rebranded on load | `Cargo.toml`, `dash-frontend/src/assets.rs`, `wayvr/src/gui/asset.rs`, new `lvrr-common` crate |
