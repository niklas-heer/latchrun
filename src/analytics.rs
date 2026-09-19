//! Persistent, bounded reports over validated metadata. Command contents never enter this store.

use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::ErrorKind,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use nix::{
    fcntl::{Flock, FlockArg},
    unistd::Uid,
};
use rusqlite::{Connection, OpenFlags, config::DbConfig, params};
use serde_json::{Value, json};

use crate::protocol::{Failure, validate_id};

const APPLICATION_ID: i64 = 0x4c_41_54_43;

pub struct Analytics {
    connection: Connection,
    path: PathBuf,
    identity: (u64, u64),
    lock: Flock<File>,
    lock_path: PathBuf,
}

#[derive(Clone)]
pub struct OperationRecord {
    pub session: String,
    pub operation: String,
    pub started_at: u64,
    pub status: String,
    pub duration_ms: Option<u64>,
    pub finished_at: Option<u64>,
    pub cache: String,
    pub provider: String,
    pub origin: String,
}

pub struct ActivityRecord {
    pub id: String,
    pub agent: String,
    pub tool: String,
    pub duration_ms: u64,
    pub success: bool,
    pub at: u64,
    pub source: String,
}

fn unavailable() -> Failure {
    Failure::new(
        "analytics_unavailable",
        "The private analytics database is unavailable or unsafe.",
    )
}

fn invalid_activity() -> Failure {
    Failure::new(
        "invalid_activity",
        "Activity metadata must use valid identifiers, a supported source, and a duration no greater than one day.",
    )
}

fn sql<T>(result: rusqlite::Result<T>) -> Result<T, Failure> {
    result.map_err(|_| unavailable())
}

fn integer(value: u64) -> Result<i64, Failure> {
    i64::try_from(value).map_err(|_| unavailable())
}

fn check_directory(path: &Path) -> Result<(), Failure> {
    let metadata = fs::symlink_metadata(path).map_err(|_| unavailable())?;
    if !metadata.is_dir()
        || metadata.uid() != Uid::current().as_raw()
        || metadata.mode() & 0o7777 != 0o700
    {
        return Err(unavailable());
    }
    Ok(())
}

fn check_file(path: &Path) -> Result<(u64, u64), Failure> {
    let metadata = fs::symlink_metadata(path).map_err(|_| unavailable())?;
    if !metadata.is_file()
        || metadata.uid() != Uid::current().as_raw()
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(unavailable());
    }
    Ok((metadata.dev(), metadata.ino()))
}

fn check_sidecars(path: &Path) -> Result<(), Failure> {
    for suffix in ["-journal", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        let sidecar = PathBuf::from(name);
        match fs::symlink_metadata(&sidecar) {
            Ok(_) => {
                check_file(&sidecar)?;
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(_) => return Err(unavailable()),
        }
    }
    Ok(())
}

fn lock_database(path: &Path) -> Result<Flock<File>, Failure> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(_) => return Err(unavailable()),
    }
    let identity = check_file(path)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| unavailable())?;
    let metadata = file.metadata().map_err(|_| unavailable())?;
    if (metadata.dev(), metadata.ino()) != identity {
        return Err(unavailable());
    }
    Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|_| {
        Failure::new(
            "analytics_in_use",
            "Another service owns this analytics directory.",
        )
    })
}

