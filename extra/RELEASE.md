# Cutting a release

There is no CI job for this yet — it is a manual sequence, documented here
rather than automated, because it is short and the macOS half can only run on
a Mac. Follow it in order; steps are marked with where they run.

## 1. Before tagging (any Linux machine)

- Confirm the working tree is clean and `main` is what you want to release:
  `git status --short` should be empty.
- Confirm every crate's version matches the tag you are about to cut:
  ```sh
  grep -h '^version' crates/*/Cargo.toml | sort -u
  ```
  This must print exactly one line, `version = "X.Y.Z"`, matching the tag. rt's
  ten workspace crates are bumped in lockstep (see `ci/check-target-deps.sh`'s
  normaliser, which was hardened against exactly this drifting the dependency
  gate) — if this prints more than one version, something did not get bumped.
- Run the full gate: `cargo test --all && ./ci/check-target-deps.sh`. Both must
  be green.

## 2. Tag and push (any Linux machine)

```sh
git tag vX.Y.Z
git push forge vX.Y.Z          # or origin, per the standing "don't push GitHub
                                # unless asked" rule — confirm which remote first
```

rt has no packaging beyond source today for Linux: no `.deb`/`.rpm` (see
`docs/ROADMAP.md`'s "Packaging matrix" entry — deliberately deferred). A Linux
user builds from the tagged source, or installs the desktop entry with
`extra/linux/install.sh`. There is nothing to build and attach here beyond the
source archive the forge/GitHub already generates for a tag.

## 3. The macOS `.dmg` (must run on a Mac — `ssh kiku` or equivalent)

```sh
cd extra/macos
./dmg.sh
```

This builds `rt` in release mode, wraps it in `rt.app` (`bundle.sh`), then
wraps that in `target/macos/rt-X.Y.Z-macos-arm64.dmg` (this script). It is
**Apple Silicon only** and **unsigned** (ad-hoc signature only, no Developer
ID, no notarisation) — both are asserted at build time (`dmg.sh` refuses to
run on a non-arm64 host, and checks the bundled binary's architecture before
wrapping it), but the release notes still need to say so in prose. See
`RELEASE_NOTES_TEMPLATE.md` in this directory.

Verify before attaching it to the release — do not skip this, a `.dmg` is
exactly the kind of file where "the script ran with no error" and "the file
someone else can open" are not the same claim:

```sh
hdiutil verify target/macos/rt-X.Y.Z-macos-arm64.dmg
MOUNT=$(hdiutil attach target/macos/rt-X.Y.Z-macos-arm64.dmg -nobrowse -readonly | tail -1 | awk -F'\t' '{print $NF}')
ls "$MOUNT"                                   # expect: Applications  rt.app
"$MOUNT/rt.app/Contents/MacOS/rt" --version    # must print X.Y.Z
lipo -archs "$MOUNT/rt.app/Contents/MacOS/rt"  # must print: arm64
hdiutil detach "$MOUNT"
```

Copy `target/macos/rt-X.Y.Z-macos-arm64.dmg` off the Mac (it cannot be built
anywhere else) to wherever the release's other artifacts are being collected.

## 4. Publish the release (any machine)

- Attach the `.dmg` from step 3.
- Paste `extra/macos/RELEASE_NOTES_TEMPLATE.md`'s contents into the release
  description, with `<VERSION>` filled in.
- If the signing or architecture decision ever changes (a Developer ID is
  obtained, an Intel build is added), update the template's prose — `dmg.sh`
  enforces the *build-time* invariants but does not write its own release
  notes.

## 5. After

- Update `project-map.js`'s `project.updated` and any node/roadmap status the
  release actually changes, per the repo's standing order in `CLAUDE.md`.
- Deploy the new build to dop561/apollo/milkv/kiku as usual (see the
  `rt-deploy-all-machines` memory) — a tagged release does not replace that.
