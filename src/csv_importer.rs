use crate::config::Config;
use std::error::Error;
use std::fs::File;
use std::path::Path;

struct ColumnMapping {
    index: usize,
    topic: String,
}

fn get_topic_for_column(col_name: &str, mains_source: &str) -> Option<String> {
    match col_name {
        "solar1_pv1_power" => Some("solax1/PV1 Power".to_string()),
        "solar1_pv2_power" => Some("solax1/PV2 Power".to_string()),
        "solar2_pv1_power" => Some("solax2/PV1 Power".to_string()),
        "solar2_pv2_power" => Some("solax2/PV2 Power".to_string()),
        "solax_x3_pv1_power" => Some("solax_x3/PV1 Power".to_string()),
        "solax_x3_pv2_power" => Some("solax_x3/PV2 Power".to_string()),
        "aurora_pv1_power" => Some("Aurora/Input 1 Power".to_string()),
        "aurora_pv2_power" => Some("Aurora/Input 2 Power".to_string()),
        "amber_import_price" => Some("tariff/import_price".to_string()),
        "amber_export_price" => Some("tariff/export_price".to_string()),
        "global_usage" => Some(format!("{}/Total system power", mains_source)),
        "mains_power" => Some(format!("{}/Total system power", mains_source)),
        "usage_and_charging" => Some("Global/Usage and Charging".to_string()),
        "weather_clouds" => Some("weather/clouds".to_string()),
        "weather_temperature" => Some("weather/temperature".to_string()),
        "weather_humidity" => Some("weather/humidity".to_string()),
        "weather_pressure" => Some("weather/pressure".to_string()),
        _ => None,
    }
}

pub fn run_csv_import(db_path: &str, csv_path: &str) -> Result<(), Box<dyn Error>> {
    println!("Initializing import from {} into database {}...", csv_path, db_path);

    if !Path::new(csv_path).exists() {
        return Err(format!("CSV file not found: {}", csv_path).into());
    }

    // Initialize historical schema in SQLite database
    crate::database::init_history_db(db_path)?;

    // Load active config to find configured Mains Meter source name
    let config = Config::load_from_db(db_path).ok();
    let mains_source = config
        .as_ref()
        .and_then(|c| c.battery_control.as_ref())
        .and_then(|b| b.source.as_ref())
        .map(|s| s.as_str())
        .unwrap_or("MainsMeter");

    println!("Using mains meter source name: {}", mains_source);

    // Open CSV file
    let file = File::open(csv_path)?;
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(file);

    // Read headers and build mapping
    let headers = rdr.headers()?;
    let mut timestamp_idx = None;
    let mut mappings = Vec::new();

    for (idx, header) in headers.iter().enumerate() {
        if header == "timestamp" {
            timestamp_idx = Some(idx);
        } else if let Some(topic) = get_topic_for_column(header, mains_source) {
            mappings.push(ColumnMapping {
                index: idx,
                topic,
            });
        }
    }

    let timestamp_idx = match timestamp_idx {
        Some(idx) => idx,
        None => return Err("CSV file is missing 'timestamp' column in headers.".into()),
    };

    println!("Mapped columns to topics:");
    for mapping in &mappings {
        println!("  Column '{}' -> Topic '{}'", &headers[mapping.index], &mapping.topic);
    }

    let mut total_rows = 0;
    let mut total_inserts = 0;
    let mut batch = Vec::new();

    for result in rdr.records() {
        let record = result?;
        total_rows += 1;

        // Extract and parse timestamp
        let ts_str = match record.get(timestamp_idx) {
            Some(s) => s.trim(),
            None => continue,
        };
        if ts_str.is_empty() {
            continue;
        }
        let timestamp: i64 = match ts_str.parse() {
            Ok(val) => val,
            Err(e) => {
                eprintln!("Row {}: Failed to parse timestamp '{}': {}", total_rows, ts_str, e);
                continue;
            }
        };

        // Process each mapped telemetry column in the row
        for mapping in &mappings {
            let val_str = match record.get(mapping.index) {
                Some(s) => s.trim(),
                None => "",
            };
            if val_str.is_empty() {
                continue; // Missing values (empty cell) are ignored
            }

            let value: f64 = match val_str.parse() {
                Ok(v) => v,
                Err(_) => continue, // Ignore invalid/non-numeric cells gracefully
            };

            batch.push((timestamp, mapping.topic.clone(), value));
            total_inserts += 1;
        }

        if batch.len() >= 50000 {
            crate::database::insert_telemetry_history_batch(db_path, &batch)?;
            batch.clear();
        }

        if total_rows % 10000 == 0 {
            println!("Processed {} rows, inserted {} points so far...", total_rows, total_inserts);
        }
    }

    if !batch.is_empty() {
        crate::database::insert_telemetry_history_batch(db_path, &batch)?;
    }

    println!("Successfully imported {} records (total {} data points) from CSV.", total_rows, total_inserts);
    Ok(())
}
