#![warn(rust_2018_idioms, clippy::all)]
#![deny(clippy::correctness)]
#![allow(clippy::let_and_return)]

use anyhow::{bail, Context, Result};
use argh::FromArgs;
use itertools::{Either, Itertools};
use regex::Regex;
use std::{collections::HashSet, ops::Range, path::PathBuf, str};

/// Search the content of diffs between git tags.
///
/// This utility takes a diff between HEAD and the parent branch and filters
/// lines in the diff by the search text. The search text is interpreted as a
/// regular expression, so regex syntax must be escaped.
#[derive(Debug, FromArgs)]
struct Args {
    /// the text to search with
    #[argh(positional)]
    search: Regex,
    /// the name of the parent branch to diff against, defaults to "master"
    #[argh(option, short = 'p')]
    parent: Option<String>,
    /// turn on debug output
    #[argh(switch)]
    debug: bool,
}

#[derive(Debug)]
struct DiffLine {
    added: bool,
    line: Line,
}

#[derive(Debug, PartialEq, Eq)]
struct Line {
    content: String,
    range: Range<usize>,
    lineno: u32,
    path: PathBuf,
}

fn main() -> Result<()> {
    let Args {
        search,
        parent: parent_branch_name,
        debug,
    } = argh::from_env::<Args>();

    let repo = git2::Repository::open_from_env()
        .context("error opening repository")?;

    let head_commit = repo
        .head()
        .and_then(|reference| reference.peel_to_commit())
        .context("error resolving head commit")?;
    if debug {
        eprintln!("HEAD commit: {}", head_commit.id());
    }

    let root_branch_name = "master";
    let parent_branch_name =
        parent_branch_name.as_deref().unwrap_or(root_branch_name);
    let parent_commit = repo
        .revparse_single(parent_branch_name)
        .and_then(|object| object.peel_to_commit())
        .context("error resolving parent commit")?;
    if debug {
        eprintln!("parent commit: {}", parent_commit.id());
    }

    let base_commit = if head_commit.id() == parent_commit.id() {
        if parent_branch_name == root_branch_name {
            // if HEAD is on the root branch, use the root commit of the repo
            let root_commit = repo
                .revwalk()
                .and_then(|mut revwalk| {
                    revwalk.push_head()?;
                    revwalk
                        .find_map(|id| {
                            (|| {
                                let id = id?;
                                let commit = repo.find_commit(id)?;
                                if commit.parent_count() == 0 {
                                    return Ok(Some(commit));
                                }
                                Ok(None)
                            })()
                            .transpose()
                        })
                        .transpose()
                })
                .context("error finding root commit")?
                .context("root commit not found")?;
            if debug {
                eprintln!(
                    "HEAD is on root branch, using root commit as diff base"
                );
            }
            root_commit
        } else {
            bail!("HEAD and parent refs are the same")
        }
    } else {
        // otherwise, find the merge base between HEAD and master
        let merge_base_commit = repo
            .merge_base(head_commit.id(), parent_commit.id())
            .and_then(|id| repo.find_commit(id))
            .context("error getting merge base commit")?;
        if debug {
            eprintln!("using merge base between HEAD and parent as diff base");
        }
        merge_base_commit
    };

    if debug {
        eprintln!("diff base commit: {}", base_commit.id());
    }
    let old_tree = base_commit.tree().context("error getting old tree")?;
    let new_tree = head_commit.tree().context("error getting new tree")?;
    let diff = repo
        .diff_tree_to_tree(
            Some(&old_tree),
            Some(&new_tree),
            Some(
                git2::DiffOptions::new()
                    .include_untracked(true)
                    .recurse_untracked_dirs(true)
                    .include_unmodified(true)
                    .ignore_filemode(true)
                    .ignore_whitespace(true)
                    .context_lines(0),
            ),
        )
        .and_then(|mut diff| {
            diff.find_similar(Some(git2::DiffFindOptions::new().all(true)))?;
            Ok(diff)
        })
        .context("error diffing")?;

    let diff_lines =
        process_diff(&diff, git2::DiffFormat::Patch, |delta, _hunk, line| {
            let added = match line.origin_value() {
                git2::DiffLineType::Addition => true,
                git2::DiffLineType::Deletion => false,
                _ => return Ok(None),
            };
            let content = str::from_utf8(line.content())
                .context("error converting line content to utf8")?;
            let content = content.trim();
            // if the line is either added or deleted, one of these must be Some
            let lineno = line
                .new_lineno()
                .or_else(|| line.old_lineno())
                .expect("no lineno");
            let path = match delta.new_file().path() {
                Some(path) => path,
                None => return Ok(None),
            };
            if let Some(r#match) = search.find(content) {
                Ok(Some(DiffLine {
                    added,
                    line: Line {
                        content: content.to_owned(),
                        range: r#match.range(),
                        lineno,
                        path: path.to_owned(),
                    },
                }))
            } else {
                Ok(None)
            }
        })
        .context("error processing diff")?;

    let (mut lines, removed): (Vec<_>, HashSet<_>) = diff_lines
        .into_iter()
        .partition_map(|DiffLine { added, line }| {
            if added {
                Either::Left(line)
            } else {
                Either::Right(line.content)
            }
        });
    for idx in (0..lines.len()).rev() {
        let line = &lines[idx];
        if removed.contains(&line.content) {
            lines.remove(idx);
        }
    }

    for Line {
        content,
        range,
        lineno,
        path,
    } in lines
    {
        let path = path.display();
        let before = &content[..range.start];
        let after = &content[range.end..];
        let r#match = &content[range];
        println!(
            "\x1b[32m{}\x1b[m:\x1b[33m{}\x1b[m: {}\x1b[36;1m{}\x1b[m{}",
            path, lineno, before, r#match, after
        );
    }

    Ok(())
}

fn process_diff<F, T>(
    diff: &git2::Diff<'_>,
    format: git2::DiffFormat,
    mut cb: F,
) -> Result<Vec<T>>
where
    F: FnMut(
        git2::DiffDelta<'_>,
        Option<git2::DiffHunk<'_>>,
        git2::DiffLine<'_>,
    ) -> Result<Option<T>>,
{
    let mut results = Vec::new();
    let mut cb_result = Ok(());
    let print_result = diff
        .print(format, |delta, hunk, line| match cb(delta, hunk, line) {
            Ok(value) => {
                if let Some(value) = value {
                    results.push(value);
                }
                true
            },
            Err(error) => {
                cb_result = Err(error);
                false
            },
        })
        .context("error in iterating diff lines");
    cb_result.and(print_result).map(|()| results)
}
