#![allow(
    clippy::collapsible_if,
    clippy::redundant_closure,
    clippy::neg_multiply,
    clippy::io_other_error,
    non_snake_case
)]

pub mod config;
pub mod csv_importer;
pub mod database;
pub mod mqtt_helper;
pub mod drivers {
    pub mod dtsu666;
    pub mod mqtt_inverter;
    pub mod mqtt_meter;
    pub mod sdm630;
    pub mod solax_g3;
    pub mod solax_g4;
    pub mod solax_modbus;
    pub mod solax_wifi;
}
pub mod battery_group;
pub mod dispatch_manager;
pub mod forwarders;
pub mod orientation_inference;
pub mod power_budget;
pub mod power_manager;
pub mod simulation;
pub mod tariff_manager;
pub mod web_assets;
pub mod web_server;
