# Formats and conversion behavior

The extension lists and targets below come from `Category::targets` and `detect` in [`src/converter.rs`](../src/converter.rs). Extensions are compared case-insensitively. Recognition does not guarantee that a malformed file or a codec unsupported by the local backend can be converted.

## Images

**Input extensions:** `png`, `apng`, `jpg`, `jpeg`, `jfif`, `webp`, `bmp`, `gif`, `tif`, `tiff`, `ico`, `tga`, `qoi`, `pnm`, `pbm`, `pgm`, `ppm`, `pam`, `hdr`, `exr`, `ff`, `dds`.

**Output targets:** `png`, `jpg`, `webp`, `avif`, `bmp`, `gif`, `tiff`, `ico`, `tga`, `qoi`, `pnm`, `hdr`, `exr`, `ff`.

The `image` crate opens the input, identifies its format from the file contents (falling back to the extension), and decodes a single image. The contents decide the decoder, so a PNG named `.jpg` still converts, but only the input formats listed above are accepted: other formats the `image` crate can identify, such as AVIF, are rejected with `unsupported image format`, and their decoders are not built into ConvertCraft. AVIF is output-only because the `image` crate's AVIF decoder needs the native dav1d library. DDS inputs decode their main surface only (no mipmaps, cube faces, or array layers).

HDR (Radiance RGBE) and OpenEXR hold floating-point, linear-light pixels. Converting them to an 8- or 16-bit format clips values to the 0–1 range without tone mapping or a gamma curve, so bright highlights clip and the result may look darker than in an HDR viewer. Converting 8- or 16-bit images to HDR or EXR stores the sample values as-is, without linearizing them.

Images larger than **100 megapixels** (for example 10,000×10,000) are rejected before their pixels are decoded. Normalizing to the target's layout needs up to 4 bytes per pixel on top of the decoded image (8 for Farbfeld, 16 for HDR and EXR), so all running image jobs share an estimated memory budget of 1.5 GiB; a job waits for memory instead of running alongside the others when the budget is full.

An EXIF orientation tag (set by cameras and phones for rotated shots) is applied to the pixels, so the output displays upright even though the metadata itself is not copied.

**Shrink images to fit** (off by default) scales images down with Lanczos3 filtering so they fit within the chosen maximum width and height, keeping the aspect ratio. The limits apply to the upright image after EXIF rotation. Images already within the limits are never enlarged.

- **JPG:** converts to 8-bit RGB and encodes at the selected quality (90 by default). JPEG has no transparency, so transparent and semi-transparent pixels are composited over white.
- **PNG:** saves the decoded image as-is, keeping its channels and 8- or 16-bit depth. Float inputs (HDR, EXR) are stored as 16-bit RGB or RGBA.
- **AVIF:** 8-bit, encoded with the pure-Rust rav1e encoder (speed preset 6) at the selected quality (90 by default); alpha is kept, 16-bit and float inputs are reduced to 8 bits. Encoding is much slower than the other formats, especially for large images.
- **TIFF, PNM:** keep 16-bit depth for 16-bit inputs (grayscale, RGB, or RGBA); 16-bit gray+alpha is stored as RGBA. TIFF also stores 8-bit gray+alpha as RGBA.
- **PNM:** written as PAM (`P7`), which holds every channel layout including alpha. Tools that only read PBM/PGM/PPM may not open it.
- **ICO:** fits the image within 256×256 (or the smaller **Shrink images to fit** limits) while preserving aspect ratio, using Lanczos3 filtering, and adds 48, 32, and 16 pixel versions when they are smaller, so Windows has a sharp icon for each view.
- **BMP, TGA, WebP, PNM:** keep the input's channel layout at 8 bits (grayscale, gray+alpha, RGB, or RGBA); no alpha channel is added to opaque images.
- **QOI:** RGB for opaque inputs, RGBA otherwise.
- **GIF:** converts to 8-bit RGBA and quantizes to a 256-color palette.
- **HDR:** 32-bit float RGB; alpha is dropped.
- **EXR:** 32-bit float RGB for opaque inputs, RGBA otherwise.
- **Farbfeld (`ff`):** always 16-bit RGBA.

There are no controls for metadata, color profiles, WebP quality, or animation preservation. Do not assume conversion preserves metadata or all animation frames; APNG inputs convert their first frame. Only PNG, TIFF, PNM, and Farbfeld keep 16-bit depth, and only HDR and EXR keep float samples.

