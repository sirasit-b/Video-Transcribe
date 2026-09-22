//! Working out what this machine can encode with, and picking the fastest.
//!
//! A hardware encoder is 5-15x faster than x264 at these settings, and the render is
//! the whole cost of a trim, so one is used whenever the machine has one. ffmpeg
//! being *built* with `h264_nvenc` says nothing about whether this container can
//! reach a GPU — the Debian build advertises NVENC, QSV and VAAPI on a machine with
//! no `/dev/dri` at all — so nothing is taken on trust: each candidate is tried
//! against a real clip, through the same filter chain the renderer uses, and only a
//! candidate that produces frames is used.
//!
//! Each candidate is tried twice. First decoding on the GPU too, which keeps frames
//! there for the whole pipeline (`select` and `setpts` pass frames along without
//! touching pixels, so they work on hardware frames); then, if that fails, decoding
//! on the CPU and letting the encoder upload. The first shape that works wins.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct Encoder {
    pub name: String,
    pub hardware: bool,
    /// How this encoder was reached: "gpu decode + gpu encode", and so on.
    pub pipeline: String,
    /// Options that must appear before the input (device setup, `-hwaccel`).
    pub input_args: Vec<String>,
    /// Appended to the chunk's filter chain, for encoders that need the frames
    /// moved onto the device.
    pub filter_suffix: String,
    /// Rate control, resolved from the requested quality.
    pub quality: Vec<String>,
    /// Left unset when frames live on the GPU: the format is the hardware's.
    pub pix_fmt: Option<String>,
    /// A first guess at pixels per second, used for the estimate until the
    /// calibration has seen a real render.
    pub pixel_rate_seed: f64,
}

impl Encoder {
    pub fn software(preset: &str, crf: i32, threads: usize) -> Encoder {
        let mut quality = Vec::new();
        if !preset.is_empty() {
            quality.extend(["-preset".to_string(), preset.to_string()]);
        }
        if crf >= 0 {
            quality.extend(["-crf".to_string(), crf.to_string()]);
        }
        Encoder {
            name: "libx264".to_string(),
            hardware: false,
            pipeline: "cpu decode + cpu encode".to_string(),
            input_args: Vec::new(),
            filter_suffix: String::new(),
            quality,
            pix_fmt: Some("yuv420p".to_string()),
            // Roughly what one x264 thread manages on 1080p at these presets.
            pixel_rate_seed: threads.max(1) as f64 * 12_000_000.0,
        }
    }
}

