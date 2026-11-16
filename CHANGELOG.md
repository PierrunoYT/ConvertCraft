# Changelog

This changelog records the current implementation. `Cargo.toml` declares version `0.1.0`, but the repository has no release tags, so every change is listed under **Unreleased**.

## Unreleased

### Changed

- README: CI, platform, and license badges, a screenshot of a finished batch, and separate Windows, macOS, and Linux setup sections.
- README: note that ConvertCraft is one of Pierruno's open-source desktop apps, linking to https://pierruno.dev/.
- README: Linux file pickers need an XDG desktop portal or `zenity`; without either, **Add files…** and **Add folder…** do nothing.
- README: OGG output needs Homebrew's `ffmpeg-full` on macOS, because the `ffmpeg` formula has no Vorbis encoder.
- Repository moved to https://github.com/PierrunoYT/ConvertCraft; Cargo metadata and README links point there.
- Renamed the Cargo package and executable from `transmute` to `convertcraft`.
- Replaced the logo with an orange-and-blue conversion-arrow mark, shared by the window icon, app header, empty queue, and README.
- Separated branding from queue controls and rewrote the README with setup, workflow, supported formats, and conversion limitations.
- Redesigned the interface with a light theme: white header with an ffmpeg status badge, an orange **Add files…** button and color-coded category chips for **Convert all**, a card-style queue with category icons and status badges, an **Output options** side panel, and a bottom bar with progress, **Clear finished**, **Clear all**, **Cancel**, and **Convert**.

### Added

