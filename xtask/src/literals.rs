use std::{
    fs,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::Path,
};

pub fn scan(root: &Path) -> Result<Vec<String>, String> {
    let mut findings = Vec::new();
    walk(root, root, &mut findings)?;
    findings.sort();
    Ok(findings)
}

fn walk(root: &Path, dir: &Path, findings: &mut Vec<String>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|e| format!("source directory: {e}"))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let relative = path.strip_prefix(root).map_err(|e| e.to_string())?;
        let name = relative.to_string_lossy().replace('\\', "/");
        if matches!(entry.file_name().to_str(), Some(".git" | "target"))
            || name == "xtask/tests/fixtures"
        {
            continue;
        }
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_symlink() {
            return Err(format!("source scan does not follow symlinks: {name}"));
        }
        if kind.is_dir() {
            walk(root, &path, findings)?;
        } else if entry.file_name() == "Cargo.toml"
            || (path.extension().is_some_and(|ext| ext == "rs")
                && (name.starts_with("crates/") || name.starts_with("xtask/src/")))
        {
            let source = fs::read_to_string(&path).map_err(|e| format!("{name}: {e}"))?;
            for (index, line) in source.lines().enumerate() {
                for (column, category) in in_line(line) {
                    findings.push(format!("{name}:{}:{}: {category}", index + 1, column + 1));
                }
            }
        }
    }
    Ok(())
}

fn documentation(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            (a == 192 && b == 0 && c == 2)
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113)
        }
        IpAddr::V6(ip) => ip.segments()[..2] == [0x2001, 0x0db8],
    }
}

fn tokens(line: &str, allowed: impl Fn(char) -> bool) -> Vec<(usize, &str)> {
    let mut result = Vec::new();
    let mut start = None;
    for (index, ch) in line
        .char_indices()
        .chain(std::iter::once((line.len(), '\0')))
    {
        if allowed(ch) {
            start.get_or_insert(index);
        } else if let Some(begin) = start.take() {
            result.push((begin, &line[begin..index]));
        }
    }
    result
}

fn bounded(line: &str, column: usize, length: usize) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    !line[..column].chars().next_back().is_some_and(word)
        && !line[column + length..].chars().next().is_some_and(word)
}

pub fn in_line(line: &str) -> Vec<(usize, &'static str)> {
    let mut findings = Vec::new();
    let mut ipv6_spans = Vec::new();
    for (column, token) in tokens(line, |c| c.is_ascii_hexdigit() || matches!(c, ':' | '.')) {
        // Colons adjacent to source punctuation must not hide an address.
        let candidate = if token.parse::<Ipv6Addr>().is_ok() {
            token
        } else {
            token.trim_end_matches(':')
        };
        if bounded(line, column, token.len())
            && let Ok(ip) = candidate.parse::<Ipv6Addr>()
        {
            ipv6_spans.push(column..column + token.len());
            if !documentation(ip.into()) {
                findings.push((column, "IPv6 literal"));
            }
        }
    }
    for (column, token) in tokens(line, |c| c.is_ascii_digit() || c == '.') {
        let candidate = token.trim_matches('.');
        if bounded(line, column, token.len())
            && !ipv6_spans.iter().any(|span| span.contains(&column))
            && candidate
                .parse::<Ipv4Addr>()
                .is_ok_and(|ip| !documentation(ip.into()))
        {
            findings.push((column, "IPv4 literal"));
        }
    }
    for scheme in ["http", "https"] {
        let prefix = format!("{scheme}://");
        for (column, _) in line.match_indices(&prefix) {
            findings.push((column, "URL literal"));
        }
    }
    for prefix in [format!("/{}/", "home"), format!("/{}/", "Users")] {
        for (column, tail) in line.match_indices(&prefix) {
            let rest = &line[column + tail.len()..];
            if rest
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
            {
                findings.push((column, "absolute home path"));
            }
        }
    }
    for (column, token) in tokens(line, |c| {
        c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '%' | '+' | '-')
    }) {
        let token = token.trim_matches('.');
        if let Some((local, domain)) = token.split_once('@') {
            let labels: Vec<_> = domain.split('.').collect();
            if !local.is_empty()
                && labels.len() >= 2
                && labels.iter().all(|label| {
                    !label.is_empty()
                        && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                })
                && labels.last().is_some_and(|label| {
                    label.len() >= 2 && label.chars().all(|c| c.is_ascii_alphabetic())
                })
            {
                findings.push((column, "email address"));
            }
        }
    }
    findings.sort();
    findings
}
