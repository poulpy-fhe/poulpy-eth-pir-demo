use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

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
        load_snapshot(path, false)
    }

    /// Load serving/bootstrap state. Unlike the compatibility loader, this
    /// requires USDTPIR3 and validates the complete framing and row set.
    pub fn load_strict(path: &Path) -> io::Result<Self> {
        load_snapshot(path, true)
    }
}

fn load_snapshot(path: &Path, strict: bool) -> io::Result<BalanceMap> {
    let mut f = io::BufReader::new(std::fs::File::open(path)?);
    let checksummed = read_magic(&mut f)?;
    if strict && !checksummed {
        return Err(invalid_data(format!(
            "{path:?} is checksumless USDTPIR2; authoritative state must be USDTPIR3"
        )));
    }
    let cursor = read_u64(&mut f)?;
    let count = usize::try_from(read_u64(&mut f)?)
        .map_err(|_| invalid_data(format!("{path:?} row count does not fit usize")))?;
    let (inner, digest) = read_rows(&mut f, count, strict)?;
    if checksummed {
        verify_checksum(&mut f, cursor, count, digest, path)?;
    }
    if strict {
        ensure_eof(&mut f, path)?;
    }
    Ok(BalanceMap { inner, cursor })
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

fn ensure_eof<R: Read>(f: &mut R, path: &Path) -> io::Result<()> {
    let mut trailing = [0u8; 1];
    if f.read(&mut trailing)? != 0 {
        return Err(invalid_data(format!(
            "{path:?} contains trailing bytes after its checksum"
        )));
    }
    Ok(())
}

pub(super) fn parent_to_create(path: &Path) -> Option<&Path> {
    path.parent().filter(|d| !d.as_os_str().is_empty())
}

fn write_snapshot_atomically(path: &Path, map: &BalanceMap) -> io::Result<()> {
    let (tmp, file) = create_unique_temp(path)?;
    let result = (|| {
        write_snapshot_file(file, map)?;
        std::fs::rename(&tmp, path)?;
        fsync_parent(path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(super) fn create_unique_temp(path: &Path) -> io::Result<(std::path::PathBuf, std::fs::File)> {
    for _ in 0..128 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".tmp.{}.{}", std::process::id(), sequence));
        let tmp = path.with_file_name(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("could not reserve a unique temporary file beside {path:?}"),
    ))
}

fn write_snapshot_file(file: std::fs::File, map: &BalanceMap) -> io::Result<()> {
    let mut f = io::BufWriter::new(file);
    f.write_all(&SNAPSHOT_MAGIC)?;
    f.write_all(&map.cursor.to_le_bytes())?;
    f.write_all(&(map.inner.len() as u64).to_le_bytes())?;
    let digest = write_rows(&mut f, map)?;
    f.write_all(&digest.seal(map.cursor, map.inner.len()).to_le_bytes())?;
    f.into_inner()?.sync_all()
}

/// Complete the rename durability boundary. Opening a directory is supported
/// on the Unix platforms on which the advisory snapshot lock is used.
pub(crate) fn fsync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::File::open(parent)?.sync_all()
}

pub(crate) fn remove_file_durable(path: &Path) -> io::Result<()> {
    std::fs::remove_file(path)?;
    fsync_parent(path)
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

fn read_rows<R: Read>(
    f: &mut R,
    count: usize,
    strict: bool,
) -> io::Result<(HashMap<Address, Entry>, Digest)> {
    // Grow into the claimed size instead of trusting it up front.
    let mut inner = HashMap::with_capacity(count.min(CAPACITY_GUARD));
    let mut digest = Digest::new();
    for _ in 0..count {
        let (addr, entry) = read_row(f, &mut digest)?;
        if strict && addr.is_zero() {
            return Err(invalid_data("snapshot contains the zero address"));
        }
        if strict && entry.is_zero() {
            return Err(invalid_data(format!(
                "snapshot contains zero/zero row for {addr}"
            )));
        }
        if inner.insert(addr, entry).is_some() && strict {
            return Err(invalid_data(format!(
                "snapshot contains duplicate row for {addr}"
            )));
        }
    }
    Ok((inner, digest))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
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
