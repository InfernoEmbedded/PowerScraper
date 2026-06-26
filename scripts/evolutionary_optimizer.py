#!/usr/bin/env python3
import sys
import os
import random
import multiprocessing
import argparse
import datetime
import json
import sqlite3

# Ensure we can import battery_simulation
sys.path.append(os.path.dirname(__file__))

from battery_simulation import (
    calculate_demand_charges,
    BATTERY_CAPACITY_KWH,
    BATTERY_MAX_POWER_W,
    CHARGE_EFFICIENCY,
    DISCHARGE_EFFICIENCY,
    INITIAL_SOC,
    DEMAND_WINDOW_START,
    DEMAND_WINDOW_END,
)

# Search space bounds for parameters:
# 0: neg_price_threshold (c/kWh)
# 1: export_dump_threshold (c/kWh)
# 2: dump_reserve_demand (kWh)
# 3: dump_reserve_normal (kWh)
# 4: pre_charge_price_threshold (c/kWh)
# 5: pre_charge_soc_limit (0.0 to 1.0)
# 6: pre_charge_start_hour (0 to 15)
# 7: use_adaptive_shaving (0 or 1)
# 8: adaptive_safety_buffer (W)
BOUNDS = [
    (-15.0, 5.0),       # neg_price_threshold
    (10.0, 60.0),       # export_dump_threshold
    (0.0, 13.8),        # dump_reserve_demand
    (0.0, 10.0),        # dump_reserve_normal
    (0.0, 30.0),        # pre_charge_price_threshold
    (0.0, 1.0),         # pre_charge_soc_limit
    (0.0, 15.0),        # pre_charge_start_hour
    (0, 1),             # use_adaptive_shaving
    (-500.0, 1500.0),   # adaptive_safety_buffer
]

PARAM_NAMES = [
    "neg_price_threshold",
    "export_dump_threshold",
    "dump_reserve_demand",
    "dump_reserve_normal",
    "pre_charge_price_threshold",
    "pre_charge_soc_limit",
    "pre_charge_start_hour",
    "use_adaptive_shaving",
    "adaptive_safety_buffer"
]

# Global variables for worker initialization
global_records = None
global_cycle_penalty = 0.0
global_total_days = 30.0

def init_worker(records, cycle_penalty, total_days):
    global global_records, global_cycle_penalty, global_total_days
    global_records = records
    global_cycle_penalty = cycle_penalty
    global_total_days = total_days

