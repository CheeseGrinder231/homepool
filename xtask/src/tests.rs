use super::*;
use config::Allowance;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, Ipv6Addr},
};

fn allowance(id: &str, metric: &str, amount: u64) -> Allowance {
    Allowance {
        id: id.into(),
        metric: metric.into(),
        amount,
        reason: "temporary exception".into(),
        issue: format!("{}://{}/issues/1", "https", "repository"),
    }
}

#[test]
fn ratchet_fixture_cases() {
    #[derive(Deserialize)]
    struct Cases {
        cases: Vec<Case>,
    }
    #[derive(Deserialize)]
    struct Case {
        name: String,
        current: u64,
        baseline: u64,
        old_count: u64,
        old_baseline: u64,
        allowance: u64,
        expected: String,
    }
    let cases: Cases = toml::from_str(include_str!("../tests/fixtures/ratchet.toml")).unwrap();
    for case in cases.cases {
        let allowances = if case.allowance == 0 {
            vec![]
        } else {
            vec![
                allowance("literal", "instance_literals", case.allowance),
                allowance("budget", "allowance_count", 2),
            ]
        };
        let current = Counts {
            instance_literals: case.current,
            allowance_count: allowances.len() as u64,
            ..Counts::default()
        };
        let baseline = Counts {
            instance_literals: case.baseline,
            ..Counts::default()
        };
        let old_counts = Counts {
            instance_literals: case.old_count,
            ..Counts::default()
        };
        let old_baseline = Counts {
            instance_literals: case.old_baseline,
            ..Counts::default()
        };
        let result = ratchet::enforce(
            &current,
            &baseline,
            &allowances,
            &old_counts,
            &old_baseline,
            &[],
        );
        if case.expected.is_empty() {
            assert!(result.is_ok(), "{}: {result:?}", case.name);
        } else {
            assert!(
                result.unwrap_err().contains(&case.expected),
                "{}",
                case.name
            );
        }
    }
}

#[test]
fn existing_allowances_are_immutable_and_cannot_fund_an_increase() {
    let old = vec![
        allowance("literal", "instance_literals", 9),
        allowance("budget", "allowance_count", 2),
    ];
    let counts = Counts {
        instance_literals: 1,
        allowance_count: 2,
        ..Counts::default()
    };
    let increased = Counts {
        instance_literals: 2,
        allowance_count: 2,
        ..Counts::default()
    };
    assert!(
        ratchet::enforce(
            &increased,
            &Counts::default(),
            &old,
            &counts,
            &Counts::default(),
            &old
        )
        .unwrap_err()
        .contains("new allowances")
    );
    let mut edited = old.clone();
    edited[0].reason = "changed".into();
    assert!(
        ratchet::enforce(
            &counts,
            &Counts::default(),
            &edited,
            &counts,
            &Counts::default(),
            &old
        )
        .unwrap_err()
        .contains("immutable")
    );
    assert!(
        ratchet::enforce(
            &Counts::default(),
            &Counts::default(),
            &[],
            &counts,
            &Counts::default(),
            &old
        )
        .is_ok()
    );
}

#[test]
fn allowance_entries_also_need_coverage() {
    let entries = [allowance("literal", "instance_literals", 1)];
    let counts = Counts {
        instance_literals: 1,
        allowance_count: 1,
        ..Counts::default()
    };
    let error = ratchet::enforce(
        &counts,
        &Counts::default(),
        &entries,
        &Counts::default(),
        &Counts::default(),
        &[],
    )
    .unwrap_err();
    assert!(error.contains("allowance_count"));
}

#[test]
fn allowance_validation_rejects_bad_ids_metrics_amounts_and_issues() {
    let prefix = format!("{}://{}/issues/", "https", "repository");
    let entry = allowance("valid", "instance_literals", 1);
    assert!(ratchet::validate_allowances(std::slice::from_ref(&entry), &prefix).is_ok());
    assert!(ratchet::validate_allowances(&[entry.clone(), entry.clone()], &prefix).is_err());
    for field in ["id", "metric", "amount", "reason", "issue"] {
        let mut bad = entry.clone();
        match field {
            "id" => bad.id.clear(),
            "metric" => bad.metric = "unknown".into(),
            "amount" => bad.amount = 0,
            "reason" => bad.reason.clear(),
            "issue" => bad.issue.push_str("/extra"),
            _ => unreachable!(),
        }
        assert!(ratchet::validate_allowances(&[bad], &prefix).is_err());
    }
}