impl Analytics {
    pub fn open(path: &Path) -> Result<Self, Failure> {
        if !path.is_absolute() {
            return Err(unavailable());
        }
        let parent = path.parent().ok_or_else(unavailable)?;
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|_| unavailable())?;
        check_directory(parent)?;
        let path = parent
            .canonicalize()
            .map_err(|_| unavailable())?
            .join(path.file_name().ok_or_else(unavailable)?);
        let mut lock_name = path.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock_path = PathBuf::from(lock_name);
        let lock = lock_database(&lock_path)?;
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(_) => return Err(unavailable()),
        }
        let identity = check_file(&path)?;
        check_sidecars(&path)?;
        let connection = sql(Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        ))?;
        sql(connection.busy_timeout(Duration::from_millis(250)))?;
        sql(connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true))?;
        sql(connection.execute_batch(
            "PRAGMA trusted_schema=OFF; PRAGMA synchronous=FULL; PRAGMA temp_store=MEMORY;",
        ))?;
        let version: i64 = sql(connection.query_row("PRAGMA user_version", [], |row| row.get(0)))?;
        let application: i64 =
            sql(connection.query_row("PRAGMA application_id", [], |row| row.get(0)))?;
        if version == 0 && application == 0 {
            let tables: i64 = sql(connection.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            ))?;
            if tables != 0 {
                return Err(unavailable());
            }
        } else if version != 1 || application != APPLICATION_ID {
            return Err(unavailable());
        }
        // Reject foreign databases before changing their persistent journal setting.
        sql(connection.execute_batch("PRAGMA journal_mode=DELETE;"))?;
        if version == 0 {
            let transaction = sql(connection.unchecked_transaction())?;
            sql(transaction.execute_batch(SCHEMA))?;
            sql(transaction.pragma_update(None, "application_id", APPLICATION_ID))?;
            sql(transaction.pragma_update(None, "user_version", 1))?;
            sql(transaction.commit())?;
        }
        // The exclusive lifetime lock proves no previous owner can still finish these operations.
        sql(connection.execute(
            "UPDATE operations SET status='unknown' WHERE status IN ('reserved','running')",
            [],
        ))?;
        let analytics = Self {
            connection,
            path,
            identity,
            lock,
            lock_path,
        };
        analytics.check_paths()?;
        Ok(analytics)
    }

    fn check_paths(&self) -> Result<(), Failure> {
        check_directory(self.path.parent().ok_or_else(unavailable)?)?;
        if check_file(&self.path)? != self.identity {
            return Err(unavailable());
        }
        let lock_metadata = self.lock.metadata().map_err(|_| unavailable())?;
        if check_file(&self.lock_path)? != (lock_metadata.dev(), lock_metadata.ino()) {
            return Err(unavailable());
        }
        check_sidecars(&self.path)
    }

    #[cfg(test)]
    pub fn record_operation(&self, record: &OperationRecord) -> Result<(), Failure> {
        self.record_operations(std::slice::from_ref(record))
    }

    pub fn record_operations(&self, records: &[OperationRecord]) -> Result<(), Failure> {
        self.check_paths()?;
        if records.len() > 1024 {
            return Err(unavailable());
        }
        for record in records {
            validate_operation(record)?;
        }
        let transaction = sql(self.connection.unchecked_transaction())?;
        {
            let mut statement = sql(transaction.prepare(UPSERT_OPERATION))?;
            for record in records {
                let changed = sql(statement.execute(params![
                    record.session,
                    record.operation,
                    integer(record.started_at)?,
                    record.status,
                    record.duration_ms.map(integer).transpose()?,
                    record.finished_at.map(integer).transpose()?,
                    record.cache,
                    record.provider,
                    record.origin
                ]))?;
                if changed != 1 {
                    return Err(unavailable());
                }
            }
        }
        sql(transaction.commit())?;
        self.check_paths()
    }

    pub fn record_denial(&self, at: u64, session: Option<&str>) -> Result<(), Failure> {
        self.check_paths()?;
        if let Some(session) = session {
            validate_id(session).map_err(|_| unavailable())?;
        }
        sql(self.connection.execute(
            "INSERT INTO denials(at,session) VALUES (?1,?2)",
            params![integer(at)?, session],
        ))?;
        self.check_paths()
    }

    pub fn record_activity(&self, record: &ActivityRecord) -> Result<(), Failure> {
        self.check_paths()?;
        for id in [&record.id, &record.agent, &record.tool] {
            validate_id(id).map_err(|_| invalid_activity())?;
        }
        if record.duration_ms > 86_400_000 || !matches!(record.source.as_str(), "mcp" | "external")
        {
            return Err(invalid_activity());
        }
        let changed = sql(self.connection.execute(
            "INSERT INTO activity(id,agent,tool,duration_ms,success,at,source) VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(id) DO UPDATE SET id=excluded.id WHERE activity.agent=excluded.agent AND activity.tool=excluded.tool
             AND activity.duration_ms=excluded.duration_ms AND activity.success=excluded.success AND activity.source=excluded.source",
            params![record.id, record.agent, record.tool, integer(record.duration_ms)?, record.success, integer(record.at)?, record.source]))?;
        if changed != 1 {
            return Err(Failure::new(
                "activity_conflict",
                "This activity identifier already belongs to a different event.",
            ));
        }
        self.check_paths()
    }

    pub fn query(&self, days: u16) -> Result<Value, Failure> {
        let until = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| unavailable())?
            .as_secs();
        self.query_at(days, until)
    }

    fn query_at(&self, days: u16, until: u64) -> Result<Value, Failure> {
        self.check_paths()?;
        if !matches!(days, 1 | 7 | 30 | 90) {
            return Err(Failure::new(
                "invalid_window",
                "Analytics supports 1, 7, 30, or 90 days.",
            ));
        }
        let (width, buckets) = if days == 1 {
            (3600_u64, 24_u64)
        } else {
            (86_400, u64::from(days))
        };
        let since = (until / width).saturating_sub(buckets - 1) * width;
        let end = until.checked_add(1).ok_or_else(unavailable)?;
        // One read transaction gives all report sections the same database snapshot.
        let transaction = sql(self.connection.unchecked_transaction())?;
        let totals = self.operation_totals(since, end)?;
        let latency = self.latency(since, end)?;
        let mut timeline = Vec::with_capacity(usize::from(days.max(24)));
        for start in (since..=until).step_by(usize::try_from(width).map_err(|_| unavailable())?) {
            let stop = (start + width).min(end);
            let totals = self.operation_totals(start, stop)?;
            let latency = self.latency(start, stop)?;
            timeline.push(
                json!({"start":start,"accepted":totals["accepted"],"succeeded":totals["succeeded"],
                "failed":totals["failed"],"unknown":totals["unknown"],"denied":totals["denied"],
                "p50_ms":latency["p50_ms"],"p95_ms":latency["p95_ms"],
                "cache_hits":totals["hits"],"cache_misses":totals["misses"]}),
            );
        }
        let (agents, agents_truncated) = self.agents(since, end)?;
        let activity = sql(self.connection.query_row(
            "SELECT count(*),coalesce(sum(NOT success),0) FROM activity WHERE at>=?1 AND at<?2",
            params![integer(since)?, integer(end)?],
            |row| Ok(json!({"calls":row.get::<_,i64>(0)?,"errors":row.get::<_,i64>(1)?})),
        ))?;
        sql(transaction.commit())?;
        self.check_paths()?;
        Ok(json!({"days":days,"since":since,"until":until,
            "totals":{"accepted":totals["accepted"],"succeeded":totals["succeeded"],"failed":totals["failed"],
                "unknown":totals["unknown"],"running":totals["running"],"denied":totals["denied"],"sessions":totals["sessions"]},
            "latency":latency,"cache":{"hits":totals["hits"],"misses":totals["misses"],"hit_rate_pct":totals["hit_rate_pct"]},
            "timeline":timeline,"agents":agents,"agents_truncated":agents_truncated,"activity":activity}))
    }

    fn operation_totals(&self, since: u64, end: u64) -> Result<Value, Failure> {
        sql(self.connection.query_row(
            "SELECT count(*),coalesce(sum(status='succeeded'),0),coalesce(sum(status='failed'),0),
             coalesce(sum(status='unknown'),0),coalesce(sum(status IN ('reserved','running')),0),count(DISTINCT session),
             coalesce(sum(cache='hit'),0),coalesce(sum(cache='miss'),0),
             100.0*sum(cache='hit')/nullif(sum(cache IN ('hit','miss')),0),
             (SELECT count(*) FROM denials WHERE at>=?1 AND at<?2) FROM operations WHERE started_at>=?1 AND started_at<?2",
            params![integer(since)?,integer(end)?], |row| Ok(json!({"accepted":row.get::<_,i64>(0)?,"succeeded":row.get::<_,i64>(1)?,
                "failed":row.get::<_,i64>(2)?,"unknown":row.get::<_,i64>(3)?,"running":row.get::<_,i64>(4)?,
                "sessions":row.get::<_,i64>(5)?,"hits":row.get::<_,i64>(6)?,"misses":row.get::<_,i64>(7)?,
                "hit_rate_pct":row.get::<_,Option<f64>>(8)?,"denied":row.get::<_,i64>(9)?}))))
    }

    fn latency(&self, since: u64, end: u64) -> Result<Value, Failure> {
        sql(self.connection.query_row(
            "WITH ranked AS (SELECT duration_ms,row_number() OVER (ORDER BY duration_ms) AS rank,count(*) OVER () AS n
             FROM operations WHERE started_at>=?1 AND started_at<?2 AND duration_ms IS NOT NULL AND status IN ('succeeded','failed'))
             SELECT count(*),max(CASE WHEN rank=(n*50+99)/100 THEN duration_ms END),
             max(CASE WHEN rank=(n*95+99)/100 THEN duration_ms END),max(CASE WHEN rank=(n*99+99)/100 THEN duration_ms END) FROM ranked",
            params![integer(since)?,integer(end)?], |row| Ok(json!({"samples":row.get::<_,i64>(0)?,"p50_ms":row.get::<_,Option<i64>>(1)?,
                "p95_ms":row.get::<_,Option<i64>>(2)?,"p99_ms":row.get::<_,Option<i64>>(3)?}))))
    }

    fn agents(&self, since: u64, end: u64) -> Result<(Vec<Value>, bool), Failure> {
        let mut statement = sql(self.connection.prepare(
            "WITH ranked AS (SELECT agent,tool,source,duration_ms,success,
             row_number() OVER (PARTITION BY agent,tool,source ORDER BY duration_ms) AS rank,
             count(*) OVER (PARTITION BY agent,tool,source) AS n FROM activity WHERE at>=?1 AND at<?2)
             SELECT agent,tool,source,count(*) AS calls,sum(NOT success),
             max(CASE WHEN rank=(n*50+99)/100 THEN duration_ms END),max(CASE WHEN rank=(n*95+99)/100 THEN duration_ms END)
             FROM ranked GROUP BY agent,tool,source ORDER BY calls DESC,agent,tool,source LIMIT 101"))?;
        let rows = sql(statement.query_map(params![integer(since)?, integer(end)?], |row| Ok(json!({
            "agent":row.get::<_,String>(0)?,"tool":row.get::<_,String>(1)?,"source":row.get::<_,String>(2)?,
            "calls":row.get::<_,i64>(3)?,"errors":row.get::<_,i64>(4)?,"p50_ms":row.get::<_,i64>(5)?,"p95_ms":row.get::<_,i64>(6)?}))))?;
        let mut agents = sql(rows.collect::<rusqlite::Result<Vec<_>>>())?;
        let truncated = agents.len() > 100;
        agents.truncate(100);
        Ok((agents, truncated))
    }
}

