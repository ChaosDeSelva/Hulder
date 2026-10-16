use serde::Deserialize;
use std::path::Path;

/// The subset of CDP's `DOM.Node` that selector building needs.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DomNode {
    pub node_type: u8,
    #[serde(default)]
    pub local_name: String,
    #[serde(default)]
    pub attributes: Vec<String>,
    pub backend_node_id: u64,
    #[serde(default)]
    pub children: Vec<DomNode>,
}

const ELEMENT_NODE: u8 = 1;

#[derive(Debug, Clone)]
pub struct Pick {
    pub url: String,
    pub selector: String,
    pub outer_html: String,
    pub computed: Vec<(String, String)>,
    pub screenshot_png: Vec<u8>,
}

pub const USEFUL_COMPUTED: [&str; 10] = [
    "display",
    "position",
    "width",
    "height",
    "color",
    "background-color",
    "font-family",
    "font-size",
    "margin",
    "padding",
];

impl DomNode {
    /// CDP flattens attributes as `[name, value, name, value, ...]`.
    fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .chunks(2)
            .find(|pair| pair[0] == name)
            .and_then(|pair| pair.get(1))
            .map(String::as_str)
    }
}

/// `CSS.escape` for an identifier: Tailwind-style `md:flex` and `w-1/2`
/// classes and ids that start with a digit are not valid selector tokens
/// as-is.
pub fn css_escape(identifier: &str) -> String {
    let mut escaped = String::with_capacity(identifier.len());
    let mut chars = identifier.chars().enumerate().peekable();
    while let Some((index, character)) = chars.next() {
        let code = character as u32;
        let second_after_dash = index == 1 && identifier.starts_with('-');
        if character == '\0' {
            escaped.push('\u{FFFD}');
        } else if (0x1..=0x1f).contains(&code) || code == 0x7f {
            escaped.push_str(&format!("\\{code:x} "));
        } else if character.is_ascii_digit() && (index == 0 || second_after_dash) {
            escaped.push_str(&format!("\\{code:x} "));
        } else if index == 0 && character == '-' && chars.peek().is_none() {
            escaped.push_str("\\-");
        } else if character == '-'
            || character == '_'
            || character.is_ascii_alphanumeric()
            || code >= 0x80
        {
            escaped.push(character);
        } else {
            escaped.push('\\');
            escaped.push(character);
        }
    }
    escaped
}

/// Builds `tag#id.class:nth-of-type(n)` steps from the nearest id-bearing
/// ancestor (or the root element) down to the node with `backend_node_id`.
pub fn selector_for(root: &DomNode, backend_node_id: u64) -> Option<String> {
    let mut path = Vec::new();
    if !find_path(root, backend_node_id, &mut path) {
        return None;
    }
    let mut steps: Vec<String> = Vec::new();
    for (parent, node) in path.iter() {
        if let Some(id) = node.attribute("id").filter(|id| !id.is_empty()) {
            steps.push(format!("#{}", css_escape(id)));
            break;
        }
        let mut step = node.local_name.clone();
        if let Some(classes) = node.attribute("class") {
            for class in classes.split_whitespace() {
                step.push('.');
                step.push_str(&css_escape(class));
            }
        }
        if let Some(parent) = parent {
            let same_type: Vec<&DomNode> = parent
                .children
                .iter()
                .filter(|child| {
                    child.node_type == ELEMENT_NODE && child.local_name == node.local_name
                })
                .collect();
            if same_type.len() > 1 {
                let index = same_type
                    .iter()
                    .position(|child| child.backend_node_id == node.backend_node_id)?
                    + 1;
                step.push_str(&format!(":nth-of-type({index})"));
            }
        }
        steps.push(step);
    }
    steps.reverse();
    Some(steps.join(" > "))
}

/// Fills `path` with `(parent element, node)` pairs from the target up to the
/// root element; the parent is `None` for the document's root element.
fn find_path<'a>(
    node: &'a DomNode,
    backend_node_id: u64,
    path: &mut Vec<(Option<&'a DomNode>, &'a DomNode)>,
) -> bool {
    for child in &node.children {
        if child.node_type != ELEMENT_NODE {
            continue;
        }
        if child.backend_node_id == backend_node_id || find_path(child, backend_node_id, path) {
            let parent = (node.node_type == ELEMENT_NODE).then_some(node);
            path.push((parent, child));
            return true;
        }
    }
    false
}

pub fn computed_table(computed: &[(String, String)]) -> Vec<(&'static str, String)> {
    USEFUL_COMPUTED
        .iter()
        .filter_map(|name| {
            computed
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| (*name, value.clone()))
        })
        .collect()
}

