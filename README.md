# git-sub-resolve

***WARNING: This tool was entirely vibe coded with Opus 4.7. This repo is provided in case you find it useful.***

A git extension for resolving submodule pointer conflicts during a merge or rebase,
in the common case where the submodule side has already been merged and the
superproject just needs its gitlink pointed at the right commit.

When you rebase or merge at the superproject and a submodule's pointer differs
between the two sides, git leaves the gitlink in a conflicted state with three
index stages: `ancestor` (stage 1), `ours` (stage 2), `theirs` (stage 3). Git
will not look inside the submodule to resolve this for you. If you've already
reconciled the submodule's own branch (by rebasing the incoming commits on top
of the current ours state), the fix at the superproject is mechanical: find
the commit in the submodule that represents that reconciliation, and stage it.
This tool automates that step.

## How it works

The assumed workflow: you first rebase the submodule so its branch contains
upstream's work with your feature work replayed on top, then you rebase (or
merge) the superproject. The submodule branch's tip — or some interior commit
in that replayed chain — is the commit the superproject's gitlink should end
up pointing at.

Given a submodule path that has a gitlink conflict, the tool:

1. Reads the three conflict stages from the superproject index: `ancestor`
   (stage 1), `ours` (stage 2), `theirs` (stage 3).
2. Computes the **incoming fingerprint**: the multiset of commit messages
   reachable from `theirs` but not from `ancestor` in the submodule DAG. This
   is exactly the submodule change described by the diff of the superproject
   commit being applied — the set of submodule commits that commit wants to
   bring in.
3. Walks every `refs/heads/*` and `refs/remotes/*` in the submodule, plus
   HEAD, hiding `ours` and its ancestors. Each visited commit is a candidate
   X that lives on top of `ours`.
4. For each candidate, compares the multiset of commit messages in `ours..X`
   against the incoming fingerprint. A match means X is `ours` with exactly
   the incoming commits replayed on top — which is the submodule state you
   produced by rebasing the submodule first.
5. Cheap pre-filter: candidates whose own commit message isn't in the
   fingerprint are skipped without the full revwalk.
6. If exactly one commit matches, the tool:
   - Checks out the submodule's working tree at that commit (safe checkout;
     aborts if the working tree has uncommitted changes), leaving HEAD
     detached, so the UI shows the submodule clean.
   - Stages the gitlink at that commit in the superproject index, clearing
     all three conflict stages.

The full message list is a much stronger fingerprint than a single commit
message, so this survives large repos with many backports and overlapping
feature branches — two unrelated branches rarely share an entire sequence of
commit messages.

If zero or multiple commits match, the tool prints a diagnostic and exits
without touching either the submodule working tree or the superproject index.

Edge case: if `theirs == ancestor` (the incoming commit doesn't change the
submodule ref), the fingerprint is empty and the tool stages `ours`. Git
shouldn't produce this as a conflict in practice, but it's handled cleanly.

## What it won't do

- By default it does not continue the rebase or merge. After the gitlink is
  staged you can review with `git diff --cached -- <path>` and then continue on
  your own. (See `--all` below for opt-in auto-continuation.)
- It does not handle reconciliations that rewrite commit messages (e.g. a
  true submodule merge commit whose message is `Merge branch...` rather than
  one of the incoming commits, or a squash that collapses the chain). The
  fingerprint relies on messages being preserved through the replay, which
  is what cherry-pick / rebase do by default.
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
- On successful completion, each touched submodule is checked: if its
  detached HEAD points exactly at the tip of a single local branch, HEAD is
  re-attached to that branch as a cosmetic cleanup. Submodules with multiple
  branches at the same commit, or none, are left detached.

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
- `no submodule commit descends from ours ... whose history since ours matches
  the N commit message(s) the incoming commit introduces` — the submodule
  hasn't been rebased to put the incoming commits on top of ours yet; rebase
  the submodule first and retry.
- `ambiguous match: multiple submodule commits descend from ours ... and carry
  the same commit-message list as the incoming diff` — more than one candidate
  has an identical message sequence; resolve manually.
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
