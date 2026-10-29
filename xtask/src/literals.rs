use std::{
    fs,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::Path,
};

pub fn scan(
    root: &Path,
    files: &[std::path::PathBuf],
    repository: &str,
) -> Result<Vec<String>, String> {
    let mut findings = Vec::new();
    for relative in files {
        let name = relative.to_string_lossy().replace('\\', "/");
        if name.starts_with("xtask/tests/fixtures/")
            || relative
                .components()
                .any(|c| c.as_os_str() == "target" || c.as_os_str() == ".git")
        {
            continue;
        }
        let rust = relative.extension().is_some_and(|ext| ext == "rs");
        let manifest = relative
            .file_name()
            .is_some_and(|file| file == "Cargo.toml");
        if !rust && !manifest {
            continue;
        }
        let path = root.join(relative);
        let mut ancestor = root.to_path_buf();
        for component in relative.components() {
            ancestor.push(component);
            if fs::symlink_metadata(&ancestor).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(format!("source scan does not follow symlinks: {name}"));
            }
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("{name}: {error}")),
        };
        if !metadata.is_file() {
            return Err(format!("source scan requires a regular file: {name}"));
        }
        let source = fs::read_to_string(&path).map_err(|e| format!("{name}: {e}"))?;
        let text = if rust {
            crate::rust_context::text_mask(&source)
        } else {
            vec![]
        };
        let metadata_spans = if manifest {
            canonical_repository_spans(&source, repository)?
        } else {
            vec![]
        };
        let mut offset = 0;
        for (index, line) in source.split_inclusive('\n').enumerate() {
            for (column, category) in in_line(line) {
                let position = offset + column;
                if category == "IPv6 literal"
                    && rust
                    && !text[position]
                    && crate::rust_context::is_path(line, column)
                {
                    continue;
                }
                if category == "URL literal"
                    && metadata_spans.iter().any(|span| span.contains(&position))
                {
                    continue;
                }
                findings.push(format!("{name}:{}:{}: {category}", index + 1, column + 1));
            }
            offset += line.len();
        }
    }
    findings.sort();
    findings.dedup();
    Ok(findings)
}

// Only the repository field in package metadata may contain the exact canonical
// public repository URL. Other fields, source literals, and comments still count.
fn canonical_repository_spans(
    source: &str,
    repository: &str,
) -> Result<Vec<std::ops::Range<usize>>, String> {
    #[derive(serde::Deserialize, Default)]
    struct Package {
        repository: Option<toml::Spanned<toml::Value>>,
    }
    #[derive(serde::Deserialize, Default)]
    struct Workspace {
        package: Option<Package>,
    }
    #[derive(serde::Deserialize)]
    struct Manifest {
        package: Option<Package>,
        workspace: Option<Workspace>,
    }
    let manifest: Manifest = toml::from_str(source).map_err(|e| format!("Cargo.toml: {e}"))?;
    Ok(manifest
        .package
        .into_iter()
        .chain(manifest.workspace.and_then(|w| w.package))
        .filter_map(|p| p.repository)
        .filter(|value| value.get_ref().as_str() == Some(repository))
        .map(|value| value.span())
        .collect())
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
        // Strip sentence periods before trying a single punctuation colon;
        // preserve compressed address suffixes rather than trimming their colons.
        let candidate = token.trim_matches('.');
        let ip = candidate
            .parse::<Ipv6Addr>()
            .or_else(|_| candidate.strip_suffix(':').unwrap_or(candidate).parse());
        if bounded(line, column, token.len())
            && let Ok(ip) = ip
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
        for (column, _) in line.to_ascii_lowercase().match_indices(&prefix) {
            findings.push((column, "URL literal"));
        }
    }
    for prefix in [format!("/{}/", "home"), format!("/{}/", "Users")] {
        for (column, tail) in line.match_indices(&prefix) {
            if column >= 2
                && line.as_bytes()[column - 1] == b':'
                && line.as_bytes()[column - 2].is_ascii_alphabetic()
            {
                continue;
            }
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
    let root_home = format!("/{}", "root");
    for (column, _) in line.match_indices(&root_home) {
        if line[column + root_home.len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric() && !matches!(c, '_' | '-' | '.'))
        {
            findings.push((column, "absolute home path"));
        }
    }
    let lower = line.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    for column in 0..bytes.len().saturating_sub(2) {
        if bytes[column].is_ascii_alphabetic() && bytes[column + 1] == b':' {
            let rest = lower[column + 2..].trim_start_matches(['\\', '/']);
            if rest.len() == lower[column + 2..].len() {
                continue;
            }
            if let Some(rest) = rest.strip_prefix("users") {
                let user = rest.trim_start_matches(['\\', '/']);
                if user.len() != rest.len()
                    && user
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
                {
                    findings.push((column, "absolute home path"));
                }
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