/// Attribute names whose values never belong in a capture, whatever the tag.
const SECRET_ATTRIBUTE_WORDS: [&str; 5] = ["csrf", "token", "nonce", "secret", "authorization"];
pub const REDACTED: &str = "<redacted>";

fn is_secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    SECRET_ATTRIBUTE_WORDS
        .iter()
        .any(|word| name.contains(word))
}

struct Attribute {
    name: String,
    /// Byte range of the value without its quotes, and the quote byte.
    value: Option<(usize, usize, Option<u8>)>,
}

/// Reads `name`, `name=value`, `name="value"` or `name='value'` pairs from
/// `html[cursor..end]`, the inside of a tag after its name.
fn tag_attributes(html: &str, mut cursor: usize, end: usize) -> Vec<Attribute> {
    let bytes = html.as_bytes();
    let byte_at = |index: usize| bytes.get(index).copied().filter(|_| index < end);
    let mut attributes = Vec::new();
    while let Some(byte) = byte_at(cursor) {
        if byte.is_ascii_whitespace() || byte == b'/' {
            cursor += 1;
            continue;
        }
        let name_start = cursor;
        while byte_at(cursor).is_some_and(|byte| !byte.is_ascii_whitespace() && byte != b'=' && byte != b'/') {
            cursor += 1;
        }
        if cursor == name_start {
            cursor += 1;
            continue;
        }
        let name = html
            .get(name_start..cursor)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let mut lookahead = cursor;
        while byte_at(lookahead).is_some_and(|byte| byte.is_ascii_whitespace()) {
            lookahead += 1;
        }
        let mut value = None;
        if byte_at(lookahead) == Some(b'=') {
            cursor = lookahead + 1;
            while byte_at(cursor).is_some_and(|byte| byte.is_ascii_whitespace()) {
                cursor += 1;
            }
            match byte_at(cursor) {
                Some(quote) if quote == b'"' || quote == b'\'' => {
                    let value_start = cursor + 1;
                    let mut value_end = value_start;
                    while byte_at(value_end).is_some_and(|byte| byte != quote) {
                        value_end += 1;
                    }
                    value = Some((value_start, value_end, Some(quote)));
                    cursor = (value_end + 1).min(end);
                }
                _ => {
                    let value_start = cursor;
                    while byte_at(cursor).is_some_and(|byte| !byte.is_ascii_whitespace()) {
                        cursor += 1;
                    }
                    value = Some((value_start, cursor, None));
                }
            }
        }
        attributes.push(Attribute { name, value });
    }
    attributes
}

/// Replaces attribute values that carry secrets with `<redacted>`: every
/// `value` on a hidden or password input, `content` on a `meta` tag with a
/// secret-looking name, and any attribute whose name mentions csrf, token,
/// nonce, secret or authorization. The rest of the markup is left as is.
pub fn redact_html(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut copied = 0;
    let mut cursor = 0;
    while let Some(offset) = html.get(cursor..).and_then(|rest| rest.find('<')) {
        let name_start = cursor + offset + 1;
        let mut name_end = name_start;
        while bytes
            .get(name_end)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        {
            name_end += 1;
        }
        if name_end == name_start {
            cursor = name_start;
            continue;
        }
        // An unterminated tag runs to the end of the text.
        let end = html
            .get(name_end..)
            .and_then(|rest| rest.find('>'))
            .map_or(html.len(), |close| name_end + close);
        redact_tag(html, &mut out, &mut copied, name_start, name_end, end);
        cursor = end;
    }
    out.push_str(html.get(copied..).unwrap_or_default());
    out
}

fn redact_tag(
    html: &str,
    out: &mut String,
    copied: &mut usize,
    name_start: usize,
    name_end: usize,
    end: usize,
) {
    let tag = html
        .get(name_start..name_end)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let attributes = tag_attributes(html, name_end, end);
    let value_of = |wanted: &str| {
        attributes
            .iter()
            .find(|attribute| attribute.name == wanted)
            .and_then(|attribute| attribute.value)
            .and_then(|(start, end, _)| html.get(start..end))
            .unwrap_or_default()
            .to_ascii_lowercase()
    };
    let secret_input = tag == "input" && matches!(value_of("type").as_str(), "hidden" | "password");
    let secret_meta = tag == "meta" && is_secret_name(&value_of("name"));
    for attribute in &attributes {
        let Some((value_start, value_end, quote)) = attribute.value else {
            continue;
        };
        let redact = is_secret_name(&attribute.name)
            || (secret_input && attribute.name == "value")
            || (secret_meta && attribute.name == "content");
        if !redact {
            continue;
        }
        let replace_start = if quote.is_some() { value_start - 1 } else { value_start };
        let replace_end = match quote {
            Some(quote) if html.as_bytes().get(value_end) == Some(&quote) => value_end + 1,
            _ => value_end,
        };
        out.push_str(html.get(*copied..replace_start).unwrap_or_default());
        out.push('"');
        out.push_str(REDACTED);
        out.push('"');
        *copied = replace_end;
    }
}

