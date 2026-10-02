//! `batch-rename` — a safe regex/template batch file renamer.
//!
//! **Dry-run is the default.** Nothing on disk changes unless you pass
//! `--commit` (or `-y`). The actual rename logic lives in the `batch_rename`
//! library crate; this binary only collects inputs, builds the rule set, prints
//! the plan, and (when committing) performs the moves.

use anyhow::{bail, Context, Result};
use batch_rename::{
    is_file_name, plan, resolve_with_suffix, CollisionPolicy, Op, Plan, Transforms,
};
use clap::{ArgAction, Parser};
use regex::Regex;
use std::collections::{HashMap, HashSet};
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

    let push = |p: PathBuf, out: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>| -> Result<()> {
        let metadata = std::fs::symlink_metadata(&p)
            .with_context(|| format!("reading source {}", p.display()))?;
        if !metadata.file_type().is_file() {
            bail!(
                "source must be a regular file, not a symlink: {}",
                p.display()
            );
        }
        let p = normalise_parent(&p)?;
        if seen.insert(p.clone()) {
            out.push(p);
        }
        Ok(())
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
                        walk_dir(&p, &mut |f| push(f, &mut out, &mut seen))?;
                    }
                    // non-recursive: skip directories from globs
                } else {
                    push(p, &mut out, &mut seen)?;
                }
            }
            if !matched {
                eprintln!("warning: glob matched nothing: '{raw}'");
            }
        } else if path.is_dir() {
            if cli.recursive {
                walk_dir(path, &mut |f| push(f, &mut out, &mut seen))?;
            } else {
                eprintln!("warning: '{raw}' is a directory; pass --recursive to descend into it");
            }
        } else if path.exists() {
            push(path.to_path_buf(), &mut out, &mut seen)?;
        } else {
            bail!("no such file: '{raw}'");
        }
    }

    Ok(out)
}

/// Walk a directory, invoking `f` on every regular file found (files only).
fn walk_dir(dir: &Path, f: &mut dyn FnMut(PathBuf) -> Result<()>) -> Result<()> {
    for entry in WalkDir::new(dir) {
        let entry = entry.with_context(|| format!("walking {}", dir.display()))?;
        if entry.file_type().is_file() {
            f(entry.into_path())?;
        }
    }
    Ok(())
}