fn validate_operation(record: &OperationRecord) -> Result<(), Failure> {
    validate_id(&record.session).map_err(|_| unavailable())?;
    validate_id(&record.operation).map_err(|_| unavailable())?;
    if !matches!(
        record.status.as_str(),
        "reserved" | "running" | "succeeded" | "failed" | "unknown"
    ) || !matches!(
        record.cache.as_str(),
        "unknown" | "none" | "disabled" | "hit" | "miss"
    ) || !matches!(
        record.provider.as_str(),
        "unknown" | "fake" | "one_password" | "file" | "password_store"
    ) || !matches!(record.origin.as_str(), "unknown" | "cli" | "mcp")
    {
        return Err(unavailable());
    }
    integer(record.started_at)?;
    record.duration_ms.map(integer).transpose()?;
    record.finished_at.map(integer).transpose()?;
    Ok(())
}

const SCHEMA: &str = "
CREATE TABLE operations(session TEXT NOT NULL,operation TEXT NOT NULL,started_at INTEGER NOT NULL,status TEXT NOT NULL,
 duration_ms INTEGER,finished_at INTEGER,cache TEXT NOT NULL,provider TEXT NOT NULL,origin TEXT NOT NULL,
 PRIMARY KEY(session,operation)) STRICT;
CREATE INDEX operations_started ON operations(started_at);
CREATE TABLE denials(id INTEGER PRIMARY KEY,at INTEGER NOT NULL,session TEXT) STRICT;
CREATE INDEX denials_at ON denials(at);
CREATE TABLE activity(id TEXT PRIMARY KEY NOT NULL,agent TEXT NOT NULL,tool TEXT NOT NULL,duration_ms INTEGER NOT NULL,
 success INTEGER NOT NULL,at INTEGER NOT NULL,source TEXT NOT NULL) STRICT;
