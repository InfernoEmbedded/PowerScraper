#![allow(
    clippy::collapsible_if,
    clippy::redundant_closure,
    clippy::neg_multiply,
    clippy::io_other_error,
    non_snake_case
)]

pub mod config;
pub mod csv_importer;
pub mod mqtt_helper;
pub mod drivers {
    pub mod mqtt_inverter;
    pub mod mqtt_meter;
    pub mod sdm630;
    pub mod dtsu666;
    pub mod solax_modbus;
    pub mod solax_xhybrid;
    pub mod solax_wifi;
}
pub mod forwarders;
pub mod power_manager;
pub mod simulation;
pub mod tariff_manager;
pub mod web_assets;
pub mod web_server;
pub mod battery_group;
pub mod orientation_inference;
pub mod power_budget;
