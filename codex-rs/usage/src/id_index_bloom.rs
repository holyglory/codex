//! Optional, checksummed routing hints. A hint can only avoid an exact probe;
//! unsupported or damaged hints never make canonical data unavailable.
use std::collections::VecDeque;
use std::fs::File;
use std::fs::Metadata;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::SystemTime;

use crate::Result;
use crate::id_index::ID_HASH_VERSION;
use crate::id_index::IdRunDescriptor;
use crate::id_index::data_path;
use crate::id_index::id_hash;
use crate::storage_file;

const MAGIC: &[u8; 8] = b"CIDX1BLM";
const VERSION: u32 = 1;
const HEADER: usize = 40;
const MIN_BYTES: usize = 64;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const CACHE_BYTES: usize = 32 * 1024 * 1024;
const CACHE_ENTRIES: usize = 64;
const BITS_PER_ENTRY: u64 = 12;
const PROBES: u64 = 8;
static CACHE: Mutex<VecDeque<Cached>> = Mutex::new(VecDeque::new());

pub(crate) struct Bloom {
    bits: Vec<u8>,
}

impl Bloom {
    pub(crate) fn new(estimated_entries: u64) -> Option<Self> {
        let bytes = estimated_entries
            .saturating_mul(BITS_PER_ENTRY)
            .div_ceil(8)
            .clamp(MIN_BYTES as u64, MAX_BYTES as u64) as usize;
        let bytes = bytes.next_power_of_two();
        let mut bits = Vec::new();
        bits.try_reserve_exact(bytes).ok()?;
        bits.resize(bytes, 0);
        Some(Self { bits })
    }

    pub(crate) fn insert(&mut self, hash: u64) {
        for bit in positions(hash, self.bits.len()) {
            self.bits[bit / 8] |= 1 << (bit % 8);
        }
    }

    pub(crate) fn might_contain(&self, hash: u64) -> bool {
        positions(hash, self.bits.len()).all(|bit| self.bits[bit / 8] & (1 << (bit % 8)) != 0)
    }

    pub(crate) fn write(self, root: &Path, data_file: &str, entries: u64) -> Result<String> {
        let mut header = Vec::with_capacity(HEADER);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&VERSION.to_le_bytes());
        header.extend_from_slice(&ID_HASH_VERSION.to_le_bytes());
        header.extend_from_slice(&entries.to_le_bytes());
        header.extend_from_slice(&id_hash(data_file).to_le_bytes());
        header.extend_from_slice(&(self.bits.len() as u64).to_le_bytes());
        let digest = checksum(header.iter().chain(&self.bits).copied());
        let name = format!("{data_file}.bloom-{digest:016x}.bin");
        let path = data_path(root, &name)?;
        let mut file = storage_file::create_new(&path)?;
        file.write_all(&header)?;
        file.write_all(&self.bits)?;
        file.sync_all()?;
        storage_file::finish_generated(&path)?;
        Ok(name)
    }
}

#[derive(Eq, PartialEq)]
struct Identity {
    bytes: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    #[cfg(unix)]
    inode: (u64, u64),
}

impl Identity {
    fn of(metadata: &Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            bytes: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            #[cfg(unix)]
            inode: (metadata.dev(), metadata.ino()),
        }
    }
}

struct Cached {
    path: PathBuf,
    identity: Identity,
    entries: u64,
    value: Arc<Bloom>,
}

/// A failed validation means "probe the exact run", never "id absent".
pub(crate) fn load(root: &Path, run: &IdRunDescriptor) -> Option<Arc<Bloom>> {
    let name = run.bloom_file.as_deref()?;
    let digest = name
        .strip_prefix(&format!("{data_file}.bloom-", data_file = &run.data_file))?
        .strip_suffix(".bin")?;
    if digest.len() != 16 {
        return None;
    }
    let digest = u64::from_str_radix(digest, 16).ok()?;
    let path = data_path(root, name).ok()?;
    storage_file::protect_existing(&path).ok()?;
    let identity = Identity::of(&std::fs::metadata(&path).ok()?);
    if let Ok(mut cache) = CACHE.lock()
        && let Some(index) = cache.iter().position(|entry| {
            entry.path == path && entry.identity == identity && entry.entries == run.entry_count
        })
        && let Some(entry) = cache.remove(index)
    {
        let value = Arc::clone(&entry.value);
        cache.push_back(entry);
        return Some(value);
    }
    let length = usize::try_from(identity.bytes).ok()?;
    if !(HEADER + MIN_BYTES..=HEADER + MAX_BYTES).contains(&length) {
        return None;
    }
    let mut file = File::open(&path).ok()?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length).ok()?;
    bytes.resize(length, 0);
    file.read_exact(&mut bytes).ok()?;
    if file.read(&mut [0]).ok()? != 0
        || Identity::of(&file.metadata().ok()?) != identity
        || checksum(bytes.iter().copied()) != digest
        || &bytes[..8] != MAGIC
        || u32::from_le_bytes(bytes[8..12].try_into().ok()?) != VERSION
        || u32::from_le_bytes(bytes[12..16].try_into().ok()?) != ID_HASH_VERSION
        || integer(&bytes, /*offset*/ 16)? != run.entry_count
        || integer(&bytes, /*offset*/ 24)? != id_hash(&run.data_file)
        || integer(&bytes, /*offset*/ 32)? != (length - HEADER) as u64
        || !(length - HEADER).is_power_of_two()
    {
        return None;
    }
    bytes.copy_within(HEADER.., 0);
    bytes.truncate(length - HEADER);
    let value = Arc::new(Bloom { bits: bytes });
    if let Ok(mut cache) = CACHE.lock() {
        cache.retain(|entry| entry.path != path);
        cache.push_back(Cached {
            path,
            identity,
            entries: run.entry_count,
            value: Arc::clone(&value),
        });
        while cache.len() > CACHE_ENTRIES
            || cache
                .iter()
                .map(|entry| entry.value.bits.capacity())
                .sum::<usize>()
                > CACHE_BYTES
        {
            cache.pop_front();
        }
    }
    Some(value)
}

fn integer(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

fn checksum(bytes: impl Iterator<Item = u8>) -> u64 {
    bytes.fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

fn mix(mut value: u64) -> u64 {
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

fn positions(hash: u64, bytes: usize) -> impl Iterator<Item = usize> {
    let first = mix(hash);
    let step = mix(hash ^ 0x9e3779b97f4a7c15) | 1;
    let mask = (bytes * 8 - 1) as u64;
    (0..PROBES).map(move |index| (first.wrapping_add(index.wrapping_mul(step)) & mask) as usize)
}
