use crate::config::EvolvedHeuristicConfig;
use super::{SimRecord, SimConfig};

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
}

impl Bounds {
    pub fn new(battery_capacity: f64) -> Self {
        Self {
            neg_price_threshold: (-15.0, 5.0),
            export_dump_threshold: (10.0, 60.0),
            dump_reserve_demand: (0.0, battery_capacity),
            dump_reserve_normal: (0.0, battery_capacity),
            pre_charge_price_threshold: (0.0, 30.0),
            pre_charge_soc_limit: (0.0, 1.0),
            pre_charge_start_hour: (0.0, 15.0),
            use_adaptive_shaving: (0.0, 1.0),
            adaptive_safety_buffer: (-500.0, 1500.0),
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
}

pub fn run_tuning(
    records: &[SimRecord],
    sim_config_template: &SimConfig,
    generations: u32,
    population_size: u32,
    cycle_penalty: f64,
    seed_config: Option<EvolvedHeuristicConfig>,
    progress_cb: Option<&(dyn Fn(TuningProgressEvent) -> bool + Send + Sync)>,
) -> Result<EvolvedHeuristicConfig, String> {
    if records.is_empty() {
        return Err("No telemetry records found for tuning".to_string());
    }

    let _total_days = if records.len() >= 2 {
        let first = records.first().unwrap();
        let last = records.last().unwrap();
        ((last.timestamp - first.timestamp) as f64 / 86400.0).max(1.0)
    } else {
        30.0
    };

    let bounds = Bounds::new(sim_config_template.battery_capacity_kwh);
    let mut rng = SimpleRng::new(1337);

    let mut population = Vec::with_capacity(population_size as usize);
    if let Some(ref seed) = seed_config {
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
        let num_threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
            
        let chunk_size = (population.len() + num_threads - 1) / num_threads;
        let mut results = vec![(0.0, 0.0, 0.0); population.len()];
        
        std::thread::scope(|s| {
            let mut threads = Vec::new();
            let population_slice = &population;
            
            for (thread_id, chunk) in population_slice.chunks(chunk_size).enumerate() {
                let records_ref = records;
                let template_ref = sim_config_template;
                
                let handle = s.spawn(move || {
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

        let percent = (gen_num as f64 / generations as f64) * 100.0;
        let log_line = format!(
            "GEN_PROGRESS: {}/{} | BEST_COST: {:.2} | BILL: {:.2} | CYCLES: {:.1} | PERCENT: {:.1}",
            gen_num, generations, best_cost, best_bill, best_cycles, percent
        );
        
        if let Some(ref cb) = progress_cb {
            let should_continue = cb(TuningProgressEvent {
                percent,
                gen_num,
                total_gens: generations,
                best_cost,
                bill: best_bill,
                cycles: best_cycles,
                log_line,
                done: false,
                best_params: Some(best_ind.to_evolved_config()),
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
    }

    let final_params = best_ind.to_evolved_config();
    if let Some(ref cb) = progress_cb {
        let _ = cb(TuningProgressEvent {
            percent: 100.0,
            gen_num: generations,
            total_gens: generations,
            best_cost,
            bill: best_bill,
            cycles: best_cycles,
            log_line: "Tuning successfully completed.".to_string(),
            done: true,
            best_params: Some(final_params.clone()),
        });
    }

    Ok(final_params)
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
        };

        // Test running 3 generations, population of 5
        let result = run_tuning(
            &records,
            &sim_config_template,
            3,
            5,
            1.0,
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
