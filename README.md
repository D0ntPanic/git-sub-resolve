# git-sub-resolve

***WARNING: This tool was entirely vibe coded with Opus 4.7. This repo is provided in case you find it useful.***

A git extension for resolving submodule pointer conflicts during a merge or rebase,
in the common case where the submodule side has already been merged and the
superproject just needs its gitlink pointed at the right commit.

When you rebase or merge at the superproject and a submodule's pointer differs
between the two sides, git leaves the gitlink in a conflicted state with three
index stages: `base` (stage 1), `ours` (stage 2), `theirs` (stage 3). Git will
not look inside the submodule to resolve this for you. If you've already
reconciled the submodule's own branch (by cherry-picking, rebasing, or merging
`theirs` into `ours` inside the submodule), the fix at the superproject is
mechanical: find the commit in the submodule that represents that
reconciliation, and stage it. This tool automates that step.

## How it works

Given a submodule path that has a gitlink conflict, the tool:

1. Reads the three conflict stages from the superproject index.
2. Reads the commit message of the `theirs` commit — this is the message the
   reconciliation commit will share, on the assumption that the submodule side
   was reconciled by cherry-pick / rebase / equivalent (which preserves the
   original commit message).
3. Walks every `refs/heads/*` and `refs/remotes/*` in the submodule, plus HEAD,
   hiding anything reachable from `ours` or `theirs`. What remains is exactly
   the commits introduced to reconcile the two sides.
4. Filters the matches to those whose ancestry contains `ours` — a genuine
   reconciliation is `theirs` applied on top of `ours`, so `ours` must be an
   ancestor. Copies on unrelated branches (backports, stale rebases) are
   discarded.
5. If exactly one commit remains, the tool:
   - Checks out the submodule's working tree at that commit (safe checkout;
     aborts if the working tree has uncommitted changes), leaving HEAD
     detached, so the UI shows the submodule clean.
   - Stages the gitlink at that commit in the superproject index, clearing all
     three conflict stages.

If zero or multiple commits survive the filter, the tool prints a diagnostic
and exits without touching either the submodule working tree or the
superproject index.

## What it won't do

- By default it does not continue the rebase or merge. After the gitlink is
  staged you can review with `git diff --cached -- <path>` and then continue on
  your own. (See `--all` below for opt-in auto-continuation.)
- It does not handle reconciliations done via a real merge commit in the
  submodule (where the merge commit has a `Merge branch...` message rather
  than the incoming commit's message). Only cherry-pick / rebase style
  reconciliations are detected.
- It does not read `.gitmodules` `branch`. That field is author-intent and is
  frequently wrong on feature branches where both the superproject and
  submodule are on alternate branches.

## Install

Requires a stable Rust toolchain.

```
cargo install --path .
```

This produces a `git-sub-resolve` binary. As long as it's on your `PATH`, git
will expose it as the `git sub-resolve` subcommand.

## Usage

Run from the superproject working tree while a submodule gitlink conflict is
present:

```
git sub-resolve <path-to-submodule>
```

Example output:

```
Resolved submodule 'api':
  base    436c5c68287d98ec794c8043c8bafdb2b19bb292
  ours    99ffe50da687e28b1eed1ce16bd01054099de554
  theirs  2ece495f9ce7a65c4694aa2781575cd8c31dfe2b
  staged  0090c43ce96266dcbb702021c6c3ac9bb13216e4
Candidate is reachable from:
  refs/heads/test_call_layout
Submodule working tree checked out at 0090c43ce96266dcbb702021c6c3ac9bb13216e4 (detached HEAD).

Review with: git diff --cached -- api
This tool did not continue the merge/rebase; do so yourself when ready.
```

### Batch mode: `--all`

```
git sub-resolve --all
```

Resolves *every* submodule gitlink conflict in the current index and then
continues the in-progress merge, rebase, cherry-pick, revert, or `git am`. If a
new conflict surfaces on the next commit in the sequence, the process repeats
until the operation finishes.

Preconditions and guarantees:

- Before touching anything, `--all` scans the index and errors out if *any*
  conflict is not a submodule gitlink. Regular file conflicts must be resolved
  by hand first — `--all` will not proceed past them.
- After resolving all reported submodule conflicts, the index is re-read and
  must be conflict-free before the tool continues the operation.
- The underlying per-submodule resolution is the same logic described above, so
  ambiguous or unfetched submodule cases still abort cleanly without staging
  anything.
- The continuation step shells out to `git <op> --continue` with
  `GIT_EDITOR=true` so no editor is opened for the commit message. The merge
  commit message is whatever git would have produced non-interactively.

Example output for a rebase with two conflicting commits, each of which touches
the same submodule:

```
[1] resolving 1 submodule conflict(s)...
  api: staged 0090c43ce962 (theirs 2ece495f9ce7)
[1] running `git rebase --continue`...
[Detached HEAD a1b2c3d] ...
[2] resolving 1 submodule conflict(s)...
  api: staged def6789abcde (theirs aabbccdd1122)
[2] running `git rebase --continue`...
Merge complete.
```

If a non-submodule conflict is present, `--all` aborts immediately:

```
git sub-resolve: conflict at 'src/foo.rs' is not a submodule (index mode 100644);
--all only resolves submodule (gitlink) conflicts
```

## Failure modes

The tool exits non-zero without mutating anything when it can't identify a
unique resolution. Common diagnostics:

- `superproject index has no conflicts` — nothing to resolve.
- `no submodule registered at '<path>'` — typo or wrong path.
- `submodule '<path>' is missing the <base|ours|theirs> commit` — fetch inside
  the submodule and retry.
- `no commit introduced between 'ours' and any submodule ref carries the
  incoming commit message` — the submodule side hasn't been reconciled yet;
  merge/cherry-pick `theirs` into the appropriate branch first.
- `found N commit(s) ... but none descend from 'ours'` — matches exist on
  unrelated branches only (backports, stale rebases); no valid resolution.
- `ambiguous match: multiple submodule commits descend from 'ours'` — more
  than one candidate; resolve manually.
- `could not update submodule working tree ... 1 conflict prevents checkout`
  — submodule has uncommitted changes; commit or stash them and retry. The
  superproject index is left untouched in this case.

Additional diagnostics specific to `--all`:

- `no merge/rebase/cherry-pick in progress` — there's nothing for `--all` to
  continue.
- `conflict at '<path>' is not a submodule (index mode <mode>)` — a
  non-submodule file is conflicted; resolve it manually, then rerun.
- `conflicts remain after resolving all reported submodules` — the tool
  resolved every submodule it saw but the index still reports conflicts. This
  shouldn't happen in practice; rerun manually to investigate.
- ``git <op> --continue` exited with <status> and no conflicts were introduced``
  — the continuation step failed for some reason other than a new conflict
  (e.g. empty commit, sequencer refusal). Rerun the command by hand to see the
  underlying error.

## Safety

- The submodule working-tree checkout happens *before* the superproject index
  is written, so a failed checkout never leaves a half-resolved superproject
  behind.
- The submodule checkout is "safe" mode — libgit2 refuses to overwrite
  modified files.
- A review step (`git diff --cached -- <path>`) is always possible because the
  tool stops after staging; nothing is committed automatically. Note that
  `--all` opts *out* of this — it commits the merge/rebase step once conflicts
  are resolved.
