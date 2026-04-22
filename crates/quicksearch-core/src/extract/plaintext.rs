//! Read the file as UTF-8 text. Handles text/plain, text/x-*, application/json
//! and most source-code MIMEs.

use std::path::Path;

use super::{ExtractError, ExtractedContent, Extractor};

pub struct PlaintextExtractor;

impl Extractor for PlaintextExtractor {
    fn supports(&self, mime: &str) -> bool {
        if mime.starts_with("text/") {
            return true;
        }
        matches!(
            mime,
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/x-shellscript"
                | "application/x-python"
                | "application/toml"
                | "application/yaml"
                | "application/x-yaml"
        )
    }

    fn extract(&self, path: &Path) -> Result<ExtractedContent, ExtractError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("plaintext read {}: {}", path.display(), e))?;
        Ok(ExtractedContent::with_text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_utf8_file() {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "qs-plaintext-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&p, "hello world").unwrap();
        let c = PlaintextExtractor.extract(&p).unwrap();
        assert_eq!(c.text, "hello world");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn supports_text_mimes() {
        let e = PlaintextExtractor;
        assert!(e.supports("text/plain"));
        assert!(e.supports("text/x-rust"));
        assert!(e.supports("application/json"));
        assert!(!e.supports("application/pdf"));
        assert!(!e.supports("image/png"));
    }
}
