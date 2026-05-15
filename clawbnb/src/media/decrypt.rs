use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use base64::Engine;

/// Parse a CDNMedia.aes_key field into 16 raw bytes.
///
/// Two encodings appear in iLink responses:
///   - base64(raw 16 bytes)           - images via `media.aes_key`
///   - base64(hex of 16 bytes)        - file/voice/video
///
/// In the second case, base64-decoded yields 32 ASCII hex chars; we then
/// decode hex to recover the 16-byte key.
pub fn parse_aes_key(aes_key_base64: &str) -> Result<[u8; 16], String> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(aes_key_base64)
        .map_err(|e| format!("aes_key base64 decode: {e}"))?;
    if decoded.len() == 16 {
        let mut out = [0u8; 16];
        out.copy_from_slice(&decoded);
        return Ok(out);
    }
    if decoded.len() == 32 {
        if let Ok(s) = std::str::from_utf8(&decoded) {
            if s.chars().all(|c| c.is_ascii_hexdigit()) {
                let bytes = hex::decode(s).map_err(|e| format!("aes_key hex decode: {e}"))?;
                if bytes.len() == 16 {
                    let mut out = [0u8; 16];
                    out.copy_from_slice(&bytes);
                    return Ok(out);
                }
            }
        }
    }
    Err(format!(
        "aes_key must decode to 16 raw bytes or 32-char hex, got {} bytes",
        decoded.len()
    ))
}

/// AES-128-ECB encryption with PKCS7 padding.
/// Inverse of `decrypt_aes_ecb_pkcs7` — used for outbound CDN upload.
pub fn encrypt_aes_ecb_pkcs7(plaintext: &[u8], key: &[u8; 16]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    // PKCS7 pad to 16-byte boundary (always pads, even if already aligned).
    let pad_len = 16 - (plaintext.len() % 16);
    let mut padded = Vec::with_capacity(plaintext.len() + pad_len);
    padded.extend_from_slice(plaintext);
    padded.extend(std::iter::repeat(pad_len as u8).take(pad_len));
    for chunk in padded.chunks_exact_mut(16) {
        let mut block = GenericArray::clone_from_slice(chunk);
        cipher.encrypt_block(&mut block);
        chunk.copy_from_slice(&block);
    }
    padded
}

/// AES-ECB ciphertext size for a plaintext of `n` bytes (always rounds up).
pub fn aes_ecb_padded_size(n: usize) -> usize {
    ((n + 1).div_ceil(16)) * 16
}

/// AES-128-ECB decryption with PKCS7 padding.
pub fn decrypt_aes_ecb_pkcs7(ciphertext: &[u8], key: &[u8; 16]) -> Result<Vec<u8>, String> {
    if ciphertext.is_empty() || ciphertext.len() % 16 != 0 {
        return Err(format!(
            "AES-ECB ciphertext must be non-empty multiple of 16, got {}",
            ciphertext.len()
        ));
    }
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = ciphertext.to_vec();
    for chunk in out.chunks_exact_mut(16) {
        let mut block = GenericArray::clone_from_slice(chunk);
        cipher.decrypt_block(&mut block);
        chunk.copy_from_slice(&block);
    }
    // PKCS7 unpadding
    let pad = *out.last().ok_or("empty plaintext")? as usize;
    if pad == 0 || pad > 16 {
        return Err(format!("invalid PKCS7 pad: {pad}"));
    }
    let new_len = out.len().checked_sub(pad).ok_or("pad > len")?;
    out.truncate(new_len);
    Ok(out)
}