#[test]
fn literal_inputs_cover_addresses_urls_paths_and_emails() {
    let v4 = Ipv4Addr::new(10, 20, 30, 40).to_string();
    let v6 = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1).to_string();
    for input in [
        &v4,
        &format!("[{v6}]:443"),
        &format!("{v4}:80"),
        &format!("{v6}/64"),
        &Ipv6Addr::UNSPECIFIED.to_string(),
    ] {
        assert!(!literals::in_line(input).is_empty(), "{input}");
    }
    for input in [
        "192.0.2.1",
        "198.51.100.4",
        "203.0.113.9",
        "2001:db8::1",
        "1.97.1",
        "999.999.999.999",
    ] {
        assert!(literals::in_line(input).is_empty(), "{input}");
    }
    for input in [
        format!("{}://192.0.2.1", "https"),
        format!("{}://[2001:db8::1]", "http"),
        format!("/{}/example/data", "home"),
        format!("/{}/example", "Users"),
        format!("{}@{}.{}", "example", "example", "invalid"),
    ] {
        assert_eq!(literals::in_line(&input).len(), 1, "{input}");
    }
    assert!(literals::in_line("é🙂 text std::fs::read core::fmt::Debug foo::<u8>").is_empty());
}

#[test]
fn scan_excludes_fixtures_and_reports_file_and_line() {
    // A Git snapshot gives each test its own scratch directory and automatic cleanup.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let snapshot = history::Snapshot::create(root, "HEAD").unwrap();
    let dir = snapshot.root().join("crates/example/src");
    std::fs::create_dir_all(&dir).unwrap();
    let input = format!("// placeholder\n{}\n", Ipv4Addr::new(10, 20, 30, 40));
    std::fs::write(dir.join("lib.rs"), &input).unwrap();
    let fixtures = snapshot.root().join("xtask/tests/fixtures");
    std::fs::create_dir_all(&fixtures).unwrap();
    std::fs::write(fixtures.join("Cargo.toml"), &input).unwrap();
    let findings = literals::scan(snapshot.root()).unwrap();
    assert!(
        findings
            .iter()
            .any(|f| f == "crates/example/src/lib.rs:2:1: IPv4 literal")
    );
    assert!(!findings.iter().any(|f| f.contains("fixtures")));
}

#[test]
fn actual_metadata_covers_dev_target_optional_and_renamed_edges() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/layers");
    let metadata = layers::metadata(&fixture).unwrap();
    let mut config = Layers {
        layers: BTreeMap::from([
            ("core".into(), vec![]),
            ("adapter".into(), vec!["core".into()]),
            ("app".into(), vec!["core".into(), "adapter".into()]),
        ]),
        crates: BTreeMap::from([
            ("fixture-core".into(), "core".into()),
            ("fixture-adapter-a".into(), "adapter".into()),
            ("fixture-adapter-b".into(), "adapter".into()),
            ("fixture-app".into(), "app".into()),
        ]),
    };
    let findings = layers::violations(&metadata, &config).unwrap();
    assert_eq!(findings.len(), 1);
    assert!(findings[0].contains("fixture-adapter-a (adapter) -> fixture-adapter-b (adapter)"));
    config.crates.insert("fixture-core".into(), "app".into());
    assert_eq!(layers::violations(&metadata, &config).unwrap().len(), 3);
    config.crates.remove("fixture-app");
    assert!(
        layers::violations(&metadata, &config)
            .unwrap_err()
            .contains("unassigned")
    );
}

#[test]
fn required_configuration_is_not_optional() {
    assert!(toml::from_str::<Counts>("instance_literals = 0").is_err());
    assert!(
        toml::from_str::<Counts>(
            "layer_violations = 0\ninstance_literals = 0\nallowance_count = 0\nextra = 0"
        )
        .is_err()
    );
}

#[test]
fn ipv4_mapped_ipv6_is_a_single_literal() {
    let address = Ipv4Addr::new(10, 20, 30, 40).to_ipv6_mapped().to_string();
    assert_eq!(literals::in_line(&address).len(), 1);
    assert!(literals::in_line("2001:db8::192.0.2.1").is_empty());
}
