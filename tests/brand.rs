//! Every string a user can read has to say Vespera.
//!
//! The check reads quoted literals only, so comments and prose that keep the
//! old name on purpose are not flagged: the attribution to ZapFast, and the
//! notes about the fork. A literal that is genuinely allowed to keep the old
//! name is listed in ALLOWED together with the reason.

/// The user-visible surfaces: window titles, tray, login, dialogs,
/// notifications, error text and the about text.
const COVERED: &[(&str, &str)] = &[
    ("src/main.rs", include_str!("../src/main.rs")),
    ("src/app.rs", include_str!("../src/app.rs")),
    ("src/archive/encryption.rs", include_str!("../src/archive/encryption.rs")),
    ("src/backend/worker.rs", include_str!("../src/backend/worker.rs")),
    ("src/demo.rs", include_str!("../src/demo.rs")),
    ("src/macos.rs", include_str!("../src/macos.rs")),
    ("src/notify.rs", include_str!("../src/notify.rs")),
    ("src/notify/windows.rs", include_str!("../src/notify/windows.rs")),
    ("src/tray.rs", include_str!("../src/tray.rs")),
    ("src/tray_native.rs", include_str!("../src/tray_native.rs")),
    ("src/ui/conversation.rs", include_str!("../src/ui/conversation.rs")),
    ("src/ui/dialogs.rs", include_str!("../src/ui/dialogs.rs")),
    ("src/ui/keys.rs", include_str!("../src/ui/keys.rs")),
    ("src/ui/login.rs", include_str!("../src/ui/login.rs")),
    ("src/ui/mod.rs", include_str!("../src/ui/mod.rs")),
    ("src/ui/settings.rs", include_str!("../src/ui/settings.rs")),
    ("src/ui/update.rs", include_str!("../src/ui/update.rs")),
    ("src/winfocus.rs", include_str!("../src/winfocus.rs")),
];

/// Quoted literals that keep an old name on purpose.
const ALLOWED: &[(&str, &str, &str)] = &[
    (
        "src/backend/worker.rs",
        "with_os(\"ZapExt\")",
        "pairing device name; renaming it changes what WhatsApp sees, not a label",
    ),
];

/// Double-quoted runs on a single line. Escapes inside a literal are not
/// tracked; none of the covered names sit next to an escaped quote.
fn quoted(line: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut open: Option<usize> = None;
    for (index, byte) in line.bytes().enumerate() {
        if byte != b'"' {
            continue;
        }
        match open.take() {
            None => open = Some(index + 1),
            Some(start) => parts.push(&line[start..index]),
        }
    }
    parts
}

#[test]
fn no_user_visible_string_still_says_zapext() {
    let mut found = Vec::new();
    for (name, text) in COVERED {
        for (number, line) in text.lines().enumerate() {
            if !quoted(line).iter().any(|part| part.contains("ZapExt")) {
                continue;
            }
            if ALLOWED
                .iter()
                .any(|(file, needle, _)| file == name && line.contains(needle))
            {
                continue;
            }
            found.push(format!("{name}:{}: {}", number + 1, line.trim()));
        }
    }
    assert!(
        found.is_empty(),
        "user-visible ZapExt left behind:\n{}",
        found.join("\n")
    );
}
