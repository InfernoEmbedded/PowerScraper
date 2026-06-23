use crate::config::{MqttBrokerConfig, SerialMeterConfig};
use crate::mqtt_helper::create_mqtt_client;
use rumqttc::QoS;
use std::collections::HashMap;
use tokio::time::{Duration, sleep};
use tokio_modbus::client::{Context, Reader, rtu};
use tokio_modbus::prelude::Slave;
use tokio_serial::{Parity, SerialStream, StopBits};

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
        .timeout(Duration::from_secs(config.timeout));

    let port = SerialStream::open(&builder)?;
    let ctx = rtu::attach_slave(port, Slave(1));
    Ok(ctx)
}

pub async fn run_sdm630_driver(
    port_path: String,
    config: SerialMeterConfig,
    mqtt_config: MqttBrokerConfig,
) {
    let base_topic = mqtt_config
        .base_topic
        .clone()
        .unwrap_or_else(|| "sensors".to_string());
    let device_name = port_path.replace("/dev/tty", "");
    let client_id = format!("powerscraper-sdm630-{}", device_name);
    let (mqtt_client, mut eventloop) = create_mqtt_client(&client_id, &mqtt_config);

    // Spawn dummy MQTT loop to keep connection alive
    let device_name_mqtt = device_name.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = eventloop.poll().await {
                println!("SDM630 [{}] MQTT error: {}", device_name_mqtt, e);
                sleep(Duration::from_secs(5)).await;
            }
        }
    });

    let poll_interval = Duration::from_secs(config.poll_period);
    let mut ctx_opt = None;

    loop {
        if ctx_opt.is_none() {
            match connect_serial_meter(&port_path, &config).await {
                Ok(ctx) => ctx_opt = Some(ctx),
                Err(e) => {
                    println!(
                        "SDM630 [{}] failed to connect to serial port: {}",
                        device_name, e
                    );
                    sleep(poll_interval).await;
                    continue;
                }
            }
        }

        let ctx = ctx_opt.as_mut().unwrap();
        let read_res = async {
            let reg1 = ctx.read_input_registers(0x0000, 60).await?;
            let reg2 = ctx.read_input_registers(0x003C, 48).await?;
            let reg3 = ctx.read_input_registers(0x00C8, 8).await?;
            let reg4 = ctx.read_input_registers(0x00E0, 46).await?;
            let reg5 = ctx.read_input_registers(0x014E, 48).await?;
            Ok::<_, std::io::Error>((reg1, reg2, reg3, reg4, reg5))
        }
        .await;

        match read_res {
            Ok((reg1, reg2, reg3, reg4, reg5)) => {
                let mut vals = HashMap::new();
                vals.insert("name".to_string(), device_name.clone());

                if reg1.len() >= 60 {
                    let base = 0x0000;
                    vals.insert(
                        "Phase 1 line to neutral volts".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x0000)),
                    );
                    vals.insert(
                        "Phase 2 line to neutral volts".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x0002)),
                    );
                    vals.insert(
                        "Phase 3 line to neutral volts".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x0004)),
                    );
                    vals.insert(
                        "Phase 1 current".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x0006)),
                    );
                    vals.insert(
                        "Phase 2 current".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x0008)),
                    );
                    vals.insert(
                        "Phase 3 current".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x000A)),
                    );
                    vals.insert(
                        "Phase 1 power".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x000C)),
                    );
                    vals.insert(
                        "Phase 2 power".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x000E)),
                    );
                    vals.insert(
                        "Phase 3 power".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x0010)),
                    );
                    vals.insert(
                        "Phase 1 volt amps".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x0012)),
                    );
                    vals.insert(
                        "Phase 2 volt amps".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x0014)),
                    );
                    vals.insert(
                        "Phase 3 volt amps".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x0016)),
                    );
                    vals.insert(
                        "Phase 1 volt amps reactive".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x0018)),
                    );
                    vals.insert(
                        "Phase 2 volt amps reactive".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x001A)),
                    );
                    vals.insert(
                        "Phase 3 volt amps reactive".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x001C)),
                    );
                    vals.insert(
                        "Phase 1 power factor".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x001E)),
                    );
                    vals.insert(
                        "Phase 2 power factor".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x0020)),
                    );
                    vals.insert(
                        "Phase 3 power factor".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x0022)),
                    );
                    vals.insert(
                        "Phase 1 phase angle".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x0024)),
                    );
                    vals.insert(
                        "Phase 2 phase angle".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x0026)),
                    );
                    vals.insert(
                        "Phase 3 phase angle".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x0028)),
                    );
                    vals.insert(
                        "Average line to neutral volts".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x002A)),
                    );
                    vals.insert(
                        "Average line current".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x002E)),
                    );
                    vals.insert(
                        "Sum of line currents".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x0030)),
                    );
                    vals.insert(
                        "Total system power".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x0034)),
                    );
                    vals.insert(
                        "Total system volt amps".to_string(),
                        format!("{:.3}", float32(&reg1, base, 0x0038)),
                    );
                }

                if reg2.len() >= 48 {
                    let base = 0x003C;
                    vals.insert(
                        "Total system VAr".to_string(),
                        format!("{:.3}", float32(&reg2, base, 0x003C)),
                    );
                    vals.insert(
                        "Total system power factor".to_string(),
                        format!("{:.3}", float32(&reg2, base, 0x003E)),
                    );
                    vals.insert(
                        "Total system phase angle".to_string(),
                        format!("{:.3}", float32(&reg2, base, 0x0042)),
                    );
                    vals.insert(
                        "Frequency of supply voltages".to_string(),
                        format!("{:.3}", float32(&reg2, base, 0x0046)),
                    );
                    vals.insert(
                        "Total import kWh".to_string(),
                        format!("{:.3}", float32(&reg2, base, 0x0048)),
                    );
                    vals.insert(
                        "Total export kWh".to_string(),
                        format!("{:.3}", float32(&reg2, base, 0x004A)),
                    );
                    vals.insert(
                        "Total import kVArh".to_string(),
                        format!("{:.3}", float32(&reg2, base, 0x004C)),
                    );
                    vals.insert(
                        "Total export kVArh".to_string(),
                        format!("{:.3}", float32(&reg2, base, 0x004E)),
                    );
                    vals.insert(
                        "Total VAh".to_string(),
                        format!("{:.3}", float32(&reg2, base, 0x0050)),
                    );
                    vals.insert(
                        "Ah".to_string(),
                        format!("{:.3}", float32(&reg2, base, 0x0052)),
                    );
                    vals.insert(
                        "Total system power demand".to_string(),
                        format!("{:.3}", float32(&reg2, base, 0x0054)),
                    );
                    vals.insert(
                        "Maximum total system power demand".to_string(),
                        format!("{:.3}", float32(&reg2, base, 0x0056)),
                    );
                    vals.insert(
                        "Total system VA demand".to_string(),
                        format!("{:.3}", float32(&reg2, base, 0x0064)),
                    );
                    vals.insert(
                        "Maximum total system VA demand".to_string(),
                        format!("{:.3}", float32(&reg2, base, 0x0066)),
                    );
                    vals.insert(
                        "Neutral current demand".to_string(),
                        format!("{:.3}", float32(&reg2, base, 0x0068)),
                    );
                    vals.insert(
                        "Maximum neutral current demand".to_string(),
                        format!("{:.3}", float32(&reg2, base, 0x006A)),
                    );
                }

                if reg3.len() >= 8 {
                    let base = 0x00C8;
                    vals.insert(
                        "Line 1 to Line 2 volts".to_string(),
                        format!("{:.3}", float32(&reg3, base, 0x00C8)),
                    );
                    vals.insert(
                        "Line 2 to Line 3 volts".to_string(),
                        format!("{:.3}", float32(&reg3, base, 0x00CA)),
                    );
                    vals.insert(
                        "Line 3 to Line 1 volts".to_string(),
                        format!("{:.3}", float32(&reg3, base, 0x00CC)),
                    );
                    vals.insert(
                        "Average line to line volts".to_string(),
                        format!("{:.3}", float32(&reg3, base, 0x00CE)),
                    );
                }

                if reg4.len() >= 46 {
                    let base = 0x00E0;
                    vals.insert(
                        "Neutral current".to_string(),
                        format!("{:.3}", float32(&reg4, base, 0x00E0)),
                    );
                    vals.insert(
                        "Phase 1 L-N volts THD".to_string(),
                        format!("{:.3}", float32(&reg4, base, 0x00EA)),
                    );
                    vals.insert(
                        "Phase 2 L-N volts THD".to_string(),
                        format!("{:.3}", float32(&reg4, base, 0x00EC)),
                    );
                    vals.insert(
                        "Phase 3 L-N volts THD".to_string(),
                        format!("{:.3}", float32(&reg4, base, 0x00EE)),
                    );
                    vals.insert(
                        "Phase 1 current THD".to_string(),
                        format!("{:.3}", float32(&reg4, base, 0x00F0)),
                    );
                    vals.insert(
                        "Phase 2 current THD".to_string(),
                        format!("{:.3}", float32(&reg4, base, 0x00F2)),
                    );
                    vals.insert(
                        "Phase 3 current THD".to_string(),
                        format!("{:.3}", float32(&reg4, base, 0x00F4)),
                    );
                    vals.insert(
                        "Average line to neutral volts THD".to_string(),
                        format!("{:.3}", float32(&reg4, base, 0x00F8)),
                    );
                    vals.insert(
                        "Average line current THD".to_string(),
                        format!("{:.3}", float32(&reg4, base, 0x00FA)),
                    );
                    vals.insert(
                        "Phase 1 current demand".to_string(),
                        format!("{:.3}", float32(&reg4, base, 0x0102)),
                    );
                    vals.insert(
                        "Phase 2 current demand".to_string(),
                        format!("{:.3}", float32(&reg4, base, 0x0104)),
                    );
                    vals.insert(
                        "Phase 3 current demand".to_string(),
                        format!("{:.3}", float32(&reg4, base, 0x0106)),
                    );
                    vals.insert(
                        "Maximum phase 1 current demand".to_string(),
                        format!("{:.3}", float32(&reg4, base, 0x0108)),
                    );
                    vals.insert(
                        "Maximum phase 2 current demand".to_string(),
                        format!("{:.3}", float32(&reg4, base, 0x010A)),
                    );
                    vals.insert(
                        "Maximum phase 3 current demand".to_string(),
                        format!("{:.3}", float32(&reg4, base, 0x010C)),
                    );
                }

                if reg5.len() >= 48 {
                    let base = 0x014E;
                    vals.insert(
                        "Line 1 to line 2 volts THD".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x014E)),
                    );
                    vals.insert(
                        "Line 2 to line 3 volts THD".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x0150)),
                    );
                    vals.insert(
                        "Line 3 to line 1 volts THD".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x0152)),
                    );
                    vals.insert(
                        "Average line to line volts THD".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x0154)),
                    );
                    vals.insert(
                        "Total kWh".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x0156)),
                    );
                    vals.insert(
                        "Total kvarh".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x0158)),
                    );
                    vals.insert(
                        "Phase 1 import kWh".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x015a)),
                    );
                    vals.insert(
                        "Phase 2 import kWh".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x015c)),
                    );
                    vals.insert(
                        "Phase 3 import kWh".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x015e)),
                    );
                    vals.insert(
                        "Phase 1 export kWh".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x0160)),
                    );
                    vals.insert(
                        "Phase 2 export kWh".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x0162)),
                    );
                    vals.insert(
                        "Phase 3 export kWh".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x0164)),
                    );
                    vals.insert(
                        "Phase 1 total kWh".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x0166)),
                    );
                    vals.insert(
                        "Phase 2 total kWh".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x0168)),
                    );
                    vals.insert(
                        "Phase 3 total kWh".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x016A)),
                    );
                    vals.insert(
                        "Phase 1 import kvarh".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x016c)),
                    );
                    vals.insert(
                        "Phase 2 import kvarh".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x016e)),
                    );
                    vals.insert(
                        "Phase 3 import kvarh".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x0170)),
                    );
                    vals.insert(
                        "Phase 1 export kvarh".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x0172)),
                    );
                    vals.insert(
                        "Phase 2 export kvarh".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x0174)),
                    );
                    vals.insert(
                        "Phase 3 export kvarh".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x0176)),
                    );
                    vals.insert(
                        "Phase 1 total kvarh".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x0178)),
                    );
                    vals.insert(
                        "Phase 2 total kvarh".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x017a)),
                    );
                    vals.insert(
                        "Phase 3 total kvarh".to_string(),
                        format!("{:.3}", float32(&reg5, base, 0x017c)),
                    );
                }

                for (metric, val) in vals {
                    let topic = format!("{}/{}/{}", base_topic, device_name, metric);
                    let _ = mqtt_client
                        .publish(&topic, QoS::AtMostOnce, false, val)
                        .await;
                }
            }
            Err(e) => {
                println!(
                    "SDM630 [{}] read error: {}, resetting connection",
                    device_name, e
                );
                ctx_opt = None;
            }
        }

        sleep(poll_interval).await;
    }
}

