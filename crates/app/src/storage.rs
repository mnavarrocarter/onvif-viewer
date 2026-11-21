use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, params};

#[derive(Clone)]
pub struct Source {
    pub id: u64,
    pub name: String,
    pub address: String,
    pub username: String,
    pub password: String,
    pub model: String,
    pub serial: String,
    pub profiles: Vec<onvif::Profile>,
    pub selected_profile: Option<String>,
}

impl Source {
    pub fn profile_label(profile: &onvif::Profile) -> String {
        let mut label = profile.name.clone();
        if let (Some(width), Some(height)) = (profile.width, profile.height) {
            label.push_str(&format!("  {width}×{height}"));
        }
        if let Some(codec) = &profile.codec {
            label.push_str(&format!("  {codec}"));
        }
        if let Some(codec) = &profile.audio_codec {
            label.push_str(&format!("  Audio: {codec}"));
        }
        label
    }

    pub fn default_profile(profiles: &[onvif::Profile]) -> Option<String> {
        profiles
            .iter()
            .find(|profile| {
                profile.codec.is_some() || profile.width.is_some() || profile.height.is_some()
            })
            .or_else(|| profiles.first())
            .map(|profile| profile.token.clone())
    }

    pub fn selected_profile_label(&self) -> String {
        self.selected_profile
            .as_ref()
            .and_then(|token| self.profiles.iter().find(|profile| &profile.token == token))
            .map(Self::profile_label)
            .unwrap_or_else(|| "No profile available".to_owned())
    }
}

pub type StorageError = Box<dyn std::error::Error + Send + Sync>;

pub trait SourceStore {
    fn list(&self) -> Result<Vec<Source>, StorageError>;
    fn insert(&mut self, source: Source) -> Result<(), StorageError>;
    fn update(&mut self, source: Source) -> Result<(), StorageError>;
    fn remove(&mut self, id: u64) -> Result<(), StorageError>;
}

#[cfg(test)]
#[derive(Default)]
pub struct MemoryStore {
    sources: Vec<Source>,
}

#[cfg(test)]
impl SourceStore for MemoryStore {
    fn list(&self) -> Result<Vec<Source>, StorageError> {
        Ok(self.sources.clone())
    }

    fn insert(&mut self, source: Source) -> Result<(), StorageError> {
        self.sources.push(source);
        Ok(())
    }
    fn update(&mut self, source: Source) -> Result<(), StorageError> {
        let stored = self
            .sources
            .iter_mut()
            .find(|item| item.id == source.id)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "Camera not found"))?;
        *stored = source;
        Ok(())
    }

    fn remove(&mut self, id: u64) -> Result<(), StorageError> {
        self.sources.retain(|source| source.id != id);
        Ok(())
    }
}

// Secret identifiers are random and never reused. A durable cleanup queue bridges
// SQLite transactions and Secret Service, which cannot participate in them.
trait Secrets: Send {
    fn get(&self, key: &str) -> Result<Option<String>, StorageError>;
    fn set(&self, key: &str, password: &str) -> Result<(), StorageError>;
    fn delete(&self, key: &str) -> Result<(), StorageError>;
}

struct KeyringSecrets;

fn secret_error() -> StorageError {
    error(
        "Cannot access the password store. Unlock GNOME Keyring and check that Secret Service is running",
    )
}

fn error(message: &'static str) -> StorageError {
    io::Error::other(message).into()
}

fn database_error() -> StorageError {
    error(
        "Cannot access the camera library. Check the data directory permissions and available disk space",
    )
}

impl KeyringSecrets {
    fn entry(key: &str) -> Result<keyring::Entry, StorageError> {
        keyring::Entry::new("onvif-viewer", key).map_err(|_| secret_error())
    }
}

impl Secrets for KeyringSecrets {
    fn get(&self, key: &str) -> Result<Option<String>, StorageError> {
        match Self::entry(key)?.get_password() {
            Ok(password) => Ok(Some(password)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(secret_error()),
        }
    }

    fn set(&self, key: &str, password: &str) -> Result<(), StorageError> {
        Self::entry(key)?
            .set_password(password)
            .map_err(|_| secret_error())
    }

    fn delete(&self, key: &str) -> Result<(), StorageError> {
        match Self::entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(secret_error()),
        }
    }
}

