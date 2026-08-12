use crate::config::Config;
use crate::power_manager::HistoryRecord;
use rusqlite::{Connection, params};
use std::path::Path;
use std::sync::{Mutex, LazyLock, mpsc as std_mpsc};

pub type DbWriteTask = Box<dyn FnOnce(&mut rusqlite::Connection) -> Result<(), String> + Send>;

pub struct DbWriteMessage {
    pub task: DbWriteTask,
    pub responder: Option<std_mpsc::Sender<Result<(), String>>>,
}

#[derive(Clone)]
struct DbWriter {
    db_path: String,
    tx: std_mpsc::Sender<DbWriteMessage>,
}

static DB_WRITER: LazyLock<Mutex<Option<DbWriter>>> = LazyLock::new(|| Mutex::new(None));
static PENDING_HISTORY: LazyLock<Mutex<Vec<HistoryRecord>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static LAST_PRUNE: LazyLock<Mutex<Option<std::time::Instant>>> = LazyLock::new(|| Mutex::new(None));

/// Initializes the dedicated background SQLite writer thread for the given database path.
/// All write operations dispatched via database writer functions execute sequentially
/// on a single persistent database connection, completely eliminating write lock contention.
pub fn init_db_writer(db_path: String) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (tx, rx) = std_mpsc::channel::<DbWriteMessage>();
    let db_path_clone = db_path.clone();

    std::thread::Builder::new()
        .name("db-writer".to_string())
        .spawn(move || {
            let mut conn = match open_db_conn(&db_path_clone) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Background DB writer failed to open connection to {}: {}", db_path_clone, e);
                    return;
                }
            };

            println!("Background DB writer thread running for '{}'", db_path_clone);

            while let Ok(msg) = rx.recv() {
                let res = (msg.task)(&mut conn);
                if let Some(responder) = msg.responder {
                    let _ = responder.send(res);
                }
            }
            println!("Background DB writer thread exiting for '{}'.", db_path_clone);
        })?;

    if let Ok(mut lock) = DB_WRITER.lock() {
        *lock = Some(DbWriter { db_path, tx });
    }
    Ok(())
}

#[cfg(test)]
pub fn shutdown_db_writer() {
    if let Ok(mut lock) = DB_WRITER.lock() {
        *lock = None;
    }
}

fn execute_write_op<F>(db_path: &str, task: F) -> Result<(), String>
where
    F: FnOnce(&mut rusqlite::Connection) -> Result<(), String> + Send + 'static,
{
    let writer_opt = DB_WRITER.lock().ok().and_then(|guard| guard.clone());
    if let Some(ref writer) = writer_opt {
        if writer.db_path == db_path {
            let (resp_tx, resp_rx) = std_mpsc::channel();
            let msg = DbWriteMessage {
                task: Box::new(task),
                responder: Some(resp_tx),
            };
            if writer.tx.send(msg).is_ok() {
                let is_in_tokio = std::panic::catch_unwind(|| {
                    tokio::runtime::Handle::try_current().is_ok()
                }).unwrap_or(false);

                let recv_res = if is_in_tokio {
                    tokio::task::block_in_place(|| resp_rx.recv_timeout(std::time::Duration::from_secs(3)))
                } else {
                    resp_rx.recv_timeout(std::time::Duration::from_secs(3))
                };
                match recv_res {
                    Ok(res) => return res,
                    Err(e) => return Err(format!("DB writer response timeout/dropped: {}", e)),
                }
            }
            return Err("DB writer channel send failed".to_string());
        }
    }

    // Direct fallback if background writer thread is not initialized or db_path differs
    let mut conn = open_db_conn(db_path).map_err(|e| e.to_string())?;
    task(&mut conn)
}

fn execute_write_op_async_fire_forget<F>(db_path: &str, task: F)
where
    F: FnOnce(&mut rusqlite::Connection) -> Result<(), String> + Send + 'static,
{
    let writer_opt = DB_WRITER.lock().ok().and_then(|guard| guard.clone());
    if let Some(ref writer) = writer_opt {
        if writer.db_path == db_path {
            let msg = DbWriteMessage {
                task: Box::new(task),
                responder: None,
            };
            let _ = writer.tx.send(msg);
            return;
        }
    }

    // Direct fallback if background writer thread is not initialized or db_path differs
    if let Ok(mut conn) = open_db_conn(db_path) {
        let _ = task(&mut conn);
    }
}

pub fn push_pending_history_record(rec: HistoryRecord) {
    if let Ok(mut lock) = PENDING_HISTORY.lock() {
        lock.push(rec);
    }
}

pub fn push_pending_history_records(records: impl IntoIterator<Item = HistoryRecord>) {
    if let Ok(mut lock) = PENDING_HISTORY.lock() {
        lock.extend(records);
    }
}

pub fn flush_pending_history_to_db(db_path: &str, retention_days: Option<u32>) -> usize {
    let mut buffer = Vec::new();
    if let Ok(mut lock) = PENDING_HISTORY.lock() {
        if lock.is_empty() {
            return 0;
        }
        std::mem::swap(&mut *lock, &mut buffer);
    }
    let count = buffer.len();
    if !buffer.is_empty() {
        flush_history_to_db(db_path, &mut buffer, retention_days);
    }
    count
}

/// Opens a database connection with Write-Ahead Logging (WAL) and a 5-second busy timeout.
pub fn open_db_conn<P: AsRef<Path>>(db_path: P) -> rusqlite::Result<Connection> {
    let conn = Connection::open(db_path)?;
    let _ = conn.execute_batch("
        PRAGMA journal_mode = WAL;
        PRAGMA busy_timeout = 5000;
        PRAGMA synchronous = NORMAL;
    ");
    Ok(conn)
}

/// Opens a read-only non-blocking database connection for query execution.
pub fn open_db_conn_read_only<P: AsRef<Path>>(db_path: P) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let _ = conn.execute_batch("
        PRAGMA busy_timeout = 5000;
        PRAGMA query_only = ON;
    ");
    Ok(conn)
}

/// Flattens a serde_json::Value tree into dot-separated hierarchical key-value pairs.
pub fn flatten_json_value(prefix: &str, val: &serde_json::Value, map: &mut std::collections::HashMap<String, String>) {
    match val {
        serde_json::Value::Object(obj) => {
            for (k, v) in obj {
                let new_key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{}.{}", prefix, k)
                };
                flatten_json_value(&new_key, v, map);
            }
        }
        _ => {
            if !prefix.is_empty() {
                if let Ok(serialized) = serde_json::to_string(val) {
                    map.insert(prefix.to_string(), serialized);
                }
            }
        }
    }
}

