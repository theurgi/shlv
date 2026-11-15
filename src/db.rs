//! Database location, connection setup, and migrations.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use rusqlite::functions::FunctionFlags;

/// Embedded migrations; `PRAGMA user_version` is the number applied so far.
const MIGRATIONS: &[&str] = &[include_str!("../migrations/001-init.sql")];

/// Resolves the database path: `$SHLV_DB`, else `$XDG_DATA_HOME/shlv/shlv.db`, else
/// `~/.local/share/shlv/shlv.db`.
pub fn default_path() -> Result<PathBuf> {
    resolve_path(|var| std::env::var(var).ok())
}

fn resolve_path(env: impl Fn(&str) -> Option<String>) -> Result<PathBuf> {
    let var = |name| env(name).filter(|v: &String| !v.is_empty());
    if let Some(p) = var("SHLV_DB") {
        return Ok(PathBuf::from(p));
    }
    // The XDG spec says relative values are invalid and should be ignored.
    if let Some(d) = var("XDG_DATA_HOME").filter(|d| Path::new(d).is_absolute()) {
        return Ok(Path::new(&d).join("shlv/shlv.db"));
    }
    match var("HOME") {
        Some(home) => Ok(Path::new(&home).join(".local/share/shlv/shlv.db")),
        None => bail!("cannot locate the database: set SHLV_DB, XDG_DATA_HOME, or HOME"),
    }
}

/// Opens (creating if needed) the database at `path` and brings its schema up to date.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("cannot create directory {}", dir.display()))?;
    }
    let conn = Connection::open(path)
        .with_context(|| format!("cannot open database {}", path.display()))?;
    setup(conn)
}

/// Opens a private in-memory database, for tests.
pub fn open_in_memory() -> Result<Connection> {
    setup(Connection::open_in_memory()?)
}

fn setup(mut conn: Connection) -> Result<Connection> {
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    // journal_mode returns a row, so it can't go through execute_batch on all versions.
    conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
    conn.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;")?;
    conn.create_scalar_function(
        "on_site",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let url = ctx.get_raw(0).as_str().unwrap_or("");
            let site = ctx.get_raw(1).as_str().unwrap_or("");
            Ok(crate::url::on_site(url, site))
        },
    )?;
    migrate(&mut conn)?;
    Ok(conn)
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let version = usize::try_from(version).unwrap_or(usize::MAX);
    if version > MIGRATIONS.len() {
        bail!(
            "database schema version {version} is newer than this shlv supports ({}); upgrade shlv",
            MIGRATIONS.len()
        );
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(version) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)
            .with_context(|| format!("migration {} failed", i + 1))?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(vars: &[(&str, &str)]) -> Option<PathBuf> {
        resolve_path(|name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        })
        .ok()
    }

    #[test]
    fn path_resolution_order() {
        let all = [
            ("SHLV_DB", "/x/my.db"),
            ("XDG_DATA_HOME", "/data"),
            ("HOME", "/home/u"),
        ];
        assert_eq!(path(&all), Some("/x/my.db".into()));
        assert_eq!(path(&all[1..]), Some("/data/shlv/shlv.db".into()));
        assert_eq!(
            path(&all[2..]),
            Some("/home/u/.local/share/shlv/shlv.db".into())
        );
        assert_eq!(
            path(&[("SHLV_DB", ""), ("XDG_DATA_HOME", "rel"), ("HOME", "/h")]),
            Some("/h/.local/share/shlv/shlv.db".into())
        );
        assert_eq!(path(&[]), None);
    }
}