/// One way of reaching an encoder, before it is known to work.
struct Candidate {
    name: &'static str,
    /// Hardware decode + device setup. Empty means the CPU decodes.
    input_args: Vec<String>,
    filter_suffix: &'static str,
    pix_fmt: Option<&'static str>,
    pipeline: &'static str,
    pixel_rate_seed: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Attempt {
    pub encoder: String,
    pub pipeline: String,
    pub ok: bool,
    /// Why it was not usable, trimmed to something readable.
    pub detail: Option<String>,
}

/// What the machine turned out to have.
#[derive(Clone, Debug, Serialize)]
pub struct Capabilities {
    pub cpu_model: String,
    pub cpu_cores: usize,
    /// Encode devices visible inside the container, which is what actually
    /// decides whether a GPU can be used here.
    pub devices: Vec<String>,
    /// Encoders this ffmpeg was built with, whether or not they work here.
    pub built_with: Vec<String>,
    pub attempts: Vec<Attempt>,
    pub chosen: String,
    pub hardware: bool,
    pub pipeline: String,
}

fn quality_args(name: &str, crf: i32, preset: &str) -> Vec<String> {
    let quality = if crf >= 0 { crf } else { 20 };
    match name {
        // p4 is NVENC's middle preset; `cq` is its constant-quality knob, and it
        // only takes effect with the bitrate target cleared.
        "h264_nvenc" | "hevc_nvenc" => vec![
            "-preset".into(),
            "p4".into(),
            "-rc".into(),
            "vbr".into(),
            "-cq".into(),
            quality.to_string(),
            "-b:v".into(),
            "0".into(),
        ],
        "h264_qsv" | "hevc_qsv" => vec![
            "-preset".into(),
            "veryfast".into(),
            "-global_quality".into(),
            quality.to_string(),
        ],
        // VAAPI's qp is on the same 0-51 scale as crf, near enough for this.
        "h264_vaapi" | "hevc_vaapi" => vec!["-qp".into(), quality.to_string()],
        _ => {
            let mut args = Vec::new();
            if !preset.is_empty() {
                args.extend(["-preset".to_string(), preset.to_string()]);
            }
            if crf >= 0 {
                args.extend(["-crf".to_string(), crf.to_string()]);
            }
            args
        }
    }
}

fn vaapi_device() -> String {
    std::env::var("VAAPI_DEVICE").unwrap_or_else(|_| "/dev/dri/renderD128".to_string())
}

/// Every way we know of reaching a hardware encoder, fastest shape first.
fn candidates() -> Vec<Candidate> {
    let device = vaapi_device();
    vec![
        Candidate {
            name: "h264_nvenc",
            input_args: vec![
                "-hwaccel".into(),
                "cuda".into(),
                "-hwaccel_output_format".into(),
                "cuda".into(),
            ],
            filter_suffix: "",
            pix_fmt: None,
            pipeline: "gpu decode + gpu encode (nvenc)",
            pixel_rate_seed: 500_000_000.0,
        },
        Candidate {
            name: "h264_nvenc",
            input_args: Vec::new(),
            filter_suffix: "",
            // NVENC takes CPU frames and uploads them itself.
            pix_fmt: Some("yuv420p"),
            pipeline: "cpu decode + gpu encode (nvenc)",
            pixel_rate_seed: 300_000_000.0,
        },
        Candidate {
            name: "h264_qsv",
            input_args: vec![
                "-hwaccel".into(),
                "qsv".into(),
                "-hwaccel_output_format".into(),
                "qsv".into(),
            ],
            filter_suffix: "",
            pix_fmt: None,
            pipeline: "gpu decode + gpu encode (quick sync)",
            pixel_rate_seed: 300_000_000.0,
        },
        Candidate {
            name: "h264_qsv",
            input_args: Vec::new(),
            filter_suffix: "",
            pix_fmt: Some("nv12"),
            pipeline: "cpu decode + gpu encode (quick sync)",
            pixel_rate_seed: 200_000_000.0,
        },
        Candidate {
            name: "h264_vaapi",
            input_args: vec![
                "-hwaccel".into(),
                "vaapi".into(),
                "-hwaccel_device".into(),
                device.clone(),
                "-hwaccel_output_format".into(),
                "vaapi".into(),
            ],
            filter_suffix: "",
            pix_fmt: None,
            pipeline: "gpu decode + gpu encode (vaapi)",
            pixel_rate_seed: 250_000_000.0,
        },
        Candidate {
            name: "h264_vaapi",
            // Frames decoded on the CPU have to be uploaded by the filter chain,
            // and VAAPI wants them as nv12 on the way up.
            input_args: vec!["-vaapi_device".into(), device],
            filter_suffix: ",format=nv12,hwupload",
            pix_fmt: None,
            pipeline: "cpu decode + gpu encode (vaapi)",
            pixel_rate_seed: 150_000_000.0,
        },
    ]
}

/// The CPU, as far as the container can see it.
fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| line.starts_with("model name"))
                .and_then(|line| line.split_once(':'))
                .map(|(_, value)| value.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// Render and encode devices passed into the container. Their absence is the
/// usual reason a GPU cannot be used even on a machine that has one.
fn devices() -> Vec<String> {
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/dev/dri") {
        for entry in entries.flatten() {
            found.push(entry.path().to_string_lossy().into_owned());
        }
    }
    if let Ok(entries) = std::fs::read_dir("/dev") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("nvidia") {
                found.push(format!("/dev/{name}"));
            }
        }
    }
    found.sort();
    found
}

