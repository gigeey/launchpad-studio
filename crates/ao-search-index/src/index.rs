use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection};

use ao_protocol::error::AoError;

use crate::query::build_match_expression;
use crate::record::{IndexRecord, SearchFilter, SearchHit};
use crate::scope::ArtifactKind;

/// Filename of the FTS5 database under the resolved data root. Shared with
/// `ao-persistence`'s `DataRoot::search_index_path()` so both crates agree
/// on where the file lives without duplicating the literal.
pub const SEARCH_INDEX_FILENAME: &str = "search_index.sqlite3";

const SCHEMA_SQL: &str = "CREATE VIRTUAL TABLE IF NOT EXISTS artifact_index USING fts5(
    entry_id UNINDEXED,
    scope_kind UNINDEXED,
    scope_key UNINDEXED,
    artifact_kind UNINDEXED,
    body,
    tokenize = 'unicode61'
);";

/// A local, offline SQLite FTS5 full-text index shared by the memory store
/// and the skill registry.
///
/// Cheap to clone (an `Arc<Mutex<Connection>>` underneath) — share one
/// instance across every caller that needs to read or write the index
/// rather than opening the database file more than once per process.
///
/// Every method has a blocking `_sync` form plus an `async` wrapper that
/// runs the same work on `tokio::task::spawn_blocking`, so both sync
/// call sites (e.g. a cold-start rebuild run before any async runtime is
/// available) and async call sites (the memory store's write path) can use
/// the same underlying implementation.
#[derive(Clone)]
pub struct SearchIndex {
    conn: Arc<Mutex<Connection>>,
}

fn sqlite_err(e: rusqlite::Error) -> AoError {
    AoError::SearchIndex(e.to_string())
}

impl SearchIndex {
    /// Open (creating if absent) the FTS5 index at an explicit file path.
    pub fn open(path: &Path) -> Result<Self, AoError> {
        let conn = Connection::open(path).map_err(sqlite_err)?;
        Self::from_connection(conn)
    }

    /// Open a private in-memory index. Useful for tests and any short-lived
    /// caller that doesn't need the index to survive process restart.
    pub fn open_in_memory() -> Result<Self, AoError> {
        let conn = Connection::open_in_memory().map_err(sqlite_err)?;
        Self::from_connection(conn)
    }

    /// Open the index at the conventional path under the resolved data root
    /// (`ao_protocol::data_root::resolve_data_root()` +
    /// [`SEARCH_INDEX_FILENAME`]), for standalone callers that don't go
    /// through `ao-persistence::PersistenceLayer`.
    pub fn open_default() -> Result<Self, AoError> {
        let root = ao_protocol::data_root::resolve_data_root()?;
        std::fs::create_dir_all(&root).map_err(AoError::Io)?;
        Self::open(&root.join(SEARCH_INDEX_FILENAME))
    }

    fn from_connection(conn: Connection) -> Result<Self, AoError> {
        conn.execute_batch(SCHEMA_SQL).map_err(sqlite_err)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, AoError> {
        self.conn
            .lock()
            .map_err(|_| AoError::SearchIndex("search index connection lock poisoned".into()))
    }

    // --- Sync core ---

    /// Insert or replace a single record, keyed by [`IndexRecord::id`].
    pub fn upsert_sync(&self, record: IndexRecord) -> Result<(), AoError> {
        self.upsert_many_sync(std::slice::from_ref(&record))
    }

    /// Insert or replace a batch of records in one transaction.
    pub fn upsert_many_sync(&self, records: &[IndexRecord]) -> Result<(), AoError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(sqlite_err)?;
        for record in records {
            write_record(&tx, record)?;
        }
        tx.commit().map_err(sqlite_err)
    }

