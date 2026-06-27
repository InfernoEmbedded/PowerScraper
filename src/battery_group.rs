use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub struct Battery {
    pub name: String,
    pub capacity_wh: f64,
    pub current_soc_pct: f64,
    pub min_soc_pct: f64,
    pub max_soc_pct: f64,
    pub max_charge_power_w: f64,
    pub max_discharge_power_w: f64,
}

#[derive(Debug, Clone)]
pub struct BatteryGroup {
    pub batteries: Vec<Battery>,
}

impl BatteryGroup {
    pub fn new(batteries: Vec<Battery>) -> Self {
        Self { batteries }
    }

    /// Apportions the target power among the batteries in the group.
    /// target_power_w > 0: Discharging.
    /// target_power_w < 0: Charging.
    /// Returns a map of battery names to allocated power in Watts (positive for discharging, negative for charging).
    pub fn apportion_power(&self, target_power_w: f64) -> HashMap<String, f64> {
        let mut allocation = HashMap::new();
        for b in &self.batteries {
            allocation.insert(b.name.clone(), 0.0);
        }

        if target_power_w.abs() < 1e-5 {
            return allocation;
        }

        if target_power_w > 0.0 {
            // Discharging (target_power_w > 0)
            let mut remaining = target_power_w;
            
            // Map batteries to their available discharge capacity in Wh
            // Filter out batteries that cannot discharge (avail_cap <= 0 or max_discharge_power <= 0)
            let mut active: Vec<(&Battery, f64)> = self.batteries.iter()
                .map(|b| {
                    let avail_cap = b.capacity_wh * ((b.current_soc_pct - b.min_soc_pct) / 100.0).max(0.0);
                    (b, avail_cap)
                })
                .filter(|(b, avail_cap)| *avail_cap > 1e-5 && b.max_discharge_power_w > 1e-5)
                .collect();

            let mut iterations = 0;
            while remaining > 1e-5 && !active.is_empty() && iterations < 100 {
                iterations += 1;
                let total_cap: f64 = active.iter().map(|(_, cap)| *cap).sum();
                if total_cap < 1e-5 {
                    break;
                }

                let current_step_remaining = remaining;
                let mut next_active = Vec::new();
                let mut progress = false;
                let mut step_allocations = Vec::new();

                for &(b, avail_cap) in &active {
                    let share = current_step_remaining * (avail_cap / total_cap);
                    let current_alloc = allocation.get(&b.name).copied().unwrap_or(0.0);
                    let space = b.max_discharge_power_w - current_alloc;

                    if share >= space {
                        // Hits limit
                        step_allocations.push((b.name.clone(), b.max_discharge_power_w));
                        remaining -= space;
                        if space > 1e-5 {
                            progress = true;
                        }
                    } else {
                        // Doesn't hit limit
                        step_allocations.push((b.name.clone(), current_alloc + share));
                        remaining -= share;
                        next_active.push((b, avail_cap));
                        if share > 1e-5 {
                            progress = true;
                        }
                    }
                }

                for (name, power) in step_allocations {
                    allocation.insert(name, power);
                }

                if !progress {
                    break;
                }
                active = next_active;
            }
        } else {
            // Charging (target_power_w < 0)
            let mut remaining = -target_power_w;

            // Map batteries to their available charge capacity in Wh
            // Filter out batteries that cannot charge (avail_cap <= 0 or max_charge_power <= 0)
            let mut active: Vec<(&Battery, f64)> = self.batteries.iter()
                .map(|b| {
                    let avail_cap = b.capacity_wh * ((b.max_soc_pct - b.current_soc_pct) / 100.0).max(0.0);
                    (b, avail_cap)
                })
                .filter(|(b, avail_cap)| *avail_cap > 1e-5 && b.max_charge_power_w > 1e-5)
                .collect();

            let mut iterations = 0;
            while remaining > 1e-5 && !active.is_empty() && iterations < 100 {
                iterations += 1;
                let total_cap: f64 = active.iter().map(|(_, cap)| *cap).sum();
                if total_cap < 1e-5 {
                    break;
                }

                let current_step_remaining = remaining;
                let mut next_active = Vec::new();
                let mut progress = false;
                let mut step_allocations = Vec::new();

                for &(b, avail_cap) in &active {
                    let share = current_step_remaining * (avail_cap / total_cap);
                    let current_alloc = -allocation.get(&b.name).copied().unwrap_or(0.0); // positive value
                    let space = b.max_charge_power_w - current_alloc;

                    if share >= space {
                        // Hits limit
                        step_allocations.push((b.name.clone(), -b.max_charge_power_w));
                        remaining -= space;
                        if space > 1e-5 {
                            progress = true;
                        }
                    } else {
                        // Doesn't hit limit
                        step_allocations.push((b.name.clone(), -(current_alloc + share)));
                        remaining -= share;
                        next_active.push((b, avail_cap));
                        if share > 1e-5 {
                            progress = true;
                        }
                    }
                }

                for (name, power) in step_allocations {
                    allocation.insert(name, power);
                }

                if !progress {
                    break;
                }
                active = next_active;
            }
        }

        allocation
    }

