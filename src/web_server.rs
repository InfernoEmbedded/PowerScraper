use crate::config::Config;
use axum::{
    Json, Router,
    http::header,
    response::{Html, IntoResponse},
    routing::{get, post},
};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock, atomic::{AtomicBool, Ordering}};
use tokio::sync::mpsc::Sender;
use tower_http::cors::CorsLayer;

#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct InverterStatus {
    pub battery_capacity: u8,
    pub battery_power: i32,
    pub pv_power: u32,
    pub run_mode: u32,
    pub last_updated: Option<u64>,
    pub calculated_battery_capacity: Option<f64>,
    pub requested_power: Option<i32>,
    pub command_power: Option<i32>,
    pub inverter_fault: Option<u32>,
    pub charger_fault: Option<u32>,
    pub manager_fault: Option<u32>,
    pub bms_warning: Option<u32>,
    pub error_code: Option<u32>,
    pub is_error: bool,
    pub error_text: Option<String>,
    pub raw_metrics: HashMap<String, String>,
    pub driver_type: Option<String>,
}

pub fn evaluate_inverter_errors(inv: &mut InverterStatus, now_secs: u64) {
    let mut errors = Vec::new();

    // 1. Preserve driver-deciphered hardware error text
    if let Some(ref text) = inv.error_text {
        if !text.is_empty() {
            errors.push(text.clone());
        }
    }

    // 2. Check communication staleness (15s threshold)
    if let Some(last_upd) = inv.last_updated {
        if now_secs > last_upd && (now_secs - last_upd) > 15 {
            let age = now_secs - last_upd;
            errors.push(format!("Offline / Communication Lost (no update for {}s)", age));
        }
    } else {
        errors.push("Offline / Never Connected".to_string());
    }

    if !errors.is_empty() {
        inv.is_error = true;
        inv.error_text = Some(errors.join(" | "));
    } else {
        inv.is_error = false;
        inv.error_text = None;
    }
}

pub static SYSTEM_STATUS: OnceLock<Mutex<SystemStatus>> = OnceLock::new();
pub static PENDING_MODBUS_WRITES: OnceLock<Mutex<HashMap<String, VecDeque<(u16, u16)>>>> = OnceLock::new();

pub fn get_system_status() -> &'static Mutex<SystemStatus> {
    SYSTEM_STATUS.get_or_init(|| Mutex::new(SystemStatus::default()))
}

pub fn get_system_status_lock() -> std::sync::MutexGuard<'static, SystemStatus> {
    get_system_status().lock().unwrap_or_else(|e| e.into_inner())
}

pub fn get_pending_modbus_writes() -> &'static Mutex<HashMap<String, VecDeque<(u16, u16)>>> {
    PENDING_MODBUS_WRITES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn enqueue_modbus_write(inverter: &str, register: u16, value: u16) {
    if let Ok(mut map) = get_pending_modbus_writes().lock() {
        map.entry(inverter.to_string()).or_default().push_back((register, value));
    }
}

