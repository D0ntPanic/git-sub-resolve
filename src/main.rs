use anyhow::{anyhow, bail, Context, Result};
use git2::build::CheckoutBuilder;
use git2::{Index, IndexEntry, IndexTime, Oid, Repository, RepositoryState, Sort};
use std::collections::HashSet;
use std::env;
use std::path::Path;
use std::process::{Command, ExitCode};

const GITLINK_MODE: u32 = 0o160000;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("git sub-resolve: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn print_usage() {
    eprintln!("Usage: git sub-resolve <path-to-submodule>");
    eprintln!("       git sub-resolve --all");
    eprintln!();
    eprintln!("Resolves a submodule merge conflict by locating the already-merged");
    eprintln!("commit in the submodule (matched by commit message) and staging it");
    eprintln!("in the superproject index.");
    eprintln!();
    eprintln!("With --all, resolves every submodule conflict in the current index,");
    eprintln!("aborting immediately if any non-submodule conflict is present, then");
    eprintln!("continues the in-progress merge/rebase/cherry-pick. Repeats until the");
    eprintln!("operation finishes or a non-submodule conflict appears.");
}

fn run() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        print_usage();
        bail!("invalid arguments");
    }
    match args[1].as_str() {
        "-h" | "--help" => {
            print_usage();
            Ok(())
        }
        "--all" => run_all(),
        path => run_single(path),
    }
}

fn run_single(raw_path: &str) -> Result<()> {
    let submodule_path = raw_path.trim_end_matches('/');

    let super_repo = Repository::discover(".")
        .context("not inside a git repository")?;

    if super_repo.is_bare() {
        bail!("superproject is bare; sub-resolve requires a working tree");
    }

    let outcome = resolve_submodule(&super_repo, submodule_path)?;

    println!(
        "Resolved submodule '{submodule_path}':\n  \
         base    {}\n  \
         ours    {}\n  \
         theirs  {}\n  \
         staged  {}",
        outcome.ancestor_oid, outcome.ours_oid, outcome.theirs_oid, outcome.staged
    );
    if !outcome.containing_refs.is_empty() {
        println!("Candidate is reachable from:");
        for name in &outcome.containing_refs {
            println!("  {name}");
        }
    }
    if outcome.checkout_moved {
        println!(
            "Submodule working tree checked out at {} (detached HEAD).",
            outcome.staged
        );
    } else {
        println!(
            "Submodule working tree already at {}; left HEAD as-is.",
            outcome.staged
        );
    }
    println!();
    println!("Review with: git diff --cached -- {submodule_path}");
    println!("This tool did not continue the merge/rebase; do so yourself when ready.");

    Ok(())
}

fn run_all() -> Result<()> {
    let super_repo = Repository::discover(".").context("not inside a git repository")?;
    if super_repo.is_bare() {
        bail!("superproject is bare; sub-resolve requires a working tree");
    }
    if super_repo.state() == RepositoryState::Clean {
        bail!("no merge/rebase/cherry-pick in progress");
    }

    let mut touched: HashSet<String> = HashSet::new();
    let mut iteration = 0;
    loop {
        iteration += 1;

        let mut index = super_repo
            .index()
            .context("could not read superproject index")?;
        index
            .read(true)
            .context("could not reload superproject index from disk")?;

        if index.has_conflicts() {
            // Verify every conflict is a submodule gitlink before touching anything.
            let paths = collect_submodule_conflict_paths(&index)?;
            drop(index);

            println!(
                "[{iteration}] resolving {} submodule conflict(s)...",
                paths.len()
            );
            for path in &paths {
                let outcome = resolve_submodule(&super_repo, path)?;
                println!(
                    "  {path}: staged {} (theirs {})",
                    short_oid(outcome.staged),
                    short_oid(outcome.theirs_oid),
                );
                touched.insert(path.clone());
            }

            let mut fresh = super_repo
                .index()
                .context("could not re-read superproject index")?;
            fresh
                .read(true)
                .context("could not reload superproject index from disk")?;
            if fresh.has_conflicts() {
                bail!(
                    "conflicts remain after resolving all reported submodules; \
                     aborting before continuing the merge"
                );
            }
        }

        let state = super_repo.state();
        if state == RepositoryState::Clean {
            return finalize_all(&super_repo, &touched);
        }

        let op = state_to_op(state)?;
        println!("[{iteration}] running `git {op} --continue`...");
        continue_operation(&super_repo, op)?;

        if super_repo.state() == RepositoryState::Clean {
            return finalize_all(&super_repo, &touched);
        }
    }
}