pub fn pick_markdown(pick: &Pick, screenshot_path: Option<&Path>) -> String {
    let html = redact_html(pick.outer_html.trim_end());
    let fence = clippings::fence_for(&html);
    let mut markdown = format!(
        "# {selector}\n\n**URL:** {url}\n\n**Selector:** `{selector}`\n\n{fence}html\n{html}\n{fence}\n\n| Property | Value |\n| --- | --- |\n",
        selector = pick.selector,
        url = pick.url,
    );
    for (name, value) in computed_table(&pick.computed) {
        markdown.push_str(&format!("| {name} | {} |\n", value.replace('|', "\\|")));
    }
    match screenshot_path {
        Some(path) => markdown.push_str(&format!("\n**Screenshot:** {}\n", path.display())),
        None => markdown.push_str("\n**Screenshot:** none (element has no visible box)\n"),
    }
    markdown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(
        name: &str,
        attributes: &[(&str, &str)],
        backend_node_id: u64,
        children: Vec<DomNode>,
    ) -> DomNode {
        DomNode {
            node_type: ELEMENT_NODE,
            local_name: name.to_string(),
            attributes: attributes
                .iter()
                .flat_map(|(key, value)| [key.to_string(), value.to_string()])
                .collect(),
            backend_node_id,
            children,
        }
    }

    fn document() -> DomNode {
        DomNode {
            node_type: 9,
            local_name: String::new(),
            attributes: vec![],
            backend_node_id: 1,
            children: vec![element(
                "html",
                &[],
                2,
                vec![element(
                    "body",
                    &[],
                    3,
                    vec![
                        element("div", &[("class", "nav main")], 4, vec![]),
                        element(
                            "div",
                            &[("id", "content")],
                            5,
                            vec![
                                element("p", &[], 6, vec![]),
                                element(
                                    "p",
                                    &[("class", "lead")],
                                    7,
                                    vec![element("a", &[("href", "/x")], 8, vec![])],
                                ),
                            ],
                        ),
                        element("span", &[], 9, vec![]),
                    ],
                )],
            )],
        }
    }

    #[test]
    fn selector_uses_id_as_anchor_and_nth_of_type_only_when_ambiguous() {
        let root = document();
        assert_eq!(
            selector_for(&root, 8).as_deref(),
            Some("#content > p.lead:nth-of-type(2) > a")
        );
        assert_eq!(
            selector_for(&root, 4).as_deref(),
            Some("html > body > div.nav.main:nth-of-type(1)")
        );
        assert_eq!(
            selector_for(&root, 9).as_deref(),
            Some("html > body > span")
        );
        assert_eq!(selector_for(&root, 5).as_deref(), Some("#content"));
        assert_eq!(selector_for(&root, 999), None);
    }

    #[test]
    fn selector_escapes_tailwind_classes_and_numeric_ids() {
        assert_eq!(css_escape("md:flex"), "md\\:flex");
        assert_eq!(css_escape("w-1/2"), "w-1\\/2");
        assert_eq!(css_escape("1abc"), "\\31 abc");
        assert_eq!(css_escape("-1x"), "-\\31 x");
        assert_eq!(css_escape("-"), "\\-");
        assert_eq!(css_escape("plain_name-9"), "plain_name-9");
        assert_eq!(css_escape("a\u{0}b"), "a\u{FFFD}b");

        let root = DomNode {
            node_type: 9,
            local_name: String::new(),
            attributes: vec![],
            backend_node_id: 1,
            children: vec![element(
                "html",
                &[],
                2,
                vec![element(
                    "div",
                    &[("id", "1abc")],
                    3,
                    vec![element("p", &[("class", "md:flex w-1/2")], 4, vec![])],
                )],
            )],
        };
        assert_eq!(
            selector_for(&root, 4).as_deref(),
            Some("#\\31 abc > p.md\\:flex.w-1\\/2")
        );
    }

    #[test]
    fn dom_node_deserializes_from_cdp_shape() {
        let root: DomNode = serde_json::from_str(
            r##"{"nodeId":1,"backendNodeId":1,"nodeType":9,"nodeName":"#document","localName":"","nodeValue":"","childNodeCount":1,"children":[{"nodeId":2,"backendNodeId":2,"nodeType":1,"nodeName":"HTML","localName":"html","nodeValue":"","attributes":["lang","en"]}]}"##,
        )
        .expect("dom node");
        assert_eq!(root.children[0].attribute("lang"), Some("en"));
        assert_eq!(selector_for(&root, 2).as_deref(), Some("html"));
    }

    #[test]
    fn computed_table_keeps_the_ten_useful_properties_in_order() {
        let computed = vec![
            ("z-index".to_string(), "1".to_string()),
            ("font-size".to_string(), "16px".to_string()),
            ("display".to_string(), "block".to_string()),
            ("color".to_string(), "rgb(0, 0, 0)".to_string()),
        ];
        let rows = computed_table(&computed);
        assert_eq!(
            rows,
            vec![
                ("display", "block".to_string()),
                ("color", "rgb(0, 0, 0)".to_string()),
                ("font-size", "16px".to_string()),
            ]
        );
    }

    #[test]
    fn hidden_inputs_passwords_and_token_attributes_are_redacted() {
        let html = concat!(
            "<form action=\"/login\" data-csrf-token=\"abc123\" data-nonce='n1'>",
            "<input type=\"hidden\" name=\"authenticity_token\" value=\"tok-999\">",
            "<input name=\"user\" value=\"alice\" type=text>",
            "<INPUT value=\"hunter2\" TYPE=\"Password\" />",
            "<input type=\"text\" value=\"visible\">",
            "<div data-secret=unquoted title=\"keep\" authorization=\"Bearer x\">",
            "<meta name=\"csrf-token\" content=\"meta-tok\">",
            "<p>x-token-text stays</p>",
            "</div></form>"
        );
        let redacted = redact_html(html);
        for secret in ["abc123", "n1", "tok-999", "hunter2", "unquoted", "Bearer x", "meta-tok"] {
            assert!(!redacted.contains(secret), "{secret} leaked into {redacted}");
        }
        for kept in [
            "alice",
            "visible",
            "title=\"keep\"",
            "x-token-text stays",
            "name=\"authenticity_token\"",
            "action=\"/login\"",
            "name=\"csrf-token\"",
        ] {
            assert!(redacted.contains(kept), "{kept} missing from {redacted}");
        }
        assert!(redacted.contains("data-csrf-token=\"<redacted>\""), "{redacted}");
        assert!(redacted.contains("value=\"<redacted>\" TYPE=\"Password\""), "{redacted}");
        assert!(redacted.contains("data-secret=\"<redacted>\""), "{redacted}");
        assert!(redacted.contains("content=\"<redacted>\""), "{redacted}");
        assert_eq!(redact_html("<p>plain</p>"), "<p>plain</p>");
        assert_eq!(redact_html("a < b and c > d"), "a < b and c > d");
        assert_eq!(redact_html("<input type=hidden value=\"x\""), "<input type=hidden value=\"<redacted>\"");
    }

    #[test]
    fn markdown_redacts_and_cannot_break_out_of_its_fence() {
        let pick = Pick {
            url: "https://example.com/".into(),
            selector: "form".into(),
            outer_html: "<form><input type=\"hidden\" value=\"tok\">\n```\n# injected\n```\n</form>".into(),
            computed: vec![],
            screenshot_png: vec![],
        };
        let markdown = pick_markdown(&pick, None);
        assert!(!markdown.contains("tok"), "{markdown}");
        assert!(markdown.contains("````html\n<form>"), "{markdown}");
        assert!(markdown.contains("</form>\n````\n"), "{markdown}");
    }

    #[test]
    fn markdown_has_url_selector_html_table_and_screenshot() {
        let pick = Pick {
            url: "https://example.com/".into(),
            selector: "html > body > span".into(),
            outer_html: "<span>hi</span>".into(),
            computed: vec![("display".into(), "inline".into())],
            screenshot_png: vec![],
        };
        let markdown = pick_markdown(&pick, Some(Path::new("/tmp/picks/1.png")));
        assert!(pick_markdown(&pick, None).contains("**Screenshot:** none"));
        assert!(markdown.starts_with("# html > body > span\n"));
        assert!(markdown.contains("**URL:** https://example.com/\n"));
        assert!(markdown.contains("```html\n<span>hi</span>\n```"));
        assert!(markdown.contains("| display | inline |"));
        assert!(markdown.contains("/tmp/picks/1.png"));
    }
}
