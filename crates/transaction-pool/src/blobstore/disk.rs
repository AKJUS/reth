//! A simple diskstore for blobs

use crate::blobstore::{BlobStore, BlobStoreCleanupStat, BlobStoreError, BlobStoreSize};
use alloy_eips::{
    eip4844::{BlobAndProofV1, BlobAndProofV2},
    eip7594::BlobTransactionSidecarVariant,
};
use alloy_primitives::{TxHash, B256};
use parking_lot::{Mutex, RwLock};
use schnellru::{ByLength, LruMap};
use std::{collections::HashSet, fmt, fs, io, path::PathBuf, sync::Arc};
use tracing::{debug, trace};

/// How many [`BlobTransactionSidecarVariant`] to cache in memory.
pub const DEFAULT_MAX_CACHED_BLOBS: u32 = 100;

/// A blob store that stores blob data on disk.
///
/// The type uses deferred deletion, meaning that blobs are not immediately deleted from disk, but
/// it's expected that the maintenance task will call [`BlobStore::cleanup`] to remove the deleted
/// blobs from disk.
#[derive(Clone, Debug)]
pub struct DiskFileBlobStore {
    inner: Arc<DiskFileBlobStoreInner>,
}

impl DiskFileBlobStore {
    /// Opens and initializes a new disk file blob store according to the given options.
    pub fn open(
        blob_dir: impl Into<PathBuf>,
        opts: DiskFileBlobStoreConfig,
    ) -> Result<Self, DiskFileBlobStoreError> {
        let blob_dir = blob_dir.into();
        let DiskFileBlobStoreConfig { max_cached_entries, .. } = opts;
        let inner = DiskFileBlobStoreInner::new(blob_dir, max_cached_entries);

        // initialize the blob store
        inner.delete_all()?;
        inner.create_blob_dir()?;

        Ok(Self { inner: Arc::new(inner) })
    }

    #[cfg(test)]
    fn is_cached(&self, tx: &B256) -> bool {
        self.inner.blob_cache.lock().get(tx).is_some()
    }

    #[cfg(test)]
    fn clear_cache(&self) {
        self.inner.blob_cache.lock().clear()
    }
}

impl BlobStore for DiskFileBlobStore {
    fn insert(&self, tx: B256, data: BlobTransactionSidecarVariant) -> Result<(), BlobStoreError> {
        self.inner.insert_one(tx, data)
    }

    fn insert_all(
        &self,
        txs: Vec<(B256, BlobTransactionSidecarVariant)>,
    ) -> Result<(), BlobStoreError> {
        if txs.is_empty() {
            return Ok(())
        }
        self.inner.insert_many(txs)
    }

    fn delete(&self, tx: B256) -> Result<(), BlobStoreError> {
        if self.inner.contains(tx)? {
            self.inner.txs_to_delete.write().insert(tx);
        }
        Ok(())
    }

    fn delete_all(&self, txs: Vec<B256>) -> Result<(), BlobStoreError> {
        let txs = self.inner.retain_existing(txs)?;
        self.inner.txs_to_delete.write().extend(txs);
        Ok(())
    }

    fn cleanup(&self) -> BlobStoreCleanupStat {
        let txs_to_delete = std::mem::take(&mut *self.inner.txs_to_delete.write());
        let mut stat = BlobStoreCleanupStat::default();
        let mut subsize = 0;
        debug!(target:"txpool::blob", num_blobs=%txs_to_delete.len(), "Removing blobs from disk");
        for tx in txs_to_delete {
            let path = self.inner.blob_disk_file(tx);
            let filesize = fs::metadata(&path).map_or(0, |meta| meta.len());
            match fs::remove_file(&path) {
                Ok(_) => {
                    stat.delete_succeed += 1;
                    subsize += filesize;
                }
                Err(e) => {
                    stat.delete_failed += 1;
                    let err = DiskFileBlobStoreError::DeleteFile(tx, path, e);
                    debug!(target:"txpool::blob", %err);
                }
            };
        }
        self.inner.size_tracker.sub_size(subsize as usize);
        self.inner.size_tracker.sub_len(stat.delete_succeed);
        stat
    }