fn finalize_all(super_repo: &Repository, touched: &HashSet<String>) -> Result<()> {
    println!("Merge complete.");
    if !touched.is_empty() {
        reattach_submodule_heads(super_repo, touched);
    }
    Ok(())
}

/// For each submodule we touched, if HEAD is detached and *exactly one* local
/// branch has its tip at that commit, move HEAD onto that branch. This is a
/// cosmetic cleanup — `checkout_submodule` always detaches, but in practice
/// the resolved commit is usually the tip of the branch the user was working
/// on in the submodule, so re-attaching matches their mental model.
///
/// Silent on failure: any error here is non-fatal (the merge already
/// finished), so we only log successful re-attaches.
fn reattach_submodule_heads(super_repo: &Repository, paths: &HashSet<String>) {
    let mut sorted: Vec<&String> = paths.iter().collect();
    sorted.sort();
    for path in sorted {
        let sub_repo = match super_repo
            .find_submodule(path)
            .and_then(|sm| sm.open())
        {
            Ok(r) => r,
            Err(_) => continue,
        };

        let head = match sub_repo.head() {
            Ok(h) => h,
            Err(_) => continue,
        };
        if head.is_branch() {
            continue;
        }
        let head_oid = match head.peel_to_commit() {
            Ok(c) => c.id(),
            Err(_) => continue,
        };

        let branches = local_branches_at(&sub_repo, head_oid);
        let branch = match branches.as_slice() {
            [only] => only,
            _ => continue,
        };

        let full_ref = format!("refs/heads/{branch}");
        if sub_repo.set_head(&full_ref).is_ok() {
            println!("  {path}: attached HEAD to {branch}");
        }
    }
}

/// Local branch short names whose tip is exactly `target`.
fn local_branches_at(sub_repo: &Repository, target: Oid) -> Vec<String> {
    let mut names = Vec::new();
    let refs = match sub_repo.references() {
        Ok(r) => r,
        Err(_) => return names,
    };
    for r in refs.flatten() {
        let Some(name) = r.name() else { continue };
        let Some(short) = name.strip_prefix("refs/heads/") else {
            continue;
        };
        if let Ok(commit) = r.peel_to_commit() {
            if commit.id() == target {
                names.push(short.to_string());
            }
        }
    }
    names.sort();
    names
}

struct ResolutionOutcome {
    ancestor_oid: Oid,
    ours_oid: Oid,
    theirs_oid: Oid,
    staged: Oid,
    containing_refs: Vec<String>,
    checkout_moved: bool,
}

fn resolve_submodule(super_repo: &Repository, submodule_path: &str) -> Result<ResolutionOutcome> {
    let (ancestor_oid, ours_oid, theirs_oid) =
        read_conflict_stages(super_repo, submodule_path)?;

    let sub_repo = super_repo
        .find_submodule(submodule_path)
        .with_context(|| format!("no submodule registered at '{submodule_path}'"))?
        .open()
        .with_context(|| format!("could not open submodule repository at '{submodule_path}'"))?;

    for (label, oid) in [("base", ancestor_oid), ("ours", ours_oid), ("theirs", theirs_oid)] {
        if sub_repo.find_commit(oid).is_err() {
            bail!(
                "submodule '{submodule_path}' is missing the {label} commit {oid}; \
                 fetch inside the submodule and retry"
            );
        }
    }

    // Sanity check: ours and theirs must share history in the submodule DAG.
    sub_repo
        .merge_base(ours_oid, theirs_oid)
        .with_context(|| {
            format!(
                "no common ancestor between {ours_oid} and {theirs_oid} in submodule \
                 '{submodule_path}'"
            )
        })?;

    let (candidate, containing_refs) =
        find_matching_commit(&sub_repo, ancestor_oid, ours_oid, theirs_oid)?;

    let checkout_moved = checkout_submodule(&sub_repo, candidate)?;
    stage_submodule(super_repo, submodule_path, candidate)?;

    Ok(ResolutionOutcome {
        ancestor_oid,
        ours_oid,
        theirs_oid,
        staged: candidate,
        containing_refs,
        checkout_moved,
    })
}

