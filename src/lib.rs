pub mod auth;
pub mod cli;
pub mod config;
pub mod daemon;
#[cfg(windows)]
mod desktop;
pub mod graph;
mod http;
pub mod img_preview;
pub mod mcp;
pub mod models;
pub mod parser;
pub mod resolver;
pub mod serve;
pub mod storage;
pub mod sync;
pub mod tui;
pub mod ui;
pub mod updater;

pub(crate) fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    const LUT: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(LUT[(b >> 4) as usize] as char);
        out.push(LUT[(b & 0x0f) as usize] as char);
    }
    out
}

fn uuid_v4_bytes() -> [u8; 16] {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes
}

pub(crate) fn uuid_v4_hyphenated() -> String {
    let hex = hex_encode(uuid_v4_bytes());
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

pub(crate) fn uuid_v4_simple() -> String {
    hex_encode(uuid_v4_bytes())
}

#[cfg(test)]
mod uuid_tests {
    use super::*;

    #[test]
    fn test_uuid_v4_format_and_rfc4122_bits() {
        let hyphenated = uuid_v4_hyphenated();
        assert_eq!(hyphenated.len(), 36);
        let bytes = hyphenated.as_bytes();
        assert_eq!(bytes[8], b'-');
        assert_eq!(bytes[13], b'-');
        assert_eq!(bytes[18], b'-');
        assert_eq!(bytes[23], b'-');
        assert_eq!(bytes[14], b'4'); // RFC 4122 version 4
        assert!(matches!(bytes[19], b'8' | b'9' | b'a' | b'b')); // RFC 4122 variant 1

        let simple = uuid_v4_simple();
        assert_eq!(simple.len(), 32);
        assert!(simple.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(simple.as_bytes()[12], b'4');
        assert!(matches!(simple.as_bytes()[16], b'8' | b'9' | b'a' | b'b'));
        assert_ne!(hyphenated.replace('-', ""), simple);
    }
}
