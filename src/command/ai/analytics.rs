use anyhow::{Context, Result, anyhow};
use serde_json::Deserializer;
use sha2::{Digest, Sha256};
use sqlx::{
    Connection, Row, SqliteConnection,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous},
};
use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::runtime::Builder;

const ANALYTICS_DB_NAME: &str = "analytics.db";
const LEGACY_ANALYTICS_LOG_NAME: &str = "analytics.jsonl";

pub(super) fn append_record(record: &super::AiAnalyticsRecord) -> Result<()> {
    append_record_with_paths(&db_path_candidates(), &legacy_log_candidates(), record)
}

pub(super) fn load_records(
    days: u64,
    workspace: Option<&str>,
) -> Result<Vec<super::AiAnalyticsRecord>> {
    load_records_with_paths(
        &db_path_candidates(),
        &legacy_log_candidates(),
        days,
        workspace,
    )
}

fn append_record_with_paths(
    db_candidates: &[PathBuf],
    legacy_candidates: &[PathBuf],
    record: &super::AiAnalyticsRecord,
) -> Result<()> {
    with_runtime(async {
        let (mut conn, _) = open_writable_database(db_candidates).await?;
        migrate_legacy_logs(&mut conn, legacy_candidates).await?;
        insert_record(&mut conn, record).await
    })
}

fn load_records_with_paths(
    db_candidates: &[PathBuf],
    legacy_candidates: &[PathBuf],
    days: u64,
    workspace: Option<&str>,
) -> Result<Vec<super::AiAnalyticsRecord>> {
    with_runtime(async {
        let cutoff_ms = cutoff_unix_ms(days);
        let mut fingerprints = HashSet::new();
        let mut records = Vec::new();

        for path in existing_paths(db_candidates) {
            let mut conn = open_existing_database(&path).await?;
            for record in query_records(&mut conn, cutoff_ms, workspace).await? {
                merge_record(&mut fingerprints, &mut records, record);
            }
        }

        for path in existing_paths(legacy_candidates) {
            for record in read_legacy_records(&path)? {
                if record.recorded_at_unix_ms < cutoff_ms {
                    continue;
                }
                if workspace.is_some_and(|current| current != record.workspace) {
                    continue;
                }
                merge_record(&mut fingerprints, &mut records, record);
            }
        }

        records.sort_by(|left, right| {
            right
                .recorded_at_unix_ms
                .cmp(&left.recorded_at_unix_ms)
                .then_with(|| left.route.cmp(&right.route))
                .then_with(|| left.source_command.cmp(&right.source_command))
        });
        Ok(records)
    })
}

fn with_runtime<T>(future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create ai analytics tokio runtime")?
        .block_on(future)
}

async fn open_writable_database(candidates: &[PathBuf]) -> Result<(SqliteConnection, PathBuf)> {
    let mut last_error = None;

    for path in candidates {
        if let Some(parent) = path.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            last_error = Some(anyhow!(
                "create ai analytics state dir {}: {error}",
                parent.display()
            ));
            continue;
        }

        let existed = path.exists();
        match open_connection(path, true).await {
            Ok(mut conn) => {
                if let Err(error) = ensure_schema(&mut conn).await {
                    if existed {
                        return Err(error).with_context(|| {
                            format!("initialize ai analytics db {}", path.display())
                        });
                    }
                    last_error = Some(
                        error.context(format!("initialize ai analytics db {}", path.display())),
                    );
                    continue;
                }
                return Ok((conn, path.clone()));
            }
            Err(error) if existed => {
                return Err(error)
                    .with_context(|| format!("open ai analytics db {}", path.display()));
            }
            Err(error) => {
                last_error =
                    Some(error.context(format!("open ai analytics db {}", path.display())));
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("no ai analytics database candidates available")))
}

async fn open_existing_database(path: &Path) -> Result<SqliteConnection> {
    let mut conn = open_connection(path, false)
        .await
        .with_context(|| format!("open ai analytics db {}", path.display()))?;
    ensure_schema(&mut conn)
        .await
        .with_context(|| format!("initialize ai analytics db {}", path.display()))?;
    Ok(conn)
}

