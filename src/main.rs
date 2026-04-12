use anyhow::{Context, Result};
use chromaprint::{Algorithm, Fingerprinter};
use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::{ItemKey, ItemValue, Tag, TagItem, TagType};
use rayon::prelude::*;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

const TMPFS_MAGIC: i64 = 0x01021994;
const RAMFS_MAGIC: i64 = 0x858458f6;

const SUPPORTED_EXTENSIONS: [&str; 8] = [
    // Match bliss-analyser (minus DSF) + add Monkey's Audio + keep existing WAV support.
    "m4a", "mp3", "ogg", "flac", "opus", "wv", "ape", "wav",
];

#[derive(Default, Clone, Copy, Debug)]
struct RunStats {
    audio_seen: usize,
    skipped_already_tagged: usize,
    attempted: usize,
    tagged_ok: usize,
    errors: usize,
}

fn is_supported_audio_path(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    SUPPORTED_EXTENSIONS
        .iter()
        .any(|allowed| ext.eq_ignore_ascii_case(allowed))
}

fn is_in_ram(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        use nix::sys::statfs::statfs;
        if let Ok(stats) = statfs(path) {
            let f_type = stats.filesystem_type().0 as i64;
            return f_type == TMPFS_MAGIC || f_type == RAMFS_MAGIC;
        }
    }
    false
}

fn main() -> Result<()> {
    let mut root_path: Option<PathBuf> = None;
    let mut process_all = false;

    let mut args = std::env::args();
    let _exe = args.next();
    for arg in args {
        if arg == "--all" || arg == "-a" {
            process_all = true;
            continue;
        }
        if arg.starts_with('-') {
            println!("Usage: oxide [--all|-a] <DIRECTORY>");
            anyhow::bail!("Unknown option: {arg}");
        }
        if root_path.is_none() {
            root_path = Some(PathBuf::from(arg));
        } else {
            println!("Usage: oxide [--all|-a] <DIRECTORY>");
            anyhow::bail!("Unexpected extra argument");
        }
    }

    let Some(root_path) = root_path else {
        println!("Usage: oxide [--all|-a] <DIRECTORY>");
        return Ok(());
    };
    if !root_path.is_dir() {
        anyhow::bail!("Target path is not a directory: {:?}", root_path);
    }

    if is_in_ram(&root_path) {
        println!("RAM disk: Parallel mode.");
        let stats = process_tree_parallel(&root_path, process_all);
        report_if_nothing_to_do(stats, process_all);
    } else {
        println!("Physical disk: Sequential mode.");
        let stats = process_tree_sequential_sorted(&root_path, process_all);
        report_if_nothing_to_do(stats, process_all);
    }
    Ok(())
}

fn report_if_nothing_to_do(stats: RunStats, process_all: bool) {
    if process_all {
        return;
    }
    if stats.audio_seen > 0 && stats.attempted == 0 && stats.skipped_already_tagged == stats.audio_seen {
        println!(
            "No files to process: all supported files already have an AcoustID fingerprint tag embedded."
        );
    }
}

fn needs_processing(path: &Path, process_all: bool) -> bool {
    if process_all {
        return true;
    }

    match lofty::read_from_path(path) {
        Ok(tagged_file) => !file_has_fingerprint(&tagged_file),
        // If we can't read tags for the skip-check, attempt processing and surface errors later.
        Err(_) => true,
    }
}

