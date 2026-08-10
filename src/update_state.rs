//! Application-wide scheduling state for release checks.
//!
//! Task data can live in any `--dir`, but update availability belongs to the
//! installed application. This small SQLite store therefore always lives in
//! the user's default mach directory and uses a short lease to coordinate
//! concurrent processes without treating an in-flight attempt as a success.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, TransactionBehavior, params};
use semver::Version;

use crate::store::{
    StoreError, configure_connection, configure_resource_limits, ensure_private_directory,
    prepare_private_database_file, set_private_file,
};

const DATABASE_FILE: &str = "update.db";
const UPDATE_STATE_DIR_ENV: &str = "MACH_UPDATE_STATE_DIR";
const DATABASE_SCHEMA_VERSION: i64 = 1;
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const SUCCESS_INTERVAL_SECONDS: i64 = 24 * 60 * 60;
pub(crate) const FAILURE_RETRY_SECONDS: i64 = 60 * 60;
pub(crate) const LEASE_SECONDS: i64 = 5 * 60;
const MIN_RETRY_SECONDS: i64 = 60;
const MAX_ETAG_BYTES: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpdateState {
    pub(crate) last_successful_check_at: Option<i64>,
    pub(crate) next_check_at: Option<i64>,
    pub(crate) lease_until: Option<i64>,
    pub(crate) etag: Option<String>,
    pub(crate) latest_version: Option<String>,
}

impl UpdateState {
    pub(crate) fn automatic_check_due(&self, now: i64) -> bool {
        match self.next_check_at {
            None => true,
            Some(next) if now >= next => true,
            // A deadline can legitimately be more than one day away when the
            // server asks us to wait. Only a clock move before the last known
            // success proves that the persisted schedule is no longer sound.
            Some(_) => self
                .last_successful_check_at
                .is_some_and(|last_success| now < last_success),
        }
    }