def run_simulation_param(records, p):
    neg_price_threshold = p[0]
    export_dump_threshold = p[1]
    dump_reserve_demand = p[2]
    dump_reserve_normal = p[3]
    pre_charge_price_threshold = p[4]
    pre_charge_soc_limit = p[5]
    pre_charge_start_hour = int(p[6])
    use_adaptive_shaving = int(p[7])
    adaptive_safety_buffer = p[8]

    import_kwh = 0.0
    export_kwh = 0.0
    energy_cost = 0.0
    
    battery_energy_kwh = BATTERY_CAPACITY_KWH * INITIAL_SOC
    monthly_peak_demand_w = {}
    cycles = 0.0

    for net_power_w, hour, month_key, import_price, export_price, duration_hours in records:
        is_demand_window = DEMAND_WINDOW_START <= hour < DEMAND_WINDOW_END
        is_pre_charge_window = pre_charge_start_hour <= hour < DEMAND_WINDOW_START
        current_month_peak_w = monthly_peak_demand_w.get(month_key, 0.0)
        
        charge_w = 0.0
        discharge_w = 0.0

        # Heuristics:
        # 1. Extreme negative price: charge from grid
        if import_price < neg_price_threshold:
            max_avail_charge_w = ((BATTERY_CAPACITY_KWH - battery_energy_kwh) / CHARGE_EFFICIENCY) / duration_hours * 1000.0
            charge_w = min(BATTERY_MAX_POWER_W, max_avail_charge_w)
            
        # 2. High export price: dump to grid (arbitrage)
        elif export_price >= export_dump_threshold:
            reserve_kwh = dump_reserve_demand if (12 <= hour < DEMAND_WINDOW_END) else dump_reserve_normal
            if battery_energy_kwh > reserve_kwh:
                max_avail_discharge_w = ((battery_energy_kwh - reserve_kwh) * DISCHARGE_EFFICIENCY) / duration_hours * 1000.0
                discharge_w = min(BATTERY_MAX_POWER_W, max_avail_discharge_w)

        # 3. Pre-charge window: top up using cheap grid
        elif is_pre_charge_window and import_price < pre_charge_price_threshold and (battery_energy_kwh / BATTERY_CAPACITY_KWH) < pre_charge_soc_limit:
            target_energy_kwh = BATTERY_CAPACITY_KWH * pre_charge_soc_limit
            deficit_kwh = target_energy_kwh - battery_energy_kwh
            max_charge_w = (deficit_kwh / CHARGE_EFFICIENCY) / duration_hours * 1000.0
            charge_w = min(BATTERY_MAX_POWER_W, max_charge_w)

        # 4. Demand window peak shaving
        elif is_demand_window:
            if use_adaptive_shaving == 1:
                target_peak = max(0.0, current_month_peak_w - adaptive_safety_buffer)
                if net_power_w > target_peak:
                    excess_w = net_power_w - target_peak
                    max_avail_discharge_w = (battery_energy_kwh * DISCHARGE_EFFICIENCY) / duration_hours * 1000.0
                    discharge_w = min(excess_w, BATTERY_MAX_POWER_W, max_avail_discharge_w)
                elif net_power_w < 0:
                    max_avail_charge_w = ((BATTERY_CAPACITY_KWH - battery_energy_kwh) / CHARGE_EFFICIENCY) / duration_hours * 1000.0
                    charge_w = min(-net_power_w, BATTERY_MAX_POWER_W, max_avail_charge_w)
            else:
                # Full shave to 0
                if net_power_w > 0:
                    max_avail_discharge_w = (battery_energy_kwh * DISCHARGE_EFFICIENCY) / duration_hours * 1000.0
                    discharge_w = min(net_power_w, BATTERY_MAX_POWER_W, max_avail_discharge_w)
                else:
                    max_avail_charge_w = ((BATTERY_CAPACITY_KWH - battery_energy_kwh) / CHARGE_EFFICIENCY) / duration_hours * 1000.0
                    charge_w = min(-net_power_w, BATTERY_MAX_POWER_W, max_avail_charge_w)

        # 5. Standard operation
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
                monthly_peak_demand_w[month_key] = max(monthly_peak_demand_w.get(month_key, 0.0), net_grid_w)
        else:
            kwh = (-net_grid_w / 1000.0) * duration_hours
            export_kwh += kwh
            energy_cost -= kwh * (export_price / 100.0)

    return import_kwh, export_kwh, energy_cost, monthly_peak_demand_w, cycles

def evaluate_individual(ind):
    import_kwh, export_kwh, energy_cost, peaks, cycles = run_simulation_param(global_records, ind)
    demand_cost = calculate_demand_charges(peaks, global_total_days)
    total_cost = energy_cost + demand_cost + cycles * global_cycle_penalty
    return total_cost, import_kwh, export_kwh, energy_cost, demand_cost, cycles

def parse_timezone_offset(tz_str):
    if not tz_str:
        return None
    tz_trimmed = tz_str.strip()
    if tz_trimmed.upper() in ["UTC", "GMT", "Z"]:
        return datetime.timezone.utc
    
    is_posix = tz_trimmed[0].isalpha() and not tz_trimmed.upper().startswith("UTC") and not tz_trimmed.upper().startswith("GMT")
    
    search_str = tz_trimmed
    if tz_trimmed.upper().startswith("UTC") or tz_trimmed.upper().startswith("GMT"):
        search_str = tz_trimmed[3:]
        
    sign = 1
    offset_str = ""
    
    for i, c in enumerate(search_str):
        if c in ['+', '-', '.'] or c.isdigit():
            remainder = search_str[i:]
            if remainder.startswith('+'):
                sign = 1
                offset_str = remainder[1:]
            elif remainder.startswith('-'):
                sign = -1
                offset_str = remainder[1:]
            else:
                sign = 1
                offset_str = remainder
            break
            
    if is_posix:
        sign = -sign
        
    if not offset_str:
        return None
        
    try:
        if ':' in offset_str:
            parts = offset_str.split(':')
            hours = int(parts[0])
            minutes = int(parts[1]) if len(parts) > 1 else 0
            seconds = sign * (hours * 3600 + minutes * 60)
        elif '.' in offset_str:
            val = float(offset_str)
            seconds = int(sign * val * 3600)
        else:
            hours = int(offset_str)
            seconds = sign * hours * 3600
        return datetime.timezone(datetime.timedelta(seconds=seconds))
    except Exception:
        return None

