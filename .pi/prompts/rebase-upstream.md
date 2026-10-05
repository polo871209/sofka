---
description: Rebase polo onto main (newest upstream release), force-push it, and replace the rolling release
argument-hint: "[extra instructions]"
---

Rebase the `polo` fork branch onto `main`, publish it, and replace the rolling release. Running this command authorizes two destructive actions: the force-push of `polo` and the deletion of the fork's old releases and tags. It authorizes nothing else. Never push `main`, and never commit outside the rebase.

Extra instructions: ${@:-none}

## Fork intent wins conflicts

`git log --reverse main..polo` lists the fork commits, and their messages state the fork's intent. If a conflict pits the fork against upstream, keep the fork's behavior. Adopt every upstream change that does not contradict that intent: bug fixes, features, config fields, version pin bumps. Port an upstream feature into the fork's shape instead of dropping it. For example, a new upstream hint goes into the fork's flat hint grid.

Standing fork decisions that upstream changes keep hitting:

- No footer, no scrollbars, no `A` sort key. Drop upstream code and tests for them. If a dropped test also covers behavior that still exists, rewrite it to check that behavior another way, for example through screen text instead of a scrollbar thumb.
- The status sort puts failing rows first, so a failing row sits on row 0, where the selection highlight hides its color. In an upstream color test, call `app.table_state.select(None)` before reading colors.
- `AGENTS.md`, `README.md`, and `.github/workflows/release.yaml` are the fork's own. Keep them, but take upstream's version pin bumps. Upstream packaging, Nix files, issue templates, and `release-packages.yaml` stay deleted.
- JSON lookups use `.at()` from `src/json.rs`, not `.pointer()`.

Resolution traps seen before:

- `git checkout --ours` or `--theirs` replaces the whole file and drops the other side's auto-merged hunks. After using it, compare the file with the other side's change (`git show <commit> -- <file>`) and re-apply what got lost.
- A Markdown table conflict is usually a column re-flow. Take one side, re-apply the other side's row changes found by a whitespace-normalized diff, then run `oxfmt <file>`.
- Resolve conflict blocks by their content, not by their index in the file. Index-based scripts have resolved the wrong block.

## Steps

1. Make sure that the tree is clean and `polo` is checked out. Run `git fetch origin --prune`. Find the newest upstream release with `git ls-remote --tags --refs https://github.com/nklmilojevic/sofka 'v*'`. If `origin/main` is behind that tag, run `gh workflow run sync-upstream.yaml --repo polo871209/sofka`, wait for the run to finish, and fetch again. Fast-forward local `main` to `origin/main`. Done when `main` equals `origin/main` at the newest `vX.Y.Z` upstream tag.
2. If `git merge-base --is-ancestor main polo` succeeds, stop and report that `polo` is already current. Do not push and do not delete tags.
3. Run `git rebase main`. At each stopped commit, resolve every conflict by the rules above, run `cargo fmt --all`, then run `just check`. If only `stalled_credential_helpers_are_quiet_and_bounded` fails, rerun `cargo test --locked --test completion`, because that test is timing-sensitive. Continue with `git add` and `GIT_EDITOR=true git rebase --continue`. Done with a commit when `just check` passes at it. Done with the step when the rebase reports success.
4. Run `just check` on the tip. Done when it passes.
5. List the old tags with `git ls-remote --tags origin`. Delete each release with `gh release delete <tag> --repo polo871209/sofka --cleanup-tag --yes`. Delete any tag still on origin with `git push origin --delete <tag>`, and delete the local tags with `git tag -d <tag>`. `gh release delete` can print HTTP 404 after it deleted the release, and `gh release list` can show stale entries, so make sure with `gh api repos/polo871209/sofka/releases --jq length` and `git ls-remote --tags origin`. Done when both show nothing.
6. Run `git push --force-with-lease origin polo`.
7. Find the release run with `gh run list --repo polo871209/sofka --workflow release.yaml --limit 1` and wait with `gh run watch <id> --repo polo871209/sofka --exit-status`. Done when the run succeeds and the releases API lists exactly one release, `v<Cargo.toml version>-polo.<run number>`, with its `.tar.gz` and `.sha256` assets.
8. Report the old and new `polo` commits, each conflict with the side that won and why, every test you changed or deleted, and the new release tag.
