// Copyright 2020 The Grin Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Utility structs to handle the 3 MMRs (output, rangeproof,
//! kernel) along the overall header MMR conveniently and transactionally.

use crate::core::core::committed::Committed;
use crate::core::core::hash::{Hash, Hashed};
use crate::core::core::merkle_proof::MerkleProof;
use crate::core::core::pmmr::{self, Backend, ReadonlyPMMR, RewindablePMMR, PMMR};
use crate::core::core::{Block, BlockHeader, Input, Output, OutputIdentifier, TxKernel};
use crate::core::core::{
	BlockTokenSums, TokenInput, TokenIssueProof, TokenKey, TokenOutput, TokenOutputIdentifier,
	TokenTxKernel,
};
use crate::core::ser::{PMMRable, ProtocolVersion};
use crate::error::{Error, ErrorKind};
use crate::store::{Batch, ChainStore};
use crate::txhashset::bitmap_accumulator::BitmapAccumulator;
use crate::txhashset::{RewindableKernelView, UTXOView};
use crate::types::{CommitPos, OutputRoots, Tip, TxHashSetRoots, TxHashsetWriteStatus};
use crate::util::secp::pedersen::{Commitment, RangeProof};
use crate::util::{file, secp_static, zip};
use croaring::Bitmap;
use grin_store;
use grin_store::pmmr::{clean_files_by_prefix, PMMRBackend};
use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

const TXHASHSET_SUBDIR: &str = "txhashset";

const OUTPUT_SUBDIR: &str = "output";
const RANGE_PROOF_SUBDIR: &str = "rangeproof";
const TOKEN_OUTPUT_SUBDIR: &str = "tokenoutput";
const TOKEN_RANGE_PROOF_SUBDIR: &str = "tokenrangeproof";
const TOKEN_ISSUE_PROOF_SUBDIR: &str = "tokenissueproof";
const KERNEL_SUBDIR: &str = "kernel";
const TOKEN_KERNEL_SUBDIR: &str = "tokenkernel";

const TXHASHSET_ZIP: &str = "txhashset_snapshot";

/// Convenience wrapper around a single prunable MMR backend.
pub struct PMMRHandle<T: PMMRable> {
	/// The backend storage for the MMR.
	pub backend: PMMRBackend<T>,
	/// The last position accessible via this MMR handle (backend may continue out beyond this).
	pub last_pos: u64,
}

impl<T: PMMRable> PMMRHandle<T> {
	/// Constructor to create a PMMR handle from an existing directory structure on disk.
	/// Creates the backend files as necessary if they do not already exist.
	pub fn new(
		root_dir: &str,
		sub_dir: &str,
		file_name: &str,
		prunable: bool,
		version: ProtocolVersion,
		header: Option<&BlockHeader>,
	) -> Result<PMMRHandle<T>, Error> {
		let path = Path::new(root_dir).join(sub_dir).join(file_name);
		fs::create_dir_all(path.clone())?;
		let path_str = path
			.to_str()
			.ok_or_else(|| ErrorKind::Other("invalid file path".to_owned()))?;
		let backend = PMMRBackend::new(path_str.to_string(), prunable, version, header)?;
		let last_pos = backend.unpruned_size();
		Ok(PMMRHandle { backend, last_pos })
	}
}

impl PMMRHandle<BlockHeader> {
	/// Get the header hash at the specified height based on the current header MMR state.
	pub fn get_header_hash_by_height(&self, height: u64) -> Result<Hash, Error> {
		let pos = pmmr::insertion_to_pmmr_index(height + 1);
		let header_pmmr = ReadonlyPMMR::at(&self.backend, self.last_pos);
		if let Some(entry) = header_pmmr.get_data(pos) {
			Ok(entry.hash())
		} else {
			Err(ErrorKind::Other("get header hash by height".to_string()).into())
		}
	}

	/// Get the header hash for the head of the header chain based on current MMR state.
	/// Find the last leaf pos based on MMR size and return its header hash.
	pub fn head_hash(&self) -> Result<Hash, Error> {
		if self.last_pos == 0 {
			return Err(ErrorKind::Other("MMR empty, no head".to_string()).into());
		}
		let header_pmmr = ReadonlyPMMR::at(&self.backend, self.last_pos);
		let leaf_pos = pmmr::bintree_rightmost(self.last_pos);
		if let Some(entry) = header_pmmr.get_data(leaf_pos) {
			Ok(entry.hash())
		} else {
			Err(ErrorKind::Other("failed to find head hash".to_string()).into())
		}
	}
}

/// An easy to manipulate structure holding the 3 MMRs necessary to
/// validate blocks and capturing the output set, associated rangeproofs and the
/// kernels. Also handles the index of Commitments to positions in the
/// output and rangeproof MMRs.
///
/// Note that the index is never authoritative, only the trees are
/// guaranteed to indicate whether an output is spent or not. The index
/// may have commitments that have already been spent, even with
/// pruning enabled.
pub struct TxHashSet {
	output_pmmr_h: PMMRHandle<Output>,
	rproof_pmmr_h: PMMRHandle<RangeProof>,
	kernel_pmmr_h: PMMRHandle<TxKernel>,

	token_output_pmmr_h: PMMRHandle<TokenOutput>,
	token_rproof_pmmr_h: PMMRHandle<RangeProof>,
	token_issue_proof_pmmr_h: PMMRHandle<TokenIssueProof>,
	token_kernel_pmmr_h: PMMRHandle<TokenTxKernel>,

	bitmap_accumulator: BitmapAccumulator,

	// chain store used as index of commitments to MMR positions
	commit_index: Arc<ChainStore>,
}

impl TxHashSet {
	/// Open an existing or new set of backends for the TxHashSet
	pub fn open(
		root_dir: String,
		commit_index: Arc<ChainStore>,
		header: Option<&BlockHeader>,
	) -> Result<TxHashSet, Error> {
		let output_pmmr_h = PMMRHandle::new(
			&root_dir,
			TXHASHSET_SUBDIR,
			OUTPUT_SUBDIR,
			true,
			ProtocolVersion(1),
			header,
		)?;

		let rproof_pmmr_h = PMMRHandle::new(
			&root_dir,
			TXHASHSET_SUBDIR,
			RANGE_PROOF_SUBDIR,
			true,
			ProtocolVersion(1),
			header,
		)?;
		let token_output_pmmr_h = PMMRHandle::new(
			&root_dir,
			TXHASHSET_SUBDIR,
			TOKEN_OUTPUT_SUBDIR,
			true,
			ProtocolVersion(1),
			header,
		)?;
		let token_rproof_pmmr_h = PMMRHandle::new(
			&root_dir,
			TXHASHSET_SUBDIR,
			TOKEN_RANGE_PROOF_SUBDIR,
			true,
			ProtocolVersion(1),
			header,
		)?;
		let token_issue_proof_pmmr_h = PMMRHandle::new(
			&root_dir,
			TXHASHSET_SUBDIR,
			TOKEN_ISSUE_PROOF_SUBDIR,
			false,
			ProtocolVersion(1),
			header,
		)?;
		let token_kernel_pmmr_h = PMMRHandle::new(
			&root_dir,
			TXHASHSET_SUBDIR,
			TOKEN_KERNEL_SUBDIR,
			false, // not prunable
			ProtocolVersion(1),
			None,
		)?;

		// Initialize the bitmap accumulator from the current output PMMR.
		let bitmap_accumulator = TxHashSet::bitmap_accumulator(&output_pmmr_h)?;

		let mut maybe_kernel_handle: Option<PMMRHandle<TxKernel>> = None;
		let versions = vec![ProtocolVersion(2), ProtocolVersion(1)];
		for version in versions {
			let handle = PMMRHandle::new(
				&root_dir,
				TXHASHSET_SUBDIR,
				KERNEL_SUBDIR,
				false, // not prunable
				version,
				None,
			)?;
			if handle.last_pos == 0 {
				debug!(
					"attempting to open (empty) kernel PMMR using {:?} - SUCCESS",
					version
				);
				maybe_kernel_handle = Some(handle);
				break;
			}
			let kernel: Option<TxKernel> = ReadonlyPMMR::at(&handle.backend, 1).get_data(1);
			if let Some(kernel) = kernel {
				if kernel.verify().is_ok() {
					debug!(
						"attempting to open kernel PMMR using {:?} - SUCCESS",
						version
					);
					maybe_kernel_handle = Some(handle);
					break;
				} else {
					debug!(
						"attempting to open kernel PMMR using {:?} - FAIL (verify failed)",
						version
					);
				}
			} else {
				debug!(
					"attempting to open kernel PMMR using {:?} - FAIL (read failed)",
					version
				);
			}
		}
		if let Some(kernel_pmmr_h) = maybe_kernel_handle {
			Ok(TxHashSet {
				output_pmmr_h,
				rproof_pmmr_h,
				kernel_pmmr_h,
				token_output_pmmr_h,
				token_rproof_pmmr_h,
				token_issue_proof_pmmr_h,
				token_kernel_pmmr_h,
				bitmap_accumulator,
				commit_index,
			})
		} else {
			Err(ErrorKind::TxHashSetErr("failed to open kernel PMMR".to_string()).into())
		}
	}

	// Build a new bitmap accumulator for the provided output PMMR.
	fn bitmap_accumulator(pmmr_h: &PMMRHandle<Output>) -> Result<BitmapAccumulator, Error> {
		let pmmr = ReadonlyPMMR::at(&pmmr_h.backend, pmmr_h.last_pos);
		let size = pmmr::n_leaves(pmmr_h.last_pos);
		let mut bitmap_accumulator = BitmapAccumulator::new();
		bitmap_accumulator.init(&mut pmmr.leaf_idx_iter(0), size)?;
		Ok(bitmap_accumulator)
	}

	/// Close all backend file handles
	pub fn release_backend_files(&mut self) {
		self.output_pmmr_h.backend.release_files();
		self.rproof_pmmr_h.backend.release_files();
		self.kernel_pmmr_h.backend.release_files();
		self.token_output_pmmr_h.backend.release_files();
		self.token_rproof_pmmr_h.backend.release_files();
		self.token_issue_proof_pmmr_h.backend.release_files();
		self.token_kernel_pmmr_h.backend.release_files();
	}

	/// Check if an output is unspent.
	/// We look in the index to find the output MMR pos.
	/// Then we check the entry in the output MMR and confirm the hash matches.
	pub fn get_unspent(&self, output_id: &OutputIdentifier) -> Result<Option<CommitPos>, Error> {
		let commit = output_id.commit;
		match self.commit_index.get_output_pos_height(&commit) {
			Ok(Some((pos, height))) => {
				let output_pmmr: ReadonlyPMMR<'_, Output, _> =
					ReadonlyPMMR::at(&self.output_pmmr_h.backend, self.output_pmmr_h.last_pos);
				if let Some(out) = output_pmmr.get_data(pos) {
					if OutputIdentifier::from(out) == *output_id {
						Ok(Some(CommitPos { pos, height }))
					} else {
						Ok(None)
					}
				} else {
					Ok(None)
				}
			}
			Ok(None) => Ok(None),
			Err(e) => Err(ErrorKind::StoreErr(e, "txhashset unspent check".to_string()).into()),
		}
	}

