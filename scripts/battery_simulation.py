#!/usr/bin/env python3
import csv
import datetime
import math
import sys
import os

# Configuration for the simulation
BATTERY_CAPACITY_KWH = 13.8    # Total battery capacity
BATTERY_MAX_POWER_W = 5000     # Max charge/discharge rate
CHARGE_EFFICIENCY = 0.95       # Efficiency when charging
DISCHARGE_EFFICIENCY = 0.95    # Efficiency when discharging (90% round trip)
INITIAL_SOC = 0.5              # Start at 50% SoC

# Demand tariff parameters
DEMAND_CHARGE_RATE_PER_KW_DAY = 0.20 # $0.20 per kW per day (about $6.00 per kW per month)
DEMAND_WINDOW_START = 15
DEMAND_WINDOW_END = 21

# Export Arbitrage parameters
EXPORT_DUMP_THRESHOLD = 30.0   # Dump battery to grid if export price >= 30c/kWh

def parse_csv(filepath):
    """
    Parses the historical data CSV.
    Headers: timestamp,datetime_local,datetime_utc,solar1_pv1_power,...
    Returns a list of dictionaries with cleaned values.
    """
    records = []
    print(f"Loading CSV data from {filepath}...")
    with open(filepath, 'r') as f:
        reader = csv.DictReader(f)
        for row in reader:
            try:
                dt_local = datetime.datetime.strptime(row['datetime_local'], "%Y-%m-%d %H:%M:%S")
            except Exception:
                continue

            # Sum solar inputs
            solar_power = 0.0
            solar_keys = [
                'solar1_pv1_power', 'solar1_pv2_power',
                'solar2_pv1_power', 'solar2_pv2_power',
                'solax_x3_pv1_power', 'solax_x3_pv2_power'
            ]
            for key in solar_keys:
                val = row.get(key)
                if val:
                    try:
                        solar_power += float(val)
                    except ValueError:
                        pass

            # Household gross load (global_usage)
            load_power = 0.0
            val_load = row.get('global_usage')
            if val_load:
                try:
                    load_power += float(val_load)
                except ValueError:
                    pass

            # Prices (cents/kWh)
            import_price = None
            export_price = None
            val_imp = row.get('amber_import_price')
            val_exp = row.get('amber_export_price')
            if val_imp and val_imp.strip() != "":
                try:
                    import_price = float(val_imp)
                except ValueError:
                    pass
            if val_exp and val_exp.strip() != "":
                try:
                    export_price = float(val_exp)
                except ValueError:
                    pass

            records.append({
                'timestamp': int(row['timestamp']),
                'dt_local': dt_local,
                'solar_power_w': solar_power,
                'load_power_w': load_power,
                'import_price_cents': import_price,
                'export_price_cents': export_price,
            })

    # Forward/backward fill prices if any were empty/default
    records.sort(key=lambda x: x['timestamp'])
    
    last_imp = 25.0
    last_exp = 8.0
    
    for r in records:
        if r['import_price_cents'] is not None and not math.isnan(r['import_price_cents']):
            last_imp = r['import_price_cents']
        r['import_price_cents'] = last_imp
        
        if r['export_price_cents'] is not None and not math.isnan(r['export_price_cents']):
            last_exp = r['export_price_cents']
        r['export_price_cents'] = last_exp

    # Calculate dynamic duration_hours and filter gaps
    raw_records = records
    records = []
    for i in range(len(raw_records) - 1):
        r = raw_records[i]
        r_next = raw_records[i + 1]
        duration_hours = (r_next['timestamp'] - r['timestamp']) / 3600.0
        if duration_hours > 2.0:
            continue
        r['duration_hours'] = duration_hours
        records.append(r)

    print(f"Loaded {len(records)} valid records.")
    return records