    fn get(&self, tx: B256) -> Result<Option<Arc<BlobTransactionSidecarVariant>>, BlobStoreError> {
        self.inner.get_one(tx)
    }

    fn contains(&self, tx: B256) -> Result<bool, BlobStoreError> {
        self.inner.contains(tx)
    }

    fn get_all(
        &self,
        txs: Vec<B256>,
    ) -> Result<Vec<(B256, Arc<BlobTransactionSidecarVariant>)>, BlobStoreError> {
        if txs.is_empty() {
            return Ok(Vec::new())
        }
        self.inner.get_all(txs)
    }

    fn get_exact(
        &self,
        txs: Vec<B256>,
    ) -> Result<Vec<Arc<BlobTransactionSidecarVariant>>, BlobStoreError> {
        if txs.is_empty() {
            return Ok(Vec::new())
        }
        self.inner.get_exact(txs)
    }

    fn get_by_versioned_hashes_v1(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<Option<BlobAndProofV1>>, BlobStoreError> {
        // the response must always be the same len as the request, misses must be None
        let mut result = vec![None; versioned_hashes.len()];

        // first scan all cached full sidecars
        for (_tx_hash, blob_sidecar) in self.inner.blob_cache.lock().iter() {
            if let Some(blob_sidecar) = blob_sidecar.as_eip4844() {
                for (hash_idx, match_result) in
                    blob_sidecar.match_versioned_hashes(versioned_hashes)
                {
                    result[hash_idx] = Some(match_result);
                }
            }

            // return early if all blobs are found.
            if result.iter().all(|blob| blob.is_some()) {
                return Ok(result);
            }
        }

        // not all versioned hashes were be found, try to look up a matching tx

        let mut missing_tx_hashes = Vec::new();

        {
            let mut versioned_to_txhashes = self.inner.versioned_hashes_to_txhash.lock();
            for (idx, _) in
                result.iter().enumerate().filter(|(_, blob_and_proof)| blob_and_proof.is_none())
            {
                // this is safe because the result vec has the same len
                let versioned_hash = versioned_hashes[idx];
                if let Some(tx_hash) = versioned_to_txhashes.get(&versioned_hash).copied() {
                    missing_tx_hashes.push(tx_hash);
                }
            }
        }

        // if we have missing blobs, try to read them from disk and try again
        if !missing_tx_hashes.is_empty() {
            let blobs_from_disk = self.inner.read_many_decoded(missing_tx_hashes);
            for (_, blob_sidecar) in blobs_from_disk {
                if let Some(blob_sidecar) = blob_sidecar.as_eip4844() {
                    for (hash_idx, match_result) in
                        blob_sidecar.match_versioned_hashes(versioned_hashes)
                    {
                        if result[hash_idx].is_none() {
                            result[hash_idx] = Some(match_result);
                        }
                    }
                }
            }
        }

        Ok(result)
    }

    fn get_by_versioned_hashes_v2(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Option<Vec<BlobAndProofV2>>, BlobStoreError> {
        // we must return the blobs in order but we don't necessarily find them in the requested
        // order
        let mut result = vec![None; versioned_hashes.len()];

        // first scan all cached full sidecars
        for (_tx_hash, blob_sidecar) in self.inner.blob_cache.lock().iter() {
            if let Some(blob_sidecar) = blob_sidecar.as_eip7594() {
                for (hash_idx, match_result) in
                    blob_sidecar.match_versioned_hashes(versioned_hashes)
                {
                    result[hash_idx] = Some(match_result);
                }
            }

            // return early if all blobs are found.
            if result.iter().all(|blob| blob.is_some()) {
                // got all blobs, can return early
                return Ok(Some(result.into_iter().map(Option::unwrap).collect()))
            }
        }

        // not all versioned hashes were found, try to look up a matching tx
        let mut missing_tx_hashes = Vec::new();

        {
            let mut versioned_to_txhashes = self.inner.versioned_hashes_to_txhash.lock();
            for (idx, _) in
                result.iter().enumerate().filter(|(_, blob_and_proof)| blob_and_proof.is_none())
            {
                // this is safe because the result vec has the same len
                let versioned_hash = versioned_hashes[idx];
                if let Some(tx_hash) = versioned_to_txhashes.get(&versioned_hash).copied() {
                    missing_tx_hashes.push(tx_hash);
                }
            }
        }

        // if we have missing blobs, try to read them from disk and try again
        if !missing_tx_hashes.is_empty() {
            let blobs_from_disk = self.inner.read_many_decoded(missing_tx_hashes);
            for (_, blob_sidecar) in blobs_from_disk {
                if let Some(blob_sidecar) = blob_sidecar.as_eip7594() {
                    for (hash_idx, match_result) in
                        blob_sidecar.match_versioned_hashes(versioned_hashes)
                    {
                        if result[hash_idx].is_none() {
                            result[hash_idx] = Some(match_result);
                        }
                    }
                }
            }
        }

        // only return the blobs if we found all requested versioned hashes
        if result.iter().all(|blob| blob.is_some()) {
            Ok(Some(result.into_iter().map(Option::unwrap).collect()))
        } else {
            Ok(None)
        }
    }

    fn data_size_hint(&self) -> Option<usize> {
        Some(self.inner.size_tracker.data_size())
    }

    fn blobs_len(&self) -> usize {
        self.inner.size_tracker.blobs_len()
    }
}

struct DiskFileBlobStoreInner {
    blob_dir: PathBuf,
    blob_cache: Mutex<LruMap<TxHash, Arc<BlobTransactionSidecarVariant>, ByLength>>,
    size_tracker: BlobStoreSize,
    file_lock: RwLock<()>,
    txs_to_delete: RwLock<HashSet<B256>>,
    /// Tracks of known versioned hashes and a transaction they exist in
    ///
    /// Note: It is possible that one blob can appear in multiple transactions but this only tracks
    /// the most recent one.
    versioned_hashes_to_txhash: Mutex<LruMap<B256, B256>>,
}

impl DiskFileBlobStoreInner {
    /// Creates a new empty disk file blob store with the given maximum length of the blob cache.
    fn new(blob_dir: PathBuf, max_length: u32) -> Self {
        Self {
            blob_dir,
            blob_cache: Mutex::new(LruMap::new(ByLength::new(max_length))),
            size_tracker: Default::default(),
            file_lock: Default::default(),
            txs_to_delete: Default::default(),
            versioned_hashes_to_txhash: Mutex::new(LruMap::new(ByLength::new(max_length * 6))),
        }
    }

    /// Creates the directory where blobs will be stored on disk.
    fn create_blob_dir(&self) -> Result<(), DiskFileBlobStoreError> {
        debug!(target:"txpool::blob", blob_dir = ?self.blob_dir, "Creating blob store");
        fs::create_dir_all(&self.blob_dir)
            .map_err(|e| DiskFileBlobStoreError::Open(self.blob_dir.clone(), e))
    }

    /// Deletes the entire blob store.
    fn delete_all(&self) -> Result<(), DiskFileBlobStoreError> {
        match fs::remove_dir_all(&self.blob_dir) {
            Ok(_) => {
                debug!(target:"txpool::blob", blob_dir = ?self.blob_dir, "Removed blob store directory");
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(DiskFileBlobStoreError::Open(self.blob_dir.clone(), err)),
        }
        Ok(())
    }

    /// Ensures blob is in the blob cache and written to the disk.
    fn insert_one(
        &self,
        tx: B256,
        data: BlobTransactionSidecarVariant,
    ) -> Result<(), BlobStoreError> {
        let mut buf = Vec::with_capacity(data.rlp_encoded_fields_length());
        data.rlp_encode_fields(&mut buf);

        {
            // cache the versioned hashes to tx hash
            let mut map = self.versioned_hashes_to_txhash.lock();
            data.versioned_hashes().for_each(|hash| {
                map.insert(hash, tx);
            });
        }

        self.blob_cache.lock().insert(tx, Arc::new(data));

        let size = self.write_one_encoded(tx, &buf)?;

        self.size_tracker.add_size(size);
        self.size_tracker.inc_len(1);
        Ok(())
    }

    /// Ensures blobs are in the blob cache and written to the disk.
    fn insert_many(
        &self,
        txs: Vec<(B256, BlobTransactionSidecarVariant)>,
    ) -> Result<(), BlobStoreError> {
        let raw = txs
            .iter()
            .map(|(tx, data)| {
                let mut buf = Vec::with_capacity(data.rlp_encoded_fields_length());
                data.rlp_encode_fields(&mut buf);
                (self.blob_disk_file(*tx), buf)
            })
            .collect::<Vec<_>>();

        {
            // cache versioned hashes to tx hash
            let mut map = self.versioned_hashes_to_txhash.lock();
            for (tx, data) in &txs {
                data.versioned_hashes().for_each(|hash| {
                    map.insert(hash, *tx);
                });
            }
        }

        {
            // cache blobs
            let mut cache = self.blob_cache.lock();
            for (tx, data) in txs {
                cache.insert(tx, Arc::new(data));
            }
        }

        let mut add = 0;
        let mut num = 0;
        {
            let _lock = self.file_lock.write();
            for (path, data) in raw {
                if path.exists() {
                    debug!(target:"txpool::blob", ?path, "Blob already exists");
                } else if let Err(err) = fs::write(&path, &data) {
                    debug!(target:"txpool::blob", %err, ?path, "Failed to write blob file");
                } else {
                    add += data.len();
                    num += 1;
                }
            }
        }
        self.size_tracker.add_size(add);
        self.size_tracker.inc_len(num);

        Ok(())
    }

    /// Returns true if the blob for the given transaction hash is in the blob cache or on disk.
    fn contains(&self, tx: B256) -> Result<bool, BlobStoreError> {
        if self.blob_cache.lock().get(&tx).is_some() {
            return Ok(true)
        }
        // we only check if the file exists and assume it's valid
        Ok(self.blob_disk_file(tx).is_file())
    }

    /// Returns all the blob transactions which are in the cache or on the disk.
    fn retain_existing(&self, txs: Vec<B256>) -> Result<Vec<B256>, BlobStoreError> {
        let (in_cache, not_in_cache): (Vec<B256>, Vec<B256>) = {
            let mut cache = self.blob_cache.lock();
            txs.into_iter().partition(|tx| cache.get(tx).is_some())
        };

        let mut existing = in_cache;
        for tx in not_in_cache {
            if self.blob_disk_file(tx).is_file() {
                existing.push(tx);
            }
        }

        Ok(existing)
    }

    /// Retrieves the blob for the given transaction hash from the blob cache or disk.
    fn get_one(
        &self,
        tx: B256,
    ) -> Result<Option<Arc<BlobTransactionSidecarVariant>>, BlobStoreError> {
        if let Some(blob) = self.blob_cache.lock().get(&tx) {
            return Ok(Some(blob.clone()))
        }

        if let Some(blob) = self.read_one(tx)? {
            let blob_arc = Arc::new(blob);
            self.blob_cache.lock().insert(tx, blob_arc.clone());
            return Ok(Some(blob_arc))
        }

        Ok(None)
    }

    /// Returns the path to the blob file for the given transaction hash.
    #[inline]
    fn blob_disk_file(&self, tx: B256) -> PathBuf {
        self.blob_dir.join(format!("{tx:x}"))
    }

    /// Retrieves the blob data for the given transaction hash.
    #[inline]
    fn read_one(&self, tx: B256) -> Result<Option<BlobTransactionSidecarVariant>, BlobStoreError> {
        let path = self.blob_disk_file(tx);
        let data = {
            let _lock = self.file_lock.read();
            match fs::read(&path) {
                Ok(data) => data,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) => {
                    return Err(BlobStoreError::Other(Box::new(DiskFileBlobStoreError::ReadFile(
                        tx, path, e,
                    ))))
                }
            }
        };
        BlobTransactionSidecarVariant::rlp_decode_fields(&mut data.as_slice())
            .map(Some)
            .map_err(BlobStoreError::DecodeError)
    }

    /// Returns decoded blobs read from disk.
    ///
    /// Only returns sidecars that were found and successfully decoded.
    fn read_many_decoded(&self, txs: Vec<TxHash>) -> Vec<(TxHash, BlobTransactionSidecarVariant)> {
        self.read_many_raw(txs)
            .into_iter()
            .filter_map(|(tx, data)| {
                BlobTransactionSidecarVariant::rlp_decode_fields(&mut data.as_slice())
                    .map(|sidecar| (tx, sidecar))
                    .ok()
            })
            .collect()
    }

    /// Retrieves the raw blob data for the given transaction hashes.
    ///
    /// Only returns the blobs that were found on file.
    #[inline]
    fn read_many_raw(&self, txs: Vec<TxHash>) -> Vec<(TxHash, Vec<u8>)> {
        let mut res = Vec::with_capacity(txs.len());
        let _lock = self.file_lock.read();
        for tx in txs {
            let path = self.blob_disk_file(tx);
            match fs::read(&path) {
                Ok(data) => {
                    res.push((tx, data));
                }
                Err(err) => {
                    debug!(target:"txpool::blob", %err, ?tx, "Failed to read blob file");
                }
            };
        }
        res
    }

    /// Writes the blob data for the given transaction hash to the disk.
    #[inline]
    fn write_one_encoded(&self, tx: B256, data: &[u8]) -> Result<usize, DiskFileBlobStoreError> {
        trace!(target:"txpool::blob", "[{:?}] writing blob file", tx);
        let mut add = 0;
        let path = self.blob_disk_file(tx);
        {
            let _lock = self.file_lock.write();
            if !path.exists() {
                fs::write(&path, data)
                    .map_err(|e| DiskFileBlobStoreError::WriteFile(tx, path, e))?;
                add = data.len();
            }
        }
        Ok(add)
    }

    /// Retrieves blobs for the given transaction hashes from the blob cache or disk.
    ///
    /// This will not return an error if there are missing blobs. Therefore, the result may be a
    /// subset of the request or an empty vector if none of the blobs were found.
    #[inline]
    fn get_all(
        &self,
        txs: Vec<B256>,
    ) -> Result<Vec<(B256, Arc<BlobTransactionSidecarVariant>)>, BlobStoreError> {
        let mut res = Vec::with_capacity(txs.len());
        let mut cache_miss = Vec::new();
        {
            let mut cache = self.blob_cache.lock();
            for tx in txs {
                if let Some(blob) = cache.get(&tx) {
                    res.push((tx, blob.clone()));
                } else {
                    cache_miss.push(tx)
                }
            }
        }
        if cache_miss.is_empty() {
            return Ok(res)
        }
        let from_disk = self.read_many_decoded(cache_miss);
        if from_disk.is_empty() {
            return Ok(res)
        }
        let from_disk = from_disk
            .into_iter()
            .map(|(tx, data)| {
                let data = Arc::new(data);
                res.push((tx, data.clone()));
                (tx, data)
            })
            .collect::<Vec<_>>();

        let mut cache = self.blob_cache.lock();
        for (tx, data) in from_disk {
            cache.insert(tx, data);
        }

        Ok(res)
    }

    /// Retrieves blobs for the given transaction hashes from the blob cache or disk.
    ///
    /// Returns an error if there are any missing blobs.
    #[inline]
    fn get_exact(
        &self,
        txs: Vec<B256>,
    ) -> Result<Vec<Arc<BlobTransactionSidecarVariant>>, BlobStoreError> {
        txs.into_iter()
            .map(|tx| self.get_one(tx)?.ok_or(BlobStoreError::MissingSidecar(tx)))
            .collect()
    }
}

impl fmt::Debug for DiskFileBlobStoreInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiskFileBlobStoreInner")
            .field("blob_dir", &self.blob_dir)
            .field("cached_blobs", &self.blob_cache.try_lock().map(|lock| lock.len()))
            .field("txs_to_delete", &self.txs_to_delete.try_read())
            .finish()
    }
}

