//! SQLCipher archive storage and automatic OS-keyring unlock.

use std::{fs, io::Read, path::Path};

use anyhow::{Context, Result, ensure};
use keyring_core::api::CredentialStoreApi;
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

const HEADER: &[u8; 16] = b"SQLite format 3\0";

fn plaintext(path: &Path) -> Result<bool> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    if file.metadata()?.len() == 0 {
        return Ok(true);
    }
    let mut header = [0; 16];
    file.read_exact(&mut header)?;
    Ok(&header == HEADER)
}

pub(super) fn key_for(path: &Path) -> Result<Zeroizing<[u8; 32]>> {
    let parent = path.parent().context("Archive has no parent directory")?;
    fs::create_dir_all(parent)?;
    // Separate profiles must not overwrite each other's keys. The credential
    // label contains a digest, never a user path, phone number or message data.
    let canonical = parent.canonicalize()?;
    let identity = identity_for_canonical_bytes(canonical.as_os_str().as_encoded_bytes());
    let entry = credential(crate::migrate::KEYRING_SERVICE, &identity)?;
    key_from_entry(path, &entry)
}

fn identity_for_canonical_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!(
        "archive-{}",
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

/// Written into the state directory before it is renamed. The bytes are the
/// canonical path the previous key was stored under.
const ORIGIN_FILE: &str = "vespera-key-origin.bin";

/// Records the canonical state path so the archive key can be found after the
/// directory moves. A note already on disk is kept: it is the original path.
pub(crate) fn note_key_origin(state: &Path) -> std::io::Result<()> {
    let note = state.join(ORIGIN_FILE);
    if note.is_file() {
        return Ok(());
    }
    let canonical = state.canonicalize()?;
    fs::write(note, canonical.as_os_str().as_encoded_bytes())
}

/// Copies the archive key from the previous keyring service to the current one.
///
/// The previous entry is removed only after the archive opens with the new
/// entry. A second call with no note does nothing. Any failure leaves the
/// previous entry in place.
pub(crate) fn finish_key_migration(state: &Path) -> Result<()> {
    let note = state.join(ORIGIN_FILE);
    if !note.is_file() {
        return Ok(());
    }
    let old_bytes = fs::read(&note)
        .context("Could not read the saved archive location. Nothing was deleted.")?;
    if old_bytes.is_empty() {
        anyhow::bail!("The saved archive location is empty. Nothing was deleted.");
    }
    let new_path = state
        .canonicalize()
        .context("Could not resolve the new archive folder. Nothing was deleted.")?;
    let old_identity = identity_for_canonical_bytes(&old_bytes);
    let new_identity = identity_for_canonical_bytes(new_path.as_os_str().as_encoded_bytes());
    let old_entry = credential(crate::migrate::LEGACY_KEYRING_SERVICE, &old_identity)?;
    let new_entry = credential(crate::migrate::KEYRING_SERVICE, &new_identity)?;
    let secret = match old_entry.get_secret() {
        Ok(secret) => Zeroizing::new(secret),
        Err(keyring_core::Error::NoEntry) => {
            if let Ok(existing) = new_entry.get_secret()
                && existing.len() == 32
            {
                let mut key = Zeroizing::new([0u8; 32]);
                key.copy_from_slice(&existing);
                if archive_opens(state, &key).is_ok() {
                    fs::remove_file(&note).context(
                        "The archive key is already in place, but the migration note remains. Restart Vespera.",
                    )?;
                    return Ok(());
                }
            }
            if archive_needs_key(state)? {
                anyhow::bail!(
                    "The message archive is encrypted, but its previous key is not in the keyring. Nothing was deleted."
                );
            }
            fs::remove_file(&note).context(
                "There was no previous archive key, but the migration note could not be removed.",
            )?;
            return Ok(());
        }
        Err(error) => {
            return Err(error)
                .context("Could not read the previous archive key. Nothing was deleted.");
        }
    };
    if secret.len() != 32 {
        anyhow::bail!("The previous archive key is invalid. Nothing was deleted.");
    }
    new_entry.set_secret(&secret).context(
        "Could not store the archive key under the new name. The previous key was kept.",
    )?;
    let saved = Zeroizing::new(
        new_entry
            .get_secret()
            .context("Could not read the new archive key back. The previous key was kept.")?,
    );
    if saved.as_slice() != secret.as_slice() {
        anyhow::bail!(
            "The new archive key does not match the previous one. The previous key was kept."
        );
    }
    let mut key = Zeroizing::new([0u8; 32]);
    key.copy_from_slice(&secret);
    archive_opens(state, &key)
        .context("The archive did not open with the moved key. The previous key was kept.")?;
    old_entry.delete_credential().context(
        "The archive opened with the new key, but the previous keyring entry is still there. Restart Vespera to remove it.",
    )?;
    fs::remove_file(&note).context(
        "The key was moved, but the migration note could not be removed. Restart Vespera.",
    )?;
    Ok(())
}

fn archive_needs_key(state: &Path) -> Result<bool> {
    let path = state.join("archive.db");
    if !path.is_file() {
        return Ok(false);
    }
    Ok(fs::metadata(&path)?.len() > 0 && !plaintext(&path)?)
}

fn archive_opens(state: &Path, key: &[u8; 32]) -> Result<()> {
    let path = state.join("archive.db");
    if !path.is_file() || fs::metadata(&path)?.len() == 0 || plaintext(&path)? {
        return Ok(());
    }
    drop(keyed(&path, key)?);
    Ok(())
}

fn credential(service: &str, identity: &str) -> Result<keyring_core::Entry> {
    // Tests share one mock so a dropped archive can be opened again without
    // a desktop secret service. Shipping builds keep the OS keyring.
    #[cfg(test)]
    {
        return test_store()?
            .build(service, identity, None)
            .context("The OS keyring could not open Vespera's archive key");
    }
    #[cfg(not(test))]
    {
        #[cfg(target_os = "linux")]
        let store = zbus_secret_service_keyring_store::Store::new();
        #[cfg(target_os = "macos")]
        let store = apple_native_keyring_store::keychain::Store::new();
        #[cfg(windows)]
        let store = windows_native_keyring_store::Store::new();
        let store = store.context("Unlock your OS keyring and restart Vespera")?;
        store
            .build(service, identity, None)
            .context("The OS keyring could not open Vespera's archive key")
    }
}

#[cfg(test)]
fn test_store() -> Result<std::sync::Arc<keyring_core::mock::Store>> {
    use std::sync::{Arc, OnceLock};
    static STORE: OnceLock<Arc<keyring_core::mock::Store>> = OnceLock::new();
    Ok(STORE
        .get_or_init(|| keyring_core::mock::Store::new().expect("mock keyring"))
        .clone())
}

fn key_from_entry(path: &Path, entry: &keyring_core::Entry) -> Result<Zeroizing<[u8; 32]>> {
    match entry.get_secret() {
        Ok(secret) => {
            let secret = Zeroizing::new(secret);
            ensure!(
                secret.len() == 32,
                "The archive key in the OS keyring is invalid"
            );
            let mut key = Zeroizing::new([0; 32]);
            key.copy_from_slice(&secret);
            Ok(key)
        }
        Err(keyring_core::Error::NoEntry) => {
            ensure!(
                plaintext(path)?,
                "The archive is encrypted but its OS keyring key is missing. Restore the original keyring; the archive has not been changed"
            );
            let mut key = Zeroizing::new([0; 32]);
            getrandom::fill(key.as_mut()).context("Could not generate an archive key")?;
            entry
                .set_secret(key.as_ref())
                .context("Could not save the archive key in the OS keyring")?;
            // Read back before touching the only copy of the message history.
            let saved = Zeroizing::new(
                entry
                    .get_secret()
                    .context("Could not verify the saved archive key")?,
            );
            ensure!(
                saved.as_slice() == key.as_ref(),
                "The OS keyring did not retain the archive key"
            );
            Ok(key)
        }
        Err(error) => Err(error).context("Unlock your OS keyring and restart Vespera"),
    }
}

fn key_literal(key: &[u8; 32]) -> Zeroizing<String> {
    use std::fmt::Write;
    let mut literal = Zeroizing::new(String::with_capacity(67));
    literal.push_str("x'");
    for byte in key {
        write!(&mut *literal, "{byte:02x}").expect("writing to a String");
    }
    literal.push('\'');
    literal
}

fn keyed(path: &Path, key: &[u8; 32]) -> Result<Connection> {
    let connection = Connection::open(path)?;
    connection.pragma_update(None, "key", &*key_literal(key))?;
    let version: String = connection
        .query_row("PRAGMA cipher_version", [], |row| row.get(0))
        .context("This build does not support encrypted archives")?;
    ensure!(
        !version.is_empty(),
        "This build does not support encrypted archives"
    );
    // PRAGMA key alone does not verify a key. Read a page before any migration.
    connection
        .query_row("SELECT count(*) FROM sqlite_master", [], |row| {
            row.get::<_, i64>(0)
        })
        .context("The archive could not be unlocked with its OS keyring key")?;
    connection.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(connection)
}

fn private_file(path: &Path) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.sync_all()?;
    Ok(())
}