pub fn pop_pending_modbus_write(inverter: &str) -> Option<(u16, u16)> {
    if let Ok(mut map) = get_pending_modbus_writes().lock() {
        map.get_mut(inverter)?.pop_front()
    } else {
        None
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct MeterStatus {
    pub last_updated: Option<u64>,
    pub is_error: bool,
    pub error_text: Option<String>,
    pub raw_metrics: HashMap<String, String>,
    pub driver_type: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default)]
pub struct PriceThresholds {
    pub import_30: f64,
    pub import_70: f64,
    pub export_30: f64,
    pub export_70: f64,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct SystemStatus {
    pub active_mode: String,
    pub grid_target: f64,
    pub inverters: HashMap<String, InverterStatus>,
    pub meters: HashMap<String, MeterStatus>,
    pub meter_power: f64,
    pub meter_last_updated: Option<u64>,
    pub mqtt_connected: bool,
    pub mqtt_enabled: bool,
    pub import_price: Option<f64>,
    pub export_price: Option<f64>,
    pub battery_unit_cost: Option<f64>,
    pub battery_kwh: Option<f64>,
    pub battery_soc: Option<f64>,
    pub price_thresholds: Option<PriceThresholds>,
    pub usage: Option<f64>,
    pub power_budget: Option<f64>,
    pub power_budget_with_charging: Option<f64>,
    pub version: String,
}

impl Default for SystemStatus {
    fn default() -> Self {
        SystemStatus {
            active_mode: String::new(),
            grid_target: 0.0,
            inverters: HashMap::new(),
            meters: HashMap::new(),
            meter_power: 0.0,
            meter_last_updated: None,
            mqtt_connected: false,
            mqtt_enabled: false,
            import_price: None,
            export_price: None,
            battery_unit_cost: None,
            battery_kwh: None,
            battery_soc: None,
            price_thresholds: None,
            usage: None,
            power_budget: None,
            power_budget_with_charging: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct RegisterDetail {
    pub address_hex: String,
    pub address_dec: u16,
    pub name: String,
    pub value: String,
    pub unit: String,
    pub category: String,
    pub writable: bool,
    pub description: String,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct InverterRegistersResponse {
    pub name: String,
    pub driver_type: String,
    pub last_updated: Option<u64>,
    pub is_error: bool,
    pub error_text: Option<String>,
    pub registers: Vec<RegisterDetail>,
}

#[derive(serde::Deserialize)]
pub struct WriteRegisterRequest {
    pub inverter: String,
    pub register: Option<u16>,
    pub register_hex: Option<String>,
    pub value: u16,
}

pub fn build_inverter_register_details(inv_name: &str, inv: &InverterStatus) -> Vec<RegisterDetail> {
    let solax_v250: &[(&str, u16, &str, &str, &str, bool, &str)] = &[
        ("0x0000", 0, "Grid Voltage", "Grid", "V", false, "AC Mains Grid Line Voltage"),
        ("0x0001", 1, "Grid Current", "Grid", "A", false, "AC Mains Grid Current"),
        ("0x0002", 2, "Grid Power", "Grid", "W", false, "AC Mains Grid Active Power"),
        ("0x0003", 3, "Inverter Power", "Inverter", "W", false, "Active Inverter Power Output"),
        ("0x0004", 4, "PV1 Voltage", "Solar PV", "V", false, "PV String 1 DC Input Voltage"),
        ("0x0005", 5, "PV2 Voltage", "Solar PV", "V", false, "PV String 2 DC Input Voltage"),
        ("0x0006", 6, "PV2 Current", "Solar PV", "A", false, "PV String 2 DC Input Current"),
        ("0x0007", 7, "Grid Frequency", "Grid", "Hz", false, "AC Mains Grid Frequency"),
        ("0x0008", 8, "Inner Temp", "Status", "°C", false, "Inverter Internal Operating Temperature"),
        ("0x0009", 9, "Run Mode", "Status", "", false, "Operating Mode: 0: Wait, 1: Check, 2: Normal, 3: Fault, 4: Permanent Fault, 7: EPS"),
        ("0x000A", 10, "PV1 Power", "Solar PV", "W", false, "PV String 1 Active DC Generation"),
        ("0x000B", 11, "PV2 Power", "Solar PV", "W", false, "PV String 2 Active DC Generation"),
        ("0x000C", 12, "PV1 Current", "Solar PV", "A", false, "PV String 1 DC Input Current"),
        ("0x000D", 13, "PV2 Current", "Solar PV", "A", false, "PV String 2 DC Input Current"),
        ("0x000E", 14, "PV1 Power", "Solar PV", "W", false, "PV String 1 Active DC Generation"),
        ("0x000F", 15, "PV2 Power", "Solar PV", "W", false, "PV String 2 Active DC Generation"),
        ("0x0014", 20, "Battery Voltage", "Battery", "V", false, "Battery Bank Terminal DC Voltage"),
        ("0x0015", 21, "Battery Current", "Battery", "A", false, "Battery Charge / Discharge Current"),
        ("0x0016", 22, "Battery Power", "Battery", "W", false, "Battery Active Power (Positive = Charge, Negative = Discharge)"),
        ("0x0017", 23, "Charger Board Temperature", "Battery", "°C", false, "Battery Charger Control Board Temperature"),
        ("0x0018", 24, "Battery Temperature", "Battery", "°C", false, "Internal Battery Module Cell Temperature"),
        ("0x0018", 24, "Charger Battery Temperature", "Battery", "°C", false, "Charger Battery Temperature Sensor"),
        ("0x0019", 25, "Charger Boost Temperature", "Battery", "°C", false, "Charger Boost Stage Heatsink Temperature"),
        ("0x001C", 28, "Battery Capacity", "Battery", "%", false, "Battery State of Charge (SOC)"),
        ("0x001D", 29, "Battery Energy Discharged", "Battery", "kWh", false, "Lifetime Total Battery Energy Discharged"),
        ("0x001F", 31, "BMS Warning", "Faults", "", false, "BMS Warning & Status Bitmask"),
        ("0x0020", 32, "Battery Energy Charged", "Battery", "kWh", false, "Lifetime Total Battery Energy Charged"),
        ("0x0023", 35, "Battery State of Health", "Battery", "%", false, "Battery Module Health Index (SOH)"),
        ("0x0028", 40, "Battery State of Health", "Battery", "%", false, "Battery Module SOH Health Index"),
        ("0x0040", 64, "Inverter Hardware Fault", "Faults", "", false, "Inverter Hardware Fault Bitmask"),
        ("0x0040", 64, "Inverter Fault", "Faults", "", false, "Inverter Fault Bitmask"),
        ("0x0042", 66, "Charger Fault", "Faults", "", false, "Charger Subsystem Fault Bitmask"),
        ("0x0043", 67, "Manager Fault", "Faults", "", false, "Inverter Manager / Communication Fault Bitmask"),
        ("0x0046", 70, "Measured Power", "Grid", "W", false, "External Grid CT / Meter Active Power Measurement"),
        ("0x0048", 72, "Feed In Energy", "Grid", "kWh", false, "Lifetime Total Solar Grid Feed-in Energy"),
        ("0x004A", 74, "Consumed Energy", "Grid", "kWh", false, "Lifetime Total Grid Consumed Energy"),
        ("0x004C", 76, "EPS Voltage", "EPS", "V", false, "Emergency Power Supply Output Voltage"),
        ("0x004D", 77, "EPS Current", "EPS", "A", false, "Emergency Power Supply Output Current"),
        ("0x004E", 78, "EPS VA", "EPS", "VA", false, "Emergency Power Supply Apparent Power"),
        ("0x004F", 79, "EPS Frequency", "EPS", "Hz", false, "Emergency Power Supply Output Frequency"),
        ("0x0050", 80, "Energy Today", "Solar PV", "kWh", false, "Daily Solar PV Generation"),
        ("0x0052", 82, "Energy Total", "Solar PV", "kWh", false, "Lifetime Total Solar PV Generation"),
        ("0x0055", 85, "Battery Temperature", "Battery", "°C", false, "Battery Temperature (Extended Register)"),

        // Configurable Holding Registers for SK-SU / Solax V2.50
        ("0x0015", 21, "PV Start Voltage", "Settings", "V", true, "Launch Voltage Threshold (Write Reg 0x0001)"),
        ("0x0016", 22, "Start Wait Time", "Settings", "s", true, "Launch Wait Time (Write Reg 0x0002)"),
        ("0x0017", 23, "PV High Stop Voltage", "Settings", "V", true, "Input High Voltage Protect (Write Reg 0x0003)"),
        ("0x0018", 24, "PV Low Stop Voltage", "Settings", "V", true, "Input Low Voltage Protect (Write Reg 0x0004)"),
        ("0x0019", 25, "Min Grid Voltage Protect", "Settings", "V", true, "Allowed Minimum Grid Voltage (Write Reg 0x0005)"),
        ("0x001A", 26, "Max Grid Voltage Protect", "Settings", "V", true, "Allowed Maximum Grid Voltage (Write Reg 0x0006)"),
        ("0x001B", 27, "Min Grid Freq Protect", "Settings", "Hz", true, "Allowed Minimum Grid Frequency (Write Reg 0x0007)"),
        ("0x001C", 28, "Max Grid Freq Protect", "Settings", "Hz", true, "Allowed Maximum Grid Frequency (Write Reg 0x0008)"),
        ("0x001D", 29, "Safety Type", "Settings", "", true, "Grid Safety Code 0-18 (Write Reg 0x0009)"),
        ("0x001E", 30, "PV Connection Mode", "Settings", "", true, "PV Mode: 1:Comm, 2:Multi (Write Reg 0x000A)"),
        ("0x001F", 31, "10Min Overvoltage Protect", "Settings", "V", true, "10 Minute Average Overvoltage Protect (Write Reg 0x000B)"),
        ("0x0020", 32, "Min Slow Grid Voltage Protect", "Settings", "V", true, "Min Slow Grid Voltage Protect (Write Reg 0x000C)"),
        ("0x0021", 33, "Max Slow Grid Voltage Protect", "Settings", "V", true, "Max Slow Grid Voltage Protect (Write Reg 0x000D)"),
        ("0x0022", 34, "Min Slow Grid Freq Protect", "Settings", "Hz", true, "Min Slow Grid Frequency Protect (Write Reg 0x000E)"),
        ("0x0023", 35, "Max Slow Grid Freq Protect", "Settings", "Hz", true, "Max Slow Grid Frequency Protect (Write Reg 0x000F)"),
        ("0x0024", 36, "DCI Limit", "Settings", "mA", true, "DC Component Current Limit (Write Reg 0x0010)"),
        ("0x0025", 37, "Active Power Limit", "Settings", "%", true, "Output Active Power Limit Percent (Write Reg 0x0011)"),
        ("0x007C", 124, "Power Manager Enable", "Remote Control", "", true, "Power Manager Enable (Write Reg 0x001B)"),
        ("0x008B", 139, "Work Mode", "Battery Control", "", true, "Operating Mode: 0:Self Use, 1:Force Time, 2:Remote (Write Reg 0x001F)"),
        ("0x008C", 140, "Battery Min Capacity", "Battery Control", "%", true, "Minimum Battery SOC Limit (Write Reg 0x0020)"),
        ("0x008D", 141, "Battery Type", "Battery Control", "", true, "Battery Type: 0:Lead Acid, 1:Lithium (Write Reg 0x0021)"),
        ("0x008E", 142, "Charge Cutoff Voltage", "Battery Control", "V", true, "Battery Charge Cutoff Voltage (Write Reg 0x0022)"),
        ("0x008F", 143, "Discharge Cutoff Voltage", "Battery Control", "V", true, "Battery Discharge Cutoff Voltage (Write Reg 0x0023)"),
        ("0x0090", 144, "Max Charge Current", "Battery Control", "A", true, "Max Battery Charge Current (Write Reg 0x0024)"),
        ("0x0091", 145, "Max Discharge Current", "Battery Control", "A", true, "Max Battery Discharge Current (Write Reg 0x0025)"),
        ("0x0092", 146, "Charge Window 1 Start", "Timer Windows", "HH:MM", true, "Charge Window 1 Start Time (Write Reg 0x0026)"),
        ("0x0094", 148, "Charge Window 1 End", "Timer Windows", "HH:MM", true, "Charge Window 1 End Time (Write Reg 0x0027)"),
        ("0x0096", 150, "Discharge Window 1 Start", "Timer Windows", "HH:MM", true, "Discharge Window 1 Start Time (Write Reg 0x0028)"),
        ("0x0098", 152, "Discharge Window 1 End", "Timer Windows", "HH:MM", true, "Discharge Window 1 End Time (Write Reg 0x0029)"),
        ("0x009A", 154, "Charge Window 2 Start", "Timer Windows", "HH:MM", true, "Charge Window 2 Start Time (Write Reg 0x002A)"),
        ("0x009C", 156, "Charge Window 2 End", "Timer Windows", "HH:MM", true, "Charge Window 2 End Time (Write Reg 0x002B)"),
        ("0x009E", 158, "Discharge Window 2 Start", "Timer Windows", "HH:MM", true, "Discharge Window 2 Start Time (Write Reg 0x002C)"),
        ("0x00A0", 160, "Discharge Window 2 End", "Timer Windows", "HH:MM", true, "Discharge Window 2 End Time (Write Reg 0x002D)"),
        ("0x00B4", 180, "Allow Grid Charge", "Battery Control", "", true, "Allow Charging from Grid: 0:Forbidden, 1:P1, 2:P2, 3:Both (Write Reg 0x0040)"),
        ("0x00B5", 181, "Export Control Factory Limit", "Grid", "W", true, "Export Power Control Factory Limit (Write Reg 0x0041)"),
        ("0x00B6", 182, "Export Control User Limit", "Grid", "W", true, "Export Power Control User Limit (Write Reg 0x0042)"),
        ("0x00B7", 183, "EPS Mute", "EPS", "", true, "EPS Alarm Mute: 0:Off, 1:On (Write Reg 0x0043)"),
        ("0x00B8", 184, "EPS Frequency", "EPS", "Hz", true, "EPS Nominal Frequency: 0:50Hz, 1:60Hz (Write Reg 0x0044)"),
        ("0x00B9", 185, "EPS Discharge Voltage", "EPS", "V", true, "EPS Charger 1 Minimum Discharge Voltage (Write Reg 0x0045)"),
        ("0x00BB", 187, "Language", "System Settings", "", true, "Display Language: 0:English, 1:German (Write Reg 0x0047)"),
        ("0x00BC", 188, "IP Method", "System Settings", "", true, "IP Network Method: 0:DHCP, 1:Manual (Write Reg 0x0048)"),
        ("0x00D6", 214, "Charge Absorption Voltage", "Battery Control", "V", true, "Battery Charge Absorption Stage Voltage (Write Reg 0x0053)"),
        // Additional Write-Only or Obscure Registers added for complete coverage
        ("0x0012", 18, "Adjust PV1 Current", "Settings", "", true, "Adjust PV1 Current (Write Reg 0x0012)"),
        ("0x0013", 19, "Adjust PV2 Current", "Settings", "", true, "Adjust PV2 Current (Write Reg 0x0013)"),
        ("0x0014", 20, "Adjust PV1 Volt", "Settings", "", true, "Adjust PV1 Voltage (Write Reg 0x0014)"),
        ("0x0015", 21, "Adjust PV2 Volt", "Settings", "", true, "Adjust PV2 Voltage (Write Reg 0x0015)"),
        ("0x0016", 22, "Adjust AC Current", "Settings", "", true, "Adjust AC Current (Write Reg 0x0016)"),
        ("0x0017", 23, "Adjust AC Volt", "Settings", "", true, "Adjust AC Voltage (Write Reg 0x0017)"),
        ("0x001C", 28, "Remote Switch", "Remote Control", "", true, "1: Start, 0: Stop (Write Reg 0x001C)"),
        ("0x001D", 29, "Inverter Reset E2PROM", "Settings", "", true, "Write 1 to Reset (Write Reg 0x001D)"),
        ("0x001E", 30, "Inverter Clear History", "Settings", "", true, "Write 1 to Clear (Write Reg 0x001E)"),
        ("0x0046", 70, "EPS Charger1 Min Capacity", "EPS", "%", true, "EPS Charger 1 Min Capacity (Write Reg 0x0046)"),
        ("0x0054", 84, "Self Test Start", "Settings", "", true, "Write 1 to Start (Write Reg 0x0054)"),
        ("0x0055", 85, "Clear Overload Fault", "Settings", "", true, "Write 1 to Clear (Write Reg 0x0055)"),
        ("0x0056", 86, "Battery Awaken", "Battery Control", "", true, "Write 1 to Awaken (Write Reg 0x0056)"),
        ("0x005F", 95, "Reset Manager EEPROM", "Settings", "", true, "1: Reset Normal, 2: Reset All (Write Reg 0x005F)"),
        ("0x0064", 100, "Relay 1 Match", "Relay", "", true, "Relay 1 Match (Write Reg 0x0064)"),
        ("0x0065", 101, "Relay 1 Trigger Power", "Relay", "W", true, "Relay 1 Trigger Power (Write Reg 0x0065)"),
        ("0x0074", 116, "Relay 2 Match", "Relay", "", true, "Relay 2 Match (Write Reg 0x0074)"),
        ("0x0075", 117, "Relay 2 Trigger Power", "Relay", "W", true, "Relay 2 Trigger Power (Write Reg 0x0075)"),
        ("0x0084", 132, "Relay 3 Match", "Relay", "", true, "Relay 3 Match (Write Reg 0x0084)"),
        ("0x0085", 133, "Relay 3 Trigger Power", "Relay", "W", true, "Relay 3 Trigger Power (Write Reg 0x0085)"),
        ("0x0090", 144, "Remote Wakeup", "Remote Control", "", true, "Wake up Idle 1:Enable (Write Reg 0x0090)"),
    ];

    let solax_v321: &[(&str, u16, &str, &str, &str, bool, &str)] = &[
        ("0x0000", 0, "Grid Voltage", "Grid", "V", false, "AC Mains Grid Line Voltage"),
        ("0x0000", 0, "Grid Voltage X1", "Grid", "V", false, "AC Mains Grid Line Voltage (Phase 1)"),
        ("0x0001", 1, "Grid Current", "Grid", "A", false, "AC Mains Grid Current"),
        ("0x0001", 1, "Grid Current X1", "Grid", "A", false, "AC Mains Grid Current (Phase 1)"),
        ("0x0002", 2, "Inverter Power", "Inverter", "W", false, "Inverter AC Active Power Output"),
        ("0x0002", 2, "Inverter Power X1", "Inverter", "W", false, "Inverter AC Active Power Output (Phase 1)"),
        ("0x0003", 3, "PV1 Voltage", "Solar PV", "V", false, "PV String 1 DC Input Voltage"),
        ("0x0003", 3, "PV1 Voltage Hybrid", "Solar PV", "V", false, "PV String 1 DC Input Voltage (Hybrid)"),
        ("0x0004", 4, "PV2 Voltage", "Solar PV", "V", false, "PV String 2 DC Input Voltage"),
        ("0x0004", 4, "PV2 Voltage Hybrid", "Solar PV", "V", false, "PV String 2 DC Input Voltage (Hybrid)"),
        ("0x0005", 5, "PV1 Current", "Solar PV", "A", false, "PV String 1 DC Input Current"),
        ("0x0005", 5, "PV1 Current Hybrid", "Solar PV", "A", false, "PV String 1 DC Input Current (Hybrid)"),
        ("0x0006", 6, "PV2 Current", "Solar PV", "A", false, "PV String 2 DC Input Current"),
        ("0x0006", 6, "PV2 Current Hybrid", "Solar PV", "A", false, "PV String 2 DC Input Current (Hybrid)"),
        ("0x0007", 7, "Grid Frequency", "Grid", "Hz", false, "AC Mains Grid Frequency"),
        ("0x0007", 7, "Grid Frequency X1", "Grid", "Hz", false, "AC Mains Grid Frequency (Phase 1)"),
        ("0x0008", 8, "Inner Temp", "Status", "°C", false, "Inverter Internal Operating Temperature"),
        ("0x0009", 9, "Run Mode", "Status", "", false, "Operating Mode: 0: Wait, 1: Check, 2: Normal, 3: Fault, 4: Permanent Fault, 7: EPS"),
        ("0x000A", 10, "PV1 Power", "Solar PV", "W", false, "PV String 1 Active DC Generation"),
        ("0x000B", 11, "PV2 Power", "Solar PV", "W", false, "PV String 2 Active DC Generation"),
        ("0x0014", 20, "Battery Voltage", "Battery", "V", false, "Battery Bank Terminal DC Voltage"),
        ("0x0015", 21, "Battery Current", "Battery", "A", false, "Battery Charge / Discharge Current"),
        ("0x0016", 22, "Battery Power", "Battery", "W", false, "Battery Active Power (Positive = Charge, Negative = Discharge)"),
        ("0x0017", 23, "BMS Connect State", "Battery", "", false, "Battery Management System Connection State"),
        ("0x0018", 24, "Battery Temperature", "Battery", "°C", false, "Internal Battery Module Cell Temperature"),
        ("0x0019", 25, "Charger Boost Temperature", "Battery", "°C", false, "Charger Boost Stage Heatsink Temperature"),
        ("0x001C", 28, "Battery Capacity", "Battery", "%", false, "Battery State of Charge (SOC)"),
        ("0x001D", 29, "Battery Energy Discharged", "Battery", "kWh", false, "Lifetime Total Battery Energy Discharged"),
        ("0x001F", 31, "BMS Warning", "Faults", "", false, "BMS Warning & Status Bitmask"),
        ("0x0020", 32, "Battery Energy Discharged Today", "Battery", "kWh", false, "Daily Battery Discharged Energy"),
        ("0x0021", 33, "Battery Energy Charged", "Battery", "kWh", false, "Lifetime Total Battery Energy Charged"),
        ("0x0023", 35, "Battery Energy Charged Today", "Battery", "kWh", false, "Daily Battery Charged Energy"),
        ("0x0024", 36, "BMS Max Charge Current", "Battery", "A", false, "BMS Maximum Allowed Charge Current Limit"),
        ("0x0025", 37, "BMS Max Discharge Current", "Battery", "A", false, "BMS Maximum Allowed Discharge Current Limit"),
        ("0x0028", 40, "Battery State of Health", "Battery", "%", false, "Battery Module Health Index (SOH)"),
        ("0x0040", 64, "Inverter Fault", "Faults", "", false, "Inverter Fault Bitmask"),
        ("0x0042", 66, "Charger Fault", "Faults", "", false, "Charger Subsystem Fault Bitmask"),
        ("0x0043", 67, "Manager Fault", "Faults", "", false, "Inverter Manager / Communication Fault Bitmask"),
        ("0x0046", 70, "Measured Power", "Grid", "W", false, "External Grid CT / Meter Active Power Measurement"),
        ("0x0048", 72, "Feed In Energy", "Grid", "kWh", false, "Lifetime Total Solar Grid Feed-in Energy"),
        ("0x004A", 74, "Consumed Energy", "Grid", "kWh", false, "Lifetime Total Grid Consumed Energy"),
        ("0x004C", 76, "EPS Voltage", "EPS", "V", false, "Emergency Power Supply Output Voltage"),
        ("0x004D", 77, "EPS Current", "EPS", "A", false, "Emergency Power Supply Output Current"),
        ("0x004E", 78, "EPS VA", "EPS", "VA", false, "Emergency Power Supply Apparent Power"),
        ("0x004F", 79, "EPS Frequency", "EPS", "Hz", false, "Emergency Power Supply Output Frequency"),
        ("0x0050", 80, "Energy Today", "Solar PV", "kWh", false, "Daily Solar PV Generation"),
        ("0x0052", 82, "Energy Total", "Solar PV", "kWh", false, "Lifetime Total Solar PV Generation"),
        ("0x0066", 102, "Bus Voltage", "Status", "V", false, "Internal DC Bus Voltage"),
        ("0x0067", 103, "DC Voltage Fault", "Faults", "V", false, "DC Bus Overvoltage Fault Threshold"),
        ("0x0068", 104, "Overload Fault", "Faults", "", false, "Inverter Overload Fault Status"),
        ("0x0069", 105, "Battery Voltage Fault", "Faults", "", false, "Battery Over/Undervoltage Fault Status"),
        // Write registers (Function Code 0x06)
        ("0x0000", 0, "UnlockPassword", "Settings", "", true, "Installer Password Unlock (Write Reg 0x0000)"),
        ("0x0001", 1, "PV Start Voltage", "Settings", "V", true, "PV Start Voltage Threshold (Write Reg 0x0001)"),
        ("0x0002", 2, "Start Wait Time", "Settings", "s", true, "Start Waiting Time (Write Reg 0x0002)"),
        ("0x0003", 3, "PV High Voltage Cutoff", "Settings", "V", true, "PV High Voltage Cutoff (Write Reg 0x0003)"),
        ("0x0004", 4, "PV Low Voltage Cutoff", "Settings", "V", true, "PV Low Voltage Cutoff (Write Reg 0x0004)"),
        ("0x0005", 5, "Min Grid Voltage Protect", "Settings", "V", true, "Minimum AC Voltage Limit (Write Reg 0x0005)"),
        ("0x0006", 6, "Max Grid Voltage Protect", "Settings", "V", true, "Maximum AC Voltage Limit (Write Reg 0x0006)"),
        ("0x0007", 7, "Min Grid Freq Protect", "Settings", "Hz", true, "Minimum AC Frequency Limit (Write Reg 0x0007)"),
        ("0x0008", 8, "Max Grid Freq Protect", "Settings", "Hz", true, "Maximum AC Frequency Limit (Write Reg 0x0008)"),
        ("0x0009", 9, "Safety Type", "Settings", "", true, "Safety Grid Code Selection (Write Reg 0x0009)"),
        ("0x001C", 28, "Remote Switch", "Remote Control", "", true, "Remote Inverter Switch (Write Reg 0x001C)"),
        ("0x001D", 29, "Inverter Reset E2PROM", "Settings", "", true, "1: Execute EEPROM reset (Write Reg 0x001D)"),
        ("0x001E", 30, "Inverter Clear History", "Settings", "", true, "1: Execute history log clear (Write Reg 0x001E)"),
        ("0x001F", 31, "SolarChargerUseMode", "Settings", "", true, "Operating Mode (Write Reg 0x001F)"),
        ("0x0020", 32, "Battery Min Capacity", "Battery", "%", true, "Battery Reserved Minimum Capacity (Write Reg 0x0020)"),
        ("0x0021", 33, "Battery Type", "Battery", "", true, "Battery Type (Write Reg 0x0021)"),
        ("0x0022", 34, "Charge Float Voltage", "Battery", "V", true, "Charge Float Voltage (Write Reg 0x0022)"),
        ("0x0023", 35, "Discharge Cutoff Voltage", "Battery", "V", true, "Battery Discharge Cutoff Voltage (Write Reg 0x0023)"),
        ("0x0024", 36, "Max Charge Current", "Battery", "A", true, "Battery Maximum Charge Current (Write Reg 0x0024)"),
        ("0x0025", 37, "Max Discharge Current", "Battery", "A", true, "Battery Maximum Discharge Current (Write Reg 0x0025)"),
        ("0x0026", 38, "Charge Window 1 Start", "Battery", "HH:MM", true, "Period 1 Start Time (Write Reg 0x0026)"),
        ("0x0027", 39, "Charge Window 1 End", "Battery", "HH:MM", true, "Period 1 End Time (Write Reg 0x0027)"),
        ("0x002A", 42, "Charge Window 2 Start", "Battery", "HH:MM", true, "Period 2 Start Time (Write Reg 0x002A)"),
        ("0x002B", 43, "Charge Window 2 End", "Battery", "HH:MM", true, "Period 2 End Time (Write Reg 0x002B)"),
        ("0x0040", 64, "Allow Grid Charge", "Settings", "", true, "0: Forbidden, 1: P1, 2: P2, 3: Both (Write Reg 0x0040)"),
        ("0x0041", 65, "Export Control Factory Limit", "Settings", "W", true, "Factory Export Power Limit (Write Reg 0x0041)"),
        ("0x0042", 66, "Export Control User Limit", "Settings", "W", true, "User Export Power Limit (Write Reg 0x0042)"),
        ("0x0043", 67, "EPS Mute", "Settings", "", true, "EPS Mute (Write Reg 0x0043)"),
        ("0x0044", 68, "EPS Frequency", "Settings", "", true, "EPS Frequency (Write Reg 0x0044)"),
        ("0x0051", 81, "ModbusPowerControl", "Remote Control", "", true, "Remote Power Control Enable (Write Reg 0x0051)"),
        ("0x0052", 82, "Modbus ActivePower", "Remote Control", "W", true, "Remote Active Power Target (Write Reg 0x0052)"),
        ("0x0053", 83, "Modbus ReactivePower", "Remote Control", "VAr", true, "Remote Reactive Power Target (Write Reg 0x0053)"),
        ("0x0054", 84, "Self Test start", "Settings", "", true, "Self Test start (Write Reg 0x0054)"),
        ("0x009F", 159, "PowerControl_timeout", "Remote Control", "s", true, "Remote Control Watchdog Timeout (Write Reg 0x009F)"),
    ];

    let solax_g4: &[(&str, u16, &str, &str, &str, bool, &str)] = &[
        ("0x0000", 0, "Grid Voltage", "Grid", "V", false, "AC Mains Grid Line Voltage"),
        ("0x0004", 4, "Grid Frequency", "Grid", "Hz", false, "AC Mains Grid Frequency"),
        ("0x000A", 10, "PV1 Power", "Solar PV", "W", false, "PV String 1 Active DC Generation"),
        ("0x0014", 20, "Battery Voltage", "Battery", "V", false, "Battery Bank Terminal DC Voltage"),
        ("0x0016", 22, "Battery Power", "Battery", "W", false, "Battery Active Power"),
        ("0x001C", 28, "Battery Capacity", "Battery", "%", false, "Battery State of Charge (SOC)"),
        ("0x007C", 124, "Modbus Power Control", "Remote Control", "", true, "Gen 4 Modbus Power Control Enable Register"),
        ("0x007D", 125, "Target Set Type", "Remote Control", "", true, "Gen 4 Target Control Type Register"),
        ("0x007E", 126, "Remote Power Setpoint", "Remote Control", "W", true, "Gen 4 Remote Power Setpoint (32-bit int)"),
        ("0x0088", 136, "Remote Control Timeout", "Remote Control", "s", true, "Gen 4 Remote Control Timeout Register"),
    ];

    let sdm630_map: &[(&str, u16, &str, &str, &str, bool, &str)] = &[
        ("0x0000", 0, "Phase 1 line to neutral volts", "Grid", "V", false, "Line to neutral voltage Phase 1"),
        ("0x0002", 2, "Phase 2 line to neutral volts", "Grid", "V", false, "Line to neutral voltage Phase 2"),
        ("0x0004", 4, "Phase 3 line to neutral volts", "Grid", "V", false, "Line to neutral voltage Phase 3"),
        ("0x0006", 6, "Phase 1 current", "Grid", "A", false, "Line current Phase 1"),
        ("0x0008", 8, "Phase 2 current", "Grid", "A", false, "Line current Phase 2"),
        ("0x000A", 10, "Phase 3 current", "Grid", "A", false, "Line current Phase 3"),
        ("0x000C", 12, "Phase 1 power", "Grid", "W", false, "Active power Phase 1"),
        ("0x000E", 14, "Phase 2 power", "Grid", "W", false, "Active power Phase 2"),
        ("0x0010", 16, "Phase 3 power", "Grid", "W", false, "Active power Phase 3"),
        ("0x0012", 18, "Phase 1 volt amps", "Grid", "VA", false, "Apparent power Phase 1"),
        ("0x0014", 20, "Phase 2 volt amps", "Grid", "VA", false, "Apparent power Phase 2"),
        ("0x0016", 22, "Phase 3 volt amps", "Grid", "VA", false, "Apparent power Phase 3"),
        ("0x0018", 24, "Phase 1 volt amps reactive", "Grid", "VAr", false, "Reactive power Phase 1"),
        ("0x001A", 26, "Phase 2 volt amps reactive", "Grid", "VAr", false, "Reactive power Phase 2"),
        ("0x001C", 28, "Phase 3 volt amps reactive", "Grid", "VAr", false, "Reactive power Phase 3"),
        ("0x001E", 30, "Phase 1 power factor", "Grid", "PF", false, "Power factor Phase 1"),
        ("0x0020", 32, "Phase 2 power factor", "Grid", "PF", false, "Power factor Phase 2"),
        ("0x0022", 34, "Phase 3 power factor", "Grid", "PF", false, "Power factor Phase 3"),
        ("0x0024", 36, "Phase 1 phase angle", "Grid", "°", false, "Phase angle Phase 1"),
        ("0x0026", 38, "Phase 2 phase angle", "Grid", "°", false, "Phase angle Phase 2"),
        ("0x0028", 40, "Phase 3 phase angle", "Grid", "°", false, "Phase angle Phase 3"),
        ("0x002A", 42, "Average line to neutral volts", "Grid", "V", false, "Average line to neutral voltage"),
        ("0x002E", 46, "Average line current", "Grid", "A", false, "Average line current"),
        ("0x0030", 48, "Sum of line currents", "Grid", "A", false, "Sum of line currents"),
        ("0x0034", 52, "Total system power", "Grid", "W", false, "Total active system power"),
        ("0x0038", 56, "Total system volt amps", "Grid", "VA", false, "Total apparent power"),
        ("0x003C", 60, "Total system VAr", "Grid", "VAr", false, "Total reactive power"),
        ("0x003E", 62, "Total system power factor", "Grid", "PF", false, "Total system power factor"),
        ("0x0042", 66, "Frequency of supply voltages", "Grid", "Hz", false, "AC Mains grid supply frequency"),
        ("0x0048", 72, "Total import kWh", "Grid", "kWh", false, "Lifetime total grid imported energy"),
        ("0x004A", 74, "Total export kWh", "Grid", "kWh", false, "Lifetime total grid exported energy"),
        ("0x004C", 76, "Total import kvarh", "Grid", "kVArh", false, "Lifetime total imported reactive energy"),
        ("0x004E", 78, "Total export kvarh", "Grid", "kVArh", false, "Lifetime total exported reactive energy"),
        ("0x0050", 80, "Total VAh", "Grid", "VAh", false, "Lifetime total apparent energy"),
        ("0x0052", 82, "Ah", "Grid", "Ah", false, "Lifetime total Ampere hours"),
        ("0x0054", 84, "Total system power demand", "Grid", "W", false, "Total system active power demand"),
        ("0x0056", 86, "Maximum total system power demand", "Grid", "W", false, "Maximum total system active power demand"),
        ("0x0064", 100, "Total system VA demand", "Grid", "VA", false, "Total system apparent power demand"),
        ("0x0066", 102, "Maximum total system VA demand", "Grid", "VA", false, "Maximum total system apparent power demand"),
        ("0x006E", 110, "Neutral current demand", "Grid", "A", false, "Neutral current demand"),
        ("0x0070", 112, "Maximum neutral current demand", "Grid", "A", false, "Maximum neutral current demand"),
        ("0x00C8", 200, "Phase 1 to Phase 2 volts", "Grid", "V", false, "Line-to-line voltage Phase 1-2"),
        ("0x00CA", 202, "Phase 2 to Phase 3 volts", "Grid", "V", false, "Line-to-line voltage Phase 2-3"),
        ("0x00CC", 204, "Phase 3 to Phase 1 volts", "Grid", "V", false, "Line-to-line voltage Phase 3-1"),
        ("0x00CE", 206, "Average line to line volts", "Grid", "V", false, "Average line-to-line voltage"),
        ("0x00E0", 224, "Neutral current", "Grid", "A", false, "Calculated neutral current"),
        ("0x00EA", 234, "Phase 1 current THD", "Grid", "%", false, "Current Total Harmonic Distortion Phase 1"),
        ("0x00EC", 236, "Phase 2 current THD", "Grid", "%", false, "Current Total Harmonic Distortion Phase 2"),
        ("0x00EE", 238, "Phase 3 current THD", "Grid", "%", false, "Current Total Harmonic Distortion Phase 3"),
        ("0x00F0", 240, "Phase 1 voltage THD", "Grid", "%", false, "Voltage Total Harmonic Distortion Phase 1"),
        ("0x00F2", 242, "Phase 2 voltage THD", "Grid", "%", false, "Voltage Total Harmonic Distortion Phase 2"),
        ("0x00F4", 244, "Phase 3 voltage THD", "Grid", "%", false, "Voltage Total Harmonic Distortion Phase 3"),
        ("0x0156", 342, "Total kWh", "Grid", "kWh", false, "Lifetime total active energy"),
        ("0x0158", 344, "Total kvarh", "Grid", "kVArh", false, "Lifetime total reactive energy"),
    ];

    let dtsu666_map: &[(&str, u16, &str, &str, &str, bool, &str)] = &[
        ("0x2000", 8192, "Phase A Voltage", "Grid", "V", false, "Phase A AC Line Voltage"),
        ("0x2002", 8194, "Phase A Current", "Grid", "A", false, "Phase A AC Current"),
        ("0x2004", 8196, "Active Power", "Grid", "W", false, "Total Active Grid Power"),
        ("0x2006", 8198, "Reactive Power", "Grid", "VAr", false, "Total Reactive Grid Power"),
        ("0x200A", 8202, "Power Factor", "Grid", "PF", false, "System Power Factor"),
        ("0x200E", 8206, "Frequency", "Grid", "Hz", false, "AC Mains Grid Frequency"),
        ("0x4000", 16384, "Import Active Energy", "Grid", "kWh", false, "Total Imported Active Energy"),
        ("0x4002", 16386, "Export Active Energy", "Grid", "kWh", false, "Total Exported Active Energy"),
    ];

    let empty_map: &[(&str, u16, &str, &str, &str, bool, &str)] = &[];

    let driver_type = inv.driver_type.as_deref().unwrap_or("");
    let known_map = match driver_type {
        "Solax-Modbus" => solax_v250,
        "Solax-G3-Modbus" => solax_v321,
        "Solax-G4-Modbus" => solax_g4,
        "SDM630Modbusv2" => sdm630_map,
        "DTSU666" => dtsu666_map,
        _ => empty_map,
    };

    let mut registers = Vec::new();
    let mut mapped_keys = std::collections::HashSet::new();

    for (hex, dec, name, cat, unit, writable, desc) in known_map {
        if mapped_keys.contains(*name) {
            continue;
        }

        if let Some(val) = inv.raw_metrics.get(*name) {
            mapped_keys.insert(name.to_string());
            registers.push(RegisterDetail {
                address_hex: hex.to_string(),
                address_dec: *dec,
                name: name.to_string(),
                value: val.clone(),
                unit: unit.to_string(),
                category: cat.to_string(),
                writable: *writable,
                description: desc.to_string(),
            });
        } else if *name == "Battery Capacity" {
            mapped_keys.insert(name.to_string());
            registers.push(RegisterDetail {
                address_hex: hex.to_string(),
                address_dec: *dec,
                name: name.to_string(),
                value: format!("{}", inv.battery_capacity),
                unit: unit.to_string(),
                category: cat.to_string(),
                writable: *writable,
                description: desc.to_string(),
            });
        } else if *name == "Battery Power" {
            mapped_keys.insert(name.to_string());
            registers.push(RegisterDetail {
                address_hex: hex.to_string(),
                address_dec: *dec,
                name: name.to_string(),
                value: format!("{}", inv.battery_power),
                unit: unit.to_string(),
                category: cat.to_string(),
                writable: *writable,
                description: desc.to_string(),
            });
        } else if *name == "PV1 Power" {
            mapped_keys.insert(name.to_string());
            registers.push(RegisterDetail {
                address_hex: hex.to_string(),
                address_dec: *dec,
                name: name.to_string(),
                value: format!("{}", inv.pv_power),
                unit: unit.to_string(),
                category: cat.to_string(),
                writable: *writable,
                description: desc.to_string(),
            });
        } else if *name == "Run Mode" {
            mapped_keys.insert(name.to_string());
            registers.push(RegisterDetail {
                address_hex: hex.to_string(),
                address_dec: *dec,
                name: name.to_string(),
                value: format!("{}", inv.run_mode),
                unit: unit.to_string(),
                category: cat.to_string(),
                writable: *writable,
                description: desc.to_string(),
            });
        } else if *writable {
            mapped_keys.insert(name.to_string());
            registers.push(RegisterDetail {
                address_hex: hex.to_string(),
                address_dec: *dec,
                name: name.to_string(),
                value: "".to_string(),
                unit: unit.to_string(),
                category: cat.to_string(),
                writable: *writable,
                description: desc.to_string(),
            });
        }
    }

    let mut unmapped: Vec<(&String, &String)> = inv.raw_metrics.iter()
        .filter(|(k, _)| !mapped_keys.contains(*k))
        .collect();
    unmapped.sort_by(|a, b| a.0.cmp(b.0));

    for (key, val) in unmapped {
        let (hex_label, desc) = if key == "Power Budget" || key == "Usage" || key == "Requested Battery Power" {
            ("Calculated", "Derived software calculation (no single hardware Modbus register)".to_string())
        } else {
            ("Derived", "Software / driver telemetry metric".to_string())
        };

        registers.push(RegisterDetail {
            address_hex: hex_label.to_string(),
            address_dec: 0,
            name: key.clone(),
            value: val.clone(),
            unit: "".to_string(),
            category: "Telemetry Metrics".to_string(),
            writable: false,
            description: desc,
        });
    }

    registers
}

pub async fn handle_get_inverter_registers(
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Result<Json<InverterRegistersResponse>, (axum::http::StatusCode, String)> {
    let name = params.get("name").ok_or_else(|| {
        (
            axum::http::StatusCode::BAD_REQUEST,
            "Missing 'name' query parameter".to_string(),
        )
    })?;

    let status = get_system_status_lock().clone();
    
    if let Some(inv) = status.inverters.get(name) {
        let regs = build_inverter_register_details(name, inv);
        let driver_type = inv.driver_type.clone().unwrap_or_else(|| "Unknown".to_string());
        return Ok(Json(InverterRegistersResponse {
            name: name.clone(),
            driver_type,
            last_updated: inv.last_updated,
            is_error: inv.is_error,
            error_text: inv.error_text.clone(),
            registers: regs,
        }));
    }

    // Try meters map (direct match or stripped /dev/tty match or case-insensitive)
    let clean_name = name.replace("/dev/tty", "").replace("/dev/", "");
    let meter_entry = status.meters.get(name)
        .or_else(|| status.meters.get(&clean_name))
        .or_else(|| status.meters.iter().find(|(k, _)| k.eq_ignore_ascii_case(name) || k.eq_ignore_ascii_case(&clean_name)).map(|(_, v)| v));

    if let Some(meter) = meter_entry {
        let dummy_inv = InverterStatus {
            raw_metrics: meter.raw_metrics.clone(),
            driver_type: meter.driver_type.clone(),
            last_updated: meter.last_updated,
            is_error: meter.is_error,
            error_text: meter.error_text.clone(),
            ..Default::default()
        };
        let regs = build_inverter_register_details(name, &dummy_inv);
        let driver_type = meter.driver_type.clone().unwrap_or_else(|| "Meter".to_string());
        return Ok(Json(InverterRegistersResponse {
            name: name.clone(),
            driver_type,
            last_updated: meter.last_updated,
            is_error: meter.is_error,
            error_text: meter.error_text.clone(),
            registers: regs,
        }));
    }

    Err((
        axum::http::StatusCode::NOT_FOUND,
        format!("Device / meter '{}' not found", name),
    ))
}

pub async fn handle_write_inverter_register(
    Json(payload): Json<WriteRegisterRequest>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let reg = if let Some(r) = payload.register {
        r
    } else if let Some(ref hex_str) = payload.register_hex {
        let clean = hex_str.trim_start_matches("0x").trim_start_matches("0X");
        u16::from_str_radix(clean, 16).map_err(|e| {
            (
                axum::http::StatusCode::BAD_REQUEST,
                format!("Invalid hex register address '{}': {}", hex_str, e),
            )
        })?
    } else {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "Must specify 'register' (decimal u16) or 'register_hex' ('0x001F')".to_string(),
        ));
    };

    enqueue_modbus_write(&payload.inverter, reg, payload.value);

    let topic = format!("sensors/power_manager/write_register/{}", payload.inverter);
    let msg_payload = serde_json::json!({
        "register": reg,
        "value": payload.value,
    })
    .to_string();
    crate::mqtt_helper::publish_mqtt_message(&topic, &msg_payload);

    println!(
        "[API] Enqueued Modbus register write to inverter '{}': Reg {:#06x} ({}) = {}",
        payload.inverter, reg, reg, payload.value
    );

    Ok(Json(serde_json::json!({
        "status": "success",
        "inverter": payload.inverter,
        "register": reg,
        "register_hex": format!("{:#06x}", reg),
        "value": payload.value,
        "message": format!("Modbus write command queued for inverter '{}' (Reg {:#06x} = {})", payload.inverter, reg, payload.value)
    })))
}

pub async fn get_health_status(db_path: &str) -> (axum::http::StatusCode, Json<serde_json::Value>) {
    let config = Config::load_from_db(db_path).unwrap_or_else(|_| Config::default_empty());
    let mut target_inverters = config.get_configured_battery_inverters();

    let status = get_system_status_lock().clone();
    if target_inverters.is_empty() {
        target_inverters = status.inverters.keys().cloned().collect();
    }
    target_inverters.sort();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut inverters_info = Vec::new();
    let mut all_healthy = true;
    let mut healthy_count = 0;

    for inv_name in &target_inverters {
        let inv_status = status.inverters.get(inv_name);
        let last_updated = inv_status.and_then(|s| s.last_updated);
        let seconds_ago = last_updated.map(|ts| now.saturating_sub(ts));

        let is_healthy = match seconds_ago {
            Some(sec) => sec <= 60,
            None => false,
        };

        if is_healthy {
            healthy_count += 1;
        } else {
            all_healthy = false;
        }

        inverters_info.push(serde_json::json!({
            "name": inv_name,
            "healthy": is_healthy,
            "last_updated": last_updated,
            "last_updated_seconds_ago": seconds_ago,
        }));
    }

    let status_str = if all_healthy { "healthy" } else { "unhealthy" };
    let http_status = if all_healthy {
        axum::http::StatusCode::OK
    } else {
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    };

    (
        http_status,
        Json(serde_json::json!({
            "status": status_str,
            "timestamp": now,
            "inverters_count": target_inverters.len(),
            "healthy_count": healthy_count,
            "inverters": inverters_info,
        })),
    )
}

#[derive(serde::Serialize, Clone)]
#[serde(tag = "type")]
pub enum SimProgressUpdate {
    Progress { percent: f64, eta_seconds: f64 },
    Result { response: crate::power_manager::SimulationResponse },
    Error { message: String },
}

#[derive(serde::Deserialize)]
struct SimQuery {
    range: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct BackupTelemetryRecord {
    pub timestamp: i64,
    pub topic: String,
    pub value: f64,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct BackupDump {
    pub version: String,
    pub exported_at: String,
    pub config: Config,
    pub telemetry_history: Vec<BackupTelemetryRecord>,
}

fn export_backup_dump(db_path: &str) -> Result<BackupDump, (axum::http::StatusCode, String)> {
    let cfg = Config::load_from_db(db_path)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to load config: {}", e)))?;

    let _ = crate::database::init_history_db(db_path);

    let history_rows = crate::database::get_all_telemetry_since(db_path, 0)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to query telemetry history: {}", e)))?;

    let telemetry_history = history_rows
        .into_iter()
        .map(|(ts, topic, val)| BackupTelemetryRecord {
            timestamp: ts,
            topic,
            value: val,
        })
        .collect();

    Ok(BackupDump {
        version: env!("CARGO_PKG_VERSION").to_string(),
        exported_at: chrono::Utc::now().to_rfc3339(),
        config: cfg,
        telemetry_history,
    })
}

fn export_telemetry_csv(db_path: &str) -> Result<([(header::HeaderName, String); 2], Vec<u8>), (axum::http::StatusCode, String)> {
    let _ = crate::database::init_history_db(db_path);

    let cfg = Config::load_from_db(db_path).unwrap_or_else(|_| Config::default_empty());
    let tz_offset = crate::power_manager::get_timezone_offset(cfg.battery_control.as_ref().and_then(|bc| bc.timezone.as_deref()));

    let history_rows = crate::database::get_all_telemetry_since(db_path, 0)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to query telemetry history: {}", e)))?;

    let mut topics_set = std::collections::BTreeSet::new();
    let mut time_map: std::collections::BTreeMap<i64, std::collections::HashMap<String, f64>> = std::collections::BTreeMap::new();

    for (ts, topic, val) in history_rows {
        topics_set.insert(topic.clone());
        time_map.entry(ts).or_default().insert(topic, val);
    }

    let topics: Vec<String> = topics_set.into_iter().collect();

    let mut wtr = csv::WriterBuilder::new().from_writer(Vec::new());

    let mut header_row = vec!["timestamp".to_string(), "datetime".to_string()];
    header_row.extend(topics.clone());

    if let Err(e) = wtr.write_record(&header_row) {
        return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("CSV write header error: {}", e)));
    }

    for (ts, vals) in time_map {
        let dt_str = match chrono::DateTime::from_timestamp(ts, 0) {
            Some(dt) => dt.with_timezone(&tz_offset).to_rfc3339(),
            None => "".to_string(),
        };

        let mut row = Vec::with_capacity(2 + topics.len());
        row.push(ts.to_string());
        row.push(dt_str);

        for topic in &topics {
            if let Some(val) = vals.get(topic) {
                row.push(val.to_string());
            } else {
                row.push("".to_string());
            }
        }

        if let Err(e) = wtr.write_record(&row) {
            return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("CSV write record error: {}", e)));
        }
    }

    let csv_bytes = wtr.into_inner()
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("CSV flush error: {}", e)))?;

    let now_str = chrono::Utc::now().with_timezone(&tz_offset).format("%Y%m%d_%H%M%S").to_string();
    let filename = format!("powerscraper_telemetry_{}.csv", now_str);

    let headers = [
        (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
        (
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", filename),
        ),
    ];

    Ok((headers, csv_bytes))
}

pub fn build_web_app(reload_tx: Sender<()>, db_path: String) -> Router {
    let db_path_clone = db_path.clone();
    let db_path_backup_export = db_path.clone();
    let db_path_backup_dl = db_path.clone();
    let db_path_telemetry_csv = db_path.clone();
    let db_path_telemetry_csv2 = db_path.clone();
    let db_path_backup_import = db_path.clone();
    let db_path_history_api = db_path.clone();
    let db_path_sim = db_path.clone();
    let db_path_train = db_path.clone();
    let db_path_apply = db_path.clone();
    let db_path_infer = db_path.clone();
    let db_path_debug = db_path.clone();
    let db_path_debug2 = db_path.clone();
    let reload_tx_apply = reload_tx.clone();
    let state = Arc::new(reload_tx);

    Router::new()
        .route("/", get(serve_dashboard))
        .route("/style.css", get(serve_style))
        .route("/app.js", get(serve_js))
        .route("/api/inverter/registers", get(handle_get_inverter_registers))
        .route("/api/inverter/write_register", post(handle_write_inverter_register))
        .route(
            "/api/mqtt/test",
            post(handle_mqtt_test),
        )
        .route(
            "/api/simulation/run",
            get(move |axum::extract::Query(query): axum::extract::Query<SimQuery>| {
                let path = db_path_sim.clone();
                async move {
                    let range_str = query.range.as_deref().unwrap_or("1m").to_string();
                    let (tx, rx) = tokio::sync::mpsc::channel(100);

                    tokio::task::spawn_blocking(move || {
                        let progress_cb = |percent: f64, eta_seconds: f64| {
                            let _ = tx.blocking_send(SimProgressUpdate::Progress { percent, eta_seconds });
                        };
                        match crate::power_manager::run_historical_simulation_impl(&path, &range_str, Some(&progress_cb)) {
                            Ok(res) => {
                                let _ = tx.blocking_send(SimProgressUpdate::Result { response: res });
                            }
                            Err(e) => {
                                let _ = tx.blocking_send(SimProgressUpdate::Error { message: e });
                            }
                        }
                    });

                    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
                        match rx.recv().await {
                            Some(item) => {
                                match axum::response::sse::Event::default().json_data(item) {
                                    Ok(ev) => Some((Ok::<axum::response::sse::Event, std::convert::Infallible>(ev), rx)),
                                    Err(_) => None,
                                }
                            }
                            None => None,
                        }
                    });

                    axum::response::Sse::new(stream)
                        .keep_alive(axum::response::sse::KeepAlive::default())
                }
            }),
        )
        .route(
            "/api/config/import",
            post(|body: String| async move {
                match toml::from_str::<Config>(&body) {
                    Ok(cfg) => Ok(Json(cfg)),
                    Err(e) => Err((axum::http::StatusCode::BAD_REQUEST, e.to_string())),
                }
            }),
        )
        .route(
            "/api/config",
            get(move || {
                let path = db_path_clone.clone();
                async move {
                    match Config::load_from_db(&path) {
                        Ok(cfg) => Ok(Json(cfg)),
                        Err(e) => {
                            Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
                        }
                    }
                }
            }),
        )
        .route(
            "/api/config",
            post({
                let reload_channel = state.clone();
                move |Json(new_cfg): Json<Config>| {
                    let path = db_path.clone();
                    let reload_channel = reload_channel.clone();
                    async move {
                        if let Err(e) = new_cfg.save_to_db_async(&path).await {
                            return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
                        }
                        // Signal live reload to daemon tasks
                        let _ = reload_channel.send(()).await;
                        Ok(Json(serde_json::json!({ "status": "success" })))
                    }
                }
            }),
        )
        .route(
            "/api/backup/export",
            get({
                let db_path = db_path_backup_export.clone();
                move || {
                    let path = db_path.clone();
                    async move {
                        export_backup_dump(&path).map(Json)
                    }
                }
            }),
        )
        .route(
            "/api/backup/download",
            get({
                let db_path = db_path_backup_dl.clone();
                move || {
                    let path = db_path.clone();
                    async move {
                        match export_backup_dump(&path) {
                            Ok(dump) => {
                                let now_str = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
                                let filename = format!("powerscraper_backup_{}.json.xz", now_str);
                                let json_bytes = match serde_json::to_vec_pretty(&dump) {
                                    Ok(b) => b,
                                    Err(e) => return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
                                };
                                let mut compressed_bytes = Vec::new();
                                if let Err(e) = lzma_rs::xz_compress(&mut std::io::Cursor::new(json_bytes), &mut compressed_bytes) {
                                    return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("XZ compression failed: {}", e)));
                                }
                                let headers = [
                                    (header::CONTENT_TYPE, "application/x-xz".to_string()),
                                    (
                                        header::CONTENT_DISPOSITION,
                                        format!("attachment; filename=\"{}\"", filename),
                                    ),
                                ];
                                Ok((headers, compressed_bytes))
                            }
                            Err(e) => Err(e),
                        }
                    }
                }
            }),
        )
        .route(
            "/api/telemetry/export.csv",
            get({
                let db_path = db_path_telemetry_csv.clone();
                move || {
                    let path = db_path.clone();
                    async move {
                        export_telemetry_csv(&path)
                    }
                }
            }),
        )
        .route(
            "/api/backup/telemetry.csv",
            get({
                let db_path = db_path_telemetry_csv2.clone();
                move || {
                    let path = db_path.clone();
                    async move {
                        export_telemetry_csv(&path)
                    }
                }
            }),
        )
        .route(
            "/api/backup/import",
            post({
                let db_path = db_path_backup_import.clone();
                let reload_channel = state.clone();
                move |body_bytes: axum::body::Bytes| {
                    let path = db_path.clone();
                    let reload_channel = reload_channel.clone();
                    async move {
                        let mut decompressed_bytes = Vec::new();
                        let data_slice: &[u8] = if lzma_rs::xz_decompress(&mut std::io::Cursor::new(&body_bytes), &mut decompressed_bytes).is_ok() {
                            &decompressed_bytes
                        } else {
                            &body_bytes
                        };

                        let (cfg, records) = match serde_json::from_slice::<BackupDump>(data_slice) {
                            Ok(dump) => {
                                let recs: Vec<(i64, String, f64)> = dump
                                    .telemetry_history
                                    .into_iter()
                                    .map(|r| (r.timestamp, r.topic, r.value))
                                    .collect();
                                (dump.config, recs)
                            }
                            Err(_) => {
                                let cfg = serde_json::from_slice::<Config>(data_slice)
                                    .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, format!("Failed to parse backup JSON: {}", e)))?;
                                (cfg, Vec::new())
                            }
                        };

                        if let Err(e) = cfg.save_to_db_async(&path).await {
                            return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to save config: {}", e)));
                        }

                        let count = records.len();
                        if count > 0 {
                            if let Err(e) = crate::database::insert_telemetry_history_batch(&path, &records) {
                                return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to import telemetry history: {}", e)));
                            }
                        }

                        let _ = reload_channel.send(()).await;

                        Ok(Json(serde_json::json!({
                            "status": "success",
                            "imported_telemetry_records": count
                        })))
                    }
                }
            }),
        )
        .route(
            "/api/history",
            get({
                let db_path = db_path_history_api.clone();
                move |axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>| {
                    let path = db_path.clone();
                    async move {
                        let now = chrono::Utc::now().timestamp();
                        let start_ts = params.get("start")
                            .and_then(|s| s.parse::<i64>().ok())
                            .unwrap_or(now - 86400);
                        let end_ts = params.get("end")
                            .and_then(|s| s.parse::<i64>().ok())
                            .unwrap_or(now);

                        let max_pixels = params.get("max_pixels")
                            .or_else(|| params.get("pixels"))
                            .or_else(|| params.get("width"))
                            .and_then(|s| s.parse::<usize>().ok())
                            .unwrap_or(1200);

                        let topics_opt: Option<Vec<String>> = params.get("topics")
                            .map(|s| {
                                let mut result = String::with_capacity(s.len());
                                let bytes = s.as_bytes();
                                let mut i = 0;
                                while i < bytes.len() {
                                    if bytes[i] == b'%' && i + 2 < bytes.len() {
                                        if let Ok(val) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                                            result.push(val as char);
                                            i += 3;
                                            continue;
                                        }
                                    }
                                    if bytes[i] == b'+' {
                                        result.push(' ');
                                    } else {
                                        result.push(bytes[i] as char);
                                    }
                                    i += 1;
                                }
                                result.split(',')
                                    .map(|t| t.trim().to_string())
                                    .filter(|t| !t.is_empty())
                                    .collect()
                            });

                        let req_start = std::time::Instant::now();
                        let res = tokio::task::spawn_blocking(move || {
                            let t_flush = std::time::Instant::now();
                            let retention_days = crate::config::Config::load_from_db(&path)
                                .ok()
                                .and_then(|c| c.history)
                                .and_then(|h| h.retention_days);
                            let flushed_count = crate::database::flush_pending_history_to_db(&path, None);
                            let flush_dur = t_flush.elapsed().as_secs_f64() * 1000.0;

                            let (records, raw_count, topic_dur, query_decimate_dur) =
                                crate::database::get_decimated_telemetry_in_range_profiled(&path, start_ts, end_ts, max_pixels, topics_opt.as_deref())
                                .map_err(|e| e.to_string())?;

                            Ok::<_, String>((flushed_count, flush_dur, records, raw_count, topic_dur, query_decimate_dur))
                        }).await;

                        match res {
                            Ok(Ok((flushed_count, flush_dur, records, raw_count, topic_dur, query_decimate_dur))) => {
                                let t_fmt = std::time::Instant::now();
                                #[derive(serde::Serialize)]
                                struct TelemetryRecordRef<'a> {
                                    timestamp: i64,
                                    topic: &'a str,
                                    value: f64,
                                }
                                let refs: Vec<TelemetryRecordRef> = records
                                    .iter()
                                    .map(|(ts, topic, val)| TelemetryRecordRef {
                                        timestamp: *ts,
                                        topic: topic.as_str(),
                                        value: *val,
                                    })
                                    .collect();
                                let json_body = serde_json::to_string(&refs).unwrap_or_else(|_| "[]".to_string());
                                let fmt_dur = t_fmt.elapsed().as_secs_f64() * 1000.0;
                                let total_dur = req_start.elapsed().as_secs_f64() * 1000.0;

                                println!(
                                    "[Profile /api/history] Flush: {:.2}ms ({} flushed), Topic Lookup: {:.2}ms, DB Query & Decimate: {:.2}ms ({} rows -> {} points), JSON Format: {:.2}ms, Total Server: {:.2}ms, Payload: {} bytes",
                                    flush_dur, flushed_count, topic_dur, query_decimate_dur, raw_count, refs.len(), fmt_dur, total_dur, json_body.len()
                                );

                                let server_timing = format!(
                                    "flush;dur={:.2};desc=\"DB Flush\", topic_lookup;dur={:.2};desc=\"Topic Lookup\", db_query;dur={:.2};desc=\"DB Query & Decimate\", format;dur={:.2};desc=\"JSON Format\", total;dur={:.2};desc=\"Total Server\"",
                                    flush_dur, topic_dur, query_decimate_dur, fmt_dur, total_dur
                                );

                                Ok((
                                    [
                                        (axum::http::header::HeaderName::from_static("server-timing"), server_timing),
                                        (axum::http::header::CONTENT_TYPE, "application/json".to_string()),
                                    ],
                                    json_body,
                                ))
                            }
                            Ok(Err(e)) => Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Database query error: {}", e))),
                            Err(e) => Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Blocking task error: {}", e))),
                        }
                    }
                }
            }),
        )
        .route(
            "/api/status",
            get(|| async {
                let mut status = get_system_status_lock().clone();
                let now_secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                for inv in status.inverters.values_mut() {
                    evaluate_inverter_errors(inv, now_secs);
                }
                Json(status)
            }),
        )
        .route(
            "/api/health",
            get({
                let db_path = db_path_debug.clone();
                move || {
                    let path = db_path.clone();
                    async move { get_health_status(&path).await }
                }
            }),
        )
        .route(
            "/health",
            get({
                let db_path = db_path_debug.clone();
                move || {
                    let path = db_path.clone();
                    async move { get_health_status(&path).await }
                }
            }),
        )
        .route(
            "/api/debug",
            get({
                let db_path = db_path_debug.clone();
                move || {
                    let path = db_path.clone();
                    async move {
                        let config = Config::load_from_db(&path).unwrap_or_else(|_| Config::default_empty());
                        let status = get_system_status_lock().clone();

                        let mut data_sources = Vec::new();
                        if let Some(ref wifi) = config.solax_wifi {
                            for host in &wifi.inverters {
                                data_sources.push(serde_json::json!({
                                    "name": host,
                                    "type": "solax_wifi",
                                    "status": status.inverters.get(host),
                                }));
                            }
                        }
                        if let Some(ref mb) = config.solax_modbus {
                            for inv in &mb.inverters {
                                data_sources.push(serde_json::json!({
                                    "name": inv,
                                    "type": "solax_modbus",
                                    "status": status.inverters.get(inv),
                                }));
                            }
                        }
                        if let Some(ref g4) = config.solax_g4_modbus {
                            for inv in &g4.inverters {
                                data_sources.push(serde_json::json!({
                                    "name": inv,
                                    "type": "solax_g4_modbus",
                                    "status": status.inverters.get(inv),
                                }));
                            }
                        }
                        if let Some(ref g3) = config.solax_g3_modbus {
                            for inv in &g3.inverters {
                                data_sources.push(serde_json::json!({
                                    "name": inv,
                                    "type": "solax_g3_modbus",
                                    "status": status.inverters.get(inv),
                                }));
                            }
                        }
                        if let Some(ref sdm) = config.sdm630_modbus_v2 {
                            for port in &sdm.ports {
                                data_sources.push(serde_json::json!({
                                    "name": port,
                                    "type": "sdm630_modbus_v2",
                                    "meter_power": status.meter_power,
                                    "meter_last_updated": status.meter_last_updated,
                                }));
                            }
                        }
                        if let Some(ref dtsu) = config.dtsu666 {
                            for port in &dtsu.ports {
                                data_sources.push(serde_json::json!({
                                    "name": port,
                                    "type": "dtsu666",
                                    "meter_power": status.meter_power,
                                    "meter_last_updated": status.meter_last_updated,
                                }));
                            }
                        }
                        if let Some(ref meter) = config.mqtt_power_meter {
                            for name in &meter.meters {
                                data_sources.push(serde_json::json!({
                                    "name": name,
                                    "type": "mqtt_power_meter",
                                    "meter_power": status.meter_power,
                                    "meter_last_updated": status.meter_last_updated,
                                }));
                            }
                        }
                        if let Some(ref inv) = config.mqtt_inverter {
                            for name in &inv.inverters {
                                data_sources.push(serde_json::json!({
                                    "name": name,
                                    "type": "mqtt_inverter",
                                    "status": status.inverters.get(name),
                                }));
                            }
                        }

                        let debug_payload = serde_json::json!({
                            "timestamp": chrono::Utc::now().to_rfc3339(),
                            "timestamp_epoch": chrono::Utc::now().timestamp(),
                            "config": config,
                            "system_status": status,
                            "data_sources": data_sources,
                            "power_manager_internal_state": {
                                "active_mode": status.active_mode,
                                "grid_target": status.grid_target,
                                "inverters": status.inverters,
                                "meter_power": status.meter_power,
                                "meter_last_updated": status.meter_last_updated,
                                "import_price": status.import_price,
                                "export_price": status.export_price,
                                "price_thresholds": status.price_thresholds,
                                "usage": status.usage,
                                "power_budget": status.power_budget,
                                "power_budget_with_charging": status.power_budget_with_charging,
                            }
                        });

                        Json(debug_payload)
                    }
                }
            }),
        )
        .route(
            "/debug",
            get({
                let db_path = db_path_debug2.clone();
                move || {
                    let path = db_path.clone();
                    async move {
                        let config = Config::load_from_db(&path).unwrap_or_else(|_| Config::default_empty());
                        let status = get_system_status_lock().clone();

                        let mut data_sources = Vec::new();
                        if let Some(ref wifi) = config.solax_wifi {
                            for host in &wifi.inverters {
                                data_sources.push(serde_json::json!({
                                    "name": host,
                                    "type": "solax_wifi",
                                    "status": status.inverters.get(host),
                                }));
                            }
                        }
                        if let Some(ref mb) = config.solax_modbus {
                            for inv in &mb.inverters {
                                data_sources.push(serde_json::json!({
                                    "name": inv,
                                    "type": "solax_modbus",
                                    "status": status.inverters.get(inv),
                                }));
                            }
                        }
                        if let Some(ref g4) = config.solax_g4_modbus {
                            for inv in &g4.inverters {
                                data_sources.push(serde_json::json!({
                                    "name": inv,
                                    "type": "solax_g4_modbus",
                                    "status": status.inverters.get(inv),
                                }));
                            }
                        }
                        if let Some(ref g3) = config.solax_g3_modbus {
                            for inv in &g3.inverters {
                                data_sources.push(serde_json::json!({
                                    "name": inv,
                                    "type": "solax_g3_modbus",
                                    "status": status.inverters.get(inv),
                                }));
                            }
                        }
                        if let Some(ref sdm) = config.sdm630_modbus_v2 {
                            for port in &sdm.ports {
                                data_sources.push(serde_json::json!({
                                    "name": port,
                                    "type": "sdm630_modbus_v2",
                                    "meter_power": status.meter_power,
                                    "meter_last_updated": status.meter_last_updated,
                                }));
                            }
                        }
                        if let Some(ref dtsu) = config.dtsu666 {
                            for port in &dtsu.ports {
                                data_sources.push(serde_json::json!({
                                    "name": port,
                                    "type": "dtsu666",
                                    "meter_power": status.meter_power,
                                    "meter_last_updated": status.meter_last_updated,
                                }));
                            }
                        }
                        if let Some(ref meter) = config.mqtt_power_meter {
                            for name in &meter.meters {
                                data_sources.push(serde_json::json!({
                                    "name": name,
                                    "type": "mqtt_power_meter",
                                    "meter_power": status.meter_power,
                                    "meter_last_updated": status.meter_last_updated,
                                }));
                            }
                        }
                        if let Some(ref inv) = config.mqtt_inverter {
                            for name in &inv.inverters {
                                data_sources.push(serde_json::json!({
                                    "name": name,
                                    "type": "mqtt_inverter",
                                    "status": status.inverters.get(name),
                                }));
                            }
                        }

                        let debug_payload = serde_json::json!({
                            "timestamp": chrono::Utc::now().to_rfc3339(),
                            "timestamp_epoch": chrono::Utc::now().timestamp(),
                            "config": config,
                            "system_status": status,
                            "data_sources": data_sources,
                            "power_manager_internal_state": {
                                "active_mode": status.active_mode,
                                "grid_target": status.grid_target,
                                "inverters": status.inverters,
                                "meter_power": status.meter_power,
                                "meter_last_updated": status.meter_last_updated,
                                "import_price": status.import_price,
                                "export_price": status.export_price,
                                "price_thresholds": status.price_thresholds,
                                "usage": status.usage,
                                "power_budget": status.power_budget,
                                "power_budget_with_charging": status.power_budget_with_charging,
                            }
                        });

                        Json(debug_payload)
                    }
                }
            }),
        )
        .route(
            "/api/location/infer-orientation",
            post({
                let db_path_clone = db_path_infer.clone();
                move |body: Json<crate::orientation_inference::InferRequest>| async move {
                    crate::orientation_inference::handle_infer_orientation(db_path_clone, body).await
                }
            }),
        )
        .route(
            "/api/train/start",
            post({
                let db_path = db_path_train.clone();
                move |Json(req): Json<StartTrainingRequest>| {
                    let db_path = db_path.clone();
                    async move {
                        handle_start_training(db_path, req).await
                    }
                }
            })
        )
        .route(
            "/api/train/cancel",
            post(handle_cancel_training)
        )
        .route(
            "/api/train/apply",
            post({
                let db_path = db_path_apply.clone();
                let reload_tx = reload_tx_apply.clone();
                move |Json(params): Json<ApplyParamsRequest>| {
                    let db_path = db_path.clone();
                    let reload_tx = reload_tx.clone();
                    async move {
                        handle_apply_training(reload_tx, db_path, params).await
                    }
                }
            })
        )
        .route(
            "/api/train/status",
            get(|| async {
                let lock = get_tuning_progress().lock().unwrap();
                Json(lock.clone())
            })
        )
        .route(
            "/api/train/progress",
            get(handle_training_progress)
        )
        .layer(CorsLayer::permissive())
}