/// Collect the set of paths that currently exist in the parent directory of any
/// input. This is what the planner uses to detect "target already exists".
fn existing_paths(inputs: &[PathBuf]) -> Result<HashSet<PathBuf>> {
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
        let rd = read.with_context(|| format!("checking existing names in {}", d.display()))?;
        for e in rd {
            let e = e.with_context(|| format!("reading existing names in {}", d.display()))?;
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
    Ok(set)
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

    let existing = existing_paths(&inputs)?;
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

/// Resolve directory aliases without following a source-file symlink or losing
/// the file name that the user selected.
fn normalise_parent(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("path has no file name: {}", path.display()))?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = std::fs::canonicalize(parent)
        .with_context(|| format!("resolving parent directory of {}", path.display()))?;
    Ok(parent.join(name))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Location {
    Original,
    Staged,
    Final,
}

struct Rename {
    old: PathBuf,
    new: PathBuf,
    staged: PathBuf,
    metadata: std::fs::Metadata,
    location: Location,
}

/// Stage every source in a reserved directory, then publish using hard links:
/// creating a link fails atomically if a destination already exists. Unlinking
/// the previous name preserves file contents/identity. On ordinary I/O failure,
/// return completed moves to staging before restoring their original names.
fn apply_renames(plan: &Plan) -> Result<usize> {
    if plan.has_collisions() {
        bail!("refusing to apply a plan with collisions");
    }
    let mut sources = HashSet::new();
    let mut targets = HashSet::new();
    let mut renames = Vec::new();
    for (old, new) in &plan.entries {
        let old = normalise_parent(old)?;
        let new = normalise_parent(new)?;
        let name = new.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if old.parent() != new.parent() || !is_file_name(name) {
            bail!(
                "destination must be a single file name in the source directory: {}",
                new.display()
            );
        }
        if !sources.insert(old.clone()) {
            bail!("duplicate source path: {}", old.display());
        }
        if !targets.insert(new.clone()) {
            bail!("duplicate destination path: {}", new.display());
        }
        let metadata = std::fs::symlink_metadata(&old)
            .with_context(|| format!("reading source {}", old.display()))?;
        if !metadata.file_type().is_file() {
            bail!(
                "source must be a regular file, not a symlink: {}",
                old.display()
            );
        }
        if old != new {
            renames.push(Rename {
                old,
                new,
                staged: PathBuf::new(),
                metadata,
                location: Location::Original,
            });
        }
    }
    if renames.is_empty() {
        return Ok(0);
    }

    // Reserve all directories before moving any source. A directory is owned
    // only after exclusive creation succeeds; unrelated candidates are ignored.
    let mut directories: HashMap<PathBuf, PathBuf> = HashMap::new();
    for rename in &mut renames {
        let parent = rename.old.parent().unwrap().to_path_buf();
        if !directories.contains_key(&parent) {
            match reserve_staging(&parent) {
                Ok(directory) => {
                    directories.insert(parent.clone(), directory);
                }
                Err(error) => {
                    cleanup_staging(&directories)
                        .context("sources unchanged; staging cleanup failed")?;
                    return Err(error);
                }
            }
        }
        rename.staged = directories[&parent].join(rename.old.file_name().unwrap());
    }

    for index in 0..renames.len() {
        let rename = &renames[index];
        if let Err(error) = move_without_clobber(&rename.old, &rename.staged, &rename.metadata) {
            return Err(recover_batch(error, &mut renames, &directories));
        }
        renames[index].location = Location::Staged;
    }
    for index in 0..renames.len() {
        let rename = &renames[index];
        if let Err(error) = move_without_clobber(&rename.staged, &rename.new, &rename.metadata) {
            return Err(recover_batch(error, &mut renames, &directories));
        }
        renames[index].location = Location::Final;
    }
    cleanup_staging(&directories).context("renames completed, but staging cleanup failed")?;
    Ok(renames.len())
}

fn reserve_staging(parent: &Path) -> Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    for attempt in 0..1000 {
        let directory = parent.join(format!(
            ".batch-rename.stage.{}.{stamp}.{attempt}",
            std::process::id()
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&directory) {
            Ok(()) => return Ok(directory),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("reserving staging directory in {}", parent.display())
                })
            }
        }
    }
    bail!(
        "could not reserve a staging directory in {}",
        parent.display()
    )
}

fn check_identity(path: &Path, expected: &std::fs::Metadata) -> Result<()> {
    let actual = std::fs::symlink_metadata(path)
        .with_context(|| format!("checking file identity at {}", path.display()))?;
    #[cfg(unix)]
    let same = {
        use std::os::unix::fs::MetadataExt;
        actual.dev() == expected.dev() && actual.ino() == expected.ino()
    };
    #[cfg(not(unix))]
    let same = actual.len() == expected.len()
        && actual.modified().ok() == expected.modified().ok()
        && actual.created().ok() == expected.created().ok();
    if !actual.file_type().is_file() || !same {
        bail!(
            "file changed during rename; refusing to remove {}",
            path.display()
        );
    }
    Ok(())
}

fn move_without_clobber(old: &Path, new: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    check_identity(old, metadata)?;
    std::fs::hard_link(old, new).with_context(|| {
        format!(
            "creating {} without overwriting (from {})",
            new.display(),
            old.display()
        )
    })?;
    let remove_source = check_identity(new, metadata)
        .and_then(|_| check_identity(old, metadata))
        .and_then(|_| {
            std::fs::remove_file(old)
                .with_context(|| format!("removing previous name {}", old.display()))
        });
    if let Err(error) = remove_source {
        // Remove the new link only while the previous name still holds this
        // file. Otherwise retain the surviving copy for batch recovery.
        check_identity(old, metadata).with_context(|| {
            format!("{error:#}; destination link retained at {}", new.display())
        })?;
        check_identity(new, metadata)
            .and_then(|_| std::fs::remove_file(new).context("removing unused destination link"))
            .with_context(|| {
                format!(
                    "{error:#}; source retained, but destination cleanup failed at {}",
                    new.display()
                )
            })?;
        return Err(error);
    }
    Ok(())
}

