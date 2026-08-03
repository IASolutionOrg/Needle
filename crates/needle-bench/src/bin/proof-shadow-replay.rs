use needle_bench::{HistoricalCostEvidence, ShadowReplaySource, run_shadow_replay};
use serde_json::Value;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    if let Err(error) = run() {
        eprintln!("proof-shadow-replay: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    let workspace = option_path(&arguments, "--workspace-root")
        .unwrap_or(env::current_dir()?)
        .canonicalize()?;
    let output = option_path(&arguments, "--output")
        .unwrap_or_else(|| workspace.join("target/proof-shadow-v04-r22/proof-shadow-report.json"));
    let output = absolute_from(&workspace, output);
    let artifact_root = output.parent().ok_or("output path must have a parent directory")?;
    fs::create_dir_all(artifact_root)?;

    let r15_root = workspace.join("target/cache-pilot-v03-r15");
    let r20_root = workspace.join("target/mutation-pilot-v03-r20");
    let r21_root = workspace.join("target/product-pilot-locate-v03-r21");
    let r15_report = r15_root.join("cache-pilot-report-v2.json");
    let r20_report = r20_root.join("mutation-pilot-report.json");
    let r21_report = r21_root.join("pilot-report.json");
    let r21_observations = r21_root.join("product-observations.jsonl");
    for report in [&r15_report, &r20_report, &r21_report] {
        require_passed_report(report)?;
    }
    let pricing_digests = pricing_digests(&[&r15_report, &r20_report], &r21_observations)?;
    if pricing_digests.len() != 1 {
        return Err(format!(
            "expected exactly one pricing snapshot, observed {}",
            pricing_digests.len()
        )
        .into());
    }

    let (r15_main, r15_worker) = cache_report_costs(&r15_report, &["publication", "exact"])?;
    let (r20_main, r20_worker) =
        cache_report_costs(&r20_report, &["publication", "irrelevant", "relevant"])?;
    let (r21_main, r21_worker, r21_repairs) = observation_costs(&r21_observations)?;
    let main_microcredits = r15_main
        .checked_add(r20_main)
        .and_then(|value| value.checked_add(r21_main))
        .ok_or("historical main cost overflow")?;
    let worker_microcredits = r15_worker
        .checked_add(r20_worker)
        .and_then(|value| value.checked_add(r21_worker))
        .ok_or("historical worker cost overflow")?;
    let total_microcredits = main_microcredits
        .checked_add(worker_microcredits)
        .ok_or("historical total cost overflow")?;

    let report = run_shadow_replay(
        &[
            ShadowReplaySource {
                run_id: "r15".to_owned(),
                route: "trace.state-flow".to_owned(),
                product_data: r15_root.join("product-data"),
                repository_root: r15_root.join("repo"),
                report_path: r15_report,
                recorded_cases: 1,
            },
            ShadowReplaySource {
                run_id: "r20".to_owned(),
                route: "trace.state-flow".to_owned(),
                product_data: r20_root.join("product-data"),
                repository_root: r20_root.join("repo"),
                report_path: r20_report,
                recorded_cases: 2,
            },
            ShadowReplaySource {
                run_id: "r21".to_owned(),
                route: "locate.implementation".to_owned(),
                product_data: r21_root.join("n1/product-data"),
                repository_root: r21_root.join("n1/repo"),
                report_path: r21_report,
                recorded_cases: 1,
            },
        ],
        &artifact_root.join("scratch"),
        HistoricalCostEvidence {
            main_microcredits,
            worker_microcredits,
            repair_microcredits: Some(r21_repairs),
            escalation_microcredits: None,
            total_microcredits,
            pricing_snapshot_digest: pricing_digests
                .into_iter()
                .next()
                .expect("one pricing digest"),
        },
    )?;
    fs::write(&output, serde_json::to_vec_pretty(&report)?)?;
    println!("{}", output.display());
    Ok(())
}

fn option_path(arguments: &[String], option: &str) -> Option<PathBuf> {
    arguments
        .iter()
        .position(|argument| argument == option)
        .and_then(|index| arguments.get(index + 1))
        .map(PathBuf::from)
}

fn require_passed_report(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let report: Value = serde_json::from_slice(&fs::read(path)?)?;
    if report.get("passed").and_then(Value::as_bool) != Some(true) {
        return Err(format!("{} is not a passing source report", path.display()).into());
    }
    Ok(())
}

fn absolute_from(workspace: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() { path } else { workspace.join(path) }
}

fn cache_report_costs(
    path: &Path,
    arms: &[&str],
) -> Result<(u64, u64), Box<dyn std::error::Error>> {
    let report: Value = serde_json::from_slice(&fs::read(path)?)?;
    let mut main = 0_u64;
    let mut worker = 0_u64;
    for arm in arms {
        let observation =
            report.get(*arm).ok_or_else(|| format!("{} has no `{arm}` arm", path.display()))?;
        main =
            main.checked_add(cost_value(observation, "main_cost")?).ok_or("main cost overflow")?;
        worker = worker
            .checked_add(optional_cost_value(observation, "worker_cost")?)
            .ok_or("worker cost overflow")?;
    }
    Ok((main, worker))
}

fn observation_costs(path: &Path) -> Result<(u64, u64, u64), Box<dyn std::error::Error>> {
    let mut main = 0_u64;
    let mut worker = 0_u64;
    let mut repair = 0_u64;
    for line in fs::read_to_string(path)?.lines().filter(|line| !line.trim().is_empty()) {
        let observation: Value = serde_json::from_str(line)?;
        main =
            main.checked_add(cost_value(&observation, "main_cost")?).ok_or("main cost overflow")?;
        let worker_cost = optional_cost_value(&observation, "worker_cost")?;
        worker = worker.checked_add(worker_cost).ok_or("worker cost overflow")?;
        if observation.get("repair_performed").and_then(Value::as_bool).unwrap_or(false) {
            repair = repair.checked_add(worker_cost).ok_or("repair cost overflow")?;
        }
    }
    Ok((main, worker, repair))
}

fn cost_value(observation: &Value, field: &str) -> Result<u64, Box<dyn std::error::Error>> {
    observation
        .get(field)
        .and_then(|cost| cost.get("total_microcredits"))
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("missing `{field}.total_microcredits`").into())
}

fn optional_cost_value(
    observation: &Value,
    field: &str,
) -> Result<u64, Box<dyn std::error::Error>> {
    match observation.get(field) {
        None | Some(Value::Null) => Ok(0),
        Some(_) => cost_value(observation, field),
    }
}

fn pricing_digests(
    cache_reports: &[&Path],
    observations: &Path,
) -> Result<BTreeSet<String>, Box<dyn std::error::Error>> {
    let mut digests = BTreeSet::new();
    for path in cache_reports {
        collect_pricing_digests(&serde_json::from_slice(&fs::read(path)?)?, &mut digests);
    }
    for line in fs::read_to_string(observations)?.lines().filter(|line| !line.trim().is_empty()) {
        collect_pricing_digests(&serde_json::from_str(line)?, &mut digests);
    }
    Ok(digests)
}

fn collect_pricing_digests(value: &Value, output: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(digest) = map.get("pricing_snapshot_digest").and_then(Value::as_str) {
                output.insert(digest.to_owned());
            }
            for child in map.values() {
                collect_pricing_digests(child, output);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_pricing_digests(child, output);
            }
        }
        _ => {}
    }
}
