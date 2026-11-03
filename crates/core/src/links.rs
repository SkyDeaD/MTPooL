//! Telegram proxy links.

/// `tg://proxy` link with a fake-TLS secret: `ee` + secret + hex of the SNI domain.
pub fn build_proxy_link(domain: &str, port: u16, secret_hex: &str, sni: &str) -> String {
    let mut sni_hex = String::with_capacity(sni.len() * 2);
    for b in sni.bytes() {
        sni_hex.push_str(&format!("{b:02x}"));
    }
    format!("tg://proxy?server={domain}&port={port}&secret=ee{secret_hex}{sni_hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_matches_telegram_format() {
        assert_eq!(
            build_proxy_link(
                "p.example.com",
                443,
                "00112233445566778899aabbccddeeff",
                "a.b"
            ),
            "tg://proxy?server=p.example.com&port=443&secret=ee00112233445566778899aabbccddeeff612e62"
        );
    }

    #[test]
    fn link_encodes_sni_bytes_as_lowercase_hex() {
        let link = build_proxy_link(
            "p.example.com",
            8443,
            "00112233445566778899aabbccddeeff",
            "Example.COM",
        );
        assert!(
            link.ends_with("ee00112233445566778899aabbccddeeff4578616d706c652e434f4d"),
            "{link}"
        );
        assert!(link.contains("&port=8443&"));
    }
}
