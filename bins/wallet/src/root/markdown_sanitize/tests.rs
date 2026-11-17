use markdown::{ParseOptions, mdast::Node, to_mdast};

#[test]
fn proposal_markdown_links_are_rendered_as_inert_visible_content() {
    let source = "[human **label**](https://example.com) [https://label.example](https://destination.example) [https://same.example](https://same.example) [support@example.com](mailto:support@example.com) *[docs at https://example.com](https://destination.example)* *[contact support@example.com](mailto:support@example.com)* https://bare.example <https://autolink.example>\n\n[full][guide] [shortcut] [collapsed][] [![image](https://image.example)](https://wrapper.example)\n\n[guide]: https://reference.example\n[shortcut]: https://shortcut.example\n[collapsed]: https://collapsed.example";
    let rendered = super::sanitize_render_source(source).expect("source should sanitize");
    assert!(rendered.contains("**label**"));
    assert!(rendered.contains("human"));
    assert!(rendered.contains("&#58;"));
    let Node::Root(root) = to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
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
    let Node::Root(root) = to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
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
    let Node::Root(root) = to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
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
    let Node::Root(root) = to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
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
    let Node::Root(root) = to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
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
    let Node::Root(root) = to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
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
    let Node::Root(root) = to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
    else {
        panic!("sanitized markdown should have a root");
    };
    assert!(!super::contains_activatable_markdown(&root.children));
}
