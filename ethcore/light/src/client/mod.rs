// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! Light client implementation. Stores data from light sync

use std::sync::{Weak, Arc};

use ethcore::block_status::BlockStatus;
use ethcore::client::{ClientReport, EnvInfo};
use ethcore::engines::Engine;
use ethcore::error::BlockImportError;
use ethcore::ids::BlockId;
use ethcore::header::Header;
use ethcore::verification::queue::{self, HeaderQueue};
use ethcore::blockchain_info::BlockChainInfo;
use ethcore::spec::Spec;
use ethcore::service::ClientIoMessage;
use ethcore::encoded;
use io::IoChannel;

use util::{H256, U256, Mutex, RwLock};
use util::kvdb::{KeyValueDB, CompactionProfile};

use self::header_chain::{AncestryIter, HeaderChain};

use cache::Cache;

pub use self::service::Service;

mod header_chain;
mod service;

/// Configuration for the light client.
#[derive(Debug, Clone)]
pub struct Config {
	/// Verification queue config.
	pub queue: queue::Config,
	/// Chain column in database.
	pub chain_column: Option<u32>,
	/// Database cache size. `None` => rocksdb default.
	pub db_cache_size: Option<usize>,
	/// State db compaction profile
	pub db_compaction: CompactionProfile,
	/// Should db have WAL enabled?
	pub db_wal: bool,
	/// Should it do full verification of blocks?
	pub verify_full: bool,
	/// Should it check the seal of blocks?
	pub check_seal: bool,
}

impl Default for Config {
	fn default() -> Config {
		Config {
			queue: Default::default(),
			chain_column: None,
			db_cache_size: None,
			db_compaction: CompactionProfile::default(),
			db_wal: true,
			verify_full: true,
			check_seal: true,
		}
	}
}

/// Trait for interacting with the header chain abstractly.
pub trait LightChainClient: Send + Sync {
	/// Get chain info.
	fn chain_info(&self) -> BlockChainInfo;

	/// Queue header to be verified. Required that all headers queued have their
	/// parent queued prior.
	fn queue_header(&self, header: Header) -> Result<H256, BlockImportError>;

	/// Attempt to get a block hash by block id.
	fn block_hash(&self, id: BlockId) -> Option<H256>;

	/// Attempt to get block header by block id.
	fn block_header(&self, id: BlockId) -> Option<encoded::Header>;

	/// Get the best block header.
	fn best_block_header(&self) -> encoded::Header;

	/// Get a block's chain score by ID.
	fn score(&self, id: BlockId) -> Option<U256>;

	/// Get an iterator over a block and its ancestry.
	fn ancestry_iter<'a>(&'a self, start: BlockId) -> Box<Iterator<Item=encoded::Header> + 'a>;

	/// Get the signing network ID.
	fn signing_network_id(&self) -> Option<u64>;

	/// Get environment info for execution at a given block.
	/// Fails if that block's header is not stored.
	fn env_info(&self, id: BlockId) -> Option<EnvInfo>;

	/// Get a handle to the consensus engine.
	fn engine(&self) -> &Arc<Engine>;

	/// Query whether a block is known.
	fn is_known(&self, hash: &H256) -> bool;

	/// Clear the queue.
	fn clear_queue(&self);

	/// Flush the queue.
	fn flush_queue(&self);

	/// Get queue info.
	fn queue_info(&self) -> queue::QueueInfo;

	/// Get the `i`th CHT root.
	fn cht_root(&self, i: usize) -> Option<H256>;

	/// Get the EIP-86 transition block number.
	fn eip86_transition(&self) -> u64;

	/// Get a report of import activity since the last call.
	fn report(&self) -> ClientReport;
}

/// An actor listening to light chain events.
pub trait LightChainNotify: Send + Sync {
	/// Notifies about imported headers.
	fn new_headers(&self, good: &[H256]);
}

/// Something which can be treated as a `LightChainClient`.
pub trait AsLightClient {
	/// The kind of light client this can be treated as.
	type Client: LightChainClient;

	/// Access the underlying light client.
	fn as_light_client(&self) -> &Self::Client;
}

impl<T: LightChainClient> AsLightClient for T {
	type Client = Self;

	fn as_light_client(&self) -> &Self { self }
}

/// Light client implementation.
pub struct Client {
	queue: HeaderQueue,
	engine: Arc<Engine>,
	chain: HeaderChain,
	report: RwLock<ClientReport>,
	import_lock: Mutex<()>,
	db: Arc<KeyValueDB>,
	listeners: RwLock<Vec<Weak<LightChainNotify>>>,
	verify_full: bool,
}

