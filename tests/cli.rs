//! End-to-end integration tests driving the real binary against a tempdir of
//! actual files. These assert behaviour on disk, the dry-run default, and that
//! collisions are refused with a non-zero exit.

use assert_cmd::Command;
use predicates::prelude::*;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// Create files (empty) in `dir`.
fn touch_all(dir: &Path, names: &[&str]) {
    for n in names {
        let p = dir.join(n);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, b"").unwrap();
    }
}

/// Sorted set of file names currently in `dir` (non-recursive).
fn list(dir: &Path) -> BTreeSet<String> {
    fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

fn bin() -> Command {
    Command::cargo_bin("batch-rename").unwrap()
}

#[test]
fn dry_run_is_default_and_changes_nothing() {
    let dir = TempDir::new().unwrap();
    touch_all(dir.path(), &["IMG_1.jpg", "IMG_2.jpg"]);
    let before = list(dir.path());

    bin()
        .current_dir(&dir)
        .args(["--regex", "s/IMG_/photo_/", "*.jpg"])
        .assert()
        .success()
        .stdout(predicate::str::contains("photo_1.jpg"))
        .stdout(predicate::str::contains("Dry-run"));

    // Nothing on disk changed.
    let after = list(dir.path());
    assert_eq!(before, after, "dry-run must not touch the filesystem");
}

#[test]
fn commit_actually_renames() {
    let dir = TempDir::new().unwrap();
    touch_all(dir.path(), &["IMG_1.jpg", "IMG_2.jpg"]);

    bin()
        .current_dir(&dir)
        .args(["--regex", "s/IMG_/photo_/", "--commit", "*.jpg"])
        .assert()
        .success()
        .stdout(predicate::str::contains("RENAMED"));

    let after = list(dir.path());
    let expected: BTreeSet<String> = ["photo_1.jpg", "photo_2.jpg"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(after, expected);
}

#[test]
fn commit_via_short_y_flag() {
    let dir = TempDir::new().unwrap();
    touch_all(dir.path(), &["a.txt"]);

    bin()
        .current_dir(&dir)
        .args(["--upper", "-y", "a.txt"])
        .assert()
        .success();

    let after = list(dir.path());
    assert!(after.contains("A.TXT"), "got {after:?}");
}

#[test]
fn template_seq_padding_on_disk() {
    let dir = TempDir::new().unwrap();
    // Names chosen so sort order is deterministic via explicit args.
    touch_all(dir.path(), &["alpha.png", "beta.png", "gamma.png"]);

    bin()
        .current_dir(&dir)
        .args([
            "--seq",
            "--start",
            "1",
            "--pad",
            "3",
            "--template",
            "shot_{n}.{ext}",
            "--commit",
            // pass explicitly in order to guarantee numbering
            "alpha.png",
            "beta.png",
            "gamma.png",
        ])
        .assert()
        .success();

    let after = list(dir.path());
    let expected: BTreeSet<String> = ["shot_001.png", "shot_002.png", "shot_003.png"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(after, expected);
}

#[test]
fn colliding_rename_is_refused_nonzero() {
    let dir = TempDir::new().unwrap();
    // Two files that both map to the same target.
    touch_all(dir.path(), &["a.txt", "b.txt"]);

    bin()
        .current_dir(&dir)
        .args([
            "--regex",
            r"s/^[ab]\.txt$/merged.txt/",
            "--commit",
            "a.txt",
            "b.txt",
        ])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("collision"));

    // Disk untouched: originals still there, no 'merged.txt'.
    let after = list(dir.path());
    assert!(after.contains("a.txt") && after.contains("b.txt"));
    assert!(!after.contains("merged.txt"));
}

#[test]
fn collision_with_existing_file_is_refused() {
    let dir = TempDir::new().unwrap();
    touch_all(dir.path(), &["a.txt", "b.txt"]); // b.txt already exists

    // a.txt -> b.txt would clobber the existing b.txt.
    bin()
        .current_dir(&dir)
        .args(["--regex", r"s/^a\.txt$/b.txt/", "--commit", "a.txt"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("clobber"));

    let after = list(dir.path());
    assert!(after.contains("a.txt") && after.contains("b.txt"));
}

#[test]
fn collision_suffix_policy_disambiguates_on_disk() {
    let dir = TempDir::new().unwrap();
    touch_all(dir.path(), &["a.txt", "b.txt"]);

    bin()
        .current_dir(&dir)
        .args([
            "--regex",
            r"s/^[ab]\.txt$/merged.txt/",
            "--collision",
            "suffix",
            "--commit",
            "a.txt",
            "b.txt",
        ])
        .assert()
        .success();

    let after = list(dir.path());
    assert!(after.contains("merged.txt"), "got {after:?}");
    assert!(after.contains("merged (1).txt"), "got {after:?}");
}

#[test]
fn recursive_walk_renames_nested() {
    let dir = TempDir::new().unwrap();
    touch_all(
        dir.path(),
        &["top.LOG", "sub/inner.LOG", "sub/deep/leaf.LOG"],
    );

    bin()
        .current_dir(&dir)
        .args(["--lower", "--recursive", "--commit", "."])
        .assert()
        .success();

    // Each .LOG became .log, directory structure preserved.
    assert!(dir.path().join("top.log").is_file());
    assert!(dir.path().join("sub/inner.log").is_file());
    assert!(dir.path().join("sub/deep/leaf.log").is_file());
}

#[test]
fn prefix_suffix_ext_combined() {
    let dir = TempDir::new().unwrap();
    touch_all(dir.path(), &["report.md"]);

    bin()
        .current_dir(&dir)
        .args([
            "--prefix",
            "2026_",
            "--suffix",
            "_v1",
            "--ext",
            "txt",
            "--commit",
            "report.md",
        ])
        .assert()
        .success();

    let after = list(dir.path());
    assert!(after.contains("2026_report_v1.txt"), "got {after:?}");
}

#[test]
fn no_inputs_errors() {
    bin().assert().failure();
}

#[test]
fn swap_via_two_phase_rename() {
    // a -> b and b -> a in one batch: must not lose a file.
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a"), b"AAA").unwrap();
    fs::write(dir.path().join("b"), b"BBB").unwrap();

    bin()
        .current_dir(&dir)
        .args([
            "--regex",
            "s/^a$/TMPX/",
            "--regex",
            "s/^b$/a/",
            "--regex",
            "s/^TMPX$/b/",
            "--commit",
            "a",
            "b",
        ])
        .assert()
        .success();

    // a now holds BBB, b now holds AAA (contents swapped).
    let a = fs::read_to_string(dir.path().join("a")).unwrap();
    let b = fs::read_to_string(dir.path().join("b")).unwrap();
    assert_eq!(a, "BBB");
    assert_eq!(b, "AAA");
}
