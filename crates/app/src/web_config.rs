//! Persistent LAN viewer configuration. Passwords are stored only in Secret Service.
//! `load` and `save` perform blocking disk/keyring I/O; call them off the GTK thread.

use rusqlite::{Connection, OptionalExtension, params};
use std::{
    fs::{self, OpenOptions},
    io,
    path::{Path, PathBuf},
    time::Duration,
};

pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// Configuration to pass to the LAN server. Never log the password.
#[derive(Clone)]
pub struct Config {
    pub enabled: bool,
    pub port: u16,
    pub username: String,
    pub password: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 8080,
            username: String::new(),
            password: String::new(),
        }
    }
}

fn error(message: &'static str) -> Error {
    io::Error::other(message).into()
}
fn db_error() -> Error {
    error("Cannot access LAN viewer settings. Check data directory permissions and disk space.")
}
fn secret_error() -> Error {
    error(
        "Cannot access the password store. Unlock GNOME Keyring and check that Secret Service is running.",
    )
}
fn entry(key: &str) -> Result<keyring::Entry, Error> {
    keyring::Entry::new("onvif-viewer", key).map_err(|_| secret_error())
}
fn delete_secret(key: &str) -> Result<(), Error> {
    match entry(key)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(_) => Err(secret_error()),
    }
}

fn directory() -> Result<PathBuf, Error> {
    let base = match std::env::var_os("XDG_DATA_HOME").filter(|p| Path::new(p).is_absolute()) {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(std::env::var_os("HOME").ok_or_else(|| {
            error("Cannot locate LAN viewer settings. Set HOME or an absolute XDG_DATA_HOME.")
        })?)
        .join(".local/share"),
    };
    Ok(base.join("onvif-viewer"))
}

// Serialize readers and writers, including the lifetime of keyring references.
fn with_store<T>(f: impl FnOnce(&mut Connection) -> Result<T, Error>) -> Result<T, Error> {
    let dir = directory()?;
    fs::create_dir_all(&dir).map_err(|_| db_error())?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("web-settings.lock"))
        .map_err(|_| db_error())?;
    lock.lock().map_err(|_| db_error())?;
    let mut db = Connection::open(dir.join("web-settings.sqlite3")).map_err(|_| db_error())?;
    db.busy_timeout(Duration::from_secs(5))
        .map_err(|_| db_error())?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS settings (id INTEGER PRIMARY KEY CHECK(id=1), enabled INTEGER NOT NULL, port INTEGER NOT NULL, username TEXT NOT NULL, secret_key TEXT); CREATE TABLE IF NOT EXISTS secret_cleanup (secret_key TEXT PRIMARY KEY);").map_err(|_| db_error())?;
    f(&mut db)
}

/// Load settings, returning disabled defaults on a fresh installation.
/// A missing/inaccessible saved password is an error, never an empty fallback.
pub fn load() -> Result<Config, Error> {
    with_store(|db| {
        let row = db
            .query_row(
                "SELECT enabled, port, username, secret_key FROM settings WHERE id=1",
                [],
                |row| {
                    Ok((
                        row.get::<_, bool>(0)?,
                        row.get::<_, u16>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| db_error())?;
        let Some((enabled, port, username, key)) = row else {
            return Ok(Config::default());
        };
        let password = match key {
            Some(key) => entry(&key)?.get_password().map_err(|_| secret_error())?,
            None => String::new(),
        };
        let config = Config {
            enabled,
            port,
            username,
            password,
        };
        validate(&config)?;
        Ok(config)
    })
}

/// Validate settings before enabling the server. Port zero is never persisted.
pub fn validate(config: &Config) -> Result<(), Error> {
    if config.port == 0 {
        return Err(error("Enter a port between 1 and 65535."));
    }
    if config.enabled && (config.username.is_empty() || config.password.is_empty()) {
        return Err(error(
            "Enter a username and password before enabling the LAN viewer.",
        ));
    }
    if config.username.contains(':') || config.username.chars().any(char::is_control) {
        return Err(error(
            "The username cannot contain a colon or control characters.",
        ));
    }
    Ok(())
}

/// Persist settings atomically with respect to other users of this API.
/// Old secrets are queued for cleanup so a failed keyring deletion is retried.
pub fn save(config: &Config) -> Result<(), Error> {
    validate(config)?;
    with_store(|db| {
        let old: Option<String> = db
            .query_row("SELECT secret_key FROM settings WHERE id=1", [], |row| {
                row.get(0)
            })
            .optional()
            .map_err(|_| db_error())?
            .flatten();
        // Retry previous cleanup before introducing another secret.
        let pending = {
            let mut statement = db
                .prepare("SELECT secret_key FROM secret_cleanup")
                .map_err(|_| db_error())?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|_| db_error())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|_| db_error())?
        };
        for key in pending {
            delete_secret(&key)?;
            db.execute("DELETE FROM secret_cleanup WHERE secret_key=?1", [&key])
                .map_err(|_| db_error())?;
        }
        let key = if config.password.is_empty() {
            None
        } else {
            let key = format!("web-viewer-{}", uuid::Uuid::new_v4());
            db.execute("INSERT INTO secret_cleanup VALUES (?1)", [&key])
                .map_err(|_| db_error())?;
            entry(&key)?
                .set_password(&config.password)
                .map_err(|_| secret_error())?;
            Some(key)
        };
        let tx = db.transaction().map_err(|_| db_error())?;
        tx.execute("INSERT INTO settings (id, enabled, port, username, secret_key) VALUES (1, ?1, ?2, ?3, ?4) ON CONFLICT(id) DO UPDATE SET enabled=excluded.enabled, port=excluded.port, username=excluded.username, secret_key=excluded.secret_key", params![config.enabled, config.port, config.username, key]).map_err(|_| db_error())?;
        if let Some(old) = old.as_deref() {
            tx.execute("INSERT OR IGNORE INTO secret_cleanup VALUES (?1)", [old])
                .map_err(|_| db_error())?;
        }
        if let Some(key) = key.as_deref() {
            tx.execute("DELETE FROM secret_cleanup WHERE secret_key=?1", [key])
                .map_err(|_| db_error())?;
        }
        tx.commit().map_err(|_| db_error())?;
        // The commit is authoritative; failed cleanup remains queued for retry.
        if let Some(old) = old.as_deref()
            && delete_secret(old).is_ok()
        {
            let _ = db.execute("DELETE FROM secret_cleanup WHERE secret_key=?1", [old]);
        }
        Ok(())
    })
}
