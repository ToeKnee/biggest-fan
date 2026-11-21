//! The GPU stressor: a Vulkan compute hot loop, written in WGSL and
//! compiled at runtime by wgpu — no CUDA/OpenCL toolchain needed, just the
//! installed driver (same card NVML already talks to in `core`).
//!
//! The shader burns fused multiply-add chains that are cross-coupled and
//! clamped, in the spirit of `cpu.rs`: a compiler cannot fold them into a
//! closed form, and the per-thread result is stored to a buffer whose
//! checksum is verified after the run, so "the loop silently optimised
//! away" becomes a loud error rather than a silently cool GPU.
//!
//! Work arrives as repeated bounded dispatches, sized so each one is a
//! fraction of a second of shader time, so stopping means simply not
//! submitting more, and the shared thermal guard (see `thermal.rs`) lands
//! within a chunk.

use crate::thermal::{self, LabError};
use clap::Args;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const SHADER: &str = r#"
struct Params {
    iterations: u32,
    _pad: u32,
    _pad2: u32,
    _pad3: u32,
};

@group(0) @binding(0) var<storage, read_write> out: array<f32>;
@group(0) @binding(1) var<uniform> params: Params;

@compute @workgroup_size(128)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let tid = gid.x;
    // Per-thread seeds; keep chains bounded with the min() clamps below.
    let s0 = f32(tid % 7u) * 0.11 + 0.31;
    let s1 = f32(tid % 11u) * 0.07 + 0.53;
    let s2 = f32(tid % 13u) * 0.09 + 0.29;
    let s3 = f32(tid % 5u) * 0.13 + 0.41;
    var a = s0; var b = s1; var c = s2; var d = s3;
    var e = s1 * 0.9 + 0.05; var f = s2 * 0.9 + 0.07;
    var g = s3 * 0.9 + 0.11; var h = s0 * 0.9 + 0.13;
    for (var i = 0u; i < params.iterations; i = i + 1u) {
        // Cross-coupled pairs: each chain's partner feeds the next step.
        let t0 = a; a = a * b + s0; b = t0 * a + s1;
        let t1 = c; c = c * d + s2; d = t1 * c + s3;
        let t2 = e; e = e * f + s0; f = t2 * e + s2;
        let t3 = g; g = g * h + s1; h = t3 * g + s3;
        a = min(a, 4.0); b = min(b, 4.0); c = min(c, 4.0); d = min(d, 4.0);
        e = min(e, 4.0); f = min(f, 4.0); g = min(g, 4.0); h = min(h, 4.0);
    }
    out[tid] = a + b * 0.5 + c * 0.25 + d * 0.125
        + e * 1.5 + f * 0.75 + g * 0.375 + h * 0.1875;
}
"#;

/// 16 flops per iteration per thread: 8 cross-coupled FMAs × 2 flops.
const FLOPS_PER_ITER: f64 = 16.0;

#[derive(Args, Clone, Debug)]
pub struct GpuArgs {
    /// Seconds of stress to run.
    #[arg(long, default_value_t = 60)]
    pub secs: u64,
    /// Total shader threads (must be a multiple of 128). Measured on an
    /// RTX 4070 SUPER: 65536 threads only reach ~2 TFLOP/s (99 W) because
    /// the FMA chains are latency-bound; ~2M threads fill the SMs for
    /// ~13 TFLOP/s and ~200 W, close to the card's power limit.
    #[arg(long, default_value_t = 2_097_152)]
    pub threads: u32,
    /// Shader loop iterations per dispatch; smaller means snappier stops.
    #[arg(long, default_value_t = 20_000)]
    pub iterations: u32,
    /// Temperature (k10temp or GPU) that aborts the run; 0 disables.
    #[arg(long, default_value_t = 85.0)]
    pub max_temp_c: f64,
    /// hwmon root to find k10temp under (test seam).
    #[arg(long, default_value = "/sys/class/hwmon")]
    pub hwmon_root: PathBuf,
}

impl Default for GpuArgs {
    fn default() -> Self {
        Self {
            secs: 60,
            threads: 2_097_152,
            iterations: 20_000,
            max_temp_c: 85.0,
            hwmon_root: PathBuf::from("/sys/class/hwmon"),
        }
    }
}

/// The CLI wrapper: fresh flags, one printed summary line, and a thermal
/// trip becomes [`LabError::Thermal`] (exit code 3 upstream).
pub fn run(args: &GpuArgs) -> Result<i32, LabError> {
    let stop = Arc::new(AtomicBool::new(false));
    let tripped = Arc::new(AtomicBool::new(false));
    let report = stress(args, stop, tripped)?;
    println!("stress-lab gpu: {}", report.summary);
    if report.tripped {
        return Err(LabError::Thermal(format!(
            "gpu stress aborted by the thermal guard after {:.1} s",
            report.elapsed_secs
        )));
    }
    Ok(0)
}

