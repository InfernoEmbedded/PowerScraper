use crate::config::Config;
use crate::power_manager::HistoryRecord;
use rusqlite::{Connection, params};
use std::path::Path;
use std::sync::{Mutex, LazyLock};

static PENDING_HISTORY: LazyLock<Mutex<Vec<HistoryRecord>>> = LazyLock::new(|| Mutex::new(Vec::new()));

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

pub fn flush_pending_history_to_db(db_path: &str, retention_days: Option<u32>) {
    let mut buffer = Vec::new();
    if let Ok(mut lock) = PENDING_HISTORY.lock() {
        if lock.is_empty() {
            return;
        }
        std::mem::swap(&mut *lock, &mut buffer);
    }
    if !buffer.is_empty() {
        flush_history_to_db(db_path, &mut buffer, retention_days);
    }
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

/// Saves application configuration into hierarchical key-value database (config_kv).
pub fn save_config_to_db(db_path: &str, config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let mut conn = open_db_conn(db_path)?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS config_kv (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )",
        [],
    )?;

    let val = serde_json::to_value(config)?;
    let mut map = std::collections::HashMap::new();
    flatten_json_value("", &val, &mut map);

    let tx = conn.transaction()?;
    {
        tx.execute("DELETE FROM config_kv", [])?;
        let mut stmt = tx.prepare("INSERT INTO config_kv (key, value) VALUES (?1, ?2)")?;
        for (k, v) in &map {
            stmt.execute(params![k, v])?;
        }
    }
    tx.commit()?;

    // Compact WAL file to prevent unbounded growth on embedded storage
    if let Err(e) = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);") {
        eprintln!("WAL checkpoint after config save failed: {}", e);
    }
    Ok(())
}


/// Initializes database tables and indices for telemetry history and solar forecasts.
pub fn init_history_db(db_path: &str) -> Result<(), rusqlite::Error> {
    let conn = open_db_conn(db_path)?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS telemetry_history (
            timestamp INTEGER NOT NULL,
            topic TEXT NOT NULL,
            device TEXT NOT NULL DEFAULT '',
            field TEXT NOT NULL DEFAULT '',
            value REAL NOT NULL,
            PRIMARY KEY (timestamp, topic)
        )",
        [],
    )?;

    // Migration: add device and field columns if table existed without them
    let has_device: bool = conn
        .prepare("PRAGMA table_info(telemetry_history)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(Result::ok)
        .any(|col| col == "device");

    if !has_device {
        let _ = conn.execute("ALTER TABLE telemetry_history ADD COLUMN device TEXT NOT NULL DEFAULT ''", []);
        let _ = conn.execute("ALTER TABLE telemetry_history ADD COLUMN field TEXT NOT NULL DEFAULT ''", []);
        loop {
            let updated = conn.execute(
                "UPDATE telemetry_history SET 
                    device = CASE WHEN instr(topic, '/') > 0 THEN substr(topic, 1, instr(topic, '/') - 1) ELSE '' END,
                    field = CASE WHEN instr(topic, '/') > 0 THEN substr(topic, instr(topic, '/') + 1) ELSE topic END
                WHERE rowid IN (
                    SELECT rowid FROM telemetry_history WHERE device = '' AND field = '' LIMIT 50000
                )",
                [],
            ).unwrap_or(0);
            if updated == 0 {
                break;
            }
        }
    }

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_telemetry_history_timestamp ON telemetry_history (timestamp)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_telemetry_history_topic_timestamp ON telemetry_history (topic, timestamp)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_telemetry_history_field_timestamp ON telemetry_history (field, timestamp)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_telemetry_history_device_field_timestamp ON telemetry_history (device, field, timestamp)",
        [],
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS solar_forecast (
            timestamp INTEGER PRIMARY KEY,
            predicted_solar_w REAL NOT NULL
        )",
        [],
    )?;
    Ok(())
}

/// Flushes a batch of in-memory telemetry records to the history table and prunes old records.
pub fn flush_history_to_db(db_path: &str, buffer: &mut Vec<HistoryRecord>, retention_days: Option<u32>) {
    if buffer.is_empty() {
        return;
    }
    let mut conn = match open_db_conn(db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to open DB for telemetry flush: {}", e);
            return;
        }
    };
    let tx = match conn.transaction() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to start transaction for telemetry flush: {}", e);
            return;
        }
    };
    {
        let mut stmt = match tx.prepare(
            "INSERT OR REPLACE INTO telemetry_history (timestamp, topic, device, field, value) VALUES (?1, ?2, ?3, ?4, ?5)"
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Failed to prepare telemetry flush statement: {}", e);
                return;
            }
        };
        for rec in buffer.iter() {
            let (device, field) = if let Some(idx) = rec.topic.find('/') {
                (&rec.topic[..idx], &rec.topic[idx + 1..])
            } else {
                ("", rec.topic.as_str())
            };
            if let Err(e) = stmt.execute(params![rec.timestamp, rec.topic, device, field, rec.value]) {
                eprintln!("Failed to insert telemetry record: {}", e);
            }
        }
    }
    if let Err(e) = tx.commit() {
        eprintln!("Failed to commit telemetry flush transaction: {}", e);
        return;
    }
    buffer.clear();

    if let Some(days) = retention_days {
        let cutoff = chrono::Utc::now().timestamp() - (days as i64 * 24 * 3600);
        if let Err(e) = conn.execute("DELETE FROM telemetry_history WHERE timestamp < ?1", params![cutoff]) {
            eprintln!("Failed to prune old telemetry records: {}", e);
        }
    }

    // Compact WAL file to prevent unbounded growth on embedded storage
    if let Err(e) = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);") {
        eprintln!("WAL checkpoint after telemetry flush failed: {}", e);
    }
}