	/// Check if an token output is unspent.
	/// We look in the index to find the token output MMR pos.
	/// Then we check the entry in the token output MMR and confirm the hash matches.
	pub fn get_token_unspent(
		&self,
		output_id: &TokenOutputIdentifier,
	) -> Result<Option<CommitPos>, Error> {
		let commit = output_id.commit;
		match self.commit_index.get_token_output_pos_height(&commit) {
			Ok(Some((pos, height))) => {
				let output_pmmr: ReadonlyPMMR<'_, TokenOutput, _> = ReadonlyPMMR::at(
					&self.token_output_pmmr_h.backend,
					self.token_output_pmmr_h.last_pos,
				);
				if let Some(out) = output_pmmr.get_data(pos) {
					if TokenOutputIdentifier::from(out) == *output_id {
						Ok(Some(CommitPos { pos, height }))
					} else {
						Ok(None)
					}
				} else {
					Ok(None)
				}
			}
			Ok(None) => Ok(None),
			Err(e) => Err(ErrorKind::StoreErr(e, "txhashset unspent check".to_string()).into()),
		}
	}

	/// returns the last N nodes inserted into the tree (i.e. the 'bottom'
	/// nodes at level 0
	/// TODO: These need to return the actual data from the flat-files instead
	/// of hashes now
	pub fn last_n_output(&self, distance: u64) -> Vec<(Hash, OutputIdentifier)> {
		ReadonlyPMMR::at(&self.output_pmmr_h.backend, self.output_pmmr_h.last_pos)
			.get_last_n_insertions(distance)
	}

	/// returns the last N nodes inserted into the tree (i.e. the 'bottom'
	/// nodes at level 0
	/// TODO: These need to return the actual data from the flat-files instead
	/// of hashes now
	pub fn last_n_token_output(&self, distance: u64) -> Vec<(Hash, TokenOutputIdentifier)> {
		ReadonlyPMMR::at(
			&self.token_output_pmmr_h.backend,
			self.token_output_pmmr_h.last_pos,
		)
		.get_last_n_insertions(distance)
	}

	/// as above, for range proofs
	pub fn last_n_rangeproof(&self, distance: u64) -> Vec<(Hash, RangeProof)> {
		ReadonlyPMMR::at(&self.rproof_pmmr_h.backend, self.rproof_pmmr_h.last_pos)
			.get_last_n_insertions(distance)
	}

	/// as above, for token range proofs
	pub fn last_n_token_rangeproof(&self, distance: u64) -> Vec<(Hash, RangeProof)> {
		ReadonlyPMMR::at(
			&self.token_rproof_pmmr_h.backend,
			self.token_rproof_pmmr_h.last_pos,
		)
		.get_last_n_insertions(distance)
	}

	/// as above, for kernels
	pub fn last_n_kernel(&self, distance: u64) -> Vec<(Hash, TxKernel)> {
		ReadonlyPMMR::at(&self.kernel_pmmr_h.backend, self.kernel_pmmr_h.last_pos)
			.get_last_n_insertions(distance)
	}

	/// as above, for token issue proof
	pub fn last_n_token_issue_proof(&self, distance: u64) -> Vec<(Hash, TokenIssueProof)> {
		ReadonlyPMMR::at(
			&self.token_issue_proof_pmmr_h.backend,
			self.token_issue_proof_pmmr_h.last_pos,
		)
		.get_last_n_insertions(distance)
	}

	/// Convenience function to query the db for a header by its hash.
	pub fn get_block_header(&self, hash: &Hash) -> Result<BlockHeader, Error> {
		Ok(self.commit_index.get_block_header(&hash)?)
	}

	/// returns outputs from the given pmmr index up to the
	/// specified limit. Also returns the last index actually populated
	/// max index is the last PMMR index to consider, not leaf index
	pub fn outputs_by_pmmr_index(
		&self,
		start_index: u64,
		max_count: u64,
		max_index: Option<u64>,
	) -> (u64, Vec<OutputIdentifier>) {
		ReadonlyPMMR::at(&self.output_pmmr_h.backend, self.output_pmmr_h.last_pos)
			.elements_from_pmmr_index(start_index, max_count, max_index)
	}

	/// returns outputs from the given insertion (leaf) index up to the
	/// specified limit. Also returns the last index actually populated
	pub fn token_outputs_by_pmmr_index(
		&self,
		start_index: u64,
		max_count: u64,
		max_index: Option<u64>,
	) -> (u64, Vec<TokenOutputIdentifier>) {
		ReadonlyPMMR::at(
			&self.token_output_pmmr_h.backend,
			self.token_output_pmmr_h.last_pos,
		)
		.elements_from_pmmr_index(start_index, max_count, max_index)
	}

	/// highest output insertion index available
	pub fn highest_output_insertion_index(&self) -> u64 {
		self.output_pmmr_h.last_pos
	}

	/// highest token output insertion index available
	pub fn highest_token_output_insertion_index(&self) -> u64 {
		self.token_output_pmmr_h.last_pos
	}

	/// As above, for rangeproofs
	pub fn rangeproofs_by_pmmr_index(
		&self,
		start_index: u64,
		max_count: u64,
		max_index: Option<u64>,
	) -> (u64, Vec<RangeProof>) {
		ReadonlyPMMR::at(&self.rproof_pmmr_h.backend, self.rproof_pmmr_h.last_pos)
			.elements_from_pmmr_index(start_index, max_count, max_index)
	}

	/// As above, for rangeproofs
	pub fn token_rangeproofs_by_pmmr_index(
		&self,
		start_index: u64,
		max_count: u64,
		max_index: Option<u64>,
	) -> (u64, Vec<RangeProof>) {
		ReadonlyPMMR::at(
			&self.token_rproof_pmmr_h.backend,
			self.token_rproof_pmmr_h.last_pos,
		)
		.elements_from_pmmr_index(start_index, max_count, max_index)
	}

	/// Find a kernel with a given excess. Work backwards from `max_index` to `min_index`
	pub fn find_kernel(
		&self,
		excess: &Commitment,
		min_index: Option<u64>,
		max_index: Option<u64>,
	) -> Option<(TxKernel, u64)> {
		let min_index = min_index.unwrap_or(1);
		let max_index = max_index.unwrap_or(self.kernel_pmmr_h.last_pos);

		let pmmr = ReadonlyPMMR::at(&self.kernel_pmmr_h.backend, self.kernel_pmmr_h.last_pos);
		let mut index = max_index + 1;
		while index > min_index {
			index -= 1;
			if let Some(kernel) = pmmr.get_data(index) {
				if &kernel.excess == excess {
					return Some((kernel, index));
				}
			}
		}
		None
	}

	/// Find a token kernel with a given excess. Work backwards from `max_index` to `min_index`
	pub fn find_token_kernel(
		&self,
		excess: &Commitment,
		min_index: Option<u64>,
		max_index: Option<u64>,
	) -> Option<(TokenTxKernel, u64)> {
		let min_index = min_index.unwrap_or(1);
		let max_index = max_index.unwrap_or(self.token_kernel_pmmr_h.last_pos);

		let pmmr = ReadonlyPMMR::at(
			&self.token_kernel_pmmr_h.backend,
			self.token_kernel_pmmr_h.last_pos,
		);
		let mut index = max_index + 1;
		while index > min_index {
			index -= 1;
			if let Some(kernel) = pmmr.get_data(index) {
				if &kernel.excess == excess {
					return Some((kernel, index));
				}
			}
		}
		None
	}

	/// Get MMR roots.
	pub fn roots(&self) -> TxHashSetRoots {
		let output_pmmr =
			ReadonlyPMMR::at(&self.output_pmmr_h.backend, self.output_pmmr_h.last_pos);
		let rproof_pmmr =
			ReadonlyPMMR::at(&self.rproof_pmmr_h.backend, self.rproof_pmmr_h.last_pos);
		let kernel_pmmr =
			ReadonlyPMMR::at(&self.kernel_pmmr_h.backend, self.kernel_pmmr_h.last_pos);

		let token_output_pmmr = ReadonlyPMMR::at(
			&self.token_output_pmmr_h.backend,
			self.token_output_pmmr_h.last_pos,
		);
		let token_rproof_pmmr = ReadonlyPMMR::at(
			&self.token_rproof_pmmr_h.backend,
			self.token_rproof_pmmr_h.last_pos,
		);
		let token_issue_proof_pmmr = ReadonlyPMMR::at(
			&self.token_issue_proof_pmmr_h.backend,
			self.token_issue_proof_pmmr_h.last_pos,
		);
		let token_kernel_pmmr = ReadonlyPMMR::at(
			&self.token_kernel_pmmr_h.backend,
			self.token_kernel_pmmr_h.last_pos,
		);

		TxHashSetRoots {
			output_roots: OutputRoots {
				pmmr_root: output_pmmr.root(),
				bitmap_root: self.bitmap_accumulator.root(),
			},
			rproof_root: rproof_pmmr.root(),
			kernel_root: kernel_pmmr.root(),
			token_output_root: token_output_pmmr.root(),
			token_rproof_root: token_rproof_pmmr.root(),
			token_issue_proof_root: token_issue_proof_pmmr.root(),
			token_kernel_root: token_kernel_pmmr.root(),
		}
	}

	/// Return Commit's MMR position
	pub fn get_output_pos(&self, commit: &Commitment) -> Result<u64, Error> {
		Ok(self.commit_index.get_output_pos(&commit)?)
	}

	/// Return Commit's MMR position
	pub fn get_token_output_pos(&self, commit: &Commitment) -> Result<u64, Error> {
		Ok(self.commit_index.get_token_output_pos(&commit)?)
	}

	/// build a new merkle proof for the given position.
	pub fn merkle_proof(&mut self, commit: Commitment) -> Result<MerkleProof, Error> {
		let pos = self.commit_index.get_output_pos(&commit)?;
		PMMR::at(&mut self.output_pmmr_h.backend, self.output_pmmr_h.last_pos)
			.merkle_proof(pos)
			.map_err(|_| ErrorKind::MerkleProof.into())
	}

	/// build a new merkle proof for the given position.
	pub fn token_merkle_proof(&mut self, commit: Commitment) -> Result<MerkleProof, Error> {
		let pos = self.commit_index.get_token_output_pos(&commit)?;
		PMMR::at(
			&mut self.token_output_pmmr_h.backend,
			self.token_output_pmmr_h.last_pos,
		)
		.merkle_proof(pos)
		.map_err(|_| ErrorKind::MerkleProof.into())
	}

	/// Compact the MMR data files and flush the rm logs
	pub fn compact(
		&mut self,
		horizon_header: &BlockHeader,
		batch: &Batch<'_>,
	) -> Result<(), Error> {
		debug!("txhashset: starting compaction...");

		let head_header = batch.head_header()?;

		let rewind_rm_pos = input_pos_to_rewind(&horizon_header, &head_header, batch)?;
		let token_rewind_rm_pos = token_input_pos_to_rewind(&horizon_header, &head_header, batch)?;

		debug!("txhashset: check_compact output mmr backend...");
		self.output_pmmr_h
			.backend
			.check_compact(horizon_header.output_mmr_size, &rewind_rm_pos)?;

		debug!("txhashset: check_compact rangeproof mmr backend...");
		self.rproof_pmmr_h
			.backend
			.check_compact(horizon_header.output_mmr_size, &rewind_rm_pos)?;

		debug!("txhashset: check_compact token_output mmr backend...");
		self.token_output_pmmr_h
			.backend
			.check_compact(horizon_header.token_output_mmr_size, &token_rewind_rm_pos)?;

		debug!("txhashset: check_compact token_rangeproof mmr backend...");
		self.token_rproof_pmmr_h.backend.check_compact(
			horizon_header.token_issue_proof_mmr_size,
			&token_rewind_rm_pos,
		)?;

		debug!("txhashset: ... compaction finished");

		Ok(())
	}

	/// (Re)build the output_pos index to be consistent with the current UTXO set.
	/// Remove any "stale" index entries that do not correspond to outputs in the UTXO set.
	/// Add any missing index entries based on UTXO set.
	pub fn init_output_pos_index(
		&self,
		header_pmmr: &PMMRHandle<BlockHeader>,
		batch: &Batch<'_>,
	) -> Result<(), Error> {
		let now = Instant::now();

		let output_pmmr =
			ReadonlyPMMR::at(&self.output_pmmr_h.backend, self.output_pmmr_h.last_pos);

		// Iterate over the current output_pos index, removing any entries that
		// do not point to to the expected output.
		let mut removed_count = 0;
		for (key, (pos, _)) in batch.output_pos_iter()? {
			if let Some(out) = output_pmmr.get_data(pos) {
				if let Ok(pos_via_mmr) = batch.get_output_pos(&out.commitment()) {
					// If the pos matches and the index key matches the commitment
					// then keep the entry, other we want to clean it up.
					if pos == pos_via_mmr && batch.is_match_output_pos_key(&key, &out.commitment())
					{
						continue;
					}
				}
			}
			batch.delete(&key)?;
			removed_count += 1;
		}
		debug!(
			"init_output_pos_index: removed {} stale index entries",
			removed_count
		);

		let mut outputs_pos: Vec<(Commitment, u64)> = vec![];
		for pos in output_pmmr.leaf_pos_iter() {
			if let Some(out) = output_pmmr.get_data(pos) {
				outputs_pos.push((out.commit, pos));
			}
		}

		debug!("init_output_pos_index: {} utxos", outputs_pos.len());

		outputs_pos.retain(|x| {
			batch
				.get_output_pos_height(&x.0)
				.map(|p| p.is_none())
				.unwrap_or(true)
		});

		debug!(
			"init_output_pos_index: {} utxos with missing index entries",
			outputs_pos.len()
		);

		if outputs_pos.is_empty() {
			return Ok(());
		}

		let total_outputs = outputs_pos.len();
		let max_height = batch.head()?.height;

		let mut i = 0;
		for search_height in 0..max_height {
			let hash = header_pmmr.get_header_hash_by_height(search_height + 1)?;
			let h = batch.get_block_header(&hash)?;
			while i < total_outputs {
				let (commit, pos) = outputs_pos[i];
				if pos > h.output_mmr_size {
					// Note: MMR position is 1-based and not 0-based, so here must be '>' instead of '>='
					break;
				}
				batch.save_output_pos_height(&commit, pos, h.height)?;
				i += 1;
			}
		}
		debug!(
			"init_height_pos_index: added entries for {} utxos, took {}s",
			total_outputs,
			now.elapsed().as_secs(),
		);
		Ok(())
	}

	/// (Re)build the token output_pos index to be consistent with the current UTXO set.
	/// Remove any "stale" index entries that do not correspond to outputs in the UTXO set.
	/// Add any missing index entries based on UTXO set.
	pub fn init_token_output_pos_index(
		&self,
		header_pmmr: &PMMRHandle<BlockHeader>,
		batch: &Batch<'_>,
	) -> Result<(), Error> {
		let now = Instant::now();

		let output_pmmr = ReadonlyPMMR::at(
			&self.token_output_pmmr_h.backend,
			self.token_output_pmmr_h.last_pos,
		);

		// Iterate over the current output_pos index, removing any entries that
		// do not point to to the expected output.
		let mut removed_count = 0;
		for (key, (pos, _)) in batch.token_output_pos_iter()? {
			if let Some(out) = output_pmmr.get_data(pos) {
				if let Ok(pos_via_mmr) = batch.get_token_output_pos(&out.commitment()) {
					// If the pos matches and the index key matches the commitment
					// then keep the entry, other we want to clean it up.
					if pos == pos_via_mmr
						&& batch.is_match_token_output_pos_key(&key, &out.commitment())
					{
						continue;
					}
				}
			}
			batch.delete(&key)?;
			removed_count += 1;
		}
		debug!(
			"init_token_output_pos_index: removed {} stale index entries",
			removed_count
		);

		let mut outputs_pos: Vec<(Commitment, u64)> = vec![];
		for pos in output_pmmr.leaf_pos_iter() {
			if let Some(out) = output_pmmr.get_data(pos) {
				outputs_pos.push((out.commit, pos));
			}
		}

		debug!("init_token_output_pos_index: {} utxos", outputs_pos.len());

		outputs_pos.retain(|x| {
			batch
				.get_token_output_pos_height(&x.0)
				.map(|p| p.is_none())
				.unwrap_or(true)
		});

		debug!(
			"init_token_output_pos_index: {} utxos with missing index entries",
			outputs_pos.len()
		);

		if outputs_pos.is_empty() {
			return Ok(());
		}

		let total_outputs = outputs_pos.len();
		let max_height = batch.head()?.height;

		let mut i = 0;
		for search_height in 0..max_height {
			let hash = header_pmmr.get_header_hash_by_height(search_height + 1)?;
			let h = batch.get_block_header(&hash)?;
			while i < total_outputs {
				let (commit, pos) = outputs_pos[i];
				if pos > h.token_output_mmr_size {
					// Note: MMR position is 1-based and not 0-based, so here must be '>' instead of '>='
					break;
				}
				batch.save_token_output_pos_height(&commit, pos, h.height)?;
				i += 1;
			}
		}
		debug!(
			"init_token_output_pos_index: added entries for {} utxos, took {}s",
			total_outputs,
			now.elapsed().as_secs(),
		);
		Ok(())
	}
}

