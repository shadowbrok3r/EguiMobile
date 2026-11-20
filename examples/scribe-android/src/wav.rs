//! 16-bit mono WAV, written so that a recording survives the process dying mid-way.
//!
//! The header's size fields are refreshed every few seconds while samples are appended, so a
//! file abandoned by a crash is at worst a few seconds short by its own account — and
//! [`probe`] ignores that account anyway whenever the file is longer than its header claims,
//! which is exactly the state a killed recorder leaves behind.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

pub const HEADER_LEN: u64 = 44;
/// Samples between header refreshes (5 s at 16 kHz).
const HEADER_EVERY: u64 = 80_000;

/// A canonical 44-byte PCM header for `samples` mono 16-bit samples.
pub fn header(rate: u32, samples: u64) -> [u8; 44] {
    let data = (samples * 2).min(u32::MAX as u64 - 36) as u32;
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&1u16.to_le_bytes());
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * 2).to_le_bytes());
    h[32..34].copy_from_slice(&2u16.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data.to_le_bytes());
    h
}

/// Appends samples to a WAV file that is valid at every moment.
pub struct WavWriter {
    file: File,
    rate: u32,
    samples: u64,
    header_at: u64,
    bytes: Vec<u8>,
}

impl WavWriter {
    pub fn create(path: &Path, rate: u32) -> io::Result<Self> {
        let mut file = OpenOptions::new().write(true).create(true).truncate(true).open(path)?;
        file.write_all(&header(rate, 0))?;
        Ok(Self { file, rate, samples: 0, header_at: 0, bytes: Vec::new() })
    }

    /// Append samples. Unbuffered on purpose: a transcriber reading the same file sees every
    /// sample this has returned from.
    ///
    /// Every write names its own offset. A write that fails part-way (a full disk) is then
    /// simply written over by the next one, on a sample boundary, instead of shifting every
    /// later sample by a byte; and a header refresh that fails cannot leave the cursor at the
    /// start of the file for the next samples to land on.
    pub fn write(&mut self, pcm: &[i16]) -> io::Result<()> {
        self.bytes.clear();
        self.bytes.extend(pcm.iter().flat_map(|s| s.to_le_bytes()));
        self.file.write_all_at(&self.bytes, HEADER_LEN + self.samples * 2)?;
        self.samples += pcm.len() as u64;
        if self.samples - self.header_at >= HEADER_EVERY {
            self.write_header()?;
        }
        Ok(())
    }

    pub fn samples(&self) -> u64 {
        self.samples
    }

    fn write_header(&mut self) -> io::Result<()> {
        self.file.write_all_at(&header(self.rate, self.samples), 0)?;
        self.header_at = self.samples;
        Ok(())
    }

    /// Write the final header and flush to storage. Returns the sample count.
    pub fn finish(mut self) -> io::Result<u64> {
        self.write_header()?;
        self.file.sync_all()?;
        Ok(self.samples)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WavInfo {
    pub rate: u32,
    pub channels: u16,
    /// Frames of audio actually in the file.
    pub samples: u64,
    pub data_offset: u64,
    /// Other chunks follow the audio, so it ends where its header says and cannot grow.
    pub bounded: bool,
}

impl WavInfo {
    pub fn seconds(&self) -> f64 {
        self.samples as f64 / self.rate.max(1) as f64
    }
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

/// Read a 16-bit PCM WAV's layout. The data length comes from the file's real size whenever
/// the header disagrees with it, so a recording cut off before its last header refresh — or
/// before its first — reads back whole.
pub fn probe(path: &Path) -> io::Result<WavInfo> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut riff = [0u8; 12];
    file.read_exact(&mut riff)?;
    if &riff[0..4] != b"RIFF" || &riff[8..12] != b"WAVE" {
        return Err(invalid("not a WAV file"));
    }
    let (mut rate, mut channels, mut bits) = (0u32, 0u16, 0u16);
    let mut at = 12u64;
    loop {
        let mut chunk = [0u8; 8];
        file.seek(SeekFrom::Start(at))?;
        if file.read_exact(&mut chunk).is_err() {
            return Err(invalid("WAV has no data chunk"));
        }
        let size = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]) as u64;
        match &chunk[0..4] {
            b"fmt " => {
                let mut fmt = [0u8; 16];
                file.read_exact(&mut fmt)?;
                if u16::from_le_bytes([fmt[0], fmt[1]]) != 1 {
                    return Err(invalid("WAV is not PCM"));
                }
                channels = u16::from_le_bytes([fmt[2], fmt[3]]);
                rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
                bits = u16::from_le_bytes([fmt[14], fmt[15]]);
            }
            b"data" => {
                if rate == 0 || channels == 0 || bits != 16 {
                    return Err(invalid("WAV is not 16-bit PCM"));
                }
                let data_offset = at + 8;
                let on_disk = len.saturating_sub(data_offset);
                // The header is believed only when what follows its count is more chunks. A
                // live or abandoned recording holds more audio than its header says instead.
                let bounded = size > 0 && tiles_to_end(&mut file, data_offset + size + (size & 1), len);
                let bytes = if bounded { size } else { on_disk };
                return Ok(WavInfo { rate, channels, samples: bytes / (2 * channels as u64), data_offset, bounded });
            }
            _ => {}
        }
        at += 8 + size + (size & 1);
    }
}