/// Reconstructs a nested serde_json::Value object tree from dot-separated hierarchical key-value pairs.
pub fn unflatten_json_map(map: &std::collections::HashMap<String, String>) -> serde_json::Value {
    let mut root = serde_json::Value::Object(serde_json::Map::new());

    for (key, val_str) in map {
        let val: serde_json::Value = match serde_json::from_str(val_str) {
            Ok(v) => v,
            Err(_) => serde_json::Value::String(val_str.clone()),
        };

        let parts: Vec<&str> = key.split('.').collect();
        let mut curr = &mut root;

        for (i, part) in parts.iter().enumerate() {
            if i == parts.len() - 1 {
                if let serde_json::Value::Object(m) = curr {
                    m.insert(part.to_string(), val.clone());
                }
            } else {
                if let serde_json::Value::Object(m) = curr {
                    if !m.contains_key(*part) || !m.get(*part).unwrap().is_object() {
                        m.insert(part.to_string(), serde_json::Value::Object(serde_json::Map::new()));
                    }
                    curr = m.get_mut(*part).unwrap();
                }
            }
        }

    }

    root
}

/// Loads application configuration from hierarchical key-value database (config_kv).
/// Automatically migrates legacy single-JSON `settings` table to `config_kv` if detected.
pub fn load_config_from_db(db_path: &str) -> Result<Config, Box<dyn std::error::Error>> {
    let conn = open_db_conn(db_path)?;

    // Ensure config_kv table exists
    conn.execute(
        "CREATE TABLE IF NOT EXISTS config_kv (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )",
        [],
    )?;

    // Check if legacy settings table exists
    let has_settings_table: bool = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='settings'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        .unwrap_or(false);

    if has_settings_table {
        let has_config_json: bool = conn
            .prepare("SELECT config_json FROM settings LIMIT 1")
            .is_ok();

        if has_config_json {
            let json_str: Option<String> = {
                let mut stmt = conn.prepare("SELECT config_json FROM settings WHERE id = 1")?;
                let mut rows = stmt.query([])?;
                if let Some(row) = rows.next()? {
                    Some(row.get(0)?)
                } else {
                    None
                }
            };

            if let Some(ref json) = json_str {
                if let Ok(config) = serde_json::from_str::<Config>(json) {
                    println!("Migrating legacy settings table to hierarchical config_kv key-value store...");
                    save_config_to_db(db_path, &config)?;
                    let _ = conn.execute_batch("DROP TABLE settings;");
                    return Ok(config);
                }
            }
        } else {
            println!("Dropping obsolete legacy settings table without config_json column...");
            let _ = conn.execute_batch("DROP TABLE settings;");
        }
    }


    // Query all key-value entries from config_kv
    let mut stmt = conn.prepare("SELECT key, value FROM config_kv")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut map = std::collections::HashMap::new();
    for r in rows {
        if let Ok((k, v)) = r {
            map.insert(k, v);
        }
    }

    if map.is_empty() {
        return Err("No configuration found in config_kv table".into());
    }

    let root = unflatten_json_map(&map);
    let config: Config = serde_json::from_value(root)?;
    Ok(config)
}

pub fn save_config_with_conn(conn: &mut Connection, config: &Config) -> Result<(), String> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS config_kv (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )",
        [],
    ).map_err(|e| e.to_string())?;

    let val = serde_json::to_value(config).map_err(|e| e.to_string())?;
    let mut map = std::collections::HashMap::new();
    flatten_json_value("", &val, &mut map);

    let tx = conn.transaction().map_err(|e| e.to_string())?;
    {
        tx.execute("DELETE FROM config_kv", []).map_err(|e| e.to_string())?;
        let mut stmt = tx.prepare("INSERT INTO config_kv (key, value) VALUES (?1, ?2)").map_err(|e| e.to_string())?;
        for (k, v) in &map {
            stmt.execute(params![k, v]).map_err(|e| e.to_string())?;
        }
    }
    tx.commit().map_err(|e| e.to_string())?;

    // Compact WAL file to prevent unbounded growth on embedded storage
    if let Err(e) = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);") {
        eprintln!("WAL checkpoint after config save failed: {}", e);
    }
    Ok(())
}

/// Saves application configuration into hierarchical key-value database (config_kv).
pub fn save_config_to_db(db_path: &str, config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let config = config.clone();
    execute_write_op(db_path, move |conn| {
        save_config_with_conn(conn, &config)
    }).map_err(|e| e.into())
}

/// Async variant to save application configuration without blocking Tokio runtime worker threads.
pub async fn save_config_to_db_async(db_path: &str, config: Config) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let writer_opt = DB_WRITER.lock().ok().and_then(|guard| guard.clone());
    if let Some(ref writer) = writer_opt {
        if writer.db_path == db_path {
            let (resp_tx, resp_rx) = std_mpsc::channel();
            let config_clone = config.clone();
            let msg = DbWriteMessage {
                task: Box::new(move |conn| save_config_with_conn(conn, &config_clone)),
                responder: Some(resp_tx),
            };
            if writer.tx.send(msg).is_ok() {
                let res = tokio::task::spawn_blocking(move || resp_rx.recv())
                    .await
                    .map_err(|e| format!("DB writer task join error: {}", e))?
                    .map_err(|e| format!("DB writer channel dropped: {}", e))?;
                return res.map_err(|e| e.into());
            }
        }
    }

    let mut conn = open_db_conn(db_path)?;
    save_config_with_conn(&mut conn, &config).map_err(|e| e.into())
}