/// Starts a new unit of work to extend (or rewind) the chain with additional
/// blocks. Accepts a closure that will operate within that unit of work.
/// The closure has access to an Extension object that allows the addition
/// of blocks to the txhashset and the checking of the current tree roots.
///
/// The unit of work is always discarded (always rollback) as this is read-only.
pub fn extending_readonly<F, T>(
	handle: &mut PMMRHandle<BlockHeader>,
	trees: &mut TxHashSet,
	inner: F,
) -> Result<T, Error>
where
	F: FnOnce(&mut ExtensionPair<'_>, &Batch<'_>) -> Result<T, Error>,
{
	let commit_index = trees.commit_index.clone();
	let batch = commit_index.batch()?;

	trace!("Starting new txhashset (readonly) extension.");

	let head = batch.head()?;

	// Find header head based on current header MMR (the rightmost leaf node in the MMR).
	let header_head = {
		let hash = handle.head_hash()?;
		let header = batch.get_block_header(&hash)?;
		Tip::from_header(&header)
	};

	let res = {
		let header_pmmr = PMMR::at(&mut handle.backend, handle.last_pos);
		let mut header_extension = HeaderExtension::new(header_pmmr, header_head);
		let mut extension = Extension::new(trees, head);
		let mut extension_pair = ExtensionPair {
			header_extension: &mut header_extension,
			extension: &mut extension,
		};
		inner(&mut extension_pair, &batch)
	};

	trace!("Rollbacking txhashset (readonly) extension.");

	handle.backend.discard();

	trees.output_pmmr_h.backend.discard();
	trees.rproof_pmmr_h.backend.discard();
	trees.kernel_pmmr_h.backend.discard();

	trees.token_output_pmmr_h.backend.discard();
	trees.token_rproof_pmmr_h.backend.discard();
	trees.token_issue_proof_pmmr_h.backend.discard();
	trees.token_kernel_pmmr_h.backend.discard();

	trace!("TxHashSet (readonly) extension done.");

	res
}

/// Readonly view on the UTXO set.
/// Based on the current txhashset output_pmmr.
pub fn utxo_view<F, T>(
	handle: &PMMRHandle<BlockHeader>,
	trees: &TxHashSet,
	inner: F,
) -> Result<T, Error>
where
	F: FnOnce(&UTXOView<'_>, &Batch<'_>) -> Result<T, Error>,
{
	let res: Result<T, Error>;
	{
		let header_pmmr = ReadonlyPMMR::at(&handle.backend, handle.last_pos);
		let output_pmmr =
			ReadonlyPMMR::at(&trees.output_pmmr_h.backend, trees.output_pmmr_h.last_pos);
		let token_output_pmmr = ReadonlyPMMR::at(
			&trees.token_output_pmmr_h.backend,
			trees.token_output_pmmr_h.last_pos,
		);
		let token_issue_proof_pmmr = ReadonlyPMMR::at(
			&trees.token_issue_proof_pmmr_h.backend,
			trees.token_issue_proof_pmmr_h.last_pos,
		);
		let rproof_pmmr =
			ReadonlyPMMR::at(&trees.rproof_pmmr_h.backend, trees.rproof_pmmr_h.last_pos);

		let token_rproof_pmmr = ReadonlyPMMR::at(
			&trees.token_rproof_pmmr_h.backend,
			trees.token_rproof_pmmr_h.last_pos,
		);

		// Create a new batch here to pass into the utxo_view.
		// Discard it (rollback) after we finish with the utxo_view.
		let batch = trees.commit_index.batch()?;
		let utxo = UTXOView::new(
			header_pmmr,
			output_pmmr,
			token_output_pmmr,
			token_issue_proof_pmmr,
			rproof_pmmr,
			token_rproof_pmmr,
		);
		res = inner(&utxo, &batch);
	}
	res
}

/// Rewindable (but still readonly) view on the kernel MMR.
/// The underlying backend is readonly. But we permit the PMMR to be "rewound"
/// via last_pos.
/// We create a new db batch for this view and discard it (rollback)
/// when we are done with the view.
pub fn rewindable_kernel_view<F, T>(trees: &TxHashSet, inner: F) -> Result<T, Error>
where
	F: FnOnce(&mut RewindableKernelView<'_>, &Batch<'_>) -> Result<T, Error>,
{
	let res: Result<T, Error>;
	{
		let kernel_pmmr =
			RewindablePMMR::at(&trees.kernel_pmmr_h.backend, trees.kernel_pmmr_h.last_pos);

		let token_kernel_pmmr = RewindablePMMR::at(
			&trees.token_kernel_pmmr_h.backend,
			trees.token_kernel_pmmr_h.last_pos,
		);

		// Create a new batch here to pass into the kernel_view.
		// Discard it (rollback) after we finish with the kernel_view.
		let batch = trees.commit_index.batch()?;
		let header = batch.head_header()?;
		let mut view = RewindableKernelView::new(kernel_pmmr, token_kernel_pmmr, header);
		res = inner(&mut view, &batch);
	}
	res
}