    pub(crate) fn lease_active(&self, now: i64) -> bool {
        self.lease_until
            .is_some_and(|until| now < until && until <= now.saturating_add(LEASE_SECONDS))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpdateLease {
    token: String,
    pub(crate) etag: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AutomaticClaim {
    Claimed(UpdateLease),
    Waiting(UpdateState),
}

pub(crate) struct UpdateStateStore {
    connection: Connection,
}

impl UpdateStateStore {
    pub(crate) fn open_default() -> Result<Self, StoreError> {
        Self::open(default_path(
            std::env::var_os(UPDATE_STATE_DIR_ENV).map(PathBuf::from),
            dirs::home_dir(),
        )?)
    }

    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| StoreError::validation("update-state path has no parent directory"))?;
        ensure_private_directory(parent)?;
        prepare_private_database_file(&path)?;
        let mut connection = Connection::open(&path)?;
        set_private_file(&path)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        configure_resource_limits(&connection)?;
        initialize_schema(&mut connection, &path)?;
        configure_connection(&connection)?;
        quick_check(&connection)?;
        Ok(Self { connection })
    }

    pub(crate) fn open_in_memory() -> Result<Self, StoreError> {
        let mut connection = Connection::open_in_memory()?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        configure_resource_limits(&connection)?;
        initialize_schema(&mut connection, Path::new(":memory:"))?;
        connection.pragma_update(None, "journal_mode", "MEMORY")?;
        quick_check(&connection)?;
        Ok(Self { connection })
    }

    pub(crate) fn snapshot(&self) -> Result<UpdateState, StoreError> {
        load_state(&self.connection)
    }

    pub(crate) fn try_claim_automatic(&mut self, now: i64) -> Result<AutomaticClaim, StoreError> {
        validate_now(now)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = load_state(&tx)?;
        if !state.automatic_check_due(now) || state.lease_active(now) {
            tx.commit()?;
            return Ok(AutomaticClaim::Waiting(state));
        }
        let lease = UpdateLease {
            token: uuid::Uuid::new_v4().to_string(),
            etag: state.etag,
        };
        tx.execute(
            "UPDATE update_state
             SET lease_token = ?1, lease_until = ?2
             WHERE id = 1",
            params![lease.token, now.saturating_add(LEASE_SECONDS)],
        )?;
        tx.commit()?;
        Ok(AutomaticClaim::Claimed(lease))
    }

    /// Manual checks ignore the normal deadline and supersede any older
    /// in-flight lease. A late result from that older worker can no longer
    /// overwrite the manual scheduler state; executable replacement is
    /// serialized separately by the destination install lock.
    pub(crate) fn claim_manual(&mut self, now: i64) -> Result<UpdateLease, StoreError> {
        validate_now(now)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = load_state(&tx)?;
        let lease = UpdateLease {
            token: uuid::Uuid::new_v4().to_string(),
            etag: state.etag,
        };
        tx.execute(
            "UPDATE update_state
             SET lease_token = ?1, lease_until = ?2
             WHERE id = 1",
            params![lease.token, now.saturating_add(LEASE_SECONDS)],
        )?;
        tx.commit()?;
        Ok(lease)
    }

    pub(crate) fn finish_modified(
        &mut self,
        lease: &UpdateLease,
        now: i64,
        etag: Option<&str>,
        latest_version: &str,
    ) -> Result<bool, StoreError> {
        validate_now(now)?;
        validate_etag(etag)?;
        validate_release_version(Some(latest_version))?;
        let changed = self.connection.execute(
            "UPDATE update_state
             SET last_successful_check_at = ?1,
                 next_check_at = ?2,
                 lease_token = NULL,
                 lease_until = NULL,
                 etag = ?3,
                 latest_version = ?4
             WHERE id = 1 AND lease_token = ?5",
            params![
                now,
                now.saturating_add(SUCCESS_INTERVAL_SECONDS),
                etag,
                latest_version,
                lease.token
            ],
        )?;
        Ok(changed == 1)
    }

    pub(crate) fn finish_not_modified(
        &mut self,
        lease: &UpdateLease,
        now: i64,
    ) -> Result<bool, StoreError> {
        validate_now(now)?;
        let changed = self.connection.execute(
            "UPDATE update_state
             SET last_successful_check_at = ?1,
                 next_check_at = ?2,
                 lease_token = NULL,
                 lease_until = NULL
             WHERE id = 1 AND lease_token = ?3",
            params![
                now,
                now.saturating_add(SUCCESS_INTERVAL_SECONDS),
                lease.token
            ],
        )?;
        Ok(changed == 1)
    }

    pub(crate) fn finish_failure(
        &mut self,
        lease: &UpdateLease,
        now: i64,
        retry_at: Option<i64>,
    ) -> Result<bool, StoreError> {
        validate_now(now)?;
        let minimum = now.saturating_add(MIN_RETRY_SECONDS);
        let next = retry_at
            .unwrap_or_else(|| now.saturating_add(FAILURE_RETRY_SECONDS))
            .max(minimum);
        let changed = self.connection.execute(
            "UPDATE update_state
             SET next_check_at = ?1,
                 lease_token = NULL,
                 lease_until = NULL
             WHERE id = 1 AND lease_token = ?2",
            params![next, lease.token],
        )?;
        Ok(changed == 1)
    }
}

fn default_path(
    configured_directory: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Result<PathBuf, StoreError> {
    if let Some(directory) = configured_directory {
        if directory.as_os_str().is_empty() {
            return Err(StoreError::validation(format!(
                "{UPDATE_STATE_DIR_ENV} cannot be empty"
            )));
        }
        return Ok(directory.join(DATABASE_FILE));
    }
    home.map(|home| home.join(".mach").join(DATABASE_FILE))
        .ok_or_else(|| {
            StoreError::validation("could not determine the home directory for update state")
        })
}

fn initialize_schema(connection: &mut Connection, path: &Path) -> Result<(), StoreError> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != 0 && version != DATABASE_SCHEMA_VERSION {
        return Err(StoreError::UnsupportedDatabaseSchema {
            path: path.to_path_buf(),
            found: version,
            expected: DATABASE_SCHEMA_VERSION,
        });
    }
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS update_state (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            last_successful_check_at INTEGER CHECK (last_successful_check_at >= 0),
            next_check_at INTEGER CHECK (next_check_at >= 0),
            lease_token TEXT,
            lease_until INTEGER CHECK (lease_until >= 0),
            etag TEXT,
            latest_version TEXT,
            CHECK ((lease_token IS NULL) = (lease_until IS NULL))
        ) STRICT;
        INSERT OR IGNORE INTO update_state(id) VALUES (1);
        ",
    )?;
    if version == 0 {
        tx.pragma_update(None, "user_version", DATABASE_SCHEMA_VERSION)?;
    }
    tx.commit()?;
    Ok(())
}