pub struct PersistentStore {
    db: Connection,
    lock_path: PathBuf,
    secrets: Box<dyn Secrets>,
}

impl PersistentStore {
    pub fn open() -> Result<Self, StorageError> {
        let base = match std::env::var_os("XDG_DATA_HOME").filter(|p| Path::new(p).is_absolute()) {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(std::env::var_os("HOME").ok_or_else(|| {
                error("Cannot locate the camera library. Set HOME or an absolute XDG_DATA_HOME")
            })?)
            .join(".local/share"),
        };
        Self::open_at(&base.join("onvif-viewer"), Box::new(KeyringSecrets))
    }

    fn open_at(dir: &Path, secrets: Box<dyn Secrets>) -> Result<Self, StorageError> {
        fs::create_dir_all(dir).map_err(|_| database_error())?;
        let lock_path = dir.join("library.lock");
        let lock = Self::lock(&lock_path)?;
        let db = Connection::open(dir.join("library.sqlite3")).map_err(|_| database_error())?;
        db.busy_timeout(Duration::from_secs(5))
            .map_err(|_| database_error())?;
        let version: i64 = db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|_| database_error())?;
        if version > 1 {
            return Err(error(
                "This camera library was created by a newer version. Update ONVIF Viewer",
            ));
        }
        db.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS sources (
                 id TEXT PRIMARY KEY, name TEXT NOT NULL, address TEXT NOT NULL,
                 username TEXT NOT NULL, model TEXT NOT NULL, serial TEXT NOT NULL,
                 selected_profile TEXT, secret_key TEXT NOT NULL UNIQUE
             );
             CREATE TABLE IF NOT EXISTS profiles (
                 source_id TEXT NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
                 position INTEGER NOT NULL, token TEXT NOT NULL, name TEXT NOT NULL,
                 width INTEGER, height INTEGER, codec TEXT, audio_codec TEXT,
                 PRIMARY KEY (source_id, position)
             );
             CREATE TABLE IF NOT EXISTS secret_cleanup (secret_key TEXT PRIMARY KEY);
             PRAGMA user_version = 1;
             COMMIT;
             PRAGMA foreign_keys = ON;",
        )
        .map_err(|_| database_error())?;
        let store = Self {
            db,
            lock_path,
            secrets,
        };
        store.cleanup()?;
        drop(lock);
        Ok(store)
    }

    // Every process uses the same advisory lock, including readers, so a secret
    // cannot be retired while another process is loading its source.
    fn lock(path: &Path) -> Result<File, StorageError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|_| database_error())?;
        file.lock().map_err(|_| database_error())?;
        Ok(file)
    }

    fn cleanup(&self) -> Result<(), StorageError> {
        let mut statement = self
            .db
            .prepare("SELECT secret_key FROM secret_cleanup")
            .map_err(|_| database_error())?;
        let keys = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|_| database_error())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| database_error())?;
        drop(statement);
        for key in keys {
            self.secrets.delete(&key)?;
            self.db
                .execute("DELETE FROM secret_cleanup WHERE secret_key = ?1", [&key])
                .map_err(|_| database_error())?;
        }
        Ok(())
    }

    fn validate_address(address: &str) -> Result<(), StorageError> {
        let input = address.trim();
        if input.is_empty() {
            return Err(error("Enter the camera address"));
        }
        let url = if input.contains("://") {
            url::Url::parse(input)
        } else {
            url::Url::parse(&format!("http://{input}"))
        }
        .map_err(|_| error("Invalid camera address; enter a host or HTTP(S) URL"))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(error("Camera address must be a host or HTTP(S) URL"));
        }
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(error(
                "Remove credentials, query parameters, and fragments from the camera address",
            ));
        }
        Ok(())
    }

    fn save(&mut self, source: Source, update: bool) -> Result<(), StorageError> {
        Self::validate_address(&source.address)?;
        let _lock = Self::lock(&self.lock_path)?;
        self.cleanup()?;
        let id = source.id.to_string();
        let old_key: Option<String> = self
            .db
            .query_row(
                "SELECT secret_key FROM sources WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| database_error())?;
        if update && old_key.is_none() {
            return Err(error(
                "Camera not found. Reload the camera library and try again",
            ));
        }
        if !update && old_key.is_some() {
            return Err(error(
                "Camera ID already exists. Reload the camera library and try again",
            ));
        }

        let key = uuid::Uuid::new_v4().to_string();
        // Commit the intent before touching Secret Service. If the process stops
        // at any point, the next opener can safely delete this orphan.
        self.db
            .execute(
                "INSERT INTO secret_cleanup (secret_key) VALUES (?1)",
                [&key],
            )
            .map_err(|_| database_error())?;
        self.secrets.set(&key, &source.password)?;
        let tx = self.db.transaction().map_err(|_| database_error())?;
        if update {
            tx.execute(
                "UPDATE sources SET name=?2, address=?3, username=?4, model=?5, serial=?6,
                 selected_profile=?7, secret_key=?8 WHERE id=?1",
                params![
                    id,
                    source.name,
                    source.address,
                    source.username,
                    source.model,
                    source.serial,
                    source.selected_profile,
                    key
                ],
            )
            .map_err(|_| database_error())?;
            tx.execute("DELETE FROM profiles WHERE source_id=?1", [&id])
                .map_err(|_| database_error())?;
        } else {
            tx.execute(
                "INSERT INTO sources (id, name, address, username, model, serial, selected_profile, secret_key)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![id, source.name, source.address, source.username, source.model,
                    source.serial, source.selected_profile, key]
            ).map_err(|_| database_error())?;
        }
        for (position, profile) in source.profiles.iter().enumerate() {
            tx.execute(
                "INSERT INTO profiles (source_id, position, token, name, width, height, codec, audio_codec)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![id, position as i64, profile.token, profile.name, profile.width,
                    profile.height, profile.codec, profile.audio_codec]
            ).map_err(|_| database_error())?;
        }
        tx.execute("DELETE FROM secret_cleanup WHERE secret_key=?1", [&key])
            .map_err(|_| database_error())?;
        if let Some(old_key) = old_key {
            tx.execute(
                "INSERT INTO secret_cleanup (secret_key) VALUES (?1)",
                [&old_key],
            )
            .map_err(|_| database_error())?;
        }
        tx.commit().map_err(|_| database_error())?;
        self.cleanup().map_err(|_| error(
            "Camera was saved, but an old password could not be removed. Unlock GNOME Keyring and reopen the library"
        ))?;
        Ok(())
    }
}

