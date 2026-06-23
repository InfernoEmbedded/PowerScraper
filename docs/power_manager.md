# Battery Control Power Manager Guide

The Power Manager in [src/power_manager.rs](file:///home/deece/src/PowerScraper/src/power_manager.rs) coordinates battery charging and discharging rates dynamically. It regulates grid interactions based on real-time consumption telemetry, time-of-use constraints, and physical battery limits.

---

## 1. Core Regulation Loop (Auto Mode)

In **Auto Mode**, the Power Manager regulates battery power outputs to keep grid interactive power matching the configured target (`grid_target` in Watts, defaulting to `0.0`).

### Target Error Calculation
For each active inverter, the manager calculates the error relative to the grid target:
- **Total regulation**: `total_error = total_power - grid_target`
- **Phase regulation**: `phase_error = phase_power - (grid_target / number_of_inverters)`

### Proportional Correction Gain
A proportional correction gain of **0.25** is applied on each iteration:
```rust
discharge_power += error * 0.25;
```
This fractional gain ensures a smooth, non-oscillating converge towards the target grid balance, mitigating sudden charging spikes or rapid cycling.

---

## 2. Operation Modes

The Power Manager can be toggled via the `{base_topic}/power_manager/command/mode` MQTT topic:

1. **Auto**: Regulates grid interactive power towards the configured `grid_target`.
2. **ChargeBatteries**: Overrides grid tracking and forces batteries to charge at their maximum supported rate (`-max_charge` constraint).
3. **MaximumFeedin**: Overrides grid tracking and forces batteries to discharge at their maximum supported rate (`max_discharge`), clamped by standard battery min SOC safety bounds.

---

## 3. Time-of-Use Scheduling & Planning

The manager evaluates the configured tables of `[Solax-BatteryControl.period.<Name>]` daily time windows:

- **Midnight Wrap-Around**: Periods can overlap midnight (e.g., `start = "22:00:00"`, `end = "06:00:00"`). The manager parses these strings and handles diurnal transition wrapping automatically.
- **Minimum State of Charge (SOC)**: Each period defines a `min-charge` threshold (%). If the battery's capacity drops below this threshold, discharging is suspended.
- **Grid Charging**: If battery capacity is below `min-charge` and `grid-charge = true` is configured for the active period, the manager commands the inverter to charge the battery from the utility grid.
- **Forced Discharge**: If `force-discharge` (W) is set for a period (e.g. during evening peak hours), the manager commands that constant rate directly, bypassing grid regulation.
- **Prefer Battery**: If `prefer-battery = true` is configured, solar generation is routed to charge the battery to its minimum threshold before allowing home loads to draw it down.

---

## 4. Advanced Phase Balancing Algorithms

In multi-phase installations, PowerScraper implements premium algorithms to balance batteries across phases:

### Phase vs. Total Power Regulation
- By default, inverters regulate against the specific phase (`phase = 1, 2, or 3`) they are physically wired to.
- If `use-total-power = true` is set, the inverter regulates against the *total* net grid import/export across all three phases. This is useful for systems where utility meters billing aggregates net consumption.
- **Dynamic Limits**: Setting `single-phase-charge-limit` and `single-phase-discharge-limit` enables hybrid regulation. Below the limit, the inverter regulates phase power to balance its local wire. Above the limit, it regulates total power to absorb high system loads.

### Linked Batteries
If `linked-batteries = true` is enabled, charge and discharge rates are balanced proportionally across all available inverters. Rather than allowing one inverter to work at 100% and another at 0%, the Power Manager distributes the required power demand equally, extending battery pack lifetimes.

### Grace Capacity Headroom
During solar generation hours, SolaX inverters can experience sudden grid export spikes if home loads drop while the battery is full. To prevent high export surges:
- Setting `grace = true` active for a period reserves capacity at the top of the charge curve (up to `grace-capacity` %, e.g., 70%).
- Charging is capped early using `grace-charge-power` (e.g. 500W) to leave headroom in the battery pack.
- **PV Power Bypass**: If solar generation exceeds `grace-power-threshold` (W), the headroom block is bypassed to avoid wasting solar energy.
