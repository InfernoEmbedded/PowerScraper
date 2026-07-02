use crate::config::Config;
use crate::power_manager::HistoryRecord;
use rusqlite::{Connection, params};
use std::path::Path;

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

/// Loads the application settings configuration from the database.
pub fn load_config_from_db(db_path: &str) -> Result<Config, Box<dyn std::error::Error>> {
    let conn = open_db_conn(db_path)?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS settings (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            config_json TEXT NOT NULL
        )",
        [],
    )?;

    let mut stmt = conn.prepare("SELECT config_json FROM settings WHERE id = 1")?;
    let mut rows = stmt.query([])?;

    if let Some(row) = rows.next()? {
        let json_str: String = row.get(0)?;
        let config: Config = serde_json::from_str(&json_str)?;
        Ok(config)
    } else {
        Err("No configuration found in settings table".into())
    }
}

/// Saves the application settings configuration to the database.
pub fn save_config_to_db(db_path: &str, config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let conn = open_db_conn(db_path)?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS settings (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            config_json TEXT NOT NULL
        )",
        [],
    )?;

    let json_str = serde_json::to_string_pretty(config)?;
    conn.execute(
        "INSERT OR REPLACE INTO settings (id, config_json) VALUES (1, ?1)",
        params![json_str],
    )?;

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
            value REAL NOT NULL,
            PRIMARY KEY (timestamp, topic)
        )",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_telemetry_history_timestamp ON telemetry_history (timestamp)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_telemetry_history_topic_timestamp ON telemetry_history (topic, timestamp)",
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
            "INSERT OR REPLACE INTO telemetry_history (timestamp, topic, value) VALUES (?1, ?2, ?3)"
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Failed to prepare telemetry flush statement: {}", e);
                return;
            }
        };
        for rec in buffer.iter() {
            if let Err(e) = stmt.execute(params![rec.timestamp, rec.topic, rec.value]) {
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
}
