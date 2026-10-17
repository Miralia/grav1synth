# Grav1synth

[![docs.rs](https://img.shields.io/docsrs/grav1synth?style=for-the-badge)](https://docs.rs/grav1synth)
[![Crates.io](https://img.shields.io/crates/v/grav1synth?style=for-the-badge)](https://crates.io/crates/grav1synth)
[![LICENSE](https://img.shields.io/crates/l/grav1synth?style=for-the-badge)](https://github.com/rust-av/grav1synth/blob/main/LICENSE)

Grain Synth analyzer and editor for AV1 files

## Build

- Prerequisites:
  - ffmpeg headers
  - Rust compiler
- Pull the repo
- Run `cargo build --release`
- Copy the binary from `target/release/grav1synth` to wherever you want

## Usage

### `grav1synth inspect my_encode.mkv -o grain_file.txt`

Reads `my_encode.mkv` and outputs a film grain table file at `grain_file.txt`

### `grav1synth apply my_encode.mkv -o grainy_encode.mkv -g grain_file.txt`

Reads `my_encode.mkv`, applies film grain from `grain_file.txt`, and outputs the video to `grainy_encode.mkv`.

### `grav1synth apply my_encode.mkv -o grainy_encode.mkv --iso 400 --chroma`

Reads `my_encode.mkv`, generates photon-noise-based film grain at the strength given by `--iso` (up to `4294967295`), and outputs the video to `grainy_encode.mkv`. By default grain is applied to the luma plane only; `--chroma` enables grain on chroma planes as well.

In both forms, if the input already has film grain headers the command will print a notice and skip processing — add `--replace` to overwrite existing grain instead. This makes it safe to use in automated workflows where you want to protect videos that already have grain while still applying grain to those that do not.

### `grav1synth remove my_encode.mkv -o clean_encode.mkv`

Reads `my_encode.mkv`, removes all synthesized film grain, and outputs the video at `clean_encode.mkv`

### `grav1synth diff my_source.mkv denoised_source.mkv -o grain_file.txt`

Compares `my_source.mkv` and `denoised_source.mkv` and generates a film grain table at `grain_file.txt` based on the difference between them. This will provide the most accurate estimation of source film grain.

### `grav1synth generate scenes.json -o grain_file.txt --reference my_encode.mkv`

Builds a multi-scene film grain table from a JSON scene spec, using av1-grain's photon-noise model (luma only). Each scene carries its own ISO:

```json
{
  "scenes": [
    { "start_frame": 0, "end_frame": 240, "iso": 400 },
    { "start_frame": 240, "end_frame": 480, "iso": 1600 }
  ]
}
```

Pass `--reference my_encode.mkv` — the zero-grain encode you intend to inject into — so that scene
boundaries are taken from that file's actual packet timestamps and `width`, `height`, transfer
function, and color range are filled in from it automatically. Without `--reference`, the spec must
also provide `width`, `height`, and `fps` (for example `{ "num": 24000, "den": 1001 }`), and
boundaries are computed from the frame rate instead, which can be off by a frame for non-integer
frame rates.

Scenes must be ordered and must not overlap, but they may leave gaps; frames in a gap get no grain.
The resulting table is consumed by `apply -g`:

```sh
grav1synth generate scenes.json -o grain_file.txt --reference my_encode.mkv
grav1synth apply my_encode.mkv -o grainy_encode.mkv -g grain_file.txt
```

<!-- ### `grav1synth estimate my_source.mkv -o grain_file.txt`

Analyzes `my_source.mkv` and estimates the amount of noise in the source, then generates an appropriate film grain table at `grain_file.txt`. This is less accurate than the diff method, but is significantly faster. -->

## Debug Tracing

To enable AV1 bitstream trace header output (similar to FFmpeg's `trace_headers` bitstream filter), set the `RUST_LOG` environment variable:

```sh
RUST_LOG=trace_headers=debug grav1synth inspect my_encode.mkv -o grain_file.txt
```

This outputs each parsed OBU field with its bit position, binary representation, and decimal value.

## Known Issues

- There have been reports that certain videos will fail to apply film grain properly. This is likely related to aomenc's `--keyframe-filtering=2`.
- Frames shown through `show_existing_frame` (as used for alternate-reference frames) are left untouched by `apply` and display the grain baked into the frame they reference. A scene switch therefore takes effect on the first coded frame at or after the segment boundary.
