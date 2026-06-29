use crate::config::EvolvedHeuristicConfig;
use super::{SimRecord, SimConfig};
use std::collections::HashMap;
use chrono::Datelike;

#[cfg(unix)]
fn lower_current_thread_priority() {
    unsafe {
        // Set nice value to 19 (lowest standard nice priority)
        let _ = libc::setpriority(libc::PRIO_PROCESS, 0, 19);
        
        // On Linux, set scheduling policy to SCHED_IDLE (absolute lowest priority class)
        #[cfg(target_os = "linux")]
        {
            let param = libc::sched_param { sched_priority: 0 };
            let _ = libc::sched_setscheduler(0, libc::SCHED_IDLE, &param);
        }
    }
}

#[cfg(not(unix))]
fn lower_current_thread_priority() {}

pub struct SimpleRng {
    state: u64,
}

impl SimpleRng {
    pub fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() as f64) / (u64::MAX as f64)
    }

    pub fn range(&mut self, min: f64, max: f64) -> f64 {
        min + self.next_f64() * (max - min)
    }

    pub fn gauss(&mut self, mean: f64, std_dev: f64) -> f64 {
        // Box-Muller transform
        let mut u1 = self.next_f64();
        while u1 <= 1e-15 {
            u1 = self.next_f64();
        }
        let u2 = self.next_f64();
        let z = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
        mean + z * std_dev
    }
}

pub struct Bounds {
    pub neg_price_threshold: (f64, f64),
    pub export_dump_threshold: (f64, f64),
    pub dump_reserve_demand: (f64, f64),
    pub dump_reserve_normal: (f64, f64),
    pub pre_charge_price_threshold: (f64, f64),
    pub pre_charge_soc_limit: (f64, f64),
    pub pre_charge_start_hour: (f64, f64),
    pub use_adaptive_shaving: (f64, f64),
    pub adaptive_safety_buffer: (f64, f64),
    pub forecast_solar_weight: (f64, f64),
    pub tier2_export_dump_threshold: (f64, f64),
    pub tier2_dump_reserve: (f64, f64),
}

