use crate::{layers::Metadata, rust_text};
use ra_ap_rustc_lexer::{LiteralKind as L, TokenKind as T};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

pub type Sources = BTreeMap<PathBuf, Vec<u8>>;

pub fn read(root: &Path, files: &[PathBuf]) -> Result<Sources, String> {
    let mut sources = Sources::new();
    for path in files {
        if path.extension().is_none_or(|ext| ext != "rs")
            || path.starts_with("xtask/tests/fixtures")
            || path.starts_with("docs")
        {
            continue;
        }
        // The literal scanner applies the shared inventory's symlink policy first.
        match fs::read(root.join(path)) {
            Ok(bytes) => {
                sources.insert(path.clone(), bytes);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(format!("cannot read source: {}", path.display())),
        }
    }
    Ok(sources)
}

pub fn copies(sources: &Sources) -> Vec<String> {
    let mut originals = BTreeMap::new();
    let mut findings = Vec::new();
    for (path, bytes) in sources.iter().filter(|(_, bytes)| !bytes.is_empty()) {
        if let Some(original) = originals.get(bytes) {
            findings.push(format!(
                "source_copies: {} copies {original}",
                path.display()
            ));
        } else {
            originals.insert(bytes, path.display().to_string());
        }
    }
    findings
}

pub fn settings(root: &Path, sources: &Sources, metadata: &Metadata) -> Vec<String> {
    let designated: Vec<_> = metadata
        .packages
        .iter()
        .filter(|p| metadata.workspace_members.contains(&p.id))
        .filter_map(|p| p.manifest_path.parent()?.strip_prefix(root).ok())
        .map(|p| p.join("src/settings.rs"))
        .collect();
    let mut findings = Vec::new();
    for (path, bytes) in sources {
        if designated.contains(path) {
            continue;
        }
        let Ok(source) = std::str::from_utf8(bytes) else {
            continue;
        };
        let tokens = rust_text::code_tokens(source);
        for (i, token) in tokens.iter().enumerate() {
            if !matches!(token.text, "let" | "const" | "static") {
                continue;
            }
            let mut next = i + 1;
            while tokens
                .get(next)
                .is_some_and(|t| matches!(t.text, "mut" | "ref"))
            {
                next += 1;
            }
            let Some(name) = tokens
                .get(next)
                .filter(|t| matches!(t.kind, T::Ident | T::RawIdent))
            else {
                continue;
            };
            let name_text = name.text.trim_start_matches("r#").to_ascii_lowercase();
            let family = [
                "timeout_ms",
                "timeout_secs",
                "interval_ms",
                "interval_secs",
                "retry_count",
                "size_limit",
                "size_limit_bytes",
                "port",
                "path",
            ]
            .into_iter()
            .find(|suffix| name_text == *suffix || name_text.ends_with(&format!("_{suffix}")));
            let Some(family) = family else {
                continue;
            };
            next += 1;
            // Only simple bindings, optionally with a type, and a direct literal initializer.
            if tokens.get(next).is_some_and(|t| t.text == ":") {
                next += 1;
                while tokens
                    .get(next)
                    .is_some_and(|t| !matches!(t.text, "=" | ";" | "{" | "}"))
                {
                    next += 1;
                }
            }
            if tokens.get(next).is_none_or(|t| t.text != "=") {
                continue;
            }
            let Some(value) = tokens.get(next + 1) else {
                continue;
            };
            let expected = match value.kind {
                T::Literal {
                    kind: L::Int { .. } | L::Float { .. },
                    ..
                } => family != "path",
                T::Literal {
                    kind: L::Str { .. } | L::RawStr { .. },
                    ..
                } => family == "path",
                _ => false,
            };
            if expected && tokens.get(next + 2).is_some_and(|t| t.text == ";") {
                let before = &source[..name.start];
                let line = before.bytes().filter(|b| *b == b'\n').count() + 1;
                let column = name.start - before.rfind('\n').map_or(0, |n| n + 1) + 1;
                findings.push(format!(
                    "{}:{line}:{column}: settings_in_code: {}",
                    path.display(),
                    name.text
                ));
            }
        }
    }
    findings
}