/// Helper to get or create a topic_id in the telemetry_topics dictionary table.
pub fn get_or_create_topic_id(conn: &rusqlite::Connection, topic: &str) -> Result<i64, rusqlite::Error> {
    if let Ok(id) = conn.query_row("SELECT id FROM telemetry_topics WHERE topic = ?1", params![topic], |r| r.get(0)) {
        return Ok(id);
    }
    let (device, field) = if let Some(idx) = topic.find('/') {
        (&topic[..idx], &topic[idx + 1..])
    } else {
        ("", topic)
    };
    conn.execute(
        "INSERT OR IGNORE INTO telemetry_topics (topic, device, field) VALUES (?1, ?2, ?3)",
        params![topic, device, field],
    )?;
    conn.query_row("SELECT id FROM telemetry_topics WHERE topic = ?1", params![topic], |r| r.get(0))
}

/// Initializes database tables and indices for telemetry history and solar forecasts.
pub fn init_history_db(db_path: &str) -> Result<(), rusqlite::Error> {
    let mut conn = open_db_conn(db_path)?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS telemetry_topics (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            topic TEXT NOT NULL UNIQUE,
            device TEXT NOT NULL DEFAULT '',
            field TEXT NOT NULL DEFAULT ''
        )",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_telemetry_topics_field ON telemetry_topics (field)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_telemetry_topics_device ON telemetry_topics (device)",
        [],
    )?;
    conn.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_telemetry_topics_topic ON telemetry_topics (topic)",
        [],
    )?;

    // Check if telemetry_history is in old schema (has 'topic' column instead of 'topic_id')
    let is_old_schema: bool = conn
        .prepare("PRAGMA table_info(telemetry_history)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(Result::ok)
        .any(|col| col == "topic");

    if is_old_schema {
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO telemetry_topics (topic, device, field)
             SELECT DISTINCT topic,
                    CASE WHEN instr(topic, '/') > 0 THEN substr(topic, 1, instr(topic, '/') - 1) ELSE '' END,
                    CASE WHEN instr(topic, '/') > 0 THEN substr(topic, instr(topic, '/') + 1) ELSE topic END
             FROM telemetry_history",
            [],
        )?;
        tx.execute(
            "CREATE TABLE telemetry_history_new (
                timestamp INTEGER NOT NULL,
                topic_id INTEGER NOT NULL,
                value REAL NOT NULL,
                PRIMARY KEY (topic_id, timestamp)
            ) WITHOUT ROWID",
            [],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO telemetry_history_new (timestamp, topic_id, value)
             SELECT h.timestamp, t.id, h.value
             FROM telemetry_history h
             JOIN telemetry_topics t ON h.topic = t.topic",
            [],
        )?;
        tx.execute("DROP TABLE telemetry_history", [])?;
        tx.execute("ALTER TABLE telemetry_history_new RENAME TO telemetry_history", [])?;
        tx.execute("CREATE INDEX IF NOT EXISTS idx_telemetry_history_ts ON telemetry_history (timestamp)", [])?;
        tx.commit()?;
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;");
    } else {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS telemetry_history (
                timestamp INTEGER NOT NULL,
                topic_id INTEGER NOT NULL,
                value REAL NOT NULL,
                PRIMARY KEY (topic_id, timestamp)
            ) WITHOUT ROWID",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_telemetry_history_ts ON telemetry_history (timestamp)",
            [],
        )?;
    }

    conn.execute(
        "CREATE TABLE IF NOT EXISTS solar_forecast (
            timestamp INTEGER PRIMARY KEY,
            predicted_solar_w REAL NOT NULL
        )",
        [],
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS inferred_battery_capacity (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp INTEGER NOT NULL,
            inverter_name TEXT NOT NULL,
            capacity_kwh REAL NOT NULL
        )",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_inferred_battery_cap_inv_ts ON inferred_battery_capacity (inverter_name, timestamp DESC)",
        [],
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS battery_energy_state (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            energy_kwh REAL NOT NULL,
            total_cost_cents REAL NOT NULL,
            updated_at INTEGER NOT NULL
        )",
        [],
    )?;
    Ok(())
}

pub fn save_battery_energy_state_with_conn(conn: &mut Connection, energy_kwh: f64, total_cost_cents: f64) -> Result<(), String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    conn.execute(
        "INSERT INTO battery_energy_state (id, energy_kwh, total_cost_cents, updated_at) VALUES (1, ?1, ?2, ?3)
         ON CONFLICT(id) DO UPDATE SET energy_kwh = ?1, total_cost_cents = ?2, updated_at = ?3",
        params![energy_kwh, total_cost_cents, now],
    ).map_err(|e| e.to_string())?;
    Ok(())
}

/// Saves stored battery energy and total cost tracking state to database.
pub fn save_battery_energy_state(db_path: &str, energy_kwh: f64, total_cost_cents: f64) -> Result<(), rusqlite::Error> {
    execute_write_op(db_path, move |conn| {
        save_battery_energy_state_with_conn(conn, energy_kwh, total_cost_cents)
    }).map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))
}

/// Loads the latest persisted stored battery energy and total cost tracking state from database.
pub fn load_battery_energy_state(db_path: &str) -> Option<(f64, f64)> {
    let conn = open_db_conn_read_only(db_path).ok()?;
    conn.query_row(
        "SELECT energy_kwh, total_cost_cents FROM battery_energy_state WHERE id = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .ok()
}

pub fn save_inferred_battery_capacity_with_conn(conn: &mut Connection, inverter_name: &str, capacity_kwh: f64) -> Result<(), String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    conn.execute(
        "INSERT INTO inferred_battery_capacity (timestamp, inverter_name, capacity_kwh) VALUES (?1, ?2, ?3)",
        params![now, inverter_name, capacity_kwh],
    ).map_err(|e| e.to_string())?;
    Ok(())
}

/// Saves an inferred battery capacity reading for a specific inverter.
pub fn save_inferred_battery_capacity(db_path: &str, inverter_name: &str, capacity_kwh: f64) -> Result<(), rusqlite::Error> {
    let inverter_name = inverter_name.to_string();
    execute_write_op(db_path, move |conn| {
        save_inferred_battery_capacity_with_conn(conn, &inverter_name, capacity_kwh)
    }).map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))
}

/// Retrieves the latest cached inferred battery capacity for an inverter.
pub fn get_latest_inferred_battery_capacity(db_path: &str, inverter_name: &str) -> Option<f64> {
    let conn = open_db_conn_read_only(db_path).ok()?;
    conn.query_row(
        "SELECT capacity_kwh FROM inferred_battery_capacity WHERE inverter_name = ?1 ORDER BY timestamp DESC, id DESC LIMIT 1",
        params![inverter_name],
        |row| row.get(0),
    )
    .ok()
}

