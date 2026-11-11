//! What the phone's lock settings keep: a value under a name, for each
//! user. Among them is which of the protectors the phone keeps for a user
//! is the one that user's own credential opens.

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Where the phone keeps them.
pub const DATABASE: &str = "/data/system/locksettings.db";

/// The name the database has while it is read, and the two files SQLite
/// keeps beside it when it is written with a log.
const NAME: &str = "locksettings.db";
const WAL: &str = "-wal";
const SHM: &str = "-shm";

/// The name the protector of a user's own secret is recorded under. Its
/// value is the number that protector's files are named after.
const CURRENT_PROTECTOR: &str = "sp-handle";

/// The number that names no protector. The files the phone keeps for a
/// user's secret itself, rather than for any one protector, are named
/// after it.
const NO_PROTECTOR: i64 = 0;

/// Which protector `user`'s own credential opens, named as its files are.
/// A phone that recorded none for this user gives nothing.
pub fn current_protector(database: &Path, user: u32) -> Result<Option<String>> {
    let Some(recorded) = value(database, CURRENT_PROTECTOR, user)? else { return Ok(None) };
    let id: i64 = recorded
        .parse()
        .with_context(|| format!("{} records user {user}'s protector as {recorded:?}", database.display()))?;
    Ok((id != NO_PROTECTOR).then(|| crate::user::protector_name(id)))
}

/// What the lock settings keep for `user` under `name`, where they keep
/// anything.
///
/// The database is read from a copy, because the phone writes it in SQLite's
/// write-ahead log mode: a screen lock set or changed while the phone was
/// running can still be in `locksettings.db-wal` and not yet in
/// `locksettings.db` itself. Opening the file where it lies, without letting
/// SQLite touch the log, reads the value before the change — which is a
/// recovery that checks a credential against the screen lock the phone no
/// longer has. So the database and its log are copied somewhere writable and
/// opened there, where SQLite can replay the log and make its shared-memory
/// file itself, and the copy is taken away afterwards.
///
/// Nothing in `/data` is written to, and the phone is not needed to have been
/// shut down cleanly: a log left half-written is replayed as far as its own
/// checksums allow, which is what SQLite does when the phone starts again.
fn value(database: &Path, name: &str, user: u32) -> Result<Option<String>> {
    let copy = Copy::of(database)?;
    let at = format!("file:{}?immutable=1", copy.db.display());
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI;
    let db = rusqlite::Connection::open_with_flags(&at, flags).with_context(|| format!("open {}", database.display()))?;
    let mut query = db
        .prepare("SELECT value FROM locksettings WHERE name = ?1 AND user = ?2")
        .context("ask the lock settings what they keep")?;
    let mut rows = query
        .query((name, user))
        .with_context(|| format!("ask the lock settings what {name} is for user {user}"))?;
    let Some(row) = rows.next().with_context(|| format!("read what {} says about {name}", database.display()))? else {
        return Ok(None);
    };
    row.get(0).with_context(|| format!("read what {name} is for user {user}"))
}

/// A writable copy of a phone's lock settings, and of the log beside them.
struct Copy {
    /// The folder holding both, taken away again when this is dropped.
    folder: PathBuf,
    db: PathBuf,
}

impl Copy {
    /// Copy `database` and, where the phone kept them, its `-wal` and `-shm`
    /// files, into a folder of their own under /tmp. The copy is checkpointed
    /// once so that what the log holds is folded into the database file
    /// itself: from then on a plain read of the copy sees the newest value
    /// even though `immutable=1` keeps SQLite from looking at a log at all.
    fn of(database: &Path) -> Result<Copy> {
        let folder = scratch_folder()?;
        let db = folder.join(NAME);
        // A database that is not there is an error the caller says the same
        // way it always did; copying nothing is what leaves it that way.
        fs::copy(database, &db).with_context(|| format!("copy {} to {}", database.display(), db.display()))?;
        for tail in [WAL, SHM] {
            let from = beside(database, tail);
            if from.is_file() {
                let to = beside(&db, tail);
                fs::copy(&from, &to).with_context(|| format!("copy {} to {}", from.display(), to.display()))?;
            }
        }
        let copy = Copy { folder, db };
        copy.fold_the_log_in().with_context(|| format!("fold the log of {} into its copy", database.display()))?;
        Ok(copy)
    }

