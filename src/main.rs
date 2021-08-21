#![warn(rust_2018_idioms, clippy::all)]
#![deny(clippy::correctness)]

use anyhow::Result;
use argh::FromArgs;
use git2::{
    Diff,
    DiffDelta,
    DiffFormat,
    DiffHunk,
    DiffLine,
    DiffOptions,
    Repository,
};
use std::str;

/// Search the content of diffs between git tags.
#[derive(Debug, FromArgs)]
struct Args {
    #[argh(positional)]
    search: String,
}

fn main() -> Result<()> {
    let Args { search } = argh::from_env::<Args>();

    let repo = Repository::open_from_env()?;
    let old_tree = repo.revparse_single("master")?.peel_to_tree()?;
    let new_tree = repo.revparse_single("HEAD")?.peel_to_tree()?;
    let diff = repo.diff_tree_to_tree(
        Some(&old_tree),
        Some(&new_tree),
        Some(
            DiffOptions::new()
                .include_untracked(true)
                .recurse_untracked_dirs(true)
                .include_unmodified(true)
                .ignore_filemode(true)
                .ignore_whitespace(true)
                .context_lines(0),
        ),
    )?;
    // TODO: do diff.find_similar
    dbg!(diff.stats()?.files_changed());
    process_diff(&diff, DiffFormat::Patch, |delta, _hunk, line| {
        let added = match line.origin() {
            '+' => true,
            '-' => false,
            _ => return Ok(()),
        };
        let content = str::from_utf8(line.content())?;
        let lineno = line.new_lineno();
        let file = match delta.new_file().path() {
            Some(file) => file,
            None => return Ok(()),
        };
        if content.contains(search.as_str()) {
            dbg!(added, content, lineno, file);
        }
        Ok(())
    })?;

    Ok(())
}

fn process_diff<F>(diff: &Diff<'_>, format: DiffFormat, mut cb: F) -> Result<()>
where
    F: FnMut(DiffDelta<'_>, Option<DiffHunk<'_>>, DiffLine<'_>) -> Result<()>,
{
    let mut print_result = Ok(());
    diff.print(format, |delta, hunk, line| match cb(delta, hunk, line) {
        Ok(()) => true,
        Err(error) => {
            print_result = Err(error);
            false
        },
    })?;
    print_result
}
