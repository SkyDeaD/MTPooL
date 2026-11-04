//! Telegram proxy links.

/// The secret of a fake-TLS link: `ee` + secret + hex of the SNI domain.
pub fn link_secret(secret_hex: &str, sni: &str) -> String {
    let mut text = String::with_capacity(2 + secret_hex.len() + sni.len() * 2);
    text.push_str("ee");
    text.push_str(secret_hex);
    for b in sni.bytes() {
        text.push_str(&format!("{b:02x}"));
    }
    text
}

/// `tg://proxy` link with a fake-TLS secret: `ee` + secret + hex of the SNI domain.
pub fn build_proxy_link(domain: &str, port: u16, secret_hex: &str, sni: &str) -> String {
    let secret = link_secret(secret_hex, sni);
    format!("tg://proxy?server={domain}&port={port}&secret={secret}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_secret_is_ee_secret_and_hex_of_the_sni() {
        assert_eq!(
            link_secret("00112233445566778899aabbccddeeff", "a.b"),
            "ee00112233445566778899aabbccddeeff612e62"
        );
        assert_eq!(link_secret("00", ""), "ee00");
    }

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
