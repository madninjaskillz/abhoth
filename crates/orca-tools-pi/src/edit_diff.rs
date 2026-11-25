//! Port of Pi's edit-diff.ts (`PiEditDiff`): line-ending handling, exact-then-fuzzy matching,
//! and the position-based multi-edit application that rewrites only the lines a fuzzy match
//! touches. C# works on UTF-16 indices and Rust on UTF-8 byte indices; both are only compared
//! and sliced within the same string, so the results are the same.

use unicode_normalization::UnicodeNormalization;

/// One `edits[]` entry of Pi's edit tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiEdit {
    pub old_text: String,
    pub new_text: String,
}

impl PiEdit {
    pub fn new(old_text: impl Into<String>, new_text: impl Into<String>) -> Self {
        Self {
            old_text: old_text.into(),
            new_text: new_text.into(),
        }
    }
}

pub fn detect_line_ending(content: &str) -> &'static str {
    let crlf = content.find("\r\n");
    let lf = content.find('\n');
    match (crlf, lf) {
        (Some(c), Some(l)) if c < l => "\r\n",
        _ => "\n",
    }
}

pub fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

pub fn restore_line_endings(text: &str, ending: &str) -> String {
    if ending == "\r\n" {
        text.replace('\n', "\r\n")
    } else {
        text.to_string()
    }
}

/// Splits a leading BOM (U+FEFF) off the decoded text.
pub fn split_bom(content: &str) -> (&'static str, &str) {
    match content.strip_prefix('\u{FEFF}') {
        Some(rest) => ("\u{FEFF}", rest),
        None => ("", content),
    }
}

/// JavaScript `String.prototype.trimEnd`'s whitespace and line terminators.
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{00A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// Pi's normalizeForFuzzyMatch: NFKC, strip trailing whitespace per line, smart quotes to
/// ASCII, Unicode dashes (U+2010-2015, U+2212) to '-', special spaces to ' '.
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    let nfkc: String = text.nfkc().collect();
    let joined = nfkc
        .split('\n')
        .map(|l| l.trim_end_matches(is_js_whitespace))
        .collect::<Vec<_>>()
        .join("\n");
    joined
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            c => c,
        })
        .collect()
}

struct Match {
    index: usize,
    length: usize,
    used_fuzzy: bool,
}

fn fuzzy_find_text(content: &str, old_text: &str) -> Option<Match> {
    if let Some(i) = content.find(old_text) {
        return Some(Match {
            index: i,
            length: old_text.len(),
            used_fuzzy: false,
        });
    }
    let fc = normalize_for_fuzzy_match(content);
    let fo = normalize_for_fuzzy_match(old_text);
    fc.find(&fo).map(|i| Match {
        index: i,
        length: fo.len(),
        used_fuzzy: true,
    })
}

fn count_occurrences(content: &str, old_text: &str) -> usize {
    let fc = normalize_for_fuzzy_match(content);
    let fo = normalize_for_fuzzy_match(old_text);
    if fo.is_empty() {
        return 0;
    }
    fc.matches(fo.as_str()).count()
}

#[derive(Clone)]
struct MatchedEdit {
    edit_index: usize,
    match_index: usize,
    match_length: usize,
    new_text: String,
}

