//! Fuzzy fallback for `edit` when `old_string` has no exact match.
//!
//! Compact port of Hermes `tools/fuzzy_match.py` (line-trimmed,
//! whitespace-normalized, indent-flexible, unicode-normalized) and Prime's
//! `edit-diff.ts` (smart quotes, dashes, whitespace). Whole-line, line-based
//! matching; applied only when the match is UNIQUE. The replacement is
//! re-indented to the matched block (as Hermes does).

fn unicode_norm(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            c => c,
        })
        .collect()
}

fn collapse(s: &str) -> String {
    unicode_norm(s).split_whitespace().collect::<Vec<_>>().join(" ")
}

fn indent_of(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// Find a unique whole-line fuzzy match of `old` in `content`.
/// Returns the byte range to replace and the re-indented `new`.
pub fn fuzzy_replace(content: &str, old: &str, new: &str) -> Option<(std::ops::Range<usize>, String)> {
    let trailing_nl = old.ends_with('\n');
    let old_lines: Vec<&str> = old.strip_suffix('\n').unwrap_or(old).split('\n').collect();
    if old_lines.iter().all(|l| l.trim().is_empty()) {
        return None;
    }
    // (start offset, line text without EOL, full length including EOL)
    let mut lines = Vec::new();
    let mut offset = 0;
    for raw in content.split_inclusive('\n') {
        lines.push((offset, raw.trim_end_matches(['\n', '\r']), raw.len()));
        offset += raw.len();
    }
    if lines.len() < old_lines.len() {
        return None;
    }
    let tiers: [fn(&str) -> String; 2] = [|s| s.trim().to_string(), collapse];
    for norm in tiers {
        let want: Vec<String> = old_lines.iter().map(|l| norm(l)).collect();
        let hits: Vec<usize> = (0..=lines.len() - old_lines.len())
            .filter(|&i| {
                lines[i..i + old_lines.len()]
                    .iter()
                    .zip(&want)
                    .all(|((_, text, _), w)| norm(text) == *w)
            })
            .collect();
        match hits.as_slice() {
            [] => continue,
            [i] => {
                let first = lines[*i];
                let last = lines[*i + old_lines.len() - 1];
                let end = if trailing_nl { last.0 + last.2 } else { last.0 + last.1.len() };
                let start = first.0;
                let old_first = old_lines.iter().find(|l| !l.trim().is_empty())?;
                let got_first = lines[*i..].iter().map(|l| l.1).find(|l| !l.trim().is_empty())?;
                let (old_ind, got_ind) = (indent_of(old_first), indent_of(got_first));
                let new_text = if old_ind == got_ind {
                    new.to_string()
                } else {
                    reindent(new, old_ind, got_ind)
                };
                return Some((start..end, new_text));
            }
            _ => return None, // ambiguous: keep the existing error path
        }
    }
    None
}

fn reindent(new: &str, from: &str, to: &str) -> String {
    new.split_inclusive('\n')
        .map(|l| match l.strip_prefix(from) {
            Some(rest) if !l.trim().is_empty() => format!("{to}{rest}"),
            _ => l.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(content: &str, old: &str, new: &str) -> Option<String> {
        let (r, n) = fuzzy_replace(content, old, new)?;
        Some(format!("{}{}{}", &content[..r.start], n, &content[r.end..]))
    }

    #[test]
    fn indent_flexible_reindents() {
        let c = "fn a() {\n        let x = 1;\n        foo(x);\n}\n";
        let out = apply(c, "let x = 1;\nfoo(x);", "let x = 2;\nfoo(x);").unwrap();
        assert_eq!(out, "fn a() {\n        let x = 2;\n        foo(x);\n}\n");
    }

    #[test]
    fn smart_quotes_and_whitespace() {
        let c = "say(\u{201C}hi\u{201D});  // a\tb\n";
        assert!(apply(c, "say(\"hi\"); // a b", "say(\"yo\");").is_some());
    }

    #[test]
    fn ambiguous_or_missing_is_none() {
        assert!(apply("  a\n    a\n", "a", "b").is_none());
        assert!(apply("x\n", "y", "z").is_none());
    }

    #[test]
    fn trailing_newline_preserved() {
        let out = apply("  a\n  b\n", "a\n", "c\n").unwrap();
        assert_eq!(out, "  c\n  b\n");
    }
}
