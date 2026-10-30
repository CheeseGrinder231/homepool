use crate::{
    layers::Metadata,
    rust_text::{self, Token, closing},
    sources::Sources,
};
use ra_ap_rustc_lexer::TokenKind as T;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

type Module = Vec<String>;
type Graph = BTreeMap<Module, BTreeSet<Module>>;

#[derive(Default)]
pub struct Cycles {
    pub count: u64,
    pub findings: Vec<String>,
}

#[derive(Default)]
struct Tree {
    graph: Graph,
    paths: Vec<(Module, Module)>,
    visited: BTreeSet<PathBuf>,
}

impl Tree {
    fn file(&mut self, sources: &Sources, file: &Path, module: Module, directory: &Path) {
        // Invalid recursive file inclusion is Cargo's concern; stop walking it here.
        if !self.visited.insert(file.into()) {
            return;
        }
        let Some(bytes) = sources.get(file) else {
            return;
        };
        let Ok(source) = std::str::from_utf8(bytes) else {
            return;
        };
        self.graph.entry(module.clone()).or_default();
        self.walk(sources, &rust_text::code_tokens(source), module, directory);
    }

    fn walk(&mut self, sources: &Sources, tokens: &[Token<'_>], module: Module, directory: &Path) {
        let mut i = 0;
        let mut block_depth = 0;
        while i < tokens.len() {
            if tokens[i].text == "mod"
                && let Some(name) = tokens
                    .get(i + 1)
                    .filter(|t| matches!(t.kind, T::Ident | T::RawIdent))
            {
                let name = name.text.trim_start_matches("r#");
                let mut child = module.clone();
                // Blocks do not add a `super` level, but their module declarations
                // must not share an identity with module-level or other local names.
                child.push(if block_depth == 0 {
                    name.into()
                } else {
                    format!("{name}[local@{}]", tokens[i].start)
                });
                match tokens.get(i + 2).map(|t| t.text) {
                    Some("{") => {
                        if let Some(end) = closing(tokens, i + 2) {
                            self.graph.entry(child.clone()).or_default();
                            self.walk(sources, &tokens[i + 3..end], child, &directory.join(name));
                            i = end + 1;
                            continue;
                        }
                    }
                    Some(";") => {
                        let flat = directory.join(format!("{name}.rs"));
                        let nested = directory.join(name).join("mod.rs");
                        let file = if sources.contains_key(&flat) {
                            flat
                        } else {
                            nested
                        };
                        self.file(sources, &file, child, &directory.join(name));
                        i += 3;
                        continue;
                    }
                    _ => {}
                }
            }
            if matches!(tokens[i].text, "crate" | "super")
                && tokens.get(i + 1).is_some_and(|t| t.text == ":")
                && tokens.get(i + 2).is_some_and(|t| t.text == ":")
            {
                let mut prefix = if tokens[i].text == "crate" {
                    vec![]
                } else {
                    module.clone()
                };
                if tokens[i].text == "super" {
                    prefix.pop();
                }
                i = self.path(tokens, i + 3, &module, prefix);
                continue;
            }
            match tokens[i].text {
                "{" => block_depth += 1,
                "}" => block_depth -= 1,
                _ => {}
            }
            i += 1;
        }
    }

    // Expand nested grouped imports as well as ordinary paths.
    fn path(
        &mut self,
        tokens: &[Token<'_>],
        mut i: usize,
        from: &Module,
        mut prefix: Module,
    ) -> usize {
        while let Some(token) = tokens.get(i) {
            if token.text == "{" {
                if let Some(end) = closing(tokens, i) {
                    i += 1;
                    while i < end {
                        i = self.path(tokens, i, from, prefix.clone());
                        // Skip aliases and other tails to the next group member.
                        while i < end && tokens[i].text != "," {
                            i += 1;
                        }
                        i += 1;
                    }
                    return end + 1;
                }
                break;
            }
            if !matches!(token.kind, T::Ident | T::RawIdent) {
                break;
            }
            match token.text {
                "super" => {
                    prefix.pop();
                }
                "self" => {}
                _ => prefix.push(token.text.trim_start_matches("r#").into()),
            }
            i += 1;
            if tokens.get(i).is_some_and(|t| t.text == ":")
                && tokens.get(i + 1).is_some_and(|t| t.text == ":")
            {
                i += 2;
            } else {
                break;
            }
        }
        self.paths.push((from.clone(), prefix));
        i
    }

    fn resolve(&mut self) {
        for (from, mut to) in self.paths.drain(..) {
            while !self.graph.contains_key(&to) && !to.is_empty() {
                to.pop();
            }
            if to != from {
                self.graph.entry(from).or_default().insert(to);
            }
        }
    }
}

fn components(graph: &Graph) -> Vec<Vec<Module>> {
    let reachable: BTreeMap<_, _> = graph
        .keys()
        .map(|start| {
            let mut seen = BTreeSet::new();
            let mut pending = vec![start.clone()];
            while let Some(node) = pending.pop() {
                if seen.insert(node.clone())
                    && let Some(edges) = graph.get(&node)
                {
                    pending.extend(edges.iter().cloned());
                }
            }
            (start.clone(), seen)
        })
        .collect();
    let mut used = BTreeSet::new();
    let mut result = Vec::new();
    for (start, reaches) in &reachable {
        if used.contains(start) {
            continue;
        }
        let component: Vec<_> = reaches
            .iter()
            .filter(|node| {
                reachable
                    .get(*node)
                    .is_some_and(|back| back.contains(start))
            })
            .cloned()
            .collect();
        used.extend(component.iter().cloned());
        if component.len() > 1 {
            result.push(component);
        }
    }
    result
}

pub fn scan(root: &Path, sources: &Sources, metadata: &Metadata) -> Result<Cycles, String> {
    let mut result = Cycles::default();
    for package in metadata
        .packages
        .iter()
        .filter(|p| metadata.workspace_members.contains(&p.id))
    {
        let mut largest = 0;
        for target in &package.targets {
            let file = target
                .src_path
                .strip_prefix(root)
                .map_err(|_| "target source must be inside workspace")?;
            let mut tree = Tree::default();
            tree.file(
                sources,
                file,
                vec![],
                file.parent().unwrap_or(Path::new("")),
            );
            tree.resolve();
            for component in components(&tree.graph) {
                largest = largest.max(component.len() as u64);
                let names: Vec<_> = component
                    .iter()
                    .map(|name| {
                        if name.is_empty() {
                            "crate".into()
                        } else {
                            format!("crate::{}", name.join(&":".repeat(2)))
                        }
                    })
                    .collect();
                result.findings.push(format!(
                    "module_cycles: {}/{}: {} mutually reachable modules: {}",
                    package.name,
                    target.name,
                    component.len(),
                    names.join(", ")
                ));
            }
        }
        result.count += largest;
    }
    Ok(result)
}
