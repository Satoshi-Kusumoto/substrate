// Copyright 2017-2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! DB-backed changes tries storage.

use std::collections::HashMap;
use std::sync::Arc;
use kvdb::{KeyValueDB, DBTransaction};
use parity_codec::Encode;
use parking_lot::RwLock;
use client::error::{Error as ClientError, Result as ClientResult};
use trie::MemoryDB;
use client::blockchain::well_known_cache_keys;
use primitives::{H256, Blake2Hasher, ChangesTrieConfiguration, convert_hash};
use runtime_primitives::traits::{
	Block as BlockT, Header as HeaderT, NumberFor, Zero, One,
};
use runtime_primitives::generic::{BlockId, DigestItem, ChangesTrieSignal};
use state_machine::DBValue;
use crate::utils::{self, Meta};
use crate::cache::{DbCacheSync, DbCache, DbCacheTransactionOps, ComplexBlockId, EntryType as CacheEntryType};

/// Extract new changes trie configuration (if available) from the header.
pub fn extract_new_configuration<Header: HeaderT>(header: &Header) -> Option<&Option<ChangesTrieConfiguration>> {
	header.digest()
		.log(DigestItem::as_changes_trie_signal)
		.and_then(ChangesTrieSignal::as_new_configuration)
}

pub struct DbChangesTrieStorage<Block: BlockT> {
	db: Arc<dyn KeyValueDB>,
	changes_tries_column: Option<u32>,
	key_lookup_column: Option<u32>,
	header_column: Option<u32>,
	meta: Arc<RwLock<Meta<NumberFor<Block>, Block::Hash>>>,
	min_blocks_to_keep: Option<u32>,
	cache: DbCacheSync<Block>,
	_phantom: ::std::marker::PhantomData<Block>,
}

impl<Block: BlockT<Hash=H256>> DbChangesTrieStorage<Block> {
	/// Create new changes trie storage.
	pub fn new(
		db: Arc<dyn KeyValueDB>,
		changes_tries_column: Option<u32>,
		key_lookup_column: Option<u32>,
		header_column: Option<u32>,
		cache_column: Option<u32>,
		meta: Arc<RwLock<Meta<NumberFor<Block>, Block::Hash>>>,
		min_blocks_to_keep: Option<u32>,
	) -> Self {
		let (finalized_hash, finalized_number, genesis_hash) = {
			let meta = meta.read();
			(meta.finalized_hash, meta.finalized_number, meta.genesis_hash)
		};
		Self {
			db: db.clone(),
			changes_tries_column,
			key_lookup_column,
			header_column,
			meta,
			min_blocks_to_keep,
			cache: DbCacheSync(RwLock::new(DbCache::new(
				db.clone(),
				key_lookup_column,
				header_column,
				cache_column,
				genesis_hash,
				ComplexBlockId::new(finalized_hash, finalized_number),
			))),
			_phantom: Default::default(),
		}
	}

	/// Commit new changes trie.
	pub fn commit(
		&self,
		tx: &mut DBTransaction,
		mut changes_trie: MemoryDB<Blake2Hasher>,
		parent_block: ComplexBlockId<Block>,
		block: ComplexBlockId<Block>,
		finalized: bool,
		new_configuration: Option<Option<ChangesTrieConfiguration>>,
	) -> ClientResult<Option<DbCacheTransactionOps<Block>>> {
		for (key, (val, _)) in changes_trie.drain() {
			tx.put(self.changes_tries_column, &key[..], &val);
		}

		// TODO: (separate PR - make cache treat None as previous value, not the unknown one)
		if let Some(new_configuration) = new_configuration {
			let mut cache_at = HashMap::new();
			cache_at.insert(well_known_cache_keys::CHANGES_TRIE_CONFIG, new_configuration.encode());

			Ok(Some(self.cache.0.write().transaction(tx)
				.on_block_insert(
					parent_block,
					block,
					cache_at,
					if finalized { CacheEntryType::Final } else { CacheEntryType::NonFinal },
				)?
				.into_ops()))
		} else {
			Ok(None)
		}
	}

	/// When transaction has been committed.
	pub fn post_commit(&self, cache_ops: DbCacheTransactionOps<Block>) {
		self.cache.0.write().commit(cache_ops);
	}