fn short_oid(oid: Oid) -> String {
    let s = oid.to_string();
    s[..s.len().min(12)].to_string()
}

/// Returns the unique paths of conflicted index entries, bailing as soon as we
/// see one whose mode isn't a gitlink. This is what lets `--all` refuse to touch
/// anything unless every conflict is a submodule pointer.
fn collect_submodule_conflict_paths(index: &Index) -> Result<Vec<String>> {
    let mut paths: Vec<String> = Vec::new();
    let mut seen: HashSet<Vec<u8>> = HashSet::new();

    let conflicts = index
        .conflicts()
        .context("could not iterate index conflicts")?;
    for conflict in conflicts {
        let conflict = conflict?;

        let mut path_bytes: Option<Vec<u8>> = None;
        for entry in [&conflict.ancestor, &conflict.our, &conflict.their]
            .iter()
            .filter_map(|e| e.as_ref())
        {
            if entry.mode != GITLINK_MODE {
                let p = String::from_utf8_lossy(&entry.path);
                bail!(
                    "conflict at '{p}' is not a submodule (index mode {:o}); \
                     --all only resolves submodule (gitlink) conflicts",
                    entry.mode
                );
            }
            if path_bytes.is_none() {
                path_bytes = Some(entry.path.clone());
            }
        }

        if let Some(pb) = path_bytes {
            if seen.insert(pb.clone()) {
                let p = std::str::from_utf8(&pb)
                    .context("conflict path is not valid UTF-8")?
                    .to_string();
                paths.push(p);
            }
        }
    }

    Ok(paths)
}

fn state_to_op(state: RepositoryState) -> Result<&'static str> {
    Ok(match state {
        RepositoryState::Merge => "merge",
        RepositoryState::CherryPick | RepositoryState::CherryPickSequence => "cherry-pick",
        RepositoryState::Rebase
        | RepositoryState::RebaseInteractive
        | RepositoryState::RebaseMerge => "rebase",
        RepositoryState::Revert | RepositoryState::RevertSequence => "revert",
        RepositoryState::ApplyMailbox | RepositoryState::ApplyMailboxOrRebase => "am",
        RepositoryState::Clean => bail!("repository is clean; no operation to continue"),
        RepositoryState::Bisect => {
            bail!("repository is in bisect; sub-resolve --all cannot continue a bisect")
        }
    })
}

/// Invokes `git <op> --continue`. A non-zero exit is tolerated *only* when the
/// next step in a sequence (rebase/cherry-pick/revert/am) hit a new conflict;
/// the caller's loop picks that up on the next pass. Any other failure aborts.
fn continue_operation(super_repo: &Repository, op: &str) -> Result<()> {
    let workdir = super_repo
        .workdir()
        .ok_or_else(|| anyhow!("repository has no working directory"))?;

    let status = Command::new("git")
        .args([op, "--continue"])
        .current_dir(workdir)
        .env("GIT_EDITOR", "true")
        .status()
        .with_context(|| format!("failed to invoke `git {op} --continue`"))?;

    if !status.success() {
        let mut index = super_repo
            .index()
            .context("could not read index after continue")?;
        index
            .read(true)
            .context("could not reload index after continue")?;
        if !index.has_conflicts() {
            bail!(
                "`git {op} --continue` exited with {status} and no conflicts were \
                 introduced; rerun the command manually to see the underlying error"
            );
        }
    }

    Ok(())
}