impl Bounds {
    pub fn new(battery_capacity: f64) -> Self {
        Self {
            neg_price_threshold: (-15.0, 5.0),
            export_dump_threshold: (10.0, 100.0),
            dump_reserve_demand: (0.0, battery_capacity),
            dump_reserve_normal: (0.0, battery_capacity),
            pre_charge_price_threshold: (0.0, 100.0),
            pre_charge_soc_limit: (0.0, 1.0),
            pre_charge_start_hour: (0.0, 24.0),
            use_adaptive_shaving: (0.0, 1.0),
            adaptive_safety_buffer: (-500.0, 1500.0),
            forecast_solar_weight: (0.0, 1.0),
            tier2_export_dump_threshold: (10.0, 100.0),
            tier2_dump_reserve: (0.0, battery_capacity),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Individual {
    pub neg_price_threshold: f64,
    pub export_dump_threshold: f64,
    pub dump_reserve_demand: f64,
    pub dump_reserve_normal: f64,
    pub pre_charge_price_threshold: f64,
    pub pre_charge_soc_limit: f64,
    pub pre_charge_start_hour: f64,
    pub use_adaptive_shaving: f64,
    pub adaptive_safety_buffer: f64,
    pub forecast_solar_weight: f64,
    pub tier2_export_dump_threshold: f64,
    pub tier2_dump_reserve: f64,
}

impl Individual {
    pub fn random(rng: &mut SimpleRng, bounds: &Bounds) -> Self {
        Self {
            neg_price_threshold: rng.range(bounds.neg_price_threshold.0, bounds.neg_price_threshold.1),
            export_dump_threshold: rng.range(bounds.export_dump_threshold.0, bounds.export_dump_threshold.1),
            dump_reserve_demand: rng.range(bounds.dump_reserve_demand.0, bounds.dump_reserve_demand.1),
            dump_reserve_normal: rng.range(bounds.dump_reserve_normal.0, bounds.dump_reserve_normal.1),
            pre_charge_price_threshold: rng.range(bounds.pre_charge_price_threshold.0, bounds.pre_charge_price_threshold.1),
            pre_charge_soc_limit: rng.range(bounds.pre_charge_soc_limit.0, bounds.pre_charge_soc_limit.1),
            pre_charge_start_hour: rng.range(bounds.pre_charge_start_hour.0, bounds.pre_charge_start_hour.1),
            use_adaptive_shaving: rng.range(bounds.use_adaptive_shaving.0, bounds.use_adaptive_shaving.1),
            adaptive_safety_buffer: rng.range(bounds.adaptive_safety_buffer.0, bounds.adaptive_safety_buffer.1),
            forecast_solar_weight: rng.range(bounds.forecast_solar_weight.0, bounds.forecast_solar_weight.1),
            tier2_export_dump_threshold: rng.range(bounds.tier2_export_dump_threshold.0, bounds.tier2_export_dump_threshold.1),
            tier2_dump_reserve: rng.range(bounds.tier2_dump_reserve.0, bounds.tier2_dump_reserve.1),
        }
    }

    pub fn clamp(&mut self, bounds: &Bounds) {
        self.neg_price_threshold = self.neg_price_threshold.clamp(bounds.neg_price_threshold.0, bounds.neg_price_threshold.1);
        self.export_dump_threshold = self.export_dump_threshold.clamp(bounds.export_dump_threshold.0, bounds.export_dump_threshold.1);
        self.dump_reserve_demand = self.dump_reserve_demand.clamp(bounds.dump_reserve_demand.0, bounds.dump_reserve_demand.1);
        self.dump_reserve_normal = self.dump_reserve_normal.clamp(bounds.dump_reserve_normal.0, bounds.dump_reserve_normal.1);
        self.pre_charge_price_threshold = self.pre_charge_price_threshold.clamp(bounds.pre_charge_price_threshold.0, bounds.pre_charge_price_threshold.1);
        self.pre_charge_soc_limit = self.pre_charge_soc_limit.clamp(bounds.pre_charge_soc_limit.0, bounds.pre_charge_soc_limit.1);
        self.pre_charge_start_hour = self.pre_charge_start_hour.clamp(bounds.pre_charge_start_hour.0, bounds.pre_charge_start_hour.1);
        self.use_adaptive_shaving = self.use_adaptive_shaving.clamp(bounds.use_adaptive_shaving.0, bounds.use_adaptive_shaving.1);
        self.adaptive_safety_buffer = self.adaptive_safety_buffer.clamp(bounds.adaptive_safety_buffer.0, bounds.adaptive_safety_buffer.1);
        self.forecast_solar_weight = self.forecast_solar_weight.clamp(bounds.forecast_solar_weight.0, bounds.forecast_solar_weight.1);
        self.tier2_export_dump_threshold = self.tier2_export_dump_threshold.clamp(bounds.tier2_export_dump_threshold.0, bounds.tier2_export_dump_threshold.1);
        self.tier2_dump_reserve = self.tier2_dump_reserve.clamp(bounds.tier2_dump_reserve.0, bounds.tier2_dump_reserve.1);
    }

    pub fn mutate(&mut self, rng: &mut SimpleRng, bounds: &Bounds, rate: f64) {
        let mutate_param = |val: &mut f64, bounds: (f64, f64), rng: &mut SimpleRng| {
            if rng.next_f64() < rate {
                let sigma = (bounds.1 - bounds.0) * 0.05;
                *val += rng.gauss(0.0, sigma);
            }
        };

        mutate_param(&mut self.neg_price_threshold, bounds.neg_price_threshold, rng);
        mutate_param(&mut self.export_dump_threshold, bounds.export_dump_threshold, rng);
        mutate_param(&mut self.dump_reserve_demand, bounds.dump_reserve_demand, rng);
        mutate_param(&mut self.dump_reserve_normal, bounds.dump_reserve_normal, rng);
        mutate_param(&mut self.pre_charge_price_threshold, bounds.pre_charge_price_threshold, rng);
        mutate_param(&mut self.pre_charge_soc_limit, bounds.pre_charge_soc_limit, rng);
        mutate_param(&mut self.pre_charge_start_hour, bounds.pre_charge_start_hour, rng);
        mutate_param(&mut self.use_adaptive_shaving, bounds.use_adaptive_shaving, rng);
        mutate_param(&mut self.adaptive_safety_buffer, bounds.adaptive_safety_buffer, rng);
        mutate_param(&mut self.forecast_solar_weight, bounds.forecast_solar_weight, rng);
        mutate_param(&mut self.tier2_export_dump_threshold, bounds.tier2_export_dump_threshold, rng);
        mutate_param(&mut self.tier2_dump_reserve, bounds.tier2_dump_reserve, rng);

        self.clamp(bounds);
    }

    pub fn crossover(parent1: &Self, parent2: &Self, rng: &mut SimpleRng, bounds: &Bounds) -> Self {
        let crossover_param = |p1: f64, p2: f64, bounds: (f64, f64), rng: &mut SimpleRng| -> f64 {
            let alpha = 0.3;
            let x_min = p1.min(p2);
            let x_max = p1.max(p2);
            let range = x_max - x_min;
            let val = rng.range(x_min - alpha * range, x_max + alpha * range);
            val.clamp(bounds.0, bounds.1)
        };

        Self {
            neg_price_threshold: crossover_param(parent1.neg_price_threshold, parent2.neg_price_threshold, bounds.neg_price_threshold, rng),
            export_dump_threshold: crossover_param(parent1.export_dump_threshold, parent2.export_dump_threshold, bounds.export_dump_threshold, rng),
            dump_reserve_demand: crossover_param(parent1.dump_reserve_demand, parent2.dump_reserve_demand, bounds.dump_reserve_demand, rng),
            dump_reserve_normal: crossover_param(parent1.dump_reserve_normal, parent2.dump_reserve_normal, bounds.dump_reserve_normal, rng),
            pre_charge_price_threshold: crossover_param(parent1.pre_charge_price_threshold, parent2.pre_charge_price_threshold, bounds.pre_charge_price_threshold, rng),
            pre_charge_soc_limit: crossover_param(parent1.pre_charge_soc_limit, parent2.pre_charge_soc_limit, bounds.pre_charge_soc_limit, rng),
            pre_charge_start_hour: crossover_param(parent1.pre_charge_start_hour, parent2.pre_charge_start_hour, bounds.pre_charge_start_hour, rng),
            use_adaptive_shaving: crossover_param(parent1.use_adaptive_shaving, parent2.use_adaptive_shaving, bounds.use_adaptive_shaving, rng),
            adaptive_safety_buffer: crossover_param(parent1.adaptive_safety_buffer, parent2.adaptive_safety_buffer, bounds.adaptive_safety_buffer, rng),
            forecast_solar_weight: crossover_param(parent1.forecast_solar_weight, parent2.forecast_solar_weight, bounds.forecast_solar_weight, rng),
            tier2_export_dump_threshold: crossover_param(parent1.tier2_export_dump_threshold, parent2.tier2_export_dump_threshold, bounds.tier2_export_dump_threshold, rng),
            tier2_dump_reserve: crossover_param(parent1.tier2_dump_reserve, parent2.tier2_dump_reserve, bounds.tier2_dump_reserve, rng),
        }
    }

    pub fn to_evolved_config(&self) -> EvolvedHeuristicConfig {
        EvolvedHeuristicConfig {
            neg_price_threshold: (self.neg_price_threshold * 100.0).round() / 100.0,
            export_dump_threshold: (self.export_dump_threshold * 100.0).round() / 100.0,
            dump_reserve_demand: (self.dump_reserve_demand * 100.0).round() / 100.0,
            dump_reserve_normal: (self.dump_reserve_normal * 100.0).round() / 100.0,
            pre_charge_price_threshold: (self.pre_charge_price_threshold * 100.0).round() / 100.0,
            pre_charge_soc_limit: (self.pre_charge_soc_limit * 100.0).round() / 100.0,
            pre_charge_start_hour: self.pre_charge_start_hour.round() as u32,
            use_adaptive_shaving: self.use_adaptive_shaving.round() >= 0.5,
            adaptive_safety_buffer: (self.adaptive_safety_buffer * 100.0).round() / 100.0,
            forecast_solar_weight: (self.forecast_solar_weight * 1000.0).round() / 1000.0,
            tier2_export_dump_threshold: (self.tier2_export_dump_threshold * 100.0).round() / 100.0,
            tier2_dump_reserve: (self.tier2_dump_reserve * 100.0).round() / 100.0,
        }
    }

    pub fn from_evolved_config(cfg: &EvolvedHeuristicConfig) -> Self {
        Self {
            neg_price_threshold: cfg.neg_price_threshold,
            export_dump_threshold: cfg.export_dump_threshold,
            dump_reserve_demand: cfg.dump_reserve_demand,
            dump_reserve_normal: cfg.dump_reserve_normal,
            pre_charge_price_threshold: cfg.pre_charge_price_threshold,
            pre_charge_soc_limit: cfg.pre_charge_soc_limit,
            pre_charge_start_hour: cfg.pre_charge_start_hour as f64,
            use_adaptive_shaving: if cfg.use_adaptive_shaving { 1.0 } else { 0.0 },
            adaptive_safety_buffer: cfg.adaptive_safety_buffer,
            forecast_solar_weight: cfg.forecast_solar_weight,
            tier2_export_dump_threshold: cfg.tier2_export_dump_threshold,
            tier2_dump_reserve: cfg.tier2_dump_reserve,
        }
    }
}

pub struct TuningProgressEvent {
    pub percent: f64,
    pub gen_num: u32,
    pub total_gens: u32,
    pub best_cost: f64,
    pub bill: f64,
    pub cycles: f64,
    pub log_line: String,
    pub done: bool,
    pub best_params: Option<EvolvedHeuristicConfig>,
    pub best_params_monthly: Option<HashMap<String, EvolvedHeuristicConfig>>,
}

pub fn run_tuning(
    records: &[SimRecord],
    sim_config_template: &SimConfig,
    generations: u32,
    population_size: u32,
    cycle_penalty: f64,
    seed_config: Option<EvolvedHeuristicConfig>,
    seed_config_monthly: Option<HashMap<String, EvolvedHeuristicConfig>>,
    progress_cb: Option<&(dyn Fn(TuningProgressEvent) -> bool + Send + Sync)>,
) -> Result<HashMap<String, EvolvedHeuristicConfig>, String> {
    lower_current_thread_priority();
    if records.is_empty() {
        return Err("No telemetry records found for tuning".to_string());
    }

    use std::collections::BTreeMap;
    let mut monthly_records: BTreeMap<u32, Vec<SimRecord>> = BTreeMap::new();
    for r in records {
        let m = r.dt_local.month();
        monthly_records.entry(m).or_default().push(r.clone());
    }

    let active_months: Vec<u32> = monthly_records.iter()
        .filter(|(_, recs)| recs.len() >= 24)
        .map(|(&m, _)| m)
        .collect();

    if active_months.is_empty() {
        return Err("No months found with sufficient telemetry data (>= 24 points) to train.".to_string());
    }

    let total_months = active_months.len();
    let mut tuned_monthly_params = HashMap::new();
    let mut accumulated_best_bill = 0.0;
    let mut accumulated_best_cycles = 0.0;
    let mut accumulated_best_cost = 0.0;

    let bounds = Bounds::new(sim_config_template.battery_capacity_kwh);
    let mut rng = SimpleRng::new(1337);

    for (month_idx, &month) in active_months.iter().enumerate() {
        let month_recs = &monthly_records[&month];
        let month_name = match month {
            1 => "Jan", 2 => "Feb", 3 => "Mar", 4 => "Apr",
            5 => "May", 6 => "Jun", 7 => "Jul", 8 => "Aug",
            9 => "Sep", 10 => "Oct", 11 => "Nov", 12 => "Dec",
            _ => "Unknown",
        };

        let month_seed = seed_config_monthly.as_ref()
            .and_then(|m: &HashMap<String, EvolvedHeuristicConfig>| m.get(&month.to_string()).cloned())
            .or_else(|| seed_config.clone());

        let mut population = Vec::with_capacity(population_size as usize);
        if let Some(ref seed) = month_seed {
            let seed_ind = Individual::from_evolved_config(seed);
            population.push(seed_ind.clone());
            
            for _ in 0..std::cmp::min(5, population_size - 1) {
                let mut clone = seed_ind.clone();
                clone.mutate(&mut rng, &bounds, 1.0);
                population.push(clone);
            }
        }

        while population.len() < population_size as usize {
            population.push(Individual::random(&mut rng, &bounds));
        }

        let mut best_ind = population[0].clone();
        let mut best_cost = f64::MAX;
        let mut best_bill = 0.0;
        let mut best_cycles = 0.0;

        for gen_num in 1..=generations {
            let num_cores = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4);
            let num_threads = if num_cores > 1 { num_cores - 1 } else { 1 };
                
            let chunk_size = (population.len() + num_threads - 1) / num_threads;
            let mut results = vec![(0.0, 0.0, 0.0); population.len()];
            
            std::thread::scope(|s| {
                let mut threads = Vec::new();
                let population_slice = &population;
                
                for (thread_id, chunk) in population_slice.chunks(chunk_size).enumerate() {
                    let records_ref = month_recs;
                    let template_ref = sim_config_template;
                    
                    let handle = s.spawn(move || {
                        lower_current_thread_priority();
                        let mut chunk_res = Vec::new();
                        for ind in chunk {
                             let mut sim_config = template_ref.clone();
                             sim_config.evolved_heuristic = ind.to_evolved_config();
                             let res = crate::simulation::evolved_heuristic::run(records_ref, &sim_config);
                             let cost = res.energy_cost + res.demand_charges + res.cycles * cycle_penalty;
                             let bill = res.energy_cost + res.demand_charges;
                             chunk_res.push((cost, bill, res.cycles));
                        }
                        (thread_id, chunk_res)
                    });
                    threads.push(handle);
                }
                
                for handle in threads {
                    if let Ok((thread_id, chunk_res)) = handle.join() {
                        let start_idx = thread_id * chunk_size;
                        for (i, val) in chunk_res.into_iter().enumerate() {
                            results[start_idx + i] = val;
                        }
                    }
                }
            });

            let mut gen_best_cost = f64::MAX;
            let mut gen_best_idx = 0;
            for (i, &(cost, _, _)) in results.iter().enumerate() {
                if cost < gen_best_cost {
                    gen_best_cost = cost;
                    gen_best_idx = i;
                }
            }

            if gen_best_cost < best_cost {
                best_cost = gen_best_cost;
                best_ind = population[gen_best_idx].clone();
                best_bill = results[gen_best_idx].1;
                best_cycles = results[gen_best_idx].2;
            }

            let progress_gens = (month_idx as f64) * (generations as f64) + (gen_num as f64);
            let total_gens_all = (total_months as f64) * (generations as f64);
            let percent = (progress_gens / total_gens_all) * 100.0;

            let log_line = format!(
                "[{}] Gen {}/{} | Month Cost: {:.2} | Total Bill: {:.2} | Total Cycles: {:.1} | {:.1}%",
                month_name, gen_num, generations, best_cost,
                accumulated_best_bill + best_bill,
                accumulated_best_cycles + best_cycles,
                percent
            );
            
            if let Some(ref cb) = progress_cb {
                let mut current_map = tuned_monthly_params.clone();
                current_map.insert(month.to_string(), best_ind.to_evolved_config());
                
                let should_continue = cb(TuningProgressEvent {
                    percent,
                    gen_num: progress_gens as u32,
                    total_gens: total_gens_all as u32,
                    best_cost: accumulated_best_cost + best_cost,
                    bill: accumulated_best_bill + best_bill,
                    cycles: accumulated_best_cycles + best_cycles,
                    log_line,
                    done: false,
                    best_params: Some(best_ind.to_evolved_config()),
                    best_params_monthly: Some(current_map),
                });
                if !should_continue {
                    return Err("Tuning cancelled by user".to_string());
                }
            }

            let mut pop_with_res: Vec<(Individual, f64)> = population.iter()
                .zip(results.iter())
                .map(|(ind, &(cost, _, _))| (ind.clone(), cost))
                .collect();
                
            pop_with_res.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

            let mut new_population = Vec::with_capacity(population_size as usize);
            new_population.push(pop_with_res[0].0.clone());
            new_population.push(pop_with_res[1].0.clone());

            let tournament_select = |pop: &[(Individual, f64)], rng: &mut SimpleRng| -> Individual {
                let mut best_candidate = &pop[rng.range(0.0, pop.len() as f64) as usize];
                for _ in 0..2 {
                    let candidate = &pop[rng.range(0.0, pop.len() as f64) as usize];
                    if candidate.1 < best_candidate.1 {
                        best_candidate = candidate;
                    }
                }
                best_candidate.0.clone()
            };

            while new_population.len() < population_size as usize {
                let parent1 = tournament_select(&pop_with_res, &mut rng);
                let parent2 = tournament_select(&pop_with_res, &mut rng);
                
                let mut child = Individual::crossover(&parent1, &parent2, &mut rng, &bounds);
                child.mutate(&mut rng, &bounds, 0.15);
                new_population.push(child);
            }

            population = new_population;
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        tuned_monthly_params.insert(month.to_string(), best_ind.to_evolved_config());
        accumulated_best_bill += best_bill;
        accumulated_best_cycles += best_cycles;
        accumulated_best_cost += best_cost;
    }

    if let Some(ref cb) = progress_cb {
        let _ = cb(TuningProgressEvent {
            percent: 100.0,
            gen_num: (total_months as u32) * generations,
            total_gens: (total_months as u32) * generations,
            best_cost: accumulated_best_cost,
            bill: accumulated_best_bill,
            cycles: accumulated_best_cycles,
            log_line: "Tuning successfully completed for all months.".to_string(),
            done: true,
            best_params: None,
            best_params_monthly: Some(tuned_monthly_params.clone()),
        });
    }

    Ok(tuned_monthly_params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulation::SimRecord;
    use chrono::{TimeZone, FixedOffset};

    #[test]
    fn test_rng_and_individual_mutation() {
        let mut rng = SimpleRng::new(42);
        let bounds = Bounds::new(10.0);
        let mut ind = Individual::random(&mut rng, &bounds);
        
        let initial_neg = ind.neg_price_threshold;
        ind.mutate(&mut rng, &bounds, 1.0);
        assert_ne!(ind.neg_price_threshold, initial_neg);
        
        // Crossover
        let parent2 = Individual::random(&mut rng, &bounds);
        let child = Individual::crossover(&ind, &parent2, &mut rng, &bounds);
        assert!(child.neg_price_threshold >= bounds.neg_price_threshold.0);
        assert!(child.neg_price_threshold <= bounds.neg_price_threshold.1);
    }

    #[test]
    fn test_run_tuning_execution_and_cancellation() {
        let tz = FixedOffset::east_opt(36000).unwrap();
        let dt = tz.with_ymd_and_hms(2026, 6, 26, 12, 0, 0).unwrap();

        let mut records = Vec::new();
        for h in 0..24 {
            let record_dt = dt.clone() + chrono::Duration::hours(h);
            records.push(SimRecord {
                timestamp: record_dt.timestamp(),
                dt_local: record_dt,
                solar_power_w: 1000.0,
                load_power_w: 500.0,
                import_price_cents: 20.0,
                export_price_cents: 8.0,
                duration_hours: 1.0,
                day_solar_kwh: 24.0,
            });
        }

        let sim_config_template = SimConfig {
            battery_capacity_kwh: 10.0,
            max_power_w: 3000.0,
            min_charge_pct: 20,
            max_charge_pct: 100,
            demand_window: None,
            demand_rate: 0.0,
            negative_export_prevent: false,
            low_price_charge: false,
            low_price_threshold: 0.0,
            high_price_discharge: false,
            high_price_threshold: 0.0,
            periods: vec![],
            evolved_heuristic: EvolvedHeuristicConfig::default(),
            evolved_heuristic_monthly: None,
        };

        // Test running 3 generations, population of 5
        let result = run_tuning(
            &records,
            &sim_config_template,
            3,
            5,
            1.0,
            None,
            None,
            Some(&|event| {
                assert!(event.gen_num <= 3);
                true // continue
            }),
        );
        assert!(result.is_ok());

        // Test cancellation
        let cancel_result = run_tuning(
            &records,
            &sim_config_template,
            3,
            5,
            1.0,
            None,
            None,
            Some(&|event| {
                if event.gen_num == 1 {
                    false // cancel
                } else {
                    true
                }
            }),
        );
        assert!(cancel_result.is_err());
        assert_eq!(cancel_result.err().unwrap(), "Tuning cancelled by user");
    }
}