fn process_tree_parallel(root: &Path, process_all: bool) -> RunStats {
    // Stream the walk and process in parallel; ordering is not guaranteed.
    use std::sync::atomic::{AtomicUsize, Ordering};

    let audio_seen = AtomicUsize::new(0);
    let skipped_already_tagged = AtomicUsize::new(0);
    let attempted = AtomicUsize::new(0);
    let tagged_ok = AtomicUsize::new(0);
    let errors = AtomicUsize::new(0);

    walkdir::WalkDir::new(root)
        .into_iter()
        .par_bridge()
        .for_each(|entry| {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("Walk error: {e}");
                    return;
                }
            };

            if !entry.file_type().is_file() {
                return;
            }

            let p = entry.path();
            if !is_supported_audio_path(p) {
                return;
            }

            audio_seen.fetch_add(1, Ordering::Relaxed);

            if !needs_processing(p, process_all) {
                skipped_already_tagged.fetch_add(1, Ordering::Relaxed);
                return;
            }

            attempted.fetch_add(1, Ordering::Relaxed);

            if let Err(e) = process_file(p) {
                errors.fetch_add(1, Ordering::Relaxed);
                eprintln!("Error tagging {:?}: {}", p, e);
            } else {
                tagged_ok.fetch_add(1, Ordering::Relaxed);
            }
        });

    RunStats {
        audio_seen: audio_seen.load(Ordering::Relaxed),
        skipped_already_tagged: skipped_already_tagged.load(Ordering::Relaxed),
        attempted: attempted.load(Ordering::Relaxed),
        tagged_ok: tagged_ok.load(Ordering::Relaxed),
        errors: errors.load(Ordering::Relaxed),
    }
}

fn process_tree_sequential_sorted(root: &Path, process_all: bool) -> RunStats {
    match process_dir_sequential_sorted(root, process_all) {
        Ok(stats) => stats,
        Err(e) => {
            eprintln!("Error scanning {:?}: {e}", root);
            RunStats::default()
        }
    }
}

fn process_dir_sequential_sorted(dir: &Path, process_all: bool) -> Result<RunStats> {
    let mut stats = RunStats::default();

    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            // Permissions, transient IO, etc. Don't fail the whole run.
            eprintln!("Cannot read dir {:?}: {e}", dir);
            return Ok(stats);
        }
    };

    let mut subdirs: Vec<PathBuf> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();

    for ent in rd {
        let ent = match ent {
            Ok(e) => e,
            Err(e) => {
                eprintln!("Dir entry error in {:?}: {e}", dir);
                continue;
            }
        };

        let ft = match ent.file_type() {
            Ok(ft) => ft,
            Err(e) => {
                eprintln!("Cannot stat dir entry in {:?}: {e}", dir);
                continue;
            }
        };

        let p = ent.path();
        if ft.is_dir() {
            subdirs.push(p);
        } else if ft.is_file() {
            if is_supported_audio_path(&p) {
                files.push(p);
            }
        }
    }

    let empty = OsStr::new("");
    files.sort_by(|a, b| a.file_name().unwrap_or(empty).cmp(b.file_name().unwrap_or(empty)));
    subdirs.sort_by(|a, b| a.file_name().unwrap_or(empty).cmp(b.file_name().unwrap_or(empty)));

    // Process all files in this directory in filename order.
    for f in files {
        stats.audio_seen += 1;

        if !needs_processing(&f, process_all) {
            stats.skipped_already_tagged += 1;
            continue;
        }

        stats.attempted += 1;
        if let Err(e) = process_file(&f) {
            stats.errors += 1;
            eprintln!("Error tagging {:?}: {}", f, e);
        } else {
            stats.tagged_ok += 1;
        }
    }

    // Recurse into subdirectories.
    for d in subdirs {
        if let Ok(child) = process_dir_sequential_sorted(&d, process_all) {
            stats.audio_seen += child.audio_seen;
            stats.skipped_already_tagged += child.skipped_already_tagged;
            stats.attempted += child.attempted;
            stats.tagged_ok += child.tagged_ok;
            stats.errors += child.errors;
        }
    }

    Ok(stats)
}