fn read_conflict_stages(
    repo: &Repository,
    submodule_path: &str,
) -> Result<(Oid, Oid, Oid)> {
    let index = repo.index().context("could not read superproject index")?;

    if !index.has_conflicts() {
        bail!("superproject index has no conflicts");
    }

    let path_bytes = submodule_path.as_bytes();
    let mut ancestor = None;
    let mut ours = None;
    let mut theirs = None;

    let conflicts = index.conflicts().context("could not iterate index conflicts")?;
    for conflict in conflicts {
        let conflict = conflict?;
        let matches = [&conflict.ancestor, &conflict.our, &conflict.their]
            .iter()
            .any(|e| e.as_ref().map(|e| e.path.as_slice()) == Some(path_bytes));
        if !matches {
            continue;
        }
        for entry in [&conflict.ancestor, &conflict.our, &conflict.their]
            .iter()
            .filter_map(|e| e.as_ref())
        {
            if entry.path != path_bytes {
                continue;
            }
            if entry.mode != GITLINK_MODE {
                bail!(
                    "index entry for '{submodule_path}' is not a gitlink (mode {:o}); \
                     is this actually a submodule?",
                    entry.mode
                );
            }
            let stage = (entry.flags as u32 >> 12) & 0x3;
            match stage {
                1 => ancestor = Some(entry.id),
                2 => ours = Some(entry.id),
                3 => theirs = Some(entry.id),
                _ => {}
            }
        }
    }

    let ancestor =
        ancestor.ok_or_else(|| anyhow!("no common-ancestor stage for '{submodule_path}' in index"))?;
    let ours =
        ours.ok_or_else(|| anyhow!("no 'ours' stage for '{submodule_path}' in index"))?;
    let theirs =
        theirs.ok_or_else(|| anyhow!("no 'theirs' stage for '{submodule_path}' in index"))?;

    Ok((ancestor, ours, theirs))
}

