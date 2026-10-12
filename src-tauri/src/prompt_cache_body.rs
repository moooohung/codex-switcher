//! Change the prewarm control without reserializing the rendered prompt.
//! serde_json's default maps sort keys, including nested tool schema properties.
use anyhow::{ensure, Result};
use std::{collections::BTreeSet, ops::Range};

struct Member {
    key: String,
    value: Range<usize>,
}

fn whitespace(bytes: &[u8], mut at: usize) -> usize {
    while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
        at += 1;
    }
    at
}

fn string_end(bytes: &[u8], mut at: usize) -> Result<usize> {
    ensure!(bytes.get(at) == Some(&b'"'), "Expected JSON string");
    at += 1;
    while let Some(byte) = bytes.get(at) {
        match byte {
            b'\\' => at += 2,
            b'"' => return Ok(at + 1),
            _ => at += 1,
        }
    }
    anyhow::bail!("Unclosed JSON string")
}

// The whole input is validated by serde first. This scanner only finds byte
// spans, so it never decodes or rewrites prompt/schema strings or numbers.
fn value_end(bytes: &[u8], at: usize) -> Result<usize> {
    match bytes.get(at) {
        Some(b'"') => string_end(bytes, at),
        Some(b'{' | b'[') => {
            let mut depth = 0usize;
            let mut cursor = at;
            while let Some(byte) = bytes.get(cursor) {
                match byte {
                    b'"' => {
                        cursor = string_end(bytes, cursor)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth = depth
                            .checked_sub(1)
                            .ok_or_else(|| anyhow::anyhow!("Invalid JSON depth"))?;
                        if depth == 0 {
                            return Ok(cursor + 1);
                        }
                    }
                    _ => {}
                }
                cursor += 1;
            }
            anyhow::bail!("Unclosed JSON value")
        }
        Some(_) => {
            let mut cursor = at;
            while bytes
                .get(cursor)
                .is_some_and(|b| !b.is_ascii_whitespace() && !b",}]".contains(b))
            {
                cursor += 1;
            }
            ensure!(cursor > at, "Empty JSON value");
            Ok(cursor)
        }
        None => anyhow::bail!("Missing JSON value"),
    }
}

fn members(bytes: &[u8], object: Range<usize>) -> Result<Vec<Member>> {
    ensure!(
        bytes.get(object.start) == Some(&b'{') && bytes.get(object.end - 1) == Some(&b'}'),
        "Expected JSON object"
    );
    let mut result = vec![];
    let mut keys = BTreeSet::new();
    let mut cursor = whitespace(bytes, object.start + 1);
    while cursor < object.end - 1 {
        let key_end = string_end(bytes, cursor)?;
        let key: String = serde_json::from_slice(&bytes[cursor..key_end])?;
        ensure!(keys.insert(key.clone()), "Duplicate JSON control key");
        cursor = whitespace(bytes, key_end);
        ensure!(bytes.get(cursor) == Some(&b':'), "Missing JSON colon");
        let start = whitespace(bytes, cursor + 1);
        let end = value_end(bytes, start)?;
        ensure!(end < object.end, "JSON span outside object");
        result.push(Member {
            key,
            value: start..end,
        });
        cursor = whitespace(bytes, end);
        match bytes.get(cursor) {
            Some(b',') => cursor = whitespace(bytes, cursor + 1),
            Some(b'}') if cursor == object.end - 1 => break,
            _ => anyhow::bail!("Invalid JSON member delimiter"),
        }
    }
    Ok(result)
}

fn replace(bytes: &[u8], range: Range<usize>, replacement: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len() + replacement.len());
    output.extend_from_slice(&bytes[..range.start]);
    output.extend_from_slice(replacement);
    output.extend_from_slice(&bytes[range.end..]);
    output
}