pub async fn run_web_server_with_listener(
    reload_tx: Sender<()>,
    db_path: String,
    listener: tokio::net::TcpListener,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    let app = build_web_app(reload_tx, db_path);
    let _ = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            cancel_token.cancelled().await;
        })
        .await;
}

#[derive(serde::Serialize, Clone, Debug, Default)]
pub struct TuningProgress {
    pub is_running: bool,
    pub last_generation: u32,
    pub total_generations: u32,
    pub percent: f64,
    pub best_cost: f64,
    pub bill: f64,
    pub cycles: f64,
    pub logs: Vec<String>,
    pub best_params: Option<crate::config::EvolvedHeuristicConfig>,
    pub best_params_monthly: Option<HashMap<String, crate::config::EvolvedHeuristicConfig>>,
    pub error: Option<String>,
}

#[derive(serde::Serialize, Clone, Debug)]
pub struct TuningLogEvent {
    pub percent: f64,
    #[serde(rename = "gen")]
    pub gen_num: u32,
    pub total_gens: u32,
    pub best_cost: f64,
    pub bill: f64,
    pub cycles: f64,
    pub log_line: String,
    pub done: bool,
    pub error: Option<String>,
    pub best_params: Option<crate::config::EvolvedHeuristicConfig>,
    pub best_params_monthly: Option<HashMap<String, crate::config::EvolvedHeuristicConfig>>,
}

