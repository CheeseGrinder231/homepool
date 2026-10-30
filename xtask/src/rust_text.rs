use ra_ap_rustc_lexer::{
    FrontmatterAllowed, LiteralKind as L, TokenKind as T, strip_shebang, tokenize,
};
use std::ops::Range;

/// Source ranges containing comment text, string contents, or a shebang.
/// Token lengths include suffixes; suffixes never start another literal here.
pub fn ranges(source: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut offset = if source.starts_with('\u{feff}') { 3 } else { 0 };
    if let Some(length) = strip_shebang(&source[offset..]) {
        ranges.push(offset..offset + length);
        offset += length;
    }
    for token in tokenize(&source[offset..], FrontmatterAllowed::Yes) {
        let text = &source[offset..offset + token.len as usize];
        let content = match token.kind {
            T::LineComment { .. } => Some(2..text.len()),
            T::BlockComment { terminated, .. } => Some(2..text.len() - 2 * usize::from(terminated)),
            T::Literal { kind, suffix_start } => {
                let closing = match kind {
                    L::Str { terminated } | L::ByteStr { terminated } | L::CStr { terminated } => {
                        Some(usize::from(terminated))
                    }
                    L::RawStr { n_hashes }
                    | L::RawByteStr { n_hashes }
                    | L::RawCStr { n_hashes } => Some(n_hashes.map_or(0, |n| n as usize + 1)),
                    _ => None,
                };
                closing.and_then(|closing| {
                    text.find('"')
                        .map(|quote| quote + 1..suffix_start as usize - closing)
                })
            }
            _ => None,
        };
        if let Some(content) = content.filter(|range| !range.is_empty()) {
            ranges.push(offset + content.start..offset + content.end);
        }
        offset += token.len as usize;
    }
    ranges
}

pub struct Token<'a> {
    pub kind: T,
    pub text: &'a str,
    pub start: usize,
}

/// Significant, unexpanded Rust tokens, with byte positions for diagnostics.
pub fn tokens(source: &str) -> Vec<Token<'_>> {
    let mut offset = usize::from(source.starts_with('\u{feff}')) * 3;
    offset += strip_shebang(&source[offset..]).unwrap_or(0);
    tokenize(&source[offset..], FrontmatterAllowed::Yes)
        .filter_map(|token| {
            let start = offset;
            offset += token.len as usize;
            (!matches!(
                token.kind,
                T::Whitespace | T::LineComment { .. } | T::BlockComment { .. }
            ))
            .then_some(Token {
                kind: token.kind,
                text: &source[start..offset],
                start,
            })
        })
        .collect()
}

pub fn closing(tokens: &[Token<'_>], start: usize) -> Option<usize> {
    let mut depth = 0;
    for (i, token) in tokens.iter().enumerate().skip(start) {
        match token.text {
            "{" | "(" | "[" => depth += 1,
            "}" | ")" | "]" => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Macro token trees are arbitrary input, not parsed Rust items or expressions.
pub fn code_tokens(source: &str) -> Vec<Token<'_>> {
    let tokens = tokens(source);
    let mut result = Vec::new();
    let mut iter = tokens.into_iter().peekable();
    while let Some(token) = iter.next() {
        if token.text == "!"
            && result.last().is_some_and(|t: &Token<'_>| {
                matches!(t.kind, T::Ident | T::RawIdent)
                    && !matches!(
                        t.text,
                        "return" | "if" | "else" | "while" | "match" | "yield" | "break"
                    )
            })
        {
            if result.last().is_some_and(|t| t.text == "macro_rules")
                && iter
                    .peek()
                    .is_some_and(|t| matches!(t.kind, T::Ident | T::RawIdent))
            {
                iter.next(); // The name in a macro_rules declaration.
            }
            if iter
                .peek()
                .is_some_and(|t| matches!(t.text, "{" | "(" | "["))
            {
                let mut depth = 0;
                for input in iter.by_ref() {
                    match input.text {
                        "{" | "(" | "[" => depth += 1,
                        "}" | ")" | "]" => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                continue;
            }
        }
        result.push(token);
    }
    result
}
