# oxide

`oxide` is a small CLI that scans a directory tree of audio files, generates a **Chromaprint / AcoustID-style audio fingerprint**, and stores it in the file’s tags.

It is designed to be safe to run repeatedly:

- By default, it **skips files that already have an AcoustID fingerprint tag**.
- Use `--all` to force re-processing (recomputes and re-writes the fingerprint).

## Supported file types

`oxide` will consider files with these extensions:

- `flac`
- `m4a`
- `mp3`
- `ogg`
- `opus`
- `wav`
- `wv` (WavPack)
- `ape` (Monkey’s Audio)

Decoding is attempted with Symphonia first and falls back to `ffmpeg` for formats/codecs Symphonia can’t decode.

## Usage

Build:

```bash
cargo build --release
```

Run (only files missing the fingerprint tag):

```bash
./target/release/oxide /path/to/music
```

Run (process everything, even if already tagged):

```bash
./target/release/oxide --all /path/to/music
```

## What tag is written?

`oxide` writes the fingerprint into the container’s primary tagging format:

- FLAC/Ogg/Opus: Vorbis comments (`ACOUSTID_FINGERPRINT`)
- MP3/WAV (ID3v2): user text frame (`TXXX`) with description `Acoustid Fingerprint`
- MP4/M4A: MP4 freeform atom (`----:com.apple.iTunes:Acoustid Fingerprint`)
- APE/WV (APEv2): `ACOUSTID_FINGERPRINT`

## Verifying output

Examples:

FLAC (exact stored Vorbis comment key):

```bash
metaflac --export-tags-to=- some.flac | grep -i acoustid
```

Many formats (human-friendly view):

```bash
exiftool -a -G1 -s some.flac | grep -i acoustid
```

## Notes

- Fingerprinting uses up to ~120 seconds of audio.
- On Linux, `oxide` will process in parallel when the target directory is on a RAM-backed filesystem (`tmpfs`/`ramfs`). On physical disks it processes sequentially and, within each directory, in filename order.