impl SourceStore for PersistentStore {
    fn list(&self) -> Result<Vec<Source>, StorageError> {
        let _lock = Self::lock(&self.lock_path)?;
        self.cleanup()?;
        let mut statement = self.db.prepare(
            "SELECT id, name, address, username, model, serial, selected_profile, secret_key FROM sources"
        ).map_err(|_| database_error())?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                ))
            })
            .map_err(|_| database_error())?;
        let mut sources = Vec::new();
        for row in rows {
            let (id, name, address, username, model, serial, selected_profile, key) =
                row.map_err(|_| database_error())?;
            Self::validate_address(&address)?;
            let password = self.secrets.get(&key)?.ok_or_else(|| error(
                "A camera password is missing from GNOME Keyring. Restore the keyring or re-add the camera"
            ))?;
            let mut profiles = self.db.prepare(
                "SELECT token, name, width, height, codec, audio_codec FROM profiles WHERE source_id=?1 ORDER BY position"
            ).map_err(|_| database_error())?;
            let profiles = profiles
                .query_map([&id], |row| {
                    Ok(onvif::Profile {
                        token: row.get(0)?,
                        name: row.get(1)?,
                        width: row.get(2)?,
                        height: row.get(3)?,
                        codec: row.get(4)?,
                        audio_codec: row.get(5)?,
                    })
                })
                .map_err(|_| database_error())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| database_error())?;
            sources.push(Source {
                id: id.parse().map_err(|_| database_error())?,
                name,
                address,
                username,
                password,
                model,
                serial,
                profiles,
                selected_profile,
            });
        }
        sources.sort_by_key(|source| source.id);
        Ok(sources)
    }

    fn insert(&mut self, source: Source) -> Result<(), StorageError> {
        self.save(source, false)
    }

    fn update(&mut self, source: Source) -> Result<(), StorageError> {
        self.save(source, true)
    }

    fn remove(&mut self, id: u64) -> Result<(), StorageError> {
        let _lock = Self::lock(&self.lock_path)?;
        self.cleanup()?;
        let tx = self.db.transaction().map_err(|_| database_error())?;
        let id = id.to_string();
        let key: Option<String> = tx
            .query_row("SELECT secret_key FROM sources WHERE id=?1", [&id], |row| {
                row.get(0)
            })
            .optional()
            .map_err(|_| database_error())?;
        if let Some(key) = key {
            tx.execute(
                "INSERT INTO secret_cleanup (secret_key) VALUES (?1)",
                [&key],
            )
            .map_err(|_| database_error())?;
            tx.execute("DELETE FROM sources WHERE id=?1", [&id])
                .map_err(|_| database_error())?;
        }
        tx.commit().map_err(|_| database_error())?;
        self.cleanup().map_err(|_| error(
            "Camera was removed, but its password could not be removed. Unlock GNOME Keyring and reopen the library"
        ))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{MemoryStore, Source, SourceStore};
    use super::{PersistentStore, Secrets, StorageError};
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    #[derive(Default)]
    struct FakeState {
        values: HashMap<String, String>,
        fail_set_after_write: bool,
        fail_delete: bool,
        fail_get: bool,
    }

    #[derive(Clone, Default)]
    struct FakeSecrets(Arc<Mutex<FakeState>>);

    impl Secrets for FakeSecrets {
        fn get(&self, key: &str) -> Result<Option<String>, StorageError> {
            let state = self.0.lock().unwrap();
            if state.fail_get {
                return Err(super::secret_error());
            }
            Ok(state.values.get(key).cloned())
        }

        fn set(&self, key: &str, value: &str) -> Result<(), StorageError> {
            let mut state = self.0.lock().unwrap();
            state.values.insert(key.into(), value.into());
            if state.fail_set_after_write {
                return Err(super::secret_error());
            }
            Ok(())
        }

        fn delete(&self, key: &str) -> Result<(), StorageError> {
            let mut state = self.0.lock().unwrap();
            if state.fail_delete {
                return Err(super::secret_error());
            }
            state.values.remove(key);
            Ok(())
        }
    }

    fn persistent(dir: &std::path::Path, secrets: &FakeSecrets) -> PersistentStore {
        PersistentStore::open_at(dir, Box::new(secrets.clone())).unwrap()
    }

    #[test]
    fn persistent_crud_restores_all_metadata_and_rotates_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = FakeSecrets::default();
        let mut store = persistent(dir.path(), &secrets);
        let mut camera = source(u64::MAX, "alice", "first unique password");
        camera.address = "camera.local:8080".into();
        camera.profiles = vec![
            onvif::Profile {
                token: "main".into(),
                name: "Main".into(),
                width: Some(1920),
                height: Some(1080),
                codec: Some("H264".into()),
                audio_codec: Some("AAC".into()),
            },
            onvif::Profile {
                token: "sub".into(),
                name: "Sub".into(),
                width: None,
                height: None,
                codec: None,
                audio_codec: None,
            },
        ];
        camera.selected_profile = Some("sub".into());
        store.insert(camera).unwrap();
        let first_key = secrets
            .0
            .lock()
            .unwrap()
            .values
            .keys()
            .next()
            .unwrap()
            .clone();
        drop(store);

        let mut store = persistent(dir.path(), &secrets);
        let mut cameras = store.list().unwrap();
        let mut camera = cameras.remove(0);
        assert_eq!(camera.id, u64::MAX);
        assert_eq!(camera.address, "camera.local:8080");
        assert_eq!(camera.username, "alice");
        assert_eq!(camera.password, "first unique password");
        assert_eq!(
            (camera.model.as_str(), camera.serial.as_str()),
            ("Model", "Serial")
        );
        assert_eq!(camera.selected_profile.as_deref(), Some("sub"));
        assert_eq!(camera.profiles.len(), 2);
        assert_eq!(camera.profiles[0].width, Some(1920));
        assert_eq!(camera.profiles[0].height, Some(1080));
        assert_eq!(camera.profiles[0].codec.as_deref(), Some("H264"));
        assert_eq!(camera.profiles[0].audio_codec.as_deref(), Some("AAC"));
        assert_eq!(camera.profiles[1].name, "Sub");
        camera.password = "second unique password".into();
        camera.selected_profile = Some("main".into());
        store.update(camera).unwrap();
        {
            let state = secrets.0.lock().unwrap();
            assert_eq!(state.values.len(), 1);
            assert!(!state.values.contains_key(&first_key));
        }
        drop(store);
        let mut store = persistent(dir.path(), &secrets);
        let camera = store.list().unwrap().remove(0);
        assert_eq!(camera.password, "second unique password");
        assert_eq!(camera.selected_profile.as_deref(), Some("main"));
        store.remove(u64::MAX).unwrap();
        assert!(store.list().unwrap().is_empty());
        assert!(secrets.0.lock().unwrap().values.is_empty());
    }

    #[test]
    fn failed_secret_write_and_database_transaction_recover_orphans() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = FakeSecrets::default();
        let mut store = persistent(dir.path(), &secrets);
        store.insert(source(1, "user", "old")).unwrap();
        secrets.0.lock().unwrap().fail_set_after_write = true;
        let err = store
            .update(source(1, "user", "new"))
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("Unlock GNOME Keyring"));
        assert!(!err.contains("new"));
        assert_eq!(secrets.0.lock().unwrap().values.len(), 2);
        secrets.0.lock().unwrap().fail_set_after_write = false;
        drop(store);
        let mut store = persistent(dir.path(), &secrets);
        assert_eq!(store.list().unwrap()[0].password, "old");
        assert_eq!(secrets.0.lock().unwrap().values.len(), 1);

        store.db.execute_batch(
            "CREATE TRIGGER fail_update BEFORE UPDATE ON sources BEGIN SELECT RAISE(ABORT, 'failure'); END;"
        ).unwrap();
        assert!(store.update(source(1, "user", "new")).is_err());
        assert_eq!(secrets.0.lock().unwrap().values.len(), 2);
        drop(store);
        let store = persistent(dir.path(), &secrets);
        assert_eq!(store.list().unwrap()[0].password, "old");
        assert_eq!(secrets.0.lock().unwrap().values.len(), 1);
    }

    #[test]
    fn committed_update_and_removal_retry_failed_cleanup_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = FakeSecrets::default();
        let mut store = persistent(dir.path(), &secrets);
        store.insert(source(1, "user", "old")).unwrap();
        secrets.0.lock().unwrap().fail_delete = true;
        let message = store
            .update(source(1, "user", "new"))
            .err()
            .unwrap()
            .to_string();
        assert!(message.contains("was saved"));
        assert_eq!(secrets.0.lock().unwrap().values.len(), 2);
        drop(store);
        assert!(PersistentStore::open_at(dir.path(), Box::new(secrets.clone())).is_err());
        secrets.0.lock().unwrap().fail_delete = false;
        let mut store = persistent(dir.path(), &secrets);
        assert_eq!(store.list().unwrap()[0].password, "new");
        assert_eq!(secrets.0.lock().unwrap().values.len(), 1);
        secrets.0.lock().unwrap().fail_delete = true;
        let message = store.remove(1).err().unwrap().to_string();
        assert!(message.contains("was removed"));
        drop(store);
        secrets.0.lock().unwrap().fail_delete = false;
        let store = persistent(dir.path(), &secrets);
        assert!(store.list().unwrap().is_empty());
        assert!(secrets.0.lock().unwrap().values.is_empty());
    }

    #[test]
    fn rejects_sensitive_addresses_and_never_writes_passwords_to_sqlite_or_wal() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = FakeSecrets::default();
        let mut store = persistent(dir.path(), &secrets);
        store
            .db
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
        for address in [
            "http://user:password@camera/",
            "camera/?password=hidden",
            "https://camera/#password",
            "http://camera/?",
            "camera/#",
            "ftp://camera/",
        ] {
            let mut camera = source(1, "user", "secret-password");
            camera.address = address.into();
            let message = store.insert(camera).err().unwrap().to_string();
            assert!(!message.contains("password=hidden"));
            assert!(!message.contains("secret-password"));
        }
        assert!(secrets.0.lock().unwrap().values.is_empty());
        store.insert(source(1, "user", "secret-password")).unwrap();
        store
            .update(source(1, "user", "other-secret-password"))
            .unwrap();
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let bytes = std::fs::read(entry.unwrap().path()).unwrap();
            assert!(
                !bytes
                    .windows(b"secret-password".len())
                    .any(|w| w == b"secret-password")
            );
        }
    }

    #[test]
    fn independent_libraries_and_reused_ids_do_not_share_secret_keys() {
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let secrets = FakeSecrets::default();
        let mut first = persistent(first_dir.path(), &secrets);
        let mut second = persistent(second_dir.path(), &secrets);
        first.insert(source(1, "first", "one")).unwrap();
        second.insert(source(1, "second", "two")).unwrap();
        assert_eq!(secrets.0.lock().unwrap().values.len(), 2);
        assert_eq!(first.list().unwrap()[0].password, "one");
        assert_eq!(second.list().unwrap()[0].password, "two");
        let old_key: String = first
            .db
            .query_row("SELECT secret_key FROM sources WHERE id='1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        first.remove(1).unwrap();
        first.insert(source(1, "first", "three")).unwrap();
        let new_key: String = first
            .db
            .query_row("SELECT secret_key FROM sources WHERE id='1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_ne!(old_key, new_key);
        assert_eq!(second.list().unwrap()[0].password, "two");
        assert!(first.insert(source(1, "duplicate", "unused")).is_err());
        assert!(first.update(source(99, "missing", "unused")).is_err());
        assert_eq!(secrets.0.lock().unwrap().values.len(), 2);
    }

    #[test]
    fn missing_or_unavailable_secret_is_reported_without_leaking_values() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = FakeSecrets::default();
        let mut store = persistent(dir.path(), &secrets);
        store.insert(source(1, "user", "private-value")).unwrap();
        secrets.0.lock().unwrap().fail_get = true;
        let message = store.list().err().unwrap().to_string();
        assert!(message.contains("Unlock GNOME Keyring"));
        assert!(!message.contains("private-value"));
        {
            let mut state = secrets.0.lock().unwrap();
            state.fail_get = false;
            state.values.clear();
        }
        assert!(store.list().err().unwrap().to_string().contains("missing"));
    }

    fn source(id: u64, username: &str, password: &str) -> Source {
        Source {
            id,
            name: format!("Camera {id}"),
            address: format!("http://camera-{id}/onvif/device_service"),
            username: username.into(),
            password: password.into(),
            model: "Model".into(),
            serial: "Serial".into(),
            profiles: Vec::new(),
            selected_profile: None,
        }
    }

    #[test]
    fn adds_lists_and_removes_sources() {
        let mut store = MemoryStore::default();
        assert!(store.list().unwrap().is_empty());

        store
            .insert(source(1, "first-user", "first-password"))
            .unwrap();
        store
            .insert(source(2, "second-user", "second-password"))
            .unwrap();
        let sources = store.list().unwrap();
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].id, 1);
        assert_eq!(sources[0].name, "Camera 1");
        assert_eq!(sources[0].address, "http://camera-1/onvif/device_service");
        assert_eq!(sources[1].id, 2);

        store.remove(1).unwrap();
        store.remove(99).unwrap();
        let sources = store.list().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].id, 2);
        store.remove(2).unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn updates_the_selected_profile_without_replacing_other_sources() {
        let mut store = MemoryStore::default();
        store.insert(source(1, "user", "password")).unwrap();
        store.insert(source(2, "other", "other-password")).unwrap();
        let mut updated = store.list().unwrap().remove(0);
        updated.selected_profile = Some("profile-2".into());
        updated.name = "Updated".into();
        store.update(updated).unwrap();
        let sources = store.list().unwrap();
        assert_eq!(sources[0].selected_profile.as_deref(), Some("profile-2"));
        assert_eq!(sources[0].name, "Updated");
        assert_eq!(sources[1].name, "Camera 2");
        assert!(store.update(source(99, "", "")).is_err());
    }

    #[test]
    fn credentials_are_isolated_per_source_and_from_list_copies() {
        let mut store = MemoryStore::default();
        store
            .insert(source(1, "first-user", "first-password"))
            .unwrap();
        store
            .insert(source(2, "second-user", "second-password"))
            .unwrap();

        let mut sources = store.list().unwrap();
        assert_eq!(sources[0].username, "first-user");
        assert_eq!(sources[0].password, "first-password");
        assert_eq!(sources[1].username, "second-user");
        assert_eq!(sources[1].password, "second-password");
        sources[0].username.clear();
        sources[0].password.clear();

        let sources = store.list().unwrap();
        assert_eq!(sources[0].username, "first-user");
        assert_eq!(sources[0].password, "first-password");
        assert_eq!(sources[1].username, "second-user");
        assert_eq!(sources[1].password, "second-password");
    }
}