## Structured data

**Input extensions:** `json`, `yaml`, `yml`, `toml`, `csv`.

**Output targets:** `json`, `yaml`, `toml`, `csv`.

Inputs are read as UTF-8 text and parsed into a `serde_json::Value`. The target serializer then writes that value. Original formatting, comments, and format-specific features are not preserved; conversions are not guaranteed to be lossless.

Duplicate keys in JSON or YAML objects, and duplicate CSV header names, are rejected with a `duplicate key` / `duplicate CSV header` error instead of silently keeping one value.

**Size limits.** Data files larger than 64 MiB are rejected. While parsing, the estimated in-memory size of the document is capped at 768 MiB, which stops YAML aliases or very wide CSV headers from expanding a small file into gigabytes (`document expands beyond the 768 MiB memory limit`). Output is written directly to the staged file and capped at 1 GiB. CSV input and output are limited to 16,384 columns (the Excel column limit).

### JSON and YAML

JSON output is pretty-printed and ends with a newline. YAML is parsed and written with `serde-saphyr`, a pure-Rust YAML library. Nesting deeper than 128 levels is rejected, as for JSON. Because the intermediate value is JSON-shaped, source features outside that data model may fail or change representation.

### TOML

An object root is serialized directly. Other roots are wrapped in an object under `items` to satisfy TOML's table-root requirement. For example, this JSON:

```json
[{"name": "sample", "enabled": true}]
```

becomes:

```toml
[[items]]
enabled = true
name = "sample"
```

Converting back to JSON retains the `items` wrapper; it is not automatically removed. Nulls and other TOML-incompatible values cause serialization to fail.

### CSV input

The first record supplies headers. Each remaining record becomes an object in an array. The default CSV reader expects consistent record lengths and a comma delimiter; there is no delimiter or header configuration in the UI.

Each field is inferred in this order:

1. Empty string → null.
2. Integer with a leading zero (`00123`, `-007`) → string, so identifiers keep their formatting.
3. Valid signed or unsigned 64-bit integer → number.
4. Other integer text (too large for 64 bits) → string, so it is not rounded.
5. Valid finite floating-point number (`0.5`, `1e3`) → number.
6. Exact lowercase `true` or `false` → boolean.
7. Anything else → string.

For example, `00123` stays the string `"00123"` and `18446744073709551615` stays an exact integer. Quoting a CSV field does not disable inference.

### CSV output

The root must be one object or an array containing only objects. An object whose only key holds an array of objects is treated as that array, so TOML output's `items` wrapper (or a JSON `{"data": [...]}` envelope) becomes one row per item instead of a single cell. Headers are the union of row keys encountered during iteration. With the current `serde_json` configuration, object keys use sorted map order, not guaranteed source insertion order.

- Missing keys and nulls become empty fields.
- Strings are written directly, with CSV quoting as needed.
- Numbers and booleans use their JSON text representation.
- Nested objects and arrays become JSON text inside a field, not flattened columns.

Nested JSON text is not automatically parsed back into nested values when reading CSV. Empty strings, nulls, and missing values can become indistinguishable.

**Safe CSV for spreadsheets** (on by default) prefixes headers and string cells that begin with `=`, `+`, `-`, `@`, tab, or carriage return with an apostrophe, so spreadsheet programs show them as text instead of evaluating them as formulas. Numbers are not changed. Turn the option off to write such values unmodified, for example when the CSV is consumed by another program rather than a spreadsheet.

## Audio

**Input extensions:** `mp3`, `mp2`, `wav`, `w64`, `flac`, `ogg`, `oga`, `m4a`, `m4b`, `aac`, `opus`, `wma`, `aiff`, `aif`, `aifc`, `ac3`, `eac3`, `dts`, `mka`, `caf`, `wv`, `tta`, `au`.

**Output targets:** `mp3`, `wav`, `flac`, `ogg`, `m4a`, `opus`, `aac`, `aiff`, `wma`, `ac3`.

All audio target presets disable video with `-vn`:

- **MP3:** `libmp3lame`, quality `2`.
- **WAV:** FFmpeg's output-format defaults; no explicit audio codec is selected.
- **FLAC:** `flac` encoder.
- **OGG:** `libvorbis`, quality `5`.
- **M4A:** AAC at `192k`.
- **Opus:** `libopus` at `128k`.
- **AAC:** raw ADTS AAC at `192k`.
- **AIFF:** 16-bit big-endian PCM.
- **WMA:** `wmav2` at `192k`.
- **AC3:** Dolby Digital at `384k`.

