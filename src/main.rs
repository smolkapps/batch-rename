//! `batch-rename` — a safe regex/template batch file renamer.
//!
//! **Dry-run is the default.** Nothing on disk changes unless you pass
//! `--commit` (or `-y`). The actual rename logic lives in the `batch_rename`
//! library crate; this binary only collects inputs, builds the rule set, prints
//! the plan, and (when committing) performs the moves.

use anyhow::{bail, Context, Result};
use batch_rename::{plan, resolve_with_suffix, CollisionPolicy, Op, Plan, Transforms};
use clap::{ArgAction, Parser};
use regex::Regex;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Safe batch file renamer. Dry-run by default — pass --commit to apply.
#[derive(Parser, Debug)]
#[command(
    name = "batch-rename",
    version,
    about = "Safe regex/template batch file renamer (dry-run by default).",
    long_about = "Rename many files via composable rules.\n\nDRY-RUN IS THE DEFAULT: by default it only prints the planned `old -> new` \ntable and changes nothing. Pass --commit (or -y) to actually rename."
)]
struct Cli {
    /// Files to rename, and/or a glob like '*.jpeg'. Globs are expanded by the
    /// tool itself (quote them so your shell doesn't expand them first).
    #[arg(value_name = "PATH_OR_GLOB")]
    inputs: Vec<String>,

    /// sed-style substitution applied to the file name, e.g. 's/IMG_(\d+)/photo_$1/'.
    /// Use $1, $2 for capture groups. Repeatable; applied in order.
    #[arg(long = "regex", value_name = "s/PAT/REPL/", action = ArgAction::Append)]
    regex: Vec<String>,

    /// Literal substring replacement: every FROM becomes TO.
    #[arg(long = "replace", num_args = 2, value_names = ["FROM", "TO"], action = ArgAction::Append)]
    replace: Vec<String>,

    /// Prepend STR to the stem.
    #[arg(long, value_name = "STR")]
    prefix: Option<String>,

    /// Append STR to the stem (before the extension).
    #[arg(long, value_name = "STR")]
    suffix: Option<String>,

    /// Lowercase file names.
    #[arg(long)]
    lower: bool,

    /// Uppercase file names.
    #[arg(long)]
    upper: bool,

    /// Replace the extension with NEWEXT (no leading dot; empty removes it).
    #[arg(long, value_name = "NEWEXT")]
    ext: Option<String>,

    /// Sequential numbering, exposing the {n} template token. Without a
    /// --template, the number is appended to each stem.
    #[arg(long)]
    seq: bool,

    /// First sequence number (with --seq / {n}).
    #[arg(long, default_value_t = 1)]
    start: usize,

    /// Zero-pad the sequence number to this width (e.g. 3 -> 001).
    #[arg(long, default_value_t = 0)]
    pad: usize,

    /// Template defining the whole new name. Tokens: {name}, {ext}, {n}.
    /// e.g. '{name}_{n}.{ext}'.
    #[arg(long, value_name = "TEMPLATE")]
    template: Option<String>,

    /// Recurse into directories given as inputs.
    #[arg(long)]
    recursive: bool,

    /// Collision handling: 'refuse' (default, safe) or 'suffix' (append " (1)").
    #[arg(long, value_name = "POLICY", default_value = "refuse")]
    collision: CollisionArg,

