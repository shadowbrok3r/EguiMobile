//! QUIC Initial packets, for the hostname a UDP 443 flow carries.
//!
//! Initial packets are encrypted with keys anyone can derive from the client's destination
//! connection ID (RFC 9001 §5.2), so the ClientHello inside reads like TCP's. A post-quantum key
//! share no longer fits one packet, so CRYPTO frames are reassembled across datagrams.

use rustls::Side;
use rustls::quic::{Keys, Version};

/// Datagrams to inspect before giving up on a hello.
pub const MAX_DATAGRAMS: usize = 8;
const MAX_CID: usize = 20;
const MAX_HELLO: usize = super::sniff::MAX_HELLO;

pub enum Sniffed {
    /// The hello is incomplete; hold the datagram and read the next.
    More,
    /// The complete ClientHello handshake message.
    Hello(Vec<u8>),
    /// Not a client Initial this can read.
    Unreadable,
}

/// Collects one flow's client Initial packets until its ClientHello is whole.
#[derive(Default)]
pub struct Sniffer {
    keys: Option<(Keys, Vec<u8>)>,
    segments: Vec<(u64, Vec<u8>)>,
    held: usize,
    datagrams: usize,
}

impl Sniffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, datagram: &[u8]) -> Sniffed {
        self.datagrams += 1;
        let first = self.keys.is_none();
        let mut opened = false;
        let mut at = 0;
        while let Some(packet) = datagram.get(at..).and_then(Packet::parse) {
            let bytes = &datagram[at..at + packet.len];
            at += packet.len;
            if !packet.initial {
                continue;
            }
            let (keys, dcid) = self
                .keys
                .get_or_insert_with(|| (initial_keys(packet.version, packet.dcid), packet.dcid.to_vec()));
            // Every client Initial before the server answers reuses the first DCID.
            if dcid.as_slice() != packet.dcid {
                continue;
            }
            let Some(plain) = open(keys, bytes, packet.pn_offset) else {
                continue;
            };
            opened = true;
            if self.read_frames(&plain).is_none() {
                return Sniffed::Unreadable;
            }
        }

        // A genuine first Initial always decrypts; anything else on UDP 443 is not QUIC.
        if first && !opened {
            return Sniffed::Unreadable;
        }
        match self.hello() {
            Some(Ok(hello)) => Sniffed::Hello(hello),
            Some(Err(())) => Sniffed::Unreadable,
            None if self.datagrams >= MAX_DATAGRAMS => Sniffed::Unreadable,
            None => Sniffed::More,
        }
    }

    fn read_frames(&mut self, plain: &[u8]) -> Option<()> {
        let mut frames = Cursor::new(plain);
        while !frames.is_empty() {
            match frames.varint()? {
                // PADDING, PING.
                0x00 | 0x01 => {}
                kind @ (0x02 | 0x03) => {
                    frames.varint()?; // largest acknowledged
                    frames.varint()?; // delay
                    let ranges = frames.varint()?;
                    frames.varint()?; // first range
                    for _ in 0..ranges {
                        frames.varint()?;
                        frames.varint()?;
                    }
                    if kind == 0x03 {
                        for _ in 0..3 {
                            frames.varint()?;
                        }
                    }
                }
                0x06 => {
                    let offset = frames.varint()?;
                    let len = usize::try_from(frames.varint()?).ok()?;
                    let data = frames.take(len)?;
                    let end = offset.checked_add(len as u64)?;
                    self.held = self.held.checked_add(len)?;
                    if end > MAX_HELLO as u64 || self.held > 2 * MAX_HELLO {
                        return None;
                    }
                    self.segments.push((offset, data.to_vec()));
                }
                0x1c => {
                    frames.varint()?; // error code
                    frames.varint()?; // frame type
                    let len = usize::try_from(frames.varint()?).ok()?;
                    frames.take(len)?;
                }
                // Nothing else is allowed in an Initial.
                _ => return None,
            }
        }
        Some(())
    }

    /// The hello once the CRYPTO stream holds all of it; `Err` when it is not a ClientHello.
    fn hello(&self) -> Option<Result<Vec<u8>, ()>> {
        let mut stream: Vec<u8> = Vec::new();
        loop {
            let have = stream.len() as u64;
            let next = self
                .segments
                .iter()
                .filter(|(offset, data)| *offset <= have && offset + data.len() as u64 > have)
                .max_by_key(|(offset, data)| offset + data.len() as u64);
            let Some((offset, data)) = next else {
                break;
            };
            stream.extend_from_slice(&data[(have - offset) as usize..]);
        }

        let header = stream.get(..4)?;
        if header[0] != 0x01 {
            return Some(Err(()));
        }
        let len = 4 + (usize::from(header[1]) << 16 | usize::from(header[2]) << 8 | usize::from(header[3]));
        if len > MAX_HELLO {
            return Some(Err(()));
        }
        (stream.len() >= len).then(|| {
            stream.truncate(len);
            Ok(stream)
        })
    }
}

