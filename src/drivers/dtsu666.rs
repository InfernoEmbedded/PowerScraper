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

    loop {
        if cancel_token.is_cancelled() {
            break;
        }

        // Stale Meter Watchdog: If no valid telemetry update has been received for 3 minutes,
        // force a connection reset to flush TTY serial buffers and restart polling clean.
        if last_successful_read.elapsed() >= watchdog_timeout {
            println!(
                "DTSU666 [{}] Stale meter detected (no telemetry update for 3 mins). Flushing serial buffers & restarting connection...",
                device_name
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
            match connect_serial_meter(&port_path, &config).await {
                Ok(ctx) => ctx_opt = Some(ctx),
                Err(e) => {
                    println!(
                        "DTSU666 [{}] failed to connect to serial port: {}",
                        device_name, e
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
                        "Line 1 to Line 2 volts".to_string(),
                        format!("{:.1}", float32(&reg1, base, 0x2000) / 10.0),
                    );
                    vals.insert(
                        "Line 2 to Line 3 volts".to_string(),
                        format!("{:.1}", float32(&reg1, base, 0x2002) / 10.0),
                    );
                    vals.insert(
                        "Line 3 to Line 1 volts".to_string(),
                        format!("{:.1}", float32(&reg1, base, 0x2004) / 10.0),
                    );
                    vals.insert(
                        "Phase 1 line to neutral volts".to_string(),
                        format!("{:.1}", float32(&reg1, base, 0x2006) / 10.0),
                    );
                    vals.insert(
                        "Phase 2 line to neutral volts".to_string(),
                        format!("{:.1}", float32(&reg1, base, 0x2008) / 10.0),
                    );
                    vals.insert(
                        "Phase 3 line to neutral volts".to_string(),
                        format!("{:.1}", float32(&reg1, base, 0x200A) / 10.0),
                    );
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
                        format!("{:.1}", float32(&reg1, base, 0x2014) / 10.0),
                    );
                    vals.insert(
                        "Phase 2 power".to_string(),
                        format!("{:.1}", float32(&reg1, base, 0x2016) / 10.0),
                    );
                    vals.insert(
                        "Phase 3 power".to_string(),
                        format!("{:.1}", float32(&reg1, base, 0x2018) / 10.0),
                    );
                    vals.insert(
                        "Phase 1 volt amps reactive".to_string(),
                        format!("{:.1}", float32(&reg1, base, 0x201C) / 10.0),
                    );
                    vals.insert(
                        "Phase 2 volt amps reactive".to_string(),
                        format!("{:.1}", float32(&reg1, base, 0x201E) / 10.0),
                    );
                    vals.insert(
                        "Phase 3 volt amps reactive".to_string(),
                        format!("{:.1}", float32(&reg1, base, 0x2020) / 10.0),
                    );
                    vals.insert(
                        "Phase 1 power factor".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x202C) / 1000.0),
                    );
                    vals.insert(
                        "Phase 2 power factor".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x202E) / 1000.0),
                    );
                    vals.insert(
                        "Phase 3 power factor".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x2030) / 1000.0),
                    );
                    vals.insert(
                        "Total system power".to_string(),
                        format!("{:.1}", float32(&reg1, base, 0x2012) / 10.0),
                    );
                    vals.insert(
                        "Total system VAr".to_string(),
                        format!("{:.1}", float32(&reg1, base, 0x201A) / 10.0),
                    );
                    vals.insert(
                        "Total system power factor".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x202A) / 1000.0),
                    );
                    vals.insert(
                        "Frequency Of supply voltages".to_string(),
                        format!("{:.2}", float32(&reg1, base, 0x2044) / 100.0),
                    );
                    vals.insert(
                        "Total system power demand".to_string(),
                        format!("{:.1}", float32(&reg1, base, 0x2044) / 10.0),
                    );
                }

                if reg2.len() >= 52 {
                    let base = 0x401E;
                    vals.insert(
                        "Total import kWh".to_string(),
                        format!("{:.1}", float32(&reg2, base, 0x401E) * 1000.0),
                    );
                    vals.insert(
                        "Total export kWh".to_string(),
                        format!("{:.1}", float32(&reg2, base, 0x4028) * 1000.0),
                    );
                    vals.insert(
                        "Total Q1 kvarh".to_string(),
                        format!("{:.1}", float32(&reg2, base, 0x4032) * 1000.0),
                    );
                    vals.insert(
                        "Total Q2 kvarh".to_string(),
                        format!("{:.1}", float32(&reg2, base, 0x403C) * 1000.0),
                    );
                    vals.insert(
                        "Total Q3 kvarh".to_string(),
                        format!("{:.1}", float32(&reg2, base, 0x4046) * 1000.0),
                    );
                    vals.insert(
                        "Total Q4 kvarh".to_string(),
                        format!("{:.1}", float32(&reg2, base, 0x4050) * 1000.0),
                    );
                }

                let mut numeric_metrics = HashMap::new();
                for (metric, val_str) in vals {
                    if let Ok(num) = val_str.parse::<f64>() {
                        numeric_metrics.insert(metric, num);
                    }
                }
                if !numeric_metrics.is_empty() {
                    let batch = crate::dispatch_manager::TelemetryBatch {
                        device_name: device_name.clone(),
                        timestamp: chrono::Utc::now().timestamp(),
                        metrics: numeric_metrics,
                    };
                    if tx_telemetry.try_send(batch).is_ok() {
                        last_successful_read = tokio::time::Instant::now();
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
                    "DTSU666 [{}] read timeout, resetting connection",
                    device_name
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