/// Locates the reconciled submodule commit by fingerprinting the *incoming*
/// submodule diff — i.e. what the commit being applied intends to change.
///
/// Workflow context: the user rebases the submodule first so its branch
/// contains main's work plus the feature work replayed on top, then rebases
/// the superproject. When a gitlink conflict surfaces, the *commit being
/// applied* (stage 3, "theirs") specifies an old→new submodule ref in its
/// diff; that old is stage 1 ("ancestor"). The commits in `ancestor..theirs`
/// are the work this particular superproject commit intends to bring into
/// the submodule.
///
/// Strategy:
/// 1. Compute `target` = commit messages reachable from `theirs_oid` but not
///    from `ancestor_oid` in the submodule DAG. This is the set of submodule
///    commits the superproject commit being applied wants to add.
/// 2. Find a submodule commit X such that the commits reachable from X but
///    not from `ours_oid` carry that same multiset of messages. That is: X
///    is the current state (`ours_oid`) with the incoming commits replayed
///    on top — which is exactly what the pre-rebased submodule branch tip
///    (or an interior commit in that chain) should be.
///
/// Using the full message list as a fingerprint is dramatically less
/// ambiguous than matching on a single theirs message — two unrelated
/// branches rarely share an entire sequence of commit messages, so this
/// survives large repos with many backports and overlapping work.
///
/// Edge case: if `theirs_oid == ancestor_oid` (the incoming commit didn't
/// change the submodule ref), target is empty and the answer is `ours_oid`.
/// A genuine gitlink conflict shouldn't produce this state, but we handle
/// it gracefully.
///
/// We deliberately avoid trusting `.gitmodules` `branch`, which on feature
/// branches is often wrong: both the superproject and submodule may be on
/// alternate branches that `.gitmodules` doesn't name.
fn find_matching_commit(
    sub_repo: &Repository,
    ancestor_oid: Oid,
    ours_oid: Oid,
    theirs_oid: Oid,
) -> Result<(Oid, Vec<String>)> {
    let target = messages_between(sub_repo, ancestor_oid, theirs_oid).with_context(|| {
        format!(
            "could not enumerate commits between base {ancestor_oid} and theirs \
             {theirs_oid} in submodule"
        )
    })?;
    let mut target_sorted = target.clone();
    target_sorted.sort();

    if target_sorted.is_empty() {
        // Incoming commit didn't change the submodule ref past the base; the
        // resolved ref should just be ours.
        let containing = refs_containing(sub_repo, ours_oid);
        return Ok((ours_oid, containing));
    }

    let target_set: HashSet<Vec<u8>> = target.iter().cloned().collect();

    let seeds = collect_ref_tips(sub_repo)?;
    if seeds.is_empty() {
        bail!("submodule has no refs to search; create or fetch the branch with the merged commit");
    }

    // Walk every commit reachable from a ref tip, hiding `ours_oid` and its
    // ancestors. Cheap pre-filter: skip any commit whose own message isn't
    // in the target set — a valid X's tip-most commit (in theirs..X) must be
    // theirs's tip message, which is always in the target set.
    let mut revwalk = sub_repo.revwalk().context("could not create revwalk")?;
    revwalk.set_sorting(Sort::TOPOLOGICAL)?;
    for (oid, name) in &seeds {
        revwalk
            .push(*oid)
            .with_context(|| format!("could not push {name} ({oid}) into revwalk"))?;
    }
    revwalk
        .hide(ours_oid)
        .with_context(|| format!("could not hide 'ours' {ours_oid} from revwalk"))?;

    let mut matches: Vec<Oid> = Vec::new();
    for oid_result in revwalk {
        let oid = oid_result?;
        let commit = sub_repo.find_commit(oid)?;
        let msg = commit.message_raw_bytes().to_vec();
        if !target_set.contains(&msg) {
            continue;
        }
        if !sub_repo.graph_descendant_of(oid, ours_oid).unwrap_or(false) {
            continue;
        }

        let cand = messages_between(sub_repo, ours_oid, oid)?;
        let mut cand_sorted = cand;
        cand_sorted.sort();
        if cand_sorted == target_sorted {
            matches.push(oid);
        }
    }

    matches.sort();
    matches.dedup();

    let found = match matches.as_slice() {
        [] => {
            bail!(
                "no submodule commit descends from ours {ours_oid} whose history since \
                 ours matches the {} commit message(s) the incoming commit introduces \
                 (base {ancestor_oid}..theirs {theirs_oid}); has the submodule been \
                 rebased to include the incoming commits on top of ours?",
                target_sorted.len()
            );
        }
        [only] => *only,
        many => {
            let list = many
                .iter()
                .map(|o| o.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "ambiguous match: multiple submodule commits descend from ours \
                 {ours_oid} and carry the same commit-message list as the incoming \
                 diff ({list}); resolve manually"
            );
        }
    };

    let containing = refs_containing(sub_repo, found);
    Ok((found, containing))
}

/// Commit-message bytes for every commit reachable from `include` but not from
/// `exclude`, in revwalk order. We use raw bytes (not str) so commits with
/// non-UTF-8 messages still compare correctly.
fn messages_between(repo: &Repository, exclude: Oid, include: Oid) -> Result<Vec<Vec<u8>>> {
    let mut walk = repo.revwalk().context("could not create revwalk")?;
    walk.push(include)
        .with_context(|| format!("could not push {include} into revwalk"))?;
    walk.hide(exclude)
        .with_context(|| format!("could not hide {exclude} from revwalk"))?;
    let mut msgs = Vec::new();
    for oid in walk {
        let oid = oid?;
        let commit = repo.find_commit(oid)?;
        msgs.push(commit.message_raw_bytes().to_vec());
    }
    Ok(msgs)
}