/// Hardware encoders this ffmpeg was compiled with — which says what *could* work,
/// not what does.
fn built_with(ffmpeg: &str) -> Vec<String> {
    let Ok(output) = Command::new(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .stdin(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .filter(|name| {
            ["nvenc", "qsv", "vaapi", "videotoolbox", "amf"]
                .iter()
                .any(|kind| name.contains(kind))
        })
        .map(|name| name.to_string())
        .collect()
}

/// A short h264 clip to probe against. Probing with `lavfi` alone would never
/// exercise hardware *decoding*, which is half of what is being tested.
fn probe_clip(ffmpeg: &str, work_dir: &Path) -> Option<PathBuf> {
    let path = work_dir.join("encoder-probe.mp4");
    if path.is_file() {
        return Some(path);
    }
    let ok = Command::new(ffmpeg)
        .args([
            "-v",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=30:duration=1",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "15",
        ])
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    ok.then_some(path)
}

/// Run the real pipeline shape — hardware decode, `select`, `setpts`, hardware
/// encode — over the probe clip. Anything that cannot do that is not usable for a
/// render, whatever `-encoders` claims.
fn try_pipeline(
    ffmpeg: &str,
    clip: &Path,
    name: &str,
    input_args: &[String],
    filter_suffix: &str,
    pix_fmt: Option<&str>,
    quality: &[String],
) -> Result<(), String> {
    let filter = format!(
        "[0:v]select='between(t\\,0.0\\,0.5)',setpts=N/TB/30.0{filter_suffix}[v]"
    );

    let mut command = Command::new(ffmpeg);
    command.args(["-v", "error", "-nostdin", "-y"]);
    command.args(input_args);
    command.arg("-i").arg(clip);
    command
        .arg("-filter_complex")
        .arg(filter)
        .args(["-map", "[v]", "-an", "-sn", "-dn"])
        .args(["-c:v", name])
        .args(quality);
    if let Some(format) = pix_fmt {
        command.args(["-pix_fmt", format]);
    }
    command
        .args(["-r", "30", "-fps_mode", "cfr", "-frames:v", "5", "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let output = command
        .output()
        .map_err(|err| format!("could not run ffmpeg: {err}"))?;
    if output.status.success() {
        return Ok(());
    }

    // ffmpeg's reason is usually the last line it printed.
    let message = String::from_utf8_lossy(&output.stderr);
    let detail = message
        .lines()
        .filter(|line| !line.trim().is_empty())
        .next_back()
        .unwrap_or("encoder failed")
        .trim()
        .to_string();
    Err(detail.chars().take(200).collect())
}

/// Find the best encoder this machine can actually use.
///
/// `forced` names one to use regardless of speed (still smoke-tested, and still
/// falling back if it cannot run here).
pub fn detect(
    ffmpeg: &str,
    work_dir: &Path,
    forced: Option<&str>,
    preset: &str,
    crf: i32,
    threads: usize,
) -> (Encoder, Capabilities) {
    let mut capabilities = Capabilities {
        cpu_model: cpu_model(),
        cpu_cores: std::thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(0),
        devices: devices(),
        built_with: built_with(ffmpeg),
        attempts: Vec::new(),
        chosen: String::new(),
        hardware: false,
        pipeline: String::new(),
    };

    let software = Encoder::software(preset, crf, threads);
    if forced == Some("libx264") {
        return finish(software, capabilities, "forced by VIDEO_CODEC");
    }

    let Some(clip) = probe_clip(ffmpeg, work_dir) else {
        eprintln!("auto-trim: could not build a probe clip; using libx264");
        return finish(software, capabilities, "no probe clip");
    };

    let wanted: Vec<Candidate> = match forced.filter(|name| !name.is_empty()) {
        // A named encoder still gets both shapes tried, fastest first.
        Some(name) => candidates()
            .into_iter()
            .filter(|candidate| candidate.name == name)
            .collect(),
        None => candidates(),
    };

    if let Some(name) = forced.filter(|name| !name.is_empty()) {
        if wanted.is_empty() {
            // Something we have no recipe for: try it as a plain encoder.
            let quality = quality_args(name, crf, preset);
            match try_pipeline(ffmpeg, &clip, name, &[], "", Some("yuv420p"), &quality) {
                Ok(()) => {
                    let encoder = Encoder {
                        name: name.to_string(),
                        hardware: false,
                        pipeline: "cpu decode + cpu encode".to_string(),
                        input_args: Vec::new(),
                        filter_suffix: String::new(),
                        quality,
                        pix_fmt: Some("yuv420p".to_string()),
                        pixel_rate_seed: software.pixel_rate_seed,
                    };
                    capabilities.attempts.push(Attempt {
                        encoder: name.to_string(),
                        pipeline: encoder.pipeline.clone(),
                        ok: true,
                        detail: None,
                    });
                    return finish(encoder, capabilities, "forced by VIDEO_CODEC");
                }
                Err(detail) => {
                    capabilities.attempts.push(Attempt {
                        encoder: name.to_string(),
                        pipeline: "cpu decode + cpu encode".to_string(),
                        ok: false,
                        detail: Some(detail),
                    });
                    eprintln!("auto-trim: {name} cannot encode here; using libx264");
                    return finish(software, capabilities, "fallback");
                }
            }
        }
    }

    for candidate in wanted {
        let quality = quality_args(candidate.name, crf, preset);
        let result = try_pipeline(
            ffmpeg,
            &clip,
            candidate.name,
            &candidate.input_args,
            candidate.filter_suffix,
            candidate.pix_fmt,
            &quality,
        );
        match result {
            Ok(()) => {
                capabilities.attempts.push(Attempt {
                    encoder: candidate.name.to_string(),
                    pipeline: candidate.pipeline.to_string(),
                    ok: true,
                    detail: None,
                });
                let encoder = Encoder {
                    name: candidate.name.to_string(),
                    hardware: true,
                    pipeline: candidate.pipeline.to_string(),
                    input_args: candidate.input_args,
                    filter_suffix: candidate.filter_suffix.to_string(),
                    quality,
                    pix_fmt: candidate.pix_fmt.map(|format| format.to_string()),
                    pixel_rate_seed: candidate.pixel_rate_seed,
                };
                return finish(encoder, capabilities, "detected");
            }
            Err(detail) => capabilities.attempts.push(Attempt {
                encoder: candidate.name.to_string(),
                pipeline: candidate.pipeline.to_string(),
                ok: false,
                detail: Some(detail),
            }),
        }
    }

    finish(software, capabilities, "no usable hardware encoder")
}

fn finish(encoder: Encoder, mut capabilities: Capabilities, why: &str) -> (Encoder, Capabilities) {
    capabilities.chosen = encoder.name.clone();
    capabilities.hardware = encoder.hardware;
    capabilities.pipeline = encoder.pipeline.clone();
    if encoder.hardware {
        eprintln!(
            "auto-trim: {} on {} cores; encoding with {} ({})",
            capabilities.cpu_model, capabilities.cpu_cores, encoder.name, encoder.pipeline
        );
    } else {
        eprintln!(
            "auto-trim: {} on {} cores; encoding with {} ({why}){}",
            capabilities.cpu_model,
            capabilities.cpu_cores,
            encoder.name,
            if capabilities.devices.is_empty() {
                " — no GPU device is visible in this container"
            } else {
                ""
            }
        );
    }
    (encoder, capabilities)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_family_gets_its_own_rate_control() {
        assert_eq!(
            quality_args("h264_nvenc", 20, "veryfast"),
            vec!["-preset", "p4", "-rc", "vbr", "-cq", "20", "-b:v", "0"]
        );
        assert_eq!(
            quality_args("h264_qsv", 22, "veryfast"),
            vec!["-preset", "veryfast", "-global_quality", "22"]
        );
        assert_eq!(quality_args("h264_vaapi", 24, ""), vec!["-qp", "24"]);
        assert_eq!(
            quality_args("libx264", 20, "veryfast"),
            vec!["-preset", "veryfast", "-crf", "20"]
        );
    }

    #[test]
    fn the_gpu_pipeline_is_tried_before_the_half_way_one() {
        // For each family, keeping frames on the device is tried before letting
        // the encoder upload them.
        let all = candidates();
        let nvenc: Vec<&Candidate> = all
            .iter()
            .filter(|candidate| candidate.name == "h264_nvenc")
            .collect();
        assert_eq!(nvenc.len(), 2);
        assert!(nvenc[0].input_args.contains(&"cuda".to_string()));
        assert!(nvenc[1].input_args.is_empty());
        assert!(nvenc[0].pixel_rate_seed > nvenc[1].pixel_rate_seed);
    }

    #[test]
    fn frames_on_the_device_carry_no_pixel_format() {
        // `-pix_fmt yuv420p` against a hardware frame is a contradiction; only the
        // CPU-decode shapes set one.
        for candidate in candidates() {
            let on_device = candidate
                .input_args
                .iter()
                .any(|arg| arg == "-hwaccel_output_format");
            assert!(
                !(on_device && candidate.pix_fmt.is_some()),
                "{} sets a pixel format for device frames",
                candidate.pipeline
            );
        }
    }

    #[test]
    fn cpu_frames_for_vaapi_are_uploaded_by_the_filter_chain() {
        let uploads: Vec<(&str, &str)> = candidates()
            .iter()
            .filter(|candidate| !candidate.filter_suffix.is_empty())
            .map(|candidate| (candidate.name, candidate.filter_suffix))
            .collect();
        assert_eq!(uploads.len(), 1, "only VAAPI needs an explicit upload");
        assert_eq!(uploads[0].0, "h264_vaapi");
        assert!(uploads[0].1.contains("hwupload"));
    }

    #[test]
    fn software_scales_its_guess_with_the_thread_count() {
        let small = Encoder::software("veryfast", 20, 4);
        let large = Encoder::software("veryfast", 20, 16);
        assert!(large.pixel_rate_seed > small.pixel_rate_seed);
        assert!(!large.hardware);
        assert_eq!(large.pix_fmt.as_deref(), Some("yuv420p"));
    }
}
