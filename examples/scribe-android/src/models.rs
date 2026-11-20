//! Model packs: which ones exist, which are installed, and fetching one.
//!
//! The packs are Qualcomm AI Hub's precompiled Whisper context binaries for this exact chip
//! (Snapdragon 8 Elite Gen 5 for Galaxy, HTP V81), pinned to the last release compiled with
//! QAIRT 2.45. That pin is deliberate: a context binary loads on a runtime at least as new as
//! the compiler that made it, and the QNN runtime this app bundles is 2.48 — newer releases
//! are built with 2.50 and would be refused.

use local_whisper::{WhisperPack, DECODER, ENCODER};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::time::Duration;

const RELEASE: &str = "https://qaihub-public-assets.s3.us-west-2.amazonaws.com/qai-hub-models/models";
const TARGET: &str = "qnn_context_binary-float-qualcomm_snapdragon_8_elite_gen5_for_galaxy";

pub struct ModelSpec {
    /// Pack directory name, and how settings refer to it.
    pub name: &'static str,
    pub label: &'static str,
    pub blurb: &'static str,
    /// AI Hub's id for the model, as it appears in the release path.
    hub_id: &'static str,
    pub zip_bytes: u64,
    pub encoder_bytes: u64,
    pub decoder_bytes: u64,
}

impl ModelSpec {
    pub fn url(&self) -> String {
        format!("{RELEASE}/{}/releases/v0.62.2/{}-{TARGET}.zip", self.hub_id, self.hub_id)
    }

    /// Bytes the pack occupies once installed.
    pub fn installed_bytes(&self) -> u64 {
        self.encoder_bytes + self.decoder_bytes
    }

    /// Free space an install needs: the zip and its contents exist side by side for a while.
    pub fn space_needed(&self) -> u64 {
        self.zip_bytes + self.installed_bytes() + 64 * 1024 * 1024
    }
}

/// Best first. Sizes are the release's own, and double as the integrity check after extraction.
pub const CATALOG: [ModelSpec; 3] = [
    ModelSpec {
        name: "whisper-large-v3-turbo",
        label: "Large v3 Turbo",
        blurb: "Most accurate, and still far faster than real time on this NPU.",
        hub_id: "whisper_large_v3_turbo",
        zip_bytes: 2_016_740_782,
        encoder_bytes: 1_753_546_752,
        decoder_bytes: 452_861_952,
    },
    ModelSpec {
        name: "whisper-small",
        label: "Small",
        blurb: "A third of the download. Good for clear speech in one language.",
        hub_id: "whisper_small",
        zip_bytes: 573_242_247,
        encoder_bytes: 268_091_392,
        decoder_bytes: 362_188_800,
    },
    ModelSpec {
        name: "whisper-base",
        label: "Base",
        blurb: "Smallest. Fine for quick memos, weak on names and accents.",
        hub_id: "whisper_base",
        zip_bytes: 180_937_668,
        encoder_bytes: 49_827_840,
        decoder_bytes: 152_367_104,
    },
];

pub fn spec(name: &str) -> Option<&'static ModelSpec> {
    CATALOG.iter().find(|spec| spec.name == name)
}

/// Complete packs under `root`, by directory name.
pub fn installed(root: &Path) -> Vec<WhisperPack> {
    let mut packs: Vec<WhisperPack> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|entry| WhisperPack::open(entry.path()).ok())
        .collect();
    packs.sort_by_key(|pack| pack.name());
    packs
}

/// The pack to transcribe with: the preferred one if installed, else the best that is.
pub fn choose(root: &Path, preferred: &str) -> Option<WhisperPack> {
    let packs = installed(root);
    let rank = |pack: &WhisperPack| {
        let name = pack.name();
        (name != preferred, CATALOG.iter().position(|spec| spec.name == name).unwrap_or(usize::MAX), name)
    };
    packs.into_iter().min_by_key(rank)
}

/// Bytes free on the filesystem holding `path`.
pub fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } == 0).then(|| stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Downloading,
    Unpacking,
}

/// An install in flight, shared between its worker and the screen showing it.
#[derive(Default)]
pub struct Progress {
    phase: AtomicU8,
    done: AtomicU64,
    total: AtomicU64,
    cancel: AtomicBool,
}

