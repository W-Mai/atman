pub mod anthropic;
pub mod mock;
pub mod openai;

pub(crate) fn classify_attachment_error(status: u16, body: &str) -> Option<String> {
    if status == 413 {
        return Some("payload_too_large".into());
    }
    let lower = body.to_ascii_lowercase();
    let attachment_markers = [
        "image",
        "attachment",
        "media_type",
        "unsupported",
        "invalid image",
        "image_parse_error",
        "invalid_image_url",
        "request_too_large",
    ];
    let has_marker = attachment_markers.iter().any(|m| lower.contains(m));
    let is_size_signal = lower.contains("too large") || lower.contains("payload_too_large");
    if status == 400 && has_marker {
        return Some(pick_reason(&lower));
    }
    if is_size_signal {
        return Some("payload_too_large".into());
    }
    None
}

fn pick_reason(lower: &str) -> String {
    if lower.contains("invalid_image_url") {
        "invalid_image_url".into()
    } else if lower.contains("image_parse_error") {
        "image_parse_error".into()
    } else if lower.contains("unsupported") {
        "unsupported_media_type".into()
    } else if lower.contains("too large") {
        "payload_too_large".into()
    } else {
        "invalid_image".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_413_maps_to_payload_too_large() {
        let r = classify_attachment_error(413, "").unwrap();
        assert_eq!(r, "payload_too_large");
    }

    #[test]
    fn openai_invalid_image_url_recognized() {
        let body = r#"{"error":{"code":"invalid_image_url","message":"..."}}"#;
        let r = classify_attachment_error(400, body).unwrap();
        assert_eq!(r, "invalid_image_url");
    }

    #[test]
    fn anthropic_image_error_recognized() {
        let body = "unsupported media_type for image";
        let r = classify_attachment_error(400, body).unwrap();
        assert_eq!(r, "unsupported_media_type");
    }

    #[test]
    fn non_attachment_error_returns_none() {
        assert!(classify_attachment_error(500, "internal").is_none());
        assert!(classify_attachment_error(401, "unauthorized").is_none());
    }
}