	/// Prune obsolete changes tries.
	pub fn prune(
		&self,
		config: &ChangesTrieConfiguration,
		tx: &mut DBTransaction,
		block_hash: Block::Hash,
		block_num: NumberFor<Block>,
	) {
		// never prune on archive nodes
		let min_blocks_to_keep = match self.min_blocks_to_keep {
			Some(min_blocks_to_keep) => min_blocks_to_keep,
			None => return,
		};

		state_machine::prune_changes_tries(
			Zero::zero(), // TODO: not true
			config,
			&*self,
			min_blocks_to_keep.into(),
			&state_machine::ChangesTrieAnchorBlockId {
				hash: convert_hash(&block_hash),
				number: block_num,
			},
			|node| tx.delete(self.changes_tries_column, node.as_ref()));
	}
}

impl<Block> client::backend::PrunableStateChangesTrieStorage<Block, Blake2Hasher>
	for DbChangesTrieStorage<Block>
where
	Block: BlockT<Hash=H256>,
{
	fn oldest_changes_trie_block(
		&self,
		config: &ChangesTrieConfiguration,
		best_finalized_block: NumberFor<Block>,
	) -> NumberFor<Block> {
		match self.min_blocks_to_keep {
			Some(min_blocks_to_keep) => state_machine::oldest_non_pruned_changes_trie(
				Zero::zero(), // TODO: not true
				config,
				min_blocks_to_keep.into(),
				best_finalized_block,
			),
			None => One::one(),
		}
	}
}

impl<Block> state_machine::ChangesTrieRootsStorage<Blake2Hasher, NumberFor<Block>>
	for DbChangesTrieStorage<Block>
where
	Block: BlockT<Hash=H256>,
{
	fn build_anchor(
		&self,
		hash: H256,
	) -> Result<state_machine::ChangesTrieAnchorBlockId<H256, NumberFor<Block>>, String> {
		utils::read_header::<Block>(&*self.db, self.key_lookup_column, self.header_column, BlockId::Hash(hash))
			.map_err(|e| e.to_string())
			.and_then(|maybe_header| maybe_header.map(|header|
				state_machine::ChangesTrieAnchorBlockId {
					hash,
					number: *header.number(),
				}
			).ok_or_else(|| format!("Unknown header: {}", hash)))
	}

	fn root(
		&self,
		anchor: &state_machine::ChangesTrieAnchorBlockId<H256, NumberFor<Block>>,
		block: NumberFor<Block>,
	) -> Result<Option<H256>, String> {
		// check API requirement: we can't get NEXT block(s) based on anchor
		if block > anchor.number {
			return Err(format!("Can't get changes trie root at {} using anchor at {}", block, anchor.number));
		}

		// we need to get hash of the block to resolve changes trie root
		let block_id = if block <= self.meta.read().finalized_number {
			// if block is finalized, we could just read canonical hash
			BlockId::Number(block)
		} else {
			// the block is not finalized
			let mut current_num = anchor.number;
			let mut current_hash: Block::Hash = convert_hash(&anchor.hash);
			let maybe_anchor_header: Block::Header = utils::require_header::<Block>(
				&*self.db, self.key_lookup_column, self.header_column, BlockId::Number(current_num)
			).map_err(|e| e.to_string())?;
			if maybe_anchor_header.hash() == current_hash {
				// if anchor is canonicalized, then the block is also canonicalized
				BlockId::Number(block)
			} else {
				// else (block is not finalized + anchor is not canonicalized):
				// => we should find the required block hash by traversing
				// back from the anchor to the block with given number
				while current_num != block {
					let current_header: Block::Header = utils::require_header::<Block>(
						&*self.db, self.key_lookup_column, self.header_column, BlockId::Hash(current_hash)
					).map_err(|e| e.to_string())?;

					current_hash = *current_header.parent_hash();
					current_num = current_num - One::one();
				}

				BlockId::Hash(current_hash)
			}
		};

		Ok(utils::require_header::<Block>(&*self.db, self.key_lookup_column, self.header_column, block_id)
			.map_err(|e| e.to_string())?
			.digest().log(DigestItem::as_changes_trie_root)
			.map(|root| H256::from_slice(root.as_ref())))
	}
}

impl<Block> state_machine::ChangesTrieStorage<Blake2Hasher, NumberFor<Block>>
	for DbChangesTrieStorage<Block>
