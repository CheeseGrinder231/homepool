use crate::config::Layers;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::Command,
};

#[derive(Debug, Deserialize)]
pub struct Metadata {
    pub packages: Vec<Package>,
    pub workspace_members: Vec<String>,
    pub resolve: Resolve,
}

#[derive(Debug, Deserialize)]
pub struct Package {
    pub id: String,
    pub name: String,
    pub manifest_path: std::path::PathBuf,
    pub targets: Vec<Target>,
}

#[derive(Debug, Deserialize)]
pub struct Target {
    pub name: String,
    pub src_path: std::path::PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct Resolve {
    pub nodes: Vec<Node>,
}

#[derive(Debug, Deserialize)]
pub struct Node {
    pub id: String,
    pub dependencies: Vec<String>,
}

pub fn metadata(root: &Path) -> Result<Metadata, String> {
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--format-version",
            "1",
            "--all-features",
            "--locked",
        ])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cargo metadata: {e}"))?;
    if !output.status.success() {
        // Cargo's diagnostics can include local absolute paths; do not relay them.
        return Err(
            "cargo metadata failed; run cargo metadata --format-version 1 locally for details"
                .into(),
        );
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("cargo metadata JSON: {e}"))
}

pub fn violations(metadata: &Metadata, config: &Layers) -> Result<Vec<String>, String> {
    let required = BTreeMap::from([
        ("core".into(), vec![]),
        ("adapter".into(), vec!["core".into()]),
        ("app".into(), vec!["core".into(), "adapter".into()]),
    ]);
    let normalized = |map: &BTreeMap<String, Vec<String>>| {
        map.iter()
            .map(|(k, v)| (k.clone(), v.iter().cloned().collect::<BTreeSet<_>>()))
            .collect::<BTreeMap<_, _>>()
    };
    if normalized(&config.layers) != normalized(&required) {
        return Err("structure/layers.toml must declare core -> [], adapter -> [core], app -> [core, adapter]".into());
    }
    let members: BTreeSet<_> = metadata.workspace_members.iter().collect();
    let packages: BTreeMap<_, _> = metadata
        .packages
        .iter()
        .filter(|p| members.contains(&p.id))
        .map(|p| (&p.id, &p.name))
        .collect();
    for name in config.crates.keys() {
        if !packages.values().any(|n| *n == name) {
            return Err(format!(
                "layer assignment names a non-workspace crate: {name}"
            ));
        }
    }
    for name in packages.values() {
        let layer = config
            .crates
            .get(*name)
            .ok_or_else(|| format!("unassigned workspace crate: {name}"))?;
        if !config.layers.contains_key(layer) {
            return Err(format!("unknown layer {layer} for {name}"));
        }
    }
    let mut findings = Vec::new();
    for node in &metadata.resolve.nodes {
        let Some(from) = packages.get(&node.id) else {
            continue;
        };
        let from_layer = &config.crates[*from];
        for dependency in &node.dependencies {
            let Some(to) = packages.get(dependency) else {
                continue;
            };
            let to_layer = &config.crates[*to];
            if !config.layers[from_layer].contains(to_layer) {
                findings.push(format!("layer: {from} ({from_layer}) -> {to} ({to_layer})"));
            }
        }
    }
    findings.sort();
    findings.dedup();
    Ok(findings)
}