    /// Open the copy read-write once, so that SQLite replays whatever the log
    /// holds and writes it into the database file, then close it: the
    /// read that follows can then use `immutable=1`, which is what keeps this
    /// from writing anything else.
    fn fold_the_log_in(&self) -> Result<()> {
        let db = rusqlite::Connection::open(&self.db).context("open the copy of the lock settings")?;
        // Reading one row makes SQLite recover the log. It is thrown away:
        // what matters is the work of recovery, which the file keeps.
        let _ = db.query_row("SELECT count(*) FROM locksettings", [], |row| row.get::<_, i64>(0));
        drop(db);
        Ok(())
    }
}

impl Drop for Copy {
    fn drop(&mut self) {
        // Whatever went wrong above, nothing of the copy outlives this read:
        // it holds the phone's lock settings.
        let _ = fs::remove_dir_all(&self.folder);
    }
}

/// A folder of this process's own to copy the lock settings into.
///
/// The copy is made somewhere writable that costs nothing to leave behind if
/// the phone stops: a recovery's /tmp is memory, and the host's own temporary
/// folder is where the tests make theirs.
fn scratch_folder() -> Result<PathBuf> {
    let base = std::env::temp_dir();
    for n in 0..32u32 {
        let at = base.join(format!("skyward-locksettings-{}-{n}", std::process::id()));
        match fs::create_dir(&at) {
            Ok(()) => return Ok(at),
            // Another process's copy of the same name is not this one's to
            // take away, so the next name is tried.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("make {}", at.display())),
        }
    }
    bail!("no folder under {} could be made for a copy of the lock settings", base.display())
}