fn quick_check(connection: &Connection) -> Result<(), StoreError> {
    let result: String = connection.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
    if result != "ok" {
        return Err(StoreError::Corrupt(format!(
            "update-state SQLite quick check failed: {result}"
        )));
    }
    Ok(())
}

fn load_state(connection: &Connection) -> Result<UpdateState, StoreError> {
    let state = connection.query_row(
        "SELECT last_successful_check_at, next_check_at, lease_until, etag, latest_version
         FROM update_state WHERE id = 1",
        [],
        |row| {
            Ok(UpdateState {
                last_successful_check_at: row.get(0)?,
                next_check_at: row.get(1)?,
                lease_until: row.get(2)?,
                etag: row.get(3)?,
                latest_version: row.get(4)?,
            })
        },
    )?;
    for (label, timestamp) in [
        (
            "last successful update check",
            state.last_successful_check_at,
        ),
        ("next update check", state.next_check_at),
        ("update-check lease", state.lease_until),
    ] {
        if timestamp.is_some_and(|value| value < 0) {
            return Err(StoreError::Corrupt(format!(
                "{label} timestamp cannot be negative"
            )));
        }
    }
    validate_etag(state.etag.as_deref())?;
    validate_release_version(state.latest_version.as_deref())?;
    if state.etag.is_some() && state.latest_version.is_none() {
        return Err(StoreError::Corrupt(
            "cached release ETag has no matching release version".into(),
        ));
    }
    Ok(state)
}

fn validate_now(now: i64) -> Result<(), StoreError> {
    if now < 0 {
        return Err(StoreError::validation(
            "update-check timestamp cannot be negative",
        ));
    }
    Ok(())
}

fn validate_etag(etag: Option<&str>) -> Result<(), StoreError> {
    if let Some(etag) = etag
        && (etag.len() > MAX_ETAG_BYTES || etag.chars().any(char::is_control))
    {
        return Err(StoreError::validation("invalid update-check ETag"));
    }
    Ok(())
}