/// Retrieves historical inferred battery capacity readings for degradation tracking over time.
pub fn get_inferred_battery_capacity_history(db_path: &str, inverter_name: &str) -> Result<Vec<(i64, f64)>, rusqlite::Error> {
    let conn = open_db_conn_read_only(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT timestamp, capacity_kwh FROM inferred_battery_capacity WHERE inverter_name = ?1 ORDER BY timestamp ASC, id ASC",
    )?;
    let rows = stmt.query_map(params![inverter_name], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?;

    let mut result = Vec::new();
    for r in rows {
        result.push(r?);
    }
    Ok(result)
}

pub fn flush_history_with_conn(
    conn: &mut Connection,
    records: &[HistoryRecord],
    retention_days: Option<u32>,
) -> Result<(), String> {
    if records.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    {
        for rec in records.iter() {
            let topic_id = match get_or_create_topic_id(&tx, &rec.topic) {
                Ok(id) => id,
                Err(e) => {
                    eprintln!("Failed to resolve topic id for {}: {}", rec.topic, e);
                    continue;
                }
            };
            let mut stmt = match tx.prepare_cached(
                "INSERT OR REPLACE INTO telemetry_history (timestamp, topic_id, value) VALUES (?1, ?2, ?3)"
            ) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Failed to prepare telemetry flush statement: {}", e);
                    return Err(e.to_string());
                }
            };
            if let Err(e) = stmt.execute(params![rec.timestamp, topic_id, rec.value]) {
                eprintln!("Failed to insert telemetry record: {}", e);
            }
        }
    }
    tx.commit().map_err(|e| e.to_string())?;

    if let Some(days) = retention_days {
        let mut should_prune = false;
        if let Ok(mut lock) = LAST_PRUNE.lock() {
            if lock.map_or(true, |t| t.elapsed() >= std::time::Duration::from_secs(24 * 3600)) {
                *lock = Some(std::time::Instant::now());
                should_prune = true;
            }
        }
        if should_prune {
            let cutoff = chrono::Utc::now().timestamp() - (days as i64 * 24 * 3600);
            if let Err(e) = conn.execute("DELETE FROM telemetry_history WHERE timestamp < ?1", params![cutoff]) {
                eprintln!("Failed to prune old telemetry records: {}", e);
            }
        }
    }

    // Non-blocking WAL checkpoint to prevent unbounded growth on embedded storage
    if let Err(e) = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);") {
        eprintln!("WAL checkpoint after telemetry flush failed: {}", e);
    }
    Ok(())
}

/// Flushes a batch of in-memory telemetry records to the history table and prunes old records.
pub fn flush_history_to_db(db_path: &str, buffer: &mut Vec<HistoryRecord>, retention_days: Option<u32>) {
    if buffer.is_empty() {
        return;
    }
    let records = buffer.clone();

    let res = execute_write_op(db_path, move |conn| {
        flush_history_with_conn(conn, &records, retention_days)
    });

    if res.is_ok() {
        buffer.clear();
    }
}

/// Queries recent price telemetry values.
pub fn get_price_history(db_path: &str, topic: &str, since_timestamp: i64) -> Result<Vec<f64>, rusqlite::Error> {
    let conn = open_db_conn(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT h.value FROM telemetry_history h
         JOIN telemetry_topics t ON h.topic_id = t.id
         WHERE t.topic = ?1 AND h.timestamp >= ?2 ORDER BY h.timestamp ASC"
    )?;
    let rows = stmt.query_map(params![topic, since_timestamp], |row| {
        row.get(0)
    })?;
    let mut values = Vec::new();
    for val in rows {
        if let Ok(v) = val {
            values.push(v);
        }
    }
    Ok(values)
}

/// Checks if any telemetry records exist for a topic after a given timestamp.
pub fn check_telemetry_exists(db_path: &str, topic: &str, since_timestamp: i64) -> bool {
    if let Ok(conn) = open_db_conn(db_path) {
        conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM telemetry_history h
                JOIN telemetry_topics t ON h.topic_id = t.id
                WHERE t.topic = ?1 AND h.timestamp >= ?2
             )",
            params![topic, since_timestamp],
            |row| row.get(0)
        ).unwrap_or(false)
    } else {
        false
    }
}

/// Gets the average value for a telemetry topic over a time range.
pub fn get_avg_telemetry_value(db_path: &str, topic: &str, since_timestamp: i64) -> Option<f64> {
    if let Ok(conn) = open_db_conn(db_path) {
        conn.query_row(
            "SELECT AVG(h.value) FROM telemetry_history h
             JOIN telemetry_topics t ON h.topic_id = t.id
             WHERE t.topic = ?1 AND h.timestamp >= ?2",
            params![topic, since_timestamp],
            |row| row.get(0)
        ).ok().flatten()
    } else {
        None
    }
}

/// Queries telemetry records ordered chronologically.
pub fn get_telemetry_history(db_path: &str, topic: &str, since_timestamp: i64) -> Result<Vec<(i64, f64)>, rusqlite::Error> {
    let conn = open_db_conn(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT h.timestamp, h.value FROM telemetry_history h
         JOIN telemetry_topics t ON h.topic_id = t.id
         WHERE t.topic = ?1 AND h.timestamp >= ?2 ORDER BY h.timestamp ASC"
    )?;
    let rows = stmt.query_map(params![topic, since_timestamp], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
    })?;
    let mut records = Vec::new();
    for r in rows {
        if let Ok(val) = r {
            records.push(val);
        }
    }
    Ok(records)
}

/// Queries all telemetry records since a given timestamp.
pub fn get_all_telemetry_since(db_path: &str, since_timestamp: i64) -> Result<Vec<(i64, String, f64)>, rusqlite::Error> {
    let conn = open_db_conn(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT h.timestamp, t.topic, h.value FROM telemetry_history h
         JOIN telemetry_topics t ON h.topic_id = t.id
         WHERE h.timestamp >= ?1 ORDER BY h.timestamp ASC"
    )?;
    let rows = stmt.query_map(params![since_timestamp], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, f64>(2)?))
    })?;
    let mut records = Vec::new();
    for r in rows {
        if let Ok(val) = r {
            records.push(val);
        }
    }
    Ok(records)
}

