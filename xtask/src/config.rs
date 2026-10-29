use serde::Deserialize;
use std::{collections::BTreeMap, fs, path::Path};

pub const METRICS: [&str; 3] = ["layer_violations", "instance_literals", "allowance_count"];

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Counts {
    pub layer_violations: u64,
    pub instance_literals: u64,
    pub allowance_count: u64,
}

impl Counts {
    pub fn get(&self, metric: &str) -> u64 {
        match metric {
            "layer_violations" => self.layer_violations,
            "instance_literals" => self.instance_literals,
            "allowance_count" => self.allowance_count,
            _ => unreachable!("validated metric"),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layers {
    pub layers: BTreeMap<String, Vec<String>>,
    pub crates: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Allowance {
    pub id: String,
    pub metric: String,
    pub amount: u64,
    pub reason: String,
    pub issue: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allowances {
    pub allowances: Vec<Allowance>,
}

pub fn read<T: serde::de::DeserializeOwned>(root: &Path, name: &str) -> Result<T, String> {
    let text = fs::read_to_string(root.join(name)).map_err(|e| format!("{name}: {e}"))?;
    toml::from_str(&text).map_err(|e| format!("{name}: {e}"))
}
