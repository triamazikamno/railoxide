//! Rewrites Markdown so links, images and raw HTML become inert visible text.
//! Returns `None` when the source cannot be rewritten or would still contain activatable nodes.

use std::collections::BTreeMap;

use markdown::{ParseOptions, mdast::Node, to_mdast};

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
) -> Option<String> {
    match node {
        Node::Text(text) => Some(inert_text_source(&text.value)),
        Node::Link(_)
        | Node::LinkReference(_)
        | Node::Image(_)
        | Node::ImageReference(_)
        | Node::Html(_) => sanitized_node_source(source, node, definitions),
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
                rendered.push_str(&inert_equivalent_label_source(source, child, definitions)?);
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

fn sanitized_image_source(alt: &str, destination: &str) -> String {
    if label_represents_destination(alt, destination) {
        inert_text_source(alt)
    } else {
        format!(
            "{} ({})",
            inert_text_source(alt),
            inert_text_source(destination)
        )
    }
}

fn sanitized_link_source(
    source: &str,
    children: &[Node],
    destination: &str,
    definitions: &BTreeMap<String, String>,
) -> Option<String> {
    let visible_label = children.iter().map(visible_node_text).collect::<String>();
    let represents_destination = label_represents_destination(&visible_label, destination);
    let mut rendered = String::new();
    for child in children {
        let child_source = if represents_destination {
            inert_equivalent_label_source(source, child, definitions)?
        } else {
            sanitized_node_source(source, child, definitions)?
        };
        rendered.push_str(&child_source);
    }
    if !represents_destination
        && !children
            .iter()
            .any(|child| child_discloses_destination(child, destination, definitions))
    {
        rendered.push_str(" (");
        rendered.push_str(&inert_text_source(destination));
        rendered.push(')');
    }
    Some(rendered)
}

fn sanitized_node_source(
    source: &str,
    node: &Node,
    definitions: &BTreeMap<String, String>,
) -> Option<String> {
    match node {
        Node::Link(link) => sanitized_link_source(source, &link.children, &link.url, definitions),
        Node::LinkReference(reference) => {
            let destination = definitions.get(&reference.identifier)?;
            sanitized_link_source(source, &reference.children, destination, definitions)
        }
        Node::Image(image) => Some(sanitized_image_source(&image.alt, &image.url)),
        Node::ImageReference(image) => {
            let destination = definitions.get(&image.identifier)?;
            Some(sanitized_image_source(&image.alt, destination))
        }
        Node::Html(html) => Some(inert_code_span(&html.value)),
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
                rendered.push_str(&sanitized_node_source(source, child, definitions)?);
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
            let replacement = sanitized_node_source(source, node, definitions)?;
            source.get(position.start.offset..position.end.offset)?;
            edits.push(SourceEdit {
                start: position.start.offset,
                end: position.end.offset,
                replacement,
            });
        } else if let Some(children) = node.children() {
            collect_sanitizer_edits(source, children, definitions, edits)?;
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

pub(super) fn sanitize_render_source(source: &str) -> Option<String> {
    let Node::Root(root) = to_mdast(source, &ParseOptions::gfm()).ok()? else {
        return None;
    };
    let definitions = collect_effective_definition_urls(&root.children);
    let mut edits = Vec::new();
    collect_sanitizer_edits(source, &root.children, &definitions, &mut edits)?;
    let mut rendered = apply_source_edits(source, edits)?;
    let Node::Root(mut rendered_root) = to_mdast(&rendered, &ParseOptions::gfm()).ok()? else {
        return None;
    };
    if contains_activatable_markdown(&rendered_root.children) {
        let definitions = collect_effective_definition_urls(&rendered_root.children);
        let mut edits = Vec::new();
        collect_sanitizer_edits(&rendered, &rendered_root.children, &definitions, &mut edits)?;
        rendered = apply_source_edits(&rendered, edits)?;
        let Node::Root(reparsed_root) = to_mdast(&rendered, &ParseOptions::gfm()).ok()? else {
            return None;
        };
        rendered_root = reparsed_root;
    }
    if contains_activatable_markdown(&rendered_root.children) {
        return None;
    }
    Some(rendered)
}

#[cfg(test)]
mod tests {
    use markdown::{ParseOptions, mdast::Node, to_mdast};

    #[test]
    fn proposal_markdown_links_are_rendered_as_inert_visible_content() {
        let source = "[human **label**](https://example.com) [https://label.example](https://destination.example) [https://same.example](https://same.example) [support@example.com](mailto:support@example.com) *[docs at https://example.com](https://destination.example)* *[contact support@example.com](mailto:support@example.com)* https://bare.example <https://autolink.example>\n\n[full][guide] [shortcut] [collapsed][] [![image](https://image.example)](https://wrapper.example)\n\n[guide]: https://reference.example\n[shortcut]: https://shortcut.example\n[collapsed]: https://collapsed.example";
        let rendered = super::sanitize_render_source(source).expect("source should sanitize");
        assert!(rendered.contains("**label**"));
        assert!(rendered.contains("human"));
        assert!(rendered.contains("&#58;"));
        let Node::Root(root) =
            to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
        else {
            panic!("sanitized markdown should have a root");
        };
        assert!(!super::contains_activatable_markdown(&root.children));
        let visible = super::visible_node_text(&Node::Root(root));
        assert!(visible.contains("human label (https://example.com)"));
        assert!(visible.contains("https://label.example (https://destination.example)"));
        assert_eq!(visible.matches("https://same.example").count(), 1);
        assert_eq!(visible.matches("mailto:support@example.com").count(), 1);
        assert!(visible.contains("docs at https://example.com (https://destination.example)"));
        assert!(visible.contains("contact support@example.com"));
        assert!(visible.contains("full (https://reference.example)"));
        assert!(visible.contains("shortcut (https://shortcut.example)"));
        assert!(visible.contains("collapsed (https://collapsed.example)"));
        assert!(visible.contains("image (https://image.example) (https://wrapper.example)"));
        assert_eq!(visible.matches("https://autolink.example").count(), 1);
        assert_eq!(visible.matches("https://bare.example").count(), 1);
    }

    #[test]
    fn proposal_reference_links_use_first_definition_and_deduplicate_equivalent_urls() {
        let source = "[same][same] [same-short] [same-collapsed][] [email][email-ref]\n\n[same]: https://same.example\n[same-short]: https://short.example\n[same-collapsed]: https://collapsed.example\n[email-ref]: mailto:support@example.com\n[same]: https://later.example";
        let rendered = super::sanitize_render_source(source).expect("source should sanitize");
        let Node::Root(root) =
            to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
        else {
            panic!("sanitized markdown should have a root");
        };
        assert!(!super::contains_activatable_markdown(&root.children));
        let visible = super::visible_node_text(&Node::Root(root));
        assert!(visible.contains("same (https://same.example)"));
        assert!(!visible.contains("later.example"));
        assert!(visible.contains("same-short (https://short.example)"));
        assert!(visible.contains("same-collapsed (https://collapsed.example)"));
        assert!(visible.contains("email (mailto:support@example.com)"));
    }

    fn contains_markdown_node(nodes: &[Node], predicate: &impl Fn(&Node) -> bool) -> bool {
        nodes.iter().any(|node| {
            predicate(node)
                || node
                    .children()
                    .is_some_and(|children| contains_markdown_node(children, predicate))
        })
    }

    #[test]
    fn proposal_images_are_inert_and_preserve_distinct_destinations() {
        let source = "![direct](https://image.example) ![https://same-image.example](https://same-image.example) [![nested](https://shared.example)](https://shared.example) [![different](https://inner.example)](https://outer.example) ![reference][image-ref]\n\n[image-ref]: https://reference-image.example";
        let rendered = super::sanitize_render_source(source).expect("source should sanitize");
        let Node::Root(root) =
            to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
        else {
            panic!("sanitized markdown should have a root");
        };
        assert!(!super::contains_activatable_markdown(&root.children));
        let visible = super::visible_node_text(&Node::Root(root));
        assert!(visible.contains("direct (https://image.example)"));
        assert_eq!(visible.matches("https://same-image.example").count(), 1);
        assert_eq!(visible.matches("https://shared.example").count(), 1);
        assert!(visible.contains("different (https://inner.example) (https://outer.example)"));
        assert!(visible.contains("reference (https://reference-image.example)"));
    }

    #[test]
    fn proposal_destination_equivalence_is_exact_and_mailto_specific() {
        let source = "[mailto:https://label.example](https://label.example) [https://label.example](mailto:https://label.example) [foo](foo) [support@example.com](mailto:support@example.com)";
        let rendered = super::sanitize_render_source(source).expect("source should sanitize");
        let Node::Root(root) =
            to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
        else {
            panic!("sanitized markdown should have a root");
        };
        assert!(!super::contains_activatable_markdown(&root.children));
        let visible = super::visible_node_text(&Node::Root(root));
        assert!(visible.contains("mailto:https://label.example (https://label.example)"));
        assert!(visible.contains("https://label.example (mailto:https://label.example)"));
        assert!(visible.contains("foo (foo)"));
        assert_eq!(visible.matches("support@example.com").count(), 1);
        assert!(!visible.contains("mailto:support@example.com"));
    }

    #[test]
    fn proposal_equivalent_url_labels_preserve_safe_formatting() {
        let source = "[**https://strong.example**](https://strong.example) [*https://emphasis.example*](https://emphasis.example) [~~https://delete.example~~](https://delete.example) [`https://code.example`](https://code.example) [https://**mixed**.example](https://mixed.example)";
        let rendered = super::sanitize_render_source(source).expect("source should sanitize");
        let Node::Root(root) =
            to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
        else {
            panic!("sanitized markdown should have a root");
        };
        assert!(!super::contains_activatable_markdown(&root.children));
        assert!(contains_markdown_node(&root.children, &|node| matches!(
            node,
            Node::Strong(_)
        )));
        assert!(contains_markdown_node(&root.children, &|node| matches!(
            node,
            Node::Emphasis(_)
        )));
        assert!(contains_markdown_node(&root.children, &|node| matches!(
            node,
            Node::Delete(_)
        )));
        assert!(contains_markdown_node(&root.children, &|node| matches!(
            node,
            Node::InlineCode(_)
        )));
        let visible = super::visible_node_text(&Node::Root(root));
        for destination in [
            "https://strong.example",
            "https://emphasis.example",
            "https://delete.example",
            "https://code.example",
            "https://mixed.example",
        ] {
            assert_eq!(visible.matches(destination).count(), 1);
        }
    }

    #[test]
    fn proposal_destinations_preserve_decoded_boundary_whitespace() {
        let source = "[inline](<https://example.com/a&#32;>) [reference][space]\n\n[space]: <&#32;https://example.com/b&#32;>";
        let rendered = super::sanitize_render_source(source).expect("source should sanitize");
        let Node::Root(root) =
            to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
        else {
            panic!("sanitized markdown should have a root");
        };
        assert!(!super::contains_activatable_markdown(&root.children));
        let visible = super::visible_node_text(&Node::Root(root));
        assert!(visible.contains("inline (https://example.com/a )"));
        assert!(visible.contains("reference ( https://example.com/b )"));
    }

    #[test]
    fn proposal_markdown_sanitizes_nested_structures_and_html() {
        let source = "> quote [link](https://example.com)\n>\n> - [nested][ref]\n\n[ref]: https://reference.example\n\n<a href=\"https://example.com\">anchor</a>";
        let rendered = super::sanitize_render_source(source).expect("source should sanitize");
        assert!(rendered.contains("quote link"));
        assert!(rendered.contains("nested"));
        assert!(rendered.contains("anchor"));
        assert!(rendered.contains("`<a href=\"https://example.com\">"));
        assert!(rendered.contains("</a>`"));
        let Node::Root(root) =
            to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
        else {
            panic!("sanitized markdown should have a root");
        };
        assert!(!super::contains_activatable_markdown(&root.children));
    }
}