where
	Block: BlockT<Hash=H256>,
{
	fn get(&self, key: &H256, _prefix: &[u8]) -> Result<Option<DBValue>, String> {
		self.db.get(self.changes_tries_column, &key[..])
			.map_err(|err| format!("{}", err))
	}
}

#[cfg(test)]
mod tests {
	use client::backend::Backend as ClientBackend;
	use client::blockchain::HeaderBackend as BlockchainHeaderBackend;
	use state_machine::{ChangesTrieRootsStorage, ChangesTrieStorage};
	use crate::Backend;
	use crate::tests::{Block, insert_header, prepare_changes};
	use super::*;

	#[test]
	fn changes_trie_storage_works() {
		let backend = Backend::<Block>::new_test(1000, 100);
		backend.changes_tries_storage.meta.write().finalized_number = 1000;


		let check_changes = |backend: &Backend<Block>, block: u64, changes: Vec<(Vec<u8>, Vec<u8>)>| {
			let (changes_root, mut changes_trie_update) = prepare_changes(changes);
			let anchor = state_machine::ChangesTrieAnchorBlockId {
				hash: backend.blockchain().header(BlockId::Number(block)).unwrap().unwrap().hash(),
				number: block
			};
			assert_eq!(backend.changes_tries_storage.root(&anchor, block), Ok(Some(changes_root)));

			for (key, (val, _)) in changes_trie_update.drain() {
				assert_eq!(backend.changes_trie_storage().unwrap().get(&key, &[]), Ok(Some(val)));
			}
		};

		let changes0 = vec![(b"key_at_0".to_vec(), b"val_at_0".to_vec())];
		let changes1 = vec![
			(b"key_at_1".to_vec(), b"val_at_1".to_vec()),
			(b"another_key_at_1".to_vec(), b"another_val_at_1".to_vec()),
		];
		let changes2 = vec![(b"key_at_2".to_vec(), b"val_at_2".to_vec())];

		let block0 = insert_header(&backend, 0, Default::default(), changes0.clone(), Default::default());
		let block1 = insert_header(&backend, 1, block0, changes1.clone(), Default::default());
		let _ = insert_header(&backend, 2, block1, changes2.clone(), Default::default());

		// check that the storage contains tries for all blocks
		check_changes(&backend, 0, changes0);
		check_changes(&backend, 1, changes1);
		check_changes(&backend, 2, changes2);
	}

	#[test]
	fn changes_trie_storage_works_with_forks() {
		let backend = Backend::<Block>::new_test(1000, 100);

		let changes0 = vec![(b"k0".to_vec(), b"v0".to_vec())];
		let changes1 = vec![(b"k1".to_vec(), b"v1".to_vec())];
		let changes2 = vec![(b"k2".to_vec(), b"v2".to_vec())];
		let block0 = insert_header(&backend, 0, Default::default(), changes0.clone(), Default::default());
		let block1 = insert_header(&backend, 1, block0, changes1.clone(), Default::default());
		let block2 = insert_header(&backend, 2, block1, changes2.clone(), Default::default());

		let changes2_1_0 = vec![(b"k3".to_vec(), b"v3".to_vec())];
		let changes2_1_1 = vec![(b"k4".to_vec(), b"v4".to_vec())];
		let block2_1_0 = insert_header(&backend, 3, block2, changes2_1_0.clone(), Default::default());
		let block2_1_1 = insert_header(&backend, 4, block2_1_0, changes2_1_1.clone(), Default::default());

		let changes2_2_0 = vec![(b"k5".to_vec(), b"v5".to_vec())];
		let changes2_2_1 = vec![(b"k6".to_vec(), b"v6".to_vec())];
		let block2_2_0 = insert_header(&backend, 3, block2, changes2_2_0.clone(), Default::default());
		let block2_2_1 = insert_header(&backend, 4, block2_2_0, changes2_2_1.clone(), Default::default());

		// finalize block1
		backend.changes_tries_storage.meta.write().finalized_number = 1;

		// branch1: when asking for finalized block hash
		let (changes1_root, _) = prepare_changes(changes1);
		let anchor = state_machine::ChangesTrieAnchorBlockId { hash: block2_1_1, number: 4 };
		assert_eq!(backend.changes_tries_storage.root(&anchor, 1), Ok(Some(changes1_root)));

		// branch2: when asking for finalized block hash
		let anchor = state_machine::ChangesTrieAnchorBlockId { hash: block2_2_1, number: 4 };
		assert_eq!(backend.changes_tries_storage.root(&anchor, 1), Ok(Some(changes1_root)));

		// branch1: when asking for non-finalized block hash (search by traversal)
		let (changes2_1_0_root, _) = prepare_changes(changes2_1_0);
		let anchor = state_machine::ChangesTrieAnchorBlockId { hash: block2_1_1, number: 4 };
		assert_eq!(backend.changes_tries_storage.root(&anchor, 3), Ok(Some(changes2_1_0_root)));

		// branch2: when asking for non-finalized block hash (search using canonicalized hint)
		let (changes2_2_0_root, _) = prepare_changes(changes2_2_0);
		let anchor = state_machine::ChangesTrieAnchorBlockId { hash: block2_2_1, number: 4 };
		assert_eq!(backend.changes_tries_storage.root(&anchor, 3), Ok(Some(changes2_2_0_root)));

		// finalize first block of branch2 (block2_2_0)
		backend.changes_tries_storage.meta.write().finalized_number = 3;

		// branch2: when asking for finalized block of this branch
		assert_eq!(backend.changes_tries_storage.root(&anchor, 3), Ok(Some(changes2_2_0_root)));

		// branch1: when asking for finalized block of other branch
		// => result is incorrect (returned for the block of branch1), but this is expected,
		// because the other fork is abandoned (forked before finalized header)
		let anchor = state_machine::ChangesTrieAnchorBlockId { hash: block2_1_1, number: 4 };
		assert_eq!(backend.changes_tries_storage.root(&anchor, 3), Ok(Some(changes2_2_0_root)));
	}