    /// Simulates a time step of charging or discharging for all batteries in the group.
    /// allocations: Map of battery names to power in Watts.
    /// duration_hours: Duration of the step in hours.
    /// efficiency: Round-trip/charging/discharging efficiency (e.g. 0.95).
    pub fn step(&mut self, allocations: &HashMap<String, f64>, duration_hours: f64, efficiency: f64) {
        let eff = if efficiency <= 0.0 { 1.0 } else { efficiency };
        for b in &mut self.batteries {
            if let Some(&power_w) = allocations.get(&b.name) {
                if power_w.abs() < 1e-5 {
                    continue;
                }
                if power_w < 0.0 {
                    // Charging
                    let charge_power = -power_w;
                    let added_energy_wh = charge_power * duration_hours * eff;
                    let new_soc_pct = b.current_soc_pct + (added_energy_wh / b.capacity_wh) * 100.0;
                    b.current_soc_pct = new_soc_pct.clamp(0.0, 100.0);
                } else {
                    // Discharging
                    let discharged_energy_wh = (power_w * duration_hours) / eff;
                    let new_soc_pct = b.current_soc_pct - (discharged_energy_wh / b.capacity_wh) * 100.0;
                    b.current_soc_pct = new_soc_pct.clamp(0.0, 100.0);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to generate a typical battery
    fn make_test_battery(name: &str, soc: f64, capacity_wh: f64) -> Battery {
        Battery {
            name: name.to_string(),
            capacity_wh,
            current_soc_pct: soc,
            min_soc_pct: 10.0,
            max_soc_pct: 95.0,
            max_charge_power_w: 2000.0,
            max_discharge_power_w: 2000.0,
        }
    }

    #[test]
    fn test_zero_power_allocation() {
        let group = BatteryGroup::new(vec![
            make_test_battery("A", 50.0, 10000.0),
            make_test_battery("B", 60.0, 10000.0),
        ]);
        let alloc = group.apportion_power(0.0);
        assert_eq!(alloc.len(), 2);
        assert_eq!(*alloc.get("A").unwrap(), 0.0);
        assert_eq!(*alloc.get("B").unwrap(), 0.0);
    }

    #[test]
    fn test_proportional_discharging() {
        // Battery A has: 10000 Wh * (50% - 10%) = 4000 Wh available
        // Battery B has: 10000 Wh * (90% - 10%) = 8000 Wh available
        // Total available capacity = 12000 Wh. Proportions are A: 1/3, B: 2/3.
        let group = BatteryGroup::new(vec![
            make_test_battery("A", 50.0, 10000.0),
            make_test_battery("B", 90.0, 10000.0),
        ]);

        let alloc = group.apportion_power(1500.0);
        assert_eq!(*alloc.get("A").unwrap(), 500.0);
        assert_eq!(*alloc.get("B").unwrap(), 1000.0);
    }

    #[test]
    fn test_discharge_clamping_and_redistribution() {
        // Battery A has: 10000 Wh * (50% - 10%) = 4000 Wh available (max discharge = 600W)
        // Battery B has: 10000 Wh * (90% - 10%) = 8000 Wh available (max discharge = 2000W)
        // Proportions are A: 1/3, B: 2/3.
        // Target: 1500W.
        // Initial share A = 500W, B = 1000W.
        // Since A is capped at 600W, it does not exceed. Let's make target 2100W:
        // Share A = 700W (exceeds its limit of 600W). A is clamped to 600W.
        // The remaining 1500W goes to B. Since 1500W is less than B's max limit of 2000W, B gets 1500W.
        let mut bat_a = make_test_battery("A", 50.0, 10000.0);
        bat_a.max_discharge_power_w = 600.0;
        let mut bat_b = make_test_battery("B", 90.0, 10000.0);
        bat_b.max_discharge_power_w = 2000.0;

        let group = BatteryGroup::new(vec![bat_a, bat_b]);
        let alloc = group.apportion_power(2100.0);
        assert_eq!(*alloc.get("A").unwrap(), 600.0);
        assert_eq!(*alloc.get("B").unwrap(), 1500.0);
    }

    #[test]
    fn test_discharge_with_no_capacity() {
        // Battery A is at min SOC (10%), so 0 Wh available.
        // Battery B is at 50% SOC, so 4000 Wh available.
        // Target: 1000W.
        // B should take the full 1000W, A should get 0W.
        let group = BatteryGroup::new(vec![
            make_test_battery("A", 10.0, 10000.0),
            make_test_battery("B", 50.0, 10000.0),
        ]);
        let alloc = group.apportion_power(1000.0);
        assert_eq!(*alloc.get("A").unwrap(), 0.0);
        assert_eq!(*alloc.get("B").unwrap(), 1000.0);
    }

    #[test]
    fn test_max_group_discharge_exceeded() {
        // Battery A limit is 500W, Battery B limit is 500W.
        // Target: 1500W.
        // Both should clamp to 500W.
        let mut bat_a = make_test_battery("A", 50.0, 10000.0);
        bat_a.max_discharge_power_w = 500.0;
        let mut bat_b = make_test_battery("B", 50.0, 10000.0);
        bat_b.max_discharge_power_w = 500.0;

        let group = BatteryGroup::new(vec![bat_a, bat_b]);
        let alloc = group.apportion_power(1500.0);
        assert_eq!(*alloc.get("A").unwrap(), 500.0);
        assert_eq!(*alloc.get("B").unwrap(), 500.0);
    }

    #[test]
    fn test_proportional_charging() {
        // Battery A has: 10000 Wh * (95% - 85%) = 1000 Wh charge capacity remaining
        // Battery B has: 10000 Wh * (95% - 45%) = 5000 Wh charge capacity remaining
        // Total available charge capacity = 6000 Wh. Proportions A: 1/6, B: 5/6.
        // Target: -1200W (charge 1200W).
        // Allocation: A should get -200W, B should get -1000W.
        let group = BatteryGroup::new(vec![
            make_test_battery("A", 85.0, 10000.0),
            make_test_battery("B", 45.0, 10000.0),
        ]);
        let alloc = group.apportion_power(-1200.0);
        assert_eq!(*alloc.get("A").unwrap(), -200.0);
        assert_eq!(*alloc.get("B").unwrap(), -1000.0);
    }

    #[test]
    fn test_charge_clamping_and_redistribution() {
        // Battery A capacity remaining: 1000 Wh (max charge = 100W)
        // Battery B capacity remaining: 5000 Wh (max charge = 2000W)
        // Proportions A: 1/6, B: 5/6.
        // Target: -1200W.
        // Initial share: A = -200W (exceeds limit 100W). A clamped to -100W.
        // Overage (1100W) goes to B. B gets -1100W.
        let mut bat_a = make_test_battery("A", 85.0, 10000.0);
        bat_a.max_charge_power_w = 100.0;
        let mut bat_b = make_test_battery("B", 45.0, 10000.0);
        bat_b.max_charge_power_w = 2000.0;

        let group = BatteryGroup::new(vec![bat_a, bat_b]);
        let alloc = group.apportion_power(-1200.0);
        assert_eq!(*alloc.get("A").unwrap(), -100.0);
        assert_eq!(*alloc.get("B").unwrap(), -1100.0);
    }

    #[test]
    fn test_charge_with_no_capacity() {
        // Battery A is at max SOC (95%), so 0 Wh charge capacity.
        // Battery B is at 50% SOC, so 4500 Wh charge capacity.
        // Target: -1000W.
        // B should get -1000W, A should get 0W.
        let group = BatteryGroup::new(vec![
            make_test_battery("A", 95.0, 10000.0),
            make_test_battery("B", 50.0, 10000.0),
        ]);
        let alloc = group.apportion_power(-1000.0);
        assert_eq!(*alloc.get("A").unwrap(), 0.0);
        assert_eq!(*alloc.get("B").unwrap(), -1000.0);
    }

    #[test]
    fn test_max_group_charge_exceeded() {
        // Target: -3000W.
        // Limit: A = 1000W, B = 1000W.
        // Both should clamp to -1000W.
        let mut bat_a = make_test_battery("A", 50.0, 10000.0);
        bat_a.max_charge_power_w = 1000.0;
        let mut bat_b = make_test_battery("B", 50.0, 10000.0);
        bat_b.max_charge_power_w = 1000.0;

        let group = BatteryGroup::new(vec![bat_a, bat_b]);
        let alloc = group.apportion_power(-3000.0);
        assert_eq!(*alloc.get("A").unwrap(), -1000.0);
        assert_eq!(*alloc.get("B").unwrap(), -1000.0);
    }

    #[test]
    fn test_simulation_step() {
        let mut group = BatteryGroup::new(vec![
            make_test_battery("A", 50.0, 10000.0),
            make_test_battery("B", 50.0, 10000.0),
        ]);

        let mut allocations = HashMap::new();
        // A charges at 1000W for 1 hour. efficiency 0.95.
        // energy added = 1000W * 1h * 0.95 = 950 Wh.
        // added SOC = (950 / 10000) * 100 = 9.5%. New SOC = 59.5%.
        allocations.insert("A".to_string(), -1000.0);

        // B discharges at 1000W for 1 hour. efficiency 0.95.
        // energy removed = (1000W * 1h) / 0.95 = 1052.63 Wh.
        // removed SOC = (1052.63 / 10000) * 100 = 10.5263%. New SOC = 39.4737%.
        allocations.insert("B".to_string(), 1000.0);

        group.step(&allocations, 1.0, 0.95);

        assert!((group.batteries[0].current_soc_pct - 59.5).abs() < 1e-4);
        assert!((group.batteries[1].current_soc_pct - 39.4737).abs() < 1e-4);
    }

    #[test]
    fn test_step_efficiency_boundary() {
        let mut group = BatteryGroup::new(vec![
            make_test_battery("A", 50.0, 10000.0),
        ]);
        let mut allocations = HashMap::new();
        allocations.insert("A".to_string(), -1000.0);

        // Efficiency 0.0 or negative should default to 1.0
        group.step(&allocations, 1.0, -0.5);
        assert_eq!(group.batteries[0].current_soc_pct, 60.0);
    }
}
