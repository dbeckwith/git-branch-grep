#![warn(rust_2018_idioms, clippy::all)]
#![deny(clippy::correctness)]

use anyhow::Result;
use argh::FromArgs;
use itertools::{Either, Itertools};
use regex::Regex;
use std::{collections::HashSet, path::PathBuf, str};

/// Search the content of diffs between git tags.
#[derive(Debug, FromArgs)]
struct Args {
    #[argh(positional)]
    search: Regex,
}

#[derive(Debug)]
struct DiffLine {
    added: bool,
    line: Line,
}

#[derive(Debug, PartialEq, Eq)]
struct Line {
    content: String,
    lineno: u32,
    path: PathBuf,
}

fn main() -> Result<()> {
    let Args { search } = argh::from_env::<Args>();

    let repo = git2::Repository::open_from_env()?;
    let old_tree = repo.revparse_single("master")?.peel_to_tree()?;
    let new_tree = repo.revparse_single("HEAD")?.peel_to_tree()?;
    let diff = repo.diff_tree_to_tree(
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
    )?;
    // TODO: do diff.find_similar

    let diff_lines =
        process_diff(&diff, git2::DiffFormat::Patch, |delta, _hunk, line| {
            let added = match line.origin_value() {
                git2::DiffLineType::Addition => true,
                git2::DiffLineType::Deletion => false,
                _ => return Ok(None),
            };
            let content = str::from_utf8(line.content())?;
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
            if search.is_match(content) {
                Ok(Some(DiffLine {
                    added,
                    line: Line {
                        content: content.to_owned(),
                        lineno,
                        path: path.to_owned(),
                    },
                }))
            } else {
                Ok(None)
            }
        })?;

    let (mut lines, removed): (Vec<_>, HashSet<_>) = dbg!(diff_lines)
        .into_iter()
        .partition_map(|DiffLine { added, line }| {
            if added {
                Either::Left(line)
            } else {
                Either::Right(line.content)
            }
        });
    for idx in (0..lines.len()).rev() {
        let line = &mut lines[idx];
        if removed.contains(&line.content) {
            lines.remove(idx);
        }
    }
    dbg!(lines);

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
    let mut print_result = Ok(());
    let _ =
        diff.print(format, |delta, hunk, line| match cb(delta, hunk, line) {
            Ok(value) => {
                if let Some(value) = value {
                    results.push(value);
                }
                true
            },
            Err(error) => {
                print_result = Err(error);
                false
            },
        });
    print_result.map(|()| results)
}
