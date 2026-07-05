//! Manual smoke probe for the KMS capture path. Not a test; run against a real
//! `gsr-kms-server`:
//!
//!   cargo run --example kms_probe --features kms -- /dev/dri/card1 /dev/dri/renderD129
//!
//! Does one GET_KMS, imports the primary plane via EGL, reads it back, and
//! prints plane metadata + pixel sanity (center pixel, frame average) so we can
//! confirm real pixels flow before wiring the capture loop.

#[cfg(feature = "kms")]
fn main() -> anyhow::Result<()> {
    use rustiferin::capture::kms::{
        client::close_response_fds, client::GsrKmsClient, egl::KmsImporter, protocol,
    };
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let card = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/dev/dri/card1".into());
    let render = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "/dev/dri/renderD129".into());
    eprintln!("card={card} render={render}");

    let mut client = GsrKmsClient::spawn(&card)?;
    let resp = client.get_kms()?;
    let n = resp.num_items.max(0) as usize;
    println!("num_items = {n}");
    for (i, it) in resp.items.iter().take(n).enumerate() {
        println!(
            "  item[{i}]: {}x{} cursor={} connector={} fmt={:#010x} modifier={:#018x} dma_bufs={}",
            it.width,
            it.height,
            it.is_cursor(),
            it.connector_id,
            it.pixel_format,
            it.modifier,
            it.num_dma_bufs
        );
    }

    let Some(idx) = protocol::select_primary_plane(&resp, None) else {
        close_response_fds(&resp);
        anyhow::bail!("no primary plane");
    };

    let importer = KmsImporter::new(&render)?;
    let mut buf = Vec::new();
    let (w, h, fmt) = importer.read_plane(&resp.items[idx], &mut buf)?;
    close_response_fds(&resp);

    println!("imported {w}x{h} {fmt:?}, {} bytes", buf.len());
    let png = "/tmp/kms_capture.png";
    image::save_buffer(png, &buf, w, h, image::ExtendedColorType::Rgba8)?;
    println!("wrote {png}");
    // Center pixel + whole-frame channel averages (RGBA byte order).
    let center = (((h / 2) * w + w / 2) * 4) as usize;
    println!("center RGBA = {:?}", &buf[center..center + 4]);
    let (mut sr, mut sg, mut sb) = (0u64, 0u64, 0u64);
    let px = (w as u64) * (h as u64);
    for c in buf.chunks_exact(4) {
        sr += c[0] as u64;
        sg += c[1] as u64;
        sb += c[2] as u64;
    }
    println!("frame avg RGB = ({}, {}, {})", sr / px, sg / px, sb / px);
    Ok(())
}

#[cfg(not(feature = "kms"))]
fn main() {
    eprintln!("built without the `kms` feature");
}
