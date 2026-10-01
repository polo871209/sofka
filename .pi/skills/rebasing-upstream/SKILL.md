---
name: rebasing-upstream
description: Rebase polo onto main (newest upstream release), force-push it, and replace the rolling release.
argument-hint: "[extra instructions]"
disable-model-invocation: true
---

Rebase the `polo` fork branch onto `main`, publish it, and replace the rolling release. Running this skill authorizes three destructive actions: folding follow-up commits into their feature commits, the force-push of `polo`, and the deletion of the fork's old releases and tags. It authorizes nothing else. Never push `main`, and never commit outside the fold and the rebase.

Text after the command is extra instructions from the user. They win over this file.

## One commit per feature

`polo` holds one commit per fork feature on top of an upstream release, and nothing else: no fix, review, or follow-up commits. Each message is the single source of its feature's intent, one bullet per behavior. `git log --reverse main..polo` lists them. A follow-up commit changes a feature that an earlier commit introduced, and step 3 folds it into that commit. A commit that adds a new feature stays a separate commit.

## Fork intent wins conflicts

If a conflict pits the fork against upstream, keep the fork's behavior. Adopt every upstream change that does not contradict that intent: bug fixes, features, config fields, version pin bumps. Port an upstream feature into the fork's shape instead of dropping it.

Standing decisions for the layout:

- No footer, no scrollbars, no live indicator (`● refresh`, `○ stopped`, `○ snapshot`), and no `A` sort key. Drop upstream code and tests for them, including `app.scrollbar_activity` lines and `navigation_hint`. If a dropped test also covers behavior that still exists, rewrite it to check screen text. In an upstream test that uses `Action::SortAge`, use another table-only action, for example `Action::InvertSort`.
- Header key hints are one flat `(Action, label)` list per kind in `header_hints` in `src/ui.rs`. Port each upstream `hint_line(app, &[...])` row into that list.

Standing decisions for table behavior:

- The status sort puts failing rows first, so a failing row sits on row 0, where the selection highlight hides its color. In an upstream color test, call `app.table_state.select(None)` before reading colors.
- A newly picked sort column sorts high to low. Flip upstream expectations that assume ascending order after `S`.
- The table shows no rows until the first list is complete, because `Store::begin_reset` always buffers into `pending`. `Count:` reads `loading…` meanwhile. An upstream test that expects rows before `Msg::Synced` (or before the ns-resources `Update::Done`) must check the empty table first and the rows after the sync.

Standing decisions for files:

- `AGENTS.md`, `README.md`, and `.github/workflows/release.yaml` are the fork's own. Keep them, but take upstream's version pin bumps. Upstream packaging, Nix files, issue templates, `package-repos.yaml`, `release-packages.yaml`, and `.github/actions/setup-nix` stay deleted (`git rm` on a modify/delete conflict).
- JSON lookups use `.at()` from `src/json.rs`, not `.pointer()`. `clippy.toml` rejects `.pointer()`, so port every upstream call.
- Port-forwards go through the Kubernetes API in `src/portforward.rs`, not `kubectl`. The test spawner takes four arguments: `|_, _, _, _|`.

## Resolution traps

- `git checkout --ours` or `--theirs` replaces the whole file and drops the other side's auto-merged hunks. After using it, compare the file with the other side's change (`git diff <base> main -- <file>` for upstream) and re-apply what got lost.
- A Markdown table conflict is usually a column re-flow. Take one side, re-apply the other side's row changes found by a whitespace-normalized diff, then run `oxfmt <file>`.
- Resolve conflict blocks by their content, never by their index. Use [`conflicts.py`](conflicts.py): `python3 conflicts.py show <file>` prints every block, and `resolve(path, fn)` replaces the blocks that `fn(ours, base, theirs)` recognizes. Assert on content inside `fn`, so an unexpected block stops the script. During the rebase, "ours" is upstream and "theirs" is the fork.
- `rerere` is on. If the rebase prints `Resolved '<file>' using previous resolution`, read the result anyway, because that resolution was recorded against an older upstream.