pub(crate) fn prewarm_body(bytes: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        bytes.len() <= 16 * 1024 * 1024,
        "Oversized captured request"
    );
    let parsed: serde_json::Value = serde_json::from_slice(bytes)?;
    ensure!(parsed.is_object(), "Expected Responses request object");
    let start = whitespace(bytes, 0);
    let end = value_end(bytes, start)?;
    let root = members(bytes, start..end)?;
    let output = if let Some(options) = root.iter().find(|m| m.key == "prompt_cache_options") {
        ensure!(
            parsed["prompt_cache_options"].is_object(),
            "Invalid cache options"
        );
        let controls = members(bytes, options.value.clone())?;
        if let Some(prewarm) = controls.iter().find(|m| m.key == "prewarm") {
            ensure!(
                parsed["prompt_cache_options"]["prewarm"] == false,
                "Captured request is already prewarming or malformed"
            );
            replace(bytes, prewarm.value.clone(), b"true")
        } else {
            let insertion = if controls.is_empty() {
                br#""prewarm":true"#.as_slice()
            } else {
                br#","prewarm":true"#.as_slice()
            };
            replace(
                bytes,
                options.value.end - 1..options.value.end - 1,
                insertion,
            )
        }
    } else {
        let insertion = if root.is_empty() {
            br#""prompt_cache_options":{"prewarm":true}"#.as_slice()
        } else {
            br#","prompt_cache_options":{"prewarm":true}"#.as_slice()
        };
        replace(bytes, end - 1..end - 1, insertion)
    };
    // Catch scanner regressions before anything reaches a network transport.
    let warmed: serde_json::Value = serde_json::from_slice(&output)?;
    ensure!(
        warmed["prompt_cache_options"]["prewarm"] == true,
        "Missing prewarm control"
    );
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_prefix_and_schema_order_remain_byte_identical() {
        let original = br#" {"model":"gpt-6.1-sol","tools":[{"parameters":{"properties":{"z":{"type":"string"},"a":{"type":"string"}}}}],"instructions":"keep \\\" and } [","input":[{"role":"user","content":"hello"}],"reasoning":{"effort":"high"},"stream":true,"store":false}  "#;
        let output = prewarm_body(original).unwrap();
        let close = original.iter().rposition(|b| *b == b'}').unwrap();
        assert_eq!(&output[..close], &original[..close]);
        assert_eq!(
            &output[close + br#","prompt_cache_options":{"prewarm":true}"#.len()..],
            &original[close..]
        );
        let mut expected: serde_json::Value = serde_json::from_slice(original).unwrap();
        expected["prompt_cache_options"] = serde_json::json!({"prewarm":true});
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output).unwrap(),
            expected
        );
    }

    #[test]
    fn existing_options_change_only_the_control_value() {
        let original = br#"{"prompt_cache_options":{"ttl":"30m", "prewarm" : false ,"mode":"implicit"},"input":["abc"],"store":false}"#;
        let expected = br#"{"prompt_cache_options":{"ttl":"30m", "prewarm" : true ,"mode":"implicit"},"input":["abc"],"store":false}"#;
        assert_eq!(prewarm_body(original).unwrap(), expected);
        for options in ["{}", "{\"mode\":\"explicit\"}"] {
            let raw = format!("{{\"prompt_cache_options\":{options},\"input\":[]}}");
            let warm = prewarm_body(raw.as_bytes()).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&warm).unwrap();
            assert_eq!(value["prompt_cache_options"]["prewarm"], true);
        }
    }

    #[test]
    fn ambiguous_or_invalid_controls_never_replay() {
        for raw in [
            r#"{"model":"gpt-6","model":"other"}"#,
            r#"{"prompt_cache_options":{},"prompt_cache_options":{}}"#,
            r#"{"prompt_cache_options":{"prewarm":false,"prewarm":false}}"#,
            r#"{"prompt_cache_options":{"prewarm":true}}"#,
            r#"{"prompt_cache_options":{"prewarm":"false"}}"#,
            r#"{"prompt_cache_options":null}"#,
            r#"{"prompt_cache_options":[]}"#,
            r#"{"input":[]},"#,
            r#"[{}]"#,
        ] {
            assert!(prewarm_body(raw.as_bytes()).is_err(), "{raw}");
        }
    }

    #[test]
    fn encoded_control_key_and_unicode_prompt_preserve_original_text() {
        let raw = r#"{"\u0070rompt_cache_options":{"prewarm":false},"input":["한글😀 { \"prewarm\":false }"]}"#;
        let expected = raw.replacen(":false", ":true", 1);
        assert_eq!(prewarm_body(raw.as_bytes()).unwrap(), expected.as_bytes());
    }
}