	#[test]
	fn changes_tries_with_digest_are_pruned_on_finalization() {
		let mut backend = Backend::<Block>::new_test(1000, 100);
		backend.changes_tries_storage.min_blocks_to_keep = Some(8);
		let config = ChangesTrieConfiguration {
			digest_interval: 2,
			digest_levels: 2,
		};

		// insert some blocks
		let block0 = insert_header(&backend, 0, Default::default(), vec![(b"key_at_0".to_vec(), b"val_at_0".to_vec())], Default::default());
		let block1 = insert_header(&backend, 1, block0, vec![(b"key_at_1".to_vec(), b"val_at_1".to_vec())], Default::default());
		let block2 = insert_header(&backend, 2, block1, vec![(b"key_at_2".to_vec(), b"val_at_2".to_vec())], Default::default());
		let block3 = insert_header(&backend, 3, block2, vec![(b"key_at_3".to_vec(), b"val_at_3".to_vec())], Default::default());
		let block4 = insert_header(&backend, 4, block3, vec![(b"key_at_4".to_vec(), b"val_at_4".to_vec())], Default::default());
		let block5 = insert_header(&backend, 5, block4, vec![(b"key_at_5".to_vec(), b"val_at_5".to_vec())], Default::default());
		let block6 = insert_header(&backend, 6, block5, vec![(b"key_at_6".to_vec(), b"val_at_6".to_vec())], Default::default());
		let block7 = insert_header(&backend, 7, block6, vec![(b"key_at_7".to_vec(), b"val_at_7".to_vec())], Default::default());
		let block8 = insert_header(&backend, 8, block7, vec![(b"key_at_8".to_vec(), b"val_at_8".to_vec())], Default::default());
		let block9 = insert_header(&backend, 9, block8, vec![(b"key_at_9".to_vec(), b"val_at_9".to_vec())], Default::default());
		let block10 = insert_header(&backend, 10, block9, vec![(b"key_at_10".to_vec(), b"val_at_10".to_vec())], Default::default());
		let block11 = insert_header(&backend, 11, block10, vec![(b"key_at_11".to_vec(), b"val_at_11".to_vec())], Default::default());
		let block12 = insert_header(&backend, 12, block11, vec![(b"key_at_12".to_vec(), b"val_at_12".to_vec())], Default::default());
		let block13 = insert_header(&backend, 13, block12, vec![(b"key_at_13".to_vec(), b"val_at_13".to_vec())], Default::default());
		backend.changes_tries_storage.meta.write().finalized_number = 13;

		// check that roots of all tries are in the columns::CHANGES_TRIE
		let anchor = state_machine::ChangesTrieAnchorBlockId { hash: block13, number: 13 };
		fn read_changes_trie_root(backend: &Backend<Block>, num: u64) -> H256 {
			backend.blockchain().header(BlockId::Number(num)).unwrap().unwrap().digest().logs().iter()
				.find(|i| i.as_changes_trie_root().is_some()).unwrap().as_changes_trie_root().unwrap().clone()
		}
		let root1 = read_changes_trie_root(&backend, 1); assert_eq!(backend.changes_tries_storage.root(&anchor, 1).unwrap(), Some(root1));
		let root2 = read_changes_trie_root(&backend, 2); assert_eq!(backend.changes_tries_storage.root(&anchor, 2).unwrap(), Some(root2));
		let root3 = read_changes_trie_root(&backend, 3); assert_eq!(backend.changes_tries_storage.root(&anchor, 3).unwrap(), Some(root3));
		let root4 = read_changes_trie_root(&backend, 4); assert_eq!(backend.changes_tries_storage.root(&anchor, 4).unwrap(), Some(root4));
		let root5 = read_changes_trie_root(&backend, 5); assert_eq!(backend.changes_tries_storage.root(&anchor, 5).unwrap(), Some(root5));
		let root6 = read_changes_trie_root(&backend, 6); assert_eq!(backend.changes_tries_storage.root(&anchor, 6).unwrap(), Some(root6));
		let root7 = read_changes_trie_root(&backend, 7); assert_eq!(backend.changes_tries_storage.root(&anchor, 7).unwrap(), Some(root7));
		let root8 = read_changes_trie_root(&backend, 8); assert_eq!(backend.changes_tries_storage.root(&anchor, 8).unwrap(), Some(root8));
		let root9 = read_changes_trie_root(&backend, 9); assert_eq!(backend.changes_tries_storage.root(&anchor, 9).unwrap(), Some(root9));
		let root10 = read_changes_trie_root(&backend, 10); assert_eq!(backend.changes_tries_storage.root(&anchor, 10).unwrap(), Some(root10));
		let root11 = read_changes_trie_root(&backend, 11); assert_eq!(backend.changes_tries_storage.root(&anchor, 11).unwrap(), Some(root11));
		let root12 = read_changes_trie_root(&backend, 12); assert_eq!(backend.changes_tries_storage.root(&anchor, 12).unwrap(), Some(root12));

		// now simulate finalization of block#12, causing prune of tries at #1..#4
		let mut tx = DBTransaction::new();
		backend.changes_tries_storage.prune(&config, &mut tx, Default::default(), 12);
		backend.storage.db.write(tx).unwrap();
		assert!(backend.changes_tries_storage.get(&root1, &[]).unwrap().is_none());
		assert!(backend.changes_tries_storage.get(&root2, &[]).unwrap().is_none());
		assert!(backend.changes_tries_storage.get(&root3, &[]).unwrap().is_none());
		assert!(backend.changes_tries_storage.get(&root4, &[]).unwrap().is_none());
		assert!(backend.changes_tries_storage.get(&root5, &[]).unwrap().is_some());
		assert!(backend.changes_tries_storage.get(&root6, &[]).unwrap().is_some());
		assert!(backend.changes_tries_storage.get(&root7, &[]).unwrap().is_some());
		assert!(backend.changes_tries_storage.get(&root8, &[]).unwrap().is_some());

		// now simulate finalization of block#16, causing prune of tries at #5..#8
		let mut tx = DBTransaction::new();
		backend.changes_tries_storage.prune(&config, &mut tx, Default::default(), 16);
		backend.storage.db.write(tx).unwrap();
		assert!(backend.changes_tries_storage.get(&root5, &[]).unwrap().is_none());
		assert!(backend.changes_tries_storage.get(&root6, &[]).unwrap().is_none());
		assert!(backend.changes_tries_storage.get(&root7, &[]).unwrap().is_none());
		assert!(backend.changes_tries_storage.get(&root8, &[]).unwrap().is_none());

		// now "change" pruning mode to archive && simulate finalization of block#20
		// => no changes tries are pruned, because we never prune in archive mode
		backend.changes_tries_storage.min_blocks_to_keep = None;
		let mut tx = DBTransaction::new();
		backend.changes_tries_storage.prune(&config, &mut tx, Default::default(), 20);
		backend.storage.db.write(tx).unwrap();
		assert!(backend.changes_tries_storage.get(&root9, &[]).unwrap().is_some());
		assert!(backend.changes_tries_storage.get(&root10, &[]).unwrap().is_some());
		assert!(backend.changes_tries_storage.get(&root11, &[]).unwrap().is_some());
		assert!(backend.changes_tries_storage.get(&root12, &[]).unwrap().is_some());
	}