/// Applies every edit against the ORIGINAL (LF-normalised) content. Returns the new LF content,
/// or Pi's error text.
pub fn apply_edits(
    normalized_content: &str,
    edits: &[PiEdit],
    path: &str,
) -> Result<String, String> {
    let norm: Vec<PiEdit> = edits
        .iter()
        .map(|e| PiEdit::new(normalize_to_lf(&e.old_text), normalize_to_lf(&e.new_text)))
        .collect();
    let many = norm.len() != 1;

    for (i, e) in norm.iter().enumerate() {
        if e.old_text.is_empty() {
            return Err(if many {
                format!("edits[{i}].oldText must not be empty in {path}.")
            } else {
                format!("oldText must not be empty in {path}.")
            });
        }
    }

    let used_fuzzy_any = norm
        .iter()
        .any(|e| fuzzy_find_text(normalized_content, &e.old_text).is_some_and(|m| m.used_fuzzy));
    let base_content = if used_fuzzy_any {
        normalize_for_fuzzy_match(normalized_content)
    } else {
        normalized_content.to_string()
    };

    let mut matched = Vec::with_capacity(norm.len());
    for (i, e) in norm.iter().enumerate() {
        let Some(m) = fuzzy_find_text(&base_content, &e.old_text) else {
            return Err(if many {
                format!(
                    "Could not find edits[{i}] in {path}. The oldText must match exactly including all whitespace and newlines."
                )
            } else {
                format!(
                    "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
                )
            });
        };
        let occurrences = count_occurrences(&base_content, &e.old_text);
        if occurrences > 1 {
            return Err(if many {
                format!(
                    "Found {occurrences} occurrences of edits[{i}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
                )
            } else {
                format!(
                    "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
                )
            });
        }
        matched.push(MatchedEdit {
            edit_index: i,
            match_index: m.index,
            match_length: m.length,
            new_text: e.new_text.clone(),
        });
    }

    // Stable sort by position (Pi sorts the same way).
    matched.sort_by_key(|m| m.match_index);

    for pair in matched.windows(2) {
        let (prev, cur) = (&pair[0], &pair[1]);
        if prev.match_index + prev.match_length > cur.match_index {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                prev.edit_index, cur.edit_index
            ));
        }
    }

    let new_content = if used_fuzzy_any {
        apply_replacements_preserving_unchanged_lines(normalized_content, &base_content, &matched)?
    } else {
        apply_replacements(&base_content, &matched, 0)
    };

    if normalized_content == new_content {
        return Err(if many {
            format!("No changes made to {path}. The replacements produced identical content.")
        } else {
            format!(
                "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
            )
        });
    }
    Ok(new_content)
}

/// `content.match(/[^\n]*\n|[^\n]+/g)`: each line keeps its "\n".
pub fn split_lines_with_endings(content: &str) -> Vec<&str> {
    content.split_inclusive('\n').collect()
}

fn apply_replacements(content: &str, replacements: &[MatchedEdit], offset: usize) -> String {
    let mut result = content.to_string();
    for r in replacements.iter().rev() {
        let i = r.match_index - offset;
        result = format!(
            "{}{}{}",
            &result[..i],
            r.new_text,
            &result[i + r.match_length..]
        );
    }
    result
}

fn apply_replacements_preserving_unchanged_lines(
    original_content: &str,
    base_content: &str,
    replacements: &[MatchedEdit],
) -> Result<String, String> {
    let original_lines = split_lines_with_endings(original_content);
    let base_lines = split_lines_with_endings(base_content);
    let mut spans = Vec::with_capacity(base_lines.len());
    let mut offset = 0;
    for line in &base_lines {
        spans.push((offset, offset + line.len()));
        offset += line.len();
    }
    if original_lines.len() != spans.len() {
        return Err(
            "Cannot preserve unchanged lines because the base content has a different line count."
                .into(),
        );
    }

    let mut sorted = replacements.to_vec();
    sorted.sort_by_key(|r| r.match_index);
    let mut groups: Vec<(usize, usize, Vec<MatchedEdit>)> = Vec::new();
    for r in sorted {
        let (start_line, end_line) = get_replacement_line_range(&spans, &r)?;
        if let Some(last) = groups.last_mut()
            && start_line < last.1
        {
            last.1 = last.1.max(end_line);
            last.2.push(r);
            continue;
        }
        groups.push((start_line, end_line, vec![r]));
    }

    let mut sb = String::new();
    let mut original_line_index = 0;
    for (start_line, end_line, group) in &groups {
        for line in &original_lines[original_line_index..*start_line] {
            sb.push_str(line);
        }
        let group_start = spans[*start_line].0;
        let group_end = spans[end_line - 1].1;
        sb.push_str(&apply_replacements(
            &base_content[group_start..group_end],
            group,
            group_start,
        ));
        original_line_index = *end_line;
    }
    for line in &original_lines[original_line_index..] {
        sb.push_str(line);
    }
    Ok(sb)
}

fn get_replacement_line_range(
    lines: &[(usize, usize)],
    r: &MatchedEdit,
) -> Result<(usize, usize), String> {
    let start = r.match_index;
    let end = r.match_index + r.match_length;
    let outside = || "Replacement range is outside the base content.".to_string();
    let start_line = lines
        .iter()
        .position(|&(s, e)| start >= s && start < e)
        .ok_or_else(outside)?;
    let mut end_line = start_line;
    while end_line < lines.len() && lines[end_line].1 < end {
        end_line += 1;
    }
    if end_line >= lines.len() {
        return Err(outside());
    }
    Ok((start_line, end_line + 1))
}