fn acoustid_fingerprint_keys(tag_type: TagType) -> Vec<ItemKey> {
    match tag_type {
        // Vorbis comments (FLAC/Ogg): keys are stored as provided; different tools use different conventions.
        TagType::VorbisComments => vec![
            ItemKey::Unknown("ACOUSTID_FINGERPRINT".to_string()),
            ItemKey::Unknown("Acoustid Fingerprint".to_string()),
            ItemKey::Unknown("acoustid_fingerprint".to_string()),
            ItemKey::Unknown("acoustid fingerprint".to_string()),
        ],
        // ID3v2: stored as TXXX description.
        TagType::Id3v2 => vec![
            ItemKey::Unknown("Acoustid Fingerprint".to_string()),
            ItemKey::Unknown("ACOUSTID_FINGERPRINT".to_string()),
        ],
        // MP4/M4A: freeform atoms. Keep a stable primary, but remove common variants.
        TagType::Mp4Ilst => vec![
            ItemKey::Unknown("----:com.apple.iTunes:Acoustid Fingerprint".to_string()),
            ItemKey::Unknown("----:com.apple.iTunes:ACOUSTID_FINGERPRINT".to_string()),
        ],
        // APEv2 (Monkey's Audio, WavPack, etc.).
        TagType::Ape => vec![
            ItemKey::Unknown("ACOUSTID_FINGERPRINT".to_string()),
            ItemKey::Unknown("Acoustid Fingerprint".to_string()),
            ItemKey::Unknown("acoustid_fingerprint".to_string()),
        ],
        _ => vec![ItemKey::Unknown("Acoustid Fingerprint".to_string())],
    }
}

fn fingerprint_via_symphonia(path: &Path, ext: &str) -> Result<String> {
    let src = std::fs::File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(src), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(ext);

    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())?;

    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .context("No audio")?;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())?;

    let sample_rate = track.codec_params.sample_rate.context("No rate")?;
    let channels = track.codec_params.channels.context("No channels")?.count();

    let mut fp = Fingerprinter::new(Algorithm::default());
    fp.start(sample_rate, channels as u16)?;

    let max_samples = 120 * sample_rate * (channels as u32);
    let mut total_samples = 0;

    while let Ok(packet) = format.next_packet() {
        if total_samples >= max_samples {
            break;
        }
        let decoded = decoder.decode(&packet)?;
        let mut sample_buf = SampleBuffer::<i16>::new(decoded.capacity() as u64, *decoded.spec());
        sample_buf.copy_interleaved_ref(decoded);
        let _ = fp.feed(sample_buf.samples());
        total_samples += sample_buf.samples().len() as u32;
    }

    fp.finish()?;
    Ok(fp.encode())
}

fn fingerprint_via_ffmpeg(path: &Path) -> Result<String> {
    // Use a stable PCM format that Chromaprint expects.
    const SAMPLE_RATE: u32 = 44100;
    const CHANNELS: u16 = 2;
    let max_samples = 120u32 * SAMPLE_RATE * (CHANNELS as u32);

    let mut child = Command::new("ffmpeg")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-nostdin")
        .arg("-i")
        .arg(path)
        .arg("-vn")
        .arg("-sn")
        .arg("-dn")
        .arg("-f")
        .arg("s16le")
        .arg("-ac")
        .arg(CHANNELS.to_string())
        .arg("-ar")
        .arg(SAMPLE_RATE.to_string())
        .arg("-")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn ffmpeg (is it installed?)")?;

    let mut fp = Fingerprinter::new(Algorithm::default());
    fp.start(SAMPLE_RATE, CHANNELS)?;

    let mut stdout = child.stdout.take().context("ffmpeg stdout missing")?;
    let mut carry_byte: Option<u8> = None;
    let mut buf = [0u8; 64 * 1024];
    let mut total_samples: u32 = 0;

    loop {
        if total_samples >= max_samples {
            // Stop ffmpeg early once we have enough audio.
            let _ = child.kill();
            break;
        }

        let n = stdout.read(&mut buf)?;
        if n == 0 {
            break;
        }

        let mut samples: Vec<i16> = Vec::with_capacity((n + 1) / 2);
        let mut idx = 0usize;

        if let Some(lo) = carry_byte.take() {
            if n > 0 {
                samples.push(i16::from_le_bytes([lo, buf[0]]));
                idx = 1;
            } else {
                carry_byte = Some(lo);
                continue;
            }
        }

        while idx + 1 < n {
            samples.push(i16::from_le_bytes([buf[idx], buf[idx + 1]]));
            idx += 2;
        }

        if idx < n {
            carry_byte = Some(buf[idx]);
        }

        if !samples.is_empty() {
            let remaining = (max_samples - total_samples) as usize;
            let to_feed = remaining.min(samples.len());
            let _ = fp.feed(&samples[..to_feed]);
            total_samples += to_feed as u32;
        }
    }

    let status = child.wait()?;
    if !status.success() && total_samples == 0 {
        let mut stderr = String::new();
        if let Some(mut s) = child.stderr.take() {
            let _ = s.read_to_string(&mut stderr);
        }
        anyhow::bail!("ffmpeg decode failed for {:?}: {}", path, stderr.trim());
    }

    fp.finish()?;
    Ok(fp.encode())
}