/// Whether the bytes from `at` are well-formed chunks that end exactly at `len`. Audio almost
/// never parses as that; chunks a tool appended after the audio always do.
fn tiles_to_end(file: &mut File, mut at: u64, len: u64) -> bool {
    if at >= len {
        return false;
    }
    while at < len {
        let mut chunk = [0u8; 8];
        if file.seek(SeekFrom::Start(at)).is_err() || file.read_exact(&mut chunk).is_err() {
            return false;
        }
        if !chunk[..4].iter().all(|b| b.is_ascii_alphanumeric() || *b == b' ') {
            return false;
        }
        let size = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]) as u64;
        at += 8 + size + (size & 1);
    }
    at == len
}

/// Rewrite a canonical recording's header from its real length, and return its layout. The
/// rewrite is a courtesy to other players — this app reads the length off the file — so it
/// failing (a full or read-only volume) is not a failure to read the recording.
pub fn repair(path: &Path) -> io::Result<WavInfo> {
    let info = probe(path)?;
    if info.data_offset == HEADER_LEN && info.channels == 1 {
        let rewrite = OpenOptions::new().write(true).open(path).and_then(|file| {
            file.write_all_at(&header(info.rate, info.samples), 0)?;
            file.sync_all()
        });
        if let Err(e) = rewrite {
            log::warn!("header of {} not rewritten: {e}", path.display());
        }
    }
    Ok(info)
}

/// Random access to a WAV's samples as mono floats, including one still being written.
pub struct WavReader {
    file: File,
    info: WavInfo,
    bytes: Vec<u8>,
}

impl WavReader {
    pub fn open(path: &Path) -> io::Result<Self> {
        let info = probe(path)?;
        Ok(Self { file: File::open(path)?, info, bytes: Vec::new() })
    }

    pub fn info(&self) -> WavInfo {
        self.info
    }

    /// Copy frames from `start` into `out` as -1..1, mixing channels down. Returns how many
    /// frames the file held from there — which grows while a recorder is appending.
    pub fn read(&mut self, start: u64, out: &mut [f32]) -> io::Result<usize> {
        let channels = self.info.channels as usize;
        let frame = 2 * channels;
        // Past a bounded file's audio lie other chunks, not samples.
        let wanted = if self.info.bounded { out.len().min(self.info.samples.saturating_sub(start) as usize) } else { out.len() };
        self.bytes.resize(wanted * frame, 0);
        self.file.seek(SeekFrom::Start(self.info.data_offset + start * frame as u64))?;
        let mut filled = 0;
        while filled < self.bytes.len() {
            match self.file.read(&mut self.bytes[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        let frames = filled / frame;
        for (slot, chunk) in out.iter_mut().zip(self.bytes[..frames * frame].chunks_exact(frame)) {
            let sum: i32 = chunk.chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]]) as i32).sum();
            *slot = sum as f32 / (32768.0 * channels as f32);
        }
        Ok(frames)
    }
}

