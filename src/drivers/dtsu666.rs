use crate::config::SerialMeterConfig;
use std::collections::HashMap;
use tokio::time::{Duration, sleep};
use tokio_modbus::client::{Context, Reader, rtu};
use tokio_modbus::prelude::Slave;
use tokio_serial::{ClearBuffer, Parity, SerialPort, SerialStream, StopBits};
use tokio_util::sync::CancellationToken;

fn float32(registers: &[u16], base: usize, addr: usize) -> f32 {
    let low = registers[addr - base];
    let high = registers[addr - base + 1];

    let bytes = [
        (high & 0xff) as u8,
        (high >> 8) as u8,
        (low & 0xff) as u8,
        (low >> 8) as u8,
    ];
    f32::from_le_bytes(bytes)
}

async fn connect_serial_meter(
    port_path: &str,
    config: &SerialMeterConfig,
) -> Result<Context, std::io::Error> {
    let serial_parity = match config.parity.as_str() {
        "E" => Parity::Even,
        "O" => Parity::Odd,
        _ => Parity::None,
    };

    let serial_stopbits = match config.stopbits {
        2 => StopBits::Two,
        _ => StopBits::One,
    };

    let builder = tokio_serial::new(port_path, config.baud)
        .parity(serial_parity)
        .stop_bits(serial_stopbits)
        .timeout(Duration::from_secs_f64(config.timeout));

    let mut port = SerialStream::open(&builder)?;
    let _ = port.clear(ClearBuffer::All);
    let ctx = rtu::attach_slave(port, Slave(1));
    Ok(ctx)
}