impl Progress {
    pub fn phase(&self) -> Phase {
        if self.phase.load(Ordering::Relaxed) == 0 { Phase::Downloading } else { Phase::Unpacking }
    }

    /// Bytes done and expected in the current phase.
    pub fn bytes(&self) -> (u64, u64) {
        (self.done.load(Ordering::Relaxed), self.total.load(Ordering::Relaxed))
    }

    pub fn fraction(&self) -> f32 {
        let (done, total) = self.bytes();
        if total == 0 { 0.0 } else { (done as f64 / total as f64).min(1.0) as f32 }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    fn cancelled(&self) -> io::Result<()> {
        if self.cancel.load(Ordering::Relaxed) { Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled")) } else { Ok(()) }
    }

    fn enter(&self, phase: Phase, done: u64, total: u64) {
        self.phase.store(phase as u8, Ordering::Relaxed);
        self.done.store(done, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }
}

fn zip_part(root: &Path, spec: &ModelSpec) -> PathBuf {
    root.join(format!(".{}.zip.part", spec.name))
}

/// Download and install `spec` under `root`. Blocking; run it on its own thread. An
/// interrupted download resumes where it stopped the next time this is called.
pub fn install(spec: &ModelSpec, root: &Path, progress: &Progress) -> Result<WhisperPack, String> {
    std::fs::create_dir_all(root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    let zip = zip_part(root, spec);
    // Left by a process that died while unpacking; it would be counted as space in use.
    let _ = std::fs::remove_dir_all(staging_dir(root, spec));
    let have = std::fs::metadata(&zip).map_or(0, |m| m.len());
    if let Some(free) = free_bytes(root) {
        let needed = spec.space_needed().saturating_sub(have);
        if free < needed {
            return Err(format!("Not enough space: {} needed, {} free.", human_bytes(needed), human_bytes(free)));
        }
    }
    download(&spec.url(), &zip, spec.zip_bytes, progress).map_err(|e| describe(&e, "Download"))?;
    let pack = unpack(&zip, root, spec, progress).map_err(|e| {
        // A complete download that does not unpack is damaged, and would be "resumed" as
        // complete for ever; the next attempt starts over. A full disk or a cancel is not that.
        if e.kind() == io::ErrorKind::InvalidData {
            let _ = std::fs::remove_file(&zip);
        }
        describe(&e, "Unpacking")
    })?;
    let _ = std::fs::remove_file(&zip);
    Ok(pack)
}

fn describe(error: &io::Error, what: &str) -> String {
    if error.kind() == io::ErrorKind::Interrupted { "Cancelled.".into() } else { format!("{what} failed: {error}") }
}

/// Fetch `url` into `dest`, continuing from however much of it is already there.
fn download(url: &str, dest: &Path, expected: u64, progress: &Progress) -> io::Result<()> {
    let mut have = std::fs::metadata(dest).map_or(0, |m| m.len());
    if have > expected {
        // Left over from a different release; it cannot be a prefix of this one.
        have = 0;
    }
    progress.enter(Phase::Downloading, have, expected);
    if have == expected {
        return Ok(());
    }

    let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(30)).timeout_read(Duration::from_secs(60)).build();
    let mut request = agent.get(url);
    if have > 0 {
        request = request.set("Range", &format!("bytes={have}-"));
    }
    let response = request.call().map_err(|e| io::Error::other(e.to_string()))?;
    // A server that ignores the range answers 200 with the whole file.
    let resumed = response.status() == 206;
    let mut file = OpenOptions::new().create(true).write(true).truncate(!resumed).open(dest)?;
    if resumed {
        file.seek(SeekFrom::End(0))?;
    } else {
        have = 0;
    }

    let mut reader = response.into_reader();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        progress.cancelled()?;
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        file.write_all(&buffer[..n])?;
        have += n as u64;
        progress.done.store(have, Ordering::Relaxed);
    }
    file.sync_all()?;
    if have != expected {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("got {have} of {expected} bytes; try again to resume")));
    }
    Ok(())
}

/// What is wrong with the zip itself, as opposed to with writing what it holds.
fn damaged(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("the download is damaged ({error})"))
}