def run_simulation_no_battery(records):
    """
    Simulates the system with no battery.
    """
    import_kwh = 0.0
    export_kwh = 0.0
    energy_cost = 0.0
    monthly_peak_demand_w = {}

    for r in records:
        net_power_w = r['load_power_w'] - r['solar_power_w']
        hour = r['dt_local'].hour
        month_key = r['dt_local'].strftime("%Y-%m")
        duration_hours = r['duration_hours']

        if net_power_w > 0:
            kwh = (net_power_w / 1000.0) * duration_hours
            import_kwh += kwh
            energy_cost += kwh * (r['import_price_cents'] / 100.0)
            
            if DEMAND_WINDOW_START <= hour < DEMAND_WINDOW_END:
                monthly_peak_demand_w[month_key] = max(monthly_peak_demand_w.get(month_key, 0.0), net_power_w)
        else:
            kwh = (-net_power_w / 1000.0) * duration_hours
            export_kwh += kwh
            energy_cost -= kwh * (r['export_price_cents'] / 100.0)

    return import_kwh, export_kwh, energy_cost, monthly_peak_demand_w


def run_simulation_baseline_battery(records):
    """
    Model 1: Solar Self-Consumption (Baseline)
    - Battery only charges from excess solar
    - Battery only discharges to cover household load
    """
    import_kwh = 0.0
    export_kwh = 0.0
    energy_cost = 0.0
    
    battery_energy_kwh = BATTERY_CAPACITY_KWH * INITIAL_SOC
    monthly_peak_demand_w = {}
    cycles = 0.0

    for r in records:
        net_power_w = r['load_power_w'] - r['solar_power_w']
        hour = r['dt_local'].hour
        month_key = r['dt_local'].strftime("%Y-%m")
        duration_hours = r['duration_hours']

        if net_power_w > 0:
            # Deficit: discharge battery if possible
            max_avail_discharge_w = (battery_energy_kwh * DISCHARGE_EFFICIENCY) / duration_hours * 1000.0
            discharge_w = min(net_power_w, BATTERY_MAX_POWER_W, max_avail_discharge_w)
            
            battery_energy_kwh -= (discharge_w / 1000.0) * duration_hours / DISCHARGE_EFFICIENCY
            cycles += (discharge_w / 1000.0) * duration_hours / BATTERY_CAPACITY_KWH
            
            net_grid_w = net_power_w - discharge_w
        else:
            # Excess solar: charge battery if possible
            max_avail_charge_w = ((BATTERY_CAPACITY_KWH - battery_energy_kwh) / CHARGE_EFFICIENCY) / duration_hours * 1000.0
            charge_w = min(-net_power_w, BATTERY_MAX_POWER_W, max_avail_charge_w)
            
            battery_energy_kwh += (charge_w / 1000.0) * duration_hours * CHARGE_EFFICIENCY
            cycles += (charge_w / 1000.0) * duration_hours / BATTERY_CAPACITY_KWH
            
            net_grid_w = net_power_w + charge_w

        if net_grid_w > 0:
            kwh = (net_grid_w / 1000.0) * duration_hours
            import_kwh += kwh
            energy_cost += kwh * (r['import_price_cents'] / 100.0)
            
            if DEMAND_WINDOW_START <= hour < DEMAND_WINDOW_END:
                monthly_peak_demand_w[month_key] = max(monthly_peak_demand_w.get(month_key, 0.0), net_grid_w)
        else:
            kwh = (-net_grid_w / 1000.0) * duration_hours
            export_kwh += kwh
            energy_cost -= kwh * (r['export_price_cents'] / 100.0)

    return import_kwh, export_kwh, energy_cost, monthly_peak_demand_w, cycles