/// Queries recent price telemetry values.
pub fn get_price_history(db_path: &str, topic: &str, since_timestamp: i64) -> Result<Vec<f64>, rusqlite::Error> {
    let conn = open_db_conn(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT value FROM telemetry_history WHERE topic = ?1 AND timestamp >= ?2"
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
            "SELECT EXISTS(SELECT 1 FROM telemetry_history WHERE topic = ?1 AND timestamp >= ?2)",
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
            "SELECT AVG(value) FROM telemetry_history WHERE topic = ?1 AND timestamp >= ?2",
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
        "SELECT timestamp, value FROM telemetry_history WHERE topic = ?1 AND timestamp >= ?2 ORDER BY timestamp ASC"
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
        "SELECT timestamp, topic, value FROM telemetry_history WHERE timestamp >= ?1 ORDER BY timestamp ASC"
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
        "SELECT timestamp, topic, value FROM telemetry_history WHERE timestamp >= ?1 AND timestamp <= ?2 ORDER BY timestamp ASC"
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
    if let Some(topic_list) = topics {
        if !topic_list.is_empty() {
            let placeholders = vec!["?"; topic_list.len()].join(",");
            let sql = format!(
                "SELECT timestamp, topic, value FROM telemetry_history WHERE timestamp >= ? AND timestamp <= ? AND topic IN ({}) ORDER BY timestamp ASC",
                placeholders
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            params_vec.push(Box::new(start_ts));
            params_vec.push(Box::new(end_ts));
            for t in topic_list {
                params_vec.push(Box::new(t.clone()));
            }
            let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(params_refs.as_slice(), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, f64>(2)?))
            })?;
            let mut records = Vec::new();
            for r in rows {
                if let Ok(val) = r {
                    records.push(val);
                }
            }
            return Ok(records);
        }
    }

    let default_fields = [
        "PV1 Power", "PV2 Power", "PV Power", "Solar Power",
        "Battery Capacity", "Battery SOC", "SOC", "Battery Power",
        "Total active power", "Grid Power", "Grid Power (P1)", "Grid Power (P2)", "Grid Power (P3)", "Measured Power",
        "Usage"
    ];
    let placeholders = vec!["?"; default_fields.len()].join(",");
    let sql = format!(
        "SELECT timestamp, topic, value FROM telemetry_history WHERE timestamp >= ? AND timestamp <= ? AND field IN ({}) ORDER BY timestamp ASC",
        placeholders
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    params_vec.push(Box::new(start_ts));
    params_vec.push(Box::new(end_ts));
    for f in default_fields {
        params_vec.push(Box::new(f));
    }
    let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
    let rows = stmt.query_map(params_refs.as_slice(), |row| {
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

/// Queries and decimates telemetry records in a specific window range, returning timing profiling data.
pub fn get_decimated_telemetry_in_range_profiled(
    db_path: &str,
    start_ts: i64,
    end_ts: i64,
    max_pixels: usize,
    topics: Option<&[String]>,
) -> Result<(Vec<(i64, String, f64)>, usize, f64, f64), rusqlite::Error> {
    let t0 = std::time::Instant::now();
    let records = get_telemetry_in_range_filtered(db_path, start_ts, end_ts, topics)?;
    let db_dur = t0.elapsed().as_secs_f64() * 1000.0;
    let raw_count = records.len();

    let t1 = std::time::Instant::now();
    let decimated = decimate_telemetry_records(records, start_ts, end_ts, max_pixels);
    let decimate_dur = t1.elapsed().as_secs_f64() * 1000.0;

    Ok((decimated, raw_count, db_dur, decimate_dur))
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
            "SELECT MAX(value) FROM telemetry_history WHERE topic = ?1 AND timestamp >= ?2",
            params![topic, since_timestamp],
            |row| row.get::<_, f64>(0)
        ).unwrap_or(0.0)
    } else {
        0.0
    }
}

/// Overwrites the solar forecast table with a new forecast dataset in a transaction.
pub fn delete_and_save_solar_forecast(db_path: &str, predictions: &[(i64, f64)]) -> Result<(), rusqlite::Error> {
    let mut conn = open_db_conn(db_path)?;
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM solar_forecast", [])?;
    {
        let mut stmt = tx.prepare("INSERT OR REPLACE INTO solar_forecast (timestamp, predicted_solar_w) VALUES (?1, ?2)")?;
        for (ts, predicted_w) in predictions {
            stmt.execute(params![ts, *predicted_w])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Loads solar forecast predictions within a time range.
pub fn load_solar_forecast_range(db_path: &str, start_ts: i64, end_ts: i64) -> Result<Vec<(i64, f64)>, rusqlite::Error> {
    let conn = open_db_conn(db_path)?;
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
    let conn = open_db_conn(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT timestamp, topic, value 
         FROM telemetry_history 
         WHERE (topic = ?1 OR topic = ?2) AND timestamp >= ?3 
         ORDER BY timestamp ASC"
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

/// Inserts a batch of telemetry history records in a transaction.
pub fn insert_telemetry_history_batch(
    db_path: &str,
    records: &[(i64, String, f64)],
) -> Result<(), rusqlite::Error> {
    let mut conn = open_db_conn(db_path)?;
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT OR REPLACE INTO telemetry_history (timestamp, topic, value) VALUES (?1, ?2, ?3)"
        )?;
        for (ts, topic, val) in records {
            stmt.execute(params![*ts, topic, *val])?;
        }
    }
    tx.commit()?;
    Ok(())
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
}