- Video files can extract audio to FLAC, OGG, and Opus, not only MP3, WAV, and M4A.
- YUV4MPEG (`.y4m`) video input.
- RealMedia (`.rm`, `.rmvb`) video input.
- MXF video input.
- DV video input.
- OGV (Ogg video) input.
- 3GP/3G2 video input.
- MTS/M2TS (AVCHD, Blu-ray transport stream) video input.
- VOB (DVD video) input.
- M2V (raw MPEG-2 video) input.
- F4V video input.
- ASF video input.
- DivX (`.divx`, AVI container) video input.
- Sun AU (`.au`) audio input.
- TTA (True Audio) input.
- WavPack (`.wv`) audio input.
- CAF (Core Audio Format) input.
- MKA (Matroska audio) input.
- DTS audio input.
- E-AC3 (Dolby Digital Plus) audio input.
- AC3 (Dolby Digital) audio input.
- AIFC audio input.
- M4B audiobook input.
- OGA (Ogg audio) input.
- Sony Wave64 (`.w64`) audio input.
- MP2 audio input.
- AVIF image output via the pure-Rust rav1e encoder. The **JPEG quality** slider is now **JPEG/AVIF quality** and applies to both.
- DDS image input (main surface only).
- Farbfeld image input and output (16-bit RGBA).
- OpenEXR image input and output (32-bit float RGB or RGBA).
- Radiance HDR image input and output. Float images become 16-bit when saved as PNG and are clipped without tone mapping in 8- and 16-bit formats.
- `.jfif`, `.pam`, and `.apng` image inputs; APNG converts its first frame.
- PNM image output, written as PAM (`P7`) and keeping 16-bit depth.
- `scripts/bundle-macos.sh` builds a double-clickable `ConvertCraft.app` with the logo as its icon. On macOS, FFmpeg is also looked up in Homebrew's `/opt/homebrew/bin` and `/usr/local/bin`, so an app opened from Finder or the Dock finds a Homebrew FFmpeg despite launchd's minimal `PATH`.
- **Cancel** button for running batches; FFmpeg jobs stop immediately (#22, #23).
- CI workflow: rustfmt, Clippy with `-D warnings`, tests on Linux/Windows/macOS with FFmpeg, and `cargo deny check` with a committed `deny.toml` policy (#20).
- `#![forbid(unsafe_code)]` at the crate root.
- `CONVERTCRAFT_REQUIRE_FFMPEG` / `CONVERTCRAFT_REQUIRE_SYMLINKS` test switches that turn skipped media/symlink tests into failures.
- ConvertCraft desktop interface built with egui/eframe.
- GPL-3.0 license.
- Supplied logo embedded as the window icon and displayed in the README.
- Logo embedded as the Windows executable icon (Explorer, taskbar, shortcuts) via `build.rs`.
- EXIF orientation is applied when converting images, so rotated phone photos come out upright.
- JPG output composites transparent pixels over white instead of exposing the hidden (often black) color underneath.
- Image outputs keep the input's channel layout (no alpha added to opaque images), and TIFF keeps 16-bit depth.
- **Shrink images to fit** option: scales images down to a maximum width and height, keeping the aspect ratio.
- ICO output contains 48, 32, and 16 pixel versions alongside the largest size.
- File selection, recursive folder import, drag-and-drop, and duplicate-path filtering.
- Image conversion to PNG, JPG, WebP, BMP, GIF, TIFF, ICO, TGA, and QOI, including JPEG quality control and ICO resizing.
- JSON, YAML, TOML, and CSV conversion, including CSV scalar inference and TOML wrapping for non-object roots.
- FFmpeg-backed audio/video conversion, video-to-GIF conversion, and audio extraction from video.
- Per-file and category-wide output format selection.
- Output beside sources or in a selected folder, optional overwriting, and batch output-path reservation with numbered collision suffixes.
- Background conversion with up to four workers, per-file errors, and completed-job progress.
- Completed-file reveal through Explorer, Finder, or `xdg-open`.
- Tests for image targets, structured data round trips, output-path collisions, and FFmpeg media conversions.
- README and user, format, and development documentation.

### Fixed

- Output planning runs on a background thread, so a slow network share or an output folder crowded with numbered names no longer freezes the window (#24).
- Windows file identity uses the 128-bit file ID (`FILE_ID_INFO`) instead of the 64-bit index, which is not unique on ReFS; volumes that cannot report it get an unknown identity and existing files there are never replaced (#26).
- Upgraded eframe to 0.37, which drops the `quick-xml 0.30` advisories (RUSTSEC-2026-0194/-0195) and the unmaintained `paste` and `ttf-parser`, and replaced `serde_yaml`/`unsafe-libyaml` with the maintained pure-Rust `serde-saphyr` (#20).
- A panic during a conversion fails only that row, and rows left unfinished by exiting workers are marked failed instead of staying "Converting…" forever. Closing the window stops FFmpeg and waits briefly for workers to clean up their staging files (#22).
- FFmpeg runs with a demuxer whitelist (no HLS/ffconcat playlists), `-protocol_whitelist file`, and `-nostdin`; a job with no progress for 5 minutes is stopped; stderr capture is capped at 64 KiB (#23).
- FFmpeg is located only in absolute `PATH` entries, and Explorer is launched by absolute path, so executables placed next to ConvertCraft are not run (#19).
- Output paths are shown and passed to Explorer without the `\\?\` prefix, and the Explorer argument is quoted so names containing commas select the right file (#25).
- The FFmpeg availability check runs on a background thread instead of blocking startup (#24).
- Batches no longer keep two open handles per queued job: source and destination identities are stored as keys, so large batches no longer run into the Unix open-file limit (#17).
- A missing, unreadable, or non-regular source fails only its own row instead of the whole batch; completed rows whose sources were deleted no longer block new conversions (#16).
- On Linux and macOS, converted files get ordinary permissions (0666 minus the umask, usually 0644) instead of owner-only 0600, so other users and services can read them.
- Updated the yanked `async-executor` 1.14.0 and `clipboard-win` 5.4.1 to 1.14.1 and 5.4.3, so `cargo deny check` passes again.
- On Linux, a source or existing destination deleted and recreated right after planning is now detected. ext4 reuses the inode number and, on kernels without fine-grained timestamps (such as Linux 6.1), often the change time, so the file key now includes the inode generation. The two identity tests failed about half the time on such kernels.
- File systems that report no file ID are detected, and overwrite then uses a numbered name instead of replacing a file it cannot re-identify (#26).
- Sources are opened once and re-identified before conversion, so a file or FIFO swapped in after planning is rejected instead of converted or blocking a worker (#27).
- Images over 100 megapixels are rejected before decoding, and concurrent image jobs share a 1.5 GiB memory budget; a 260 KB PNG could previously drive one conversion to 1.35 GB of memory and a 1 GiB output (#14).
- Only the advertised image input formats are decoded. The `image` crate is built without its EXR, HDR, DDS, farbfeld, and AVIF support, and files sniffed as other formats are rejected (#21).
- Data conversion is bounded: 64 MiB input limit, a 768 MiB estimated memory budget enforced while parsing (stops YAML alias and wide-CSV-header expansion), output streamed to disk with a 1 GiB limit, and at most 16,384 CSV columns. The CSV header union is no longer quadratic (#13).
- CSV output neutralizes spreadsheet formulas by default (**Safe CSV for spreadsheets** option) (#15).
- Duplicate JSON/YAML keys and CSV headers are rejected instead of silently keeping the last value; CSV integers with leading zeros or beyond 64 bits stay exact text instead of becoming rounded numbers (#28).
- Folder import no longer follows links inside folders, runs on a background thread with a Cancel button, and is bounded by depth, file, and folder limits; a folder with links back to itself used to hang the app (#18).
- Import errors (unreadable folders or entries, special files) are now shown in the top bar instead of being silently dropped (#29).
- Enforce no-clobber when publishing new outputs, including files created after planning and case-only name collisions.
- Preserve existing destinations when image/data encoding or FFmpeg fails; publish successful conversions from owned staging files.
- Protect all queued source paths and file identities from overwrite-enabled batch outputs.
- Report JPEG buffered-write failures before marking a job successful.
- Isolate conversion test fixtures in unique temporary directories and surface output-directory creation errors.

### Known limitations

- No persistent settings, saved queue, or conversion command-line interface.
- Media conversions require an external FFmpeg installation with the required codecs.
- Image conversion uses a single decoded image rather than preserving animation.
- Structured data conversion does not preserve comments, formatting, or every source-format type.
- Output publication is pathname-based; hostile ancestor-directory replacement in the final check-to-rename interval is not fully isolated.
- **Cancel** stops FFmpeg jobs immediately, but image and data jobs already running finish first.
