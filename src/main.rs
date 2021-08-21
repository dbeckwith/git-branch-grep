#![warn(rust_2018_idioms, clippy::all)]
#![deny(clippy::correctness)]
#![allow(clippy::let_and_return)]

use anyhow::{bail, Context, Error, Result};
use argh::FromArgs;
use regex::Regex;
use std::{
    borrow::Borrow,
    collections::{hash_map, HashMap},
    fmt,
    hash::Hash,
    ops::Range,
    path::PathBuf,
    str,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

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
    /// the name of the parent branch to diff against, defaults to
    /// "master"/"main"
    #[argh(option, short = 'p')]
    parent: Option<String>,
    /// a reference to a commit to diff against
    #[argh(option, short = 'd')]
    diff_base: Option<String>,
    /// turn on debug output
    #[argh(switch)]
    debug: bool,
    /// color output, "always", "auto" (default), or "never"
    #[argh(option, default = "ColorOption::Auto")]
    color: ColorOption,
}

#[derive(Debug)]
enum ColorOption {
    Always,
    Auto,
    Never,
}

impl str::FromStr for ColorOption {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "always" => Ok(Self::Always),
            "auto" => Ok(Self::Auto),
            "never" => Ok(Self::Never),
            s => bail!("{:?} is not a valid color option", s),
        }
    }
}

static COLOR: AtomicBool = AtomicBool::new(false);

#[derive(Debug, PartialEq, Eq)]
struct Line {
    content: String,
    range: Range<usize>,
    lineno: u32,
    path: PathBuf,
}

impl fmt::Display for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Line {
            content,
            range,
            lineno,
            path,
        } = self;
        let path = path.display();
        if COLOR.load(Ordering::SeqCst) {
            let before = &content[..range.start];
            let r#match = &content[range.clone()];
            let after = &content[range.end..];
            write!(
                f,
                "\x1b[32m{}\x1b[m:\x1b[33m{}\x1b[m: {}\x1b[36;1m{}\x1b[m{}",
                path, lineno, before, r#match, after
            )
        } else {
            write!(f, "{}:{}: {}", path, lineno, content)
        }
    }
}

struct MultiSet<T>(HashMap<T, usize>);