CREATE INDEX activity_at ON activity(at);
";

const UPSERT_OPERATION: &str = "
INSERT INTO operations(session,operation,started_at,status,duration_ms,finished_at,cache,provider,origin)
 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
ON CONFLICT(session,operation) DO UPDATE SET
 status=CASE WHEN operations.status IN ('succeeded','failed') OR (operations.status='unknown' AND excluded.status IN ('reserved','running')) THEN operations.status ELSE excluded.status END,
 duration_ms=coalesce(operations.duration_ms,excluded.duration_ms),finished_at=coalesce(operations.finished_at,excluded.finished_at),
 cache=CASE WHEN excluded.cache='unknown' THEN operations.cache ELSE excluded.cache END,
 provider=CASE WHEN excluded.provider='unknown' THEN operations.provider ELSE excluded.provider END,
 origin=CASE WHEN excluded.origin='unknown' THEN operations.origin ELSE excluded.origin END
WHERE operations.started_at=excluded.started_at;";

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    const NOW: u64 = 1_800_000_123;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            Self(std::env::temp_dir().canonicalize().unwrap().join(format!(
                "latchrun-analytics-{}",
                crate::protocol::random_id().unwrap()
            )))
        }

        fn path(&self) -> PathBuf {
            self.0.join("analytics.sqlite3")
        }
        fn open(&self) -> Analytics {
            Analytics::open(&self.path()).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn operation(id: u64, status: &str, duration: Option<u64>) -> OperationRecord {
        OperationRecord {
            session: "session".into(),
            operation: format!("operation_{id}"),
            started_at: NOW - 10,
            status: status.into(),
            duration_ms: duration,
            finished_at: duration.map(|_| NOW),
            cache: "miss".into(),
            provider: "fake".into(),
            origin: "cli".into(),
        }
    }

    fn activity(id: &str) -> ActivityRecord {
        ActivityRecord {
            id: id.into(),
            agent: "codex".into(),
            tool: "run".into(),
            duration_ms: 10,
            success: true,
            at: NOW - 10,
            source: "mcp".into(),
        }
    }

    #[test]
    fn empty_reports_are_zero_filled_and_windows_are_validated() {
        let fixture = Fixture::new();
        let database = fixture.open();
        for (days, buckets) in [(1, 24), (7, 7), (30, 30), (90, 90)] {
            let report = database.query_at(days, NOW).unwrap();
            assert_eq!(report["timeline"].as_array().unwrap().len(), buckets);
            assert_eq!(report["totals"]["accepted"], 0);
            assert!(report["latency"]["p50_ms"].is_null());
            assert!(report["cache"]["hit_rate_pct"].is_null());
            assert_eq!(report["agents"], json!([]));
            assert_eq!(
                report["since"].as_u64().unwrap() % if days == 1 { 3600 } else { 86400 },
                0
            );
        }
        assert_eq!(
            database.query_at(2, NOW).unwrap_err().code,
            "invalid_window"
        );
    }

    #[test]
    fn exact_percentiles_status_cohorts_and_cache_survive_reopen() {
        let fixture = Fixture::new();
        let database = fixture.open();
        let mut records = (1..=100)
            .map(|id| {
                operation(
                    id,
                    if id % 2 == 0 { "succeeded" } else { "failed" },
                    Some(id),
                )
            })
            .collect::<Vec<_>>();
        for record in &mut records[..25] {
            record.cache = "hit".into();
        }
        records.push(operation(101, "unknown", None));
        records.push(operation(102, "running", None));
        records.push(operation(103, "reserved", None));
        records[100].cache = "none".into();
        records[101].cache = "unknown".into();
        records[102].cache = "disabled".into();
        let mut old = operation(104, "succeeded", Some(999));
        old.started_at = NOW - 90 * 86_400;
        records.push(old);
        database.record_operations(&records).unwrap();
        database.record_denial(NOW - 2, Some("session")).unwrap();
        database.record_denial(NOW - 1, None).unwrap();
        drop(database);
        let database = fixture.open();
        let report = database.query_at(1, NOW).unwrap();
        assert_eq!(
            report["totals"],
            json!({"accepted":103,"succeeded":50,"failed":50,"unknown":3,"running":0,"denied":2,"sessions":1})
        );
        assert_eq!(
            report["latency"],
            json!({"samples":100,"p50_ms":50,"p95_ms":95,"p99_ms":99})
        );
        assert_eq!(
            report["cache"],
            json!({"hits":25,"misses":75,"hit_rate_pct":25.0})
        );
        let buckets = report["timeline"].as_array().unwrap();
        assert_eq!(
            buckets
                .iter()
                .map(|b| b["accepted"].as_u64().unwrap())
                .sum::<u64>(),
            103
        );
        assert_eq!(
            buckets
                .iter()
                .map(|b| b["denied"].as_u64().unwrap())
                .sum::<u64>(),
            2
        );
    }

    #[test]
    fn operation_sync_is_atomic_and_preserves_final_metadata() {
        let fixture = Fixture::new();
        let database = fixture.open();
        let mut record = operation(1, "reserved", None);
        database.record_operation(&record).unwrap();
        record.status = "succeeded".into();
        record.duration_ms = Some(123);
        record.finished_at = Some(NOW);
        record.cache = "hit".into();
        database.record_operation(&record).unwrap();
        record.status = "running".into();
        record.cache = "unknown".into();
        record.provider = "unknown".into();
        record.origin = "unknown".into();
        record.duration_ms = None;
        database.record_operation(&record).unwrap();
        let report = database.query_at(1, NOW).unwrap();
        assert_eq!(report["totals"]["succeeded"], 1);
        assert_eq!(report["cache"]["hits"], 1);
        assert_eq!(report["latency"]["p50_ms"], 123);
        let metadata: (String, String) = database
            .connection
            .query_row("SELECT provider,origin FROM operations", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(metadata, ("fake".into(), "cli".into()));
        record.started_at += 1;
        assert!(
            database
                .record_operations(&[operation(2, "succeeded", Some(1)), record])
                .is_err()
        );
        assert_eq!(database.query_at(1, NOW).unwrap()["totals"]["accepted"], 1);
    }

    #[test]
    fn activity_deduplicates_server_timestamps_rejects_conflicts_and_bounds_groups() {
        let fixture = Fixture::new();
        let database = fixture.open();
        let mut record = activity("event");
        database.record_activity(&record).unwrap();
        record.at = NOW + 86_400;
        database.record_activity(&record).unwrap();
        record.success = false;
        assert_eq!(
            database.record_activity(&record).unwrap_err().code,
            "activity_conflict"
        );
        assert_eq!(
            database.query_at(1, NOW).unwrap()["activity"],
            json!({"calls":1,"errors":0})
        );
        for index in 0..101 {
            let mut record = activity(&format!("event_{index}"));
            record.agent = format!("agent_{index}");
            record.success = false;
            database.record_activity(&record).unwrap();
        }
        let report = database.query_at(1, NOW).unwrap();
        assert_eq!(report["agents"].as_array().unwrap().len(), 100);
        assert_eq!(report["agents_truncated"], true);
        assert_eq!(report["activity"], json!({"calls":102,"errors":101}));
        record.id = "not an identifier".into();
        assert_eq!(
            database.record_activity(&record).unwrap_err().code,
            "invalid_activity"
        );
    }

    #[test]
    fn unsafe_permissions_links_and_sidecars_fail_without_repairs() {
        let fixture = Fixture::new();
        let database = fixture.open();
        fs::set_permissions(fixture.path(), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(database.record_denial(NOW, None).is_err());
        assert!(Analytics::open(&fixture.path()).is_err());
        assert_eq!(fs::metadata(fixture.path()).unwrap().mode() & 0o777, 0o644);
        fs::set_permissions(fixture.path(), fs::Permissions::from_mode(0o600)).unwrap();
        let target = fixture.0.join("target");
        fs::write(&target, b"fake-private-fixture").unwrap();
        let sidecar = fixture.0.join("analytics.sqlite3-journal");
        symlink(&target, &sidecar).unwrap();
        assert!(database.query_at(1, NOW).is_err());
        assert!(Analytics::open(&fixture.path()).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"fake-private-fixture");
        fs::remove_file(sidecar).unwrap();
        let link = fixture.0.join("hardlink");
        fs::hard_link(fixture.path(), &link).unwrap();
        assert!(Analytics::open(&fixture.path()).is_err());
        fs::remove_file(link).unwrap();
        drop(database);
        fs::rename(fixture.path(), fixture.0.join("original")).unwrap();
        symlink(fixture.0.join("original"), fixture.path()).unwrap();
        assert!(Analytics::open(&fixture.path()).is_err());
    }

    #[test]
    fn schema_and_database_replacement_are_rejected() {
        let fixture = Fixture::new();
        let database = fixture.open();
        database
            .connection
            .pragma_update(None, "user_version", 2)
            .unwrap();
        assert!(Analytics::open(&fixture.path()).is_err());
        database
            .connection
            .pragma_update(None, "user_version", 1)
            .unwrap();
        fs::rename(fixture.path(), fixture.0.join("previous")).unwrap();
        assert!(database.record_denial(NOW, None).is_err());
        drop(database);
        let replacement = fixture.open();
        assert_eq!(
            replacement.query_at(1, NOW).unwrap()["totals"]["accepted"],
            0
        );
    }

    #[test]
    fn utc_boundaries_and_activity_percentiles_use_exact_nearest_rank() {
        let fixture = Fixture::new();
        let database = fixture.open();
        let since = database.query_at(1, NOW).unwrap()["since"]
            .as_u64()
            .unwrap();
        for (id, at) in [since - 1, since, NOW, NOW + 1].into_iter().enumerate() {
            let mut record = operation(u64::try_from(id).unwrap(), "succeeded", Some(1));
            record.started_at = at;
            database.record_operation(&record).unwrap();
        }
        for duration in 1..=20 {
            let mut record = activity(&format!("sample_{duration}"));
            record.duration_ms = duration;
            record.success = duration % 2 == 0;
            database.record_activity(&record).unwrap();
        }
        let report = database.query_at(1, NOW).unwrap();
        assert_eq!(report["totals"]["accepted"], 2);
        assert_eq!(report["timeline"][0]["accepted"], 1);
        assert_eq!(report["timeline"][23]["accepted"], 1);
        assert_eq!(
            report["agents"][0],
            json!({"agent":"codex","tool":"run","source":"mcp","calls":20,"errors":10,"p50_ms":10,"p95_ms":19})
        );
    }

    #[test]
    fn data_directory_creation_respects_existing_ancestors_and_rejects_unsafe_final_directory() {
        let fixture = Fixture::new();
        fs::create_dir(&fixture.0).unwrap();
        fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o755)).unwrap();
        let nested = fixture.0.join("new/private/analytics.sqlite3");
        let database = Analytics::open(&nested).unwrap();
        assert_eq!(fs::metadata(&fixture.0).unwrap().mode() & 0o777, 0o755);
        assert_eq!(
            fs::metadata(nested.parent().unwrap()).unwrap().mode() & 0o777,
            0o700
        );
        assert!(Analytics::open(&fixture.path()).is_err());
        assert!(!fixture.path().exists());
        let linked = fixture.0.join("linked");
        symlink(nested.parent().unwrap(), &linked).unwrap();
        assert!(Analytics::open(&linked.join("analytics.sqlite3")).is_err());
        drop(database);
    }

    #[test]
    fn foreign_database_is_rejected_without_changing_its_journal_mode() {
        let fixture = Fixture::new();
        let database = fixture.open();
        database
            .connection
            .pragma_update(None, "user_version", 99)
            .unwrap();
        database
            .connection
            .execute_batch("PRAGMA journal_mode=WAL;")
            .unwrap();
        drop(database);
        assert!(Analytics::open(&fixture.path()).is_err());
        let connection = Connection::open(fixture.path()).unwrap();
        let mode: String = connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn exclusive_store_ownership_recovers_abandoned_operations_without_false_latency() {
        let fixture = Fixture::new();
        let database = fixture.open();
        assert!(
            matches!(Analytics::open(&fixture.path()), Err(error) if error.code == "analytics_in_use")
        );
        database
            .record_operation(&operation(1, "running", None))
            .unwrap();
        database
            .record_operation(&operation(2, "reserved", None))
            .unwrap();
        database
            .record_operation(&operation(3, "unknown", Some(999)))
            .unwrap();
        database
            .record_operation(&operation(4, "succeeded", Some(10)))
            .unwrap();
        drop(database);
        let database = fixture.open();
        let report = database.query_at(1, NOW).unwrap();
        assert_eq!(report["totals"]["running"], 0);
        assert_eq!(report["totals"]["unknown"], 3);
        assert_eq!(
            report["latency"],
            json!({"samples":1,"p50_ms":10,"p95_ms":10,"p99_ms":10})
        );
        let lock_path = fixture.0.join("analytics.sqlite3.lock");
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(database.record_denial(NOW, None).is_err());
        drop(database);
        assert!(Analytics::open(&fixture.path()).is_err());
        fs::remove_file(&lock_path).unwrap();
        symlink(fixture.path(), &lock_path).unwrap();
        assert!(Analytics::open(&fixture.path()).is_err());
    }
}