/// Errors that can occur when interacting with a disk file blob store.
#[derive(Debug, thiserror::Error)]
pub enum DiskFileBlobStoreError {
    /// Thrown during [`DiskFileBlobStore::open`] if the blob store directory cannot be opened.
    #[error("failed to open blobstore at {0}: {1}")]
    /// Indicates a failure to open the blob store directory.
    Open(PathBuf, io::Error),
    /// Failure while reading a blob file.
    #[error("[{0}] failed to read blob file at {1}: {2}")]
    /// Indicates a failure while reading a blob file.
    ReadFile(TxHash, PathBuf, io::Error),
    /// Failure while writing a blob file.
    #[error("[{0}] failed to write blob file at {1}: {2}")]
    /// Indicates a failure while writing a blob file.
    WriteFile(TxHash, PathBuf, io::Error),
    /// Failure while deleting a blob file.
    #[error("[{0}] failed to delete blob file at {1}: {2}")]
    /// Indicates a failure while deleting a blob file.
    DeleteFile(TxHash, PathBuf, io::Error),
}

impl From<DiskFileBlobStoreError> for BlobStoreError {
    fn from(value: DiskFileBlobStoreError) -> Self {
        Self::Other(Box::new(value))
    }
}

/// Configuration for a disk file blob store.
#[derive(Debug, Clone)]
pub struct DiskFileBlobStoreConfig {
    /// The maximum number of blobs to keep in the in memory blob cache.
    pub max_cached_entries: u32,
    /// How to open the blob store.
    pub open: OpenDiskFileBlobStore,
}