/// Queries all telemetry records in a specific window range.
pub fn get_all_telemetry_in_range(db_path: &str, start_ts: i64, end_ts: i64) -> Result<Vec<(i64, String, f64)>, rusqlite::Error> {
    let conn = open_db_conn(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT h.timestamp, t.topic, h.value FROM telemetry_history h
         JOIN telemetry_topics t ON h.topic_id = t.id
         WHERE h.timestamp >= ?1 AND h.timestamp <= ?2 ORDER BY h.timestamp ASC"
    )?;
    let rows = stmt.query_map(params![start_ts, end_ts], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, f64>(2)?))
    })?;
    let mut records = Vec::new();
    for r in rows {
        if let Ok(val) = r {
            records.push(val);
        }
    }
    Ok(records)
}

/// Decimates time-series records per topic to fit within `max_pixels` resolution using min-max chronological bucket preserving.
pub fn decimate_telemetry_records(
    records: Vec<(i64, String, f64)>,
    start_ts: i64,
    end_ts: i64,
    max_pixels: usize,
) -> Vec<(i64, String, f64)> {
    if max_pixels == 0 || records.is_empty() {
        return records;
    }

    // Group records by topic
    let mut topic_map: std::collections::HashMap<String, Vec<(i64, f64)>> = std::collections::HashMap::new();
    for (ts, topic, val) in records {
        topic_map.entry(topic).or_default().push((ts, val));
    }

    let mut result = Vec::new();
    let duration = (end_ts - start_ts).max(1) as f64;

    for (topic, points) in topic_map {
        if points.len() <= max_pixels {
            for (ts, val) in points {
                result.push((ts, topic.clone(), val));
            }
            continue;
        }

        // Divide into buckets where each bucket outputs at most 2 points (min and max)
        let num_buckets = (max_pixels / 2).max(1);
        let bucket_width = duration / (num_buckets as f64);
        let mut buckets: std::collections::BTreeMap<i64, Vec<(i64, f64)>> = std::collections::BTreeMap::new();

        for (ts, val) in points {
            let bucket_idx = (((ts - start_ts) as f64) / bucket_width).floor() as i64;
            buckets.entry(bucket_idx).or_default().push((ts, val));
        }

        for (_idx, b_points) in buckets {
            if b_points.len() == 1 {
                result.push((b_points[0].0, topic.clone(), b_points[0].1));
            } else if b_points.len() > 1 {
                let mut min_pt = b_points[0];
                let mut max_pt = b_points[0];
                for &pt in &b_points[1..] {
                    if pt.1 < min_pt.1 {
                        min_pt = pt;
                    }
                    if pt.1 > max_pt.1 {
                        max_pt = pt;
                    }
                }
                if min_pt.0 == max_pt.0 || (min_pt.1 - max_pt.1).abs() < f64::EPSILON {
                    result.push((b_points[0].0, topic.clone(), b_points[0].1));
                } else if min_pt.0 < max_pt.0 {
                    result.push((min_pt.0, topic.clone(), min_pt.1));
                    result.push((max_pt.0, topic.clone(), max_pt.1));
                } else {
                    result.push((max_pt.0, topic.clone(), max_pt.1));
                    result.push((min_pt.0, topic.clone(), min_pt.1));
                }
            }
        }
    }

    result.sort_by_key(|(ts, _, _)| *ts);
    result
}

/// Queries telemetry records in a specific window range, with optional topic filtering.
pub fn get_telemetry_in_range_filtered(
    db_path: &str,
    start_ts: i64,
    end_ts: i64,
    topics: Option<&[String]>,
) -> Result<Vec<(i64, String, f64)>, rusqlite::Error> {
    let conn = open_db_conn(db_path)?;

    // 1. Resolve matching topic_id -> topic string mappings from dictionary table
    let mut topic_map: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    if let Some(topic_list) = topics {
        if !topic_list.is_empty() {
            let placeholders = vec!["?"; topic_list.len()].join(",");
            let sql = format!("SELECT id, topic FROM telemetry_topics WHERE topic IN ({})", placeholders);
            let mut stmt = conn.prepare(&sql)?;
            let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            for t in topic_list {
                params_vec.push(Box::new(t.clone()));
            }
            let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(params_refs.as_slice(), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            for r in rows {
                if let Ok((id, top)) = r {
                    topic_map.insert(id, top);
                }
            }
        }
    }

    if topic_map.is_empty() && (topics.is_none() || topics.map_or(false, |t| t.is_empty())) {
        let default_fields = [
            "PV1 Power", "PV2 Power", "PV Power", "Solar Power",
            "Battery Capacity", "Battery SOC", "SOC", "Battery Power",
            "Total active power", "Grid Power", "Grid Power (P1)", "Grid Power (P2)", "Grid Power (P3)", "Measured Power",
            "Usage"
        ];
        let placeholders = vec!["?"; default_fields.len()].join(",");
        let sql = format!("SELECT id, topic FROM telemetry_topics WHERE field IN ({})", placeholders);
        let mut stmt = conn.prepare(&sql)?;
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        for f in default_fields {
            params_vec.push(Box::new(f));
        }
        let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        for r in rows {
            if let Ok((id, top)) = r {
                topic_map.insert(id, top);
            }
        }
    }

    if topic_map.is_empty() {
        return Ok(Vec::new());
    }

    // 2. Query telemetry_history directly using topic_id IN (...) AND timestamp range
    // SQLite uses the WITHOUT ROWID PRIMARY KEY (topic_id, timestamp) index for instant range seeks
    let topic_ids: Vec<i64> = topic_map.keys().copied().collect();
    let placeholders = vec!["?"; topic_ids.len()].join(",");
    let sql = format!(
        "SELECT timestamp, topic_id, value FROM telemetry_history WHERE topic_id IN ({}) AND timestamp >= ? AND timestamp <= ? ORDER BY timestamp ASC",
        placeholders
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    for id in &topic_ids {
        params_vec.push(Box::new(*id));
    }
    params_vec.push(Box::new(start_ts));
    params_vec.push(Box::new(end_ts));

    let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
    let rows = stmt.query_map(params_refs.as_slice(), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, f64>(2)?))
    })?;

    let mut records = Vec::new();
    for r in rows {
        if let Ok((ts, tid, val)) = r {
            if let Some(top_str) = topic_map.get(&tid) {
                records.push((ts, top_str.clone(), val));
            }
        }
    }

    records.sort_by_key(|(ts, _, _)| *ts);
    Ok(records)
}

