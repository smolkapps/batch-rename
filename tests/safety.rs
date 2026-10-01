//! Safety regressions use only generated files in disposable directories.

use assert_cmd::Command;
use batch_rename::{plan, Op, Transforms};
use predicates::prelude::*;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

fn bin() -> Command {
    Command::cargo_bin("batch-rename").unwrap()
}

#[test]
fn traversal_template_cannot_overwrite_a_file_outside_the_source_parent() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("in")).unwrap();
    fs::write(dir.path().join("in/a.txt"), b"SOURCE").unwrap();
    fs::write(dir.path().join("keep.txt"), b"KEEP").unwrap();

    bin()
        .current_dir(&dir)
        .args(["--template", "../keep.txt", "--commit", "in/a.txt"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("file name"));

    assert_eq!(fs::read(dir.path().join("in/a.txt")).unwrap(), b"SOURCE");
    assert_eq!(fs::read(dir.path().join("keep.txt")).unwrap(), b"KEEP");
}

#[test]
fn absolute_template_cannot_overwrite_an_existing_file() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("in")).unwrap();
    let source = dir.path().join("in/a.txt");
    let keep = dir.path().join("keep.txt");
    fs::write(&source, b"SOURCE").unwrap();
    fs::write(&keep, b"KEEP").unwrap();

    bin()
        .arg("--template")
        .arg(&keep)
        .arg("--commit")
        .arg(&source)
        .assert()
        .failure();

    assert_eq!(fs::read(source).unwrap(), b"SOURCE");
    assert_eq!(fs::read(keep).unwrap(), b"KEEP");
}

#[test]
fn nested_template_is_refused_without_creating_directories() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.txt"), b"SOURCE").unwrap();

    bin()
        .current_dir(&dir)
        .args(["--template", "nested/new.txt", "--commit", "a.txt"])
        .assert()
        .failure();

    assert_eq!(fs::read(dir.path().join("a.txt")).unwrap(), b"SOURCE");
    assert!(!dir.path().join("nested").exists());
}

#[test]
fn planner_rejects_non_basename_outputs_from_templates_and_operations() {
    for name in [
        ".",
        "..",
        "../keep.txt",
        "sub/file.txt",
        "./file.txt",
        "file.txt/",
        "a\\b",
        "a\0b",
        "C:keep.txt",
        "file:stream",
    ] {
        let transforms = Transforms {
            template: Some(name.into()),
            ..Default::default()
        };
        assert!(
            plan(&[PathBuf::from("in/a.txt")], &transforms, &HashSet::new()).is_err(),
            "accepted {name:?}"
        );
    }

    for op in [
        Op::Prefix("../".into()),
        Op::Suffix("/new".into()),
        Op::Replace {
            from: "a.txt".into(),
            to: "../keep.txt".into(),
        },
        Op::Ext("txt/keep".into()),
    ] {
        let transforms = Transforms {
            ops: vec![op],
            ..Default::default()
        };
        assert!(plan(&[PathBuf::from("in/a.txt")], &transforms, &HashSet::new()).is_err());
    }
}

#[test]
fn missing_explicit_source_refuses_the_entire_batch() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.txt"), b"SOURCE").unwrap();

    bin()
        .current_dir(&dir)
        .args(["--template", "new.txt", "--commit", "a.txt", "missing.txt"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no such file"));

    assert_eq!(fs::read(dir.path().join("a.txt")).unwrap(), b"SOURCE");
    assert!(!dir.path().join("new.txt").exists());
}

#[test]
fn planner_rejects_repeated_source_paths() {
    let transforms = Transforms {
        seq: true,
        start: 1,
        ..Default::default()
    };
    let result = plan(
        &[PathBuf::from("a.txt"), PathBuf::from("a.txt")],
        &transforms,
        &HashSet::new(),
    );
    assert!(
        result.is_err(),
        "one source must not be assigned two destinations"
    );
}

#[test]
fn source_aliases_are_numbered_and_renamed_once() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("a.txt"), b"SOURCE").unwrap();

    bin()
        .current_dir(&dir)
        .args([
            "--template",
            "file-{n}.txt",
            "--commit",
            "a.txt",
            "./a.txt",
            "sub/../a.txt",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Done: 1 file(s) renamed."));

    assert_eq!(fs::read(dir.path().join("file-1.txt")).unwrap(), b"SOURCE");
    assert!(!dir.path().join("file-2.txt").exists());
    assert!(!dir.path().join("file-3.txt").exists());
}

#[test]
fn existing_directory_is_a_collision() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.txt"), b"SOURCE").unwrap();
    fs::create_dir(dir.path().join("taken.txt")).unwrap();

    bin()
        .current_dir(&dir)
        .args(["--template", "taken.txt", "--commit", "./a.txt"])
        .assert()
        .failure()
        .code(2);

    assert_eq!(fs::read(dir.path().join("a.txt")).unwrap(), b"SOURCE");
    assert!(dir.path().join("taken.txt").is_dir());
}

#[cfg(unix)]
#[test]
fn dangling_symlink_target_is_preserved() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.txt"), b"SOURCE").unwrap();
    std::os::unix::fs::symlink("missing", dir.path().join("keep.txt")).unwrap();

    bin()
        .current_dir(&dir)
        .args(["--template", "keep.txt", "--commit", "a.txt"])
        .assert()
        .failure()
        .code(2);

    assert_eq!(fs::read(dir.path().join("a.txt")).unwrap(), b"SOURCE");
    assert_eq!(
        fs::read_link(dir.path().join("keep.txt")).unwrap(),
        PathBuf::from("missing")
    );
}

#[test]
fn rejected_collision_can_be_retried_without_changing_contents() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.txt"), b"SOURCE").unwrap();
    fs::write(dir.path().join("keep.txt"), b"KEEP").unwrap();

    for _ in 0..2 {
        bin()
            .current_dir(&dir)
            .args(["--template", "keep.txt", "--commit", "a.txt"])
            .assert()
            .failure()
            .code(2);
        assert_eq!(fs::read(dir.path().join("a.txt")).unwrap(), b"SOURCE");
        assert_eq!(fs::read(dir.path().join("keep.txt")).unwrap(), b"KEEP");
    }

    bin()
        .current_dir(&dir)
        .args(["--template", "new.txt", "--commit", "a.txt"])
        .assert()
        .success();
    assert_eq!(fs::read(dir.path().join("new.txt")).unwrap(), b"SOURCE");
    assert_eq!(fs::read(dir.path().join("keep.txt")).unwrap(), b"KEEP");
}