def run_simulation_smart_heuristic(records):
    """
    Model 2: Smart Heuristic (Amber-Aware + Demand Tariff Shaving + Export Arbitrage)
    - Charges from grid when price is negative.
    - Dumps to grid when export price is high (>= 30c), keeping a safety reserve for the demand window.
    - Pre-charges from cheap grid power (< 15c) before peak window if solar won't fill it.
    - Shaves grid imports to 0 during demand window.
    """
    import_kwh = 0.0
    export_kwh = 0.0
    energy_cost = 0.0
    
    battery_energy_kwh = BATTERY_CAPACITY_KWH * INITIAL_SOC
    monthly_peak_demand_w = {}
    cycles = 0.0

    for r in records:
        net_power_w = r['load_power_w'] - r['solar_power_w']
        hour = r['dt_local'].hour
        month_key = r['dt_local'].strftime("%Y-%m")
        import_price = r['import_price_cents']
        export_price = r['export_price_cents']
        duration_hours = r['duration_hours']

        is_demand_window = DEMAND_WINDOW_START <= hour < DEMAND_WINDOW_END
        is_pre_charge_window = 10 <= hour < DEMAND_WINDOW_START
        
        charge_w = 0.0
        discharge_w = 0.0

        # 1. Extreme negative price: charge from grid
        if import_price < 0.0:
            max_avail_charge_w = ((BATTERY_CAPACITY_KWH - battery_energy_kwh) / CHARGE_EFFICIENCY) / duration_hours * 1000.0
            charge_w = min(BATTERY_MAX_POWER_W, max_avail_charge_w)
            
        # 2. High export price: dump to grid (arbitrage)
        # Ensure we keep a safety reserve: 5.0 kWh if in/near demand window, 2.0 kWh otherwise
        elif export_price >= EXPORT_DUMP_THRESHOLD and battery_energy_kwh > (5.0 if (12 <= hour < DEMAND_WINDOW_END) else 2.0):
            reserve_kwh = 5.0 if (12 <= hour < DEMAND_WINDOW_END) else 2.0
            max_avail_discharge_w = ((battery_energy_kwh - reserve_kwh) * DISCHARGE_EFFICIENCY) / duration_hours * 1000.0
            discharge_w = min(BATTERY_MAX_POWER_W, max_avail_discharge_w)

        # 3. Pre-charge window: top up using cheap grid
        elif is_pre_charge_window and import_price < 15.0 and (battery_energy_kwh / BATTERY_CAPACITY_KWH) < 0.85:
            target_energy_kwh = BATTERY_CAPACITY_KWH * 0.85
            deficit_kwh = target_energy_kwh - battery_energy_kwh
            max_charge_w = (deficit_kwh / CHARGE_EFFICIENCY) / duration_hours * 1000.0
            charge_w = min(BATTERY_MAX_POWER_W, max_charge_w)
            
        # 4. Demand window: cover load
        elif is_demand_window:
            if net_power_w > 0:
                max_avail_discharge_w = (battery_energy_kwh * DISCHARGE_EFFICIENCY) / duration_hours * 1000.0
                discharge_w = min(net_power_w, BATTERY_MAX_POWER_W, max_avail_discharge_w)
            else:
                max_avail_charge_w = ((BATTERY_CAPACITY_KWH - battery_energy_kwh) / CHARGE_EFFICIENCY) / duration_hours * 1000.0
                charge_w = min(-net_power_w, BATTERY_MAX_POWER_W, max_avail_charge_w)
                
        # 5. Standard operation
        else:
            if net_power_w > 0:
                max_avail_discharge_w = (battery_energy_kwh * DISCHARGE_EFFICIENCY) / duration_hours * 1000.0
                discharge_w = min(net_power_w, BATTERY_MAX_POWER_W, max_avail_discharge_w)
            else:
                max_avail_charge_w = ((BATTERY_CAPACITY_KWH - battery_energy_kwh) / CHARGE_EFFICIENCY) / duration_hours * 1000.0
                charge_w = min(-net_power_w, BATTERY_MAX_POWER_W, max_avail_charge_w)

        if charge_w > 0:
            battery_energy_kwh += (charge_w / 1000.0) * duration_hours * CHARGE_EFFICIENCY
            cycles += (charge_w / 1000.0) * duration_hours / BATTERY_CAPACITY_KWH
            net_grid_w = net_power_w + charge_w
        elif discharge_w > 0:
            battery_energy_kwh -= (discharge_w / 1000.0) * duration_hours / DISCHARGE_EFFICIENCY
            cycles += (discharge_w / 1000.0) * duration_hours / BATTERY_CAPACITY_KWH
            net_grid_w = net_power_w - discharge_w
        else:
            net_grid_w = net_power_w

        if net_grid_w > 0:
            kwh = (net_grid_w / 1000.0) * duration_hours
            import_kwh += kwh
            energy_cost += kwh * (import_price / 100.0)
            
            if is_demand_window:
                monthly_peak_demand_w[month_key] = max(monthly_peak_demand_w.get(month_key, 0.0), net_grid_w)
        else:
            kwh = (-net_grid_w / 1000.0) * duration_hours
            export_kwh += kwh
            energy_cost -= kwh * (export_price / 100.0)

    return import_kwh, export_kwh, energy_cost, monthly_peak_demand_w, cycles


