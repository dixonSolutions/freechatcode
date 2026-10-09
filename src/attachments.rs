//! Extract only explicit attachment parts; never treat prose as a file path.
use base64::Engine;
use serde_json::Value;

pub struct Attachment {
    pub name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

const LIMIT: usize = 8 * 1024 * 1024;

pub async fn from_prompt(prompt: &str) -> Result<Vec<Attachment>, String> {
    let Ok(payload) = serde_json::from_str::<Value>(prompt) else {
        return Ok(Vec::new());
    };
    let mut files = Vec::new();
    let mut total = 0usize;
    for message in payload["messages"].as_array().into_iter().flatten() {
        for part in message["content"].as_array().into_iter().flatten() {
            let kind = part["type"].as_str().unwrap_or_default();
            let file = match kind {
                "image_url" | "input_image" => &part["image_url"],
                "file" | "input_file" => part.get("file").unwrap_or(part),
                _ => continue,
            };
            let source = file
                .as_str()
                .or_else(|| file["url"].as_str())
                .or_else(|| file["file_data"].as_str())
                .or_else(|| file["data"].as_str());
            let name = file["filename"]
                .as_str()
                .or_else(|| part["filename"].as_str());
            let attachment = if let Some(data) = source.filter(|s| s.starts_with("data:")) {
                let (header, encoded) = data
                    .split_once(',')
                    .ok_or("attachment data URL has no comma")?;
                let mime = header
                    .strip_prefix("data:")
                    .unwrap()
                    .strip_suffix(";base64")
                    .ok_or("attachment data URL must use base64")?;
                if encoded.len() > LIMIT * 4 / 3 + 4 {
                    return Err("attachment exceeds 8 MiB".into());
                }
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .map_err(|_| "attachment contains invalid base64")?;
                Attachment {
                    name: name.map(str::to_owned).unwrap_or_else(|| {
                        format!(
                            "attachment-{}.{}",
                            files.len() + 1,
                            match mime {
                                "image/png" => "png",
                                "image/jpeg" => "jpg",
                                "application/pdf" => "pdf",
                                _ => "bin",
                            }
                        )
                    }),
                    mime: mime.to_owned(),
                    bytes,
                }
            } else {
                let path = file["path"]
                    .as_str()
                    .or_else(|| file["file_path"].as_str())
                    .or_else(|| source.and_then(|s| s.strip_prefix("file://")));
                let Some(path) = path else { continue }; // Remote URLs/IDs remain in the original payload.
                let metadata = tokio::fs::metadata(path)
                    .await
                    .map_err(|e| format!("attachment {path}: {e}"))?;
                if !metadata.is_file() || metadata.len() > LIMIT as u64 {
                    return Err(format!("attachment {path} must be a file of at most 8 MiB"));
                }
                let bytes = tokio::fs::read(path)
                    .await
                    .map_err(|e| format!("attachment {path}: {e}"))?;
                Attachment {
                    name: name.map(str::to_owned).unwrap_or_else(|| {
                        std::path::Path::new(path)
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                    }),
                    mime: file["mime_type"]
                        .as_str()
                        .unwrap_or("application/octet-stream")
                        .to_owned(),
                    bytes,
                }
            };
            total = total
                .checked_add(attachment.bytes.len())
                .ok_or("attachments too large")?;
            if total > LIMIT || files.len() >= 16 {
                return Err("attachments exceed the 8 MiB / 16-file limit".into());
            }
            files.push(attachment);
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn explicit_inline_and_local_files_are_read_but_prose_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.txt");
        tokio::fs::write(&path, "FILE-TOKEN").await.unwrap();
        let prompt = serde_json::json!({"messages":[{"content":[
            {"type":"text","text":"/etc/passwd"},
            {"type":"image_url","image_url":{"url":"data:image/png;base64,UE5H"}},
            {"type":"file","file":{"path":path,"filename":"note.txt"}}
        ]}]})
        .to_string();
        let files = from_prompt(&prompt).await.unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].mime, "image/png");
        assert_eq!(files[0].bytes, b"PNG");
        assert_eq!(files[1].bytes, b"FILE-TOKEN");
        assert_eq!(files[1].name, "note.txt");
    }
    #[tokio::test]
    async fn malformed_data_and_missing_local_files_fail_explicitly() {
        for file in [
            serde_json::json!({"file_data":"data:image/png;base64,?!"}),
            serde_json::json!({"path":"/nonexistent/attachment"}),
        ] {
            let prompt =
                serde_json::json!({"messages":[{"content":[{"type":"file","file":file}]}]})
                    .to_string();
            assert!(from_prompt(&prompt).await.is_err());
        }
    }
}
