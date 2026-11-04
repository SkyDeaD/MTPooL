//! Traffic of a real client: the way a Telegram client reaches a FakeTLS proxy (a ClientHello
//! that proves knowledge of the user's secret, then an obfuscated MTProto handshake and some
//! data inside TLS application records), sent from one node to another. Only what telemt needs
//! to count the bytes as a user's is implemented; the answers of the proxy are not read.

use aes::Aes256;
use aes::cipher::{KeyIvInit, StreamCipher};
use ctr::Ctr128BE;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

use crate::{Node, Result, StandError, fail};

type Aes256Ctr = Ctr128BE<Aes256>;

/// The record types and the version of the TLS that telemt's FakeTLS speaks.
const RECORD_HANDSHAKE: u8 = 0x16;
const RECORD_CHANGE_CIPHER: u8 = 0x14;
const RECORD_APPLICATION: u8 = 0x17;
/// The most one record may hold.
const RECORD_MAX: usize = 16_384;
/// The protocol tag of the padded intermediate transport, which FakeTLS clients use.
const TAG_SECURE: [u8; 4] = [0xdd; 4];
/// The Telegram data centre the handshake names.
const DC_INDEX: i16 = 2;

/// Bytes that differ from one call to the next. They need to be unpredictable to nothing but a
/// test, so a hash of the clock and a counter will do.
fn noise(len: usize) -> Vec<u8> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut out = Vec::with_capacity(len + 32);
    let mut block = 0u64;
    while out.len() < len {
        let mut hash = Sha256::new();
        hash.update(nanos.to_le_bytes());
        hash.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
        hash.update(block.to_le_bytes());
        out.extend_from_slice(&hash.finalize());
        block += 1;
    }
    out.truncate(len);
    out
}

fn decode_secret(secret_hex: &str) -> Result<Vec<u8>> {
    data_encoding::HEXLOWER_PERMISSIVE
        .decode(secret_hex.as_bytes())
        .ok()
        .filter(|s| s.len() == 16)
        .ok_or_else(|| StandError("the user secret is not 32 hex digits".to_owned()))
}

/// A ClientHello whose random field proves that the sender knows `secret`.
fn client_hello(secret: &[u8], host: &str, unix_time: u32) -> Vec<u8> {
    let mut body = vec![0x03, 0x03];
    body.extend_from_slice(&[0u8; 32]); // the random field: filled in below
    body.push(32);
    body.extend_from_slice(&noise(32)); // the session id
    let suites: [u16; 3] = [0x1301, 0x1302, 0x1303];
    body.extend_from_slice(&((suites.len() * 2) as u16).to_be_bytes());
    for suite in suites {
        body.extend_from_slice(&suite.to_be_bytes());
    }
    body.extend_from_slice(&[1, 0]); // compression: none

    let mut extensions = Vec::new();
    let mut extension = |kind: u16, data: &[u8]| {
        extensions.extend_from_slice(&kind.to_be_bytes());
        extensions.extend_from_slice(&(data.len() as u16).to_be_bytes());
        extensions.extend_from_slice(data);
    };
    let name = host.as_bytes();
    let mut sni = Vec::new();
    sni.extend_from_slice(&(name.len() as u16 + 3).to_be_bytes());
    sni.push(0);
    sni.extend_from_slice(&(name.len() as u16).to_be_bytes());
    sni.extend_from_slice(name);
    extension(0x0000, &sni);
    extension(0x000a, &[0x00, 0x02, 0x00, 0x1d]); // supported groups: x25519
    extension(0x002b, &[0x02, 0x03, 0x04]); // supported versions: TLS 1.3
    let mut key_share = vec![0x00, 0x24, 0x00, 0x1d, 0x00, 0x20];
    key_share.extend_from_slice(&noise(32));
    extension(0x0033, &key_share);
    body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
    body.extend_from_slice(&extensions);

    let mut handshake = vec![0x01];
    handshake.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    handshake.extend_from_slice(&body);
    let mut record = vec![RECORD_HANDSHAKE, 0x03, 0x01];
    record.extend_from_slice(&(handshake.len() as u16).to_be_bytes());
    record.extend_from_slice(&handshake);

    // The random field is the HMAC of the whole record (that field being zero) under the secret,
    // its last four bytes xor-ed with the time.
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("any key length");
    mac.update(&record);
    let mut digest: [u8; 32] = mac.finalize().into_bytes().into();
    for (byte, time) in digest[28..].iter_mut().zip(unix_time.to_le_bytes()) {
        *byte ^= time;
    }
    record[11..43].copy_from_slice(&digest);
    record
}