pub static TUNING_PROGRESS: OnceLock<Mutex<TuningProgress>> = OnceLock::new();
pub static ACTIVE_CHILD: OnceLock<Mutex<Option<std::process::Child>>> = OnceLock::new();
pub static TUNING_CHANNEL: OnceLock<tokio::sync::broadcast::Sender<TuningLogEvent>> = OnceLock::new();

pub fn get_tuning_progress() -> &'static Mutex<TuningProgress> {
    TUNING_PROGRESS.get_or_init(|| Mutex::new(TuningProgress::default()))
}

pub fn get_active_child() -> &'static Mutex<Option<std::process::Child>> {
    ACTIVE_CHILD.get_or_init(|| Mutex::new(None))
}

pub fn get_tuning_channel() -> &'static tokio::sync::broadcast::Sender<TuningLogEvent> {
    TUNING_CHANNEL.get_or_init(|| {
        let (tx, _) = tokio::sync::broadcast::channel(1024);
        tx
    })
}

#[derive(serde::Deserialize)]
pub struct StartTrainingRequest {
    pub seed: bool,
    pub generations: u32,
    pub population_size: u32,
    pub cycle_penalty: f64,
    pub cores: Option<u32>,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
pub enum ApplyParamsRequest {
    Single(crate::config::EvolvedHeuristicConfig),
    Monthly(HashMap<String, crate::config::EvolvedHeuristicConfig>),
}

pub static CANCEL_TUNING: AtomicBool = AtomicBool::new(false);

pub async fn handle_start_training(
    db_path: String,
    req: StartTrainingRequest,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let mut progress = get_tuning_progress().lock().unwrap();
    if progress.is_running {
        return Err((axum::http::StatusCode::CONFLICT, "Training is already running".to_string()));
    }

    // Parse seed if requested
    let mut seed_config = None;
    let mut seed_config_monthly = None;
    if req.seed {
        if let Ok(cfg) = Config::load_from_db(&db_path) {
            if let Some(bc) = cfg.battery_control {
                seed_config = bc.evolved_heuristic.clone();
                seed_config_monthly = bc.evolved_heuristic_monthly.clone();
            }
        }
    }

    // Reset cancellation flag
    CANCEL_TUNING.store(false, Ordering::Relaxed);

    let db_path_clone = db_path.clone();
    let req_generations = req.generations;
    let req_population_size = req.population_size;
    let req_cycle_penalty = req.cycle_penalty;

    // Reset progress state
    *progress = TuningProgress {
        is_running: true,
        total_generations: req_generations,
        ..Default::default()
    };

    let tx = get_tuning_channel().clone();

    // Spawn tuning thread
    tokio::task::spawn_blocking(move || {
        let (records, sim_config) = match crate::power_manager::load_sim_records(&db_path_clone, "all") {
            Ok(res) => res,
            Err(e) => {
                let mut progress = get_tuning_progress().lock().unwrap();
                progress.is_running = false;
                progress.error = Some(e.clone());
                let _ = tx.send(TuningLogEvent {
                    percent: 0.0,
                    gen_num: 0,
                    total_gens: req_generations,
                    best_cost: 0.0,
                    bill: 0.0,
                    cycles: 0.0,
                    log_line: format!("Error loading data: {}", e),
                    done: true,
                    error: Some(e),
                    best_params: None,
                    best_params_monthly: None,
                });
                return;
            }
        };

        let progress_tx = tx.clone();
        let progress_cb = move |event: crate::simulation::tuning::TuningProgressEvent| -> bool {
            if CANCEL_TUNING.load(Ordering::Relaxed) {
                return false;
            }

            let log_event = TuningLogEvent {
                percent: event.percent,
                gen_num: event.gen_num,
                total_gens: event.total_gens,
                best_cost: event.best_cost,
                bill: event.bill,
                cycles: event.cycles,
                log_line: event.log_line.clone(),
                done: event.done,
                error: None,
                best_params: event.best_params.clone(),
                best_params_monthly: event.best_params_monthly.clone(),
            };

            if let Ok(mut prog) = get_tuning_progress().lock() {
                prog.last_generation = event.gen_num;
                prog.percent = event.percent;
                prog.best_cost = event.best_cost;
                prog.bill = event.bill;
                prog.cycles = event.cycles;
                prog.logs.push(event.log_line);
                if event.done {
                    prog.is_running = false;
                    prog.best_params = event.best_params.clone();
                    prog.best_params_monthly = event.best_params_monthly.clone();
                }
            }

            let _ = progress_tx.send(log_event);
            true
        };

        match crate::simulation::tuning::run_tuning(
            &records,
            &sim_config,
            req_generations,
            req_population_size,
            req_cycle_penalty,
            seed_config,
            seed_config_monthly,
            Some(&progress_cb),
        ) {
            Ok(best_params_monthly) => {
                if CANCEL_TUNING.load(Ordering::Relaxed) {
                    return;
                }
                
                let mut progress = get_tuning_progress().lock().unwrap();
                progress.is_running = false;
                progress.best_params_monthly = Some(best_params_monthly.clone());

                let _ = tx.send(TuningLogEvent {
                    percent: 100.0,
                    gen_num: progress.last_generation,
                    total_gens: progress.total_generations,
                    best_cost: progress.best_cost,
                    bill: progress.bill,
                    cycles: progress.cycles,
                    log_line: "Tuning successfully completed in Rust for all months.".to_string(),
                    done: true,
                    error: None,
                    best_params: None,
                    best_params_monthly: Some(best_params_monthly),
                });
            }
            Err(e) => {
                let mut progress = get_tuning_progress().lock().unwrap();
                progress.is_running = false;
                if e == "Tuning cancelled by user" {
                    progress.error = Some("Cancelled by user".to_string());
                } else {
                    progress.error = Some(e.clone());
                }

                let _ = tx.send(TuningLogEvent {
                    percent: progress.percent,
                    gen_num: progress.last_generation,
                    total_gens: progress.total_generations,
                    best_cost: progress.best_cost,
                    bill: progress.bill,
                    cycles: progress.cycles,
                    log_line: format!("Error during tuning: {}", e),
                    done: true,
                    error: Some(e),
                    best_params: None,
                    best_params_monthly: None,
                });
            }
        }
    });

    Ok(Json(serde_json::json!({ "status": "started" })))
}

pub async fn handle_cancel_training() -> impl IntoResponse {
    CANCEL_TUNING.store(true, Ordering::Relaxed);

    let mut prog = get_tuning_progress().lock().unwrap();
    prog.is_running = false;
    prog.error = Some("Training cancelled by user".to_string());

    let _ = get_tuning_channel().send(TuningLogEvent {
        percent: prog.percent,
        gen_num: prog.last_generation,
        total_gens: prog.total_generations,
        best_cost: prog.best_cost,
        bill: prog.bill,
        cycles: prog.cycles,
        log_line: "Training cancelled by user.".to_string(),
        done: true,
        error: Some("Cancelled by user".to_string()),
        best_params: None,
        best_params_monthly: None,
    });

    Json(serde_json::json!({ "status": "cancelled" }))
}

pub async fn handle_apply_training(
    reload_tx: Sender<()>,
    db_path: String,
    params: ApplyParamsRequest,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let mut cfg = match Config::load_from_db(&db_path) {
        Ok(c) => c,
        Err(e) => return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to load config: {}", e))),
    };

    if let Some(ref mut bc) = cfg.battery_control {
        match params {
            ApplyParamsRequest::Single(single) => {
                bc.evolved_heuristic = Some(single);
            }
            ApplyParamsRequest::Monthly(monthly) => {
                bc.evolved_heuristic_monthly = Some(monthly);
            }
        }
    } else {
        return Err((axum::http::StatusCode::BAD_REQUEST, "Battery control config not initialized in database settings.".to_string()));
    }

    if let Err(e) = cfg.save_to_db_async(&db_path).await {
        return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to save config: {}", e)));
    }

    let _ = reload_tx.send(()).await;
    Ok(Json(serde_json::json!({ "status": "applied" })))
}

pub async fn handle_training_progress() -> impl IntoResponse {
    let rx = get_tuning_channel().subscribe();
    
    let logs = {
        let prog = get_tuning_progress().lock().unwrap();
        prog.logs.clone()
    };

    let stream = futures_util::stream::unfold((rx, logs, 0), |(mut rx, logs, mut sent_logs_idx)| async move {
        if sent_logs_idx < logs.len() {
            let log_line = logs[sent_logs_idx].clone();
            let event = TuningLogEvent {
                percent: 0.0,
                gen_num: 0,
                total_gens: 0,
                best_cost: 0.0,
                bill: 0.0,
                cycles: 0.0,
                log_line,
                done: false,
                error: None,
                best_params: None,
                best_params_monthly: None,
            };
            sent_logs_idx += 1;
            return Some((Ok::<axum::response::sse::Event, std::convert::Infallible>(
                axum::response::sse::Event::default().json_data(event).unwrap()
            ), (rx, logs, sent_logs_idx)));
        }

        match rx.recv().await {
            Ok(item) => {
                let ev = axum::response::sse::Event::default().json_data(item).unwrap();
                Some((Ok(ev), (rx, logs, sent_logs_idx)))
            }
            Err(_) => None,
        }
    });

    axum::response::Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
}

pub async fn run_web_server(
    reload_tx: Sender<()>,
    db_path: String,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    let mut retries = 0;
    let listener = loop {
        if cancel_token.is_cancelled() {
            return;
        }
        match tokio::net::TcpListener::bind("0.0.0.0:3000").await {
            Ok(l) => break l,
            Err(e) => {
                retries += 1;
                eprintln!("[Web Server] Failed to bind 0.0.0.0:3000 (attempt {}): {}. Retrying in 1s...", retries, e);
                tokio::select! {
                    _ = cancel_token.cancelled() => return,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                }
            }
        }
    };
    println!("Web UI and REST API running at http://localhost:3000");
    run_web_server_with_listener(reload_tx, db_path, listener, cancel_token).await;
}

async fn serve_dashboard() -> Html<&'static str> {
    Html(crate::web_assets::INDEX_HTML)
}

