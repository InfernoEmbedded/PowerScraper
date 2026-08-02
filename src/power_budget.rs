

/// Calculates Power Budget: the amount of power that can be drawn by the household
/// without drawing power from the grid or batteries (discharging).
pub fn calculate_power_budget(
    total_solar_production: f64,
    total_consumption: f64,
) -> f64 {
    (total_solar_production - total_consumption).max(0.0)
}

/// Calculates Power Budget with Charging: the amount of power that can be drawn by the household
/// without reducing the amount of power drawn by the batteries for charging.
pub fn calculate_power_budget_with_charging(
    total_solar_production: f64,
    total_charging: f64,
    total_consumption: f64,
) -> f64 {
    (total_solar_production - total_charging - total_consumption).max(0.0)
}
