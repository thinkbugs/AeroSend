//! Just enough XML reading to survive real device descriptions and SOAP
//! envelopes, without pulling a parser into the dependency tree.
//!
//! UPnP responses are machine-generated and shallow, so a scanner over tag
//! names is enough; the one thing it has to get right is entity handling,
//! because DIDL-Lite metadata arrives escaped inside a SOAP argument.

/// Decode the XML entities a UPnP peer emits. Unknown `&name;` is left alone
/// rather than dropped, so a device using an extension entity still yields
/// readable text.
pub fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        // An entity is short; a `;` far away belongs to other text, and
        // treating it as the end would swallow a whole title.
        let entity = match after.find(';') {
            Some(semi) if semi > 0 && semi <= 10 => &after[..semi],
            _ => "",
        };
        let decoded: Option<String> = match entity {
            "lt" => Some("<".into()),
            "gt" => Some(">".into()),
            "quot" => Some("\"".into()),
            "apos" => Some("'".into()),
            "amp" => Some("&".into()),
            "nbsp" => Some(" ".into()),
            n if n.starts_with('#') => {
                let (digits, radix) = match n[1..].strip_prefix('x') {
                    Some(h) => (h, 16),
                    None => (&n[1..], 10),
                };
                u32::from_str_radix(digits, radix)
                    .ok()
                    .and_then(char::from_u32)
                    .map(|c| c.to_string())
            }
            _ => None,
        };
        match decoded {
            Some(r) => {
                out.push_str(&r);
                rest = &after[entity.len() + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Escape a value for use as XML character data.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Position of the first `<name` tag boundary at or after `from`, or None.
/// The boundary check stops `<port` from matching inside `<portal`.
fn find_tag(text: &str, name: &str, from: usize) -> Option<(usize, usize)> {
    let mut cursor = from;
    loop {
        let open = text[cursor..].find(&format!("<{name}"))? + cursor;
        let after = open + name.len() + 1;
        let next = text.as_bytes().get(after);
        match next {
            Some(b'>') | Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') => {
                return Some((open, after))
            }
            // `<empty/>` is the same element, written shut; rejecting it would
            // hide a device's optional children from the scanner.
            Some(b'/') => return Some((open, after)),
            _ => cursor = after,
        }
    }
}

/// Text content of the first element called `name`, entities decoded.
pub fn element_text(text: &str, name: &str) -> Option<String> {
    let (body_start, close_start) = element_range(text, name, 0)?;
    if body_start >= close_start {
        return Some(String::new());
    }
    Some(unescape(&text[body_start..close_start]))
}

/// All text contents of every element called `name`, in document order.
pub fn all_element_texts(text: &str, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some((body_start, close_start)) = element_range(text, name, cursor) {
        out.push(unescape(&text[body_start..close_start]));
        cursor = close_start + name.len() + 3;
    }
    out
}

/// `(body start, closing tag start)` for the first `name` element at or after
/// `from`. A self-closing element reports an empty body.
fn element_range(text: &str, name: &str, from: usize) -> Option<(usize, usize)> {
    let (_, after_name) = find_tag(text, name, from)?;
    let gt = text[after_name..].find('>')? + after_name;
    let body_start = gt + 1;
    if text.as_bytes()[gt - 1] == b'/' {
        return Some((body_start, body_start));
    }
    let close = text[body_start..].find(&format!("</{name}>"))? + body_start;
    Some((body_start, close))
}

/// Attribute of the first element called `name`.
pub fn element_attr(text: &str, name: &str, attr: &str) -> Option<String> {
    let (_, after_name) = find_tag(text, name, 0)?;
    let gt = text[after_name..].find('>')? + after_name;
    // One relative cursor over the attribute run only; mixing document
    // absolute and block-relative indexes is how a boundary check lies.
    let tag = &text[after_name..gt];
    let mut cursor = 0;
    while let Some(rel) = tag[cursor..].find(attr) {
        let at = cursor + rel;
        let before_ok = at == 0 || !is_name_char(tag.as_bytes()[at - 1]);
        let rest = &tag[at + attr.len()..];
        let after_ok = rest
            .chars()
            .next()
            .is_none_or(|c| matches!(c, '=' | ' ' | '\t'));
        if before_ok && after_ok {
            let value = rest.trim_start_matches(|c| c != '"' && c != '\'');
            let quote = value.chars().next()?;
            let inner = &value[1..];
            let end = inner.find(quote)?;
            return Some(unescape(&inner[..end]));
        }
        cursor = at + attr.len();
    }
    None
}

fn is_name_char(b: u8) -> bool {
    matches!(
        b,
        b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b':'
    )
}

/// The raw markup of every element called `name`, in document order.
///
/// Used for `<service>` and `<item>` lists: reading `serviceType` and
/// `controlURL` as two parallel lists would mis-align the moment a device
/// omits one child, so each element is carved out and searched on its own.
pub fn element_blocks(text: &str, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some((open, after_name)) = find_tag(text, name, cursor) {
        let gt = match text[after_name..].find('>') {
            Some(rel) => after_name + rel,
            None => break,
        };
        if text.as_bytes()[gt - 1] == b'/' {
            // Shut in one tag: the markup itself is the whole block.
            out.push(text[open..=gt].to_string());
            cursor = gt + 1;
            continue;
        }
        let close = match text[gt..].find(&format!("</{name}>")) {
            Some(rel) => gt + rel,
            None => text.len(),
        };
        out.push(text[open..close].to_string());
        cursor = close + name.len() + 3;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_plain_and_namespaced_elements() {
        let xml = r#"<?xml version="1.0"?><root><friendlyName>Living Room TV</friendlyName><x:upnpClass>video</x:upnpClass></root>"#;
        assert_eq!(element_text(xml, "friendlyName").as_deref(), Some("Living Room TV"));
        assert_eq!(element_text(xml, "x:upnpClass").as_deref(), Some("video"));
        assert_eq!(element_text(xml, "missing"), None);
    }

    #[test]
    fn tag_prefix_does_not_false_match() {
        let xml = "<doc><portal>x</portal><port>8290</port></doc>";
        assert_eq!(element_text(xml, "port").as_deref(), Some("8290"));
    }

    #[test]
    fn decodes_escaped_payload() {
        let xml = "<s><metadata>&lt;DIDL&gt;&amp;&#233;tc&#x41;</metadata></s>";
        assert_eq!(
            element_text(xml, "metadata").as_deref(),
            Some("<DIDL>&\u{e9}tc\u{41}")
        );
    }

    #[test]
    fn self_closing_element_is_empty_not_whole_document() {
        let xml = "<s><empty/><name>real</name></s>";
        assert_eq!(element_text(xml, "empty").as_deref(), Some(""));
        assert_eq!(element_text(xml, "name").as_deref(), Some("real"));
    }

    #[test]
    fn reads_attributes_and_lists() {
        let xml = r#"<item id="7" parentID="-1" restricted="1"><dc:title>Big Bang "Theory"</dc:title></item>"#;
        assert_eq!(element_attr(xml, "item", "id").as_deref(), Some("7"));
        assert_eq!(element_attr(xml, "item", "restricted").as_deref(), Some("1"));
        assert_eq!(element_attr(xml, "item", "parent"), None);
        assert_eq!(
            element_attr(xml, "item", "parentID").as_deref(),
            Some("-1")
        );
        assert_eq!(
            element_text(xml, "dc:title").as_deref(),
            Some("Big Bang \"Theory\"")
        );
    }

    #[test]
    fn collects_repeated_elements_in_order() {
        let xml = "<a><v>1</v><v>2</v><v>3</v></a>";
        assert_eq!(all_element_texts(xml, "v"), vec!["1", "2", "3"]);
    }

    #[test]
    fn escaping_round_trips() {
        let raw = r#"<a href="x">&'é</a>"#;
        let escaped = escape(raw);
        assert!(!escaped.contains('<'), "{escaped}");
        let doc = format!("<s><v>{escaped}</v></s>");
        assert_eq!(element_text(&doc, "v").as_deref(), Some(raw));
    }

    #[test]
    fn groups_service_children_with_their_parent() {
        let xml = r#"<serviceList>
            <service><serviceType>urn:a</serviceType><controlURL>/ctl/a</controlURL></service>
            <service><serviceType>urn:b</serviceType><controlURL>/ctl/b</controlURL></service>
        </serviceList>"#;
        let blocks = element_blocks(xml, "service");
        assert_eq!(blocks.len(), 2);
        // Each block is searched on its own, so the two URLs stay paired with
        // the service that declares them.
        assert_eq!(
            blocks
                .iter()
                .map(|b| (element_text(b, "serviceType"), element_text(b, "controlURL")))
                .collect::<Vec<_>>(),
            vec![
                (Some("urn:a".into()), Some("/ctl/a".into())),
                (Some("urn:b".into()), Some("/ctl/b".into())),
            ]
        );
    }

    #[test]
    fn a_service_that_omits_a_child_stays_aligned() {
        let xml = r#"<service><serviceType>urn:a</serviceType></service>
                     <service><serviceType>urn:b</serviceType><controlURL>/ctl/b</controlURL></service>"#;
        let urls: Vec<_> = element_blocks(xml, "service")
            .iter()
            .map(|b| element_text(b, "controlURL"))
            .collect();
        assert_eq!(urls, vec![None, Some("/ctl/b".into())]);
    }
}
