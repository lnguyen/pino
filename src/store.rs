//! Metrics persistence on rusqlite. One row per proxied /v1/messages response.
//! Also an event source: every record_request publishes the clean row on a
//! broadcast channel so the dashboard can push live SSE updates. Mirrors
//! src/store.js (EventEmitter -> tokio broadcast).

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::types::ValueRef;
use rusqlite::Connection;
use serde_json::{json, Map, Value};
use tokio::sync::broadcast;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS requests (
  req_id        TEXT PRIMARY KEY,
  ts            INTEGER NOT NULL,
  session_id    TEXT,
  agent_id      TEXT,
  parent_agent  TEXT,
  project       TEXT,
  model         TEXT,
  family        TEXT,
  input_tokens  INTEGER,
  cache_read    INTEGER,
  cache_create  INTEGER,
  ephem_5m      INTEGER,
  ephem_1h      INTEGER,
  output_tokens INTEGER,
  cost_actual   REAL,
  cost_uncached REAL,
  saved         REAL,
  estimate      INTEGER DEFAULT 0,
  gap_ms        INTEGER,
  saved_marginal REAL DEFAULT 0,
  write_premium  REAL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_req_ts       ON requests(ts);
CREATE INDEX IF NOT EXISTS idx_req_project  ON requests(project, ts);
CREATE INDEX IF NOT EXISTS idx_req_session  ON requests(session_id, ts);
";

/// Columns added after the original schema shipped. Applied as idempotent
/// ALTER TABLE ADD COLUMN so DBs created by an older build pick them up.
const MIGRATIONS: &[(&str, &str)] = &[
    ("gap_ms", "ALTER TABLE requests ADD COLUMN gap_ms INTEGER"),
    ("saved_marginal", "ALTER TABLE requests ADD COLUMN saved_marginal REAL DEFAULT 0"),
    ("write_premium", "ALTER TABLE requests ADD COLUMN write_premium REAL DEFAULT 0"),
];

/// Add any columns missing from a pre-existing `requests` table.
fn migrate(conn: &Connection) {
    let existing: std::collections::HashSet<String> = {
        let mut stmt = conn.prepare("PRAGMA table_info(requests)").unwrap();
        let cols = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        cols
    };
    for (col, ddl) in MIGRATIONS {
        if !existing.contains(*col) {
            conn.execute_batch(ddl).expect("apply migration");
        }
    }
}

/// One metered request, as handed to the store. All fields canonical snake_case.
#[derive(Clone, Debug, Default)]
pub struct RowInput {
    pub req_id: String,
    pub ts: i64,
    pub session_id: String,
    pub agent_id: String,
    pub parent_agent: String,
    pub project: String,
    pub model: String,
    pub family: String,
    pub input_tokens: i64,
    pub cache_read: i64,
    pub cache_create: i64,
    pub ephem_5m: i64,
    pub ephem_1h: i64,
    pub output_tokens: i64,
    pub cost_actual: f64,
    pub cost_uncached: f64,
    pub saved: f64,
    pub estimate: bool,
    /// Time since the previous request in this session (ms). None for the first
    /// request in a session or a backfill row with no known predecessor.
    pub gap_ms: Option<i64>,
    /// Net dollars the 1h cache bump saved over Claude Code's default 5m cache.
    pub saved_marginal: f64,
    /// Extra dollars paid this request to write at 1h instead of 5m.
    pub write_premium: f64,
}

pub struct Store {
    conn: Mutex<Connection>,
    tx: broadcast::Sender<Value>,
    pub ready: bool,
}

pub type SharedStore = Arc<Store>;

/// SQL grouping expression for each groupBy dimension.
fn group_expr(group_by: &str) -> &'static str {
    match group_by {
        "session" => "session_id",
        "agent" => "agent_id",
        "model" => "family",
        "day" => "strftime('%Y-%m-%d', ts/1000, 'unixepoch', 'localtime')",
        _ => "project",
    }
}

pub fn create_store(db_path: &str) -> SharedStore {
    if db_path != ":memory:" {
        if let Some(parent) = Path::new(db_path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    let conn = Connection::open(db_path).expect("open sqlite db");
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
        .expect("set pragmas");
    conn.execute_batch(SCHEMA).expect("init schema");
    migrate(&conn);

    let (tx, _rx) = broadcast::channel(1024);
    Arc::new(Store {
        conn: Mutex::new(conn),
        tx,
        ready: true,
    })
}

fn with_pct(obj: &mut Map<String, Value>) {
    let saved = obj.get("saved").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let uncached = obj.get("cost_uncached").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let pct = if uncached > 0.0 { saved / uncached * 100.0 } else { 0.0 };
    obj.insert("pct".to_string(), json!(pct));
}

fn col_to_json(v: ValueRef) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
        ValueRef::Blob(_) => Value::Null,
    }
}

