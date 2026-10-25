//! NEAR AI's Responses input schema calls replayed assistant text `input_text`.

/// Rewrite only assistant message content type literals, preserving every
/// other byte. Ambiguous JSON is left for the provider to reject.
#[must_use]
pub fn normalize_assistant_text(body: &[u8]) -> Option<Vec<u8>> {
    let input = field(body, "input")??;
    let mut replacements = Vec::new();
    for (item_start, item_end) in elements(&body[input.0..input.1])? {
        let item_offset = input.0 + item_start;
        let item = &body[item_offset..input.0 + item_end];
        if !matches_string(item, "role", "assistant")? {
            continue;
        }
        if field(item, "type")?.is_some() && !matches_string(item, "type", "message")? {
            continue;
        }
        let Some(content) = field(item, "content")? else {
            continue;
        };
        if item[content.0] != b'[' {
            continue;
        }
        for (part_start, part_end) in elements(&item[content.0..content.1])? {
            let part_offset = item_offset + content.0 + part_start;
            let part = &body[part_offset..item_offset + content.0 + part_end];
            if matches_string(part, "type", "output_text")? {
                let (start, end) = field(part, "type")??;
                replacements.push((part_offset + start, part_offset + end));
            }
        }
    }
    if replacements.is_empty() {
        return None;
    }
    let mut output = Vec::with_capacity(body.len());
    let mut cursor = 0;
    for (start, end) in replacements {
        output.extend_from_slice(&body[cursor..start]);
        output.extend_from_slice(br#""input_text""#);
        cursor = end;
    }
    output.extend_from_slice(&body[cursor..]);
    Some(output)
}

fn field(body: &[u8], name: &str) -> Option<Option<(usize, usize)>> {
    Some(
        crate::admission::members(body)
            .ok()?
            .into_iter()
            .find(|(key, _, _)| key == name)
            .map(|(_, start, end)| (start, end)),
    )
}

fn matches_string(body: &[u8], name: &str, expected: &str) -> Option<bool> {
    let Some((start, end)) = field(body, name)? else {
        return Some(false);
    };
    Some(
        serde_json::from_slice::<String>(&body[start..end])
            .ok()
            .as_deref()
            == Some(expected),
    )
}

// The containing object's span walker already validated the JSON; stream each
// array value solely to locate its original bytes without re-encoding it.
fn elements(body: &[u8]) -> Option<Vec<(usize, usize)>> {
    if body.first() != Some(&b'[') {
        return None;
    }
    let mut at = 1;
    let mut spans = Vec::new();
    loop {
        while body.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        if body.get(at) == Some(&b']') {
            return Some(spans);
        }
        let start = at;
        let mut value =
            serde_json::Deserializer::from_slice(&body[at..]).into_iter::<serde::de::IgnoredAny>();
        value.next()?.ok()?;
        at += value.byte_offset();
        spans.push((start, at));
        while body.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        match body.get(at) {
            Some(b',') => at += 1,
            Some(b']') => return Some(spans),
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_keeps_roles_text_precision_escapes_unknown_fields_and_whitespace() {
        let body = br#"{ "input" : [ {"role":"user","content":[{"type":"output_text","text":"leave user alone"}]}, { "role":"assistant", "id":null, "future":0.100000000000000005, "content":[{"type":"output_text","text":"\u00e9 output_text","annotations":[],"future":true},{"type":"input_text","text":"already input"},{"type":"output_text","text":"second"}]} ],"unknown":{"type":"output_text"} }  "#;
        let expected = String::from_utf8(body.to_vec())
            .unwrap()
            .replace(
                "\"type\":\"output_text\",\"text\":\"\\u00e9",
                "\"type\":\"input_text\",\"text\":\"\\u00e9",
            )
            .replace(
                "\"type\":\"output_text\",\"text\":\"second\"",
                "\"type\":\"input_text\",\"text\":\"second\"",
            );
        assert_eq!(normalize_assistant_text(body).unwrap(), expected.as_bytes());
    }

    #[test]
    fn escaped_field_names_and_type_literals_are_matched_semantically() {
        let body = br#"{"inpu\u0074":[{"ro\u006ce":"assistant","content":[{"ty\u0070e":"output_\u0074ext","text":"keep \u00e9"}]}]}"#;
        let expected = br#"{"inpu\u0074":[{"ro\u006ce":"assistant","content":[{"ty\u0070e":"input_text","text":"keep \u00e9"}]}]}"#;
        assert_eq!(normalize_assistant_text(body).unwrap(), expected);
    }

    #[test]
    fn unrelated_and_ambiguous_inputs_are_unchanged() {
        for body in [
            r#"{"input":"hello"}"#,
            r#"{"input":[{"role":"assistant","content":[{"type":"output_text","text":"truncated"}]}"#,
            r#"{"input":[{"role":"assistant","content":[{"type":"output_text","text":"bad comma"},]}]}"#,
            r#"{"input":[{"role":"assistant","content":"hello"}]}"#,
            r#"{"input":[{"type":"function_call_output","role":"assistant","content":[{"type":"output_text","text":"hello"}]}]}"#,
            r#"{"input":[{"role":"assistant","role":"user","content":[{"type":"output_text","text":"hello"}]}]}"#,
            r#"{"input":[{"role":"assistant","content":[{"type":"output_text","type":"input_text","text":"hello"}]}]}"#,
        ] {
            assert!(
                normalize_assistant_text(body.as_bytes()).is_none(),
                "{body}"
            );
        }
    }
}