fn recover_batch(
    error: anyhow::Error,
    renames: &mut [Rename],
    directories: &HashMap<PathBuf, PathBuf>,
) -> anyhow::Error {
    let mut failures = Vec::new();
    // An unlink error can leave only the newly created link. Discover that
    // copy before recovery instead of trusting the last completed step.
    for rename in renames.iter_mut() {
        if rename.location == Location::Original
            && check_identity(&rename.old, &rename.metadata).is_err()
            && check_identity(&rename.staged, &rename.metadata).is_ok()
        {
            rename.location = Location::Staged;
        }
        if rename.location == Location::Staged
            && check_identity(&rename.staged, &rename.metadata).is_err()
            && check_identity(&rename.new, &rename.metadata).is_ok()
        {
            rename.location = Location::Final;
        }
    }
    // Vacate every successfully published target first, including cycles.
    for rename in renames
        .iter_mut()
        .rev()
        .filter(|r| r.location == Location::Final)
    {
        match move_without_clobber(&rename.new, &rename.staged, &rename.metadata) {
            Ok(()) => rename.location = Location::Staged,
            Err(failure) => failures.push(format!(
                "{} -> {}: {failure:#}",
                rename.new.display(),
                rename.old.display()
            )),
        }
    }
    for rename in renames
        .iter_mut()
        .filter(|r| r.location == Location::Staged)
    {
        match move_without_clobber(&rename.staged, &rename.old, &rename.metadata) {
            Ok(()) => rename.location = Location::Original,
            Err(failure) => failures.push(format!(
                "{} -> {}: {failure:#}",
                rename.staged.display(),
                rename.old.display()
            )),
        }
    }
    for rename in renames.iter().filter(|r| r.location == Location::Original) {
        if let Err(failure) = check_identity(&rename.old, &rename.metadata) {
            failures.push(format!(
                "original contents not restored at {}: {failure:#}",
                rename.old.display()
            ));
        }
    }
    if failures.is_empty() {
        match cleanup_staging(directories) {
            Ok(()) => error.context("rename failed; all original names restored"),
            Err(cleanup) => error.context(format!(
                "all original names restored; staging cleanup failed: {cleanup:#}"
            )),
        }
    } else {
        let retained = directories
            .values()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        error.context(format!(
            "rollback incomplete; retained staging directories: {retained}; recovery errors: {}",
            failures.join("; ")
        ))
    }
}