/// `database`'s sibling with `tail` after its name, as SQLite names a log.
fn beside(database: &Path, tail: &str) -> PathBuf {
    let mut name = database.as_os_str().to_os_string();
    name.push(tail);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lock settings as the phone writes them: the table it keeps, holding
    /// the rows given.
    fn scratch(test: &str, rows: &[(&str, u32, Option<&str>)]) -> std::path::PathBuf {
        let at = std::env::temp_dir().join(format!("skyward-locksettings-{test}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&at);
        let db = rusqlite::Connection::open(&at).unwrap();
        db.execute("CREATE TABLE locksettings (_id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, user INTEGER, value TEXT)", ()).unwrap();
        for (name, user, value) in rows {
            db.execute("INSERT INTO locksettings (name, user, value) VALUES (?1, ?2, ?3)", (name, user, value)).unwrap();
        }
        at
    }

    #[test]
    fn the_protector_recorded_for_a_user_is_the_one_named() {
        let at = scratch("recorded", &[
            ("sp-handle", 0, Some("623346955855960006")),
            ("sp-handle", 10, Some("-1")),
            ("lockscreen.password_type", 0, Some("65536")),
        ]);
        assert_eq!(current_protector(&at, 0).unwrap().as_deref(), Some("08a692c0f39d97c6"));
        assert_eq!(current_protector(&at, 10).unwrap().as_deref(), Some("ffffffffffffffff"));
        assert_eq!(current_protector(&at, 11).unwrap(), None, "a user the phone recorded nothing for");
        std::fs::remove_file(&at).unwrap();
    }

    #[test]
    fn the_number_of_no_protector_and_a_row_holding_nothing_give_none() {
        let at = scratch("none", &[("sp-handle", 0, Some("0")), ("sp-handle", 10, None)]);
        assert_eq!(current_protector(&at, 0).unwrap(), None);
        assert_eq!(current_protector(&at, 10).unwrap(), None, "a row holding nothing");
        std::fs::remove_file(&at).unwrap();
    }

    #[test]
    fn a_value_that_is_no_number_and_a_database_that_is_not_there_are_errors() {
        let at = scratch("odd", &[("sp-handle", 0, Some("behind the sofa"))]);
        assert!(current_protector(&at, 0).is_err());
        assert!(current_protector(Path::new("/nowhere/locksettings.db"), 0).is_err());
        std::fs::remove_file(&at).unwrap();
    }

    /// A database as a phone leaves one: in the write-ahead log mode, with
    /// `newest` in the log rather than folded into the database file, and
    /// copied out while the writing connection is still open.
    ///
    /// The copy matters: SQLite folds a log into the database and deletes it
    /// when the last connection to it closes, which is what a phone does as
    /// it shuts down cleanly and what it does *not* do when it is switched
    /// off or loses power. Copying before the close is what a recovery sees.
    fn scratch_with_a_log(test: &str, older: &str, newest: &str) -> std::path::PathBuf {
        let live = std::env::temp_dir().join(format!("skyward-locksettings-{test}-{}-live.db", std::process::id()));
        let at = std::env::temp_dir().join(format!("skyward-locksettings-{test}-{}.db", std::process::id()));
        for path in [&live, &at] {
            let _ = std::fs::remove_file(path);
            let _ = std::fs::remove_file(beside(path, WAL));
            let _ = std::fs::remove_file(beside(path, SHM));
        }
        let db = rusqlite::Connection::open(&live).unwrap();
        db.pragma_update(None, "journal_mode", "WAL").unwrap();
        // Nothing folds the log in on its own: on a phone that happens when
        // the checkpoint interval comes round, and not in between.
        db.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        db.execute("CREATE TABLE locksettings (_id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, user INTEGER, value TEXT)", ()).unwrap();
        db.execute("INSERT INTO locksettings (name, user, value) VALUES ('sp-handle', 0, ?1)", (older,)).unwrap();
        db.pragma_update(None, "wal_checkpoint", "TRUNCATE").unwrap();
        db.execute("UPDATE locksettings SET value = ?1 WHERE name = 'sp-handle'", (newest,)).unwrap();
        // Copied while the connection is open, which is the state a phone that
        // was switched off leaves behind.
        std::fs::copy(&live, &at).unwrap();
        for tail in [WAL, SHM] {
            if beside(&live, tail).is_file() {
                std::fs::copy(beside(&live, tail), beside(&at, tail)).unwrap();
            }
        }
        drop(db);
        for tail in ["", WAL, SHM] {
            let _ = std::fs::remove_file(beside(&live, tail));
        }
        at
    }

    #[test]
    fn the_screen_lock_changed_last_is_the_one_read() {
        // The fault this exists for: the phone's owner changed their screen
        // lock, the new value was still in the log, and a recovery that read
        // the database file alone went on checking the credential the phone
        // no longer had — so the old password opened it and the new one did
        // not. `623346955855960006` is a protector of one credential and
        // `-498161957179021965` of another.
        let at = scratch_with_a_log("changed", "623346955855960006", "-498161957179021965");
        assert!(beside(&at, WAL).is_file(), "the newer value has to be in the log for this test to mean anything");
        assert_eq!(
            current_protector(&at, 0).unwrap().as_deref(),
            Some("f9162c56ddbb2573"),
            "the protector the phone records now, not the one before the change"
        );
        std::fs::remove_file(&at).unwrap();
    }

    #[test]
    fn the_log_is_read_whether_or_not_its_shared_memory_file_is_there() {
        // A phone switched off without anything writing the database out
        // leaves the log and no shared-memory file: the recovery has to read
        // the same value either way.
        let at = scratch_with_a_log("no-shm", "623346955855960006", "-498161957179021965");
        let _ = std::fs::remove_file(beside(&at, SHM));
        assert_eq!(current_protector(&at, 0).unwrap().as_deref(), Some("f9162c56ddbb2573"));
        std::fs::remove_file(&at).unwrap();
        std::fs::remove_file(beside(&at, WAL)).unwrap();
    }

    #[test]
    fn a_database_written_without_a_log_reads_as_it_stands() {
        // The older phones and the tests above: no log beside it at all.
        let at = scratch("journal", &[("sp-handle", 0, Some("623346955855960006"))]);
        assert!(!beside(&at, WAL).is_file());
        assert_eq!(current_protector(&at, 0).unwrap().as_deref(), Some("08a692c0f39d97c6"));
        std::fs::remove_file(&at).unwrap();
    }

    #[test]
    fn what_is_left_of_a_copy_does_not_outlive_the_read() {
        // The copy holds the phone's lock settings: none of it may be left
        // behind in /tmp.
        let at = scratch("copy", &[("sp-handle", 0, Some("623346955855960006"))]);
        let copy = Copy::of(&at).unwrap();
        let folder = copy.folder.clone();
        assert!(folder.is_dir(), "the copy was made");
        drop(copy);
        assert!(!folder.exists(), "and taken away again with the read");
        std::fs::remove_file(&at).unwrap();
    }
}
