//! This is a basic kv database that stores all metadata about our blockchain and utreexo
//! state.

extern crate std;

use std::num::NonZeroUsize;
use std::path::Path;

use bitcoin::consensus::{deserialize, serialize};
use bitcoin::BlockHash;
use floresta_common::prelude::*;
use lru::LruCache;
use redb::{Database, Error as RedbError, TableDefinition};
use spin::Mutex;

use crate::{BestChain, ChainStore, DiskBlockHeader};

// redb tables: names mirror your old kv buckets
const HEADERS_TABLE: TableDefinition<'static, &'static [u8], &'static [u8]> =
    TableDefinition::new("headers");

const INDEX_TABLE: TableDefinition<'static, u32, &'static [u8]> =
    TableDefinition::new("index");

const META_TABLE: TableDefinition<'static, &'static str, &'static [u8]> =
    TableDefinition::new("meta");

const ROOTS_TABLE: TableDefinition<'static, &'static str, &'static [u8]> =
    TableDefinition::new("roots");

// Tune these numbers to your memory / perf target
const HEADER_CACHE_CAPACITY: usize = 64_000;
const INDEX_CACHE_CAPACITY: usize = 64_000;

pub struct KvChainStore {
    db: Database,

    // LRU caches like the FlatChainStore header cache.
    // We use interior mutability so we can mutate them from &self.
    header_cache: Mutex<LruCache<BlockHash, DiskBlockHeader>>,
    index_cache:  Mutex<LruCache<u32, BlockHash>>,
}

impl KvChainStore {
    pub fn new(datadir: String) -> Result<Self, RedbError> {
        std::fs::create_dir_all(&datadir).expect("Failed to create dir");
        let path = Path::new(&datadir).join("chain_data.redb");

        // You can also use Database::builder().set_cache_size(...) if you want.
        let db = Database::create(path)?;

        // Pre-create tables so later open_table() calls cannot fail with "table not found".
        let write_txn = db.begin_write()?;
        {
            write_txn.open_table(HEADERS_TABLE)?;
            write_txn.open_table(INDEX_TABLE)?;
            write_txn.open_table(META_TABLE)?;
            write_txn.open_table(ROOTS_TABLE)?;
        }
        write_txn.commit()?;

        Ok(Self {
            db,
            header_cache: Mutex::new(LruCache::new(NonZeroUsize::try_from(HEADER_CACHE_CAPACITY).unwrap())),
            index_cache:  Mutex::new(LruCache::new(NonZeroUsize::try_from(INDEX_CACHE_CAPACITY).unwrap())),
        })
    }
}

impl ChainStore for KvChainStore {
    type Error = RedbError;

    fn check_integrity(&self) -> Result<(), Self::Error> {
        // redb has Database::check_integrity(&mut self) but that needs &mut self
        // and the trait only gives us &self here, so we keep this as a no-op
        // (same story as the old sled/kv backend commentary).
        Ok(())
    }

    // ---------- Utreexo roots ----------

    fn load_roots_for_block(&mut self, height: u32) -> Result<Option<Vec<u8>>, Self::Error> {
        let key = format!("roots_{height}");

        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ROOTS_TABLE)?;

        if let Some(roots) = table.get(key.as_str())? {
            Ok(Some(roots.value().to_vec()))
        } else {
            Ok(None)
        }
    }

    fn save_roots_for_block(&mut self, roots: Vec<u8>, height: u32) -> Result<(), Self::Error> {
        let key = format!("roots_{height}");

        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(ROOTS_TABLE)?;
            table.insert(key.as_str(), roots.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    // ---------- BestChain / height ----------

    fn load_height(&self) -> Result<Option<BestChain>, Self::Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(META_TABLE)?;

        if let Some(entry) = table.get("height")? {
            let bytes = entry.value();
            let best =
                deserialize(bytes).expect("infallible: came from `serialize(height)`");
            Ok(Some(best))
        } else {
            Ok(None)
        }
    }

    fn save_height(&mut self, height: &BestChain) -> Result<(), Self::Error> {
        let bytes = serialize(height);

        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(META_TABLE)?;
            table.insert("height", bytes.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    // ---------- Headers (with LRU cache) ----------

    fn get_header(&self, block_hash: &BlockHash) -> Result<Option<DiskBlockHeader>, Self::Error> {
        // Fast path: check the LRU cache
        {
            let mut cache = self.header_cache.lock();
            if let Some(header) = cache.get(block_hash) {
                return Ok(Some(*header));
            }
        }

        // Slow path: go to the DB
        let key = serialize(block_hash);

        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(HEADERS_TABLE)?;

        if let Some(entry) = table.get(key.as_slice())? {
            let bytes = entry.value();
            if let Ok(header) = deserialize::<DiskBlockHeader>(bytes) {
                // Populate the cache on a DB hit, like the flat store does
                let mut cache = self.header_cache.lock();
                cache.put(*block_hash, header);
                Ok(Some(header))
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    fn get_header_by_height(&self, height: u32) -> Result<Option<DiskBlockHeader>, Self::Error> {
        if let Some(hash) = self.get_block_hash(height)? {
            self.get_header(&hash)
        } else {
            Ok(None)
        }
    }

    fn save_header(&mut self, header: &DiskBlockHeader) -> Result<(), Self::Error> {
        let header_copy = *header;
        let hash = header_copy.block_hash();

        // Update the LRU cache immediately (like FlatChainStore::save_header)
        {
            let mut cache = self.header_cache.lock();
            cache.put(hash, header_copy);
        }

        // Write directly to the DB (canonical store)
        let key = serialize(&hash);
        let value = serialize(&header_copy);

        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(HEADERS_TABLE)?;
            table.insert(key.as_slice(), value.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    // ---------- Block index (height -> hash) with LRU cache ----------

    fn get_block_hash(&self, height: u32) -> Result<Option<BlockHash>, Self::Error> {
        // Fast path: LRU cache
        {
            let mut cache = self.index_cache.lock();
            if let Some(hash) = cache.get(&height) {
                return Ok(Some(*hash));
            }
        }

        // DB
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(INDEX_TABLE)?;

        if let Some(entry) = table.get(height)? {
            let bytes = entry.value();
            if let Ok(hash) = deserialize::<BlockHash>(bytes) {
                let mut cache = self.index_cache.lock();
                cache.put(height, hash);
                Ok(Some(hash))
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    fn update_block_index(&mut self, height: u32, hash: BlockHash) -> Result<(), Self::Error> {
        // Update cache like the flat store does for headers
        {
            let mut cache = self.index_cache.lock();
            cache.put(height, hash);
        }

        // Write directly to DB
        let value = serialize(&hash);

        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(INDEX_TABLE)?;
            table.insert(height, value.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    // ---------- Flush ----------

    fn flush(&mut self) -> Result<(), Self::Error> {
        // For this backend, every write creates its own transaction and commits,
        // so there's nothing buffered to flush here.
        //
        // You *could* later experiment with longer-lived write transactions or
        // relaxed durability settings, but from the trait's POV this is a no-op.
        Ok(())
    }
}