pub(super) fn open(path: &Path, key: &[u8; 32]) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let staging = path.with_extension("db.encrypting");
    if !path.exists() {
        private_file(path)?;
    } else if fs::metadata(path)?.len() > 0 && plaintext(path)? {
        // Keep the original authoritative until the exported copy is complete.
        // Switching away from WAL folds in committed pages and removes sidecars
        // before replacing the main file. An interruption leaves the original
        // usable, with at most an encrypted staging file to discard next time.
        let source = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        let mode: String =
            source.query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))?;
        ensure!(
            mode == "delete",
            "Close other programs using the archive before migrating it"
        );
        source.pragma_update(None, "temp_store", "MEMORY")?;
        if staging.try_exists()? {
            fs::remove_file(&staging)?;
        }
        private_file(&staging)?;
        source.execute(
            "ATTACH DATABASE ?1 AS encrypted KEY ?2",
            rusqlite::params![
                staging.to_str().context("Archive path is not UTF-8")?,
                &*key_literal(key)
            ],
        )?;
        source.query_row("SELECT sqlcipher_export('encrypted')", [], |_| Ok(()))?;
        // sqlcipher_export does not copy SQLite's application/user version.
        for pragma in ["user_version", "application_id"] {
            let value: i64 = source.pragma_query_value(None, pragma, |row| row.get(0))?;
            source.pragma_update(Some("encrypted"), pragma, value)?;
        }
        source.execute_batch("DETACH DATABASE encrypted")?;
        let verified = keyed(&staging, key)?;
        let integrity: String =
            verified.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        ensure!(
            integrity == "ok",
            "The encrypted archive failed its integrity check"
        );
        drop(verified);
        drop(source);
        // FlushFileBuffers on Windows requires a writable handle.
        fs::OpenOptions::new()
            .write(true)
            .open(&staging)?
            .sync_all()?;
        fs::rename(&staging, path)
            .context("Could not replace the archive with its encrypted copy")?;
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
    }
    let connection = keyed(path, key)?;
    if staging.try_exists()? {
        fs::remove_file(staging)?;
    }
    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn read_secret(connection: &Connection) -> String {
        connection
            .query_row("SELECT value FROM secrets", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn keyring_unlock_reuses_keys_and_never_replaces_a_missing_key() {
        let directory = directory();
        let path = directory.path().join("archive.db");
        let store = keyring_core::mock::Store::new().unwrap();
        let entry = store.build("zapfast-test", "archive", None).unwrap();
        let key = key_from_entry(&path, &entry).unwrap();
        assert_eq!(*key, *key_from_entry(&path, &entry).unwrap());
        let connection = open(&path, &key).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE example(value TEXT); INSERT INTO example VALUES ('fixture');",
            )
            .unwrap();
        drop(connection);
        let original = fs::read(&path).unwrap();
        entry.delete_credential().unwrap();
        assert!(
            key_from_entry(&path, &entry)
                .unwrap_err()
                .to_string()
                .contains("missing")
        );
        assert!(matches!(
            entry.get_secret(),
            Err(keyring_core::Error::NoEntry)
        ));
        assert_eq!(fs::read(&path).unwrap(), original);
        entry.set_secret(&[1; 12]).unwrap();
        assert!(
            key_from_entry(&path, &entry)
                .unwrap_err()
                .to_string()
                .contains("invalid")
        );
        let mock = entry
            .as_any()
            .downcast_ref::<keyring_core::mock::Cred>()
            .unwrap();
        mock.set_error(keyring_core::Error::PlatformFailure(Box::new(
            std::io::Error::other("locked"),
        )));
        assert!(
            key_from_entry(&path, &entry)
                .unwrap_err()
                .to_string()
                .contains("Unlock")
        );
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn encrypted_database_and_wal_reject_missing_or_wrong_keys() {
        let directory = directory();
        let path = directory.path().join("archive.db");
        let connection = open(&path, &[7; 32]).unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE secrets (value TEXT); INSERT INTO secrets VALUES ('private archive marker');").unwrap();
        for file in [&path, &path.with_extension("db-wal")] {
            let bytes = fs::read(file).unwrap();
            assert!(
                !bytes
                    .windows(22)
                    .any(|bytes| bytes == b"private archive marker")
            );
        }
        assert!(!plaintext(&path).unwrap());
        assert!(open(&path, &[8; 32]).is_err());
        assert!(
            Connection::open(&path)
                .unwrap()
                .query_row("SELECT count(*) FROM sqlite_master", [], |row| row
                    .get::<_, i64>(0))
                .is_err()
        );
        drop(connection);
        assert_eq!(
            read_secret(&open(&path, &[7; 32]).unwrap()),
            "private archive marker"
        );
    }

    #[test]
    fn migration_preserves_wal_data_schema_and_version() {
        let directory = directory();
        let path = directory.path().join("archive.db");
        let source = Connection::open(&path).unwrap();
        source.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; PRAGMA user_version=9; PRAGMA application_id=42; CREATE TABLE secrets (value TEXT); CREATE INDEX secret_values ON secrets(value); INSERT INTO secrets VALUES ('from the phone');").unwrap();
        // Leave the committed WAL on disk as after an interrupted old process.
        source
            .set_db_config(
                rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
                true,
            )
            .unwrap();
        drop(source);
        assert!(path.with_extension("db-wal").exists());
        fs::write(path.with_extension("db.encrypting"), b"interrupted export").unwrap();
        let encrypted = open(&path, &[9; 32]).unwrap();
        assert_eq!(read_secret(&encrypted), "from the phone");
        assert_eq!(
            encrypted
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            9
        );
        assert_eq!(
            encrypted
                .pragma_query_value(None, "application_id", |row| row.get::<_, i64>(0))
                .unwrap(),
            42
        );
        assert_eq!(
            encrypted
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name='secret_values'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert!(!plaintext(&path).unwrap());
        assert!(!path.with_extension("db.encrypting").exists());
        assert!(!path.with_extension("db-wal").exists());
    }

    #[test]
    fn a_failed_migration_leaves_the_original_readable() {
        let directory = directory();
        let path = directory.path().join("archive.db");
        let source = Connection::open(&path).unwrap();
        source
            .execute_batch(
                "CREATE TABLE secrets(value TEXT); INSERT INTO secrets VALUES ('keep me');",
            )
            .unwrap();
        drop(source);
        // A blocked staging path simulates a filesystem failure before replace.
        fs::create_dir(path.with_extension("db.encrypting")).unwrap();
        assert!(open(&path, &[9; 32]).is_err());
        assert!(plaintext(&path).unwrap());
        assert_eq!(read_secret(&Connection::open(&path).unwrap()), "keep me");
        fs::remove_dir(path.with_extension("db.encrypting")).unwrap();
        assert_eq!(read_secret(&open(&path, &[9; 32]).unwrap()), "keep me");
    }

    fn store_key(service: &str, state: &Path, key: &[u8; 32]) {
        let canonical = state.canonicalize().unwrap();
        let identity = identity_for_canonical_bytes(canonical.as_os_str().as_encoded_bytes());
        test_store()
            .unwrap()
            .build(service, &identity, None)
            .unwrap()
            .set_secret(key)
            .unwrap();
    }

    fn entry(service: &str, state: &Path) -> keyring_core::Entry {
        let canonical = state.canonicalize().unwrap();
        let identity = identity_for_canonical_bytes(canonical.as_os_str().as_encoded_bytes());
        test_store()
            .unwrap()
            .build(service, &identity, None)
            .unwrap()
    }

    /// Encrypts `archive.db` with `key` inside `state`.
    fn encrypted_archive(state: &Path, key: &[u8; 32]) {
        let path = state.join("archive.db");
        let source = Connection::open(&path).unwrap();
        source
            .execute_batch(
                "CREATE TABLE secrets(value TEXT); INSERT INTO secrets VALUES ('moved history');",
            )
            .unwrap();
        drop(source);
        drop(open(&path, key).unwrap());
    }

    #[test]
    fn the_moved_key_opens_the_archive_and_a_second_run_does_nothing() {
        let root = directory();
        let old = root.path().join("old-state");
        let new = root.path().join("new-state");
        fs::create_dir_all(&old).unwrap();
        let key = [4u8; 32];
        encrypted_archive(&old, &key);
        store_key(crate::migrate::LEGACY_KEYRING_SERVICE, &old, &key);
        note_key_origin(&old).unwrap();
        let old_canonical = fs::read(old.join(ORIGIN_FILE)).unwrap();
        fs::rename(&old, &new).unwrap();
        finish_key_migration(&new).unwrap();
        finish_key_migration(&new).unwrap();
        assert!(!new.join(ORIGIN_FILE).exists());
        let old_identity = identity_for_canonical_bytes(&old_canonical);
        let old_entry = test_store()
            .unwrap()
            .build(crate::migrate::LEGACY_KEYRING_SERVICE, &old_identity, None)
            .unwrap();
        assert!(matches!(
            old_entry.get_secret(),
            Err(keyring_core::Error::NoEntry)
        ));
        let opened = key_for(&new.join("archive.db")).unwrap();
        assert_eq!(opened.as_slice(), &key);
        assert_eq!(
            read_secret(&open(&new.join("archive.db"), &opened).unwrap()),
            "moved history"
        );
    }

    #[test]
    fn a_failed_key_move_keeps_the_previous_entry() {
        let root = directory();
        let old = root.path().join("old-state");
        let new = root.path().join("new-state");
        fs::create_dir_all(&old).unwrap();
        let key = [5u8; 32];
        encrypted_archive(&old, &key);
        store_key(crate::migrate::LEGACY_KEYRING_SERVICE, &old, &key);
        note_key_origin(&old).unwrap();
        let old_canonical = fs::read(old.join(ORIGIN_FILE)).unwrap();
        fs::rename(&old, &new).unwrap();
        let new_entry = entry(crate::migrate::KEYRING_SERVICE, &new);
        new_entry
            .as_any()
            .downcast_ref::<keyring_core::mock::Cred>()
            .unwrap()
            .set_error(keyring_core::Error::PlatformFailure(Box::new(
                std::io::Error::other("keyring refused the write"),
            )));
        let error = finish_key_migration(&new).unwrap_err();
        assert!(
            error.to_string().contains("previous key was kept"),
            "{error}"
        );
        assert!(new.join(ORIGIN_FILE).is_file());
        let old_identity = identity_for_canonical_bytes(&old_canonical);
        let old_entry = test_store()
            .unwrap()
            .build(crate::migrate::LEGACY_KEYRING_SERVICE, &old_identity, None)
            .unwrap();
        assert_eq!(old_entry.get_secret().unwrap(), key);
        finish_key_migration(&new).unwrap();
        assert_eq!(
            read_secret(&open(&new.join("archive.db"), &key).unwrap()),
            "moved history"
        );
        assert!(matches!(
            old_entry.get_secret(),
            Err(keyring_core::Error::NoEntry)
        ));
    }

    #[test]
    fn an_archive_that_does_not_open_keeps_the_previous_entry() {
        let root = directory();
        let old = root.path().join("old-state");
        let new = root.path().join("new-state");
        fs::create_dir_all(&old).unwrap();
        let key = [6u8; 32];
        encrypted_archive(&old, &key);
        store_key(crate::migrate::LEGACY_KEYRING_SERVICE, &old, &key);
        note_key_origin(&old).unwrap();
        let old_canonical = fs::read(old.join(ORIGIN_FILE)).unwrap();
        fs::rename(&old, &new).unwrap();
        fs::write(new.join("archive.db"), b"this is not the encrypted archive").unwrap();
        let error = finish_key_migration(&new).unwrap_err();
        assert!(
            error.to_string().contains("previous key was kept"),
            "{error}"
        );
        let old_identity = identity_for_canonical_bytes(&old_canonical);
        let old_entry = test_store()
            .unwrap()
            .build(crate::migrate::LEGACY_KEYRING_SERVICE, &old_identity, None)
            .unwrap();
        assert_eq!(old_entry.get_secret().unwrap(), key);
        assert!(new.join(ORIGIN_FILE).is_file());
    }

    #[test]
    fn finishing_without_a_note_does_nothing() {
        let root = directory();
        let state = root.path().join("state");
        fs::create_dir_all(&state).unwrap();
        finish_key_migration(&state).unwrap();
        assert!(!state.join(ORIGIN_FILE).exists());
    }
}