/// Starts a new unit of work to extend the chain with additional blocks,
/// accepting a closure that will work within that unit of work. The closure
/// has access to an Extension object that allows the addition of blocks to
/// the txhashset and the checking of the current tree roots.
///
/// If the closure returns an error, modifications are canceled and the unit
/// of work is abandoned. Otherwise, the unit of work is permanently applied.
pub fn extending<'a, F, T>(
	header_pmmr: &'a mut PMMRHandle<BlockHeader>,
	trees: &'a mut TxHashSet,
	batch: &'a mut Batch<'_>,
	inner: F,
) -> Result<T, Error>
where
	F: FnOnce(&mut ExtensionPair<'_>, &Batch<'_>) -> Result<T, Error>,
{
	let sizes: (u64, u64, u64, u64, u64, u64, u64);
	let res: Result<T, Error>;
	let rollback: bool;
	let bitmap_accumulator: BitmapAccumulator;

	let head = batch.head()?;

	// Find header head based on current header MMR (the rightmost leaf node in the MMR).
	let header_head = {
		let hash = header_pmmr.head_hash()?;
		let header = batch.get_block_header(&hash)?;
		Tip::from_header(&header)
	};

	// create a child transaction so if the state is rolled back by itself, all
	// index saving can be undone
	let child_batch = batch.child()?;
	{
		trace!("Starting new txhashset extension.");

		let header_pmmr = PMMR::at(&mut header_pmmr.backend, header_pmmr.last_pos);
		let mut header_extension = HeaderExtension::new(header_pmmr, header_head);
		let mut extension = Extension::new(trees, head);
		let mut extension_pair = ExtensionPair {
			header_extension: &mut header_extension,
			extension: &mut extension,
		};
		res = inner(&mut extension_pair, &child_batch);

		rollback = extension_pair.extension.rollback;
		sizes = extension_pair.extension.sizes();
		bitmap_accumulator = extension_pair.extension.bitmap_accumulator.clone();
	}

	// During an extension we do not want to modify the header_extension (and only read from it).
	// So make sure we discard any changes to the header MMR backed.
	header_pmmr.backend.discard();

	match res {
		Err(e) => {
			debug!("Error returned, discarding txhashset extension: {}", e);
			trees.output_pmmr_h.backend.discard();
			trees.rproof_pmmr_h.backend.discard();
			trees.kernel_pmmr_h.backend.discard();
			trees.token_output_pmmr_h.backend.discard();
			trees.token_rproof_pmmr_h.backend.discard();
			trees.token_issue_proof_pmmr_h.backend.discard();
			trees.token_kernel_pmmr_h.backend.discard();
			Err(e)
		}
		Ok(r) => {
			if rollback {
				trace!("Rollbacking txhashset extension. sizes {:?}", sizes);
				trees.output_pmmr_h.backend.discard();
				trees.rproof_pmmr_h.backend.discard();
				trees.kernel_pmmr_h.backend.discard();
				trees.token_output_pmmr_h.backend.discard();
				trees.token_rproof_pmmr_h.backend.discard();
				trees.token_issue_proof_pmmr_h.backend.discard();
				trees.token_kernel_pmmr_h.backend.discard();
			} else {
				trace!("Committing txhashset extension. sizes {:?}", sizes);
				child_batch.commit()?;
				trees.output_pmmr_h.backend.sync()?;
				trees.rproof_pmmr_h.backend.sync()?;
				trees.kernel_pmmr_h.backend.sync()?;
				trees.output_pmmr_h.last_pos = sizes.0;
				trees.rproof_pmmr_h.last_pos = sizes.1;
				trees.kernel_pmmr_h.last_pos = sizes.2;
				trees.token_output_pmmr_h.backend.sync()?;
				trees.token_rproof_pmmr_h.backend.sync()?;
				trees.token_issue_proof_pmmr_h.backend.sync()?;
				trees.token_kernel_pmmr_h.backend.sync()?;
				trees.output_pmmr_h.last_pos = sizes.0;
				trees.rproof_pmmr_h.last_pos = sizes.1;
				trees.kernel_pmmr_h.last_pos = sizes.2;
				trees.token_output_pmmr_h.last_pos = sizes.3;
				trees.token_rproof_pmmr_h.last_pos = sizes.4;
				trees.token_issue_proof_pmmr_h.last_pos = sizes.5;
				trees.token_kernel_pmmr_h.last_pos = sizes.6;

				// Update our bitmap_accumulator based on our extension
				trees.bitmap_accumulator = bitmap_accumulator;
			}

			trace!("TxHashSet extension done.");
			Ok(r)
		}
	}
}

/// Start a new header MMR unit of work.
/// This MMR can be extended individually beyond the other (output, rangeproof and kernel) MMRs
/// to allow headers to be validated before we receive the full block data.
pub fn header_extending<'a, F, T>(
	handle: &'a mut PMMRHandle<BlockHeader>,
	batch: &'a mut Batch<'_>,
	inner: F,
) -> Result<T, Error>
where
	F: FnOnce(&mut HeaderExtension<'_>, &Batch<'_>) -> Result<T, Error>,
{
	let size: u64;
	let res: Result<T, Error>;
	let rollback: bool;

	// create a child transaction so if the state is rolled back by itself, all
	// index saving can be undone
	let child_batch = batch.child()?;

	// Find chain head based on current MMR (the rightmost leaf node in the MMR).
	let head = match handle.head_hash() {
		Ok(hash) => {
			let header = child_batch.get_block_header(&hash)?;
			Tip::from_header(&header)
		}
		Err(_) => Tip::default(),
	};

	{
		let pmmr = PMMR::at(&mut handle.backend, handle.last_pos);
		let mut extension = HeaderExtension::new(pmmr, head);
		res = inner(&mut extension, &child_batch);

		rollback = extension.rollback;
		size = extension.size();
	}

	match res {
		Err(e) => {
			handle.backend.discard();
			Err(e)
		}
		Ok(r) => {
			if rollback {
				handle.backend.discard();
			} else {
				child_batch.commit()?;
				handle.backend.sync()?;
				handle.last_pos = size;
			}
			Ok(r)
		}
	}
}

/// A header extension to allow the header MMR to extend beyond the other MMRs individually.
/// This is to allow headers to be validated against the MMR before we have the full block data.
pub struct HeaderExtension<'a> {
	head: Tip,

	pmmr: PMMR<'a, BlockHeader, PMMRBackend<BlockHeader>>,

	/// Rollback flag.
	rollback: bool,
}

impl<'a> HeaderExtension<'a> {
	fn new(
		pmmr: PMMR<'a, BlockHeader, PMMRBackend<BlockHeader>>,
		head: Tip,
	) -> HeaderExtension<'a> {
		HeaderExtension {
			head,
			pmmr,
			rollback: false,
		}
	}

	/// Get the header hash for the specified pos from the underlying MMR backend.
	fn get_header_hash(&self, pos: u64) -> Option<Hash> {
		self.pmmr.get_data(pos).map(|x| x.hash())
	}

	/// The head representing the furthest extent of the current extension.
	pub fn head(&self) -> Tip {
		self.head.clone()
	}

	/// Get the header at the specified height based on the current state of the header extension.
	/// Derives the MMR pos from the height (insertion index) and retrieves the header hash.
	/// Looks the header up in the db by hash.
	pub fn get_header_by_height(
		&self,
		height: u64,
		batch: &Batch<'_>,
	) -> Result<BlockHeader, Error> {
		let pos = pmmr::insertion_to_pmmr_index(height + 1);
		if let Some(hash) = self.get_header_hash(pos) {
			Ok(batch.get_block_header(&hash)?)
		} else {
			Err(ErrorKind::Other("get header by height".to_string()).into())
		}
	}

	/// Compares the provided header to the header in the header MMR at that height.
	/// If these match we know the header is on the current chain.
	pub fn is_on_current_chain(
		&self,
		header: &BlockHeader,
		batch: &Batch<'_>,
	) -> Result<(), Error> {
		if header.height > self.head.height {
			return Err(ErrorKind::Other("not on current chain, out beyond".to_string()).into());
		}
		let chain_header = self.get_header_by_height(header.height, batch)?;
		if chain_header.hash() == header.hash() {
			Ok(())
		} else {
			Err(ErrorKind::Other("not on current chain".to_string()).into())
		}
	}

	/// Force the rollback of this extension, no matter the result.
	pub fn force_rollback(&mut self) {
		self.rollback = true;
	}

	/// Apply a new header to the header MMR extension.
	/// This may be either the header MMR or the sync MMR depending on the
	/// extension.
	pub fn apply_header(&mut self, header: &BlockHeader) -> Result<(), Error> {
		self.pmmr.push(header).map_err(&ErrorKind::TxHashSetErr)?;
		self.head = Tip::from_header(header);
		Ok(())
	}

	/// Rewind the header extension to the specified header.
	/// Note the close relationship between header height and insertion index.
	pub fn rewind(&mut self, header: &BlockHeader) -> Result<(), Error> {
		debug!(
			"Rewind header extension to {} at {} from {} at {}",
			header.hash(),
			header.height,
			self.head.hash(),
			self.head.height,
		);

		let header_pos = pmmr::insertion_to_pmmr_index(header.height + 1);
		self.pmmr
			.rewind(header_pos, &Bitmap::create())
			.map_err(&ErrorKind::TxHashSetErr)?;

		// Update our head to reflect the header we rewound to.
		self.head = Tip::from_header(header);

		Ok(())
	}

	/// The size of the header MMR.
	pub fn size(&self) -> u64 {
		self.pmmr.unpruned_size()
	}

	/// The root of the header MMR for convenience.
	pub fn root(&self) -> Result<Hash, Error> {
		Ok(self.pmmr.root().map_err(|_| ErrorKind::InvalidRoot)?)
	}

	/// Validate the prev_root of the header against the root of the current header MMR.
	pub fn validate_root(&self, header: &BlockHeader) -> Result<(), Error> {
		// If we are validating the genesis block then we have no prev_root.
		// So we are done here.
		if header.height == 0 {
			return Ok(());
		}
		if self.root()? != header.prev_root {
			Err(ErrorKind::InvalidRoot.into())
		} else {
			Ok(())
		}
	}
}

/// An extension "pair" consisting of a txhashet extension (outputs, rangeproofs, kernels)
/// and the associated header extension.
pub struct ExtensionPair<'a> {
	/// The header extension.
	pub header_extension: &'a mut HeaderExtension<'a>,
	/// The txhashset extension.
	pub extension: &'a mut Extension<'a>,
}

/// Allows the application of new blocks on top of the txhashset in a
/// reversible manner within a unit of work provided by the `extending`
/// function.
pub struct Extension<'a> {
	head: Tip,

	output_pmmr: PMMR<'a, Output, PMMRBackend<Output>>,
	rproof_pmmr: PMMR<'a, RangeProof, PMMRBackend<RangeProof>>,
	kernel_pmmr: PMMR<'a, TxKernel, PMMRBackend<TxKernel>>,

	token_output_pmmr: PMMR<'a, TokenOutput, PMMRBackend<TokenOutput>>,
	token_rproof_pmmr: PMMR<'a, RangeProof, PMMRBackend<RangeProof>>,
	token_issue_proof_pmmr: PMMR<'a, TokenIssueProof, PMMRBackend<TokenIssueProof>>,
	token_kernel_pmmr: PMMR<'a, TokenTxKernel, PMMRBackend<TokenTxKernel>>,

	bitmap_accumulator: BitmapAccumulator,

	/// Rollback flag.
	rollback: bool,
}

impl<'a> Committed for Extension<'a> {
	fn inputs_committed(&self) -> Vec<Commitment> {
		vec![]
	}

	fn outputs_committed(&self) -> Vec<Commitment> {
		let mut commitments = vec![];
		for pos in self.output_pmmr.leaf_pos_iter() {
			if let Some(out) = self.output_pmmr.get_data(pos) {
				commitments.push(out.commit);
			}
		}
		commitments
	}

	fn kernels_committed(&self) -> Vec<Commitment> {
		let mut commitments = vec![];
		for n in 1..self.kernel_pmmr.unpruned_size() + 1 {
			if pmmr::is_leaf(n) {
				if let Some(kernel) = self.kernel_pmmr.get_data(n) {
					commitments.push(kernel.excess());
				}
			}
		}
		commitments
	}

	fn token_inputs_committed(&self) -> HashMap<TokenKey, Vec<Commitment>> {
		let mut token_inputs_map: HashMap<TokenKey, Vec<Commitment>> = HashMap::new();
		for n in 1..self.token_issue_proof_pmmr.unpruned_size() + 1 {
			if pmmr::is_leaf(n) {
				if let Some(issue_proof) = self.token_issue_proof_pmmr.get_data(n) {
					let commit_vec = token_inputs_map
						.entry(issue_proof.token_type)
						.or_insert(vec![]);
					commit_vec.push(issue_proof.commitment());
				}
			}
		}
		token_inputs_map
	}

