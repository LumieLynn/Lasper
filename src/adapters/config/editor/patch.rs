//! Byte-preserving source mutations shared by configuration pages.
//!
//! This layer knows how to apply bounded ranges and render a reviewable diff;
//! it does not decide which settings a page owns or where a page may insert
//! declarations.

use std::collections::BTreeMap;
use std::path::Path;

use super::document::NspawnDocument;

const DIFF_CONTEXT_LINES: usize = 2;

#[derive(Debug)]
pub(super) struct SourceMutation {
    pub(super) line: usize,
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) old: Vec<String>,
    pub(super) new: Vec<String>,
    pub(super) replacement: Option<String>,
}

pub(super) fn apply_mutations(content: &str, mutations: &[SourceMutation]) -> String {
    let mut after = content.to_owned();
    for mutation in mutations.iter().rev() {
        after.replace_range(
            mutation.start..mutation.end,
            mutation.replacement.as_deref().unwrap_or_default(),
        );
    }
    after
}

pub(super) fn render_diff(
    path: &Path,
    document: &NspawnDocument<'_>,
    mutations: &[SourceMutation],
) -> String {
    let lines = document.lines();
    let mut diff = format!("--- {}\n+++ {}\n", path.display(), path.display());
    if lines.is_empty() {
        let inserted = mutations
            .iter()
            .flat_map(|mutation| mutation.new.iter())
            .collect::<Vec<_>>();
        diff.push_str(&format!("@@ -0,0 +1,{} @@\n", inserted.len()));
        for line in inserted {
            diff.push('+');
            diff.push_str(line);
            diff.push('\n');
        }
        return diff;
    }

    let mut ranges = Vec::<(usize, usize)>::new();
    for mutation in mutations {
        let (start, end) = if mutation.old.is_empty() {
            let point = mutation.line.clamp(1, lines.len() + 1);
            (
                point.saturating_sub(DIFF_CONTEXT_LINES).max(1),
                point
                    .saturating_add(DIFF_CONTEXT_LINES.saturating_sub(1))
                    .min(lines.len()),
            )
        } else {
            (
                mutation.line.saturating_sub(DIFF_CONTEXT_LINES).max(1),
                mutation
                    .line
                    .saturating_add(DIFF_CONTEXT_LINES)
                    .min(lines.len()),
            )
        };
        match ranges.last_mut() {
            Some((_, previous_end)) if start <= previous_end.saturating_add(1) => {
                *previous_end = (*previous_end).max(end);
            }
            _ => ranges.push((start, end)),
        }
    }
    let replacements = mutations
        .iter()
        .filter(|mutation| !mutation.old.is_empty())
        .map(|mutation| (mutation.line, mutation))
        .collect::<BTreeMap<_, _>>();
    for (start, end) in ranges {
        let delta_before = mutations
            .iter()
            .filter(|mutation| mutation.line < start)
            .map(|mutation| mutation.new.len() as isize - mutation.old.len() as isize)
            .sum::<isize>();
        let old_count = end - start + 1;
        let delta_here = mutations
            .iter()
            .filter(|mutation| {
                if mutation.old.is_empty() {
                    (start..=end.saturating_add(1)).contains(&mutation.line)
                } else {
                    (start..=end).contains(&mutation.line)
                }
            })
            .map(|mutation| mutation.new.len() as isize - mutation.old.len() as isize)
            .sum::<isize>();
        let new_start = (start as isize + delta_before).max(0) as usize;
        let new_count = (old_count as isize + delta_here).max(0) as usize;
        diff.push_str(&format!(
            "@@ -{start},{old_count} +{new_start},{new_count} @@\n"
        ));
        for line_number in start..=end {
            for mutation in mutations
                .iter()
                .filter(|mutation| mutation.old.is_empty() && mutation.line == line_number)
            {
                for new in &mutation.new {
                    diff.push('+');
                    diff.push_str(new);
                    diff.push('\n');
                }
            }
            if let Some(mutation) = replacements.get(&line_number) {
                for old in &mutation.old {
                    diff.push('-');
                    diff.push_str(old);
                    diff.push('\n');
                }
                for new in &mutation.new {
                    diff.push('+');
                    diff.push_str(new);
                    diff.push('\n');
                }
            } else if let Some(line) = lines.get(line_number - 1) {
                diff.push(' ');
                diff.push_str(line.body());
                diff.push('\n');
            }
        }
        for mutation in mutations
            .iter()
            .filter(|mutation| mutation.old.is_empty() && mutation.line == end + 1)
        {
            for new in &mutation.new {
                diff.push('+');
                diff.push_str(new);
                diff.push('\n');
            }
        }
    }
    diff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_mutations_in_reverse_offset_order() {
        let content = "one\ntwo\nthree\n";
        let mutations = [
            SourceMutation {
                line: 1,
                start: 0,
                end: 4,
                old: vec!["one".into()],
                new: vec!["ONE".into()],
                replacement: Some("ONE\n".into()),
            },
            SourceMutation {
                line: 3,
                start: 8,
                end: 14,
                old: vec!["three".into()],
                new: vec!["THREE".into()],
                replacement: Some("THREE\n".into()),
            },
        ];
        assert_eq!(apply_mutations(content, &mutations), "ONE\ntwo\nTHREE\n");
    }
}