fn cleanup_staging(directories: &HashMap<PathBuf, PathBuf>) -> Result<()> {
    let mut failures = Vec::new();
    for directory in directories.values() {
        if let Err(error) = std::fs::remove_dir(directory) {
            failures.push(format!("{}: {error}", directory.display()));
        }
    }
    if !failures.is_empty() {
        bail!(
            "could not remove staging directories: {}",
            failures.join("; ")
        );
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

#[cfg(test)]
mod safety_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn fixture_plan(dir: &Path, names: &[(&str, &str)]) -> Plan {
        Plan {
            entries: names
                .iter()
                .map(|(old, new)| (dir.join(old), dir.join(new)))
                .collect(),
            collisions: Vec::new(),
        }
    }

    #[test]
    fn applying_a_plan_checks_existing_destinations_again() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a"), b"SOURCE").unwrap();
        fs::write(dir.path().join("keep"), b"KEEP").unwrap();
        let plan = fixture_plan(dir.path(), &[("a", "keep")]);

        assert!(apply_renames(&plan).is_err());
        assert_eq!(fs::read(dir.path().join("a")).unwrap(), b"SOURCE");
        assert_eq!(fs::read(dir.path().join("keep")).unwrap(), b"KEEP");
    }

    #[test]
    fn swaps_preserve_preexisting_staging_candidates_across_repeated_runs() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a"), b"AAA").unwrap();
        fs::write(dir.path().join("b"), b"BBB").unwrap();
        let candidate = dir
            .path()
            .join(format!(".batch-rename.tmp.{}.0", std::process::id()));
        fs::write(&candidate, b"KEEP").unwrap();
        let plan = fixture_plan(dir.path(), &[("a", "b"), ("b", "a")]);

        assert_eq!(apply_renames(&plan).unwrap(), 2);
        assert_eq!(fs::read(&candidate).unwrap(), b"KEEP");
        assert_eq!(fs::read(dir.path().join("a")).unwrap(), b"BBB");
        assert_eq!(fs::read(dir.path().join("b")).unwrap(), b"AAA");
        assert_eq!(apply_renames(&plan).unwrap(), 2);
        assert_eq!(fs::read(&candidate).unwrap(), b"KEEP");
        assert_eq!(fs::read(dir.path().join("a")).unwrap(), b"AAA");
        assert_eq!(fs::read(dir.path().join("b")).unwrap(), b"BBB");
    }

    #[test]
    fn failed_direct_batch_keeps_all_original_sources() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a"), b"AAA").unwrap();
        let plan = fixture_plan(dir.path(), &[("a", "new"), ("missing", "other")]);

        assert!(apply_renames(&plan).is_err());
        assert_eq!(fs::read(dir.path().join("a")).unwrap(), b"AAA");
        assert!(!dir.path().join("new").exists());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_staged_batch_keeps_all_original_sources() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a"), b"AAA").unwrap();
        fs::write(dir.path().join("b"), b"BBB").unwrap();
        let plan = fixture_plan(dir.path(), &[("a", "b"), ("b", "a"), ("missing", "other")]);

        assert!(apply_renames(&plan).is_err());
        assert_eq!(fs::read(dir.path().join("a")).unwrap(), b"AAA");
        assert_eq!(fs::read(dir.path().join("b")).unwrap(), b"BBB");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn finalisation_failure_restores_the_staged_batch() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a"), b"AAA").unwrap();
        fs::write(dir.path().join("b"), b"BBB").unwrap();
        let too_long = "x".repeat(300);
        let plan = fixture_plan(dir.path(), &[("a", "b"), ("b", &too_long)]);

        let error = apply_renames(&plan).unwrap_err();
        assert!(format!("{error:#}").contains("all original names restored"));
        assert_eq!(fs::read(dir.path().join("a")).unwrap(), b"AAA");
        assert_eq!(fs::read(dir.path().join("b")).unwrap(), b"BBB");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn failed_rollback_preserves_staged_contents_and_the_new_occupant() {
        let dir = TempDir::new().unwrap();
        let old = dir.path().join("a");
        fs::write(&old, b"SOURCE").unwrap();
        let metadata = fs::symlink_metadata(&old).unwrap();
        let staging = reserve_staging(dir.path()).unwrap();
        let staged = staging.join("a");
        move_without_clobber(&old, &staged, &metadata).unwrap();
        fs::write(&old, b"KEEP").unwrap();
        let mut renames = vec![Rename {
            old: old.clone(),
            new: dir.path().join("new"),
            staged: staged.clone(),
            metadata,
            location: Location::Staged,
        }];
        let directories = HashMap::from([(dir.path().to_path_buf(), staging.clone())]);

        let error = recover_batch(
            anyhow::anyhow!("fixture I/O failure"),
            &mut renames,
            &directories,
        );
        assert!(format!("{error:#}").contains("rollback incomplete"));
        assert!(format!("{error:#}").contains(staging.to_str().unwrap()));
        assert_eq!(fs::read(old).unwrap(), b"KEEP");
        assert_eq!(fs::read(staged).unwrap(), b"SOURCE");
    }

    #[test]
    fn recovery_finds_a_retained_link_when_the_previous_name_disappeared() {
        let dir = TempDir::new().unwrap();
        let old = dir.path().join("a");
        fs::write(&old, b"SOURCE").unwrap();
        let metadata = fs::symlink_metadata(&old).unwrap();
        let staging = reserve_staging(dir.path()).unwrap();
        let staged = staging.join("a");
        fs::hard_link(&old, &staged).unwrap();
        fs::remove_file(&old).unwrap();
        let mut renames = vec![Rename {
            old: old.clone(),
            new: dir.path().join("new"),
            staged,
            metadata,
            location: Location::Original,
        }];
        let directories = HashMap::from([(dir.path().to_path_buf(), staging.clone())]);

        let error = recover_batch(
            anyhow::anyhow!("fixture unlink failure"),
            &mut renames,
            &directories,
        );
        assert!(format!("{error:#}").contains("all original names restored"));
        assert_eq!(fs::read(old).unwrap(), b"SOURCE");
        assert!(!staging.exists());
    }
}
