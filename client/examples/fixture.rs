//! Writes a keyword-directory blob and its address list, for the wasm smoke test.
//!
//! Usage: `cargo run --release -p usdt-pir-client --example fixture -- <out-dir> [count]`

use std::path::PathBuf;

use poulpy_pir::keyword::{KeywordDirectory, KeywordIndex};

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let out: PathBuf = args
        .next()
        .expect("usage: fixture <out-dir> [count]")
        .into();
    let count: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(10_000);

    let keys: Vec<[u8; 20]> = (0..count as u64)
        .map(|i| {
            let mut k = [0u8; 20];
            let h = i.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
            k[..8].copy_from_slice(&h.to_le_bytes());
            k[8..16].copy_from_slice(&(h ^ 0xA5A5_A5A5_A5A5_A5A5).to_le_bytes());
            k
        })
        .collect();

    let mphf = KeywordIndex::build(&keys).expect("mphf");
    let directory = KeywordDirectory::new(mphf, 33_554_432, 0).expect("directory");

    std::fs::create_dir_all(&out)?;
    let mut blob = Vec::new();
    directory.write_to(&mut blob)?;
    std::fs::write(out.join("directory.bin"), &blob)?;

    // Every 64th key, plus addresses the MPHF never saw, with the slot this
    // (64-bit) host resolves them to. A 32-bit client must agree on all of them.
    let probes: Vec<[u8; 20]> = keys
        .iter()
        .copied()
        .step_by(count.max(1) / 32 + 1)
        .chain((0..8u8).map(|i| [0xE0 | i; 20]))
        .collect();

    let listed: Vec<String> = probes
        .iter()
        .map(|k| {
            let mut s = String::from("{\"address\":\"0x");
            for b in k {
                s.push_str(&format!("{b:02x}"));
            }
            s.push_str(&format!("\",\"slot\":{}}}", directory.index(k)));
            s
        })
        .collect();
    std::fs::write(out.join("probes.json"), format!("[{}]", listed.join(",")))?;

    println!(
        "wrote {} bytes for {count} addresses to {}",
        blob.len(),
        out.display()
    );
    Ok(())
}
