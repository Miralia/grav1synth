//! `generate` subcommand: build a multi-scene AV1 film-grain table (`.tbl`)
//! from a JSON scene spec, reusing av1-grain's canonical photon-noise physics.
//!
//! Each scene carries its own ISO; the segments are concatenated into one
//! `filmgrn1` table consumed by `grav1synth apply` (and aomenc / SVT / rav1e).
//!
//! Segment time boundaries are derived from a `--reference` video's actual
//! packet PTSes via [`crate::parser::pts_to_av1_ts`], so `generate` and
//! `apply` produce byte-identical 100ns timestamps for the same packet and the
//! scene switch lands exactly on the intended frame (VFR-safe).

use std::path::Path;

use anyhow::{Context, Result, ensure};
use av1_grain::{
    GrainTableSegment, NoiseGenArgs, TransferFunction, generate_photon_noise_params,
    write_grain_table,
};
use ffmpeg::ffi::{AVColorRange, AVColorTransferCharacteristic};
use log::{info, warn};
use serde::Deserialize;

use crate::parser::pts_to_av1_ts;
use crate::reader::BitstreamReader;

/// Top-level JSON spec. Every field except `scenes` is optional and is filled
/// from `--reference` when absent (required if no `--reference` is given).
#[derive(Debug, Deserialize)]
pub struct GenerateSpec {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<Rational>,
    pub transfer_function: Option<TransferFunctionSpec>,
    pub full_range: Option<bool>,
    pub scenes: Vec<SceneSpec>,
}