fn validate_release_version(version: Option<&str>) -> Result<(), StoreError> {
    let Some(version) = version else {
        return Ok(());
    };
    let parsed = Version::parse(version)
        .map_err(|_| StoreError::validation("invalid cached release version"))?;
    if !parsed.pre.is_empty() || !parsed.build.is_empty() || parsed.to_string() != version {
        return Err(StoreError::validation(
            "cached release version must be a stable canonical semantic version",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_database(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("mach-{name}-{}", uuid::Uuid::new_v4()))
            .join(DATABASE_FILE)
    }

    #[test]
    fn default_update_state_is_global_to_the_user() {
        assert_eq!(
            default_path(None, Some(PathBuf::from("/home/alice"))).unwrap(),
            PathBuf::from("/home/alice/.mach/update.db")
        );
        assert!(default_path(None, None).is_err());
    }

    #[test]
    fn dedicated_directory_override_is_independent_of_the_task_store() {
        assert_eq!(
            default_path(
                Some(PathBuf::from("/tmp/mach-update-state")),
                Some(PathBuf::from("/home/alice"))
            )
            .unwrap(),
            PathBuf::from("/tmp/mach-update-state/update.db")
        );
        assert!(default_path(Some(PathBuf::new()), None).is_err());
    }

    #[test]
    fn success_and_latest_version_persist_across_connections() {
        let path = temp_database("update-success");
        let now = 1_800_000_000;
        let mut first = UpdateStateStore::open(&path).unwrap();
        let AutomaticClaim::Claimed(lease) = first.try_claim_automatic(now).unwrap() else {
            panic!("first check should be due");
        };
        assert!(
            first
                .finish_modified(&lease, now, Some("\"release-etag\""), "0.3.0")
                .unwrap()
        );
        drop(first);

        let second = UpdateStateStore::open(&path).unwrap();
        let state = second.snapshot().unwrap();
        assert_eq!(state.last_successful_check_at, Some(now));
        assert_eq!(state.next_check_at, Some(now + SUCCESS_INTERVAL_SECONDS));
        assert_eq!(state.etag.as_deref(), Some("\"release-etag\""));
        assert_eq!(state.latest_version.as_deref(), Some("0.3.0"));
        drop(second);

        let mut third = UpdateStateStore::open(&path).unwrap();
        let AutomaticClaim::Claimed(lease) = third
            .try_claim_automatic(now + SUCCESS_INTERVAL_SECONDS)
            .unwrap()
        else {
            panic!("the next daily check should be due");
        };
        assert_eq!(lease.etag.as_deref(), Some("\"release-etag\""));
        assert!(
            third
                .finish_not_modified(&lease, now + SUCCESS_INTERVAL_SECONDS)
                .unwrap()
        );
        assert_eq!(
            third.snapshot().unwrap().latest_version.as_deref(),
            Some("0.3.0")
        );
        drop(third);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn failure_releases_the_lease_and_uses_a_short_retry() {
        let mut store = UpdateStateStore::open_in_memory().unwrap();
        let now = 1_800_000_000;
        let AutomaticClaim::Claimed(lease) = store.try_claim_automatic(now).unwrap() else {
            panic!("first check should be due");
        };

        assert!(store.finish_failure(&lease, now, None).unwrap());
        let state = store.snapshot().unwrap();
        assert_eq!(state.last_successful_check_at, None);
        assert_eq!(state.next_check_at, Some(now + FAILURE_RETRY_SECONDS));
        assert!(matches!(
            store
                .try_claim_automatic(now + FAILURE_RETRY_SECONDS)
                .unwrap(),
            AutomaticClaim::Claimed(_)
        ));
    }

    #[test]
    fn server_retry_deadline_can_exceed_the_success_interval() {
        let mut store = UpdateStateStore::open_in_memory().unwrap();
        let now = 1_800_000_000;
        let AutomaticClaim::Claimed(lease) = store.try_claim_automatic(now).unwrap() else {
            panic!("first check should be due");
        };
        assert!(store.finish_modified(&lease, now, None, "0.3.1").unwrap());

        let failed_at = now + SUCCESS_INTERVAL_SECONDS;
        let retry_at = failed_at + 2 * SUCCESS_INTERVAL_SECONDS;
        let AutomaticClaim::Claimed(lease) = store.try_claim_automatic(failed_at).unwrap() else {
            panic!("the next daily check should be due");
        };

        assert!(
            store
                .finish_failure(&lease, failed_at, Some(retry_at))
                .unwrap()
        );
        let state = store.snapshot().unwrap();
        assert_eq!(state.last_successful_check_at, Some(now));
        assert_eq!(state.next_check_at, Some(retry_at));
        assert!(!state.automatic_check_due(failed_at + SUCCESS_INTERVAL_SECONDS));
        assert!(state.automatic_check_due(retry_at));
    }

    #[test]
    fn successful_schedule_recovers_after_the_clock_moves_back() {
        let mut store = UpdateStateStore::open_in_memory().unwrap();
        let now = 1_800_000_000;
        let AutomaticClaim::Claimed(lease) = store.try_claim_automatic(now).unwrap() else {
            panic!("first check should be due");
        };
        assert!(store.finish_modified(&lease, now, None, "0.3.1").unwrap());

        assert!(store.snapshot().unwrap().automatic_check_due(now - 1));
    }

    #[test]
    fn lease_is_shared_across_process_connections() {
        let path = temp_database("update-lease");
        let now = 1_800_000_000;
        let mut first = UpdateStateStore::open(&path).unwrap();
        let mut second = UpdateStateStore::open(&path).unwrap();

        assert!(matches!(
            first.try_claim_automatic(now).unwrap(),
            AutomaticClaim::Claimed(_)
        ));
        assert!(matches!(
            second.try_claim_automatic(now).unwrap(),
            AutomaticClaim::Waiting(_)
        ));
        drop(first);
        drop(second);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn manual_claim_prevents_an_older_worker_from_overwriting_scheduler_state() {
        let mut store = UpdateStateStore::open_in_memory().unwrap();
        let now = 1_800_000_000;
        let AutomaticClaim::Claimed(old) = store.try_claim_automatic(now).unwrap() else {
            panic!("first check should be due");
        };
        let manual = store.claim_manual(now + 1).unwrap();

        assert!(!store.finish_failure(&old, now + 2, None).unwrap());
        assert!(
            store
                .finish_modified(&manual, now + 2, None, "0.3.0")
                .unwrap()
        );
        assert_eq!(
            store.snapshot().unwrap().next_check_at,
            Some(now + 2 + SUCCESS_INTERVAL_SECONDS)
        );
    }
}