async fn serve_style() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css")],
        crate::web_assets::STYLE_CSS,
    )
}

async fn serve_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/javascript")],
        crate::web_assets::APP_JS,
    )
}

#[derive(serde::Serialize)]
struct MqttTestResponse {
    connected: bool,
    error: Option<String>,
    wildcard_subscription: bool,
    base_pub_sub: bool,
    ha_discovery: Option<bool>,
}

async fn handle_mqtt_test(
    Json(config): Json<crate::config::MqttBrokerConfig>,
) -> impl IntoResponse {
    let client_id = format!(
        "powerscraper-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );

    let (client, mut eventloop) = crate::mqtt_helper::create_mqtt_client(&client_id, &config);

    let base_topic = config.base_topic.clone().unwrap_or_else(|| "sensors".to_string());
    let wildcard_topic = format!("{}/#", base_topic);
    let pubsub_topic = format!("{}/test/pubsub", base_topic);
    let wildcard_test_topic = format!("{}/test/wildcard-only", base_topic);
    
    let ha_enabled = config.is_ha_discovery_enabled();
    let ha_prefix = config.ha_discovery_prefix();
    let ha_test_topic = format!("{}/sensor/test/config", ha_prefix);

    let wildcard_token = format!("token-wildcard-{}", client_id);
    let pubsub_token = format!("token-pubsub-{}", client_id);
    let ha_token = format!("token-ha-{}", client_id);

    // Attempt subscriptions
    if let Err(e) = client.subscribe(&wildcard_topic, rumqttc::QoS::AtLeastOnce).await {
        return Json(MqttTestResponse {
            connected: false,
            error: Some(format!("Failed to initiate wildcard subscription: {}", e)),
            wildcard_subscription: false,
            base_pub_sub: false,
            ha_discovery: if ha_enabled { Some(false) } else { None },
        });
    }

    if let Err(e) = client.subscribe(&pubsub_topic, rumqttc::QoS::AtLeastOnce).await {
        return Json(MqttTestResponse {
            connected: false,
            error: Some(format!("Failed to initiate pubsub subscription: {}", e)),
            wildcard_subscription: false,
            base_pub_sub: false,
            ha_discovery: if ha_enabled { Some(false) } else { None },
        });
    }

    if ha_enabled {
        if let Err(e) = client.subscribe(&ha_test_topic, rumqttc::QoS::AtLeastOnce).await {
            return Json(MqttTestResponse {
                connected: false,
                error: Some(format!("Failed to initiate HA discovery subscription: {}", e)),
                wildcard_subscription: false,
                base_pub_sub: false,
                ha_discovery: Some(false),
            });
        }
    }

    // Attempt publications
    if let Err(e) = client.publish(&wildcard_test_topic, rumqttc::QoS::AtLeastOnce, false, wildcard_token.clone()).await {
        return Json(MqttTestResponse {
            connected: false,
            error: Some(format!("Failed to publish to wildcard test topic: {}", e)),
            wildcard_subscription: false,
            base_pub_sub: false,
            ha_discovery: if ha_enabled { Some(false) } else { None },
        });
    }

    if let Err(e) = client.publish(&pubsub_topic, rumqttc::QoS::AtLeastOnce, false, pubsub_token.clone()).await {
        return Json(MqttTestResponse {
            connected: false,
            error: Some(format!("Failed to publish to pubsub topic: {}", e)),
            wildcard_subscription: false,
            base_pub_sub: false,
            ha_discovery: if ha_enabled { Some(false) } else { None },
        });
    }

    if ha_enabled {
        if let Err(e) = client.publish(&ha_test_topic, rumqttc::QoS::AtLeastOnce, false, ha_token.clone()).await {
            return Json(MqttTestResponse {
                connected: false,
                error: Some(format!("Failed to publish to HA discovery topic: {}", e)),
                wildcard_subscription: false,
                base_pub_sub: false,
                ha_discovery: Some(false),
            });
        }
    }

    let mut connected = false;
    let mut wildcard_subscription = false;
    let mut base_pub_sub = false;
    let mut ha_discovery = if ha_enabled { Some(false) } else { None };
    let mut error = None;

    let start_time = std::time::Instant::now();
    let timeout_duration = std::time::Duration::from_secs(3);

    while start_time.elapsed() < timeout_duration {
        let done = wildcard_subscription && base_pub_sub && (!ha_enabled || ha_discovery == Some(true));
        if done {
            break;
        }

        let remaining = timeout_duration.saturating_sub(start_time.elapsed());
        if remaining.is_zero() {
            break;
        }

        match tokio::time::timeout(remaining, eventloop.poll()).await {
            Ok(Ok(notification)) => {
                connected = true;
                match notification {
                    rumqttc::Event::Incoming(rumqttc::Packet::Publish(p)) => {
                        let payload = String::from_utf8_lossy(&p.payload);
                        if p.topic == wildcard_test_topic && payload == wildcard_token {
                            wildcard_subscription = true;
                        } else if p.topic == pubsub_topic && payload == pubsub_token {
                            base_pub_sub = true;
                        } else if p.topic == ha_test_topic && payload == ha_token {
                            ha_discovery = Some(true);
                        }
                    }
                    rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(connack)) => {
                        if connack.code != rumqttc::ConnectReturnCode::Success {
                            error = Some(format!("Connection refused: {:?}", connack.code));
                            break;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Err(e)) => {
                error = Some(format!("Connection/Protocol error: {}", e));
                break;
            }
            Err(_) => {
                // Timeout
                break;
            }
        }
    }

    let _ = client.disconnect().await;

    // If we didn't receive packets but did not get a specific connection/protocol error,
    // we can formulate an informative message.
    if connected && error.is_none() {
        let mut missing = Vec::new();
        if !wildcard_subscription {
            missing.push(format!("wildcard sub ({})", wildcard_topic));
        }
        if !base_pub_sub {
            missing.push(format!("base pub/sub ({})", pubsub_topic));
        }
        if ha_enabled && ha_discovery != Some(true) {
            missing.push(format!("HA discovery pub/sub ({})", ha_test_topic));
        }
        if !missing.is_empty() {
            error = Some(format!("Permission denied or message delivery failed for: {}", missing.join(", ")));
        }
    } else if !connected && error.is_none() {
        error = Some("Connection timed out".to_string());
    }

    Json(MqttTestResponse {
        connected,
        error,
        wildcard_subscription,
        base_pub_sub,
        ha_discovery,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt; // for oneshot/call

    #[tokio::test]
    async fn test_web_server_routes_and_errors() {
        let (reload_tx, _) = tokio::sync::mpsc::channel(10);
        let temp_db = "temp_test_web_server.db";
        let _ = std::fs::remove_file(temp_db);

        crate::config::Config::default_empty().save_to_db(temp_db).unwrap();

        let app = build_web_app(reload_tx, temp_db.to_string());

        // 1. Test GET / (Dashboard)
        let response = app.clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // 2. Test GET /nonexistent (404)
        let response = app.clone()
            .oneshot(Request::builder().uri("/nonexistent").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // 3. Test POST /api/config/import with invalid TOML syntax (400)
        let response = app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/config/import")
                    .header("content-type", "text/plain")
                    .body(Body::from("invalid_toml_value = =="))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // 4. Test POST /api/config with malformed JSON body (422 or 400)
        let response = app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/config")
                    .header("content-type", "application/json")
                    .body(Body::from("{invalid json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.status() == StatusCode::UNPROCESSABLE_ENTITY || response.status() == StatusCode::BAD_REQUEST);

        // 5. Test POST /api/config with unsupported content type (415)
        let response = app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/config")
                    .header("content-type", "text/plain")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

        // 6. Test GET /api/backup/export
        let response = app.clone()
            .oneshot(Request::builder().uri("/api/backup/export").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // 7. Test GET /api/backup/download
        let response = app.clone()
            .oneshot(Request::builder().uri("/api/backup/download").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key("content-disposition"));

        // 8. Test POST /api/backup/import (uncompressed JSON and compressed XZ)
        let dump_json = serde_json::json!({
            "version": "1.0.84",
            "exported_at": "2026-08-01T00:00:00Z",
            "config": crate::config::Config::default_empty(),
            "telemetry_history": [
                { "timestamp": 1000, "topic": "test/topic", "value": 42.0 }
            ]
        });
        let raw_json_bytes = dump_json.to_string().into_bytes();

        // 8a. Test uncompressed JSON import
        let response = app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/backup/import")
                    .header("content-type", "application/octet-stream")
                    .body(Body::from(raw_json_bytes.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // 8b. Test XZ compressed backup import
        let mut xz_bytes = Vec::new();
        lzma_rs::xz_compress(&mut std::io::Cursor::new(raw_json_bytes), &mut xz_bytes).unwrap();
        let response = app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/backup/import")
                    .header("content-type", "application/octet-stream")
                    .body(Body::from(xz_bytes))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // 9. Test GET /api/telemetry/export.csv
        let response = app.clone()
            .oneshot(Request::builder().uri("/api/telemetry/export.csv").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get("content-type").unwrap(), "text/csv; charset=utf-8");
        assert!(response.headers().contains_key("content-disposition"));

        // 10. Test GET /api/debug and /debug
        let response = app.clone()
            .oneshot(Request::builder().uri("/api/debug").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // 11. Test GET /api/health and /health
        {
            get_system_status().lock().unwrap().inverters.clear();
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
            let cfg = Config::load_from_db(temp_db).unwrap_or_else(|_| Config::default_empty());
            let target_invs = cfg.get_configured_battery_inverters();
            let target_invs = if target_invs.is_empty() { vec!["solax-x1".to_string()] } else { target_invs };
            for inv in &target_invs {
                get_system_status().lock().unwrap().inverters.insert(inv.clone(), InverterStatus {
                    last_updated: Some(now - 5),
                    ..Default::default()
                });
            }

            let response = app.clone()
                .oneshot(Request::builder().uri("/api/health").body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            let response = app.clone()
                .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            // Test stale inverter (>60s ago)
            get_system_status().lock().unwrap().inverters.insert("solax-x1".to_string(), InverterStatus {
                last_updated: Some(now - 120),
                ..Default::default()
            });

            let response = app.clone()
                .oneshot(Request::builder().uri("/api/health").body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        }

        let _ = std::fs::remove_file(temp_db);
    }
}