def run_simulation_predictive_opt(records):
    """
    Model 3: Predictive Look-Ahead MPC (Solar-Deficit-Aware)
    - Pre-charges from grid *only* if projected solar is insufficient.
    - Exports to grid if export price >= 35c.
    """
    import_kwh = 0.0
    export_kwh = 0.0
    energy_cost = 0.0
    
    battery_energy_kwh = BATTERY_CAPACITY_KWH * INITIAL_SOC
    monthly_peak_demand_w = {}
    cycles = 0.0
    T = len(records)
    LOOKAHEAD = 48

    for i in range(T):
        r = records[i]
        net_power_w = r['load_power_w'] - r['solar_power_w']
        hour = r['dt_local'].hour
        month_key = r['dt_local'].strftime("%Y-%m")
        import_price = r['import_price_cents']
        export_price = r['export_price_cents']
        duration_hours = r['duration_hours']

        is_demand_window = DEMAND_WINDOW_START <= hour < DEMAND_WINDOW_END

        demand_energy_needed_kwh = 0.0
        cheapest_future_steps = []
        expected_solar_kwh = 0.0
        
        end_idx = min(T, i + LOOKAHEAD)
        for j in range(i, end_idx):
            future_r = records[j]
            future_hour = future_r['dt_local'].hour
            future_net_w = future_r['load_power_w'] - future_r['solar_power_w']
            future_duration = future_r['duration_hours']
            
            if DEMAND_WINDOW_START <= future_hour < DEMAND_WINDOW_END and future_net_w > 0:
                demand_energy_needed_kwh += (future_net_w / 1000.0) * future_duration

            if future_hour < DEMAND_WINDOW_START and future_net_w < 0:
                expected_solar_kwh += (-future_net_w / 1000.0) * future_duration

            cheapest_future_steps.append((j, future_r['import_price_cents']))

        cheapest_future_steps.sort(key=lambda x: x[1])

        required_reserve_kwh = (demand_energy_needed_kwh / DISCHARGE_EFFICIENCY)
        required_reserve_kwh = min(required_reserve_kwh, BATTERY_CAPACITY_KWH * 0.95)

        charge_w = 0.0
        discharge_w = 0.0

        if is_demand_window:
            if net_power_w > 0:
                max_avail_discharge_w = (battery_energy_kwh * DISCHARGE_EFFICIENCY) / duration_hours * 1000.0
                discharge_w = min(net_power_w, BATTERY_MAX_POWER_W, max_avail_discharge_w)
            else:
                max_avail_charge_w = ((BATTERY_CAPACITY_KWH - battery_energy_kwh) / CHARGE_EFFICIENCY) / duration_hours * 1000.0
                charge_w = min(-net_power_w, BATTERY_MAX_POWER_W, max_avail_charge_w)
        else:
            projected_deficit_kwh = required_reserve_kwh - (battery_energy_kwh + expected_solar_kwh * CHARGE_EFFICIENCY)
            
            is_cheap_slot = False
            cheap_threshold_idx = max(1, len(cheapest_future_steps) // 5)
            cheap_indices = [x[0] for x in cheapest_future_steps[:cheap_threshold_idx]]
            if i in cheap_indices or import_price < 12.0:
                is_cheap_slot = True

            if projected_deficit_kwh > 0.0 and is_cheap_slot:
                max_charge_w = (projected_deficit_kwh / CHARGE_EFFICIENCY) / duration_hours * 1000.0
                charge_w = min(BATTERY_MAX_POWER_W, max_charge_w)
            elif net_power_w < 0:
                max_avail_charge_w = ((BATTERY_CAPACITY_KWH - battery_energy_kwh) / CHARGE_EFFICIENCY) / duration_hours * 1000.0
                charge_w = min(-net_power_w, BATTERY_MAX_POWER_W, max_avail_charge_w)
            elif export_price >= EXPORT_DUMP_THRESHOLD and battery_energy_kwh > (required_reserve_kwh + BATTERY_CAPACITY_KWH * 0.1):
                max_avail_discharge_w = ((battery_energy_kwh - required_reserve_kwh) * DISCHARGE_EFFICIENCY) / duration_hours * 1000.0
                discharge_w = min(BATTERY_MAX_POWER_W, max_avail_discharge_w)
            elif net_power_w > 0:
                available_battery_kwh = max(0.0, battery_energy_kwh - required_reserve_kwh)
                max_avail_discharge_w = (available_battery_kwh * DISCHARGE_EFFICIENCY) / duration_hours * 1000.0
                discharge_w = min(net_power_w, BATTERY_MAX_POWER_W, max_avail_discharge_w)

        if charge_w > 0:
            battery_energy_kwh += (charge_w / 1000.0) * duration_hours * CHARGE_EFFICIENCY
            cycles += (charge_w / 1000.0) * duration_hours / BATTERY_CAPACITY_KWH
            net_grid_w = net_power_w + charge_w
        elif discharge_w > 0:
            battery_energy_kwh -= (discharge_w / 1000.0) * duration_hours / DISCHARGE_EFFICIENCY
            cycles += (discharge_w / 1000.0) * duration_hours / BATTERY_CAPACITY_KWH
            net_grid_w = net_power_w - discharge_w
        else:
            net_grid_w = net_power_w

        if net_grid_w > 0:
            kwh = (net_grid_w / 1000.0) * duration_hours
            import_kwh += kwh
            energy_cost += kwh * (import_price / 100.0)
            
            if is_demand_window:
                monthly_peak_demand_w[month_key] = max(monthly_peak_demand_w.get(month_key, 0.0), net_grid_w)
        else:
            kwh = (-net_grid_w / 1000.0) * duration_hours
            export_kwh += kwh
            energy_cost -= kwh * (export_price / 100.0)

    return import_kwh, export_kwh, energy_cost, monthly_peak_demand_w, cycles


def run_simulation_adaptive_opt(records):
    """
    Model 4: Adaptive Peak Shaving (Monthly-Budgeted + Export Arbitrage)
    - Tracks peak grid import already incurred during the current billing month.
    - Shaves peaks during demand window ONLY if they exceed the established monthly peak.
    - Dumps to grid if export price >= 30c, keeping a safety reserve.
    """
    import_kwh = 0.0
    export_kwh = 0.0
    energy_cost = 0.0
    
    battery_energy_kwh = BATTERY_CAPACITY_KWH * INITIAL_SOC
    monthly_peak_demand_w = {}
    cycles = 0.0
    T = len(records)

    for i in range(T):
        r = records[i]
        net_power_w = r['load_power_w'] - r['solar_power_w']
        hour = r['dt_local'].hour
        month_key = r['dt_local'].strftime("%Y-%m")
        import_price = r['import_price_cents']
        export_price = r['export_price_cents']
        duration_hours = r['duration_hours']

        is_demand_window = DEMAND_WINDOW_START <= hour < DEMAND_WINDOW_END
        current_month_peak_w = monthly_peak_demand_w.get(month_key, 0.0)

        charge_w = 0.0
        discharge_w = 0.0

        # Heuristic rules:
        # 1. Negative price: charge from grid
        if import_price < 0.0:
            max_avail_charge_w = ((BATTERY_CAPACITY_KWH - battery_energy_kwh) / CHARGE_EFFICIENCY) / duration_hours * 1000.0
            charge_w = min(BATTERY_MAX_POWER_W, max_avail_charge_w)
            
        # 2. High export price: dump to grid (arbitrage)
        # Preserve a reserve: 5.0 kWh if close/in demand window, 2.0 kWh otherwise
        elif export_price >= EXPORT_DUMP_THRESHOLD and battery_energy_kwh > (5.0 if (12 <= hour < DEMAND_WINDOW_END) else 2.0):
            reserve_kwh = 5.0 if (12 <= hour < DEMAND_WINDOW_END) else 2.0
            max_avail_discharge_w = ((battery_energy_kwh - reserve_kwh) * DISCHARGE_EFFICIENCY) / duration_hours * 1000.0
            discharge_w = min(BATTERY_MAX_POWER_W, max_avail_discharge_w)

        # 3. Demand window peak shaving
        elif is_demand_window:
            if net_power_w > current_month_peak_w:
                excess_w = net_power_w - current_month_peak_w
                max_avail_discharge_w = (battery_energy_kwh * DISCHARGE_EFFICIENCY) / duration_hours * 1000.0
                discharge_w = min(excess_w, BATTERY_MAX_POWER_W, max_avail_discharge_w)
            elif net_power_w < 0:
                max_avail_charge_w = ((BATTERY_CAPACITY_KWH - battery_energy_kwh) / CHARGE_EFFICIENCY) / duration_hours * 1000.0
                charge_w = min(-net_power_w, BATTERY_MAX_POWER_W, max_avail_charge_w)
                
        # 4. Standard operation
        else:
            if net_power_w < 0:
                max_avail_charge_w = ((BATTERY_CAPACITY_KWH - battery_energy_kwh) / CHARGE_EFFICIENCY) / duration_hours * 1000.0
                charge_w = min(-net_power_w, BATTERY_MAX_POWER_W, max_avail_charge_w)
            elif net_power_w > 0:
                max_avail_discharge_w = (battery_energy_kwh * DISCHARGE_EFFICIENCY) / duration_hours * 1000.0
                discharge_w = min(net_power_w, BATTERY_MAX_POWER_W, max_avail_discharge_w)

        if charge_w > 0:
            battery_energy_kwh += (charge_w / 1000.0) * duration_hours * CHARGE_EFFICIENCY
            cycles += (charge_w / 1000.0) * duration_hours / BATTERY_CAPACITY_KWH
            net_grid_w = net_power_w + charge_w
        elif discharge_w > 0:
            battery_energy_kwh -= (discharge_w / 1000.0) * duration_hours / DISCHARGE_EFFICIENCY
            cycles += (discharge_w / 1000.0) * duration_hours / BATTERY_CAPACITY_KWH
            net_grid_w = net_power_w - discharge_w
        else:
            net_grid_w = net_power_w

        if net_grid_w > 0:
            kwh = (net_grid_w / 1000.0) * duration_hours
            import_kwh += kwh
            energy_cost += kwh * (import_price / 100.0)
            
            if is_demand_window:
                monthly_peak_demand_w[month_key] = max(current_month_peak_w, net_grid_w)
        else:
            kwh = (-net_grid_w / 1000.0) * duration_hours
            export_kwh += kwh
            energy_cost -= kwh * (export_price / 100.0)

    return import_kwh, export_kwh, energy_cost, monthly_peak_demand_w, cycles


def calculate_demand_charges(monthly_peak_demand_w, days_in_dataset):
    """
    Computes total demand charges based on monthly peak grid imports.
    """
    total_charge = 0.0
    for month, peak_w in monthly_peak_demand_w.items():
        peak_kw = peak_w / 1000.0
        monthly_rate = DEMAND_CHARGE_RATE_PER_KW_DAY * 30.0
        month_charge = peak_kw * monthly_rate
        total_charge += month_charge
    return total_charge


def main():
    csv_file = "dist/historical_data.csv"
    if not os.path.exists(csv_file):
        csv_file = "../dist/historical_data.csv"
    if not os.path.exists(csv_file):
        print(f"Error: historical_data.csv not found.")
        sys.exit(1)

    records = parse_csv(csv_file)
    if not records:
        print("No records to simulate.")
        sys.exit(1)

    start_date = records[0]['dt_local']
    end_date = records[-1]['dt_local']
    total_days = (end_date - start_date).days
    print(f"Simulation Period: {start_date} to {end_date} ({total_days} days)")

    # Run simulations
    print("Running Scenario A: No Battery...")
    imp_a, exp_a, cost_a, peaks_a = run_simulation_no_battery(records)
    demand_a = calculate_demand_charges(peaks_a, total_days)
    
    print("Running Scenario B: Baseline Battery Control...")
    imp_b, exp_b, cost_b, peaks_b, cycles_b = run_simulation_baseline_battery(records)
    demand_b = calculate_demand_charges(peaks_b, total_days)

    print("Running Scenario C: Smart Heuristic...")
    imp_c, exp_c, cost_c, peaks_c, cycles_c = run_simulation_smart_heuristic(records)
    demand_c = calculate_demand_charges(peaks_c, total_days)

    print("Running Scenario D: Look-Ahead MPC...")
    imp_d, exp_d, cost_d, peaks_d, cycles_d = run_simulation_predictive_opt(records)
    demand_d = calculate_demand_charges(peaks_d, total_days)

    print("Running Scenario E: Adaptive Peak Shaving...")
    imp_e, exp_e, cost_e, peaks_e, cycles_e = run_simulation_adaptive_opt(records)
    demand_e = calculate_demand_charges(peaks_e, total_days)

    # Print summary report
    print("\n=== Simulation Results ===")
    print(f"Scenario A: No Battery")
    print(f"  Import: {imp_a:.2f} kWh, Export: {exp_a:.2f} kWh")
    print(f"  Energy Cost: ${cost_a:.2f}, Demand Charge: ${demand_a:.2f}")
    print(f"  Total Cost: ${cost_a + demand_a:.2f}")
    
    print(f"Scenario B: Baseline Battery (Solar self-consumption)")
    print(f"  Import: {imp_b:.2f} kWh, Export: {exp_b:.2f} kWh, Cycles: {cycles_b:.1f}")
    print(f"  Energy Cost: ${cost_b:.2f}, Demand Charge: ${demand_b:.2f}")
    print(f"  Total Cost: ${cost_b + demand_b:.2f}")

    print(f"Scenario C: Smart Heuristic")
    print(f"  Import: {imp_c:.2f} kWh, Export: {exp_c:.2f} kWh, Cycles: {cycles_c:.1f}")
    print(f"  Energy Cost: ${cost_c:.2f}, Demand Charge: ${demand_c:.2f}")
    print(f"  Total Cost: ${cost_c + demand_c:.2f}")

    print(f"Scenario D: Look-Ahead MPC")
    print(f"  Import: {imp_d:.2f} kWh, Export: {exp_d:.2f} kWh, Cycles: {cycles_d:.1f}")
    print(f"  Energy Cost: ${cost_d:.2f}, Demand Charge: ${demand_d:.2f}")
    print(f"  Total Cost: ${cost_d + demand_d:.2f}")

    print(f"Scenario E: Adaptive Peak Shaving")
    print(f"  Import: {imp_e:.2f} kWh, Export: {exp_e:.2f} kWh, Cycles: {cycles_e:.1f}")
    print(f"  Energy Cost: ${cost_e:.2f}, Demand Charge: ${demand_e:.2f}")
    print(f"  Total Cost: ${cost_e + demand_e:.2f}")

if __name__ == "__main__":
    main()