/// Grinds the GPU until `stop` goes up or `args.secs` elapses — the
/// library entry point the GUI's Stress lab panel uses. The caller owns
/// the flags: raising `stop` ends the run between dispatches, and
/// `tripped` goes up when the thermal guard aborts it. Nothing is
/// printed; the numbers arrive as a [`thermal::StressReport`].
pub fn stress(
    args: &GpuArgs,
    stop: Arc<AtomicBool>,
    tripped: Arc<AtomicBool>,
) -> Result<thermal::StressReport, LabError> {
    if args.threads == 0 || !args.threads.is_multiple_of(128) {
        return Err(LabError::Failed(
            "--threads must be a positive multiple of the 128-thread workgroup size".to_string(),
        ));
    }
    if args.threads / 128 > 65_535 {
        return Err(LabError::Failed(format!(
            "{} threads exceeds the 65535-workgroup-per-dimension limit",
            args.threads
        )));
    }

    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        // Vulkan only: this is the real card, not a llvmpipe stand-in.
        backends: wgpu::Backends::VULKAN,
        ..Default::default()
    });
    let Some(adapter) =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
    else {
        return Err(LabError::Failed(
            "no Vulkan adapter at all — run this command unsandboxed".to_string(),
        ));
    };
    let info = adapter.get_info();
    if info.device_type == wgpu::DeviceType::Cpu {
        // Inside the sandbox /dev/dri is hidden, but a software Vulkan
        // (llvmpipe) is still visible — stressing it burns CPU, not the
        // card, which would silently ruin a thermal experiment.
        return Err(LabError::Failed(format!(
            "only software Vulkan visible ({}) — /dev/dri is hidden inside the sandbox; run this command unsandboxed",
            info.name
        )));
    }

    let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("stress-lab"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::default(),
        },
        None,
    ))
    .map_err(|error| LabError::Failed(format!("could not open the GPU: {error}")))?;

    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("stress-hot-loop"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("stress-hot-loop"),
        layout: None,
        module: &module,
        entry_point: Some("main"),
        compilation_options: Default::default(),
        cache: None,
    });

    // wgpu 24 refuses STORAGE and MAP_READ on one buffer, so the shader
    // writes a copyable storage buffer that is then copied into a buffer
    // dedicated to the map-readback.
    let buffer_size = u64::from(args.threads) * 4;
    let out_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("stress-results"),
        size: buffer_size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let readback_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("stress-checksum"),
        size: buffer_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let params_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("stress-params"),
        size: 16,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    // Uniform `Params`: u32 iterations then 12 bytes of padding (the
    // struct must stay 16-byte aligned for wgpu's uniform rules).
    let params_bytes: Vec<u8> = [args.iterations.to_le_bytes(), [0; 4], [0; 4], [0; 4]].concat();
    queue.write_buffer(&params_buffer, 0, &params_bytes);

    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("stress-binds"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: out_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: params_buffer.as_entire_binding(),
            },
        ],
    });

    let guard = thermal::spawn_guard(
        thermal::k10temp_path(&args.hwmon_root),
        args.max_temp_c,
        Arc::clone(&stop),
        Arc::clone(&tripped),
    );

    let workgroups = args.threads / 128;

    let start = Instant::now();
    let deadline = start + Duration::from_secs(args.secs);
    let mut dispatches: u64 = 0;
    while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("stress-dispatch"),
        });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(workgroups, 1, 1);
        }
        queue.submit([encoder.finish()]);
        dispatches += 1;
        // One dispatch in flight at a time: honest per-dispatch timing,
        // bounded queue, and the guard's stop flag lands within a chunk.
        device.poll(wgpu::Maintain::Wait);
    }
    // The deadline ended the run: tell the guard thread to leave, or its
    // join below would wait forever.
    stop.store(true, Ordering::Relaxed);

    let elapsed = start.elapsed().as_secs_f64().max(1e-6);
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("stress-readback"),
    });
    encoder.copy_buffer_to_buffer(&out_buffer, 0, &readback_buffer, 0, buffer_size);
    queue.submit([encoder.finish()]);
    device.poll(wgpu::Maintain::Wait);
    let checksum = map_checksum(&device, &readback_buffer)?;
    if let Some(guard) = guard {
        let _ = guard.join();
    }

    #[expect(clippy::cast_precision_loss, reason = "throughput reporting only")]
    let flops =
        dispatches as f64 * f64::from(args.threads) * f64::from(args.iterations) * FLOPS_PER_ITER;
    if checksum.is_nan() || checksum <= 0.0 || !checksum.is_finite() {
        return Err(LabError::Failed(format!(
            "shader checksum {checksum} — the hot loop did not run as written"
        )));
    }
    let summary = format!(
        "{}: {} threads, {}/dispatch — {dispatches} dispatches in {elapsed:.1} s, \
         ~{:.0} GFLOP/s (f32 shader estimate), checksum {checksum:.3}",
        info.name,
        args.threads,
        args.iterations,
        flops / elapsed / 1e9,
    );
    Ok(thermal::StressReport {
        elapsed_secs: elapsed,
        summary,
        tripped: tripped.load(Ordering::Relaxed),
    })
}

/// Reads the shader's per-thread results back and sums them: a
/// non-zero, finite sum proves the loop actually ran.
fn map_checksum(device: &wgpu::Device, buffer: &wgpu::Buffer) -> Result<f32, LabError> {
    let (sender, receiver) = std::sync::mpsc::channel();
    buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
    loop {
        match receiver.try_recv() {
            Ok(Ok(())) => break,
            Ok(Err(error)) => {
                return Err(LabError::Failed(format!(
                    "GPU buffer readback failed: {error}"
                )));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                device.poll(wgpu::Maintain::Poll);
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return Err(LabError::Failed(
                    "GPU buffer map callback vanished".to_string(),
                ));
            }
        }
    }
    // The mapped view must be dropped before unmap: unmapping a buffer
    // that still has accessible views aborts inside wgpu.
    let total = {
        let mapped = buffer.slice(..).get_mapped_range();
        mapped
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            .fold(0.0_f32, |acc, value| acc + value)
    };
    buffer.unmap();
    Ok(total)
}