impl<T> MultiSet<T>
where
    T: Hash + Eq,
{
    fn new() -> Self {
        Self(HashMap::new())
    }

    fn insert(&mut self, k: T) {
        match self.0.entry(k) {
            hash_map::Entry::Occupied(mut entry) => {
                *entry.get_mut() += 1;
            },
            hash_map::Entry::Vacant(entry) => {
                entry.insert(1);
            },
        }
    }

    fn remove<Q>(&mut self, k: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Hash + Eq,
    {
        if let Some(count) = self.0.get_mut(k) {
            if *count == 1 {
                self.0.remove(k);
            } else {
                *count -= 1;
            }
            true
        } else {
            false
        }
    }
}

fn main() -> Result<()> {
    let Args {
        search,
        parent: parent_branch_name,
        diff_base: base_commit_ref,
        debug,
        color,
    } = argh::from_env::<Args>();

    COLOR.store(
        match color {
            ColorOption::Always => true,
            ColorOption::Auto => atty::is(atty::Stream::Stdout),
            ColorOption::Never => false,
        },
        Ordering::SeqCst,
    );

    macro_rules! debug {
        ($msg:literal $($args:tt)*) => {
            if debug {
                if COLOR.load(Ordering::SeqCst) {
                    eprintln!(concat!("\x1b[90m[DEBUG]\x1b[m ", $msg) $($args)*);
                } else {
                    eprintln!(concat!("[DEBUG] ", $msg) $($args)*);
                }
            }
        };
    }

    if parent_branch_name.is_some() && base_commit_ref.is_some() {
        bail!("cannot specify both parent branch and direct diff base options");
    }

    let repo = git2::Repository::open_from_env()
        .context("error opening repository")?;

    let commit_resolution_timer = Instant::now();
    let base_commit = if let Some(base_commit_ref) = base_commit_ref {
        debug!("using direct base reference");
        let base_commit = repo
            .resolve_reference_from_short_name(&base_commit_ref)
            .and_then(|reference| reference.peel_to_commit())
            .context("error resolving base commit")?;
        base_commit
    } else {
        let head_commit = repo
            .head()
            .and_then(|reference| reference.peel_to_commit())
            .context("error resolving head commit")?;
        debug!("HEAD commit: {}", head_commit.id());

        let root_branch_head_commit =
            std::array::IntoIter::new(["refs/heads/master", "refs/heads/main"])
                .find_map(|name| {
                    repo.find_reference(name)
                        .and_then(|reference| reference.peel_to_commit())
                        .ok()
                })
                .context("root branch not found")?;
        let parent_commit = if let Some(parent_branch_name) = parent_branch_name
        {
            repo.find_reference(&format!("refs/heads/{}", parent_branch_name))
                .and_then(|reference| reference.peel_to_commit())
                .context("error resolving parent commit")?
        } else {
            root_branch_head_commit.clone()
        };
        debug!("parent commit: {}", parent_commit.id());

        if head_commit.id() == parent_commit.id() {
            if head_commit.id() == root_branch_head_commit.id() {
                // if HEAD is on the root branch, use the root commit of the
                // repo
                debug!(
                    "HEAD is on root branch, using root commit as diff base"
                );
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
                root_commit
            } else {
                bail!("HEAD and parent refs are the same")
            }
        } else {
            // otherwise, find the merge base between HEAD and master
            debug!("using merge base between HEAD and parent as diff base");
            let merge_base_commit = repo
                .merge_base(head_commit.id(), parent_commit.id())
                .and_then(|id| repo.find_commit(id))
                .context("error getting merge base commit")?;
            merge_base_commit
        }
    };
    let commit_resolution_timer = commit_resolution_timer.elapsed();

    debug!("diff base commit: {}", base_commit.id());
    let diff_timer = Instant::now();
    let old_tree = base_commit.tree().context("error getting old tree")?;
    let diff = repo
        .diff_tree_to_workdir_with_index(
            Some(&old_tree),
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
        // FIXME: find_similar is too aggressive
        // .and_then(|mut diff| {
        //     diff.find_similar(Some(git2::DiffFindOptions::new().all(true)))?;
        //     Ok(diff)
        // })
        .context("error diffing")?;
    let diff_timer = diff_timer.elapsed();

    let process_diff_timer = Instant::now();
    let mut added_lines = Vec::new();
    let mut removed_lines = MultiSet::new();
    process_diff(&diff, git2::DiffFormat::Patch, |delta, _hunk, line| {
        let added = match line.origin_value() {
            git2::DiffLineType::Addition => true,
            git2::DiffLineType::Deletion => false,
            _ => return Ok(()),
        };
        let file = if added {
            delta.new_file()
        } else {
            delta.old_file()
        };
        if file.is_binary() {
            return Ok(());
        }
        let content = str::from_utf8(line.content())
            .context("error converting line content to utf8")?;
        let content = content.trim();
        // if the line is either added or deleted, one of these must be Some
        let lineno = line
            .new_lineno()
            .or_else(|| line.old_lineno())
            .expect("no lineno");
        let path = match file.path() {
            Some(path) => path,
            None => return Ok(()),
        };
        if let Some(r#match) = search.find(content) {
            if added {
                let line = Line {
                    content: content.to_owned(),
                    range: r#match.range(),
                    lineno,
                    path: path.to_owned(),
                };
                debug!("added line: {}", line);
                added_lines.push(line);
            } else {
                if debug {
                    let line = Line {
                        content: content.to_owned(),
                        range: r#match.range(),
                        lineno,
                        path: path.to_owned(),
                    };
                    debug!("removed line: {}", line);
                }
                removed_lines.insert(content.to_owned());
            }
        }
        Ok(())
    })
    .context("error processing diff")?;
    let process_diff_timer = process_diff_timer.elapsed();

    let line_print_timer = Instant::now();
    for line in added_lines {
        if removed_lines.remove(&line.content) {
            debug!("filtering out added & removed line: {}", line);
        } else {
            println!("{}", line);
        }
    }
    let line_print_timer = line_print_timer.elapsed();

    if debug {
        debug!("timings:");
        macro_rules! show_timer {
            ($name:expr, $timer:expr) => {
                debug!("  {}: {:.3}s", $name, $timer.as_secs_f32());
            };
        }
        show_timer!("commit resolution", commit_resolution_timer);
        show_timer!("diff", diff_timer);
        show_timer!("process diff", process_diff_timer);
        show_timer!("line print", line_print_timer);
    }

    Ok(())
}

fn process_diff<F>(
    diff: &git2::Diff<'_>,
    format: git2::DiffFormat,
    mut cb: F,
) -> Result<()>
where
    F: FnMut(
        git2::DiffDelta<'_>,
        Option<git2::DiffHunk<'_>>,
        git2::DiffLine<'_>,
    ) -> Result<()>,
{
    let mut cb_result = Ok(());
    let print_result = diff
        .print(format, |delta, hunk, line| match cb(delta, hunk, line) {
            Ok(()) => true,
            Err(error) => {
                cb_result = Err(error);
                false
            },
        })
        .context("error in iterating diff lines");
    cb_result.and(print_result)
}