	fn token_outputs_committed(&self) -> HashMap<TokenKey, Vec<Commitment>> {
		let mut token_outputs_map: HashMap<TokenKey, Vec<Commitment>> = HashMap::new();
		for pos in self.token_output_pmmr.leaf_pos_iter() {
			if let Some(out) = self.token_output_pmmr.get_data(pos) {
				let commit_vec = token_outputs_map.entry(out.token_type).or_insert(vec![]);
				commit_vec.push(out.commit);
			}
		}
		token_outputs_map
	}

	fn token_kernels_committed(&self) -> HashMap<TokenKey, Vec<Commitment>> {
		let mut token_kernels_map: HashMap<TokenKey, Vec<Commitment>> = HashMap::new();
		for n in 1..self.token_kernel_pmmr.unpruned_size() + 1 {
			if pmmr::is_leaf(n) {
				if let Some(kernel) = self.token_kernel_pmmr.get_data(n) {
					let commit_vec = token_kernels_map.entry(kernel.token_type).or_insert(vec![]);
					if kernel.is_plain_token() {
						commit_vec.push(kernel.excess());
					}
				}
			}
		}
		token_kernels_map
	}
}

impl<'a> Extension<'a> {
	fn new(trees: &'a mut TxHashSet, head: Tip) -> Extension<'a> {
		Extension {
			head,
			output_pmmr: PMMR::at(
				&mut trees.output_pmmr_h.backend,
				trees.output_pmmr_h.last_pos,
			),
			rproof_pmmr: PMMR::at(
				&mut trees.rproof_pmmr_h.backend,
				trees.rproof_pmmr_h.last_pos,
			),
			kernel_pmmr: PMMR::at(
				&mut trees.kernel_pmmr_h.backend,
				trees.kernel_pmmr_h.last_pos,
			),

			token_output_pmmr: PMMR::at(
				&mut trees.token_output_pmmr_h.backend,
				trees.token_output_pmmr_h.last_pos,
			),
			token_rproof_pmmr: PMMR::at(
				&mut trees.token_rproof_pmmr_h.backend,
				trees.token_rproof_pmmr_h.last_pos,
			),
			token_issue_proof_pmmr: PMMR::at(
				&mut trees.token_issue_proof_pmmr_h.backend,
				trees.token_issue_proof_pmmr_h.last_pos,
			),
			token_kernel_pmmr: PMMR::at(
				&mut trees.token_kernel_pmmr_h.backend,
				trees.token_kernel_pmmr_h.last_pos,
			),
			bitmap_accumulator: trees.bitmap_accumulator.clone(),
			rollback: false,
		}
	}

	/// The head representing the furthest extent of the current extension.
	pub fn head(&self) -> Tip {
		self.head.clone()
	}

	/// Build a view of the current UTXO set based on the output PMMR
	/// and the provided header extension.
	pub fn utxo_view(&'a self, header_ext: &'a HeaderExtension<'a>) -> UTXOView<'a> {
		UTXOView::new(
			header_ext.pmmr.readonly_pmmr(),
			self.output_pmmr.readonly_pmmr(),
			self.token_output_pmmr.readonly_pmmr(),
			self.token_issue_proof_pmmr.readonly_pmmr(),
			self.rproof_pmmr.readonly_pmmr(),
			self.token_rproof_pmmr.readonly_pmmr(),
		)
	}

	/// Apply a new block to the current txhashet extension (output, rangeproof, kernel MMRs).
	/// Returns a vec of commit_pos representing the pos and height of the outputs spent
	/// by this block.
	pub fn apply_block(&mut self, b: &Block, batch: &Batch<'_>) -> Result<(), Error> {
		let mut affected_pos = vec![];

		// Apply the output to the output and rangeproof MMRs.
		// Add pos to affected_pos to update the accumulator later on.
		// Add the new output to the output_pos index.
		for out in b.outputs() {
			let pos = self.apply_output(out, batch)?;
			affected_pos.push(pos);
			batch.save_output_pos_height(&out.commitment(), pos, b.header.height)?;
		}

		// Remove the output from the output and rangeproof MMRs.
		// Add spent_pos to affected_pos to update the accumulator later on.
		// Remove the spent output from the output_pos index.
		let mut spent = vec![];
		for input in b.inputs() {
			let spent_pos = self.apply_input(input, batch)?;
			affected_pos.push(spent_pos.pos);
			batch.delete_output_pos_height(&input.commitment())?;
			spent.push(spent_pos);
		}
		batch.save_spent_index(&b.hash(), &spent)?;

		for out in b.token_outputs() {
			let pos = self.apply_token_output(out, batch)?;
			batch.save_token_output_pos_height(&out.commitment(), pos, b.header.height)?;

			if out.is_tokenissue() {
				let pos = self.apply_token_issue_output(out, batch)?;
				batch.save_token_issue_proof_pos(&out.token_type, pos)?;
			}
		}

		let mut token_spent = vec![];
		for input in b.token_inputs() {
			let spent_pos = self.apply_token_input(input, batch)?;
			batch.delete_token_output_pos_height(&input.commitment())?;
			token_spent.push(spent_pos);
		}
		batch.save_spent_token_index(&b.hash(), &token_spent)?;

		for kernel in b.kernels() {
			self.apply_kernel(kernel)?;
		}

		for token_kernel in b.token_kernels() {
			self.apply_token_kernel(token_kernel)?;
		}

		// Update our BitmapAccumulator based on affected outputs (both spent and created).
		self.apply_to_bitmap_accumulator(&affected_pos)?;

		// Update the head of the extension to reflect the block we just applied.
		self.head = Tip::from_header(&b.header);

		Ok(())
	}

	fn apply_to_bitmap_accumulator(&mut self, output_pos: &[u64]) -> Result<(), Error> {
		let mut output_idx: Vec<_> = output_pos
			.iter()
			.map(|x| pmmr::n_leaves(*x).saturating_sub(1))
			.collect();
		output_idx.sort_unstable();
		let min_idx = output_idx.first().cloned().unwrap_or(0);
		let size = pmmr::n_leaves(self.output_pmmr.last_pos);
		self.bitmap_accumulator.apply(
			output_idx,
			self.output_pmmr
				.leaf_idx_iter(BitmapAccumulator::chunk_start_idx(min_idx)),
			size,
		)
	}

	fn apply_input(&mut self, input: &Input, batch: &Batch<'_>) -> Result<CommitPos, Error> {
		let commit = input.commitment();
		if let Some((pos, height)) = batch.get_output_pos_height(&commit)? {
			// First check this input corresponds to an existing entry in the output MMR.
			if let Some(out) = self.output_pmmr.get_data(pos) {
				if OutputIdentifier::from(input) != out {
					return Err(ErrorKind::TxHashSetErr("output pmmr mismatch".to_string()).into());
				}
			}

			// Now prune the output_pmmr, rproof_pmmr and their storage.
			// Input is not valid if we cannot prune successfully (to spend an unspent
			// output).
			match self.output_pmmr.prune(pos) {
				Ok(true) => {
					self.rproof_pmmr
						.prune(pos)
						.map_err(ErrorKind::TxHashSetErr)?;
					Ok(CommitPos { pos, height })
				}
				Ok(false) => Err(ErrorKind::AlreadySpent(commit).into()),
				Err(e) => Err(ErrorKind::TxHashSetErr(e).into()),
			}
		} else {
			Err(ErrorKind::AlreadySpent(commit).into())
		}
	}

	fn apply_token_input(
		&mut self,
		token_input: &TokenInput,
		batch: &Batch<'_>,
	) -> Result<CommitPos, Error> {
		let commit = token_input.commitment();
		if let Some((pos, height)) = batch.get_token_output_pos_height(&commit)? {
			// First check this input corresponds to an existing entry in the output MMR.
			if let Some(out) = self.token_output_pmmr.get_data(pos) {
				if TokenOutputIdentifier::from(token_input) != out {
					return Err(
						ErrorKind::TxHashSetErr("token output pmmr mismatch".to_string()).into(),
					);
				}
			}

			// Now prune the output_pmmr, rproof_pmmr and their storage.
			// Input is not valid if we cannot prune successfully (to spend an unspent
			// output).
			match self.token_output_pmmr.prune(pos) {
				Ok(true) => {
					self.token_rproof_pmmr
						.prune(pos)
						.map_err(ErrorKind::TxHashSetErr)?;
					Ok(CommitPos { pos, height })
				}
				Ok(false) => Err(ErrorKind::AlreadySpent(commit).into()),
				Err(e) => Err(ErrorKind::TxHashSetErr(e).into()),
			}
		} else {
			Err(ErrorKind::AlreadySpent(commit).into())
		}
	}

	fn apply_output(&mut self, out: &Output, batch: &Batch<'_>) -> Result<u64, Error> {
		let commit = out.commitment();

		if let Ok(pos) = batch.get_output_pos(&commit) {
			if let Some(out_mmr) = self.output_pmmr.get_data(pos) {
				if out_mmr.commitment() == commit {
					return Err(ErrorKind::DuplicateCommitment(commit).into());
				}
			}
		}
		// push the new output to the MMR.
		let output_pos = self
			.output_pmmr
			.push(out)
			.map_err(&ErrorKind::TxHashSetErr)?;

		// push the rangeproof to the MMR.
		let rproof_pos = self
			.rproof_pmmr
			.push(&out.proof)
			.map_err(&ErrorKind::TxHashSetErr)?;

		// The output and rproof MMRs should be exactly the same size
		// and we should have inserted to both in exactly the same pos.
		{
			if self.output_pmmr.unpruned_size() != self.rproof_pmmr.unpruned_size() {
				return Err(
					ErrorKind::Other("output vs rproof MMRs different sizes".to_string()).into(),
				);
			}

			if output_pos != rproof_pos {
				return Err(
					ErrorKind::Other("output vs rproof MMRs different pos".to_string()).into(),
				);
			}
		}
		Ok(output_pos)
	}

	fn apply_token_output(
		&mut self,
		token_out: &TokenOutput,
		batch: &Batch<'_>,
	) -> Result<u64, Error> {
		let commit = token_out.commitment();

		if let Ok(pos) = batch.get_token_output_pos(&commit) {
			if let Some(out_mmr) = self.token_output_pmmr.get_data(pos) {
				if out_mmr.commitment() == commit {
					return Err(ErrorKind::DuplicateCommitment(commit).into());
				}
			}
		}
		// push the new output to the MMR.
		let output_pos = self
			.token_output_pmmr
			.push(token_out)
			.map_err(&ErrorKind::TxHashSetErr)?;

		// push the rangeproof to the MMR.
		let rproof_pos = self
			.token_rproof_pmmr
			.push(&token_out.proof)
			.map_err(&ErrorKind::TxHashSetErr)?;

		// The output and rproof MMRs should be exactly the same size
		// and we should have inserted to both in exactly the same pos.
		{
			if self.token_output_pmmr.unpruned_size() != self.token_rproof_pmmr.unpruned_size() {
				return Err(ErrorKind::Other(format!(
					"token_output vs token_rproof MMRs different sizes"
				))
				.into());
			}

			if output_pos != rproof_pos {
				return Err(ErrorKind::Other(format!(
					"token_output vs token_rproof MMRs different pos"
				))
				.into());
			}
		}

		Ok(output_pos)
	}

	fn apply_token_issue_output(
		&mut self,
		token_out: &TokenOutput,
		batch: &Batch<'_>,
	) -> Result<u64, Error> {
		if token_out.is_token() {
			return Err(ErrorKind::Other(format!("token_output is not a token issue")).into());
		}

		let token_key = token_out.token_type();

		if let Ok(pos) = batch.get_token_issue_proof_pos(&token_key) {
			if let Some(out_mmr) = self.token_issue_proof_pmmr.get_data(pos) {
				if out_mmr.token_type() == token_key {
					return Err(ErrorKind::DuplicateTokenKey(token_key).into());
				}
			}
		}
		// push the new output to the MMR.
		let issue_pos = self
			.token_issue_proof_pmmr
			.push(&TokenIssueProof::from_token_output(token_out))
			.map_err(&ErrorKind::TxHashSetErr)?;

		Ok(issue_pos)
	}

	/// Push kernel onto MMR (hash and data files).
	fn apply_kernel(&mut self, kernel: &TxKernel) -> Result<(), Error> {
		self.kernel_pmmr
			.push(kernel)
			.map_err(&ErrorKind::TxHashSetErr)?;
		Ok(())
	}

	/// Push kernel onto MMR (hash and data files).
	fn apply_token_kernel(&mut self, token_kernel: &TokenTxKernel) -> Result<(), Error> {
		self.token_kernel_pmmr
			.push(token_kernel)
			.map_err(&ErrorKind::TxHashSetErr)?;
		Ok(())
	}

	/// Build a Merkle proof for the given output and the block
	/// this extension is currently referencing.
	/// Note: this relies on the MMR being stable even after pruning/compaction.
	/// We need the hash of each sibling pos from the pos up to the peak
	/// including the sibling leaf node which may have been removed.
	pub fn merkle_proof(
		&self,
		output: &OutputIdentifier,
		batch: &Batch<'_>,
	) -> Result<MerkleProof, Error> {
		debug!("txhashset: merkle_proof: output: {:?}", output.commit,);
		// then calculate the Merkle Proof based on the known pos
		let pos = batch.get_output_pos(&output.commit)?;
		let merkle_proof = self
			.output_pmmr
			.merkle_proof(pos)
			.map_err(&ErrorKind::TxHashSetErr)?;

		Ok(merkle_proof)
	}

	/// Build a Merkle proof for the given token output and the block
	/// this extension is currently referencing.
	/// Note: this relies on the MMR being stable even after pruning/compaction.
	/// We need the hash of each sibling pos from the pos up to the peak
	/// including the sibling leaf node which may have been removed.
	pub fn token_merkle_proof(
		&self,
		output: &TokenOutputIdentifier,
		batch: &Batch<'_>,
	) -> Result<MerkleProof, Error> {
		debug!("txhashset: merkle_proof: output: {:?}", output.commit,);
		// then calculate the Merkle Proof based on the known pos
		let pos = batch.get_token_output_pos(&output.commit)?;
		let merkle_proof = self
			.token_output_pmmr
			.merkle_proof(pos)
			.map_err(&ErrorKind::TxHashSetErr)?;

		Ok(merkle_proof)
	}

	/// Saves a snapshot of the output and rangeproof MMRs to disk.
	/// Specifically - saves a snapshot of the utxo file, tagged with
	/// the block hash as filename suffix.
	/// Needed for fast-sync (utxo file needs to be rewound before sending
	/// across).
	pub fn snapshot(&mut self, batch: &Batch<'_>) -> Result<(), Error> {
		let header = batch.get_block_header(&self.head.last_block_h)?;
		self.output_pmmr
			.snapshot(&header)
			.map_err(ErrorKind::Other)?;
		self.rproof_pmmr
			.snapshot(&header)
			.map_err(|e| ErrorKind::Other(e))?;
		self.token_output_pmmr
			.snapshot(&header)
			.map_err(|e| ErrorKind::Other(e))?;
		self.token_rproof_pmmr
			.snapshot(&header)
			.map_err(ErrorKind::Other)?;
		Ok(())
	}

	/// Rewinds the MMRs to the provided block, rewinding to the last output pos
	/// and last kernel pos of that block.
	pub fn rewind(&mut self, header: &BlockHeader, batch: &Batch<'_>) -> Result<(), Error> {
		debug!(
			"Rewind extension to {} at {} from {} at {}",
			header.hash(),
			header.height,
			self.head.hash(),
			self.head.height
		);

		// We need to build bitmaps of added and removed output positions
		// so we can correctly rewind all operations applied to the output MMR
		// after the position we are rewinding to (these operations will be
		// undone during rewind).
		// Rewound output pos will be removed from the MMR.
		// Rewound input (spent) pos will be added back to the MMR.
		let head_header = batch.get_block_header(&self.head.hash())?;

		if head_header.height <= header.height {
			// Nothing to rewind but we do want to truncate the MMRs at header for consistency.
			self.rewind_mmrs_to_pos(
				header.output_mmr_size,
				header.kernel_mmr_size,
				header.token_output_mmr_size,
				header.token_issue_proof_mmr_size,
				header.token_kernel_mmr_size,
				&vec![],
				&vec![],
			)?;
			self.apply_to_bitmap_accumulator(&[header.output_mmr_size])?;
		} else {
			let mut affected_pos = vec![];
			let mut current = head_header;
			while header.height < current.height {
				let mut affected_pos_single_block = self.rewind_single_block(&current, batch)?;
				affected_pos.append(&mut affected_pos_single_block);
				current = batch.get_previous_header(&current)?;
			}
			// Now apply a single aggregate "affected_pos" to our bitmap accumulator.
			self.apply_to_bitmap_accumulator(&affected_pos)?;
		}

		// Update our head to reflect the header we rewound to.
		self.head = Tip::from_header(header);

		Ok(())
	}

	// Rewind the MMRs and the output_pos index.
	// Returns a vec of "affected_pos" so we can apply the necessary updates to the bitmap
	// accumulator in a single pass for all rewound blocks.
	fn rewind_single_block(
		&mut self,
		header: &BlockHeader,
		batch: &Batch<'_>,
	) -> Result<Vec<u64>, Error> {
		// The spent index allows us to conveniently "unspend" everything in a block.
		let spent = batch.get_spent_index(&header.hash());
		let token_spent = batch.get_token_spent_index(&header.hash());

		let spent_pos: Vec<_> = if let Ok(ref spent) = spent {
			spent.iter().map(|x| x.pos).collect()
		} else {
			warn!(
				"rewind_single_block: fallback to legacy input bitmap for block {} at {}",
				header.hash(),
				header.height
			);
			let bitmap = batch.get_block_input_bitmap(&header.hash())?;
			bitmap.iter().map(|x| x.into()).collect()
		};

		let token_spent_pos: Vec<_> = if let Ok(ref token_spent) = token_spent {
			token_spent.iter().map(|x| x.pos).collect()
		} else {
			warn!(
				"rewind_single_block: fallback to legacy token input bitmap for block {} at {}",
				header.hash(),
				header.height
			);
			let bitmap = batch.get_block_token_input_bitmap(&header.hash())?;
			bitmap.iter().map(|x| x.into()).collect()
		};

		if header.height == 0 {
			self.rewind_mmrs_to_pos(0, 0, 0, 0, 0, &spent_pos, &token_spent_pos)?;
		} else {
			let prev = batch.get_previous_header(&header)?;
			self.rewind_mmrs_to_pos(
				prev.output_mmr_size,
				prev.kernel_mmr_size,
				prev.token_output_mmr_size,
				prev.token_issue_proof_mmr_size,
				prev.token_kernel_mmr_size,
				&spent_pos,
				&token_spent_pos,
			)?;
		}

		// Update our BitmapAccumulator based on affected outputs.
		// We want to "unspend" every rewound spent output.
		// Treat last_pos as an affected output to ensure we rebuild far enough back.
		let mut affected_pos = spent_pos.clone();
		affected_pos.push(self.output_pmmr.last_pos);

		// Remove any entries from the output_pos created by the block being rewound.
		let block = batch.get_block(&header.hash())?;
		let mut missing_count = 0;
		for out in block.outputs() {
			if batch.delete_output_pos_height(&out.commitment()).is_err() {
				missing_count += 1;
			}
		}
		if missing_count > 0 {
			warn!(
				"rewind_single_block: {} output_pos entries missing for: {} at {}",
				missing_count,
				header.hash(),
				header.height,
			);
		}
		let mut token_missing_count = 0;
		for token_out in block.token_outputs() {
			if batch
				.delete_token_output_pos_height(&token_out.commitment())
				.is_err()
			{
				token_missing_count += 1;
			}
		}
		if token_missing_count > 0 {
			warn!(
				"rewind_single_block: {} token_output_pos entries missing for: {} at {}",
				missing_count,
				header.hash(),
				header.height,
			);
		}

		// Update output_pos based on "unspending" all spent pos from this block.
		// This is necessary to ensure the output_pos index correclty reflects a
		// reused output commitment. For example an output at pos 1, spent, reused at pos 2.
		// The output_pos index should be updated to reflect the old pos 1 when unspent.
		if let Ok(spent) = spent {
			for (x, y) in block.inputs().into_iter().zip(spent) {
				batch.save_output_pos_height(&x.commitment(), y.pos, y.height)?;
			}
		}
		if let Ok(token_spent) = token_spent {
			for (x, y) in block.token_inputs().into_iter().zip(token_spent) {
				batch.save_token_output_pos_height(&x.commitment(), y.pos, y.height)?;
			}
		}

		Ok(affected_pos)
	}

	/// Rewinds the MMRs to the provided positions, given the output and
	/// kernel pos we want to rewind to.
	fn rewind_mmrs_to_pos(
		&mut self,
		output_pos: u64,
		kernel_pos: u64,
		token_output_pos: u64,
		token_issue_proof_pos: u64,
		token_kernel_pos: u64,
		spent_pos: &[u64],
		token_spent_pos: &[u64],
	) -> Result<(), Error> {
		let bitmap: Bitmap = spent_pos.into_iter().map(|x| *x as u32).collect();
		let token_bitmap: Bitmap = token_spent_pos.into_iter().map(|x| *x as u32).collect();
		self.output_pmmr
			.rewind(output_pos, &bitmap)
			.map_err(&ErrorKind::TxHashSetErr)?;
		self.rproof_pmmr
			.rewind(output_pos, &bitmap)
			.map_err(&ErrorKind::TxHashSetErr)?;
		self.kernel_pmmr
			.rewind(kernel_pos, &Bitmap::create())
			.map_err(&ErrorKind::TxHashSetErr)?;
		self.token_output_pmmr
			.rewind(token_output_pos, &token_bitmap)
			.map_err(&ErrorKind::TxHashSetErr)?;
		self.token_rproof_pmmr
			.rewind(token_output_pos, &token_bitmap)
			.map_err(&ErrorKind::TxHashSetErr)?;
		self.token_issue_proof_pmmr
			.rewind(token_issue_proof_pos, &Bitmap::create())
			.map_err(&ErrorKind::TxHashSetErr)?;
		self.token_kernel_pmmr
			.rewind(token_kernel_pos, &Bitmap::create())
			.map_err(&ErrorKind::TxHashSetErr)?;

		Ok(())
	}

	/// Current root hashes and sums (if applicable) for the Output, range proof
	/// and kernel MMRs.
	pub fn roots(&self) -> Result<TxHashSetRoots, Error> {
		Ok(TxHashSetRoots {
			output_roots: OutputRoots {
				pmmr_root: self
					.output_pmmr
					.root()
					.map_err(|_| ErrorKind::InvalidRoot)?,
				bitmap_root: self.bitmap_accumulator.root(),
			},
			rproof_root: self
				.rproof_pmmr
				.root()
				.map_err(|_| ErrorKind::InvalidRoot)?,
			kernel_root: self
				.kernel_pmmr
				.root()
				.map_err(|_| ErrorKind::InvalidRoot)?,
			token_output_root: self
				.token_output_pmmr
				.root()
				.map_err(|_| ErrorKind::InvalidRoot)?,
			token_rproof_root: self
				.token_rproof_pmmr
				.root()
				.map_err(|_| ErrorKind::InvalidRoot)?,
			token_issue_proof_root: self
				.token_issue_proof_pmmr
				.root()
				.map_err(|_| ErrorKind::InvalidRoot)?,
			token_kernel_root: self
				.token_kernel_pmmr
				.root()
				.map_err(|_| ErrorKind::InvalidRoot)?,
		})
	}

	/// Validate the MMR (output, rangeproof, kernel) roots against the latest header.
	pub fn validate_roots(&self, header: &BlockHeader) -> Result<(), Error> {
		if header.height == 0 {
			return Ok(());
		}
		self.roots()?.validate(header)
	}

	/// Validate the header, output and kernel MMR sizes against the block header.
	pub fn validate_sizes(&self, header: &BlockHeader) -> Result<(), Error> {
		if header.height == 0 {
			return Ok(());
		}
		if (
			header.output_mmr_size,
			header.output_mmr_size,
			header.kernel_mmr_size,
			header.token_output_mmr_size,
			header.token_output_mmr_size,
			header.token_issue_proof_mmr_size,
			header.token_kernel_mmr_size,
		) != self.sizes()
		{
			Err(ErrorKind::InvalidMMRSize.into())
		} else {
			Ok(())
		}
	}

	fn validate_mmrs(&self) -> Result<(), Error> {
		let now = Instant::now();

		// validate all hashes and sums within the trees
		if let Err(e) = self.output_pmmr.validate() {
			return Err(ErrorKind::InvalidTxHashSet(e).into());
		}
		if let Err(e) = self.rproof_pmmr.validate() {
			return Err(ErrorKind::InvalidTxHashSet(e).into());
		}
		if let Err(e) = self.kernel_pmmr.validate() {
			return Err(ErrorKind::InvalidTxHashSet(e).into());
		}
		if let Err(e) = self.token_output_pmmr.validate() {
			return Err(ErrorKind::InvalidTxHashSet(e).into());
		}
		if let Err(e) = self.token_rproof_pmmr.validate() {
			return Err(ErrorKind::InvalidTxHashSet(e).into());
		}
		if let Err(e) = self.token_issue_proof_pmmr.validate() {
			return Err(ErrorKind::InvalidTxHashSet(e).into());
		}
		if let Err(e) = self.token_kernel_pmmr.validate() {
			return Err(ErrorKind::InvalidTxHashSet(e).into());
		}

		debug!(
			"txhashset: validated the output {}, rproof {}, kernel {}, token_output {}, token_rproof {}, token_issue_prrof {}, token_kernel {}  mmrs, took {}s",
			self.output_pmmr.unpruned_size(),
			self.rproof_pmmr.unpruned_size(),
			self.kernel_pmmr.unpruned_size(),
			self.token_output_pmmr.unpruned_size(),
			self.token_rproof_pmmr.unpruned_size(),
			self.token_issue_proof_pmmr.unpruned_size(),
			self.token_kernel_pmmr.unpruned_size(),
			now.elapsed().as_secs(),
		);

		Ok(())
	}

	/// Validate full kernel sums against the provided header (for overage and kernel_offset).
	/// This is an expensive operation as we need to retrieve all the UTXOs and kernels
	/// from the respective MMRs.
	/// For a significantly faster way of validating full kernel sums see BlockSums.
	pub fn validate_kernel_sums(
		&self,
		genesis: &BlockHeader,
		header: &BlockHeader,
	) -> Result<(Commitment, Commitment), Error> {
		let now = Instant::now();

		let (utxo_sum, kernel_sum) = self.verify_kernel_sums(
			header.total_overage(genesis.kernel_mmr_size > 0),
			header.total_kernel_offset(),
		)?;

		debug!(
			"txhashset: validated total kernel sums, took {}s",
			now.elapsed().as_secs(),
		);

		Ok((utxo_sum, kernel_sum))
	}

	/// Validate full token kernel sums against the provided header.
	pub fn validate_token_kernel_sums(&self) -> Result<BlockTokenSums, Error> {
		let now = Instant::now();

		let token_kernel_sum_map = self.verify_token_kernel_sum()?;

		debug!(
			"txhashset: validated total token kernel sums, took {}s",
			now.elapsed().as_secs(),
		);

		Ok(token_kernel_sum_map)
	}

	/// Validate the txhashset state against the provided block header.
	/// A "fast validation" will skip rangeproof verification and kernel signature verification.
	pub fn validate(
		&self,
		genesis: &BlockHeader,
		fast_validation: bool,
		status: &dyn TxHashsetWriteStatus,
		header: &BlockHeader,
	) -> Result<(Commitment, Commitment, BlockTokenSums), Error> {
		self.validate_mmrs()?;
		self.validate_roots(header)?;
		self.validate_sizes(header)?;

		if self.head.height == 0 {
			let zero_commit = secp_static::commit_to_zero_value();
			return Ok((zero_commit, zero_commit, BlockTokenSums::default()));
		}

		// The real magicking happens here. Sum of kernel excesses should equal
		// sum of unspent outputs minus total supply.
		let (output_sum, kernel_sum) = self.validate_kernel_sums(genesis, header)?;
		let block_token_sums = self.validate_token_kernel_sums()?;

		// These are expensive verification step (skipped for "fast validation").
		if !fast_validation {
			// Verify the rangeproof associated with each unspent output.
			self.verify_rangeproofs(status)?;

			self.verify_token_rangeproofs(status)?;

			// Verify all the kernel signatures.
			self.verify_kernel_signatures(status)?;

			self.verify_token_kernel_signatures(status)?;
		}

		Ok((output_sum, kernel_sum, block_token_sums))
	}

	/// Force the rollback of this extension, no matter the result
	pub fn force_rollback(&mut self) {
		self.rollback = true;
	}

	/// Dumps the output MMR.
	/// We use this after compacting for visual confirmation that it worked.
	pub fn dump_output_pmmr(&self) {
		debug!("-- outputs --");
		self.output_pmmr.dump_from_file(false);
		debug!("--");
		self.output_pmmr.dump_stats();
		debug!("-- end of outputs --");
	}

	/// Dumps the state of the 3 MMRs to stdout for debugging. Short
	/// version only prints the Output tree.
	pub fn dump(&self, short: bool) {
		debug!("-- outputs --");
		self.output_pmmr.dump(short);
		if !short {
			debug!("-- range proofs --");
			self.rproof_pmmr.dump(short);
			debug!("-- kernels --");
			self.kernel_pmmr.dump(short);
		}
	}

	/// Sizes of each of the MMRs
	pub fn sizes(&self) -> (u64, u64, u64, u64, u64, u64, u64) {
		(
			self.output_pmmr.unpruned_size(),
			self.rproof_pmmr.unpruned_size(),
			self.kernel_pmmr.unpruned_size(),
			self.token_output_pmmr.unpruned_size(),
			self.token_rproof_pmmr.unpruned_size(),
			self.token_issue_proof_pmmr.unpruned_size(),
			self.token_kernel_pmmr.unpruned_size(),
		)
	}

	fn verify_kernel_signatures(&self, status: &dyn TxHashsetWriteStatus) -> Result<(), Error> {
		let now = Instant::now();
		const KERNEL_BATCH_SIZE: usize = 5_000;

		let mut kern_count = 0;
		let total_kernels = pmmr::n_leaves(self.kernel_pmmr.unpruned_size());
		let mut tx_kernels: Vec<TxKernel> = Vec::with_capacity(KERNEL_BATCH_SIZE);
		for n in 1..self.kernel_pmmr.unpruned_size() + 1 {
			if pmmr::is_leaf(n) {
				let kernel = self
					.kernel_pmmr
					.get_data(n)
					.ok_or_else(|| ErrorKind::TxKernelNotFound)?;
				tx_kernels.push(kernel);
			}

			if tx_kernels.len() >= KERNEL_BATCH_SIZE || n >= self.kernel_pmmr.unpruned_size() {
				TxKernel::batch_sig_verify(&tx_kernels)?;
				kern_count += tx_kernels.len() as u64;
				tx_kernels.clear();
				status.on_validation_kernels(kern_count, total_kernels);
				debug!(
					"txhashset: verify_kernel_signatures: verified {} signatures",
					kern_count,
				);
			}
		}

		debug!(
			"txhashset: verified {} kernel signatures, pmmr size {}, took {}s",
			kern_count,
			self.kernel_pmmr.unpruned_size(),
			now.elapsed().as_secs(),
		);

		Ok(())
	}

	fn verify_token_kernel_signatures(
		&self,
		status: &dyn TxHashsetWriteStatus,
	) -> Result<(), Error> {
		let now = Instant::now();
		const KERNEL_BATCH_SIZE: usize = 5_000;

		let mut kern_count = 0;
		let total_kernels = pmmr::n_leaves(self.token_kernel_pmmr.unpruned_size());
		let mut tx_kernels: Vec<TokenTxKernel> = Vec::with_capacity(KERNEL_BATCH_SIZE);
		for n in 1..self.token_kernel_pmmr.unpruned_size() + 1 {
			if pmmr::is_leaf(n) {
				let kernel = self
					.token_kernel_pmmr
					.get_data(n)
					.ok_or::<Error>(ErrorKind::TxKernelNotFound.into())?;
				tx_kernels.push(kernel);
			}

			if tx_kernels.len() >= KERNEL_BATCH_SIZE || n >= self.token_kernel_pmmr.unpruned_size()
			{
				TokenTxKernel::batch_sig_verify(&tx_kernels)?;
				kern_count += tx_kernels.len() as u64;
				tx_kernels.clear();
				status.on_validation_token_kernels(kern_count, total_kernels);
				debug!(
					"txhashset: verify_token_kernel_signatures: verified {} signatures",
					kern_count,
				);
			}
		}

		debug!(
			"txhashset: verified {} token kernel signatures, pmmr size {}, took {}s",
			kern_count,
			self.token_kernel_pmmr.unpruned_size(),
			now.elapsed().as_secs(),
		);

		Ok(())
	}

	fn verify_rangeproofs(&self, status: &dyn TxHashsetWriteStatus) -> Result<(), Error> {
		let now = Instant::now();

		let mut commits: Vec<Commitment> = Vec::with_capacity(1_000);
		let mut proofs: Vec<RangeProof> = Vec::with_capacity(1_000);

		let mut proof_count = 0;
		let total_rproofs = self.output_pmmr.n_unpruned_leaves();

		for pos in self.output_pmmr.leaf_pos_iter() {
			let output = self.output_pmmr.get_data(pos);
			let proof = self.rproof_pmmr.get_data(pos);

			// Output and corresponding rangeproof *must* exist.
			// It is invalid for either to be missing and we fail immediately in this case.
			match (output, proof) {
				(None, _) => return Err(ErrorKind::OutputNotFound.into()),
				(_, None) => return Err(ErrorKind::RangeproofNotFound.into()),
				(Some(output), Some(proof)) => {
					commits.push(output.commit);
					proofs.push(proof);
				}
			}

			proof_count += 1;

			if proofs.len() >= 1_000 {
				Output::batch_verify_proofs(&commits, &proofs)?;
				commits.clear();
				proofs.clear();
				debug!(
					"txhashset: verify_rangeproofs: verified {} rangeproofs",
					proof_count,
				);
				if proof_count % 1_000 == 0 {
					status.on_validation_rproofs(proof_count, total_rproofs);
				}
			}
		}

		// remaining part which not full of 1000 range proofs
		if !proofs.is_empty() {
			Output::batch_verify_proofs(&commits, &proofs)?;
			commits.clear();
			proofs.clear();
			debug!(
				"txhashset: verify_rangeproofs: verified {} rangeproofs",
				proof_count,
			);
		}

		debug!(
			"txhashset: verified {} rangeproofs, pmmr size {}, took {}s",
			proof_count,
			self.rproof_pmmr.unpruned_size(),
			now.elapsed().as_secs(),
		);
		Ok(())
	}

	fn verify_token_rangeproofs(&self, status: &dyn TxHashsetWriteStatus) -> Result<(), Error> {
		let now = Instant::now();

		let mut commits: Vec<Commitment> = Vec::with_capacity(1_000);
		let mut proofs: Vec<RangeProof> = Vec::with_capacity(1_000);

		let mut proof_count = 0;
		let total_rproofs = pmmr::n_leaves(self.token_output_pmmr.unpruned_size());
		for pos in self.token_output_pmmr.leaf_pos_iter() {
			let output = self.token_output_pmmr.get_data(pos);
			let proof = self.token_rproof_pmmr.get_data(pos);

			// Output and corresponding rangeproof *must* exist.
			// It is invalid for either to be missing and we fail immediately in this case.
			match (output, proof) {
				(None, _) => return Err(ErrorKind::OutputNotFound.into()),
				(_, None) => return Err(ErrorKind::RangeproofNotFound.into()),
				(Some(output), Some(proof)) => {
					commits.push(output.commit);
					proofs.push(proof);
				}
			}

			proof_count += 1;

			if proofs.len() >= 1_000 {
				Output::batch_verify_proofs(&commits, &proofs)?;
				commits.clear();
				proofs.clear();
				debug!(
					"txhashset: verify_token_rangeproofs: verified {} rangeproofs",
					proof_count,
				);
			}

			if proof_count % 1_000 == 0 {
				status.on_validation_token_rproofs(proof_count, total_rproofs);
			}
		}

		// remaining part which not full of 1000 range proofs
		if proofs.len() > 0 {
			Output::batch_verify_proofs(&commits, &proofs)?;
			commits.clear();
			proofs.clear();
			debug!(
				"txhashset: verify_rangeproofs: verified {} token rangeproofs",
				proof_count,
			);
		}

		debug!(
			"txhashset: verified {} token rangeproofs, pmmr size {}, took {}s",
			proof_count,
			self.token_rproof_pmmr.unpruned_size(),
			now.elapsed().as_secs(),
		);
		Ok(())
	}
}

/// Packages the txhashset data files into a zip and returns a Read to the
/// resulting file
pub fn zip_read(root_dir: String, header: &BlockHeader) -> Result<File, Error> {
	let txhashset_zip = format!("{}_{}.zip", TXHASHSET_ZIP, header.hash().to_string());

	let txhashset_path = Path::new(&root_dir).join(TXHASHSET_SUBDIR);
	let zip_path = Path::new(&root_dir).join(txhashset_zip);

	// if file exist, just re-use it
	let zip_file = File::open(zip_path.clone());
	if let Ok(zip) = zip_file {
		debug!(
			"zip_read: {} at {}: reusing existing zip file: {:?}",
			header.hash(),
			header.height,
			zip_path
		);
		return Ok(zip);
	} else {
		// clean up old zips.
		// Theoretically, we only need clean-up those zip files older than STATE_SYNC_THRESHOLD.
		// But practically, these zip files are not small ones, we just keep the zips in last 24 hours
		let data_dir = Path::new(&root_dir);
		let pattern = format!("{}_", TXHASHSET_ZIP);
		if let Ok(n) = clean_files_by_prefix(data_dir, &pattern, 24 * 60 * 60) {
			debug!(
				"{} zip files have been clean up in folder: {:?}",
				n, data_dir
			);
		}
	}

	// otherwise, create the zip archive
	let path_to_be_cleanup = {
		// Temp txhashset directory
		let temp_txhashset_path = Path::new(&root_dir).join(format!(
			"{}_zip_{}",
			TXHASHSET_SUBDIR,
			header.hash().to_string()
		));
		// Remove temp dir if it exist
		if temp_txhashset_path.exists() {
			fs::remove_dir_all(&temp_txhashset_path)?;
		}
		// Copy file to another dir
		file::copy_dir_to(&txhashset_path, &temp_txhashset_path)?;

		let zip_file = File::create(zip_path.clone())?;

		// Explicit list of files to add to our zip archive.
		let files = file_list(header);

		zip::create_zip(&zip_file, &temp_txhashset_path, files)?;

		temp_txhashset_path
	};

	debug!(
		"zip_read: {} at {}: created zip file: {:?}",
		header.hash(),
		header.height,
		zip_path
	);

	// open it again to read it back
	let zip_file = File::open(zip_path.clone())?;

	// clean-up temp txhashset directory.
	if let Err(e) = fs::remove_dir_all(&path_to_be_cleanup) {
		warn!(
			"txhashset zip file: {:?} fail to remove, err: {}",
			zip_path.to_str(),
			e
		);
	}
	Ok(zip_file)
}

// Explicit list of files to extract from our zip archive.
// We include *only* these files when building the txhashset zip.
// We extract *only* these files when receiving a txhashset zip.
// Everything else will be safely ignored.
// Return Vec<PathBuf> as some of these are dynamic (specifically the "rewound" leaf files).
fn file_list(header: &BlockHeader) -> Vec<PathBuf> {
	vec![
		// kernel MMR
		PathBuf::from("kernel/pmmr_data.bin"),
		PathBuf::from("kernel/pmmr_hash.bin"),
		// output MMR
		PathBuf::from("output/pmmr_data.bin"),
		PathBuf::from("output/pmmr_hash.bin"),
		PathBuf::from("output/pmmr_prun.bin"),
		// rangeproof MMR
		PathBuf::from("rangeproof/pmmr_data.bin"),
		PathBuf::from("rangeproof/pmmr_hash.bin"),
		PathBuf::from("rangeproof/pmmr_prun.bin"),
		// Header specific "rewound" leaf files for output and rangeproof MMR.
		PathBuf::from(format!("output/pmmr_leaf.bin.{}", header.hash())),
		PathBuf::from(format!("rangeproof/pmmr_leaf.bin.{}", header.hash())),
		// token kernel MMR
		PathBuf::from("tokenkernel/pmmr_data.bin"),
		PathBuf::from("tokenkernel/pmmr_hash.bin"),
		// token output MMR
		PathBuf::from("tokenoutput/pmmr_data.bin"),
		PathBuf::from("tokenoutput/pmmr_hash.bin"),
		PathBuf::from("tokenoutput/pmmr_prun.bin"),
		// token rangeproof MMR
		PathBuf::from("tokenrangeproof/pmmr_data.bin"),
		PathBuf::from("tokenrangeproof/pmmr_hash.bin"),
		PathBuf::from("tokenrangeproof/pmmr_prun.bin"),
		// token issue proof MMR
		PathBuf::from("tokenissueproof/pmmr_data.bin"),
		PathBuf::from("tokenissueproof/pmmr_hash.bin"),
		// Header specific "rewound" leaf files for token output and token rangeproof MMR.
		PathBuf::from(format!("tokenoutput/pmmr_leaf.bin.{}", header.hash())),
		PathBuf::from(format!("tokenrangeproof/pmmr_leaf.bin.{}", header.hash())),
	]
}

/// Extract the txhashset data from a zip file and writes the content into the
/// txhashset storage dir
pub fn zip_write(
	root_dir: PathBuf,
	txhashset_data: File,
	header: &BlockHeader,
) -> Result<(), Error> {
	debug!("zip_write on path: {:?}", root_dir);
	let txhashset_path = root_dir.join(TXHASHSET_SUBDIR);
	fs::create_dir_all(&txhashset_path)?;

	// Explicit list of files to extract from our zip archive.
	let files = file_list(header);

	// We expect to see *exactly* the paths listed above.
	// No attempt is made to be permissive or forgiving with "alternative" paths.
	// These are the *only* files we will attempt to extract from the zip file.
	// If any of these are missing we will attempt to continue as some are potentially optional.
	zip::extract_files(txhashset_data, &txhashset_path, files)?;
	Ok(())
}

/// Overwrite txhashset folders in "to" folder with "from" folder
pub fn txhashset_replace(from: PathBuf, to: PathBuf) -> Result<(), Error> {
	debug!("txhashset_replace: move from {:?} to {:?}", from, to);

	// clean the 'to' folder firstly
	clean_txhashset_folder(&to);

	// rename the 'from' folder as the 'to' folder
	if let Err(e) = fs::rename(from.join(TXHASHSET_SUBDIR), to.join(TXHASHSET_SUBDIR)) {
		error!("hashset_replace fail on {}. err: {}", TXHASHSET_SUBDIR, e);
		Err(ErrorKind::TxHashSetErr("txhashset replacing fail".to_string()).into())
	} else {
		Ok(())
	}
}

/// Clean the txhashset folder
pub fn clean_txhashset_folder(root_dir: &PathBuf) {
	let txhashset_path = root_dir.clone().join(TXHASHSET_SUBDIR);
	if txhashset_path.exists() {
		if let Err(e) = fs::remove_dir_all(txhashset_path.clone()) {
			warn!(
				"clean_txhashset_folder: fail on {:?}. err: {}",
				txhashset_path, e
			);
		}
	}
}

/// Given a block header to rewind to and the block header at the
/// head of the current chain state, we need to calculate the positions
/// of all inputs (spent outputs) we need to "undo" during a rewind.
/// We do this by leveraging the "block_input_bitmap" cache and OR'ing
/// the set of bitmaps together for the set of blocks being rewound.
fn input_pos_to_rewind(
	block_header: &BlockHeader,
	head_header: &BlockHeader,
	batch: &Batch<'_>,
) -> Result<Bitmap, Error> {
	let mut bitmap = Bitmap::create();
	let mut current = head_header.clone();
	while current.height > block_header.height {
		if let Ok(block_bitmap) = batch.get_block_input_bitmap(&current.hash()) {
			bitmap.or_inplace(&block_bitmap);
		}
		current = batch.get_previous_header(&current)?;
	}
	Ok(bitmap)
}

/// Given a block header to rewind to and the block header at the
/// head of the current chain state, we need to calculate the positions
/// of all inputs (spent outputs) we need to "undo" during a rewind.
/// We do this by leveraging the "block_input_bitmap" cache and OR'ing
/// the set of bitmaps together for the set of blocks being rewound.
fn token_input_pos_to_rewind(
	block_header: &BlockHeader,
	head_header: &BlockHeader,
	batch: &Batch<'_>,
) -> Result<Bitmap, Error> {
	let mut bitmap = Bitmap::create();
	let mut current = head_header.clone();
	while current.height > block_header.height {
		if let Ok(block_bitmap) = batch.get_block_token_input_bitmap(&current.hash()) {
			bitmap.or_inplace(&block_bitmap);
		}
		current = batch.get_previous_header(&current)?;
	}
	Ok(bitmap)
}