    /// Apply the renames. Without this, runs as a dry-run and changes nothing.
    #[arg(long, short = 'y', visible_alias = "yes")]
    commit: bool,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum CollisionArg {
    Refuse,
    Suffix,
}

impl From<CollisionArg> for CollisionPolicy {
    fn from(a: CollisionArg) -> Self {
        match a {
            CollisionArg::Refuse => CollisionPolicy::Refuse,
            CollisionArg::Suffix => CollisionPolicy::Suffix,
        }
    }
}

/// Parse a sed-style `s/PAT/REPL/` (or `s/PAT/REPL/g`, the default behaviour is
/// already global here) into `(pattern, replacement)`. The delimiter is the
/// character right after `s`, so `s|a|b|` works too when the pattern contains
/// slashes.
fn parse_sed(expr: &str) -> Result<(String, String)> {
    let bytes: Vec<char> = expr.chars().collect();
    if bytes.is_empty() || bytes[0] != 's' {
        bail!("regex must be in sed form 's/PAT/REPL/': got '{expr}'");
    }
    if bytes.len() < 2 {
        bail!("regex too short: '{expr}'");
    }
    let delim = bytes[1];
    // Split into up to 3 fields by the (unescaped) delimiter.
    let mut fields: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut i = 2;
    let mut esc = false;
    while i < bytes.len() {
        let c = bytes[i];
        if esc {
            // keep escaped delimiter as a literal delimiter char; pass other
            // escapes (e.g. \d, \.) through verbatim to the regex engine.
            if c == delim {
                cur.push(c);
            } else {
                cur.push('\\');
                cur.push(c);
            }
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else if c == delim {
            fields.push(std::mem::take(&mut cur));
            if fields.len() == 2 {
                // Anything after the 3rd delimiter is flags (e.g. trailing 'g');
                // 'g'/global is already the default here, so ignore the rest.
                break;
            }
        } else {
            cur.push(c);
        }
        i += 1;
    }
    if fields.len() < 2 {
        // allow trailing form without closing delimiter: s/PAT/REPL
        fields.push(std::mem::take(&mut cur));
    }
    if fields.len() < 2 {
        bail!("regex must look like 's/PAT/REPL/': got '{expr}'");
    }
    Ok((fields[0].clone(), fields[1].clone()))
}

/// Build the ordered op list from CLI flags.
///
/// Order is deliberate and documented in the README: regex subs, then literal
/// replace, then prefix/suffix, then case, then extension.
fn build_transforms(cli: &Cli) -> Result<Transforms> {
    let mut ops: Vec<Op> = Vec::new();

    for r in &cli.regex {
        let (pat, rep) = parse_sed(r)?;
        let re = Regex::new(&pat).with_context(|| format!("invalid regex pattern: '{pat}'"))?;
        ops.push(Op::Regex { re, rep });
    }
    // --replace may be repeated; clap groups them in pairs.
    for pair in cli.replace.chunks(2) {
        if pair.len() == 2 {
            ops.push(Op::Replace {
                from: pair[0].clone(),
                to: pair[1].clone(),
            });
        }
    }
    if let Some(p) = &cli.prefix {
        ops.push(Op::Prefix(p.clone()));
    }
    if let Some(s) = &cli.suffix {
        ops.push(Op::Suffix(s.clone()));
    }
    if cli.lower {
        ops.push(Op::Lower);
    }
    if cli.upper {
        ops.push(Op::Upper);
    }
    if let Some(e) = &cli.ext {
        ops.push(Op::Ext(e.clone()));
    }

    Ok(Transforms {
        ops,
        seq: cli.seq,
        start: cli.start,
        pad: cli.pad,
        template: cli.template.clone(),
    })
}

/// Expand the positional inputs into a concrete, de-duplicated, ordered list of
/// **file** paths. Handles: literal paths, glob patterns, and (with
/// `--recursive`) directory walks.
fn gather_inputs(cli: &Cli) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    let push = |p: PathBuf, out: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>| {
        if seen.insert(p.clone()) {
            out.push(p);
        }
    };

    for raw in &cli.inputs {
        let path = Path::new(raw);
        let looks_glob = raw.contains('*') || raw.contains('?') || raw.contains('[');

        if looks_glob {
            let mut matched = false;
            for entry in glob::glob(raw).with_context(|| format!("bad glob: '{raw}'"))? {
                let p = entry?;
                matched = true;
                if p.is_dir() {
                    if cli.recursive {
                        walk_dir(&p, &mut |f| push(f, &mut out, &mut seen));
                    }
                    // non-recursive: skip directories from globs
                } else {
                    push(p, &mut out, &mut seen);
                }
            }
            if !matched {
                eprintln!("warning: glob matched nothing: '{raw}'");
            }
        } else if path.is_dir() {
            if cli.recursive {
                walk_dir(path, &mut |f| push(f, &mut out, &mut seen));
            } else {
                eprintln!("warning: '{raw}' is a directory; pass --recursive to descend into it");
            }
        } else if path.exists() {
            push(path.to_path_buf(), &mut out, &mut seen);
        } else {
            eprintln!("warning: no such file: '{raw}'");
        }
    }

    Ok(out)
}

/// Walk a directory, invoking `f` on every regular file found (files only).
fn walk_dir(dir: &Path, f: &mut dyn FnMut(PathBuf)) {
    for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            f(entry.into_path());
        }
    }
}

/// Collect the set of paths that currently exist in the parent directory of any
/// input. This is what the planner uses to detect "target already exists".
fn existing_paths(inputs: &[PathBuf]) -> HashSet<PathBuf> {
    let mut dirs: HashSet<PathBuf> = HashSet::new();
    for p in inputs {
        dirs.insert(p.parent().map(Path::to_path_buf).unwrap_or_default());
    }
    let mut set: HashSet<PathBuf> = HashSet::new();
    for d in dirs {
        let read = if d.as_os_str().is_empty() {
            std::fs::read_dir(".")
        } else {
            std::fs::read_dir(&d)
        };
        if let Ok(rd) = read {
            for e in rd.flatten() {
                // Normalise so that a "" parent (cwd) matches the planner's
                // `parent.join(name)` which yields a bare relative path.
                let path = e.path();
                if d.as_os_str().is_empty() {
                    if let Some(name) = path.file_name() {
                        set.insert(PathBuf::from(name));
                    }
                } else {
                    set.insert(path);
                }
            }
        }
    }
    set
}

