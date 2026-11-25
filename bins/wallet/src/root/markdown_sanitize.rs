//! Rewrites Markdown so links, images and raw HTML become inert visible text.
//! Fails when the source cannot be rewritten, would still contain activatable nodes, or would
//! exceed the caller's output limit.

use std::collections::BTreeMap;

use markdown::{ParseOptions, mdast::Node, to_mdast};

pub(super) const MAX_MARKDOWN_DEPTH: usize = 128;
/// markdown-rs parse time grows superlinearly with container nesting on one line; real
/// proposals nest under 10.
pub(super) const MAX_LINE_CONTAINER_PREFIX: usize = 128;

/// Root children of parsed Markdown, dismantled iteratively on drop because the default
/// recursive drop of a deep `mdast` tree overflows the stack.
pub(super) struct ParsedMarkdown {
    children: Vec<Node>,
}

impl ParsedMarkdown {
    pub(super) fn children(&self) -> &[Node] {
        &self.children
    }
}

impl Drop for ParsedMarkdown {
    fn drop(&mut self) {
        let mut pending = std::mem::take(&mut self.children);
        while let Some(mut node) = pending.pop() {
            if let Some(children) = node.children_mut() {
                pending.append(children);
            }
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum MarkdownParseError {
    Invalid,
    TooDeep,
}

/// Parses GFM Markdown and rejects trees deeper than `MAX_MARKDOWN_DEPTH`, which bounds the
/// stack use of recursive visitors. It rejects long line container prefixes before parsing,
/// which bounds parse time.
pub(super) fn parse_markdown(source: &str) -> Result<ParsedMarkdown, MarkdownParseError> {
    if source
        .split(['\n', '\r'])
        .any(|line| container_prefix_len(line.as_bytes()) > MAX_LINE_CONTAINER_PREFIX)
    {
        return Err(MarkdownParseError::TooDeep);
    }
    let root = match to_mdast(source, &ParseOptions::gfm()) {
        Ok(Node::Root(root)) => root,
        Ok(other) => {
            // Dropped through the guard so a deep non-root tree is also dismantled iteratively.
            drop(ParsedMarkdown {
                children: vec![other],
            });
            return Err(MarkdownParseError::Invalid);
        }
        Err(_) => return Err(MarkdownParseError::Invalid),
    };
    let parsed = ParsedMarkdown {
        children: root.children,
    };
    let mut pending = parsed
        .children
        .iter()
        .map(|node| (node, 1usize))
        .collect::<Vec<_>>();
    while let Some((node, depth)) = pending.pop() {
        if depth > MAX_MARKDOWN_DEPTH {
            return Err(MarkdownParseError::TooDeep);
        }
        if let Some(children) = node.children() {
            pending.extend(children.iter().map(|child| (child, depth + 1)));
        }
    }
    Ok(parsed)
}

/// Length of the run at the start of `line` that can open block quotes, list items and GFM
/// footnote definitions, the containers markdown-rs nests within one line. A BOM does not end
/// the run, since markdown-rs strips one from the start of each input and preparation parses
/// fragments that start mid-line.
fn container_prefix_len(line: &[u8]) -> usize {
    let mut len = 0;
    loop {
        let rest = &line[len..];
        let marker = match rest {
            [b'>' | b' ' | b'\t', ..] => 1,
            [0xEF, 0xBB, 0xBF, ..] => 3,
            [b'-' | b'+' | b'*', after @ ..] if list_marker_ends(after) => 1,
            [b'0'..=b'9', ..] => ordered_list_marker_len(rest).unwrap_or(0),
            _ => footnote_opener_len(rest).unwrap_or(0),
        };
        if marker == 0 {
            return len;
        }
        len += marker;
    }
}

/// A list item marker opens a container only before a space, a tab or the end of the line.
const fn list_marker_ends(after: &[u8]) -> bool {
    matches!(after.first(), None | Some(b' ' | b'\t'))
}

/// Length of an ordered list item marker at the start of `bytes`: up to 9 digits, then `.` or
/// `)`, following the pinned markdown-rs value limit.
fn ordered_list_marker_len(bytes: &[u8]) -> Option<usize> {
    let digits = bytes
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    (digits <= 9
        && matches!(bytes.get(digits), Some(b'.' | b')'))
        && list_marker_ends(&bytes[digits + 1..]))
    .then_some(digits + 1)
}

/// Length of a `[^label]:` footnote definition opener at the start of `bytes`, following the
/// pinned markdown-rs label rules.
fn footnote_opener_len(bytes: &[u8]) -> Option<usize> {
    let label = bytes.strip_prefix(b"[^")?;
    let mut index = 0;
    loop {
        match label.get(index)? {
            b'\\' if matches!(label.get(index + 1), Some(b'[' | b'\\' | b']')) => index += 2,
            b']' if index > 0 => break,
            b']' | b'[' | b' ' | b'\t' => return None,
            _ => index += 1,
        }
    }
    (label.get(index + 1) == Some(&b':')).then_some(index + 4)
}

/// Why `sanitize_render_source` could not produce inert Markdown.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum SanitizeFailure {
    /// The projected output exceeded the caller's limit.
    TooLarge,
    /// The source cannot be rewritten or would still contain activatable nodes.
    Unsupported,
}

/// Output bytes left in one sanitizer pass. The pass starts from its input length, which covers
/// every span copied unchanged, and charges each generated piece once where it is created, so
/// the projection only grows and bounds the output.
struct OutputBudget {
    remaining: usize,
    exceeded: bool,
}

impl OutputBudget {
    fn charge(&mut self, piece: String) -> Option<String> {
        if let Some(remaining) = self.remaining.checked_sub(piece.len()) {
            self.remaining = remaining;
            Some(piece)
        } else {
            self.exceeded = true;
            None
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SourceEdit {
    start: usize,
    end: usize,
    replacement: String,
}

pub(super) fn max_backtick_run(value: &str) -> usize {
    let mut longest_run = 0;
    let mut run: usize = 0;
    for character in value.chars() {
        if character == '`' {
            run = run.saturating_add(1);
            longest_run = longest_run.max(run);
        } else {
            run = 0;
        }
    }
    longest_run
}

fn inert_code_span(value: &str) -> String {
    let longest_run = max_backtick_run(value);
    let fence = "`".repeat(longest_run.saturating_add(1).max(1));
    format!("{fence}{value}{fence}")
}

fn inert_text_source(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_alphanumeric() {
                character.to_string()
            } else {
                format!("&#{};", u32::from(character))
            }
        })
        .collect()
}

fn url_shaped_label(value: &str) -> bool {
    value.starts_with("http://")
        || value.starts_with("https://")
        || value.starts_with("mailto:")
        || value.starts_with("xmpp:")
        || value.starts_with("www.")
        || (value.contains('@') && !value.chars().any(char::is_whitespace))
}

pub(super) fn visible_node_text(node: &Node) -> String {
    match node {
        Node::Image(image) => image.alt.clone(),
        Node::ImageReference(image) => image.alt.clone(),
        Node::Break(_) => "\n".to_owned(),
        _ => node.children().map_or_else(
            || node.to_string(),
            |children| children.iter().map(visible_node_text).collect(),
        ),
    }
}

fn bare_email_label(value: &str) -> bool {
    value.contains('@') && !value.contains(':') && !value.chars().any(char::is_whitespace)
}

fn label_represents_destination(label: &str, destination: &str) -> bool {
    (label == destination && url_shaped_label(label))
        || (bare_email_label(label)
            && destination
                .strip_prefix("mailto:")
                .is_some_and(|address| address == label))
}

fn inert_equivalent_label_source(
    source: &str,
    node: &Node,
    definitions: &BTreeMap<String, String>,
    budget: &mut OutputBudget,
) -> Option<String> {
    match node {
        Node::Text(text) => budget.charge(inert_text_source(&text.value)),
        Node::Link(_)
        | Node::LinkReference(_)
        | Node::Image(_)
        | Node::ImageReference(_)
        | Node::Html(_) => sanitized_node_source(source, node, definitions, budget),
        _ => {
            let position = node.position()?;
            let original = source.get(position.start.offset..position.end.offset)?;
            let Some(children) = node.children() else {
                return Some(original.to_owned());
            };
            let mut rendered = String::with_capacity(original.len());
            let mut cursor = position.start.offset;
            for child in children {
                let child_position = child.position()?;
                if child_position.start.offset < cursor
                    || child_position.end.offset > position.end.offset
                {
                    return None;
                }
                rendered.push_str(source.get(cursor..child_position.start.offset)?);
                rendered.push_str(&inert_equivalent_label_source(
                    source,
                    child,
                    definitions,
                    budget,
                )?);
                cursor = child_position.end.offset;
            }
            rendered.push_str(source.get(cursor..position.end.offset)?);
            Some(rendered)
        }
    }
}

fn child_discloses_destination(
    node: &Node,
    destination: &str,
    definitions: &BTreeMap<String, String>,
) -> bool {
    match node {
        Node::Image(image) => image.url == destination,
        Node::ImageReference(image) => definitions
            .get(&image.identifier)
            .is_some_and(|image_destination| image_destination == destination),
        _ => node.children().is_some_and(|children| {
            children
                .iter()
                .any(|child| child_discloses_destination(child, destination, definitions))
        }),
    }
}

/// The ` (destination)` suffix that discloses a link or image destination.
fn inert_destination_suffix(destination: &str, budget: &mut OutputBudget) -> Option<String> {
    budget.charge(format!(" ({})", inert_text_source(destination)))
}

fn sanitized_image_source(
    alt: &str,
    destination: &str,
    budget: &mut OutputBudget,
) -> Option<String> {
    let mut rendered = budget.charge(inert_text_source(alt))?;
    if !label_represents_destination(alt, destination) {
        rendered.push_str(&inert_destination_suffix(destination, budget)?);
    }
    Some(rendered)
}

fn sanitized_link_source(
    source: &str,
    children: &[Node],
    destination: &str,
    definitions: &BTreeMap<String, String>,
    budget: &mut OutputBudget,
) -> Option<String> {
    let visible_label = children.iter().map(visible_node_text).collect::<String>();
    let represents_destination = label_represents_destination(&visible_label, destination);
    let mut rendered = String::new();
    for child in children {
        let child_source = if represents_destination {
            inert_equivalent_label_source(source, child, definitions, budget)?
        } else {
            sanitized_node_source(source, child, definitions, budget)?
        };
        rendered.push_str(&child_source);
    }
    if !represents_destination
        && !children
            .iter()
            .any(|child| child_discloses_destination(child, destination, definitions))
    {
        rendered.push_str(&inert_destination_suffix(destination, budget)?);
    }
    Some(rendered)
}

fn sanitized_node_source(
    source: &str,
    node: &Node,
    definitions: &BTreeMap<String, String>,
    budget: &mut OutputBudget,
) -> Option<String> {
    match node {
        Node::Link(link) => {
            sanitized_link_source(source, &link.children, &link.url, definitions, budget)
        }
        Node::LinkReference(reference) => {
            let destination = definitions.get(&reference.identifier)?;
            sanitized_link_source(
                source,
                &reference.children,
                destination,
                definitions,
                budget,
            )
        }
        Node::Image(image) => sanitized_image_source(&image.alt, &image.url, budget),
        Node::ImageReference(image) => {
            let destination = definitions.get(&image.identifier)?;
            sanitized_image_source(&image.alt, destination, budget)
        }
        Node::Html(html) => budget.charge(inert_code_span(&html.value)),
        _ => {
            let position = node.position()?;
            let original = source.get(position.start.offset..position.end.offset)?;
            let Some(children) = node.children() else {
                return Some(original.to_owned());
            };
            let mut rendered = String::with_capacity(original.len());
            let mut cursor = position.start.offset;
            for child in children {
                let child_position = child.position()?;
                if child_position.start.offset < cursor
                    || child_position.end.offset > position.end.offset
                {
                    return None;
                }
                rendered.push_str(source.get(cursor..child_position.start.offset)?);
                rendered.push_str(&sanitized_node_source(source, child, definitions, budget)?);
                cursor = child_position.end.offset;
            }
            rendered.push_str(source.get(cursor..position.end.offset)?);
            Some(rendered)
        }
    }
}

fn collect_sanitizer_edits(
    source: &str,
    nodes: &[Node],
    definitions: &BTreeMap<String, String>,
    budget: &mut OutputBudget,
    edits: &mut Vec<SourceEdit>,
) -> Option<()> {
    for node in nodes {
        let target = matches!(
            node,
            Node::Link(_)
                | Node::LinkReference(_)
                | Node::Image(_)
                | Node::ImageReference(_)
                | Node::Html(_)
        );
        if target {
            let position = node.position()?;
            let replacement = sanitized_node_source(source, node, definitions, budget)?;
            source.get(position.start.offset..position.end.offset)?;
            edits.push(SourceEdit {
                start: position.start.offset,
                end: position.end.offset,
                replacement,
            });
        } else if let Some(children) = node.children() {
            collect_sanitizer_edits(source, children, definitions, budget, edits)?;
        }
    }
    Some(())
}

fn apply_source_edits(source: &str, mut edits: Vec<SourceEdit>) -> Option<String> {
    edits.sort_by_key(|edit| (edit.start, edit.end));
    let mut rendered = String::with_capacity(source.len());
    let mut cursor = 0;
    for edit in edits {
        if edit.start < cursor
            || edit.start > edit.end
            || source.get(edit.start..edit.end).is_none()
        {
            return None;
        }
        rendered.push_str(source.get(cursor..edit.start)?);
        rendered.push_str(&edit.replacement);
        cursor = edit.end;
    }
    rendered.push_str(source.get(cursor..)?);
    Some(rendered)
}

pub(super) fn contains_activatable_markdown(nodes: &[Node]) -> bool {
    nodes.iter().any(|node| {
        matches!(
            node,
            Node::Link(_)
                | Node::LinkReference(_)
                | Node::Image(_)
                | Node::ImageReference(_)
                | Node::Html(_)
        ) || node
            .children()
            .is_some_and(|children| contains_activatable_markdown(children))
    })
}

fn collect_effective_definition_urls(nodes: &[Node]) -> BTreeMap<String, String> {
    let mut definitions = BTreeMap::new();
    let mut pending = nodes.iter().rev().collect::<Vec<_>>();
    while let Some(node) = pending.pop() {
        if let Node::Definition(definition) = node {
            definitions
                .entry(definition.identifier.clone())
                .or_insert_with(|| definition.url.clone());
        }
        if let Some(children) = node.children() {
            pending.extend(children.iter().rev());
        }
    }
    definitions
}

/// Rewrites one parse of `source`, stopping before the edits are applied once the projected
/// output exceeds `max_len`.
fn sanitize_pass(source: &str, nodes: &[Node], max_len: usize) -> Result<String, SanitizeFailure> {
    let remaining = max_len
        .checked_sub(source.len())
        .ok_or(SanitizeFailure::TooLarge)?;
    let mut budget = OutputBudget {
        remaining,
        exceeded: false,
    };
    let definitions = collect_effective_definition_urls(nodes);
    let mut edits = Vec::new();
    let rendered = collect_sanitizer_edits(source, nodes, &definitions, &mut budget, &mut edits)
        .and_then(|()| apply_source_edits(source, edits));
    match rendered {
        Some(rendered) => Ok(rendered),
        None if budget.exceeded => Err(SanitizeFailure::TooLarge),
        None => Err(SanitizeFailure::Unsupported),
    }
}

/// Sanitizes `source` into inert Markdown of at most `max_len` bytes.
pub(super) fn sanitize_render_source(
    source: &str,
    max_len: usize,
) -> Result<String, SanitizeFailure> {
    let root = parse_markdown(source).map_err(|_| SanitizeFailure::Unsupported)?;
    let mut rendered = sanitize_pass(source, root.children(), max_len)?;
    let mut rendered_root = parse_markdown(&rendered).map_err(|_| SanitizeFailure::Unsupported)?;
    if contains_activatable_markdown(rendered_root.children()) {
        rendered = sanitize_pass(&rendered, rendered_root.children(), max_len)?;
        rendered_root = parse_markdown(&rendered).map_err(|_| SanitizeFailure::Unsupported)?;
    }
    if contains_activatable_markdown(rendered_root.children()) {
        return Err(SanitizeFailure::Unsupported);
    }
    Ok(rendered)
}

#[cfg(test)]
mod tests;
