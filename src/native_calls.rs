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
    let mut rest = body;
    let mut calls = Vec::new();
    while let Some(begin) = rest.find(INVOKE) {
        if rest.find(CLOSE).is_some_and(|end| end < begin) {
            break;
        }
        rest = &rest[begin..];
        let tag_end = rest.find('>').ok_or("incomplete native invoke")?;
        let tag = rest[..=tag_end].replacen("<｜｜DSML｜｜ ", "<", 1);
        let name = attribute(&tag, b"name")?.ok_or("native invoke has no name")?;
        let explicit_end = rest.find("</｜｜DSML｜｜ invoke>");
        let end = match (explicit_end, rest.find(CLOSE)) {
            (Some(end), outer) if outer.is_none_or(|outer| end < outer) => end,
            (_, Some(end)) if !rest[tag_end + 1..end].contains(INVOKE) => end,
            _ => return Err("unclosed native invoke".into()),
        };
        if rest[tag_end + 1..end].contains(INVOKE) {
            return Err("nested native invoke".into());
        }
        let explicit_end = explicit_end == Some(end);
        let mut parameters = &rest[tag_end + 1..end];
        let mut arguments = serde_json::Map::new();
        while let Some((begin, opening)) = ["<｜｜DSML｜｜ parameter ", "</｜｜DSML｜｜ parameter "]
            .into_iter()
            .filter_map(|opening| parameters.find(opening).map(|begin| (begin, opening)))
            .min_by_key(|(begin, _)| *begin)
        {
            if !parameters[..begin].trim().is_empty() {
                return Err("unexpected text before native parameter".into());
            }
            parameters = &parameters[begin..];
            let tag_end = parameters.find('>').ok_or("incomplete native parameter")?;
            // A parameter tag carrying attributes is an opening tag even when
            // the model accidentally includes a closing slash. Values stay exact.
            let tag = parameters[..=tag_end].replacen(opening, "<parameter ", 1);
            let key = attribute(&tag, b"name")?.ok_or("native parameter has no name")?;
            let is_string = attribute(&tag, b"string")?.as_deref() == Some("true");
            // Live replies sometimes omit one full-width bar in a closing
            // delimiter. Locate the delimiter without rewriting argument text.
            let (end, close) = [
                "</｜｜DSML｜｜ parameter>",
                "</｜DSML｜｜ parameter>",
                "</｜｜DSML｜ parameter>",
                "｜｜DSML｜｜ parameter>",
            ]
            .into_iter()
            .filter_map(|close| parameters.find(close).map(|end| (end, close)))
            .min_by_key(|(end, _)| *end)
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
            parameters = &parameters[end + close.len()..];
        }
        if !parameters.trim().is_empty() {
            return Err("native invoke contains incomplete parameters or trailing text".into());
        }
        calls.push(json!({"type":"function","function":{"name":name,"arguments":arguments}}));
        rest = if explicit_end {
            &rest[end + "</｜｜DSML｜｜ invoke>".len()..]
        } else {
            ""
        };
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
        let without_close = format!("{OPEN}{}", raw.strip_suffix(CLOSE).unwrap());
        assert_eq!(extract(&without_close).unwrap().unwrap().0, value);
        assert!(extract(&format!("{OPEN}<｜｜DSML｜｜ invoke name=\"read\">")).is_err());
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
        let missing_invoke_close = raw.replace("</｜｜DSML｜｜ invoke>", "");
        assert_eq!(extract(&missing_invoke_close).unwrap().unwrap().0, value);
        assert!(extract(&missing_invoke_close.replace(CLOSE, "")).is_err());
        let literal = raw.replace(
            "if x < 3:",
            "# <｜｜DSML｜｜ parameter name=\"literal\">\nif x < 3:",
        );
        assert!(extract(&literal).unwrap().unwrap().0["tool_calls"][0]["function"]["arguments"]["content"].as_str().unwrap().starts_with("# <｜｜DSML｜｜ parameter name=\"literal\">"));
        assert!(extract(&raw.replace("</｜｜DSML｜｜ parameter>", "")).is_err());
        assert!(
            extract(&format!(
                "{OPEN}{INVOKE}name=\"one\">{INVOKE}name=\"two\"></｜｜DSML｜｜ invoke>{CLOSE}"
            ))
            .is_err()
        );
        let slash_open = raw.replace(
            "<｜｜DSML｜｜ parameter name=",
            "</｜｜DSML｜｜ parameter name=",
        );
        assert_eq!(extract(&slash_open).unwrap().unwrap().0, value);
        let bare_close = raw.replace("</｜｜DSML｜｜ parameter>", "｜｜DSML｜｜ parameter>");
        assert_eq!(extract(&bare_close).unwrap().unwrap().0, value);
        let variant = raw.replace("</｜｜DSML｜｜ parameter>", "</｜DSML｜｜ parameter>");
        assert_eq!(extract(&variant).unwrap().unwrap().0, value);
        for (index, _) in raw.char_indices().skip(1) {
            assert_eq!(visible_prefix(&raw[..index]), Some(""));
        }
    }
}
