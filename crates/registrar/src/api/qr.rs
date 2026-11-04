//! QR codes of proxy links, as SVG.

use fast_qr::ECL;
use fast_qr::convert::svg::SvgBuilder;
use fast_qr::qr::QRBuilder;

/// The SVG picture of the QR code of `text`; `None` if the text does not fit in a code.
pub fn svg(text: &str) -> Option<String> {
    let code = QRBuilder::new(text).ecl(ECL::M).build().ok()?;
    Some(SvgBuilder::default().to_str(&code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_an_svg() {
        let picture = svg("tg://proxy?server=proxy.example.com&port=443").unwrap();
        assert!(picture.starts_with("<svg"));
    }

    #[test]
    fn text_too_long_for_a_code_gives_none() {
        assert!(svg(&"x".repeat(10_000)).is_none());
    }
}