async fn open_connection(path: &Path, create_if_missing: bool) -> Result<SqliteConnection> {
    let mut options = SqliteConnectOptions::new()
        .filename(path)
        .busy_timeout(Duration::from_secs(2))
        .create_if_missing(create_if_missing);
    if create_if_missing {
        options = options
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal);
    }
    SqliteConnection::connect_with(&options)
        .await
        .with_context(|| format!("connect sqlite {}", path.display()))
}

async fn ensure_schema(conn: &mut SqliteConnection) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS ai_events (
            fingerprint TEXT PRIMARY KEY,
            schema_version INTEGER NOT NULL,
            recorded_at_unix_ms INTEGER NOT NULL,
            agent TEXT NOT NULL,
            workspace TEXT NOT NULL,
            route TEXT NOT NULL,
            source_command TEXT NOT NULL,
            raw_bytes INTEGER NOT NULL,
            summary_bytes INTEGER NOT NULL,
            raw_estimated_tokens INTEGER NOT NULL,
            summary_estimated_tokens INTEGER NOT NULL,
            duration_ms INTEGER NOT NULL
        )
        "#,
    )
    .execute(&mut *conn)
    .await
    .context("create ai analytics table")?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS ai_events_workspace_recorded_at_idx ON ai_events (workspace, recorded_at_unix_ms)",
    )
    .execute(&mut *conn)
    .await
    .context("create ai analytics workspace index")?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS ai_events_route_recorded_at_idx ON ai_events (route, recorded_at_unix_ms)",
    )
    .execute(&mut *conn)
    .await
    .context("create ai analytics route index")?;

    Ok(())
}

async fn insert_record(
    conn: &mut SqliteConnection,
    record: &super::AiAnalyticsRecord,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT OR IGNORE INTO ai_events (
            fingerprint,
            schema_version,
            recorded_at_unix_ms,
            agent,
            workspace,
            route,
            source_command,
            raw_bytes,
            summary_bytes,
            raw_estimated_tokens,
            summary_estimated_tokens,
            duration_ms
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(record_fingerprint(record))
    .bind(i64::from(record.schema_version))
    .bind(i64_from_u64(
        record.recorded_at_unix_ms,
        "recorded_at_unix_ms",
    )?)
    .bind(&record.agent)
    .bind(&record.workspace)
    .bind(&record.route)
    .bind(&record.source_command)
    .bind(i64_from_u64(record.raw_bytes, "raw_bytes")?)
    .bind(i64_from_u64(record.summary_bytes, "summary_bytes")?)
    .bind(i64_from_u64(
        record.raw_estimated_tokens,
        "raw_estimated_tokens",
    )?)
    .bind(i64_from_u64(
        record.summary_estimated_tokens,
        "summary_estimated_tokens",
    )?)
    .bind(i64_from_u64(record.duration_ms, "duration_ms")?)
    .execute(&mut *conn)
    .await
    .context("insert ai analytics record")?;
    Ok(())
}

async fn query_records(
    conn: &mut SqliteConnection,
    cutoff_ms: u64,
    workspace: Option<&str>,
) -> Result<Vec<super::AiAnalyticsRecord>> {
    let cutoff_ms = i64_from_u64(cutoff_ms, "cutoff_ms")?;
    let rows = if let Some(workspace) = workspace {
        sqlx::query(
            r#"
            SELECT
                schema_version,
                recorded_at_unix_ms,
                agent,
                workspace,
                route,
                source_command,
                raw_bytes,
                summary_bytes,
                raw_estimated_tokens,
                summary_estimated_tokens,
                duration_ms
            FROM ai_events
            WHERE recorded_at_unix_ms >= ? AND workspace = ?
            ORDER BY recorded_at_unix_ms DESC
            "#,
        )
        .bind(cutoff_ms)
        .bind(workspace)
        .fetch_all(&mut *conn)
        .await
        .context("query ai analytics records for workspace")?
    } else {
        sqlx::query(
            r#"
            SELECT
                schema_version,
                recorded_at_unix_ms,
                agent,
                workspace,
                route,
                source_command,
                raw_bytes,
                summary_bytes,
                raw_estimated_tokens,
                summary_estimated_tokens,
                duration_ms
            FROM ai_events
            WHERE recorded_at_unix_ms >= ?
            ORDER BY recorded_at_unix_ms DESC
            "#,
        )
        .bind(cutoff_ms)
        .fetch_all(&mut *conn)
        .await
        .context("query ai analytics records")?
    };

    rows.into_iter().map(row_to_record).collect()
}

async fn migrate_legacy_logs(
    conn: &mut SqliteConnection,
    legacy_candidates: &[PathBuf],
) -> Result<()> {
    for path in existing_paths(legacy_candidates) {
        let records = read_legacy_records(&path)?;
        for record in &records {
            insert_record(conn, record).await?;
        }
        mark_legacy_log_migrated(&path)
            .with_context(|| format!("finalize legacy ai analytics log {}", path.display()))?;
    }
    Ok(())
}

fn read_legacy_records(path: &Path) -> Result<Vec<super::AiAnalyticsRecord>> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("read legacy ai analytics log {}", path.display()))?;
    let mut records = Vec::new();

    for record in Deserializer::from_str(&contents)
        .into_iter::<super::AiAnalyticsRecord>()
        .flatten()
    {
        records.push(record);
    }

    Ok(records)
}

