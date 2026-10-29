mod config;
mod history;
mod layers;
pub mod literals;
mod ratchet;
mod rust_context;

use config::{Allowances, Counts, Layers, METRICS};
use std::path::{Path, PathBuf};

#[derive(Default)]
struct Measurement {
    counts: Counts,
    baseline: Counts,
    allowances: Allowances,
    findings: Vec<String>,
}

fn measure(root: &Path, files: &[PathBuf], repository: &str) -> Result<Measurement, String> {
    let layers: Layers = config::read(root, "structure/layers.toml")?;
    let baseline = config::read(root, "structure/baseline.toml")?;
    let allowances: Allowances = config::read(root, "structure/allowances.toml")?;
    let layer_findings = layers::violations(&layers::metadata(root)?, &layers)?;
    let literal_findings = literals::scan(root, files, repository)?;
    let counts = Counts {
        layer_violations: layer_findings.len() as u64,
        instance_literals: literal_findings.len() as u64,
        allowance_count: allowances.allowances.len() as u64,
    };
    let findings = layer_findings.into_iter().chain(literal_findings).collect();
    Ok(Measurement {
        counts,
        baseline,
        allowances,
        findings,
    })
}

pub fn check(root: &Path, requested_base: Option<&str>) -> Result<(), String> {
    let revision = history::base(root, requested_base)?;
    let repository = history::repository_url(root)?;
    let prefix = format!("{repository}/issues/");
    let current = measure(root, &history::working_files(root)?, &repository)?;
    let snapshot = match revision {
        history::Base::Initial => history::Snapshot::empty()?,
        history::Base::Commit(revision) => history::Snapshot::create(root, &revision)?,
    };
    let old = if snapshot
        .files
        .iter()
        .any(|path| path == Path::new("Cargo.toml"))
    {
        measure(snapshot.root(), &snapshot.files, &repository)?
    } else {
        Measurement::default()
    };
    for finding in &current.findings {
        println!("{finding}");
    }
    for metric in METRICS {
        println!("{metric} = {}", current.counts.get(metric));
    }
    ratchet::validate_allowances(&current.allowances.allowances, &prefix)?;
    ratchet::enforce(
        &current.counts,
        &current.baseline,
        &current.allowances.allowances,
        &old.counts,
        &old.baseline,
        &old.allowances.allowances,
    )
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod regressions;
