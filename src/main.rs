#![warn(rust_2018_idioms, clippy::all)]
#![deny(clippy::correctness)]

use argh::FromArgs;

/// Search the content of diffs between git tags.
#[derive(Debug, FromArgs)]
struct Args {}

fn main() {
    let args = argh::from_env::<Args>();
    dbg!(args);
}
