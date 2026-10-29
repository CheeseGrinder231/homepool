use crate::config::{Allowance, Counts, METRICS};
use std::collections::{BTreeMap, BTreeSet};

pub fn validate_allowances(allowances: &[Allowance], issue_prefix: &str) -> Result<(), String> {
    let mut ids = BTreeSet::new();
    for entry in allowances {
        let number = entry.issue.strip_prefix(issue_prefix).unwrap_or("");
        if entry.id.trim().is_empty() || !ids.insert(&entry.id) {
            return Err("allowances require nonempty, unique ids".into());
        }
        if !METRICS.contains(&entry.metric.as_str())
            || entry.amount == 0
            || entry.reason.trim().is_empty()
            || number.is_empty()
            || !number.bytes().all(|c| c.is_ascii_digit())
            || number.starts_with('0')
        {
            return Err(format!(
                "invalid allowance {}: require metric, positive amount, reason, and an issue URL in this repository",
                entry.id
            ));
        }
    }
    Ok(())
}

pub fn enforce(
    counts: &Counts,
    baseline: &Counts,
    allowances: &[Allowance],
    old_counts: &Counts,
    old_baseline: &Counts,
    old_allowances: &[Allowance],
) -> Result<(), String> {
    let old: BTreeMap<_, _> = old_allowances
        .iter()
        .map(|entry| (&entry.id, entry))
        .collect();
    for entry in allowances {
        if let Some(previous) = old.get(&entry.id)
            && *previous != entry
        {
            return Err(format!(
                "existing allowance {} is immutable; delete it when retired",
                entry.id
            ));
        }
    }
    let sum = |metric: &str, only_new: bool| -> Result<u64, String> {
        allowances
            .iter()
            .filter(|a| a.metric == metric && (!only_new || !old.contains_key(&a.id)))
            .try_fold(0_u64, |sum, a| {
                sum.checked_add(a.amount)
                    .ok_or_else(|| "allowance amount overflow".into())
            })
    };
    let mut errors = Vec::new();
    for metric in METRICS {
        let current = counts.get(metric);
        let limit = baseline.get(metric);
        if limit > old_baseline.get(metric) {
            errors.push(format!("ratchet: {metric} baseline may only fall"));
        }
        if current < limit {
            errors.push(format!("ratchet: lower {metric} baseline to {current}"));
        }
        if current.saturating_sub(limit) > sum(metric, false)? {
            errors.push(format!("ratchet: {metric} is {current}, baseline is {limit}; add issue-linked allowances for the excess"));
        }
        let increase = current.saturating_sub(old_counts.get(metric).max(limit));
        if increase > sum(metric, true)? {
            errors.push(format!("ratchet: {metric} increased by {increase}; new allowances in this change must cover the increase"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}