fn fingerprint_audio(path: &Path, ext: &str) -> Result<String> {
    match fingerprint_via_symphonia(path, ext) {
        Ok(fp) => Ok(fp),
        Err(sym_err) => {
            // Symphonia doesn't decode some formats (e.g. ape/wv/dsf). Try ffmpeg if available.
            fingerprint_via_ffmpeg(path).with_context(|| {
                format!("SFS decode failed ({sym_err}); ffmpeg fallback also failed")
            })
        }
    }
}

fn file_has_fingerprint(tagged_file: &lofty::file::TaggedFile) -> bool {
    for tag in tagged_file.tags() {
        let keys = acoustid_fingerprint_keys(tag.tag_type());
        for key in keys {
            if let Some(val) = tag.get_string(&key) {
                if !val.trim().is_empty() {
                    return true;
                }
            }
        }
    }
    false
}

fn process_file(path: &Path) -> Result<()> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default();
    if !SUPPORTED_EXTENSIONS
        .iter()
        .any(|allowed| ext.eq_ignore_ascii_case(allowed))
    {
        return Ok(());
    }

    let ext = ext.to_ascii_lowercase();

    // 1. Generate fingerprint (SFS -> ffmpeg fallback)
    let fingerprint = fingerprint_audio(path, &ext)?;

    // 2. Commit tag
    // Ensure the file has its primary tag type, otherwise writing will be a no-op for many formats.
    let mut tagged_file = lofty::read_from_path(path)
        .with_context(|| format!("Lofty cannot read this file type for tagging: {:?}", path))?;
    let primary_tag_type = tagged_file.primary_tag_type();
    if tagged_file.tag_mut(primary_tag_type).is_none() {
        tagged_file.insert_tag(Tag::new(primary_tag_type));
    }

    let keys = acoustid_fingerprint_keys(primary_tag_type);
    let key = keys
        .first()
        .cloned()
        .context("No key candidates for AcoustID fingerprint")?;
    let tag = tagged_file
        .tag_mut(primary_tag_type)
        .context("Failed to get/create primary tag")?;

    for k in &keys {
        tag.remove_key(k);
    }
    tag.insert_unchecked(TagItem::new(key.clone(), ItemValue::Text(fingerprint)));

    tagged_file.save_to_path(path, WriteOptions::default())?;

    // Verify the tag actually persisted (helps surface silent drops due to format constraints).
    let check = lofty::read_from_path(path)?;
    let check_tag = check
        .tag(primary_tag_type)
        .or_else(|| check.primary_tag())
        .or_else(|| check.first_tag())
        .context("Failed to re-open tags for verification")?;
    if check_tag.get_string(&key).is_none() {
        anyhow::bail!(
            "Fingerprint tag did not persist (tag_type={:?}, key={:?})",
            primary_tag_type,
            key
        );
    }

    println!("Tagged: {:?}", path.file_name().unwrap());
    Ok(())
}