/// Extract the two graphs from a release zip into `root/<name>` and seal the pack. The zip
/// nests them in a folder; only the file names matter. `InvalidData` means the zip is bad.
pub fn unpack(zip: &Path, root: &Path, spec: &ModelSpec, progress: &Progress) -> io::Result<WhisperPack> {
    let staging = staging_dir(root, spec);
    let result = unpack_into(zip, root, &staging, spec, progress);
    if result.is_err() {
        // Up to two gigabytes nobody would ever see or reclaim: gone whichever way it failed.
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

fn staging_dir(root: &Path, spec: &ModelSpec) -> PathBuf {
    root.join(format!(".{}.partial", spec.name))
}

fn unpack_into(zip: &Path, root: &Path, staging: &Path, spec: &ModelSpec, progress: &Progress) -> io::Result<WhisperPack> {
    progress.enter(Phase::Unpacking, 0, spec.installed_bytes());
    let _ = std::fs::remove_dir_all(staging);
    std::fs::create_dir_all(staging)?;

    let mut archive = zip::ZipArchive::new(File::open(zip)?).map_err(damaged)?;
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut written = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(damaged)?;
        let name = entry.name().rsplit('/').next().unwrap_or_default().to_string();
        let expected = match name.as_str() {
            ENCODER => spec.encoder_bytes,
            DECODER => spec.decoder_bytes,
            _ => continue,
        };
        if entry.size() != expected {
            return Err(damaged(format!("{name} is {} bytes, expected {expected}", entry.size())));
        }
        let mut out = File::create(staging.join(&name))?;
        loop {
            progress.cancelled()?;
            // The zip reader checks each entry's CRC as its last bytes are read.
            let n = entry.read(&mut buffer).map_err(damaged)?;
            if n == 0 {
                break;
            }
            out.write_all(&buffer[..n])?;
            written += n as u64;
            progress.done.store(written, Ordering::Relaxed);
        }
        out.sync_all()?;
    }

    let dest = root.join(spec.name);
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::rename(staging, &dest)?;
    WhisperPack::seal(&dest).map_err(damaged)
}

pub fn remove(pack: &WhisperPack) -> io::Result<()> {
    std::fs::remove_dir_all(pack.dir())
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{bytes} B"),
        3 => format!("{value:.2} GB"),
        _ => format!("{value:.0} {}", UNITS[unit]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("scribe-models-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tiny() -> ModelSpec {
        ModelSpec { name: "whisper-test", label: "Test", blurb: "", hub_id: "whisper_test", zip_bytes: 0, encoder_bytes: 7, decoder_bytes: 11 }
    }

    fn release_zip(path: &Path, encoder: &[u8], decoder: &[u8]) {
        let mut writer = zip::ZipWriter::new(File::create(path).unwrap());
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        // Qualcomm's zips nest everything one folder down.
        for (name, bytes) in [("pack-folder/encoder.bin", encoder), ("pack-folder/decoder.bin", decoder), ("pack-folder/metadata.json", &b"{}"[..])] {
            writer.start_file(name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn release_urls_point_at_the_pinned_galaxy_build() {
        assert_eq!(
            CATALOG[0].url(),
            "https://qaihub-public-assets.s3.us-west-2.amazonaws.com/qai-hub-models/models/whisper_large_v3_turbo/releases/v0.62.2/whisper_large_v3_turbo-qnn_context_binary-float-qualcomm_snapdragon_8_elite_gen5_for_galaxy.zip"
        );
        assert!(spec("whisper-small").is_some() && spec("nope").is_none());
        assert!(CATALOG[0].space_needed() > CATALOG[0].zip_bytes + CATALOG[0].installed_bytes());
    }

    #[test]
    fn unpacking_flattens_the_zip_and_seals_a_pack() {
        let root = scratch("unpack");
        let zip = root.join("release.zip");
        release_zip(&zip, b"encoder", b"decoder-bin");
        let progress = Progress::default();
        let pack = unpack(&zip, &root, &tiny(), &progress).unwrap();
        assert_eq!(pack.name(), "whisper-test");
        assert_eq!(std::fs::read(pack.path(ENCODER)).unwrap(), b"encoder");
        assert_eq!((progress.phase(), progress.bytes()), (Phase::Unpacking, (18, 18)));
        assert!(!root.join(".whisper-test.partial").exists());
        // metadata.json is not part of a pack.
        assert!(!pack.path("metadata.json").exists());
        assert_eq!(installed(&root).len(), 1);
        remove(&pack).unwrap();
        assert!(installed(&root).is_empty());
    }

    #[test]
    fn a_zip_with_the_wrong_sizes_installs_nothing() {
        let root = scratch("badsize");
        let zip = root.join("release.zip");
        release_zip(&zip, b"short", b"decoder-bin");
        let error = unpack(&zip, &root, &tiny(), &Progress::default()).unwrap_err();
        assert!(error.to_string().contains("encoder.bin is 5 bytes"), "{error}");
        assert!(installed(&root).is_empty());
        assert!(!root.join("whisper-test").exists());
    }

    #[test]
    fn a_zip_missing_a_graph_is_not_sealed() {
        let root = scratch("missing");
        let zip = root.join("release.zip");
        let mut writer = zip::ZipWriter::new(File::create(&zip).unwrap());
        writer.start_file("x/encoder.bin", zip::write::SimpleFileOptions::default()).unwrap();
        writer.write_all(b"encoder").unwrap();
        writer.finish().unwrap();
        assert!(unpack(&zip, &root, &tiny(), &Progress::default()).is_err());
        assert!(installed(&root).is_empty());
    }

    #[test]
    fn a_damaged_download_is_thrown_away_and_a_good_one_installs() {
        let root = scratch("damaged");
        // Already complete on disk, so `install` goes straight to unpacking it.
        let part = root.join(".whisper-test.zip.part");
        std::fs::write(&part, b"not a zip at all").unwrap();
        let spec = ModelSpec { zip_bytes: 16, ..tiny() };
        let error = install(&spec, &root, &Progress::default()).err().unwrap();
        assert!(error.starts_with("Unpacking failed: the download is damaged"), "{error}");
        assert!(!part.exists(), "a damaged zip must not be resumed from");

        release_zip(&part, b"encoder", b"decoder-bin");
        let spec = ModelSpec { zip_bytes: std::fs::metadata(&part).unwrap().len(), ..tiny() };
        let pack = install(&spec, &root, &Progress::default()).unwrap();
        assert_eq!(pack.name(), "whisper-test");
        assert!(!part.exists());
    }

    #[test]
    fn cancelling_stops_an_unpack_part_way() {
        let root = scratch("cancel");
        let zip = root.join("release.zip");
        release_zip(&zip, b"encoder", b"decoder-bin");
        let progress = Progress::default();
        progress.cancel();
        let error = unpack(&zip, &root, &tiny(), &progress).unwrap_err();
        assert_eq!(describe(&error, "Unpacking"), "Cancelled.");
        assert_ne!(error.kind(), io::ErrorKind::InvalidData, "a cancel says nothing about the zip");
        assert!(installed(&root).is_empty());
        assert!(!root.join(".whisper-test.partial").exists(), "nothing half-unpacked is left behind");
    }

    #[test]
    fn the_preferred_pack_wins_then_the_best_installed() {
        let root = scratch("choose");
        for name in ["whisper-base", "whisper-small", "custom-pack"] {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(ENCODER), b"e").unwrap();
            std::fs::write(dir.join(DECODER), b"d").unwrap();
            WhisperPack::seal(&dir).unwrap();
        }
        // An unsealed directory is an interrupted install, not a choice.
        std::fs::create_dir_all(root.join("whisper-large-v3-turbo")).unwrap();
        assert_eq!(choose(&root, "whisper-base").unwrap().name(), "whisper-base");
        assert_eq!(choose(&root, "whisper-large-v3-turbo").unwrap().name(), "whisper-small");
        assert_eq!(choose(&root, "custom-pack").unwrap().name(), "custom-pack");
        assert!(choose(&scratch("none"), "whisper-base").is_none());
    }

    #[test]
    fn sizes_read_at_the_scale_they_are() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(180_937_668), "173 MB");
        assert_eq!(human_bytes(2_016_740_782), "1.88 GB");
        assert!(free_bytes(&std::env::temp_dir()).unwrap() > 0);
    }
}