fn mark_legacy_log_migrated(path: &Path) -> Result<()> {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(LEGACY_ANALYTICS_LOG_NAME);
    let migrated = path.with_file_name(format!("{file_name}.migrated"));
    if migrated.exists() {
        fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
        return Ok(());
    }
    fs::rename(path, &migrated)
        .with_context(|| format!("rename {} -> {}", path.display(), migrated.display()))
}

fn row_to_record(row: sqlx::sqlite::SqliteRow) -> Result<super::AiAnalyticsRecord> {
    Ok(super::AiAnalyticsRecord {
        schema_version: u8::try_from(row.try_get::<i64, _>("schema_version")?)
            .context("decode schema_version")?,
        recorded_at_unix_ms: u64_from_i64(
            row.try_get::<i64, _>("recorded_at_unix_ms")?,
            "recorded_at_unix_ms",
        )?,
        agent: row.try_get("agent").context("decode agent")?,
        workspace: row.try_get("workspace").context("decode workspace")?,
        route: row.try_get("route").context("decode route")?,
        source_command: row
            .try_get("source_command")
            .context("decode source_command")?,
        raw_bytes: u64_from_i64(row.try_get::<i64, _>("raw_bytes")?, "raw_bytes")?,
        summary_bytes: u64_from_i64(row.try_get::<i64, _>("summary_bytes")?, "summary_bytes")?,
        raw_estimated_tokens: u64_from_i64(
            row.try_get::<i64, _>("raw_estimated_tokens")?,
            "raw_estimated_tokens",
        )?,
        summary_estimated_tokens: u64_from_i64(
            row.try_get::<i64, _>("summary_estimated_tokens")?,
            "summary_estimated_tokens",
        )?,
        duration_ms: u64_from_i64(row.try_get::<i64, _>("duration_ms")?, "duration_ms")?,
    })
}

fn merge_record(
    fingerprints: &mut HashSet<String>,
    records: &mut Vec<super::AiAnalyticsRecord>,
    record: super::AiAnalyticsRecord,
) {
    let fingerprint = record_fingerprint(&record);
    if fingerprints.insert(fingerprint) {
        records.push(record);
    }
}

fn record_fingerprint(record: &super::AiAnalyticsRecord) -> String {
    let mut hasher = Sha256::new();
    hasher.update([record.schema_version]);
    hasher.update(record.recorded_at_unix_ms.to_le_bytes());
    hasher.update(record.agent.as_bytes());
    hasher.update([0]);
    hasher.update(record.workspace.as_bytes());
    hasher.update([0]);
    hasher.update(record.route.as_bytes());
    hasher.update([0]);
    hasher.update(record.source_command.as_bytes());
    hasher.update([0]);
    hasher.update(record.raw_bytes.to_le_bytes());
    hasher.update(record.summary_bytes.to_le_bytes());
    hasher.update(record.raw_estimated_tokens.to_le_bytes());
    hasher.update(record.summary_estimated_tokens.to_le_bytes());
    hasher.update(record.duration_ms.to_le_bytes());
    hex_string(&hasher.finalize())
}