impl Client {
	/// Create a new `Client`.
	pub fn new(config: Config, db: Arc<KeyValueDB>, chain_col: Option<u32>, spec: &Spec, io_channel: IoChannel<ClientIoMessage>, cache: Arc<Mutex<Cache>>) -> Result<Self, String> {
		let gh = ::rlp::encode(&spec.genesis_header());

		Ok(Client {
			queue: HeaderQueue::new(config.queue, spec.engine.clone(), io_channel, config.check_seal),
			engine: spec.engine.clone(),
			chain: HeaderChain::new(db.clone(), chain_col, &gh, cache)?,
			report: RwLock::new(ClientReport::default()),
			import_lock: Mutex::new(()),
			db: db,
			listeners: RwLock::new(vec![]),
			verify_full: config.verify_full,
		})
	}

	/// Adds a new `LightChainNotify` listener.
	pub fn add_listener(&self, listener: Weak<LightChainNotify>) {
		self.listeners.write().push(listener);
	}

	/// Create a new `Client` backed purely in-memory.
	/// This will ignore all database options in the configuration.
	pub fn in_memory(config: Config, spec: &Spec, io_channel: IoChannel<ClientIoMessage>, cache: Arc<Mutex<Cache>>) -> Self {
		let db = ::util::kvdb::in_memory(0);

		Client::new(config, Arc::new(db), None, spec, io_channel, cache).expect("New DB creation infallible; qed")
	}

	/// Import a header to the queue for additional verification.
	pub fn import_header(&self, header: Header) -> Result<H256, BlockImportError> {
		self.queue.import(header).map_err(Into::into)
	}

	/// Inquire about the status of a given header.
	pub fn status(&self, hash: &H256) -> BlockStatus {
		match self.queue.status(hash) {
			queue::Status::Unknown => self.chain.status(hash),
			other => other.into(),
		}
	}

	/// Get the chain info.
	pub fn chain_info(&self) -> BlockChainInfo {
		let best_hdr = self.chain.best_header();
		let best_td = self.chain.best_block().total_difficulty;

		let first_block = self.chain.first_block();
		let genesis_hash = self.chain.genesis_hash();

		BlockChainInfo {
			total_difficulty: best_td,
			pending_total_difficulty: best_td + self.queue.total_difficulty(),
			genesis_hash: genesis_hash,
			best_block_hash: best_hdr.hash(),
			best_block_number: best_hdr.number(),
			best_block_timestamp: best_hdr.timestamp(),
			ancient_block_hash: if first_block.is_some() { Some(genesis_hash) } else { None },
			ancient_block_number: if first_block.is_some() { Some(0) } else { None },
			first_block_hash: first_block.as_ref().map(|first| first.hash),
			first_block_number: first_block.as_ref().map(|first| first.number),
		}
	}

	/// Get the header queue info.
	pub fn queue_info(&self) -> queue::QueueInfo {
		self.queue.queue_info()
	}

	/// Attempt to get a block hash by block id.
	pub fn block_hash(&self, id: BlockId) -> Option<H256> {
		self.chain.block_hash(id)
	}

	/// Get a block header by Id.
	pub fn block_header(&self, id: BlockId) -> Option<encoded::Header> {
		self.chain.block_header(id)
	}

	/// Get the best block header.
	pub fn best_block_header(&self) -> encoded::Header {
		self.chain.best_header()
	}

	/// Get a block's chain score.
	pub fn score(&self, id: BlockId) -> Option<U256> {
		self.chain.score(id)
	}

	/// Get an iterator over a block and its ancestry.
	pub fn ancestry_iter(&self, start: BlockId) -> AncestryIter {
		self.chain.ancestry_iter(start)
	}

	/// Get the signing network id.
	pub fn signing_network_id(&self) -> Option<u64> {
		self.engine.signing_network_id(&self.latest_env_info())
	}

	/// Flush the header queue.
	pub fn flush_queue(&self) {
		self.queue.flush()
	}

	/// Get the `i`th CHT root.
	pub fn cht_root(&self, i: usize) -> Option<H256> {
		self.chain.cht_root(i)
	}

	/// Import a set of pre-verified headers from the queue.
	pub fn import_verified(&self) {
		const MAX: usize = 256;

		let _lock = self.import_lock.lock();

		let mut bad = Vec::new();
		let mut good = Vec::new();
		for verified_header in self.queue.drain(MAX) {
			let (num, hash) = (verified_header.number(), verified_header.hash());
			trace!(target: "client", "importing block {}", num);

			if self.verify_full && !self.check_header(&mut bad, &verified_header) {
				continue
			}

			// TODO: `epoch_end_signal`, `is_epoch_end`.
			// proofs we get from the network would be _complete_, whereas we need
			// _incomplete_ signals

			let mut tx = self.db.transaction();
			let pending = match self.chain.insert(&mut tx, verified_header) {
				Ok(pending) => {
					good.push(hash);
					self.report.write().blocks_imported += 1;
					pending
				}
				Err(e) => {
					debug!(target: "client", "Error importing header {:?}: {}", (num, hash), e);
					bad.push(hash);
					continue;
				}
			};

			self.db.write_buffered(tx);
			self.chain.apply_pending(pending);
		}

		if let Err(e) = self.db.flush() {
			panic!("Database flush failed: {}. Check disk health and space.", e);
		}

		self.queue.mark_as_bad(&bad);
		self.queue.mark_as_good(&good);

		self.notify(|listener| listener.new_headers(&good));
	}