pub async fn run_dtsu666_driver(
    port_path: String,
    config: SerialMeterConfig,
    mqtt_config: MqttBrokerConfig,
) {
    let base_topic = mqtt_config
        .base_topic
        .clone()
        .unwrap_or_else(|| "sensors".to_string());
    let device_name = port_path.replace("/dev/tty", "");
    let client_id = format!("powerscraper-dtsu666-{}", device_name);
    let (mqtt_client, mut eventloop) = create_mqtt_client(&client_id, &mqtt_config);

    // Spawn dummy MQTT loop to keep connection alive
    let device_name_mqtt = device_name.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = eventloop.poll().await {
                println!("DTSU666 [{}] MQTT error: {}", device_name_mqtt, e);
                sleep(Duration::from_secs(5)).await;
            }
        }
    });

    let poll_interval = Duration::from_secs(config.poll_period);
    let mut ctx_opt = None;

    loop {
        if ctx_opt.is_none() {
            match connect_serial_meter(&port_path, &config).await {
                Ok(ctx) => ctx_opt = Some(ctx),
                Err(e) => {
                    println!(
                        "DTSU666 [{}] failed to connect to serial port: {}",
                        device_name, e
                    );
                    sleep(poll_interval).await;
                    continue;
                }
            }
        }

        let ctx = ctx_opt.as_mut().unwrap();
        let read_res = async {
            let reg1 = ctx.read_input_registers(0x2000, 0x52).await?;
            let reg2 = ctx.read_input_registers(0x401E, 52).await?;
            Ok::<_, std::io::Error>((reg1, reg2))
        }
        .await;

        match read_res {
            Ok((reg1, reg2)) => {
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

                for (metric, val) in vals {
                    let topic = format!("{}/{}/{}", base_topic, device_name, metric);
                    let _ = mqtt_client
                        .publish(&topic, QoS::AtMostOnce, false, val)
                        .await;
                }
            }
            Err(e) => {
                println!(
                    "DTSU666 [{}] read error: {}, resetting connection",
                    device_name, e
                );
                ctx_opt = None;
            }
        }

        sleep(poll_interval).await;
    }
}
