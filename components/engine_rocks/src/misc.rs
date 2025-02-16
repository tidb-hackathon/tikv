// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

use crate::engine::RocksEngine;
use crate::util;
use engine_traits::{
    IterOptions, Iterable, Iterator, MiscExt, Mutable, Range, Result, WriteBatch, WriteBatchExt,
    CF_LOCK, MAX_DELETE_BATCH_COUNT,
};
use tikv_util::keybuilder::KeyBuilder;

impl MiscExt for RocksEngine {
    fn flush_cf(&self, cf: &str, sync: bool) -> Result<()> {
        let handle = util::get_cf_handle(self.as_inner(), cf)?;
        Ok(self.as_inner().flush_cf(handle, sync)?)
    }

    fn delete_files_in_range_cf(
        &self,
        cf: &str,
        start_key: &[u8],
        end_key: &[u8],
        include_end: bool,
    ) -> Result<()> {
        let handle = util::get_cf_handle(self.as_inner(), cf)?;
        Ok(self
            .as_inner()
            .delete_files_in_range_cf(handle, start_key, end_key, include_end)?)
    }

    fn delete_blob_files_in_range_cf(
        &self,
        cf: &str,
        start_key: &[u8],
        end_key: &[u8],
        include_end: bool,
    ) -> Result<()> {
        if self.as_inner().is_titan() {
            let handle = util::get_cf_handle(self.as_inner(), cf)?;
            self.as_inner().delete_blob_files_in_range_cf(
                handle,
                start_key,
                end_key,
                include_end,
            )?;
        }

        Ok(())
    }

    fn delete_all_in_range_cf(
        &self,
        cf: &str,
        start_key: &[u8],
        end_key: &[u8],
        use_delete_range: bool,
    ) -> Result<()> {
        let mut wb = self.write_batch();
        if use_delete_range && cf != CF_LOCK {
            wb.delete_range_cf(cf, start_key, end_key)?;
        } else {
            let start = KeyBuilder::from_slice(start_key, 0, 0);
            let end = KeyBuilder::from_slice(end_key, 0, 0);
            let mut opts = IterOptions::new(Some(start), Some(end), false);
            if self.as_inner().is_titan() {
                // Cause DeleteFilesInRange may expose old blob index keys, setting key only for Titan
                // to avoid referring to missing blob files.
                opts.set_key_only(true);
            }
            let mut it = self.iterator_cf_opt(cf, opts)?;
            let mut it_valid = it.seek(start_key.into())?;
            while it_valid {
                wb.delete_cf(cf, it.key())?;
                if wb.count() >= MAX_DELETE_BATCH_COUNT {
                    self.write(&wb)?;
                    wb.clear();
                }
                it_valid = it.next()?;
            }
        }
        if wb.count() > 0 {
            self.write(&wb)?;
        }
        Ok(())
    }

    fn get_approximate_memtable_stats_cf(&self, cf: &str, range: &Range) -> Result<(u64, u64)> {
        let range = util::range_to_rocks_range(range);
        let handle = util::get_cf_handle(self.as_inner(), cf)?;
        Ok(self
            .as_inner()
            .get_approximate_memtable_stats_cf(handle, &range))
    }
}

#[cfg(test)]
mod tests {
    use tempfile::Builder;

    use crate::engine::RocksEngine;
    use engine::rocks;
    use engine::rocks::util::{new_engine_opt, CFOptions};
    use engine::rocks::{ColumnFamilyOptions, DBOptions};
    use engine::DB;
    use std::sync::Arc;

    use super::*;
    use engine_traits::ALL_CFS;
    use engine_traits::{Iterable, Iterator, Mutable, SeekKey, SyncMutable, WriteBatchExt};

    fn check_data(db: &RocksEngine, cfs: &[&str], expected: &[(&[u8], &[u8])]) {
        for cf in cfs {
            let mut iter = db.iterator_cf(cf).unwrap();
            iter.seek(SeekKey::Start).unwrap();
            for &(k, v) in expected {
                assert_eq!(k, iter.key());
                assert_eq!(v, iter.value());
                iter.next().unwrap();
            }
            assert!(!iter.valid().unwrap());
        }
    }

