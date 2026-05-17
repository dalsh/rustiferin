//! Per-zone averaging bench. Last measured on AMD Ryzen 7 9800X3D 8-Core Processor,
//! rustc 1.90.0 (1159e78c4 2025-09-14):
//!
//!   1920x1080_100zones: 2130 µs / frame  (~469 fps headroom)
//!   3840x2160_100zones: 8590 µs / frame  (~116 fps headroom)
//!
//! Soft target is < 5 ms at 1080p; 4K has no formal
//! target but at 60fps the budget is ~16.7 ms.

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use rustiferin::capture::{Frame, PixelFormat};
use rustiferin::config::schema::{AveragingMode, LedMatrixConfig, LedZone};
use rustiferin::pipeline::zones::average_zones;
use rustiferin::pipeline::LedColor;

fn build_frame(width: u32, height: u32) -> Frame {
    let stride = width * 4;
    let mut buf = vec![0u8; (stride * height) as usize];
    // Cheap-but-non-trivial fill so the compiler can't constant-fold the averaging.
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    for chunk in buf.chunks_exact_mut(4) {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        chunk[0] = (state >> 56) as u8;
        chunk[1] = (state >> 48) as u8;
        chunk[2] = (state >> 40) as u8;
        chunk[3] = 255;
    }
    Frame {
        buf,
        width,
        height,
        stride,
        format: PixelFormat::Bgra,
    }
}

fn build_cfg(width: u32, height: u32) -> LedMatrixConfig {
    // 100 zones in a 10×10 grid.
    let mut zones = Vec::with_capacity(100);
    let zw = width / 10;
    let zh = height / 10;
    for row in 0..10 {
        for col in 0..10 {
            zones.push(LedZone {
                x: col * zw,
                y: row * zh,
                w: zw,
                h: zh,
            });
        }
    }
    LedMatrixConfig {
        reference_width: width,
        reference_height: height,
        zones,
        ..Default::default()
    }
}

fn bench_average_zones(c: &mut Criterion) {
    let mut group = c.benchmark_group("average_zones");

    // One scratch buffer per bench harness, mirroring the pipeline thread's
    // lifetime ownership: this is what we're measuring, the alloc-free path.
    let mut dominant_scratch: Vec<[f32; 3]> = Vec::new();

    let frame_hd = build_frame(1920, 1080);
    let cfg_hd = build_cfg(1920, 1080);
    let mut out_hd = vec![LedColor::default(); cfg_hd.zones.len()];
    group.bench_function("1920x1080_100zones_mean", |b| {
        b.iter(|| {
            average_zones(
                black_box(&frame_hd),
                black_box(&cfg_hd),
                1,
                AveragingMode::Mean,
                &mut dominant_scratch,
                &mut out_hd,
            )
        });
    });
    group.bench_function("1920x1080_100zones_dominant_adv", |b| {
        b.iter(|| {
            average_zones(
                black_box(&frame_hd),
                black_box(&cfg_hd),
                1,
                AveragingMode::DominantAdv,
                &mut dominant_scratch,
                &mut out_hd,
            )
        });
    });

    let frame_4k = build_frame(3840, 2160);
    let cfg_4k = build_cfg(3840, 2160);
    let mut out_4k = vec![LedColor::default(); cfg_4k.zones.len()];
    group.bench_function("3840x2160_100zones_mean", |b| {
        b.iter(|| {
            average_zones(
                black_box(&frame_4k),
                black_box(&cfg_4k),
                1,
                AveragingMode::Mean,
                &mut dominant_scratch,
                &mut out_4k,
            )
        });
    });
    group.bench_function("3840x2160_100zones_dominant_adv", |b| {
        b.iter(|| {
            average_zones(
                black_box(&frame_4k),
                black_box(&cfg_4k),
                1,
                AveragingMode::DominantAdv,
                &mut dominant_scratch,
                &mut out_4k,
            )
        });
    });

    group.finish();
}

criterion_group!(benches, bench_average_zones);
criterion_main!(benches);
