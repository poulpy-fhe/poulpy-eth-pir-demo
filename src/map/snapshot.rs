use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::Path;

use alloy::primitives::Address;

use super::types::{BalanceMap, Entry};

/// `USDTPIR3` adds the trailing checksum; `USDTPIR2` files have no footer.
const SNAPSHOT_MAGIC: [u8; 8] = *b"USDTPIR3";
const LEGACY_MAGIC: [u8; 8] = *b"USDTPIR2";
const ROW: usize = 60;
/// Rows to read before believing a header's count. A torn or wrong file can
/// claim any number, and `HashMap::with_capacity` would try to honour it.
const CAPACITY_GUARD: usize = 1 << 20;

impl BalanceMap {
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = parent_to_create(path) {
            std::fs::create_dir_all(dir)?;
        }
        write_snapshot_atomically(path, self)
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        let mut f = io::BufReader::new(std::fs::File::open(path)?);
        let checksummed = read_magic(&mut f)?;
        let cursor = read_u64(&mut f)?;
        let count = read_u64(&mut f)? as usize;
        let (inner, digest) = read_rows(&mut f, count)?;
        if checksummed {
            verify_checksum(&mut f, cursor, count, digest, path)?;
        }
        Ok(Self { inner, cursor })
    }
}

/// Fold of every byte that matters, so a torn write is rejected rather than
/// loaded as a plausible map. FNV-1a: not cryptographic, and does not need to be
/// — this catches truncation and corruption, not forgery.
#[derive(Clone, Copy)]
struct Digest(u64);

impl Digest {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn eat(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x100_0000_01b3);
        }
    }

    fn seal(mut self, cursor: u64, count: usize) -> u64 {
        self.eat(&cursor.to_le_bytes());
        self.eat(&(count as u64).to_le_bytes());
        self.0
    }
}

fn verify_checksum<R: Read>(
    f: &mut R,
    cursor: u64,
    count: usize,
    digest: Digest,
    path: &Path,
) -> io::Result<()> {
    let mut footer = [0u8; 8];
    f.read_exact(&mut footer).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{path:?} has no checksum footer; the write was cut short ({e})"),
        )
    })?;
    let want = u64::from_le_bytes(footer);
    let got = digest.seal(cursor, count);
    if want != got {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{path:?} checksum {got:#018x} does not match {want:#018x}; file is corrupt"),
        ));
    }
    Ok(())
}

pub(super) fn parent_to_create(path: &Path) -> Option<&Path> {
    path.parent().filter(|d| !d.as_os_str().is_empty())
}

fn write_snapshot_atomically(path: &Path, map: &BalanceMap) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    write_snapshot_file(&tmp, map)?;
    std::fs::rename(&tmp, path)
}

fn write_snapshot_file(path: &Path, map: &BalanceMap) -> io::Result<()> {
    let mut f = io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(&SNAPSHOT_MAGIC)?;
    f.write_all(&map.cursor.to_le_bytes())?;
    f.write_all(&(map.inner.len() as u64).to_le_bytes())?;
    let digest = write_rows(&mut f, map)?;
    f.write_all(&digest.seal(map.cursor, map.inner.len()).to_le_bytes())?;
    f.into_inner()?.sync_all()
}

fn write_rows<W: Write>(f: &mut W, map: &BalanceMap) -> io::Result<Digest> {
    let mut digest = Digest::new();
    for (addr, e) in &map.inner {
        write_row(f, addr, e, &mut digest)?;
    }
    Ok(digest)
}

fn write_row<W: Write>(
    f: &mut W,
    addr: &Address,
    e: &Entry,
    digest: &mut Digest,
) -> io::Result<()> {
    let mut row = [0u8; ROW];
    row[..20].copy_from_slice(addr.as_slice());
    row[20..36].copy_from_slice(&e.usdt.to_le_bytes());
    row[36..40].copy_from_slice(&e.usdt_block.to_le_bytes());
    row[40..56].copy_from_slice(&e.usdc.to_le_bytes());
    row[56..60].copy_from_slice(&e.usdc_block.to_le_bytes());
    digest.eat(&row);
    f.write_all(&row)
}

/// Returns whether the file carries a checksum footer.
fn read_magic<R: Read>(f: &mut R) -> io::Result<bool> {
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic)?;
    match magic {
        SNAPSHOT_MAGIC => Ok(true),
        LEGACY_MAGIC => {
            tracing::warn!(
                "snapshot is the pre-checksum USDTPIR2 format; it will be rewritten \
                 as USDTPIR3 on the next save"
            );
            Ok(false)
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "snapshot magic {:?} is not {:?}; wrong file or older schema",
                String::from_utf8_lossy(&magic),
                String::from_utf8_lossy(&SNAPSHOT_MAGIC),
            ),
        )),
    }
}

fn read_u64<R: Read>(f: &mut R) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    f.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

fn read_rows<R: Read>(f: &mut R, count: usize) -> io::Result<(HashMap<Address, Entry>, Digest)> {
    // Grow into the claimed size instead of trusting it up front.
    let mut inner = HashMap::with_capacity(count.min(CAPACITY_GUARD));
    let mut digest = Digest::new();
    for _ in 0..count {
        let (addr, entry) = read_row(f, &mut digest)?;
        inner.insert(addr, entry);
    }
    Ok((inner, digest))
}

fn read_row<R: Read>(f: &mut R, digest: &mut Digest) -> io::Result<(Address, Entry)> {
    let mut row = [0u8; ROW];
    f.read_exact(&mut row)?;
    digest.eat(&row);
    Ok((Address::from_slice(&row[..20]), entry_from_row(&row)))
}

fn entry_from_row(row: &[u8; ROW]) -> Entry {
    Entry {
        usdt: u128::from_le_bytes(row[20..36].try_into().unwrap()),
        usdt_block: u32::from_le_bytes(row[36..40].try_into().unwrap()),
        usdc: u128::from_le_bytes(row[40..56].try_into().unwrap()),
        usdc_block: u32::from_le_bytes(row[56..60].try_into().unwrap()),
    }
}