/// Everything a client sends to open a session and push `payload_len` bytes through it.
fn session_bytes(secret: &[u8], host: &str, unix_time: u32, payload_len: usize) -> Result<Vec<u8>> {
    let mut sent = client_hello(secret, host, unix_time);
    sent.extend_from_slice(&[RECORD_CHANGE_CIPHER, 0x03, 0x03, 0x00, 0x01, 0x01]);

    // The obfuscated handshake: 64 bytes, of which the key material is in the open and the
    // protocol tag and data centre are encrypted with the key it makes.
    let mut init = noise(64);
    // Words an obfuscated handshake must not start with: they would pass for other protocols.
    init[0] = 0x5a;
    init[4..8].copy_from_slice(&[0x11, 0x22, 0x33, 0x44]);
    let mut plain = init.clone();
    plain[56..60].copy_from_slice(&TAG_SECURE);
    plain[60..62].copy_from_slice(&DC_INDEX.to_le_bytes());
    let mut key_material = init[8..40].to_vec();
    key_material.extend_from_slice(secret);
    let key = Sha256::digest(&key_material);
    let mut cipher = Aes256Ctr::new_from_slices(&key, &init[40..56])
        .map_err(|e| StandError(format!("cannot start the cipher: {e}")))?;
    let mut encrypted = plain;
    cipher.apply_keystream(&mut encrypted);
    let mut handshake = init[..56].to_vec();
    handshake.extend_from_slice(&encrypted[56..]);

    // The data: frames of the padded intermediate transport (a length, then the bytes), which
    // telemt carries to the data centre without looking into.
    let mut data = Vec::with_capacity(payload_len + 4);
    data.extend_from_slice(&(payload_len as u32).to_le_bytes());
    data.extend_from_slice(&noise(payload_len));
    cipher.apply_keystream(&mut data);

    let mut stream = handshake;
    stream.extend_from_slice(&data);
    for chunk in stream.chunks(RECORD_MAX) {
        sent.extend_from_slice(&[RECORD_APPLICATION, 0x03, 0x03]);
        sent.extend_from_slice(&(chunk.len() as u16).to_be_bytes());
        sent.extend_from_slice(chunk);
    }
    Ok(sent)
}

impl Node<'_> {
    /// Opens a FakeTLS session from this node to `target_ip:port` as the user whose secret is
    /// `secret_hex`, sends `payload_len` bytes through it and closes it. Returns how many bytes
    /// were put on the wire.
    pub async fn send_proxy_traffic(
        &self,
        target_ip: &str,
        port: u16,
        secret_hex: &str,
        tls_domain: &str,
        payload_len: usize,
    ) -> Result<usize> {
        if !target_ip.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
            return fail(format!("{target_ip} is not an IPv4 address"));
        }
        let secret = decode_secret(secret_hex)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| StandError(format!("the clock is before 1970: {e}")))?
            .as_secs() as u32;
        let bytes = session_bytes(&secret, tls_domain, now, payload_len)?;
        let encoded = data_encoding::BASE64.encode(&bytes);
        // bash for /dev/tcp. The connection stays open a few seconds, so that the proxy has read
        // and relayed what was sent before it sees the end of the stream.
        let command =
            format!("bash -c 'exec 3<>/dev/tcp/{target_ip}/{port} && base64 -d >&3 && sleep 4'");
        let run = self.try_exec_with(&command, &[], Some(&encoded)).await?;
        if !run.success {
            return fail(format!(
                "the client on {} failed: {}",
                self.service, run.output
            ));
        }
        Ok(bytes.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hello_is_a_tls_record_that_carries_its_own_proof() {
        let secret = [7u8; 16];
        let hello = client_hello(&secret, "www.example.com", 1_700_000_000);
        assert_eq!(hello[0], RECORD_HANDSHAKE);
        let declared = u16::from_be_bytes([hello[3], hello[4]]) as usize;
        assert_eq!(hello.len(), 5 + declared);
        // What the proxy does: zero the random field, recompute the HMAC, and compare.
        let mut zeroed = hello.clone();
        zeroed[11..43].fill(0);
        let mut mac = Hmac::<Sha256>::new_from_slice(&secret).unwrap();
        mac.update(&zeroed);
        let computed: [u8; 32] = mac.finalize().into_bytes().into();
        assert_eq!(hello[11..39], computed[..28]);
        let time: Vec<u8> = (0..4).map(|i| hello[39 + i] ^ computed[28 + i]).collect();
        assert_eq!(time, 1_700_000_000u32.to_le_bytes());
    }

    #[test]
    fn a_session_is_hello_cipher_change_and_records() {
        let sent = session_bytes(&[1u8; 16], "www.example.com", 1, 40_000).unwrap();
        let hello_len = 5 + u16::from_be_bytes([sent[3], sent[4]]) as usize;
        assert_eq!(sent[hello_len], RECORD_CHANGE_CIPHER);
        let mut at = hello_len + 6;
        let mut carried = 0;
        while at < sent.len() {
            assert_eq!(sent[at], RECORD_APPLICATION);
            let len = u16::from_be_bytes([sent[at + 3], sent[at + 4]]) as usize;
            assert!(len <= RECORD_MAX);
            carried += len;
            at += 5 + len;
        }
        assert_eq!(at, sent.len());
        assert_eq!(carried, 64 + 4 + 40_000);
    }
}