	#[test]
	fn changes_tries_without_digest_are_pruned_on_finalization() {
		let mut backend = Backend::<Block>::new_test(1000, 100);
		backend.changes_tries_storage.min_blocks_to_keep = Some(4);
		let config = ChangesTrieConfiguration {
			digest_interval: 0,
			digest_levels: 0,
		};

		// insert some blocks
		let block0 = insert_header(&backend, 0, Default::default(), vec![(b"key_at_0".to_vec(), b"val_at_0".to_vec())], Default::default());
		let block1 = insert_header(&backend, 1, block0, vec![(b"key_at_1".to_vec(), b"val_at_1".to_vec())], Default::default());
		let block2 = insert_header(&backend, 2, block1, vec![(b"key_at_2".to_vec(), b"val_at_2".to_vec())], Default::default());
		let block3 = insert_header(&backend, 3, block2, vec![(b"key_at_3".to_vec(), b"val_at_3".to_vec())], Default::default());
		let block4 = insert_header(&backend, 4, block3, vec![(b"key_at_4".to_vec(), b"val_at_4".to_vec())], Default::default());
		let block5 = insert_header(&backend, 5, block4, vec![(b"key_at_5".to_vec(), b"val_at_5".to_vec())], Default::default());
		let block6 = insert_header(&backend, 6, block5, vec![(b"key_at_6".to_vec(), b"val_at_6".to_vec())], Default::default());

		// check that roots of all tries are in the columns::CHANGES_TRIE
		let anchor = state_machine::ChangesTrieAnchorBlockId { hash: block6, number: 6 };
		fn read_changes_trie_root(backend: &Backend<Block>, num: u64) -> H256 {
			backend.blockchain().header(BlockId::Number(num)).unwrap().unwrap().digest().logs().iter()
				.find(|i| i.as_changes_trie_root().is_some()).unwrap().as_changes_trie_root().unwrap().clone()
		}

		let root1 = read_changes_trie_root(&backend, 1); assert_eq!(backend.changes_tries_storage.root(&anchor, 1).unwrap(), Some(root1));
		let root2 = read_changes_trie_root(&backend, 2); assert_eq!(backend.changes_tries_storage.root(&anchor, 2).unwrap(), Some(root2));
		let root3 = read_changes_trie_root(&backend, 3); assert_eq!(backend.changes_tries_storage.root(&anchor, 3).unwrap(), Some(root3));
		let root4 = read_changes_trie_root(&backend, 4); assert_eq!(backend.changes_tries_storage.root(&anchor, 4).unwrap(), Some(root4));
		let root5 = read_changes_trie_root(&backend, 5); assert_eq!(backend.changes_tries_storage.root(&anchor, 5).unwrap(), Some(root5));
		let root6 = read_changes_trie_root(&backend, 6); assert_eq!(backend.changes_tries_storage.root(&anchor, 6).unwrap(), Some(root6));

		// now simulate finalization of block#5, causing prune of trie at #1
		let mut tx = DBTransaction::new();
		backend.changes_tries_storage.prune(&config, &mut tx, block5, 5);
		backend.storage.db.write(tx).unwrap();
		assert!(backend.changes_tries_storage.get(&root1, &[]).unwrap().is_none());
		assert!(backend.changes_tries_storage.get(&root2, &[]).unwrap().is_some());

		// now simulate finalization of block#6, causing prune of tries at #2
		let mut tx = DBTransaction::new();
		backend.changes_tries_storage.prune(&config, &mut tx, block6, 6);
		backend.storage.db.write(tx).unwrap();
		assert!(backend.changes_tries_storage.get(&root2, &[]).unwrap().is_none());
		assert!(backend.changes_tries_storage.get(&root3, &[]).unwrap().is_some());
	}
}