#[derive(Debug, Deserialize)]
pub struct Rational {
    pub num: u64,
    pub den: u64,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum TransferFunctionSpec {
    Bt1886,
    Smpte2084,
}

impl TransferFunctionSpec {
    #[must_use]
    pub fn to_av1(self) -> TransferFunction {
        match self {
            Self::Bt1886 => TransferFunction::BT1886,
            Self::Smpte2084 => TransferFunction::SMPTE2084,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SceneSpec {
    pub start_frame: u64,
    pub end_frame: u64,
    pub iso: u32,
}

/// Fully-resolved spec ready for table generation.
pub struct ResolvedSpec {
    pub width: u32,
    pub height: u32,
    pub transfer_function: TransferFunction,
    pub full_range: bool,
    pub scenes: Vec<SceneSpec>,
    /// Per-scene `(start_time, end_time)` in 100ns ticks, aligned to the
    /// reference's packet PTSes. `end_time` of the final scene is `u64::MAX`.
    pub segment_times: Vec<(u64, u64)>,
}

/// Read & deserialize the JSON spec.
pub fn load_spec(spec_path: &Path) -> Result<GenerateSpec> {
    let data = std::fs::read_to_string(spec_path)
        .with_context(|| format!("Failed to read spec {}", spec_path.to_string_lossy()))?;
    serde_json::from_str::<GenerateSpec>(&data)
        .with_context(|| format!("Failed to parse spec {}", spec_path.to_string_lossy()))
}

/// Merge the spec with `--reference` (filling width/height/transfer/full_range
/// and building PTS-aligned `segment_times`), or fall back to fps-rational
/// timing when no reference is provided.
pub fn resolve(spec: GenerateSpec, reference: Option<&Path>) -> Result<ResolvedSpec> {
    validate_scenes(&spec.scenes)?;

    // Pull optional scalar fields out of the spec before we move `scenes`.
    let spec_width = spec.width;
    let spec_height = spec.height;
    let spec_fps = spec.fps;
    let tf_spec = spec.transfer_function;
    let full_range_spec = spec.full_range;

    let mut resolved = ResolvedSpec {
        width: spec_width.unwrap_or(0),
        height: spec_height.unwrap_or(0),
        transfer_function: tf_spec.map_or(TransferFunction::BT1886, TransferFunctionSpec::to_av1),
        full_range: full_range_spec.unwrap_or(false),
        scenes: spec.scenes,
        segment_times: Vec::new(),
    };

    if let Some(reference) = reference {
        let frame_ts = collect_reference_pts(reference)?;
        ensure!(
            !frame_ts.is_empty(),
            "Reference {} has no video packets",
            reference.to_string_lossy()
        );

        // Fill any missing fields from the reference stream parameters.
        let reader = BitstreamReader::open(reference)?;
        let details = reader.get_video_details();
        if resolved.width == 0 {
            resolved.width = details.width as u32;
        }
        if resolved.height == 0 {
            resolved.height = details.height as u32;
        }
        if tf_spec.is_none() || full_range_spec.is_none() {
            let stream = reader.get_video_stream()?;
            // SAFETY: dereference immediately, no lifetime escapes.
            let (trc, range) = unsafe {
                let params = stream.parameters().as_ptr();
                ((*params).color_trc, (*params).color_range)
            };
            if tf_spec.is_none() {
                resolved.transfer_function = transfer_function_from_trc(trc);
            }
            if full_range_spec.is_none() {
                resolved.full_range = range == AVColorRange::JPEG;
            }
        }

        ensure!(
            resolved.width > 0 && resolved.height > 0,
            "Width/height could not be resolved (spec or reference missing them)"
        );
        ensure!(
            resolved.scenes.last().unwrap().end_frame as usize <= frame_ts.len(),
            "Scene end_frame {} exceeds reference frame count {}",
            resolved.scenes.last().unwrap().end_frame,
            frame_ts.len()
        );

        resolved.segment_times = resolved
            .scenes
            .iter()
            .enumerate()
            .map(|(i, scene)| {
                let start = frame_ts[scene.start_frame as usize];
                let end = if i == resolved.scenes.len() - 1 {
                    u64::MAX
                } else {
                    frame_ts[scene.end_frame as usize]
                };
                (start, end)
            })
            .collect();
    } else {
        // No reference: require width/height/fps/transfer in the spec.
        ensure!(
            resolved.width > 0 && resolved.height > 0,
            "No --reference given; spec must include width and height"
        );
        // transfer_function defaults to BT1886 above (fine for SDR); warn if the
        // spec omitted it and the content is likely HDR (caller's responsibility).
        if tf_spec.is_none() {
            warn!("No --reference and no transfer_function in spec; defaulting to BT1886 (SDR).");
        }
        let fps = spec_fps
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No --reference given; spec must include fps"))?;
        ensure!(fps.num > 0 && fps.den > 0, "fps num/den must be > 0");
        resolved.segment_times = resolved
            .scenes
            .iter()
            .enumerate()
            .map(|(i, scene)| {
                let start = frame_to_ts_rational(scene.start_frame, fps);
                let end = if i == resolved.scenes.len() - 1 {
                    u64::MAX
                } else {
                    frame_to_ts_rational(scene.end_frame, fps)
                };
                (start, end)
            })
            .collect();
        warn!(
            "No --reference given; using fps {}/{} for timing. Scene boundaries may be off by \
             one frame for non-1/fps time bases. Pass --reference for exact alignment.",
            fps.num, fps.den
        );
    }

    Ok(resolved)
}

/// Build av1-grain segments (one per scene) and write the multi-segment `.tbl`.
pub fn write_table(resolved: &ResolvedSpec, out: &Path) -> Result<()> {
    let mut segments: Vec<GrainTableSegment> = Vec::with_capacity(resolved.scenes.len());
    for (scene, (start_time, end_time)) in resolved
        .scenes
        .iter()
        .zip(resolved.segment_times.iter().copied())
    {
        segments.push(generate_photon_noise_params(
            start_time,
            end_time,
            NoiseGenArgs {
                iso_setting: scene.iso,
                width: resolved.width,
                height: resolved.height,
                transfer_function: resolved.transfer_function,
                full_range: resolved.full_range,
                chroma_grain: false,
                // TODO phase 2: morphology — when av1-grain exposes ar_coeff_lag
                // via NoiseGenArgs (or we hand-build segments), thread a per-scene
                // morphology field here without touching timing.
                random_seed: None,
            },
        ));
    }
    write_grain_table(out, &segments)?;
    info!(
        "Wrote grain table ({} scenes) to {}",
        resolved.scenes.len(),
        out.to_string_lossy()
    );
    Ok(())
}

fn validate_scenes(scenes: &[SceneSpec]) -> Result<()> {
    ensure!(!scenes.is_empty(), "Spec must contain at least one scene");
    for (i, scene) in scenes.iter().enumerate() {
        ensure!(
            scene.iso > 0,
            "Scene {} iso must be > 0 (got {})",
            i,
            scene.iso
        );
        ensure!(
            scene.start_frame < scene.end_frame,
            "Scene {} start_frame ({}) must be < end_frame ({})",
            i,
            scene.start_frame,
            scene.end_frame
        );
        if i > 0 {
            // Scenes may have gaps (frames in a gap get no grain on `apply`).
            // Only forbid overlap/backwards ordering.
            ensure!(
                scene.start_frame >= scenes[i - 1].end_frame,
                "Scenes must be ordered & non-overlapping: scene {} starts at {} \
                 but previous ends at {}",
                i,
                scene.start_frame,
                scenes[i - 1].end_frame
            );
        }
    }
    Ok(())
}

/// Demux the reference (no decode) and collect per-frame packet PTSes in
/// 100ns-tick units — the same unit `apply` matches against.
fn collect_reference_pts(reference: &Path) -> Result<Vec<u64>> {
    let mut reader = BitstreamReader::open(reference)?;
    let stream_idx = reader.get_video_stream()?.index();
    let time_base = reader.get_video_stream()?.time_base();
    let input = reader.input();

    let mut frame_ts: Vec<u64> = Vec::new();
    for (stream, packet) in input.packets().filter_map(Result::ok) {
        if stream.index() != stream_idx {
            continue;
        }
        if packet.data().is_none() {
            continue;
        }
        let pts = packet.pts().unwrap_or_default();
        frame_ts.push(pts_to_av1_ts(pts, time_base));
    }
    Ok(frame_ts)
}

fn transfer_function_from_trc(trc: AVColorTransferCharacteristic) -> TransferFunction {
    match trc {
        AVColorTransferCharacteristic::SMPTE2084 => TransferFunction::SMPTE2084,
        // av1-grain has no HLG (arib-std-b67) variant; fall back to BT1886.
        AVColorTransferCharacteristic::ARIB_STD_B67 => {
            warn!("HLG (arib-std-b67) transfer has no av1-grain model; falling back to BT1886");
            TransferFunction::BT1886
        }
        _ => TransferFunction::BT1886,
    }
}

/// fps-rational fallback (no reference): `frame * den * 10_000_000 / num`,
/// ceiling-divided to match [`pts_to_av1_ts`]'s rounding.
fn frame_to_ts_rational(frame: u64, fps: &Rational) -> u64 {
    // frame / (num/den) * 10_000_000 = frame * den * 10_000_000 / num
    let v = (frame as u128)
        .saturating_mul(fps.den as u128)
        .saturating_mul(10_000_000u128);
    if fps.num == 0 {
        return 0;
    }
    v.div_ceil(fps.num as u128) as u64
}
