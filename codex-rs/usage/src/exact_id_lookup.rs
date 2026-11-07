//! Shared exact probes; payload owners retain their own admission and retention rules.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::Path;

use super::{IdRunDescriptor, candidate_offset, data_path, invalid, read_fences};
use crate::Result;
use crate::id_index_run::{ExactIdEntry, next_frame};
use crate::storage_file;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Duplicates { Earliest, Reject }

pub(crate) fn lookup<'a, T: ExactIdEntry>(
    root: &Path,
    runs: impl IntoIterator<Item = &'a IdRunDescriptor>,
    ids: &BTreeSet<String>,
    hash_id: impl Fn(&str) -> u64,
    duplicates: Duplicates,
    mut retain: impl FnMut(&T) -> Result<bool>,
) -> Result<BTreeMap<String, T>> {
    #[cfg(test)]
    let mut profile = crate::performance_tests::RecoveryProfile::new(T::LOOKUP_PROFILE_COMPONENT);
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut found = BTreeMap::<String, T>::new();
    let mut by_hash = BTreeMap::<u64, Vec<&str>>::new();
    for id in ids {
        by_hash.entry(hash_id(id)).or_default().push(id);
    }
    for run in runs {
        let bloom = crate::id_index_bloom::load(root, run);
        #[cfg(test)]
        profile.mark("bloom-read");
        let candidates = by_hash
            .keys()
            .copied()
            .filter(|hash| bloom.as_ref().is_none_or(|hint| hint.might_contain(*hash)))
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            continue;
        }
        let path = data_path(root, &run.data_file)?;
        let fence_path = data_path(root, &run.fence_file)?;
        storage_file::protect_existing(&path)?;
        storage_file::protect_existing(&fence_path)?;
        let mut data = BufReader::with_capacity(/*capacity*/ 64 * 1024, File::open(path)?);
        let fences = read_fences(&fence_path, run)?;
        #[cfg(test)]
        profile.mark("fence-read");
        let mut pages = BTreeMap::<u64, Vec<u64>>::new();
        for hash in candidates {
            if let Some(offset) = candidate_offset(&fences, hash) {
                pages.entry(offset).or_default().push(hash);
            }
        }
        for (offset, hashes) in pages {
            if offset < 8 || offset >= run.data_bytes {
                return Err(invalid("record-id fence points outside its run"));
            }
            data.seek(SeekFrom::Start(offset))?;
            let mut entry = next_frame::<T>(&mut data)?;
            for hash in hashes {
                while let Some(current) = entry.as_ref() {
                    let current_hash = hash_id(current.id());
                    if current_hash > hash {
                        break;
                    }
                    if current_hash == hash && by_hash[&hash].contains(&current.id()) && retain(current)? {
                        match found.get(current.id()) {
                            Some(_) if duplicates == Duplicates::Reject => {
                                return Err(invalid("conflicting current operation addresses"));
                            }
                            Some(previous) if previous.first_sequence() < current.first_sequence() => {}
                            Some(previous) if previous.first_sequence() == current.first_sequence() => {
                                if previous != current {
                                    return Err(invalid("conflicting exact-id facts at one sequence"));
                                }
                            }
                            _ => {
                                found.insert(current.id().to_string(), current.clone());
                            }
                        }
                    }
                    entry = next_frame::<T>(&mut data)?;
                }
                if entry.is_none() {
                    break;
                }
            }
        }
        #[cfg(test)]
        profile.mark("exact-page-probes");
    }
    Ok(found)
}
