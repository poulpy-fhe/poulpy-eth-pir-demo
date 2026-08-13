//! On-disk keyword state, so a restart does not move any address.
//!
//! The MPHF is not reproducible: rebuilding it over the same addresses yields a
//! different permutation. Without persisting it, every restart renumbers every
//! slot and every client has to refetch the directory. Two files, because the
//! two halves change at very different rates:
//!
//! | file | holds | rewritten |
//! | --- | --- | --- |
//! | `<base>.index` | MPHF + delta + version, the blob clients download | every publish |
//! | `<base>.keys` | slot -> address over the MPHF range, 20 B each | only on a full rebuild |
//!
//! `.keys` is what makes restore exact. A minimal perfect hash is *total* — it
//! answers for addresses it was never built over — so membership cannot be
//! recovered from the blob alone, and a guess would hand one address's slot to
//! another. Delta membership needs no table: only delta keys index past the MPHF.
//!
//! Save order matters. `.index` is written **before** the directory is exposed
//! to clients, so what is on disk is never behind what a client already holds.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use eth_pir::{Address, KeywordCheckpoint};

const INDEX_MAGIC: [u8; 8] = *b"USDTKWI1";
const KEYS_MAGIC: [u8; 8] = *b"USDTKWK1";

#[derive(Clone, Debug)]
pub struct Paths {
    pub index: PathBuf,
    pub keys: PathBuf,
}

impl Paths {
    pub fn new(base: &Path) -> Self {
        Self {
            index: base.with_extension("index"),
            keys: base.with_extension("keys"),
        }
    }
}

pub struct Loaded {
    pub directory: Vec<u8>,
    pub keys: Vec<Address>,
    pub version: u64,
}

/// Both halves, or `None` if either is missing.
///
/// A version mismatch means the `.keys` file belongs to an older MPHF than the
/// `.index`, which can only happen if a rebuild was interrupted between the two
/// writes. Rebuilding from scratch is the honest answer; pairing them would put
/// addresses at slots that are not theirs.
pub fn load(paths: &Paths) -> Result<Option<Loaded>> {
    let (Some(directory), Some((keys, keys_version))) =
        (read_index(&paths.index)?, read_keys(&paths.keys)?)
    else {
        return Ok(None);
    };

    let version = directory_version(&directory)?;
    if version != keys_version {
        tracing::warn!(
            index_version = version,
            keys_version,
            "keyword files disagree on the MPHF generation; rebuilding"
        );
        return Ok(None);
    }

    Ok(Some(Loaded {
        directory,
        keys,
        version,
    }))
}

/// Write the directory blob. Cheap, and safe to call on every publish.
pub fn save_index(paths: &Paths, directory: &[u8]) -> Result<()> {
    create_parent(&paths.index)?;
    atomically(&paths.index, |f| {
        f.write_all(&INDEX_MAGIC)?;
        f.write_all(&(directory.len() as u64).to_le_bytes())?;
        f.write_all(directory)
    })
    .with_context(|| format!("writing {:?}", paths.index))
}

/// Write both halves. Only needed when the MPHF itself changed.
///
/// `.keys` goes first: if this dies in between, the next start sees a `.keys`
/// newer than the `.index`, the versions disagree, and it rebuilds. The other
/// order could leave an `.index` whose slots no table explains.
pub fn save_checkpoint(paths: &Paths, checkpoint: &KeywordCheckpoint) -> Result<()> {
    create_parent(&paths.keys)?;
    atomically(&paths.keys, |f| {
        f.write_all(&KEYS_MAGIC)?;
        f.write_all(&checkpoint.version.to_le_bytes())?;
        f.write_all(&(checkpoint.keys.len() as u64).to_le_bytes())?;
        for key in &checkpoint.keys {
            f.write_all(key)?;
        }
        Ok(())
    })
    .with_context(|| format!("writing {:?}", paths.keys))?;

    save_index(paths, &checkpoint.directory)
}

fn read_index(path: &Path) -> Result<Option<Vec<u8>>> {
    let Some(mut f) = open(path)? else {
        return Ok(None);
    };
    expect_magic(&mut f, &INDEX_MAGIC, path)?;
    let len = read_u64(&mut f)? as usize;
    let mut blob = vec![0u8; len];
    f.read_exact(&mut blob)
        .with_context(|| format!("{path:?} is truncated"))?;
    Ok(Some(blob))
}

fn read_keys(path: &Path) -> Result<Option<(Vec<Address>, u64)>> {
    let Some(mut f) = open(path)? else {
        return Ok(None);
    };
    expect_magic(&mut f, &KEYS_MAGIC, path)?;
    let version = read_u64(&mut f)?;
    let count = read_u64(&mut f)? as usize;
    let mut keys = vec![[0u8; 20]; count];
    for key in &mut keys {
        f.read_exact(key)
            .with_context(|| format!("{path:?} is truncated"))?;
    }
    Ok(Some((keys, version)))
}

/// The version sits inside the blob; the client wire format owns that layout, so
/// parse it rather than duplicating the offset here.
fn directory_version(blob: &[u8]) -> Result<u64> {
    Ok(
        poulpy_pir::keyword::KeywordDirectory::<20>::read_from(&mut { blob })
            .context("parsing the saved keyword directory")?
            .version(),
    )
}

fn open(path: &Path) -> Result<Option<io::BufReader<std::fs::File>>> {
    match std::fs::File::open(path) {
        Ok(f) => Ok(Some(io::BufReader::new(f))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {path:?}")),
    }
}

fn expect_magic<R: Read>(f: &mut R, want: &[u8; 8], path: &Path) -> Result<()> {
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic)
        .with_context(|| format!("{path:?} is empty or truncated"))?;
    anyhow::ensure!(
        &magic == want,
        "{path:?} magic {:?} is not {:?}; wrong file or older schema",
        String::from_utf8_lossy(&magic),
        String::from_utf8_lossy(want),
    );
    Ok(())
}

fn read_u64<R: Read>(f: &mut R) -> Result<u64> {
    let mut buf = [0u8; 8];
    f.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

fn create_parent(path: &Path) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    Ok(())
}

fn atomically<F>(path: &Path, write: F) -> io::Result<()>
where
    F: FnOnce(&mut io::BufWriter<std::fs::File>) -> io::Result<()>,
{
    let tmp = path.with_extension("tmp");
    let mut f = io::BufWriter::new(std::fs::File::create(&tmp)?);
    write(&mut f)?;
    f.into_inner()?.sync_all()?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests;