    /// Remove a record by id. A no-op (not an error) if the id isn't indexed.
    pub fn delete_sync(&self, id: &str) -> Result<(), AoError> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM artifact_index WHERE entry_id = ?1", params![id])
            .map_err(sqlite_err)?;
        Ok(())
    }

    /// Ranked full-text query, optionally narrowed by scope and/or artifact
    /// kind. Results are sorted best-match first; `SearchHit::score` is
    /// higher-is-better (the negation of FTS5's raw `bm25()`, whose scale
    /// runs the opposite direction). Returns an empty vec for a query with
    /// no indexable tokens rather than erroring.
    pub fn query_sync(
        &self,
        text: &str,
        filter: &SearchFilter,
        limit: usize,
    ) -> Result<Vec<SearchHit>, AoError> {
        let Some(match_expr) = build_match_expression(text) else {
            return Ok(Vec::new());
        };

        let conn = self.lock()?;
        let mut sql = String::from(
            "SELECT entry_id, bm25(artifact_index) AS rank FROM artifact_index \
             WHERE artifact_index MATCH ?1",
        );
        let mut sql_params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(match_expr)];

        if let Some(scope) = &filter.scope {
            sql.push_str(&format!(
                " AND scope_kind = ?{} AND scope_key = ?{}",
                sql_params.len() + 1,
                sql_params.len() + 2
            ));
            sql_params.push(Box::new(scope.kind_str().to_string()));
            sql_params.push(Box::new(scope.key_str().to_string()));
        }

        if let Some(artifact) = &filter.artifact {
            sql.push_str(&format!(" AND artifact_kind = ?{}", sql_params.len() + 1));
            sql_params.push(Box::new(artifact.as_str().to_string()));
        }

        sql.push_str(&format!(" ORDER BY rank ASC LIMIT ?{}", sql_params.len() + 1));
        sql_params.push(Box::new(limit as i64));

        let mut stmt = conn.prepare(&sql).map_err(sqlite_err)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = sql_params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt
            .query_map(param_refs.as_slice(), |row| {
                let id: String = row.get(0)?;
                let rank: f64 = row.get(1)?;
                Ok(SearchHit { id, score: -rank })
            })
            .map_err(sqlite_err)?;

        let mut hits = Vec::new();
        for row in rows {
            hits.push(row.map_err(sqlite_err)?);
        }
        Ok(hits)
    }

    /// Whether the index currently holds zero rows for `artifact`.
    ///
    /// Lets a caller distinguish "the index has never been populated for
    /// this artifact kind" (a cold-start data root, or one whose index file
    /// predates this artifact's write path) from "the index is populated but
    /// this particular query matched nothing" — the two situations call for
    /// different fallback behavior upstream.
    pub fn is_artifact_empty_sync(&self, artifact: ArtifactKind) -> Result<bool, AoError> {
        let conn = self.lock()?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM artifact_index WHERE artifact_kind = ?1",
                params![artifact.as_str()],
                |row| row.get(0),
            )
            .map_err(sqlite_err)?;
        Ok(count == 0)
    }

    /// Cold-start / corruption recovery: replace the *entire* index (every
    /// artifact kind) with `records`. Callers that only want to resync one
    /// artifact kind without disturbing the other should use
    /// [`Self::rebuild_kind_sync`] instead.
    pub fn rebuild_sync(&self, records: &[IndexRecord]) -> Result<(), AoError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(sqlite_err)?;
        tx.execute("DELETE FROM artifact_index", []).map_err(sqlite_err)?;
        for record in records {
            write_record(&tx, record)?;
        }
        tx.commit().map_err(sqlite_err)
    }

    /// Replace only the rows for `kind`, leaving every other artifact kind's
    /// rows untouched. Records not matching `kind` are skipped. This is the
    /// right primitive for stores like the skill registry that have no
    /// per-write log to replay incrementally and instead resync by
    /// rescanning their current on-disk state wholesale.
    pub fn rebuild_kind_sync(&self, kind: ArtifactKind, records: &[IndexRecord]) -> Result<(), AoError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(sqlite_err)?;
        tx.execute(
            "DELETE FROM artifact_index WHERE artifact_kind = ?1",
            params![kind.as_str()],
        )
        .map_err(sqlite_err)?;
        for record in records.iter().filter(|r| r.artifact == kind) {
            write_record(&tx, record)?;
        }
        tx.commit().map_err(sqlite_err)
    }

    // --- Async wrappers ---

    pub async fn upsert(&self, record: IndexRecord) -> Result<(), AoError> {
        let this = self.clone();
        spawn_blocking_result(move || this.upsert_sync(record)).await
    }

    pub async fn upsert_many(&self, records: Vec<IndexRecord>) -> Result<(), AoError> {
        let this = self.clone();
        spawn_blocking_result(move || this.upsert_many_sync(&records)).await
    }

    pub async fn delete(&self, id: String) -> Result<(), AoError> {
        let this = self.clone();
        spawn_blocking_result(move || this.delete_sync(&id)).await
    }

    pub async fn query(&self, text: String, filter: SearchFilter, limit: usize) -> Result<Vec<SearchHit>, AoError> {
        let this = self.clone();
        spawn_blocking_result(move || this.query_sync(&text, &filter, limit)).await
    }

    pub async fn rebuild(&self, records: Vec<IndexRecord>) -> Result<(), AoError> {
        let this = self.clone();
        spawn_blocking_result(move || this.rebuild_sync(&records)).await
    }

    pub async fn rebuild_kind(&self, kind: ArtifactKind, records: Vec<IndexRecord>) -> Result<(), AoError> {
        let this = self.clone();
        spawn_blocking_result(move || this.rebuild_kind_sync(kind, &records)).await
    }

    pub async fn is_artifact_empty(&self, artifact: ArtifactKind) -> Result<bool, AoError> {
        let this = self.clone();
        spawn_blocking_result(move || this.is_artifact_empty_sync(artifact)).await
    }
}

fn write_record(tx: &rusqlite::Transaction<'_>, record: &IndexRecord) -> Result<(), AoError> {
    tx.execute("DELETE FROM artifact_index WHERE entry_id = ?1", params![record.id])
        .map_err(sqlite_err)?;
    tx.execute(
        "INSERT INTO artifact_index (entry_id, scope_kind, scope_key, artifact_kind, body) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            record.id,
            record.scope.kind_str(),
            record.scope.key_str(),
            record.artifact.as_str(),
            record.text
        ],
    )
    .map_err(sqlite_err)?;
    Ok(())
}

async fn spawn_blocking_result<F, T>(f: F) -> Result<T, AoError>
where
    F: FnOnce() -> Result<T, AoError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| AoError::SearchIndex(format!("search index task join error: {e}")))?
}
