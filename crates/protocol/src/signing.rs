//! Release signatures are minisign signatures. Both binaries carry the public key they are
//! built with; this reads the key id off it.

use data_encoding::BASE64;

/// The key id of a minisign public key (the base64 line of the `.pub` file) as minisign writes
/// it in the key's comment: 16 upper-case hex digits. `None` when `key` is not such a key.
pub fn key_id(key: &str) -> Option<String> {
    let bytes = BASE64.decode(key.trim().as_bytes()).ok()?;
    // "Ed", 8 bytes of key id, 32 bytes of Ed25519 key.
    if bytes.len() != 42 || &bytes[..2] != b"Ed" {
        return None;
    }
    let id: [u8; 8] = bytes[2..10].try_into().ok()?;
    Some(format!("{:016X}", u64::from_le_bytes(id)))
}

#[cfg(test)]
mod tests {
    use super::key_id;

    /// The example key of the minisign documentation.
    const README_KEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";

    #[test]
    fn key_id_matches_the_comment_minisign_writes() {
        assert_eq!(key_id(README_KEY).as_deref(), Some("E7620F1842B4E81F"));
    }

    #[test]
    fn key_id_of_nothing_or_garbage_is_none() {
        assert_eq!(key_id(""), None);
        assert_eq!(key_id("not a key"), None);
        // Valid base64 of the right size, but not an Ed25519 minisign key (no "Ed" tag).
        assert_eq!(key_id(&"A".repeat(56)), None);
    }
}