impl Store {
    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.tx.subscribe()
    }

    /// The most recent request timestamp for `session_id` strictly before `ts`,
    /// if any. Used to derive the inter-request gap that decides whether the 1h
    /// cache bump actually beat the default 5m cache.
    pub fn last_ts_in_session(&self, session_id: &str, ts: i64) -> Option<i64> {
        if session_id.is_empty() {
            return None;
        }
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT MAX(ts) FROM requests WHERE session_id = ? AND ts < ?",
            rusqlite::params![session_id, ts],
            |row| row.get::<_, Option<i64>>(0),
        )
        .ok()
        .flatten()
    }

    /// Insert one request row, publish it for SSE, and return the clean row.
    pub fn record_request(&self, r: &RowInput) -> Value {
        let clean = json!({
            "req_id": r.req_id,
            "ts": r.ts,
            "session_id": r.session_id,
            "agent_id": r.agent_id,
            "parent_agent": r.parent_agent,
            "project": if r.project.is_empty() { "unknown".to_string() } else { r.project.clone() },
            "model": r.model,
            "family": r.family,
            "input_tokens": r.input_tokens,
            "cache_read": r.cache_read,
            "cache_create": r.cache_create,
            "ephem_5m": r.ephem_5m,
            "ephem_1h": r.ephem_1h,
            "output_tokens": r.output_tokens,
            "cost_actual": r.cost_actual,
            "cost_uncached": r.cost_uncached,
            "saved": r.saved,
            "estimate": if r.estimate { 1 } else { 0 },
            "gap_ms": r.gap_ms,
            "saved_marginal": r.saved_marginal,
            "write_premium": r.write_premium,
        });

        {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO requests (
                    req_id, ts, session_id, agent_id, parent_agent, project, model, family,
                    input_tokens, cache_read, cache_create, ephem_5m, ephem_1h, output_tokens,
                    cost_actual, cost_uncached, saved, estimate, gap_ms, saved_marginal, write_premium
                 ) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                rusqlite::params![
                    clean["req_id"].as_str().unwrap_or(""),
                    r.ts,
                    clean["session_id"].as_str().unwrap_or(""),
                    clean["agent_id"].as_str().unwrap_or(""),
                    clean["parent_agent"].as_str().unwrap_or(""),
                    clean["project"].as_str().unwrap_or("unknown"),
                    r.model,
                    r.family,
                    r.input_tokens,
                    r.cache_read,
                    r.cache_create,
                    r.ephem_5m,
                    r.ephem_1h,
                    r.output_tokens,
                    r.cost_actual,
                    r.cost_uncached,
                    r.saved,
                    if r.estimate { 1i64 } else { 0i64 },
                    r.gap_ms,
                    r.saved_marginal,
                    r.write_premium,
                ],
            )
            .expect("insert request row");
        }

        // Fan out to SSE subscribers (Err just means nobody's listening).
        let _ = self.tx.send(clean.clone());
        clean
    }

    /// Aggregate savings grouped by a dimension, sorted by saved desc.
    pub fn query_rollup(&self, group_by: &str, since: i64, limit: i64) -> Vec<Value> {
        let expr = group_expr(group_by);
        let sql = format!(
            "SELECT {expr} AS key,
                    COUNT(*)                       AS requests,
                    COALESCE(SUM(cache_read),0)    AS cache_read,
                    COALESCE(SUM(cache_create),0)  AS cache_create,
                    COALESCE(SUM(ephem_5m),0)      AS ephem_5m,
                    COALESCE(SUM(ephem_1h),0)      AS ephem_1h,
                    COALESCE(SUM(output_tokens),0) AS output_tokens,
                    COALESCE(SUM(cost_actual),0)   AS cost_actual,
                    COALESCE(SUM(cost_uncached),0) AS cost_uncached,
                    COALESCE(SUM(saved),0)         AS saved,
                    COALESCE(SUM(saved_marginal),0) AS saved_marginal,
                    COALESCE(SUM(write_premium),0)  AS write_premium,
                    MAX(family)                    AS family,
                    MAX(ts)                        AS last_ts
             FROM requests
             WHERE ts >= ?
             GROUP BY key
             ORDER BY saved DESC
             LIMIT ?"
        );
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sql).unwrap();
        let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let rows = stmt
            .query_map(rusqlite::params![since, limit], |row| {
                let mut obj = Map::new();
                for (i, name) in names.iter().enumerate() {
                    obj.insert(name.clone(), col_to_json(row.get_ref_unwrap(i)));
                }
                with_pct(&mut obj);
                Ok(Value::Object(obj))
            })
            .unwrap();
        rows.filter_map(|r| r.ok()).collect()
    }

    /// Per-session project + model mix. Returns { sessionId: {project, models} }.
    pub fn session_meta(&self, since: i64) -> Value {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT session_id AS key, family, COUNT(*) AS n, MAX(project) AS project
                 FROM requests WHERE ts >= ?
                 GROUP BY session_id, family",
            )
            .unwrap();
        let rows = stmt
            .query_map(rusqlite::params![since], |row| {
                let key: String = row.get::<_, Option<String>>(0)?.unwrap_or_default();
                let family: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
                let n: i64 = row.get(2)?;
                let project: String = row.get::<_, Option<String>>(3)?.unwrap_or_default();
                Ok((key, family, n, project))
            })
            .unwrap();

        let mut map = Map::new();
        for r in rows.filter_map(|r| r.ok()) {
            let (key, family, n, project) = r;
            let entry = map.entry(key).or_insert_with(|| {
                json!({ "project": project.clone(), "models": {} })
            });
            entry["project"] = json!(project);
            entry["models"][family] = json!(n);
        }
        Value::Object(map)
    }

    /// Whole-window totals.
    pub fn totals(&self, since: i64) -> Value {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT COUNT(*)                       AS requests,
                        COALESCE(SUM(cache_read),0)    AS cache_read,
                        COALESCE(SUM(cache_create),0)  AS cache_create,
                        COALESCE(SUM(ephem_5m),0)      AS ephem_5m,
                        COALESCE(SUM(ephem_1h),0)      AS ephem_1h,
                        COALESCE(SUM(output_tokens),0) AS output_tokens,
                        COALESCE(SUM(cost_actual),0)   AS cost_actual,
                        COALESCE(SUM(cost_uncached),0) AS cost_uncached,
                        COALESCE(SUM(saved),0)         AS saved,
                        COALESCE(SUM(saved_marginal),0) AS saved_marginal,
                        COALESCE(SUM(write_premium),0)  AS write_premium
                 FROM requests WHERE ts >= ?",
            )
            .unwrap();
        let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let obj = stmt
            .query_row(rusqlite::params![since], |row| {
                let mut obj = Map::new();
                for (i, name) in names.iter().enumerate() {
                    obj.insert(name.clone(), col_to_json(row.get_ref_unwrap(i)));
                }
                with_pct(&mut obj);
                Ok(Value::Object(obj))
            })
            .unwrap();
        obj
    }

    /// Time-bucketed savings. Bucket width derives from the data span so you get
    /// ~`buckets` points whether the window is minutes or weeks.
    pub fn series(&self, since: i64, buckets: i64, bucket_ms_in: Option<i64>) -> Value {
        let conn = self.conn.lock().unwrap();
        let (from, to): (i64, i64) = conn
            .query_row(
                "SELECT COALESCE(MIN(ts),0), COALESCE(MAX(ts),0) FROM requests WHERE ts >= ?",
                rusqlite::params![since],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap_or((0, 0));

        let bucket_ms = bucket_ms_in.unwrap_or_else(|| {
            let width = (to - from).max(1);
            let b = buckets.max(1);
            (1000).max(((width as f64) / (b as f64)).ceil() as i64)
        });

        let mut stmt = conn
            .prepare(
                "SELECT CAST(ts / CAST(? AS INTEGER) AS INTEGER) * CAST(? AS INTEGER) AS bucket,
                        COUNT(*)                       AS requests,
                        COALESCE(SUM(saved),0)         AS saved,
                        COALESCE(SUM(saved_marginal),0) AS saved_marginal,
                        COALESCE(SUM(cost_actual),0)   AS cost_actual,
                        COALESCE(SUM(cache_read),0)    AS cache_read
                 FROM requests WHERE ts >= ?
                 GROUP BY bucket ORDER BY bucket ASC",
            )
            .unwrap();
        let points = stmt
            .query_map(rusqlite::params![bucket_ms, bucket_ms, since], |row| {
                Ok(json!({
                    "t": row.get::<_, i64>(0)?,
                    "requests": row.get::<_, i64>(1)?,
                    "saved": row.get::<_, f64>(2)?,
                    "saved_marginal": row.get::<_, f64>(3)?,
                    "cost_actual": row.get::<_, f64>(4)?,
                    "cache_read": row.get::<_, i64>(5)?,
                }))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect::<Vec<_>>();

        json!({ "bucketMs": bucket_ms, "from": from, "to": to, "points": points })
    }

    pub fn recent_requests(&self, limit: i64, project: Option<&str>, session: Option<&str>) -> Vec<Value> {
        let mut clauses: Vec<&str> = Vec::new();
        if project.is_some() {
            clauses.push("project = ?");
        }
        if session.is_some() {
            clauses.push("session_id = ?");
        }
        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        let sql = format!("SELECT * FROM requests {where_clause} ORDER BY ts DESC LIMIT ?");

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sql).unwrap();
        let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();

        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(p) = project {
            params.push(Box::new(p.to_string()));
        }
        if let Some(s) = session {
            params.push(Box::new(s.to_string()));
        }
        params.push(Box::new(limit));
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();

        let rows = stmt
            .query_map(param_refs.as_slice(), |row| {
                let mut obj = Map::new();
                for (i, name) in names.iter().enumerate() {
                    obj.insert(name.clone(), col_to_json(row.get_ref_unwrap(i)));
                }
                Ok(Value::Object(obj))
            })
            .unwrap();
        rows.filter_map(|r| r.ok()).collect()
    }
}