pub async fn run_dtsu666_driver(
    port_path: String,
    config: SerialMeterConfig,
    tx_telemetry: tokio::sync::mpsc::Sender<crate::dispatch_manager::TelemetryBatch>,
    cancel_token: CancellationToken,
) {
    let device_name = port_path.replace("/dev/tty", "");
    let poll_interval = Duration::from_secs_f64(config.poll_period);
    let mut ctx_opt: Option<Context> = None;
    let mut last_successful_read = tokio::time::Instant::now();
    let watchdog_timeout = Duration::from_secs(180); // 3 minutes watchdog

    let last_success_ts = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
        chrono::Utc::now().timestamp() as u64,
    ));

    // Independent background watchdog task to detect if polling loop freezes or hangs
    let watchdog_last_success = last_success_ts.clone();
    let watchdog_cancel = cancel_token.clone();
    let watchdog_device = device_name.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = watchdog_cancel.cancelled() => break,
                _ = sleep(Duration::from_secs(30)) => {}
            }
            let now = chrono::Utc::now().timestamp() as u64;
            let last = watchdog_last_success.load(std::sync::atomic::Ordering::Relaxed);
            let elapsed = now.saturating_sub(last);
            if elapsed >= 180 {
                println!(
                    "DTSU666 [{}] WATCHDOG ALERT: Stale meter! No telemetry update for {}s (> 3 mins). Polling loop or serial port may be hung.",
                    watchdog_device, elapsed
                );
            }
        }
    });

    println!(
        "DTSU666 [{}] Driver starting (port: {}, baud: {}, poll_period: {}s, timeout: {}s)...",
        device_name, port_path, config.baud, config.poll_period, config.timeout
    );

    loop {
        if cancel_token.is_cancelled() {
            println!("DTSU666 [{}] Cancelled. Exiting driver loop.", device_name);
            break;
        }

        let elapsed_since_success = last_successful_read.elapsed();
        if elapsed_since_success >= watchdog_timeout {
            println!(
                "DTSU666 [{}] Stale meter detected ({:.1}s without valid telemetry update). Resetting serial connection...",
                device_name, elapsed_since_success.as_secs_f64()
            );
            ctx_opt = None;
            last_successful_read = tokio::time::Instant::now();
            tokio::select! {
                _ = cancel_token.cancelled() => break,
                _ = sleep(Duration::from_millis(100)) => {}
            }
            continue;
        }

        if ctx_opt.is_none() {
            println!("DTSU666 [{}] Connecting to serial port {}...", device_name, port_path);
            match connect_serial_meter(&port_path, &config).await {
                Ok(ctx) => {
                    println!("DTSU666 [{}] Serial port {} opened successfully.", device_name, port_path);
                    ctx_opt = Some(ctx);
                }
                Err(e) => {
                    println!(
                        "DTSU666 [{}] Failed to connect to serial port {}: {}",
                        device_name, port_path, e
                    );
                    tokio::select! {
                        _ = cancel_token.cancelled() => break,
                        _ = sleep(poll_interval) => {}
                    }
                    continue;
                }
            }
        }

        let ctx = ctx_opt.as_mut().unwrap();
        let timeout_dur = Duration::from_secs_f64(config.timeout);
        let read_res = tokio::time::timeout(timeout_dur, async {
            // Split into <= 60 register chunks to comply with Modbus RTU transaction limits
            let part1 = ctx.read_input_registers(0x2000, 40).await?;
            let part2 = ctx.read_input_registers(0x2028, 42).await?;
            let mut reg1 = part1;
            reg1.extend(part2);
            let reg2 = ctx.read_input_registers(0x401E, 52).await?;
            Ok::<_, std::io::Error>((reg1, reg2))
        })
        .await;

        match read_res {
            Ok(Ok((reg1, reg2))) => {
                let mut vals = HashMap::new();
                vals.insert("name".to_string(), device_name.clone());

                if reg1.len() >= 0x52 {
                    let base = 0x2000;
                    vals.insert(
                        "Phase 1 current".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x200C) / 1000.0),
                    );
                    vals.insert(
                        "Phase 2 current".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x200E) / 1000.0),
                    );
                    vals.insert(
                        "Phase 3 current".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x2010) / 1000.0),
                    );
                    vals.insert(
                        "Phase 1 power".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x2012)),
                    );
                    vals.insert(
                        "Phase 2 power".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x2014)),
                    );
                    vals.insert(
                        "Phase 3 power".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x2016)),
                    );
                    vals.insert(
                        "Total power".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x2018)),
                    );
                    vals.insert(
                        "Phase 1 reactive power".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x201A)),
                    );
                    vals.insert(
                        "Phase 2 reactive power".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x201C)),
                    );
                    vals.insert(
                        "Phase 3 reactive power".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x201E)),
                    );
                    vals.insert(
                        "Total reactive power".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x2020)),
                    );
                    vals.insert(
                        "Phase 1 power factor".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x202A) / 1000.0),
                    );
                    vals.insert(
                        "Phase 2 power factor".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x202C) / 1000.0),
                    );
                    vals.insert(
                        "Phase 3 power factor".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x202E) / 1000.0),
                    );
                    vals.insert(
                        "Total power factor".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x2030) / 1000.0),
                    );
                    vals.insert(
                        "Frequency".to_string(),
                        format!("{:.2}", float32(&reg1, base, 0x2036) / 100.0),
                    );
                    vals.insert(
                        "Import kWh".to_string(),
                        format!("{:.2}", float32(&reg1, base, 0x2038) / 100.0),
                    );
                    vals.insert(
                        "Export kWh".to_string(),
                        format!("{:.2}", float32(&reg1, base, 0x203A) / 100.0),
                    );
                }

                if reg2.len() >= 0x34 {
                    let base = 0x401E;
                    vals.insert(
                        "Phase 1 import kWh".to_string(),
                        format!("{:.2}", float32(&reg2, base, 0x401E) / 100.0),
                    );
                    vals.insert(
                        "Phase 2 import kWh".to_string(),
                        format!("{:.2}", float32(&reg2, base, 0x4020) / 100.0),
                    );
                    vals.insert(
                        "Phase 3 import kWh".to_string(),
                        format!("{:.2}", float32(&reg2, base, 0x4022) / 100.0),
                    );
                    vals.insert(
                        "Phase 1 export kWh".to_string(),
                        format!("{:.2}", float32(&reg2, base, 0x4024) / 100.0),
                    );
                    vals.insert(
                        "Phase 2 export kWh".to_string(),
                        format!("{:.2}", float32(&reg2, base, 0x4026) / 100.0),
                    );
                    vals.insert(
                        "Phase 3 export kWh".to_string(),
                        format!("{:.2}", float32(&reg2, base, 0x4028) / 100.0),
                    );
                }

                let mut numeric_metrics = HashMap::new();
                for (metric, val_str) in vals {
                    if let Ok(num) = val_str.parse::<f64>() {
                        numeric_metrics.insert(metric, num);
                    }
                }
                if !numeric_metrics.is_empty() {
                    let metric_count = numeric_metrics.len();
                    let batch = crate::dispatch_manager::TelemetryBatch {
                        device_name: device_name.clone(),
                        timestamp: chrono::Utc::now().timestamp(),
                        metrics: numeric_metrics,
                    };
                    match tx_telemetry.try_send(batch) {
                        Ok(_) => {
                            last_success_ts.store(chrono::Utc::now().timestamp() as u64, std::sync::atomic::Ordering::Relaxed);
                            last_successful_read = tokio::time::Instant::now();
                        }
                        Err(e) => {
                            println!(
                                "DTSU666 [{}] ERROR: Telemetry queue full / failed to send: {}",
                                device_name, e
                            );
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                println!(
                    "DTSU666 [{}] read error: {}, resetting connection",
                    device_name, e
                );
                ctx_opt = None;
            }
            Err(_) => {
                println!(
                    "DTSU666 [{}] read TIMEOUT ({}s limit reached), resetting connection",
                    device_name, timeout_dur.as_secs_f64()
                );
                ctx_opt = None;
            }
        }

        tokio::select! {
            _ = cancel_token.cancelled() => break,
            _ = sleep(poll_interval) => {}
        }
    }
}