/// Queries and decimates telemetry records in a specific window range, returning timing profiling data.
pub fn get_decimated_telemetry_in_range_profiled(
    db_path: &str,
    start_ts: i64,
    end_ts: i64,
    max_pixels: usize,
    topics: Option<&[String]>,
) -> Result<(Vec<(i64, String, f64)>, usize, f64, f64), rusqlite::Error> {
    let t0 = std::time::Instant::now();
    let conn = open_db_conn_read_only(db_path)?;

    // 1. Resolve matching topic_id -> topic string mappings from dictionary table
    let mut topic_map: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    if let Some(topic_list) = topics {
        if !topic_list.is_empty() {
            let placeholders = vec!["?"; topic_list.len()].join(",");
            let sql = format!("SELECT id, topic FROM telemetry_topics WHERE topic IN ({})", placeholders);
            let mut stmt = conn.prepare(&sql)?;
            let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            for t in topic_list {
                params_vec.push(Box::new(t.clone()));
            }
            let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(params_refs.as_slice(), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            for r in rows {
                if let Ok((id, top)) = r {
                    topic_map.insert(id, top);
                }
            }
        }
    }

    if topic_map.is_empty() && (topics.is_none() || topics.map_or(false, |t| t.is_empty())) {
        let default_fields = [
            "PV1 Power", "PV2 Power", "PV Power", "Solar Power",
            "Battery Capacity", "Battery SOC", "SOC", "Battery Power",
            "Total active power", "Grid Power", "Grid Power (P1)", "Grid Power (P2)", "Grid Power (P3)", "Measured Power",
            "Usage"
        ];
        let placeholders = vec!["?"; default_fields.len()].join(",");
        let sql = format!("SELECT id, topic FROM telemetry_topics WHERE field IN ({})", placeholders);
        let mut stmt = conn.prepare(&sql)?;
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        for f in default_fields {
            params_vec.push(Box::new(f));
        }
        let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        for r in rows {
            if let Ok((id, top)) = r {
                topic_map.insert(id, top);
            }
        }
    }

    if topic_map.is_empty() {
        let db_dur = t0.elapsed().as_secs_f64() * 1000.0;
        return Ok((Vec::new(), 0, db_dur, 0.0));
    }

    // 2. Perform streaming on-the-fly decimation per topic
    // Direct index scan on WITHOUT ROWID PRIMARY KEY (topic_id, timestamp) for zero-sorting instant retrieval
    let num_buckets = (max_pixels / 2).max(1);
    let duration = (end_ts - start_ts).max(1) as f64;
    let bucket_width = (duration / (num_buckets as f64)).max(1.0);

    let mut decimated_records = Vec::new();
    let mut raw_count = 0usize;

    let mut stmt = conn.prepare(
        "SELECT timestamp, value FROM telemetry_history WHERE topic_id = ?1 AND timestamp >= ?2 AND timestamp <= ?3 ORDER BY timestamp ASC"
    )?;

    let topic_dur = t0.elapsed().as_secs_f64() * 1000.0;
    let t1 = std::time::Instant::now();

    for (&tid, topic_str) in &topic_map {
        let mut rows = stmt.query(params![tid, start_ts, end_ts])?;

        let mut current_bucket: Option<i64> = None;
        let mut min_pt: Option<(i64, f64)> = None;
        let mut max_pt: Option<(i64, f64)> = None;

        while let Some(row) = rows.next()? {
            raw_count += 1;
            let ts: i64 = row.get(0)?;
            let val: f64 = row.get(1)?;
            let b_idx = (((ts - start_ts) as f64) / bucket_width).floor() as i64;

            match current_bucket {
                Some(b) if b == b_idx => {
                    if let Some(ref mut min) = min_pt {
                        if val < min.1 {
                            *min = (ts, val);
                        }
                    }
                    if let Some(ref mut max) = max_pt {
                        if val > max.1 {
                            *max = (ts, val);
                        }
                    }
                }
                _ => {
                    if let (Some(min), Some(max)) = (min_pt, max_pt) {
                        if min.0 == max.0 || (min.1 - max.1).abs() < f64::EPSILON {
                            decimated_records.push((min.0, topic_str.clone(), min.1));
                        } else if min.0 < max.0 {
                            decimated_records.push((min.0, topic_str.clone(), min.1));
                            decimated_records.push((max.0, topic_str.clone(), max.1));
                        } else {
                            decimated_records.push((max.0, topic_str.clone(), max.1));
                            decimated_records.push((min.0, topic_str.clone(), min.1));
                        }
                    }
                    current_bucket = Some(b_idx);
                    min_pt = Some((ts, val));
                    max_pt = Some((ts, val));
                }
            }
        }

        if let (Some(min), Some(max)) = (min_pt, max_pt) {
            if min.0 == max.0 || (min.1 - max.1).abs() < f64::EPSILON {
                decimated_records.push((min.0, topic_str.clone(), min.1));
            } else if min.0 < max.0 {
                decimated_records.push((min.0, topic_str.clone(), min.1));
                decimated_records.push((max.0, topic_str.clone(), max.1));
            } else {
                decimated_records.push((max.0, topic_str.clone(), max.1));
                decimated_records.push((min.0, topic_str.clone(), min.1));
            }
        }
    }

    let query_decimate_dur = t1.elapsed().as_secs_f64() * 1000.0;

    decimated_records.sort_by_key(|(ts, _, _)| *ts);
    Ok((decimated_records, raw_count, topic_dur, query_decimate_dur))
}

/// Queries and decimates telemetry records in a specific window range.
pub fn get_decimated_telemetry_in_range(
    db_path: &str,
    start_ts: i64,
    end_ts: i64,
    max_pixels: usize,
) -> Result<Vec<(i64, String, f64)>, rusqlite::Error> {
    let (decimated, _, _, _) = get_decimated_telemetry_in_range_profiled(db_path, start_ts, end_ts, max_pixels, None)?;
    Ok(decimated)
}

/// Queries the maximum value for a telemetry topic since a given timestamp.
pub fn get_monthly_peak_draw(db_path: &str, topic: &str, since_timestamp: i64) -> f64 {
    if let Ok(conn) = open_db_conn(db_path) {
        conn.query_row(
            "SELECT MAX(h.value) FROM telemetry_history h
             JOIN telemetry_topics t ON h.topic_id = t.id
             WHERE t.topic = ?1 AND h.timestamp >= ?2",
            params![topic, since_timestamp],
            |row| row.get::<_, f64>(0)
        ).unwrap_or(0.0)
    } else {
        0.0
    }
}

pub fn delete_and_save_solar_forecast_with_conn(conn: &mut Connection, predictions: &[(i64, f64)]) -> Result<(), String> {
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    tx.execute("DELETE FROM solar_forecast", []).map_err(|e| e.to_string())?;
    {
        let mut stmt = tx.prepare("INSERT OR REPLACE INTO solar_forecast (timestamp, predicted_solar_w) VALUES (?1, ?2)").map_err(|e| e.to_string())?;
        for (ts, predicted_w) in predictions {
            stmt.execute(params![ts, *predicted_w]).map_err(|e| e.to_string())?;
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

/// Overwrites the solar forecast table with a new forecast dataset in a transaction.
pub fn delete_and_save_solar_forecast(db_path: &str, predictions: &[(i64, f64)]) -> Result<(), rusqlite::Error> {
    let predictions = predictions.to_vec();
    execute_write_op(db_path, move |conn| {
        delete_and_save_solar_forecast_with_conn(conn, &predictions)
    }).map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))
}

/// Loads solar forecast predictions within a time range.
pub fn load_solar_forecast_range(db_path: &str, start_ts: i64, end_ts: i64) -> Result<Vec<(i64, f64)>, rusqlite::Error> {
    let conn = open_db_conn_read_only(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT timestamp, predicted_solar_w FROM solar_forecast WHERE timestamp >= ?1 AND timestamp <= ?2 ORDER BY timestamp ASC"
    )?;
    let rows = stmt.query_map(params![start_ts, end_ts], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
    })?;
    let mut records = Vec::new();
    for r in rows {
        if let Ok(val) = r {
            records.push(val);
        }
    }
    Ok(records)
}

/// Queries telemetry records for either of two topics, ordered chronologically.
pub fn get_telemetry_history_multiple_topics(
    db_path: &str,
    topic1: &str,
    topic2: &str,
    since_timestamp: i64,
) -> Result<Vec<(i64, String, f64)>, rusqlite::Error> {
    let conn = open_db_conn_read_only(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT h.timestamp, t.topic, h.value 
         FROM telemetry_history h
         JOIN telemetry_topics t ON h.topic_id = t.id
         WHERE (t.topic = ?1 OR t.topic = ?2) AND h.timestamp >= ?3 
         ORDER BY h.timestamp ASC"
    )?;
    let rows = stmt.query_map(params![topic1, topic2, since_timestamp], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, f64>(2)?))
    })?;
    let mut records = Vec::new();
    for r in rows {
        if let Ok(val) = r {
            records.push(val);
        }
    }
    Ok(records)
}

