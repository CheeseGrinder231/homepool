// Mark strings, character literals, and comments so Rust path syntax can be
// distinguished from address text without suppressing findings in prose.
pub fn text_mask(source: &str) -> Vec<bool> {
    let bytes = source.as_bytes();
    let mut mask = vec![false; bytes.len()];
    let mut index = 0;
    while index < bytes.len() {
        let end = if bytes[index..].starts_with(b"//") {
            source[index..]
                .find('\n')
                .map_or(bytes.len(), |n| index + n)
        } else if bytes[index..].starts_with(b"/*") {
            let mut depth = 1;
            let mut end = index + 2;
            while end < bytes.len() && depth > 0 {
                if bytes[end..].starts_with(b"/*") {
                    depth += 1;
                    end += 2;
                } else if bytes[end..].starts_with(b"*/") {
                    depth -= 1;
                    end += 2;
                } else {
                    end += 1;
                }
            }
            end
        } else if bytes[index] == b'r' {
            let mut quote = index + 1;
            while bytes.get(quote) == Some(&b'#') {
                quote += 1;
            }
            if bytes.get(quote) == Some(&b'"') {
                let terminator = format!("\"{}", "#".repeat(quote - index - 1));
                source[quote + 1..]
                    .find(&terminator)
                    .map_or(bytes.len(), |n| quote + 1 + n + terminator.len())
            } else {
                index + 1
            }
        } else if bytes[index] == b'"' {
            let mut end = index + 1;
            while end < bytes.len() {
                match bytes[end] {
                    b'\\' => end = (end + 2).min(bytes.len()),
                    b'"' => {
                        end += 1;
                        break;
                    }
                    _ => end += 1,
                }
            }
            end
        } else if bytes[index] == b'\'' {
            character_end(source, index).unwrap_or(index + 1)
        } else {
            index + 1
        };
        if end > index + 1 {
            mask[index..end].fill(true);
        }
        index = end;
    }
    mask
}

fn character_end(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut end = start + 1;
    if bytes.get(end) == Some(&b'\\') {
        end += 1;
        match bytes.get(end)? {
            b'u' if bytes.get(end + 1) == Some(&b'{') => {
                end += source[end..].find('}')? + 1;
            }
            b'x' => end += 3,
            _ => end += 1,
        }
    } else {
        let ch = source[end..].chars().next()?;
        if matches!(ch, '\n' | '\'') {
            return None;
        }
        end += ch.len_utf8();
    }
    (bytes.get(end) == Some(&b'\'')).then_some(end + 1)
}

pub fn is_path(line: &str, column: usize) -> bool {
    let token: String = line[column..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit() || *c == ':')
        .collect();
    let separator = ":".repeat(2);
    token.contains(&separator)
        && token.split(&separator).all(|part| {
            part.is_empty()
                || (part.starts_with(|c: char| c.is_ascii_alphabetic())
                    && part.chars().all(|c| c.is_ascii_alphanumeric()))
        })
}