    fn test_delete_all_in_range(use_delete_range: bool) {
        let path = Builder::new()
            .prefix("engine_delete_all_in_range")
            .tempdir()
            .unwrap();
        let path_str = path.path().to_str().unwrap();

        let cfs_opts = ALL_CFS
            .iter()
            .map(|cf| CFOptions::new(cf, ColumnFamilyOptions::new()))
            .collect();
        let db = new_engine_opt(path_str, DBOptions::new(), cfs_opts).unwrap();
        let db = Arc::new(db);
        let db = RocksEngine::from_db(db);

        let mut wb = db.write_batch();
        let ts: u8 = 12;
        let keys: Vec<_> = vec![
            b"k1".to_vec(),
            b"k2".to_vec(),
            b"k3".to_vec(),
            b"k4".to_vec(),
        ]
        .into_iter()
        .map(|mut k| {
            k.append(&mut vec![ts; 8]);
            k
        })
        .collect();

        let mut kvs: Vec<(&[u8], &[u8])> = vec![];
        for (_, key) in keys.iter().enumerate() {
            kvs.push((key.as_slice(), b"value"));
        }
        let kvs_left: Vec<(&[u8], &[u8])> = vec![(kvs[0].0, kvs[0].1), (kvs[3].0, kvs[3].1)];
        for &(k, v) in kvs.as_slice() {
            for cf in ALL_CFS {
                wb.put_cf(cf, k, v).unwrap();
            }
        }
        db.write(&wb).unwrap();
        check_data(&db, ALL_CFS, kvs.as_slice());

        // Delete all in ["k2", "k4").
        let start = b"k2";
        let end = b"k4";
        db.delete_all_in_range(start, end, use_delete_range)
            .unwrap();
        check_data(&db, ALL_CFS, kvs_left.as_slice());
    }

    #[test]
    fn test_delete_all_in_range_use_delete_range() {
        test_delete_all_in_range(true);
    }

    #[test]
    fn test_delete_all_in_range_not_use_delete_range() {
        test_delete_all_in_range(false);
    }

    #[test]
    fn test_delete_all_files_in_range() {
        let path = Builder::new()
            .prefix("engine_delete_all_files_in_range")
            .tempdir()
            .unwrap();
        let path_str = path.path().to_str().unwrap();

        let cfs_opts = ALL_CFS
            .iter()
            .map(|cf| {
                let mut cf_opts = ColumnFamilyOptions::new();
                cf_opts.set_level_zero_file_num_compaction_trigger(1);
                CFOptions::new(cf, cf_opts)
            })
            .collect();
        let db = new_engine_opt(path_str, DBOptions::new(), cfs_opts).unwrap();
        let db = Arc::new(db);
        let db = RocksEngine::from_db(db);

        let keys = vec![b"k1", b"k2", b"k3", b"k4"];

        let mut kvs: Vec<(&[u8], &[u8])> = vec![];
        for key in keys {
            kvs.push((key, b"value"));
        }
        let kvs_left: Vec<(&[u8], &[u8])> = vec![(kvs[0].0, kvs[0].1), (kvs[3].0, kvs[3].1)];
        for cf in ALL_CFS {
            for &(k, v) in kvs.as_slice() {
                db.put_cf(cf, k, v).unwrap();
                db.flush_cf(cf, true).unwrap();
            }
        }
        check_data(&db, ALL_CFS, kvs.as_slice());

        db.delete_all_files_in_range(b"k2", b"k4").unwrap();
        db.delete_blob_files_in_range(b"k2", b"k4").unwrap();
        check_data(&db, ALL_CFS, kvs_left.as_slice());
    }

    #[test]
    fn test_delete_range_prefix_bloom_case() {
        let path = Builder::new()
            .prefix("engine_delete_range_prefix_bloom")
            .tempdir()
            .unwrap();
        let path_str = path.path().to_str().unwrap();

        let mut opts = DBOptions::new();
        opts.create_if_missing(true);

        let mut cf_opts = ColumnFamilyOptions::new();
        // Prefix extractor(trim the timestamp at tail) for write cf.
        cf_opts
            .set_prefix_extractor(
                "FixedSuffixSliceTransform",
                Box::new(rocks::util::FixedSuffixSliceTransform::new(8)),
            )
            .unwrap_or_else(|err| panic!("{:?}", err));
        // Create prefix bloom filter for memtable.
        cf_opts.set_memtable_prefix_bloom_size_ratio(0.1 as f64);
        let cf = "default";
        let db = DB::open_cf(opts, path_str, vec![(cf, cf_opts)]).unwrap();
        let db = Arc::new(db);
        let db = RocksEngine::from_db(db);
        let mut wb = db.write_batch();
        let kvs: Vec<(&[u8], &[u8])> = vec![
            (b"kabcdefg1", b"v1"),
            (b"kabcdefg2", b"v2"),
            (b"kabcdefg3", b"v3"),
            (b"kabcdefg4", b"v4"),
        ];
        let kvs_left: Vec<(&[u8], &[u8])> = vec![(b"kabcdefg1", b"v1"), (b"kabcdefg4", b"v4")];

        for &(k, v) in kvs.as_slice() {
            wb.put_cf(cf, k, v).unwrap();
        }
        db.write(&wb).unwrap();
        check_data(&db, &[cf], kvs.as_slice());

        // Delete all in ["k2", "k4").
        db.delete_all_in_range(b"kabcdefg2", b"kabcdefg4", true)
            .unwrap();
        check_data(&db, &[cf], kvs_left.as_slice());
    }
}