/// Returns (oid, ref-name) for every local branch, remote-tracking branch, and
/// the current HEAD (if it resolves to a commit). Tags are skipped — they
/// typically mark historical releases and rarely point at in-flight merge work.
fn collect_ref_tips(sub_repo: &Repository) -> Result<Vec<(Oid, String)>> {
    let mut seeds: Vec<(Oid, String)> = Vec::new();
    let mut seen: std::collections::HashSet<Oid> = std::collections::HashSet::new();

    let refs = sub_repo
        .references()
        .context("could not enumerate submodule refs")?;
    for r in refs {
        let r = match r {
            Ok(r) => r,
            Err(_) => continue,
        };
        let name = match r.name() {
            Some(n) => n.to_string(),
            None => continue,
        };
        if !(name.starts_with("refs/heads/") || name.starts_with("refs/remotes/")) {
            continue;
        }
        if let Ok(commit) = r.peel_to_commit() {
            if seen.insert(commit.id()) {
                seeds.push((commit.id(), name));
            }
        }
    }

    if let Ok(head) = sub_repo.head() {
        if let Ok(commit) = head.peel_to_commit() {
            if seen.insert(commit.id()) {
                seeds.push((commit.id(), "HEAD".to_string()));
            }
        }
    }

    Ok(seeds)
}

/// Names of refs whose tip is at or descends from `target`, for user-facing
/// feedback about where the resolved commit lives.
fn refs_containing(sub_repo: &Repository, target: Oid) -> Vec<String> {
    let mut names = Vec::new();
    let refs = match sub_repo.references() {
        Ok(r) => r,
        Err(_) => return names,
    };
    for r in refs.flatten() {
        let name = match r.name() {
            Some(n) => n.to_string(),
            None => continue,
        };
        if !(name.starts_with("refs/heads/") || name.starts_with("refs/remotes/")) {
            continue;
        }
        if let Ok(commit) = r.peel_to_commit() {
            let tip = commit.id();
            if tip == target
                || sub_repo
                    .graph_descendant_of(tip, target)
                    .unwrap_or(false)
            {
                names.push(name);
            }
        }
    }
    names.sort();
    names
}

fn checkout_submodule(sub_repo: &Repository, target: Oid) -> Result<bool> {
    let current = sub_repo
        .head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .map(|c| c.id());
    if current == Some(target) {
        return Ok(false);
    }

    let target_commit = sub_repo
        .find_commit(target)
        .with_context(|| format!("could not load target commit {target} in submodule"))?;
    let target_tree = target_commit
        .tree()
        .with_context(|| format!("could not read tree for target commit {target}"))?;

    let mut opts = CheckoutBuilder::new();
    opts.safe();
    sub_repo
        .checkout_tree(target_tree.as_object(), Some(&mut opts))
        .with_context(|| {
            format!(
                "could not update submodule working tree to {target}; \
                 commit or stash uncommitted changes in the submodule and retry"
            )
        })?;

    sub_repo
        .set_head_detached(target)
        .with_context(|| format!("could not move submodule HEAD to {target}"))?;

    Ok(true)
}

fn stage_submodule(
    super_repo: &Repository,
    submodule_path: &str,
    target: Oid,
) -> Result<()> {
    let mut index = super_repo.index().context("could not open superproject index")?;

    index
        .remove_path(Path::new(submodule_path))
        .with_context(|| format!("could not clear conflict entries for '{submodule_path}'"))?;

    let entry = IndexEntry {
        ctime: IndexTime::new(0, 0),
        mtime: IndexTime::new(0, 0),
        dev: 0,
        ino: 0,
        mode: GITLINK_MODE,
        uid: 0,
        gid: 0,
        file_size: 0,
        id: target,
        flags: 0,
        flags_extended: 0,
        path: submodule_path.as_bytes().to_vec(),
    };
    index
        .add(&entry)
        .with_context(|| format!("could not stage submodule '{submodule_path}' at {target}"))?;

    index.write().context("could not write superproject index")?;
    Ok(())
}