pub fn insert_telemetry_history_batch_with_conn(
    conn: &mut Connection,
    records: &[(i64, String, f64)],
) -> Result<(), String> {
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    {
        for (ts, topic, val) in records {
            let topic_id = get_or_create_topic_id(&tx, topic).map_err(|e| e.to_string())?;
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO telemetry_history (timestamp, topic_id, value) VALUES (?1, ?2, ?3)"
            ).map_err(|e| e.to_string())?;
            stmt.execute(params![*ts, topic_id, *val]).map_err(|e| e.to_string())?;
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

/// Inserts a batch of telemetry history records in a transaction.
pub fn insert_telemetry_history_batch(
    db_path: &str,
    records: &[(i64, String, f64)],
) -> Result<(), rusqlite::Error> {
    let records = records.to_vec();
    execute_write_op(db_path, move |conn| {
        insert_telemetry_history_batch_with_conn(conn, &records)
    }).map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_database_init_errors() {
        // Using a directory path causes SQLite to fail with SQLITE_CANTOPEN/SQLITE_IOERR
        let invalid_path = "./tests/";
        
        let res_cfg = save_config_to_db(invalid_path, &Config::default_empty());
        assert!(res_cfg.is_err());

        let res_hist = init_history_db(invalid_path);
        assert!(res_hist.is_err());
    }

    #[test]
    fn test_flush_history_errors() {
        let mut buffer = vec![
            HistoryRecord {
                timestamp: 1000,
                topic: "test/topic".to_string(),
                value: 50.5,
            }
        ];
        // Directory path will fail to open or start transaction
        flush_history_to_db("./tests/", &mut buffer, Some(30));
        // Buffer should remain unflushed because database write failed
        assert!(!buffer.is_empty());
    }

    #[test]
    fn test_delete_and_save_solar_forecast_errors() {
        let invalid_path = "./tests/";
        let predictions = vec![(1000i64, 50.0f64)];
        let res = delete_and_save_solar_forecast(invalid_path, &predictions);
        assert!(res.is_err());
    }

    #[test]
    fn test_hierarchical_config_db() {
        let db_path = "./test_hierarchical_config_db.db";
        let _ = std::fs::remove_file(db_path);

        let cfg = Config::load_from_file("tests/test_config.toml").expect("failed to load test config file");
        save_config_to_db(db_path, &cfg).expect("failed to save config to db");

        let loaded = load_config_from_db(db_path).expect("failed to load config from db");
        assert_eq!(loaded.solax_modbus.is_some(), cfg.solax_modbus.is_some());
        assert_eq!(loaded.solax_g3_modbus.is_some(), cfg.solax_g3_modbus.is_some());
        assert_eq!(loaded.mqtt.is_some(), cfg.mqtt.is_some());

        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn test_legacy_settings_migration() {
        let db_path = "./test_legacy_settings_migration.db";
        let _ = std::fs::remove_file(db_path);

        let conn = open_db_conn(db_path).unwrap();

        // Create legacy settings table with raw json
        conn.execute(
            "CREATE TABLE settings (id INTEGER PRIMARY KEY CHECK (id = 1), config_json TEXT NOT NULL)",
            [],
        ).unwrap();

        let cfg = Config::load_from_file("tests/test_config.toml").unwrap();
        let json_str = serde_json::to_string(&cfg).unwrap();
        conn.execute(
            "INSERT INTO settings (id, config_json) VALUES (1, ?1)",
            params![json_str],
        ).unwrap();



        drop(conn);

        // load_config_from_db should detect settings table, migrate to config_kv, and drop settings
        let loaded = load_config_from_db(db_path).expect("failed to load and migrate legacy settings");
        assert_eq!(loaded.solax_g3_modbus.is_some(), true);

        // Verify settings table was dropped and config_kv table contains rows
        let conn2 = open_db_conn(db_path).unwrap();
        let has_settings: bool = conn2
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='settings'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|c| c > 0)
            .unwrap_or(false);
        assert_eq!(has_settings, false);

        let kv_count: i64 = conn2
            .query_row("SELECT count(*) FROM config_kv", [], |row| row.get(0))
            .unwrap();
        assert!(kv_count > 0);

        drop(conn2);
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn test_decimate_telemetry_records() {
        let start_ts = 1000i64;
        let end_ts = 2000i64;
        let topic = "solax1/PV1 Power".to_string();

        // Create 500 records
        let mut records = Vec::new();
        for i in 0..500 {
            let ts = start_ts + (i * 2);
            let val = (i as f64) % 50.0;
            records.push((ts, topic.clone(), val));
        }

        // Decimate to max 50 points
        let decimated = decimate_telemetry_records(records.clone(), start_ts, end_ts, 50);
        assert!(decimated.len() <= 50);
        assert!(!decimated.is_empty());

        // Under limit (max_pixels = 1000): returns all records
        let undecimated = decimate_telemetry_records(records.clone(), start_ts, end_ts, 1000);
        assert_eq!(undecimated.len(), 500);
    }

    #[test]
    fn test_inferred_battery_capacity_db() {
        let db_path = "test_inferred_cap.db";
        let _ = std::fs::remove_file(db_path);

        init_history_db(db_path).unwrap();

        // Initial check: empty
        assert_eq!(get_latest_inferred_battery_capacity(db_path, "solax-1"), None);

        // Save two readings
        save_inferred_battery_capacity(db_path, "solax-1", 13.82).unwrap();
        save_inferred_battery_capacity(db_path, "solax-1", 13.75).unwrap();

        let latest = get_latest_inferred_battery_capacity(db_path, "solax-1");
        assert_eq!(latest, Some(13.75));

        let history = get_inferred_battery_capacity_history(db_path, "solax-1").unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].1, 13.82);
        assert_eq!(history[1].1, 13.75);

        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn test_battery_energy_state_db() {
        let db_path = "test_battery_energy_state.db";
        let _ = std::fs::remove_file(db_path);

        init_history_db(db_path).unwrap();

        // Initial check: empty
        assert_eq!(load_battery_energy_state(db_path), None);

        // Save state
        save_battery_energy_state(db_path, 8.5, 170.0).unwrap();
        let state = load_battery_energy_state(db_path);
        assert_eq!(state, Some((8.5, 170.0)));

        // Update state
        save_battery_energy_state(db_path, 10.0, 210.0).unwrap();
        let updated = load_battery_energy_state(db_path);
        assert_eq!(updated, Some((10.0, 210.0)));

        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn test_background_db_writer() {
        let db_path = "./test_bg_db_writer.db";
        let _ = std::fs::remove_file(db_path);

        init_history_db(db_path).unwrap();
        init_db_writer(db_path.to_string()).unwrap();

        // Write battery state via background writer thread
        save_battery_energy_state(db_path, 12.5, 250.0).unwrap();
        let state = load_battery_energy_state(db_path);
        assert_eq!(state, Some((12.5, 250.0)));

        // Save inferred capacity via background writer thread
        save_inferred_battery_capacity(db_path, "inverter-test", 9.85).unwrap();
        let latest = get_latest_inferred_battery_capacity(db_path, "inverter-test");
        assert_eq!(latest, Some(9.85));

        shutdown_db_writer();
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn test_sync_on_kill_flushes_pending_history_to_db() {
        let db_path = "./test_sync_on_kill.db";
        let _ = std::fs::remove_file(db_path);

        init_history_db(db_path).unwrap();
        init_db_writer(db_path.to_string()).unwrap();

        // Push pending telemetry records into in-memory queue
        let now = chrono::Utc::now().timestamp();
        let records = vec![
            HistoryRecord {
                timestamp: now,
                topic: "solax1/Battery SoC".to_string(),
                value: 88.5,
            },
            HistoryRecord {
                timestamp: now + 1,
                topic: "solax1/Grid Power".to_string(),
                value: -1250.0,
            },
        ];
        push_pending_history_records(records);

        // Confirm pending buffer contains 2 items
        assert_eq!(PENDING_HISTORY.lock().unwrap().len(), 2);

        // Simulate kill / shutdown atexit flush
        let flushed = flush_pending_history_to_db(db_path, None);
        assert_eq!(flushed, 2);

        // Confirm pending buffer is now empty
        assert_eq!(PENDING_HISTORY.lock().unwrap().len(), 0);

        // Query DB to verify records were flushed through the background DB writer
        let conn = open_db_conn_read_only(db_path).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM telemetry_history h JOIN telemetry_topics t ON h.topic_id = t.id WHERE t.device = 'solax1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);

        shutdown_db_writer();
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn test_flush_on_exit_c_abi_safety() {
        let db_path = "./test_flush_on_exit.db";
        let _ = std::fs::remove_file(db_path);

        init_history_db(db_path).unwrap();
        init_db_writer(db_path.to_string()).unwrap();

        push_pending_history_records(vec![HistoryRecord {
            timestamp: chrono::Utc::now().timestamp(),
            topic: "sdm630/Voltage".to_string(),
            value: 239.4,
        }]);

        // Simulates the exact call made by the atexit C-ABI handler on process exit/SIGTERM
        let _ = std::panic::catch_unwind(|| {
            flush_pending_history_to_db(db_path, None);
        });

        let conn = open_db_conn_read_only(db_path).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM telemetry_history h JOIN telemetry_topics t ON h.topic_id = t.id WHERE t.device = 'sdm630'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        shutdown_db_writer();
        let _ = std::fs::remove_file(db_path);
    }
}