impl Default for DiskFileBlobStoreConfig {
    fn default() -> Self {
        Self { max_cached_entries: DEFAULT_MAX_CACHED_BLOBS, open: Default::default() }
    }
}

impl DiskFileBlobStoreConfig {
    /// Set maximum number of blobs to keep in the in memory blob cache.
    pub const fn with_max_cached_entries(mut self, max_cached_entries: u32) -> Self {
        self.max_cached_entries = max_cached_entries;
        self
    }
}

/// How to open a disk file blob store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OpenDiskFileBlobStore {
    /// Clear everything in the blob store.
    #[default]
    Clear,
    /// Keep the existing blob store and index
    ReIndex,
}

#[cfg(test)]
mod tests {
    use alloy_consensus::BlobTransactionSidecar;
    use alloy_eips::eip7594::BlobTransactionSidecarVariant;

    use super::*;
    use std::sync::atomic::Ordering;

    fn tmp_store() -> (DiskFileBlobStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = DiskFileBlobStore::open(dir.path(), Default::default()).unwrap();
        (store, dir)
    }

    fn rng_blobs(num: usize) -> Vec<(TxHash, BlobTransactionSidecarVariant)> {
        let mut rng = rand::rng();
        (0..num)
            .map(|_| {
                let tx = TxHash::random_with(&mut rng);
                let blob = BlobTransactionSidecarVariant::Eip4844(BlobTransactionSidecar {
                    blobs: vec![],
                    commitments: vec![],
                    proofs: vec![],
                });
                (tx, blob)
            })
            .collect()
    }