/// Pretty-print the plan as an `old -> new` table.
fn print_plan(p: &Plan, committed: bool) {
    let changes: Vec<&(PathBuf, PathBuf)> = p.changes().collect();
    if changes.is_empty() {
        println!("No files would be renamed (every name already matches the rules).");
        return;
    }

    // Column width for the left side.
    let width = changes
        .iter()
        .map(|(o, _)| o.display().to_string().len())
        .max()
        .unwrap_or(0);

    let header = if committed {
        "RENAMED"
    } else {
        "PLAN (dry-run)"
    };
    println!("{header}:");
    for (old, new) in &changes {
        println!(
            "  {:<width$}  ->  {}",
            old.display().to_string(),
            new.display(),
            width = width
        );
    }
}

fn run() -> Result<i32> {
    let cli = Cli::parse();

    if cli.inputs.is_empty() {
        bail!("no inputs given. Pass file paths and/or a glob (e.g. '*.jpg'). See --help.");
    }
    if cli.lower && cli.upper {
        bail!("--lower and --upper are mutually exclusive.");
    }

    let transforms = build_transforms(&cli)?;
    let inputs = gather_inputs(&cli)?;

    if inputs.is_empty() {
        bail!("no matching files to rename.");
    }

    let existing = existing_paths(&inputs);
    let policy: CollisionPolicy = cli.collision.into();

    let raw_plan = plan(&inputs, &transforms, &existing)?;

    // Resolve or refuse collisions.
    let final_plan = if raw_plan.has_collisions() {
        match policy {
            CollisionPolicy::Refuse => {
                eprintln!(
                    "error: {} collision(s) detected — refusing to overwrite:",
                    raw_plan.collisions.len()
                );
                for c in &raw_plan.collisions {
                    eprintln!("  would clobber: {}", c.display());
                }
                eprintln!(
                    "\nRe-run with `--collision suffix` to auto-disambiguate, or fix your rules."
                );
                // Still show what the (unsafe) plan looked like, for context.
                print_plan(&raw_plan, false);
                return Ok(2);
            }
            CollisionPolicy::Suffix => resolve_with_suffix(&raw_plan, &existing),
        }
    } else {
        raw_plan
    };

    let n = final_plan.changed_count();

    if !cli.commit {
        print_plan(&final_plan, false);
        println!(
            "\nDry-run: {} file(s) would be renamed. Nothing changed. Pass --commit (or -y) to apply.",
            n
        );
        return Ok(0);
    }

    // Commit: perform the renames. We computed targets to be collision-free, but
    // to be safe against in-batch ordering issues (A->B while B->C), apply in an
    // order that never overwrites a not-yet-moved source.
    let applied = apply_renames(&final_plan)?;
    print_plan(&final_plan, true);
    println!("\nDone: {applied} file(s) renamed.");
    Ok(0)
}

/// Apply the renames safely. To avoid clobbering a source that is itself about
/// to move (A->B, B->C), we move via temporary names when a target is also a
/// pending source. Simpler and robust: rename to unique temp names first, then
/// to final names.
fn apply_renames(plan: &Plan) -> Result<usize> {
    let changes: Vec<&(PathBuf, PathBuf)> = plan.changes().collect();
    if changes.is_empty() {
        return Ok(0);
    }

    let targets: HashSet<&PathBuf> = changes.iter().map(|(_, n)| n).collect();
    let needs_temp = changes.iter().any(|(o, _)| targets.contains(o));

    if !needs_temp {
        // No source is also a target — direct moves are safe.
        for (old, new) in &changes {
            ensure_parent(new)?;
            std::fs::rename(old, new)
                .with_context(|| format!("renaming {} -> {}", old.display(), new.display()))?;
        }
        return Ok(changes.len());
    }

    // Two-phase: old -> temp -> new.
    let mut temps: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(changes.len());
    for (i, (old, _new)) in changes.iter().enumerate() {
        let parent = old.parent().map(Path::to_path_buf).unwrap_or_default();
        let tmp = parent.join(format!(".batch-rename.tmp.{}.{}", std::process::id(), i));
        std::fs::rename(old, &tmp)
            .with_context(|| format!("staging {} -> {}", old.display(), tmp.display()))?;
        temps.push((tmp, changes[i].1.clone()));
    }
    for (tmp, new) in &temps {
        ensure_parent(new)?;
        std::fs::rename(tmp, new)
            .with_context(|| format!("finalising {} -> {}", tmp.display(), new.display()))?;
    }
    Ok(changes.len())
}

fn ensure_parent(p: &Path) -> Result<()> {
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }
    }
    Ok(())
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}
