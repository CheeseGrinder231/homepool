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