    #[test]
    fn disk_insert_all_get_all() {
        let (store, _dir) = tmp_store();

        let blobs = rng_blobs(10);
        let all_hashes = blobs.iter().map(|(tx, _)| *tx).collect::<Vec<_>>();
        store.insert_all(blobs.clone()).unwrap();

        // all cached
        for (tx, blob) in &blobs {
            assert!(store.is_cached(tx));
            let b = store.get(*tx).unwrap().map(Arc::unwrap_or_clone).unwrap();
            assert_eq!(b, *blob);
        }

        let all = store.get_all(all_hashes.clone()).unwrap();
        for (tx, blob) in all {
            assert!(blobs.contains(&(tx, Arc::unwrap_or_clone(blob))), "missing blob {tx:?}");
        }

        assert!(store.contains(all_hashes[0]).unwrap());
        store.delete_all(all_hashes.clone()).unwrap();
        assert!(store.inner.txs_to_delete.read().contains(&all_hashes[0]));
        store.clear_cache();
        store.cleanup();

        assert!(store.get(blobs[0].0).unwrap().is_none());

        let all = store.get_all(all_hashes.clone()).unwrap();
        assert!(all.is_empty());

        assert!(!store.contains(all_hashes[0]).unwrap());
        assert!(store.get_exact(all_hashes).is_err());

        assert_eq!(store.data_size_hint(), Some(0));
        assert_eq!(store.inner.size_tracker.num_blobs.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn disk_insert_and_retrieve() {
        let (store, _dir) = tmp_store();

        let (tx, blob) = rng_blobs(1).into_iter().next().unwrap();
        store.insert(tx, blob.clone()).unwrap();

        assert!(store.is_cached(&tx));
        let retrieved_blob = store.get(tx).unwrap().map(Arc::unwrap_or_clone).unwrap();
        assert_eq!(retrieved_blob, blob);
    }

    #[test]
    fn disk_delete_blob() {
        let (store, _dir) = tmp_store();

        let (tx, blob) = rng_blobs(1).into_iter().next().unwrap();
        store.insert(tx, blob).unwrap();
        assert!(store.is_cached(&tx));

        store.delete(tx).unwrap();
        assert!(store.inner.txs_to_delete.read().contains(&tx));
        store.cleanup();

        let result = store.get(tx).unwrap();
        assert_eq!(
            result,
            Some(Arc::new(BlobTransactionSidecarVariant::Eip4844(BlobTransactionSidecar {
                blobs: vec![],
                commitments: vec![],
                proofs: vec![]
            })))
        );
    }

    #[test]
    fn disk_insert_all_and_delete_all() {
        let (store, _dir) = tmp_store();

        let blobs = rng_blobs(5);
        let txs = blobs.iter().map(|(tx, _)| *tx).collect::<Vec<_>>();
        store.insert_all(blobs.clone()).unwrap();

        for (tx, _) in &blobs {
            assert!(store.is_cached(tx));
        }

        store.delete_all(txs.clone()).unwrap();
        store.cleanup();

        for tx in txs {
            let result = store.get(tx).unwrap();
            assert_eq!(
                result,
                Some(Arc::new(BlobTransactionSidecarVariant::Eip4844(BlobTransactionSidecar {
                    blobs: vec![],
                    commitments: vec![],
                    proofs: vec![]
                })))
            );
        }
    }

    #[test]
    fn disk_get_all_blobs() {
        let (store, _dir) = tmp_store();

        let blobs = rng_blobs(3);
        let txs = blobs.iter().map(|(tx, _)| *tx).collect::<Vec<_>>();
        store.insert_all(blobs.clone()).unwrap();

        let retrieved_blobs = store.get_all(txs.clone()).unwrap();
        for (tx, blob) in retrieved_blobs {
            assert!(blobs.contains(&(tx, Arc::unwrap_or_clone(blob))));
        }

        store.delete_all(txs).unwrap();
        store.cleanup();
    }

    #[test]
    fn disk_get_exact_blobs_success() {
        let (store, _dir) = tmp_store();

        let blobs = rng_blobs(3);
        let txs = blobs.iter().map(|(tx, _)| *tx).collect::<Vec<_>>();
        store.insert_all(blobs.clone()).unwrap();

        let retrieved_blobs = store.get_exact(txs).unwrap();
        for (retrieved_blob, (_, original_blob)) in retrieved_blobs.into_iter().zip(blobs) {
            assert_eq!(Arc::unwrap_or_clone(retrieved_blob), original_blob);
        }
    }

    #[test]
    fn disk_get_exact_blobs_failure() {
        let (store, _dir) = tmp_store();

        let blobs = rng_blobs(2);
        let txs = blobs.iter().map(|(tx, _)| *tx).collect::<Vec<_>>();
        store.insert_all(blobs).unwrap();

        // Try to get a blob that was never inserted
        let missing_tx = TxHash::random();
        let result = store.get_exact(vec![txs[0], missing_tx]);
        assert!(result.is_err());
    }

    #[test]
    fn disk_data_size_hint() {
        let (store, _dir) = tmp_store();
        assert_eq!(store.data_size_hint(), Some(0));

        let blobs = rng_blobs(2);
        store.insert_all(blobs).unwrap();
        assert!(store.data_size_hint().unwrap() > 0);
    }

    #[test]
    fn disk_cleanup_stat() {
        let (store, _dir) = tmp_store();

        let blobs = rng_blobs(3);
        let txs = blobs.iter().map(|(tx, _)| *tx).collect::<Vec<_>>();
        store.insert_all(blobs).unwrap();

        store.delete_all(txs).unwrap();
        let stat = store.cleanup();
        assert_eq!(stat.delete_succeed, 3);
        assert_eq!(stat.delete_failed, 0);
    }
}
