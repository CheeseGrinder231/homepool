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
        if name.starts_with("xtask/tests/fixtures/")
            || name.starts_with("docs/")
            || relative.extension().is_some_and(|ext| ext == "md")
            || relative.file_name().is_some_and(|file| file == "LICENSE")
            || relative.components().any(|c| c.as_os_str() == ".git")
        {
            continue;
        }
        let rust = relative.extension().is_some_and(|ext| ext == "rs");
        let manifest = relative
            .file_name()
            .is_some_and(|file| file == "Cargo.toml");
        let bytes = fs::read(&path).map_err(|e| format!("{name}: {e}"))?;
        let Ok(source) = std::str::from_utf8(&bytes) else {
            continue;
        };
        if source.contains('\0') {
            continue;
        }
        let mut spans: Vec<_> = if rust {
            crate::rust_text::ranges(source)
                .into_iter()
                .map(|span| (span, false))
                .collect()
        } else {
            std::iter::once((0..source.len(), false)).collect()
        };
        if rust {
            spans.push((0..source.len(), true));
        }
        let projections = if rust {
            crate::rust_text::field_projections(source)
        } else {
            vec![]
        };
        let metadata_spans = if manifest {
            canonical_repository_spans(source, repository)?
        } else if relative == Path::new("Cargo.lock") {
            registry_spans(source)?
        } else {
            vec![]
        };
        for (span, ipv4_only) in &spans {
            let mut offset = span.start;
            for line in source[span.clone()].split_inclusive('\n') {
                let candidates = if *ipv4_only {
                    ipv4_in_line(line)
                } else {
                    in_line(line)
                };
                for (column, category) in candidates {
                    let position = offset + column;
                    if *ipv4_only && projections.iter().any(|span| span.contains(&position)) {
                        continue;
                    }
                    if *ipv4_only
                        && spans
                            .iter()
                            .any(|(text, only)| !only && text.contains(&position))
                    {
                        continue;
                    }
                    if category == "URL literal"
                        && metadata_spans.iter().any(|span| span.contains(&position))
                    {
                        continue;
                    }
                    let preceding = &source[..position];
                    let line = preceding.bytes().filter(|b| *b == b'\n').count() + 1;
                    let column = position - preceding.rfind('\n').map_or(0, |n| n + 1) + 1;
                    findings.push(format!("{name}:{line}:{column}: {category}"));
                }
                offset += line.len();
            }
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
    findings.extend(
        ipv4_in_line(line)
            .into_iter()
            .filter(|(column, _)| !ipv6_spans.iter().any(|span| span.contains(column))),
    );
    for scheme in ["http", "https"] {
        let prefix = format!("{scheme}://");
        for (column, _) in line.to_ascii_lowercase().match_indices(&prefix) {
            findings.push((column, "URL literal"));
        }
    }
    for (column, path) in tokens(line, |c| {
        c.is_alphanumeric() || matches!(c, '/' | '\\' | ':' | '.' | '_' | '-' | '~' | '%' | '+')
    }) {
        if absolute_home(path.trim_end_matches('.')) {
            findings.push((column, "absolute home path"));
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

fn absolute_home(path: &str) -> bool {
    let account = |name: &str| {
        name.chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
    };
    let root = format!("/{}", "root");
    if path == root
        || path.starts_with(&format!("{root}/"))
        || ["home", "Users"]
            .iter()
            .any(|base| path.strip_prefix(&format!("/{base}/")).is_some_and(account))
    {
        return true;
    }
    let bytes = path.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
        return false;
    }
    let rest = path[2..].replace('\\', "/").to_ascii_lowercase();
    rest.starts_with('/')
        && rest
            .trim_start_matches('/')
            .strip_prefix("users/")
            .is_some_and(|name| account(name.trim_start_matches('/')))
}

fn registry_spans(source: &str) -> Result<Vec<std::ops::Range<usize>>, String> {
    #[derive(serde::Deserialize)]
    struct Package {
        source: Option<toml::Spanned<String>>,
    }
    #[derive(serde::Deserialize)]
    struct Lockfile {
        package: Vec<Package>,
    }
    let lock: Lockfile = toml::from_str(source).map_err(|e| format!("Cargo.lock: {e}"))?;
    let registry = format!(
        "registry+{}://github.com/rust-lang/crates.io-index",
        "https"
    );
    Ok(lock
        .package
        .into_iter()
        .filter_map(|p| p.source)
        .filter(|s| s.get_ref() == &registry)
        .map(|s| s.span())
        .collect())
}

fn ipv4_in_line(line: &str) -> Vec<(usize, &'static str)> {
    tokens(line, |c| c.is_ascii_digit() || c == '.')
        .into_iter()
        .filter(|(column, token)| {
            bounded(line, *column, token.len())
                && token
                    .trim_matches('.')
                    .parse::<Ipv4Addr>()
                    .is_ok_and(|ip| !documentation(ip.into()))
                && !version_context(line, *column)
        })
        .map(|(column, _)| (column, "IPv4 literal"))
        .collect()
}

// Recognize version-shaped tokens, not arbitrary four-part numbers. This is a
// text heuristic: an address written as version metadata is intentionally exempt.
fn version_context(line: &str, column: usize) -> bool {
    tokens(line, |c| {
        c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+')
    })
    .into_iter()
    .filter(|(start, token)| (*start..start + token.len()).contains(&column))
    .any(|(_, token)| {
        let (version, build) = token.split_once('+').unwrap_or((token, ""));
        let (core, pre) = version.split_once('-').unwrap_or((version, ""));
        let numbers: Vec<_> = core.split('.').collect();
        let identifiers = |text: &str| {
            !text.is_empty()
                && text.split('.').all(|part| {
                    !part.is_empty() && part.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
                })
        };
        numbers.len() == 3
            && numbers
                .iter()
                .all(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
            && (!version.contains('-') || identifiers(pre))
            && (!token.contains('+') || identifiers(build))
    })
}
