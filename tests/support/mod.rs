//! Helpers for the end-to-end tests: copy a fixture into a temporary directory, run the real
//! `magi` binary there, and read what it wrote.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// The date `today()` returns in every run.
pub const TODAY: &str = "2026-07-01";

/// A copy of `tests/fixtures/<case>` in a fresh temporary directory; outputs never touch the repo.
pub struct Fixture {
    dir: TempDir,
}

/// Exit code and captured streams of one `magi` invocation.
#[derive(Debug)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    /// Diagnostic codes (`M012`, ...) in the order they were reported on stderr.
    pub fn codes(&self) -> Vec<String> {
        let mut codes = Vec::new();
        for line in self.stderr.lines() {
            for prefix in ["error[", "warning[", "note["] {
                if let Some(rest) = line.trim_start().strip_prefix(prefix)
                    && let Some(code) = rest.split(']').next()
                {
                    codes.push(code.to_string());
                }
            }
        }
        codes
    }

    #[track_caller]
    pub fn assert_code(&self, expected: i32) -> &Self {
        assert_eq!(
            self.code, expected,
            "unexpected exit code\n--- stdout\n{}\n--- stderr\n{}",
            self.stdout, self.stderr
        );
        self
    }

    #[track_caller]
    pub fn assert_diagnostic(&self, code: &str) -> &Self {
        assert!(
            self.codes().iter().any(|c| c == code),
            "expected diagnostic {code}, got {:?}\n--- stderr\n{}",
            self.codes(),
            self.stderr
        );
        self
    }

    #[track_caller]
    pub fn assert_no_diagnostic(&self, code: &str) -> &Self {
        assert!(
            !self.codes().iter().any(|c| c == code),
            "unexpected diagnostic {code}\n--- stderr\n{}",
            self.stderr
        );
        self
    }

    /// stderr with every whitespace run collapsed to one space, for matching messages that the
    /// renderer wraps across lines.
    pub fn stderr_flat(&self) -> String {
        flatten(&self.stderr)
    }
}

/// Collapse whitespace and the renderer's continuation gutter (`│`) so wrapped messages match.
pub fn flatten(s: &str) -> String {
    s.replace('│', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

impl Fixture {
    pub fn new(case: &str) -> Fixture {
        let src = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(case);
        assert!(src.is_dir(), "no fixture directory {}", src.display());
        let dir = tempfile::Builder::new()
            .prefix(&format!("magi-e2e-{case}-"))
            .tempdir()
            .expect("temp dir");
        copy_dir(&src, dir.path());
        Fixture { dir }
    }

    pub fn file(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// Run `magi <args>` in the fixture directory.
    pub fn magi(&self, args: &[&str]) -> Output {
        self.magi_env(args, &[])
    }

    /// Run `magi <args>` with extra environment variables (only for this child process).
    pub fn magi_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_magi"));
        cmd.args(args)
            .current_dir(self.dir.path())
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env_remove("FORCE_COLOR");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("failed to start magi");
        Output {
            code: out.status.code().expect("magi was killed by a signal"),
            stdout: String::from_utf8(out.stdout).expect("stdout is UTF-8"),
            stderr: String::from_utf8(out.stderr).expect("stderr is UTF-8"),
        }
    }

    /// `magi run --quiet --today 2026-07-01 <program> [extra]`.
    pub fn run(&self, program: &str, extra: &[&str]) -> Output {
        let mut args = vec!["run", "--quiet", "--today", TODAY, program];
        args.extend_from_slice(extra);
        self.magi(&args)
    }

    /// `magi run`, which must succeed (exit 0).
    #[track_caller]
    pub fn run_ok(&self, program: &str) -> Output {
        let out = self.run(program, &[]);
        out.assert_code(0);
        out
    }

    /// `magi check <program>`.
    pub fn check(&self, program: &str) -> Output {
        self.magi(&["check", program])
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.file(rel).exists()
    }

    #[track_caller]
    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.file(rel)).unwrap_or_else(|e| panic!("cannot read {rel}: {e}"))
    }

    #[track_caller]
    pub fn bytes(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.file(rel)).unwrap_or_else(|e| panic!("cannot read {rel}: {e}"))
    }

    /// The whole exported CSV file.
    #[track_caller]
    pub fn csv(&self, rel: &str) -> Csv {
        Csv::parse(&self.read(rel))
    }

    /// Replace the temporary directory in `text` with `[TMP]`, for snapshots.
    pub fn normalize(&self, text: &str) -> String {
        let dir = self.dir.path().display().to_string();
        let canonical = std::fs::canonicalize(self.dir.path())
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| dir.clone());
        text.replace(&canonical, "[TMP]").replace(&dir, "[TMP]")
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create dir");
    for entry in std::fs::read_dir(from).expect("read fixture dir") {
        let entry = entry.expect("dir entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
}

/// A parsed CSV export (RFC 4180 quoting; MAGI writes empty fields for null).
#[derive(Debug, Clone, PartialEq)]
pub struct Csv {
    pub header: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl Csv {
    pub fn parse(text: &str) -> Csv {
        let mut records = Vec::new();
        let mut record = Vec::new();
        let mut field = String::new();
        let mut chars = text.chars().peekable();
        let mut quoted = false;
        let mut started = false;
        while let Some(c) = chars.next() {
            started = true;
            if quoted {
                match c {
                    '"' if chars.peek() == Some(&'"') => {
                        chars.next();
                        field.push('"');
                    }
                    '"' => quoted = false,
                    _ => field.push(c),
                }
                continue;
            }
            match c {
                '"' => quoted = true,
                ',' => record.push(std::mem::take(&mut field)),
                '\r' => {}
                '\n' => {
                    record.push(std::mem::take(&mut field));
                    records.push(std::mem::take(&mut record));
                    started = false;
                }
                _ => field.push(c),
            }
        }
        if started {
            record.push(field);
            records.push(record);
        }
        let mut it = records.into_iter();
        let header = it.next().expect("CSV has a header");
        Csv {
            header,
            rows: it.collect(),
        }
    }

    #[track_caller]
    pub fn col(&self, name: &str) -> usize {
        self.header
            .iter()
            .position(|h| h == name)
            .unwrap_or_else(|| panic!("no column `{name}` in {:?}", self.header))
    }

    /// Each row reduced to `columns`, joined with `,` (null is the empty string).
    #[track_caller]
    pub fn project(&self, columns: &[&str]) -> Vec<String> {
        let idx: Vec<usize> = columns.iter().map(|c| self.col(c)).collect();
        self.rows
            .iter()
            .map(|r| {
                idx.iter()
                    .map(|&i| r[i].as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect()
    }

    /// Values of one column.
    #[track_caller]
    pub fn column(&self, name: &str) -> Vec<String> {
        let i = self.col(name);
        self.rows.iter().map(|r| r[i].clone()).collect()
    }
}

/// Build a `Vec<String>` from string literals.
#[macro_export]
macro_rules! lines {
    ($($s:expr),* $(,)?) => { vec![$(String::from($s)),*] as Vec<String> };
}