/// A long-header packet's layout within a datagram.
struct Packet<'a> {
    version: Version,
    initial: bool,
    dcid: &'a [u8],
    pn_offset: usize,
    len: usize,
}

impl<'a> Packet<'a> {
    fn parse(data: &'a [u8]) -> Option<Self> {
        let first = *data.first()?;
        // Short headers belong to established connections and carry no handshake.
        if first & 0x80 == 0 {
            return None;
        }
        let raw_version = u32::from_be_bytes(data.get(1..5)?.try_into().ok()?);
        let (version, v2) = match raw_version {
            0x0000_0001 => (Version::V1, false),
            0x6b33_43cf => (Version::V2, true),
            0xff00_001d..=0xff00_0020 => (Version::V1Draft, false),
            _ => return None,
        };
        // RFC 9369 rotates the type bits: Initial is 1 and Retry 0 in v2, 0 and 3 in v1.
        let kind = (first >> 4) & 0x03;
        let (initial, retry) = if v2 { (kind == 1, kind == 0) } else { (kind == 0, kind == 3) };
        if retry {
            return None;
        }

        let mut header = Cursor::new(data);
        header.skip(5)?;
        let dcid_len = usize::from(header.u8()?);
        if dcid_len > MAX_CID {
            return None;
        }
        let dcid = header.take(dcid_len)?;
        let scid_len = usize::from(header.u8()?);
        if scid_len > MAX_CID {
            return None;
        }
        header.skip(scid_len)?;
        if initial {
            let token = usize::try_from(header.varint()?).ok()?;
            header.skip(token)?;
        }
        let length = usize::try_from(header.varint()?).ok()?;
        let pn_offset = header.at;
        let len = pn_offset.checked_add(length)?;
        (len <= data.len()).then_some(Self {
            version,
            initial,
            dcid,
            pn_offset,
            len,
        })
    }
}

fn initial_keys(version: Version, dcid: &[u8]) -> Keys {
    let rustls::SupportedCipherSuite::Tls13(suite) =
        rustls::crypto::ring::cipher_suite::TLS13_AES_128_GCM_SHA256
    else {
        unreachable!("TLS13_AES_128_GCM_SHA256 is a TLS 1.3 suite");
    };
    let suite = suite.quic_suite().expect("ring supports QUIC with AES-128-GCM");
    // The server side's remote keys are the client's, which is the direction the tun sees.
    suite.keys(dcid, Side::Server, version)
}

/// Removes header protection and decrypts one Initial packet's payload.
fn open(keys: &Keys, packet: &[u8], pn_offset: usize) -> Option<Vec<u8>> {
    let sample_start = pn_offset.checked_add(4)?;
    let sample = packet.get(sample_start..sample_start + keys.remote.header.sample_len())?;
    let mut first = packet[0];
    let mut number = [0u8; 4];
    number.copy_from_slice(packet.get(pn_offset..sample_start)?);
    keys.remote
        .header
        .decrypt_in_place(sample, &mut first, &mut number)
        .ok()?;

    let pn_len = usize::from(first & 0x03) + 1;
    let mut header = packet[..pn_offset + pn_len].to_vec();
    header[0] = first;
    header[pn_offset..].copy_from_slice(&number[..pn_len]);
    // No earlier packet to decode against, so the truncated number is the number.
    let number = number[..pn_len]
        .iter()
        .fold(0_u64, |acc, byte| acc << 8 | u64::from(*byte));
    let mut payload = packet[pn_offset + pn_len..].to_vec();
    let plain = keys
        .remote
        .packet
        .decrypt_in_place(number, &header, &mut payload)
        .ok()?;
    Some(plain.to_vec())
}

struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn is_empty(&self) -> bool {
        self.at >= self.data.len()
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(len)?;
        let bytes = self.data.get(self.at..end)?;
        self.at = end;
        Some(bytes)
    }

    fn skip(&mut self, len: usize) -> Option<()> {
        self.take(len).map(|_| ())
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|bytes| bytes[0])
    }

    /// RFC 9000 §16 variable-length integer.
    fn varint(&mut self) -> Option<u64> {
        let first = self.u8()?;
        let rest = self.take((1 << (first >> 6)) - 1)?;
        Some(
            rest.iter()
                .fold(u64::from(first & 0x3f), |acc, byte| acc << 8 | u64::from(*byte)),
        )
    }
}

#[cfg(test)]
#[path = "quic_tests.rs"]
pub(crate) mod tests;
