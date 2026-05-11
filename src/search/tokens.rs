//! BM25 tokenizer.
//!
//! Splits identifiers on snake_case (`my_func` → `my_func`, `my`, `func`) and
//! camelCase (`HandlerStack` → `handlerstack`, `handler`, `stack`), keeping
//! the original lowercase form alongside the parts so an exact-token query
//! (`my_func`) and a sub-token query (`func`) both hit. Sub-tokens come out
//! lowercased; the index is case-insensitive.

use once_cell::sync::Lazy;
use regex::Regex;

static TOKEN_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[a-zA-Z_][a-zA-Z0-9_]*").unwrap());

pub fn split_identifier(token: &str) -> Vec<String> {
    // ASCII fast path: ~99% of source-code identifiers are ASCII, and the
    // generic Unicode-aware `to_lowercase` allocates+walks per char which
    // costs ~3x the byte-level `make_ascii_lowercase`. The `is_ascii()`
    // check is one branch on the byte mask, well below the noise of the
    // allocation it gates.
    let lower = if token.is_ascii() {
        let mut s = token.to_owned();
        s.make_ascii_lowercase();
        s
    } else {
        token.to_lowercase()
    };
    let parts: Vec<String> = if token.contains('_') {
        lower
            .split('_')
            .filter(|p| !p.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    } else {
        split_camel(token)
    };

    if parts.len() >= 2 {
        let mut out = Vec::with_capacity(parts.len() + 1);
        out.push(lower);
        out.extend(parts);
        out
    } else {
        vec![lower]
    }
}

fn split_camel(token: &str) -> Vec<String> {
    let chars: Vec<(usize, char)> = token.char_indices().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let mut starts = vec![0usize];
    for i in 1..chars.len() {
        let prev = chars[i - 1].1;
        let current = chars[i].1;
        let next = chars.get(i + 1).map(|(_, c)| *c);
        let boundary = (prev.is_ascii_lowercase()
            && (current.is_ascii_uppercase() || current.is_ascii_digit()))
            || (prev.is_ascii_digit() && current.is_ascii_alphabetic())
            || (prev.is_ascii_uppercase()
                && current.is_ascii_uppercase()
                && next.is_some_and(|c| c.is_ascii_lowercase()));
        if boundary {
            starts.push(chars[i].0);
        }
    }
    starts.push(token.len());
    starts
        .windows(2)
        .filter_map(|w| {
            let part = &token[w[0]..w[1]];
            if part.is_empty() {
                return None;
            }
            // ASCII fast path mirrors `split_identifier` ~ the parts coming
            // out of `split_camel` are nearly always ASCII subwords of an
            // identifier, and the Unicode-aware `to_lowercase` allocator
            // is 2-3x slower than `to_ascii_lowercase` for them.
            Some(if part.is_ascii() {
                let mut s = part.to_owned();
                s.make_ascii_lowercase();
                s
            } else {
                part.to_lowercase()
            })
        })
        .collect()
}

pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for m in TOKEN_RE.find_iter(text) {
        out.extend(split_identifier(m.as_str()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{split_identifier, tokenize};

    #[test]
    fn tokenization_matches_identifier_expansion() {
        assert_eq!(
            split_identifier("HandlerStack"),
            vec!["handlerstack", "handler", "stack"]
        );
        assert_eq!(split_identifier("my_func"), vec!["my_func", "my", "func"]);
        assert_eq!(
            tokenize("getHTTPResponse my_func"),
            vec![
                "gethttpresponse",
                "get",
                "http",
                "response",
                "my_func",
                "my",
                "func"
            ]
        );
    }
}