## Video

**Input extensions:** `mp4`, `mkv`, `webm`, `avi`, `divx`, `mov`, `wmv`, `asf`, `flv`, `f4v`, `m4v`, `mpg`, `mpeg`, `m2v`, `vob`, `ts`, `mts`, `m2ts`, `3gp`, `3g2`, `ogv`, `mxf`, `rm`, `rmvb`, `dv`, `y4m`.

**Output targets:** `mp4`, `mkv`, `webm`, `avi`, `mov`, `m4v`, `ts`, `flv`, `wmv`, `ogv`, `gif`, `webp`, plus every audio target above for audio extraction.

- **MP4/MOV/M4V:** `libx264`, preset `medium`, CRF `23`, pixel format `yuv420p`; AAC at `160k`; `+faststart`.
- **MKV:** `libx264`, CRF `23`; AAC at `160k`.
- **TS/FLV:** `libx264`, preset `medium`, CRF `23`, pixel format `yuv420p`; AAC at `160k`.
- **WebM:** `libvpx-vp9`, CRF `32`, video bitrate `0`; `libopus` audio.
- **AVI:** `mpeg4`, video quality `5`; `libmp3lame`, audio quality `4`.
- **WMV:** `wmv2`, video quality `4`; `wmav2` at `192k`.
- **OGV:** `libtheora`, video quality `7`; `libvorbis`, quality `5`. Needs an FFmpeg built with Theora and Vorbis; Homebrew's `ffmpeg` formula has no Vorbis encoder, so use `ffmpeg-full` on macOS.
- **GIF:** no audio; 12 fps; width capped at 480 pixels with Lanczos scaling and an automatically computed even height; palette generation/use; infinite looping.
- **WebP:** animated WebP via `libwebp_anim`; no audio; 12 fps; width capped at 480 pixels like GIF; lossy quality `75`; infinite looping. Needs an FFmpeg built with libwebp.
- **Audio targets:** use the audio presets above to extract the audio track.

These presets re-encode rather than requesting stream copying. There are no UI controls for bitrate, resolution, stream selection, trimming, subtitles, or codec choice. FFmpeg's default stream-selection behavior applies because no explicit `-map` is supplied.

### FFmpeg invocation and errors

The backend executes the equivalent of:

```text
ffmpeg -y -nostdin -hide_banner -loglevel error -progress pipe:1 \
       -protocol_whitelist file -format_whitelist DEMUXERS \
       -i INPUT [target preset arguments] STAGED_OUTPUT
```

Paths and options are passed as separate process arguments, not interpolated into a shell command. On Windows the subprocess uses `CREATE_NO_WINDOW`.

FFmpeg is the first `ffmpeg` (`ffmpeg.exe` on Windows) found in an **absolute** `PATH` entry that runs `-version` successfully. On macOS, Homebrew's `/opt/homebrew/bin` (Apple Silicon) and `/usr/local/bin` (Intel) are searched after `PATH`, since an app opened from Finder or the Dock gets only `/usr/bin:/bin:/usr/sbin:/sbin`. Relative `PATH` entries are skipped, and, unlike a plain process launch on Windows, the application's own folder is not searched, so an `ffmpeg.exe` placed next to ConvertCraft is never run implicitly.

`DEMUXERS` is `aac,ac3,aiff,asf,au,avi,caf,dts,dv,eac3,flac,flv,matroska,mov,mp3,mpeg,mpegts,mpegvideo,mxf,ogg,rm,tta,w64,wav,wv,yuv4mpegpipe`, which covers every recognized audio/video input extension. Playlist and concatenation formats (HLS, ffconcat) are not on the list, so a "video" that is really a playlist cannot pull other local files into the output. Only the `file` protocol is allowed for input.

Jobs can be cancelled from the UI. A job is also stopped if FFmpeg reports no progress for 5 minutes. Only the last 64 KiB of FFmpeg's error output is kept.

The `-y` flag permits FFmpeg to write only its owned temporary output, whose suffix matches the target format. On nonzero exit, the backend reports the last nonempty stderr line and staging is cleaned up. The final destination is published after success through the same no-clobber/replacement policy as image and data conversions; failed input parsing does not delete an old destination.