	/// Get a report about blocks imported.
	pub fn report(&self) -> ClientReport {
		self.report.read().clone()
	}

	/// Get blockchain mem usage in bytes.
	pub fn chain_mem_used(&self) -> usize {
		use util::HeapSizeOf;

		self.chain.heap_size_of_children()
	}

	/// Get a handle to the verification engine.
	pub fn engine(&self) -> &Arc<Engine> {
		&self.engine
	}

	/// Get the latest environment info.
	pub fn latest_env_info(&self) -> EnvInfo {
		self.env_info(BlockId::Latest)
			.expect("Best block header and recent hashes always stored; qed")
	}

	/// Get environment info for a given block.
	pub fn env_info(&self, id: BlockId) -> Option<EnvInfo> {
		let header = match self.block_header(id) {
			Some(hdr) => hdr,
			None => return None,
		};

		Some(EnvInfo {
			number: header.number(),
			author: header.author(),
			timestamp: header.timestamp(),
			difficulty: header.difficulty(),
			last_hashes: self.build_last_hashes(header.parent_hash()),
			gas_used: Default::default(),
			gas_limit: header.gas_limit(),
		})
	}

	fn build_last_hashes(&self, mut parent_hash: H256) -> Arc<Vec<H256>> {
		let mut v = Vec::with_capacity(256);
		for _ in 0..255 {
			v.push(parent_hash);
			match self.block_header(BlockId::Hash(parent_hash)) {
				Some(header) => parent_hash = header.hash(),
				None => break,
			}
		}

		Arc::new(v)
	}

	fn notify<F: Fn(&LightChainNotify)>(&self, f: F) {
		for listener in &*self.listeners.read() {
			if let Some(listener) = listener.upgrade() {
				f(&*listener)
			}
		}
	}

	// return false if should skip, true otherwise. may push onto bad if
	// should skip.
	fn check_header(&self, bad: &mut Vec<H256>, verified_header: &Header) -> bool {
		let hash = verified_header.hash();
		let parent_header = match self.chain.block_header(BlockId::Hash(*verified_header.parent_hash())) {
			Some(header) => header,
			None => {
				trace!(target: "client", "No parent for block ({}, {})",
					verified_header.number(), hash);
				return false // skip import of block with missing parent.
			}
		};

		// Verify Block Family
		let verify_family_result = self.engine.verify_block_family(&verified_header, &parent_header.decode(), None);
		if let Err(e) = verify_family_result {
			warn!(target: "client", "Stage 3 block verification failed for #{} ({})\nError: {:?}",
				verified_header.number(), verified_header.hash(), e);
			bad.push(hash);
			return false;
		};

		// "external" verification.
		let verify_external_result = self.engine.verify_block_external(&verified_header, None);
		if let Err(e) = verify_external_result {
			warn!(target: "client", "Stage 4 block verification failed for #{} ({})\nError: {:?}",
				verified_header.number(), verified_header.hash(), e);

			bad.push(hash);
			return false;
		};

		true
	}
}

impl LightChainClient for Client {
	fn chain_info(&self) -> BlockChainInfo { Client::chain_info(self) }

	fn queue_header(&self, header: Header) -> Result<H256, BlockImportError> {
		self.import_header(header)
	}

	fn block_hash(&self, id: BlockId) -> Option<H256> {
		Client::block_hash(self, id)
	}

	fn block_header(&self, id: BlockId) -> Option<encoded::Header> {
		Client::block_header(self, id)
	}

	fn best_block_header(&self) -> encoded::Header {
		Client::best_block_header(self)
	}

	fn score(&self, id: BlockId) -> Option<U256> {
		Client::score(self, id)
	}

	fn ancestry_iter<'a>(&'a self, start: BlockId) -> Box<Iterator<Item=encoded::Header> + 'a> {
		Box::new(Client::ancestry_iter(self, start))
	}

	fn signing_network_id(&self) -> Option<u64> {
		Client::signing_network_id(self)
	}

	fn env_info(&self, id: BlockId) -> Option<EnvInfo> {
		Client::env_info(self, id)
	}

	fn engine(&self) -> &Arc<Engine> {
		Client::engine(self)
	}

	fn is_known(&self, hash: &H256) -> bool {
		self.status(hash) == BlockStatus::InChain
	}

	fn clear_queue(&self) {
		self.queue.clear()
	}

	fn flush_queue(&self) {
		Client::flush_queue(self);
	}

	fn queue_info(&self) -> queue::QueueInfo {
		self.queue.queue_info()
	}

	fn cht_root(&self, i: usize) -> Option<H256> {
		Client::cht_root(self, i)
	}

	fn eip86_transition(&self) -> u64 {
		self.engine().params().eip86_transition
	}

	fn report(&self) -> ClientReport {
		Client::report(self)
	}
}
