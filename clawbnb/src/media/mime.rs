/// Sniff image format from leading bytes and return common extension.
pub fn sniff_image_ext(bytes: &[u8]) -> &'static str {
    if bytes.len() < 4 {
        return ".bin";
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return ".jpg";
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        return ".png";
    }
    if bytes.starts_with(b"GIF8") {
        return ".gif";
    }
    if bytes.starts_with(b"RIFF") && bytes.len() >= 12 && &bytes[8..12] == b"WEBP" {
        return ".webp";
    }
    if bytes.starts_with(&[0x42, 0x4D]) {
        return ".bmp";
    }
    ".bin"
}

/// Get extension from filename if recognizable, otherwise ".bin".
pub fn ext_from_filename(name: &str) -> String {
    if let Some(dot) = name.rfind('.') {
        let ext = name[dot..].to_lowercase();
        if ext.len() <= 8 && ext.chars().skip(1).all(|c| c.is_ascii_alphanumeric()) {
            return ext;
        }
    }
    ".bin".to_string()
}
