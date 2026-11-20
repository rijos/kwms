//! Keeps `unsafe` code and `docs/src/unsafe-audit.md` in sync.
//!
//! Fails if:
//! * `unsafe` appears anywhere in `src/` other than `src/ffi.rs`;
//! * an `unsafe` in `src/ffi.rs` lacks an `// UNSAFE-AUDIT: <id>` tag in the
//!   comment block directly above it;
//! * an id is used twice;
//! * a tagged id has no `## <id> —` entry in the audit document, or the
//!   document has an entry no code uses (stale).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const FFI_MODULE: &str = "src/ffi.rs";
const AUDIT_DOC: &str = "docs/src/unsafe-audit.md";

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The code part of a line: comments and string literals removed, so only
/// real `unsafe` tokens are counted.
fn code_of(line: &str) -> String {
    if line.trim_start().starts_with("//") {
        return String::new();
    }
    let mut out = String::new();
    let mut in_string = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (in_string, c) {
            (false, '/') if chars.peek() == Some(&'/') => break,
            (false, '"') => in_string = true,
            (true, '\\') => {
                chars.next();
            }
            (true, '"') => in_string = false,
            (false, c) => out.push(c),
            (true, _) => {}
        }
    }
    out
}

fn has_unsafe_token(code: &str) -> bool {
    code.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|word| word == "unsafe")
}

/// The id in the `// UNSAFE-AUDIT: <id>` tag within the comment block that
/// directly precedes line `idx`.
fn audit_tag(lines: &[&str], idx: usize) -> Option<String> {
    lines[..idx]
        .iter()
        .rev()
        .take_while(|l| l.trim_start().starts_with("//"))
        .find_map(|l| l.split("UNSAFE-AUDIT:").nth(1))
        .map(|rest| rest.split_whitespace().next().unwrap_or("").to_string())
}

#[test]
fn every_unsafe_block_is_in_ffi_and_documented() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);

    let mut tags = Vec::new();
    for file in &files {
        let rel = file.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
        let text = fs::read_to_string(file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            if !has_unsafe_token(&code_of(line)) {
                continue;
            }
            assert_eq!(
                rel,
                FFI_MODULE,
                "`unsafe` outside {FFI_MODULE}: {rel}:{} `{}`",
                idx + 1,
                line.trim()
            );
            let id = audit_tag(&lines, idx)
                .unwrap_or_else(|| panic!("{rel}:{}: unsafe block without an `// UNSAFE-AUDIT: <id>` tag", idx + 1));
            tags.push(id);
        }
    }

    let unique: BTreeSet<_> = tags.iter().cloned().collect();
    assert_eq!(unique.len(), tags.len(), "duplicate UNSAFE-AUDIT ids: {tags:?}");
    assert!(!unique.is_empty(), "no unsafe blocks found — is the scanner broken?");

    let doc = fs::read_to_string(root.join(AUDIT_DOC)).unwrap();
    let documented: BTreeSet<String> = doc
        .lines()
        .filter_map(|l| l.strip_prefix("## "))
        .filter_map(|l| l.split(" — ").next())
        .filter(|id| id.len() >= 2 && id[1..].bytes().all(|b| b.is_ascii_digit()))
        .map(str::to_string)
        .collect();

    let undocumented: Vec<_> = unique.difference(&documented).collect();
    let stale: Vec<_> = documented.difference(&unique).collect();
    assert!(
        undocumented.is_empty(),
        "unsafe blocks missing from {AUDIT_DOC}: {undocumented:?}"
    );
    assert!(stale.is_empty(), "{AUDIT_DOC} documents ids no code uses: {stale:?}");
}

#[test]
fn scanner_sees_through_comments_and_strings() {
    assert!(has_unsafe_token(&code_of("    let x = unsafe { f() };")));
    assert!(!has_unsafe_token(&code_of("    // unsafe { f() }")));
    assert!(!has_unsafe_token(&code_of(r#"    let s = "unsafe";"#)));
    assert!(!has_unsafe_token(&code_of("#![deny(unsafe_code)]")));
    assert!(has_unsafe_token(&code_of(r#"    let s = "a\"b"; unsafe { f() }"#)));
}