def load_telemetry_from_db(db_path, mains_source="MainsMeter"):
    """
    Connects to the SQLite database and loads historical telemetry.
    """
    if not os.path.exists(db_path):
        return []
    try:
        conn = sqlite3.connect(db_path)
        cursor = conn.cursor()
        
        # Check if settings table exists and load timezone
        cursor.execute("SELECT name FROM sqlite_master WHERE type='table' AND name='settings'")
        tz_str = None
        if cursor.fetchone():
            cursor.execute("SELECT config_json FROM settings LIMIT 1")
            row = cursor.fetchone()
            if row:
                try:
                    config_data = json.loads(row[0])
                    tz_str = config_data.get("Solax-BatteryControl", {}).get("timezone")
                except Exception:
                    pass

        # Check if telemetry table exists
        cursor.execute("SELECT name FROM sqlite_master WHERE type='table' AND name='telemetry_history'")
        if not cursor.fetchone():
            conn.close()
            return []
            
        cursor.execute("SELECT timestamp, topic, value FROM telemetry_history ORDER BY timestamp ASC")
        groups = {}
        for ts, topic, val in cursor.fetchall():
            ts_rounded = (ts // 60) * 60
            if ts_rounded not in groups:
                groups[ts_rounded] = {}
            groups[ts_rounded][topic] = val
                
        conn.close()
        
        raw_records = []
        last_imp = 25.0
        last_exp = 8.0
        tz_offset = parse_timezone_offset(tz_str)
        
        for ts in sorted(groups.keys()):
            g = groups[ts]
            
            solar = 0.0
            battery = 0.0
            load = None
            import_price = None
            export_price = None
            
            for topic, val in g.items():
                if topic.endswith("/PV1 Power") or topic.endswith("/PV2 Power") or "/Input 1 Power" in topic or "/Input 2 Power" in topic:
                    solar += val
                elif topic == f"{mains_source}/Total system power":
                    load = val
                elif topic.endswith("/Battery Power"):
                    battery += val
                elif topic == "tariff/import_price":
                    import_price = val
                elif topic == "tariff/export_price":
                    export_price = val
                    
            if load is None:
                continue
                
            if import_price is not None and not math.isnan(import_price):
                last_imp = import_price
            if export_price is not None and not math.isnan(export_price):
                last_exp = export_price
                
            if tz_offset:
                dt_local = datetime.datetime.fromtimestamp(ts, tz=tz_offset)
            else:
                dt_local = datetime.datetime.fromtimestamp(ts).astimezone()
                
            gross_load_w = max(0.0, load + solar + battery)
            raw_records.append({
                'timestamp': ts,
                'dt_local': dt_local,
                'solar_power_w': solar,
                'load_power_w': gross_load_w,
                'import_price_cents': last_imp,
                'export_price_cents': last_exp,
            })

        # Calculate dynamic duration_hours and filter gaps
        records = []
        for i in range(len(raw_records) - 1):
            r = raw_records[i]
            r_next = raw_records[i + 1]
            duration_hours = (r_next['timestamp'] - r['timestamp']) / 3600.0
            if duration_hours > 2.0:
                continue
            r['duration_hours'] = duration_hours
            records.append(r)

        return records
    except Exception as e:
        print(f"Error loading from DB: {e}", file=sys.stderr)
        return []

def load_telemetry_from_csv(csv_path):
    if not os.path.exists(csv_path):
        return []
    try:
        # Using battery_simulation's parse_csv
        from battery_simulation import parse_csv
        return parse_csv(csv_path)
    except Exception as e:
        print(f"Error loading from CSV: {e}", file=sys.stderr)
        return []

def parse_seed(seed_str):
    """
    Parses seed string which can be a JSON string or comma-separated list of float values.
    """
    try:
        # Try JSON first
        data = json.loads(seed_str)
        if isinstance(data, list) and len(data) == 9:
            return data
        elif isinstance(data, dict):
            # Parse dict
            return [data.get(name, BOUNDS[i][0]) for i, name in enumerate(PARAM_NAMES)]
    except Exception:
        pass
        
    try:
        parts = [float(x.strip()) for x in seed_str.split(",")]
        if len(parts) == 9:
            return parts
    except Exception:
        pass
        
    return None

def main():
    parser = argparse.ArgumentParser(description="PowerScraper GA Evolutionary Optimizer")
    parser.add_argument("--db", help="Path to config.db sqlite file")
    parser.add_argument("--csv", help="Path to historical_data.csv fallback file")
    parser.add_argument("--generations", type=int, default=100, help="Number of GA generations")
    parser.add_argument("--pop-size", type=int, default=40, help="Population size")
    parser.add_argument("--penalty", type=type(1.0), default=1.0, help="Cycle penalty ($/cycle)")
    parser.add_argument("--seed", type=str, help="Seed parameters for initial population")
    parser.add_argument("--cores", type=int, help="Max CPU cores to utilize")
    parser.add_argument("--mains-source", default="MainsMeter", help="Grid meter name in telemetry")

    args = parser.parse_args()

    # 1. Load Records
    records = []
    if args.db:
        print(f"Attempting to load telemetry from database: {args.db}")
        records = load_telemetry_from_db(args.db, args.mains_source)
        
    if not records and args.csv:
        print(f"Attempting to load telemetry from CSV: {args.csv}")
        records = load_telemetry_from_csv(args.csv)
        
    if not records:
        # Try default paths
        default_csv = "/home/deece/watt-home/src/PowerScraper/scratch/historical_data.csv"
        print(f"No records loaded. Trying default CSV path: {default_csv}")
        records = load_telemetry_from_csv(default_csv)
        
    if not records:
        print("Error: No historical telemetry records found. Cannot optimize.", file=sys.stderr)
        sys.exit(1)

    start_date = records[0]['dt_local']
    end_date = records[-1]['dt_local']
    total_days = (end_date - start_date).days
    print(f"Optimization Period: {start_date} to {end_date} ({total_days} days)")

    # 2. Preprocess records
    print("Pre-processing records for simulation...")
    opt_records = []
    for r in records:
        net_power_w = r['load_power_w'] - r['solar_power_w']
        hour = r['dt_local'].hour
        month_key = r['dt_local'].strftime("%Y-%m")
        import_price = r['import_price_cents']
        export_price = r['export_price_cents']
        duration_hours = r['duration_hours']
        opt_records.append((net_power_w, hour, month_key, import_price, export_price, duration_hours))
    print("Pre-processing complete.")

    # 3. Configure multiprocessing cores
    cpu_count = multiprocessing.cpu_count()
    max_cores = max(1, cpu_count - 1)
    if args.cores:
        max_cores = min(max_cores, max(1, args.cores))
    print(f"Configuring multiprocessing pool with {max_cores} workers (CPU count: {cpu_count}).")

    # 4. Parse Seed
    seed_ind = None
    if args.seed:
        seed_ind = parse_seed(args.seed)
        if seed_ind:
            print(f"Parsed initial seed: {seed_ind}")
        else:
            print("Warning: Failed to parse seed parameter string.", file=sys.stderr)

    # 5. Initialize population
    population = []
    # If seed is provided, add it and a few slightly mutated clones of it to population
    if seed_ind:
        population.append(seed_ind)
        # Add 5 mutated clones to seed population
        for _ in range(min(5, args.pop_size - 1)):
            clone = []
            for i, b in enumerate(BOUNDS):
                if b[0] == 0 and b[1] == 1:
                    clone.append(seed_ind[i])
                else:
                    sigma = (b[1] - b[0]) * 0.05
                    val = seed_ind[i] + random.gauss(0, sigma)
                    clone.append(max(b[0], min(b[1], val)))
            population.append(clone)

    # Fill rest of population randomly
    while len(population) < args.pop_size:
        ind = []
        for b in BOUNDS:
            if b[0] == 0 and b[1] == 1:
                ind.append(random.choice([0, 1]))
            else:
                ind.append(random.uniform(b[0], b[1]))
        population.append(ind)

    # Start GA
    pool = multiprocessing.Pool(processes=max_cores, initializer=init_worker, initargs=(opt_records, args.penalty, total_days))

    best_ind = None
    best_fit = float('inf')
    best_results = None

    generations = args.generations
    for gen in range(generations):
        results = pool.map(evaluate_individual, population)
        
        # Sort population
        sorted_pop_with_res = sorted(zip(population, results), key=lambda x: x[1][0])
        gen_best_ind, gen_best_res = sorted_pop_with_res[0]
        gen_best_cost = gen_best_res[0]

        if gen_best_cost < best_fit:
            best_fit = gen_best_cost
            best_ind = gen_best_ind
            best_results = gen_best_res

        # Print progress to stdout for Rust daemon parsing
        percent = (gen + 1) / generations * 100.0
        bill = gen_best_res[3] + gen_best_res[4]
        cycles = gen_best_res[5]
        print(f"GEN_PROGRESS: {gen+1}/{generations} | BEST_COST: {gen_best_cost:.2f} | BILL: {bill:.2f} | CYCLES: {cycles:.1f} | PERCENT: {percent:.1f}", flush=True)

        # Selection & Crossover
        new_population = [x[0] for x in sorted_pop_with_res[:2]] # Elitism: keep best 2

        while len(new_population) < args.pop_size:
            # Tournament selection (size 3)
            p1_ind = min(random.sample(sorted_pop_with_res, 3), key=lambda x: x[1][0])[0]
            p2_ind = min(random.sample(sorted_pop_with_res, 3), key=lambda x: x[1][0])[0]

            child = []
            for i, b in enumerate(BOUNDS):
                if b[0] == 0 and b[1] == 1:
                    child.append(random.choice([p1_ind[i], p2_ind[i]]))
                else:
                    alpha = 0.3
                    x_min = min(p1_ind[i], p2_ind[i])
                    x_max = max(p1_ind[i], p2_ind[i])
                    range_val = x_max - x_min
                    val = random.uniform(x_min - alpha * range_val, x_max + alpha * range_val)
                    child.append(max(b[0], min(b[1], val)))

            # Mutation
            for i, b in enumerate(BOUNDS):
                if random.random() < 0.15:
                    if b[0] == 0 and b[1] == 1:
                        child[i] = 1 - child[i]
                    else:
                        sigma = (b[1] - b[0]) * 0.1
                        child[i] = max(b[0], min(b[1], child[i] + random.gauss(0, sigma)))

            new_population.append(child)

        population = new_population

    pool.close()
    pool.join()

    # Final Evolved parameters output
    print("\n=== Tuned Parameters ===")
    param_dict = {}
    for name, val in zip(PARAM_NAMES, best_ind):
        # Format outputs cleanly
        if name in ["pre_charge_start_hour", "use_adaptive_shaving"]:
            val = int(round(val))
        else:
            val = round(val, 2)
        param_dict[name] = val
        print(f"  {name:28s} = {val}")

    # Output JSON block for Rust parsing
    output_result = {
        "status": "success",
        "best_params": {
            "neg_price_threshold": param_dict["neg_price_threshold"],
            "export_dump_threshold": param_dict["export_dump_threshold"],
            "dump_reserve_demand": param_dict["dump_reserve_demand"],
            "dump_reserve_normal": param_dict["dump_reserve_normal"],
            "pre_charge_price_threshold": param_dict["pre_charge_price_threshold"],
            "pre_charge_soc_limit": param_dict["pre_charge_soc_limit"],
            "pre_charge_start_hour": int(param_dict["pre_charge_start_hour"]),
            "use_adaptive_shaving": param_dict["use_adaptive_shaving"] == 1,
            "adaptive_safety_buffer": param_dict["adaptive_safety_buffer"]
        },
        "metrics": {
            "net_bill": round(best_results[3] + best_results[4], 2),
            "cycles": round(best_results[5], 1),
            "total_cost": round(best_results[0], 2)
        }
    }
    
    print("\n--- JSON RESULT ---")
    print(json.dumps(output_result))
    print("-------------------")

if __name__ == "__main__":
    main()