impl local_whisper::AudioSource for WavReader {
    fn read(&mut self, start: u64, out: &mut [f32]) -> local_whisper::Result<usize> {
        Ok(WavReader::read(self, start, out)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("scribe-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn round_trips_and_reports_its_length() {
        let path = scratch("round.wav");
        let mut writer = WavWriter::create(&path, 16_000).unwrap();
        let pcm: Vec<i16> = (0..32_000).map(|i| ((i % 200) - 100) as i16 * 100).collect();
        writer.write(&pcm[..10_000]).unwrap();
        writer.write(&pcm[10_000..]).unwrap();
        assert_eq!(writer.finish().unwrap(), 32_000);

        let info = probe(&path).unwrap();
        assert_eq!(info, WavInfo { rate: 16_000, channels: 1, samples: 32_000, data_offset: 44, bounded: false });
        assert_eq!(info.seconds(), 2.0);
        let mut reader = WavReader::open(&path).unwrap();
        let mut out = vec![0f32; 100];
        assert_eq!(reader.read(31_950, &mut out).unwrap(), 50);
        assert_eq!(out[0], pcm[31_950] as f32 / 32768.0);
        assert_eq!(reader.read(40_000, &mut out).unwrap(), 0);
    }

    #[test]
    fn samples_keep_their_place_across_header_refreshes() {
        let path = scratch("refresh.wav");
        let mut writer = WavWriter::create(&path, 16_000).unwrap();
        let pcm: Vec<i16> = (0..250_000).map(|i| (i % 30_000) as i16).collect();
        // 100 ms pieces, as the recorder delivers them: three header refreshes on the way.
        for piece in pcm.chunks(1_600) {
            writer.write(piece).unwrap();
        }
        assert_eq!(writer.finish().unwrap(), 250_000);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 44 + 500_000);
        assert_eq!(&bytes[..44], &header(16_000, 250_000));
        let back: Vec<i16> = bytes[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        assert_eq!(back, pcm);
    }

    #[test]
    fn a_recording_cut_off_mid_write_reads_back_whole() {
        let path = scratch("crashed.wav");
        let mut writer = WavWriter::create(&path, 16_000).unwrap();
        writer.write(&vec![7i16; 100_000]).unwrap();
        // The header was refreshed at 100k samples; these never made it in.
        writer.write(&vec![9i16; 4_321]).unwrap();
        std::mem::forget(writer);

        let info = probe(&path).unwrap();
        assert_eq!(info.samples, 104_321);
        assert_eq!(repair(&path).unwrap().samples, 104_321);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..44], &header(16_000, 104_321));
    }

    #[test]
    fn a_recording_killed_before_any_header_refresh_still_has_its_audio() {
        let path = scratch("early.wav");
        let mut writer = WavWriter::create(&path, 16_000).unwrap();
        writer.write(&vec![3i16; 1_000]).unwrap();
        std::mem::forget(writer);
        // The header still says zero bytes of data.
        assert_eq!(probe(&path).unwrap().samples, 1_000);
    }

    #[test]
    fn a_growing_file_is_readable_while_it_is_written() {
        let path = scratch("live.wav");
        let mut writer = WavWriter::create(&path, 16_000).unwrap();
        writer.write(&vec![100i16; 500]).unwrap();
        let mut reader = WavReader::open(&path).unwrap();
        let mut out = vec![0f32; 1_000];
        assert_eq!(reader.read(0, &mut out).unwrap(), 500);
        writer.write(&vec![200i16; 300]).unwrap();
        assert_eq!(reader.read(0, &mut out).unwrap(), 800);
        assert_eq!(out[799], 200.0 / 32768.0);
    }

    #[test]
    fn foreign_wavs_with_extra_chunks_and_stereo_are_understood() {
        let path = scratch("foreign.wav");
        let mut bytes = Vec::new();
        bytes.extend(b"RIFF\0\0\0\0WAVE");
        bytes.extend(b"LIST");
        bytes.extend(4u32.to_le_bytes());
        bytes.extend(b"INFO");
        bytes.extend(b"fmt ");
        bytes.extend(16u32.to_le_bytes());
        bytes.extend(1u16.to_le_bytes());
        bytes.extend(2u16.to_le_bytes());
        bytes.extend(44_100u32.to_le_bytes());
        bytes.extend((44_100u32 * 4).to_le_bytes());
        bytes.extend(4u16.to_le_bytes());
        bytes.extend(16u16.to_le_bytes());
        bytes.extend(b"data");
        bytes.extend(8u32.to_le_bytes());
        for s in [1000i16, 3000, -2000, -4000] {
            bytes.extend(s.to_le_bytes());
        }
        bytes.extend(b"junk");
        bytes.extend(0u32.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();

        let mut reader = WavReader::open(&path).unwrap();
        assert_eq!(reader.info().samples, 2);
        assert_eq!(reader.info().channels, 2);
        assert!(reader.info().bounded);
        let mut out = [0f32; 4];
        assert_eq!(reader.read(0, &mut out).unwrap(), 2);
        assert_eq!(out[0], 2000.0 / 32768.0);
        assert_eq!(out[1], -3000.0 / 32768.0);
    }

    #[test]
    fn non_wav_and_non_pcm_are_refused() {
        let path = scratch("text.wav");
        std::fs::write(&path, b"this is not audio at all").unwrap();
        assert!(probe(&path).is_err());
    }
}