## Steps

1. Load the tools. Run `eval "$(mise env -s bash)"` in every shell that runs `cargo`, `just`, or `oxfmt`. If `cargo` is missing, run `mise install rust`, because the mise install can be a broken symlink. Run `export GH_TOKEN=$(env -u GITHUB_TOKEN gh auth token --user polo871209); unset GITHUB_TOKEN` in every shell that runs `gh`, because the `GITHUB_TOKEN` account has no push access and the API answers its writes with HTTP 404. Done when `cargo --version` works and `gh api repos/polo871209/sofka --jq .permissions.push` prints `true`.
2. Make sure that the tree is clean and `polo` is checked out. Run `git fetch origin --prune`. Find the newest upstream release with `git ls-remote --tags --refs https://github.com/nklmilojevic/sofka 'v*'`. If `origin/main` is behind that tag, run `gh workflow run sync-upstream.yaml --repo polo871209/sofka`, wait for the run to finish, and fetch again. Fast-forward local `main` to `origin/main`. Done when `main` equals `origin/main` at the newest `vX.Y.Z` upstream tag.
3. Fold follow-ups. Set `base=$(git merge-base main polo)` and read `git log --reverse --format='%h %s' $base..polo`. If a commit is a follow-up, write a todo file that lists every commit as `pick <hash>` in order, moves each follow-up directly under its feature as `fixup <hash>`, and adds `exec git commit --amend --no-verify -F <message file>` after the last `fixup` of a feature. The message file holds the feature's message with the follow-up's bullets merged in. Run `GIT_SEQUENCE_EDITOR="cp <todo file>" git rebase -i $base`. Done when every commit in `$base..polo` is a feature and `git diff <old polo> polo` prints nothing.
4. If `git merge-base --is-ancestor main polo` succeeds and step 3 changed nothing, stop and report that `polo` is already current. Do not push and do not delete tags. If only step 3 changed `polo`, skip to step 6.
5. Run `git rebase main`. At each stopped commit, resolve every conflict by the rules above, run `cargo fmt --all`, then run `just check`. A commit that applies without a stop can still fail `just check`. If the stop fails on a problem that the HEAD commit brought in, fix it in HEAD: add `pick <stopped commit's original hash>` to the top of `.git/rebase-merge/git-rebase-todo`, run `git reset --merge HEAD`, fix the code, run `git commit --amend --no-edit -a`, then run `git rebase --skip`. If only `stalled_credential_helpers_are_quiet_and_bounded` fails, rerun `cargo test --locked --test completion`, because that test is timing-sensitive. Continue with `git add -A` and `GIT_EDITOR=true git rebase --continue`. Done with a commit when `just check` passes at it. Done with the step when the rebase reports success.
6. Run `just check` on the tip. Done when it passes.
7. List the old tags with `git ls-remote --tags origin`. Delete each release with `gh release delete <tag> --repo polo871209/sofka --cleanup-tag --yes`. Delete any tag still on origin with `git push origin --delete <tag>`, and delete the local tags with `git tag -d <tag>`. `gh release list` can show stale entries, so make sure with `gh api repos/polo871209/sofka/releases --jq length` and `git ls-remote --tags origin`. Done when the first prints `0` and the second prints nothing.
8. Run `git push --force-with-lease origin polo`.
9. Find the release run with `gh run list --repo polo871209/sofka --workflow release.yaml --limit 1`. Wait with `gh run watch <id> --repo polo871209/sofka --exit-status --interval 30`, because the build took 8 minutes on run 17. Done when the run succeeds and the releases API lists exactly one release, `v<Cargo.toml version>-polo.<run number>`, with its `.tar.gz` and `.sha256` assets.
10. Report the old and new `polo` commits, each conflict with the side that won and why, every test you changed or deleted, and the new release tag.