fn hex_string(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn cutoff_unix_ms(days: u64) -> u64 {
    let lookback_ms = days.saturating_mul(24 * 60 * 60 * 1000);
    super::unix_timestamp_ms().saturating_sub(lookback_ms)
}

fn db_path_candidates() -> Vec<PathBuf> {
    candidate_paths(ANALYTICS_DB_NAME)
}

fn legacy_log_candidates() -> Vec<PathBuf> {
    candidate_paths(LEGACY_ANALYTICS_LOG_NAME)
}

fn candidate_paths(file_name: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(state_home) = state_home_dir() {
        paths.push(state_home.join("za/ai").join(file_name));
    }
    paths.push(env::temp_dir().join("za-state/ai").join(file_name));
    dedupe_paths(paths)
}

fn state_home_dir() -> Option<PathBuf> {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
}

fn existing_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths.iter().filter(|path| path.exists()).cloned().collect()
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for path in paths {
        let key = path.display().to_string();
        if seen.insert(key) {
            out.push(path);
        }
    }
    out
}

fn i64_from_u64(value: u64, field: &str) -> Result<i64> {
    i64::try_from(value).with_context(|| format!("encode {field} as sqlite integer"))
}

fn u64_from_i64(value: i64, field: &str) -> Result<u64> {
    u64::try_from(value).with_context(|| format!("decode {field} from sqlite integer"))
}

#[cfg(test)]
mod tests {
    use super::{
        append_record_with_paths, load_records_with_paths, mark_legacy_log_migrated,
        record_fingerprint,
    };
    use crate::command::ai::AiAnalyticsRecord;
    use std::{fs, path::PathBuf};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "za-ai-analytics-test-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn sample_record(route: &str, recorded_at_unix_ms: u64) -> AiAnalyticsRecord {
        AiAnalyticsRecord {
            schema_version: 1,
            recorded_at_unix_ms,
            agent: "codex".to_string(),
            workspace: "/opt/app/za".to_string(),
            route: route.to_string(),
            source_command: route.to_string(),
            raw_bytes: 1200,
            summary_bytes: 300,
            raw_estimated_tokens: 300,
            summary_estimated_tokens: 75,
            duration_ms: 12,
        }
    }

    #[test]
    fn sqlite_storage_round_trips_and_migrates_legacy_jsonl() {
        let dir = temp_dir("roundtrip");
        let db_path = dir.join("analytics.db");
        let legacy_path = dir.join("analytics.jsonl");
        let legacy_record = sample_record("git status", super::super::unix_timestamp_ms());
        fs::write(
            &legacy_path,
            format!(
                "{}\n",
                serde_json::to_string(&legacy_record).expect("serialize legacy record")
            ),
        )
        .expect("write legacy log");

        append_record_with_paths(
            std::slice::from_ref(&db_path),
            std::slice::from_ref(&legacy_path),
            &sample_record("git diff", super::super::unix_timestamp_ms()),
        )
        .expect("append record");

        let records = load_records_with_paths(
            std::slice::from_ref(&db_path),
            &[
                legacy_path.clone(),
                legacy_path.with_file_name("analytics.jsonl.migrated"),
            ],
            30,
            Some("/opt/app/za"),
        )
        .expect("load records");

        assert_eq!(records.len(), 2);
        assert!(
            legacy_path
                .with_file_name("analytics.jsonl.migrated")
                .exists()
        );
    }

    #[test]
    fn record_fingerprint_is_stable_for_same_payload() {
        let record = sample_record("git status", 1_700_000_000_000);
        assert_eq!(record_fingerprint(&record), record_fingerprint(&record));
    }

    #[test]
    fn mark_legacy_log_migrated_replaces_source() {
        let dir = temp_dir("mark");
        let path = dir.join("analytics.jsonl");
        fs::write(&path, "[]").expect("write source");
        mark_legacy_log_migrated(&path).expect("mark migrated");
        assert!(!path.exists());
        assert!(path.with_file_name("analytics.jsonl.migrated").exists());
    }
}
