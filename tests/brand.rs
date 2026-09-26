//! Fails if a previous product name appears outside the documented exceptions.
//!
//! The names are assembled in `tokens` so this file does not contain them.

use std::fs;
use std::path::{Path, PathBuf};

fn tokens() -> Vec<String> {
    ["zap", "fasts", "fast"]
        .into_iter()
        .zip(["fast", "app", "whatsapp"])
        .map(|(left, right)| format!("{left}{right}"))
        .chain(std::iter::once(format!("{}{}", "zap", "ext")))
        .collect()
}

/// A whole file may keep a previous name.
const ALLOWED_FILES: &[(&str, &str)] = &[
    (
        "src/migrate.rs",
        "the one-time move reads the previous directories and keyring service",
    ),
    ("CHANGELOG.md", "older entries stay as published"),
    ("LICENSE", "copyright notices"),
];

/// A directory of recorded pages may keep a previous name.
const ALLOWED_PREFIXES: &[(&str, &str)] = &[(
    "docs/assets/benchmarks/",
    "measurements recorded on 2026-09-15 and the existing site label",
)];

fn exception(line: &str) -> bool {
    let lower = line.to_lowercase();
    let product = format!("{}{}", "zap", "fast");
    let needles = [
        format!("{product}-extra"),
        format!("github.com/crmne/{product}"),
        format!("crmne/{product}"),
        format!("{product}.rocks"),
        format!("fork of {product}"),
        format!("upstream {product}"),
        format!("based on {product}"),
        format!("adapted from upstream {product}"),
        format!("ported from upstream {product}"),
        format!("{product} 0.16.2"),
        format!("community fork of {product}"),
        format!("overlap-add used by {product}"),
    ];
    needles.iter().any(|needle| lower.contains(needle))
        || (lower.contains(&product) && lower.contains("0.16.2"))
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn relative(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn skipped(relative: &str) -> bool {
    ALLOWED_FILES.iter().any(|(file, _)| relative == *file)
        || ALLOWED_PREFIXES
            .iter()
            .any(|(prefix, _)| relative.starts_with(prefix))
}

fn walk(dir: &Path, root: &Path, found: &mut Vec<String>, names: &[String]) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == ".git" || name == "target" {
            continue;
        }
        if path.is_dir() {
            walk(&path, root, found, names);
            continue;
        }
        if matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("png" | "ico" | "svg" | "pdf" | "mp3" | "woff" | "woff2")
        ) {
            continue;
        }
        let relative = relative(&path, root);
        if skipped(&relative) {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        for (number, line) in text.lines().enumerate() {
            let lower = line.to_lowercase();
            if !names.iter().any(|token| lower.contains(token)) || exception(line) {
                continue;
            }
            found.push(format!("{relative}:{}: {}", number + 1, line.trim()));
        }
    }
}

#[test]
fn previous_product_names_stay_inside_the_exceptions() {
    let root = repo_root();
    let names = tokens();
    let mut found = Vec::new();
    walk(&root, &root, &mut found, &names);
    assert!(
        found.is_empty(),
        "a previous product name is outside the exceptions:\n{}\n\nallowed files:\n{}",
        found.join("\n"),
        ALLOWED_FILES
            .iter()
            .chain(ALLOWED_PREFIXES.iter())
            .map(|(path, reason)| format!("{path} — {reason}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
