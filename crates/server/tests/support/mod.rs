#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("repo root path")
}

pub fn read_repo_file(relative: impl AsRef<Path>) -> String {
    let path = repo_root().join(relative);
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {}", path.display(), err))
}

pub fn read_repo_files(relatives: &[&str]) -> String {
    let mut out = String::new();
    for relative in relatives {
        out.push_str("\n// FILE: ");
        out.push_str(relative);
        out.push('\n');
        out.push_str(&read_repo_file(relative));
        out.push('\n');
    }
    out
}

pub fn assert_contains_all(haystack: &str, needles: &[&str]) {
    for needle in needles {
        assert!(
            haystack.contains(needle),
            "expected to find `{}` in source text",
            needle
        );
    }
}

pub fn assert_contains_in_order(haystack: &str, needles: &[&str]) {
    let mut offset = 0usize;
    for needle in needles {
        let search = &haystack[offset..];
        let Some(index) = search.find(needle) else {
            panic!("expected to find `{}` after byte offset {}", needle, offset);
        };
        offset += index + needle.len();
    }
}

pub fn requirement_lines(prefix: &str) -> Vec<String> {
    let marker = format!("- {}", prefix);
    read_repo_file("REQUIREMENTS.md")
        .lines()
        .filter(|line| line.starts_with(&marker))
        .map(|line| line.to_string())
        .collect()
}
