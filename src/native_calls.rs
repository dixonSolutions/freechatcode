//! Adapt the native DSML wire envelope without adding instructions to prompts.
use serde_json::{Value, json};

const OPEN: &str = "<｜｜DSML｜｜ calls>";
const CLOSE: &str = "</｜｜DSML｜｜ calls>";
const INVOKE: &str = "<｜｜DSML｜｜ invoke ";

pub fn visible_prefix(raw: &str) -> Option<&str> {
    if let Some(index) = raw.find("<｜｜DSML") {
        return Some(&raw[..index]);
    }
    raw.char_indices()
        .find(|(index, _)| OPEN.starts_with(&raw[*index..]) || INVOKE.starts_with(&raw[*index..]))
        .map(|(index, _)| &raw[..index])
}

fn attribute(tag: &str, key: &[u8]) -> Result<Option<String>, String> {
    let mut reader = quick_xml::Reader::from_str(tag);
    if let quick_xml::events::Event::Start(start) =
        reader.read_event().map_err(|e| e.to_string())?
    {
        for item in start.attributes() {
            let attr = item.map_err(|e| e.to_string())?;
            if attr.key.as_ref() == key {
                return attr
                    .decoded_and_normalized_value(
                        quick_xml::XmlVersion::Implicit1_0,
                        reader.decoder(),
                    )
                    .map(|s| Some(s.into_owned()))
                    .map_err(|e| e.to_string());
            }
        }
    }
    Ok(None)
}

pub fn extract(raw: &str) -> Result<Option<(Value, String)>, String> {
    let Some(start) = raw.find(OPEN).or_else(|| raw.find(INVOKE)) else {
        return Ok(None);
    };
    // The live page also emits complete invocations without the outer opening
    // tag. Keep the actual invocations; never synthesize a missing function.
    let body = if raw[start..].starts_with(OPEN) {
        &raw[start + OPEN.len()..]
    } else {
        &raw[start..]
    };
    let end = body.find(CLOSE).unwrap_or(body.len());
    if raw[start..].starts_with(OPEN) && !body.contains(CLOSE) {
        return Err("incomplete native tool-call envelope".into());
    }
    let body = body[..end]
        .replace("<｜｜DSML｜｜ ", "<")
        .replace("</｜｜DSML｜｜ ", "</");
    let mut rest = body.as_str();
    let mut calls = Vec::new();
    while let Some(begin) = rest.find("<invoke ") {
        rest = &rest[begin..];
        let tag_end = rest.find('>').ok_or("incomplete native invoke")?;
        let name = attribute(&rest[..=tag_end], b"name")?.ok_or("native invoke has no name")?;
        let end = rest.find("</invoke>").ok_or("unclosed native invoke")?;
        let mut parameters = &rest[tag_end + 1..end];
        let mut arguments = serde_json::Map::new();
        while let Some(begin) = parameters.find("<parameter ") {
            parameters = &parameters[begin..];
            let tag_end = parameters.find('>').ok_or("incomplete native parameter")?;
            let tag = &parameters[..=tag_end];
            let key = attribute(tag, b"name")?.ok_or("native parameter has no name")?;
            let is_string = attribute(tag, b"string")?.as_deref() == Some("true");
            let end = parameters
                .find("</parameter>")
                .ok_or("unclosed native parameter")?;
            let text = &parameters[tag_end + 1..end];
            let value = if is_string {
                Value::String(text.to_owned())
            } else {
                serde_json::from_str(text)
                    .map_err(|e| format!("invalid native argument {key}: {e}"))?
            };
            if arguments.insert(key, value).is_some() {
                return Err("duplicate native parameter".into());
            }
            parameters = &parameters[end + "</parameter>".len()..];
        }
        calls.push(json!({"type":"function","function":{"name":name,"arguments":arguments}}));
        rest = &rest[end + "</invoke>".len()..];
    }
    if calls.is_empty() {
        return Err("native tool-call envelope has no invocations".into());
    }
    Ok(Some((
        json!({"type":"tool_calls","tool_calls":calls}),
        raw[..start].to_owned(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn standalone_native_invocation_is_still_a_tool_call() {
        let raw = "<｜｜DSML｜｜ invoke name=\"read\"><｜｜DSML｜｜ parameter name=\"path\" string=\"true\">invoice.py</｜｜DSML｜｜ parameter></｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>";
        let (value, _) = extract(raw).unwrap().unwrap();
        assert_eq!(value["tool_calls"][0]["function"]["name"], "read");
        assert_eq!(visible_prefix(raw), Some(""));
        assert!(extract("<｜｜DSML｜｜ invoke name=\"read\">").is_err());
    }
    #[test]
    fn native_parameters_preserve_code_strings_and_json_types() {
        let raw = "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"write\"><｜｜DSML｜｜ parameter name=\"content\" string=\"true\">if x < 3:\n    print('a & b')</｜｜DSML｜｜ parameter><｜｜DSML｜｜ parameter name=\"count\" string=\"false\">3</｜｜DSML｜｜ parameter></｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>";
        let (value, _) = extract(raw).unwrap().unwrap();
        assert_eq!(value["tool_calls"][0]["function"]["arguments"]["count"], 3);
        assert_eq!(
            value["tool_calls"][0]["function"]["arguments"]["content"],
            "if x < 3:\n    print('a & b')"
        );
        for (index, _) in raw.char_indices().skip(1) {
            assert_eq!(visible_prefix(&raw[..index]), Some(""));
        }
    }
}
