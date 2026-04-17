use anyhow::{anyhow, bail, Context, Result};
use git2::build::CheckoutBuilder;
use git2::{IndexEntry, IndexTime, Oid, Repository, Sort};
use std::env;
use std::path::Path;
use std::process::ExitCode;

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

fn run() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 || matches!(args[1].as_str(), "-h" | "--help") {
        eprintln!("Usage: git sub-resolve <path-to-submodule>");
        eprintln!();
        eprintln!("Resolves a submodule merge conflict by locating the already-merged");
        eprintln!("commit in the submodule (matched by commit message) and staging it");
        eprintln!("in the superproject index. Does not continue the rebase/merge.");
        if args.len() == 2 {
            return Ok(());
        }
        bail!("invalid arguments");
    }
    let raw_path = &args[1];
    let submodule_path = raw_path.trim_end_matches('/');

    let super_repo = Repository::discover(".")
        .context("not inside a git repository")?;

    if super_repo.is_bare() {
        bail!("superproject is bare; sub-resolve requires a working tree");
    }

    let (ancestor_oid, ours_oid, theirs_oid) =
        read_conflict_stages(&super_repo, submodule_path)?;

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

    let theirs_commit = sub_repo.find_commit(theirs_oid)?;
    let target_message = theirs_commit
        .message_raw()
        .ok_or_else(|| anyhow!("incoming commit {theirs_oid} has no UTF-8 commit message"))?
        .to_owned();

    let (candidate, containing_refs) = find_matching_commit(
        &sub_repo,
        ours_oid,
        theirs_oid,
        &target_message,
    )?;

    let checkout_moved = checkout_submodule(&sub_repo, candidate)?;
    stage_submodule(&super_repo, submodule_path, candidate)?;

    println!(
        "Resolved submodule '{submodule_path}':\n  \
         base    {ancestor_oid}\n  \
         ours    {ours_oid}\n  \
         theirs  {theirs_oid}\n  \
         staged  {candidate}"
    );
    if !containing_refs.is_empty() {
        println!("Candidate is reachable from:");
        for name in &containing_refs {
            println!("  {name}");
        }
    }
    if checkout_moved {
        println!("Submodule working tree checked out at {candidate} (detached HEAD).");
    } else {
        println!("Submodule working tree already at {candidate}; left HEAD as-is.");
    }
    println!();
    println!("Review with: git diff --cached -- {submodule_path}");
    println!("This tool did not continue the merge/rebase; do so yourself when ready.");

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

/// Walks every ref in the submodule, hiding commits reachable from either
/// `ours_oid` or `theirs_oid`. What remains are exactly the commits introduced
/// to reconcile one side with the other (cherry-pick, rebase, merge commit).
/// Of those, only commits that descend from `ours_oid` are valid resolutions:
/// the reconciliation is by definition `theirs` applied on top of `ours`, so
/// `ours_oid` must be in the candidate's ancestry. Copies cherry-picked onto
/// other branches (backports, stale rebases) fail this test and are rejected.
///
/// Note: we intentionally do NOT filter by the stage-1 "ancestor" recorded in
/// the superproject index. That value is the submodule SHA at the superproject
/// merge base, which in real repos is often *not* an ancestor of `ours_oid`
/// (e.g. when the submodule pointer was moved sideways in a later commit on
/// the ours branch). `ours_oid` is the sharper, more reliable anchor.
///
/// This also deliberately avoids trusting `.gitmodules` `branch`, which on
/// feature branches is often wrong: both the superproject and submodule may
/// be on alternate branches that `.gitmodules` doesn't name.
fn find_matching_commit(
    sub_repo: &Repository,
    ours_oid: Oid,
    theirs_oid: Oid,
    target_message: &str,
) -> Result<(Oid, Vec<String>)> {
    let seeds = collect_ref_tips(sub_repo)?;
    if seeds.is_empty() {
        bail!("submodule has no refs to search; create or fetch the branch with the merged commit");
    }

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
    revwalk
        .hide(theirs_oid)
        .with_context(|| format!("could not hide 'theirs' {theirs_oid} from revwalk"))?;

    let mut matches: Vec<Oid> = Vec::new();
    for oid_result in revwalk {
        let oid = oid_result?;
        let commit = sub_repo.find_commit(oid)?;
        if commit.message_raw() == Some(target_message) {
            matches.push(oid);
        }
    }

    if matches.is_empty() {
        bail!(
            "no commit introduced between 'ours' and any submodule ref carries the \
             incoming commit message (searched {} ref(s)); has the submodule side \
             been merged yet?",
            seeds.len()
        );
    }

    let valid: Vec<Oid> = matches
        .iter()
        .copied()
        .filter(|&oid| {
            sub_repo
                .graph_descendant_of(oid, ours_oid)
                .unwrap_or(false)
        })
        .collect();

    let found = match valid.as_slice() {
        [] => {
            let list = matches
                .iter()
                .map(|o| o.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "found {} commit(s) with the incoming commit message ({list}) but none \
                 descend from 'ours' {ours_oid}; these look like copies on other branches \
                 (backports or stale rebases) rather than the reconciliation of 'theirs' \
                 into 'ours'",
                matches.len()
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
                "ambiguous match: multiple submodule commits descend from 'ours' \
                 {ours_oid} and carry the incoming commit message ({list}); \
                 resolve manually"
            );
        }
    };

    let containing = refs_containing(sub_repo, found);
    Ok((found, containing))
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
