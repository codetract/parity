// Copyright 2015, 2016 Ethcore (UK) Ltd.
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

//! Eth rpc implementation.

extern crate ethash;

use std::io::{Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Instant, Duration};
use std::sync::{Arc, Weak};
use time::get_time;
use ethsync::{SyncProvider, SyncState};
use ethcore::miner::{MinerService, ExternalMinerService};
use jsonrpc_core::*;
use util::{H256, Address, FixedHash, U256, H64, Uint};
use util::sha3::*;
use util::{FromHex, Mutex};
use rlp::{self, UntrustedRlp, View};
use ethcore::account_provider::AccountProvider;
use ethcore::client::{MiningBlockChainClient, BlockID, TransactionID, UncleID};
use ethcore::header::{Header as BlockHeader, BlockNumber as EthBlockNumber};
use ethcore::block::IsBlock;
use ethcore::views::*;
use ethcore::ethereum::Ethash;
use ethcore::transaction::{Transaction as EthTransaction, SignedTransaction, Action};
use ethcore::log_entry::LogEntry;
use ethcore::filter::Filter as EthcoreFilter;
use self::ethash::SeedHashCompute;
use v1::traits::Eth;
use v1::types::{
	Block, BlockTransactions, BlockNumber, Bytes, SyncStatus, SyncInfo,
	Transaction, CallRequest, Index, Filter, Log, Receipt, Work,
	H64 as RpcH64, H256 as RpcH256, H160 as RpcH160, U256 as RpcU256,
};
use v1::helpers::{CallRequest as CRequest, errors, limit_logs};
use v1::helpers::dispatch::{default_gas_price, dispatch_transaction};
use v1::helpers::auto_args::Trailing;

/// Eth RPC options
pub struct EthClientOptions {
	/// Returns receipt from pending blocks
	pub allow_pending_receipt_query: bool,
	/// Send additional block number when asking for work
	pub send_block_number_in_get_work: bool,
}

impl Default for EthClientOptions {
	fn default() -> Self {
		EthClientOptions {
			allow_pending_receipt_query: true,
			send_block_number_in_get_work: true,
		}
	}
}

/// Eth rpc implementation.
pub struct EthClient<C, S: ?Sized, M, EM> where
	C: MiningBlockChainClient,
	S: SyncProvider,
	M: MinerService,
	EM: ExternalMinerService {

	client: Weak<C>,
	sync: Weak<S>,
	accounts: Weak<AccountProvider>,
	miner: Weak<M>,
	external_miner: Arc<EM>,
	seed_compute: Mutex<SeedHashCompute>,
	options: EthClientOptions,
}

impl<C, S: ?Sized, M, EM> EthClient<C, S, M, EM> where
	C: MiningBlockChainClient,
	S: SyncProvider,
	M: MinerService,
	EM: ExternalMinerService {

	/// Creates new EthClient.
	pub fn new(client: &Arc<C>, sync: &Arc<S>, accounts: &Arc<AccountProvider>, miner: &Arc<M>, em: &Arc<EM>, options: EthClientOptions)
		-> EthClient<C, S, M, EM> {
		EthClient {
			client: Arc::downgrade(client),
			sync: Arc::downgrade(sync),
			miner: Arc::downgrade(miner),
			accounts: Arc::downgrade(accounts),
			external_miner: em.clone(),
			seed_compute: Mutex::new(SeedHashCompute::new()),
			options: options,
		}
	}

	fn block(&self, id: BlockID, include_txs: bool) -> Result<Option<Block>, Error> {
		let client = take_weak!(self.client);
		match (client.block(id.clone()), client.block_total_difficulty(id)) {
			(Some(bytes), Some(total_difficulty)) => {
				let block_view = BlockView::new(&bytes);
				let view = block_view.header_view();
				let block = Block {
					hash: Some(view.sha3().into()),
					size: Some(bytes.len().into()),
					parent_hash: view.parent_hash().into(),
					uncles_hash: view.uncles_hash().into(),
					author: view.author().into(),
					miner: view.author().into(),
					state_root: view.state_root().into(),
					transactions_root: view.transactions_root().into(),
					receipts_root: view.receipts_root().into(),
					number: Some(view.number().into()),
					gas_used: view.gas_used().into(),
					gas_limit: view.gas_limit().into(),
					logs_bloom: view.log_bloom().into(),
					timestamp: view.timestamp().into(),
					difficulty: view.difficulty().into(),
					total_difficulty: total_difficulty.into(),
					seal_fields: view.seal().into_iter().map(|f| rlp::decode(&f)).map(Bytes::new).collect(),
					uncles: block_view.uncle_hashes().into_iter().map(Into::into).collect(),
					transactions: match include_txs {
						true => BlockTransactions::Full(block_view.localized_transactions().into_iter().map(Into::into).collect()),
						false => BlockTransactions::Hashes(block_view.transaction_hashes().into_iter().map(Into::into).collect()),
					},
					extra_data: Bytes::new(view.extra_data())
				};
				Ok(Some(block))
			},
			_ => Ok(None)
		}
	}

	fn transaction(&self, id: TransactionID) -> Result<Option<Transaction>, Error> {
		match take_weak!(self.client).transaction(id) {
			Some(t) => Ok(Some(Transaction::from(t))),
			None => Ok(None),
		}
	}

	fn uncle(&self, id: UncleID) -> Result<Option<Block>, Error> {
		let client = take_weak!(self.client);
		let uncle: BlockHeader = match client.uncle(id) {
			Some(rlp) => rlp::decode(&rlp),
			None => { return Ok(None); }
		};
		let parent_difficulty = match client.block_total_difficulty(BlockID::Hash(uncle.parent_hash().clone())) {
			Some(difficulty) => difficulty,
			None => { return Ok(None); }
		};

		let block = Block {
			hash: Some(uncle.hash().into()),
			size: None,
			parent_hash: uncle.parent_hash().clone().into(),
			uncles_hash: uncle.uncles_hash().clone().into(),
			author: uncle.author().clone().into(),
			miner: uncle.author().clone().into(),
			state_root: uncle.state_root().clone().into(),
			transactions_root: uncle.transactions_root().clone().into(),
			number: Some(uncle.number().into()),
			gas_used: uncle.gas_used().clone().into(),
			gas_limit: uncle.gas_limit().clone().into(),
			logs_bloom: uncle.log_bloom().clone().into(),
			timestamp: uncle.timestamp().into(),
			difficulty: uncle.difficulty().clone().into(),
			total_difficulty: (uncle.difficulty().clone() + parent_difficulty).into(),
			receipts_root: uncle.receipts_root().clone().into(),
			extra_data: uncle.extra_data().clone().into(),
			seal_fields: uncle.seal().clone().into_iter().map(|f| rlp::decode(&f)).map(Bytes::new).collect(),
			uncles: vec![],
			transactions: BlockTransactions::Hashes(vec![]),
		};
		Ok(Some(block))
	}

	fn sign_call(&self, request: CRequest) -> Result<SignedTransaction, Error> {
		let (client, miner) = (take_weak!(self.client), take_weak!(self.miner));
		let from = request.from.unwrap_or(Address::zero());
		Ok(EthTransaction {
			nonce: request.nonce.unwrap_or_else(|| client.latest_nonce(&from)),
			action: request.to.map_or(Action::Create, Action::Call),
			gas: request.gas.unwrap_or(U256::from(50_000_000)),
			gas_price: request.gas_price.unwrap_or_else(|| default_gas_price(&*client, &*miner)),
			value: request.value.unwrap_or_else(U256::zero),
			data: request.data.map_or_else(Vec::new, |d| d.to_vec())
		}.fake_sign(from))
	}
}

pub fn pending_logs<M>(miner: &M, best_block: EthBlockNumber, filter: &EthcoreFilter) -> Vec<Log> where M: MinerService {
	let receipts = miner.pending_receipts(best_block);

	let pending_logs = receipts.into_iter()
		.flat_map(|(hash, r)| r.logs.into_iter().map(|l| (hash.clone(), l)).collect::<Vec<(H256, LogEntry)>>())
		.collect::<Vec<(H256, LogEntry)>>();

	let result = pending_logs.into_iter()
		.filter(|pair| filter.matches(&pair.1))
		.map(|pair| {
			let mut log = Log::from(pair.1);
			log.transaction_hash = Some(pair.0.into());
			log
		})
		.collect();

	result
}

const MAX_QUEUE_SIZE_TO_MINE_ON: usize = 4;	// because uncles go back 6.

impl<C, S: ?Sized, M, EM> EthClient<C, S, M, EM> where
	C: MiningBlockChainClient + 'static,
	S: SyncProvider + 'static,
	M: MinerService + 'static,
	EM: ExternalMinerService + 'static {

	fn active(&self) -> Result<(), Error> {
		// TODO: only call every 30s at most.
		take_weak!(self.client).keep_alive();
		Ok(())
	}
}

#[cfg(windows)]
static SOLC: &'static str = "solc.exe";

#[cfg(not(windows))]
static SOLC: &'static str = "solc";

impl<C, S: ?Sized, M, EM> Eth for EthClient<C, S, M, EM> where
	C: MiningBlockChainClient + 'static,
	S: SyncProvider + 'static,
	M: MinerService + 'static,
	EM: ExternalMinerService + 'static {

	fn protocol_version(&self) -> Result<String, Error> {
		try!(self.active());

		let version = take_weak!(self.sync).status().protocol_version.to_owned();
		Ok(format!("{}", version))
	}

	fn syncing(&self) -> Result<SyncStatus, Error> {
		try!(self.active());

		let status = take_weak!(self.sync).status();
		match status.state {
			SyncState::Idle => Ok(SyncStatus::None),
			SyncState::Waiting | SyncState::Blocks | SyncState::NewBlocks | SyncState::ChainHead
				| SyncState::SnapshotManifest | SyncState::SnapshotData | SyncState::SnapshotWaiting => {
				let current_block = U256::from(take_weak!(self.client).chain_info().best_block_number);
				let highest_block = U256::from(status.highest_block_number.unwrap_or(status.start_block_number));

				if highest_block > current_block + U256::from(6) {
					let info = SyncInfo {
						starting_block: status.start_block_number.into(),
						current_block: current_block.into(),
						highest_block: highest_block.into(),
					};
					Ok(SyncStatus::Info(info))
				} else {
					Ok(SyncStatus::None)
				}
			}
		}
	}

	fn author(&self) -> Result<RpcH160, Error> {
		try!(self.active());

		Ok(RpcH160::from(take_weak!(self.miner).author()))
	}

	fn is_mining(&self) -> Result<bool, Error> {
		try!(self.active());

		Ok(take_weak!(self.miner).is_sealing())
	}

	fn hashrate(&self) -> Result<RpcU256, Error> {
		try!(self.active());

		Ok(RpcU256::from(self.external_miner.hashrate()))
	}

	fn gas_price(&self) -> Result<RpcU256, Error> {
		try!(self.active());

		let (client, miner) = (take_weak!(self.client), take_weak!(self.miner));
		Ok(RpcU256::from(default_gas_price(&*client, &*miner)))
	}

	fn accounts(&self) -> Result<Vec<RpcH160>, Error> {
		try!(self.active());

		let store = take_weak!(self.accounts);
		let accounts = try!(store.accounts().map_err(|e| errors::internal("Could not fetch accounts.", e)));
		Ok(accounts.into_iter().map(Into::into).collect())
	}

	fn block_number(&self) -> Result<RpcU256, Error> {
		try!(self.active());

		Ok(RpcU256::from(take_weak!(self.client).chain_info().best_block_number))
	}

	fn balance(&self, address: RpcH160, num: Trailing<BlockNumber>) -> Result<RpcU256, Error> {
		try!(self.active());

		let address = address.into();
		match num.0 {
			BlockNumber::Pending => Ok(take_weak!(self.miner).balance(&*take_weak!(self.client), &address).into()),
			id => match take_weak!(self.client).balance(&address, id.into()) {
				Some(balance) => Ok(balance.into()),
				None => Err(errors::state_pruned()),
			}
		}
	}

	fn storage_at(&self, address: RpcH160, pos: RpcU256, num: Trailing<BlockNumber>) -> Result<RpcH256, Error> {
		try!(self.active());
		let address: Address = RpcH160::into(address);
		let position: U256 = RpcU256::into(pos);
		match num.0 {
			BlockNumber::Pending => Ok(take_weak!(self.miner).storage_at(&*take_weak!(self.client), &address, &H256::from(position)).into()),
			id => match take_weak!(self.client).storage_at(&address, &H256::from(position), id.into()) {
				Some(s) => Ok(s.into()),
				None => Err(errors::state_pruned()),
			}
		}
	}

	fn transaction_count(&self, address: RpcH160, num: Trailing<BlockNumber>) -> Result<RpcU256, Error> {
		try!(self.active());

		let address: Address = RpcH160::into(address);
		match num.0 {
			BlockNumber::Pending => Ok(take_weak!(self.miner).nonce(&*take_weak!(self.client), &address).into()),
			id => match take_weak!(self.client).nonce(&address, id.into()) {
				Some(nonce) => Ok(nonce.into()),
				None => Err(errors::state_pruned()),
			}
		}
	}

	fn block_transaction_count_by_hash(&self, hash: RpcH256) -> Result<Option<RpcU256>, Error> {
		try!(self.active());
		Ok(
			take_weak!(self.client).block(BlockID::Hash(hash.into()))
				.map(|bytes| BlockView::new(&bytes).transactions_count().into())
		)
	}

	fn block_transaction_count_by_number(&self, num: BlockNumber) -> Result<Option<RpcU256>, Error> {
		try!(self.active());

		match num {
			BlockNumber::Pending => Ok(Some(
				take_weak!(self.miner).status().transactions_in_pending_block.into()
			)),
			_ => Ok(
				take_weak!(self.client).block(num.into())
					.map(|bytes| BlockView::new(&bytes).transactions_count().into())
				)
		}
	}

	fn block_uncles_count_by_hash(&self, hash: RpcH256) -> Result<Option<RpcU256>, Error> {
		try!(self.active());

		Ok(
			take_weak!(self.client).block(BlockID::Hash(hash.into()))
				.map(|bytes| BlockView::new(&bytes).uncles_count().into())
		)
	}

	fn block_uncles_count_by_number(&self, num: BlockNumber) -> Result<Option<RpcU256>, Error> {
		try!(self.active());

		match num {
			BlockNumber::Pending => Ok(Some(0.into())),
			_ => Ok(
				take_weak!(self.client).block(num.into())
					.map(|bytes| BlockView::new(&bytes).uncles_count().into())
			),
		}
	}

	fn code_at(&self, address: RpcH160, num: Trailing<BlockNumber>) -> Result<Bytes, Error> {
		try!(self.active());

		let address: Address = RpcH160::into(address);
		match num.0 {
			BlockNumber::Pending => Ok(take_weak!(self.miner).code(&*take_weak!(self.client), &address).map_or_else(Bytes::default, Bytes::new)),
			_ => match take_weak!(self.client).code(&address, num.0.into()) {
				Some(code) => Ok(code.map_or_else(Bytes::default, Bytes::new)),
				None => Err(errors::state_pruned()),
			},
		}
	}

	fn block_by_hash(&self, hash: RpcH256, include_txs: bool) -> Result<Option<Block>, Error> {
		try!(self.active());

		self.block(BlockID::Hash(hash.into()), include_txs)
	}

	fn block_by_number(&self, num: BlockNumber, include_txs: bool) -> Result<Option<Block>, Error> {
		try!(self.active());

		self.block(num.into(), include_txs)
	}

	fn transaction_by_hash(&self, hash: RpcH256) -> Result<Option<Transaction>, Error> {
		try!(self.active());
		let hash: H256 = hash.into();
		let miner = take_weak!(self.miner);
		let client = take_weak!(self.client);
		Ok(try!(self.transaction(TransactionID::Hash(hash))).or_else(|| miner.transaction(client.chain_info().best_block_number, &hash).map(Into::into)))
	}

	fn transaction_by_block_hash_and_index(&self, hash: RpcH256, index: Index) -> Result<Option<Transaction>, Error> {
		try!(self.active());

		self.transaction(TransactionID::Location(BlockID::Hash(hash.into()), index.value()))
	}

	fn transaction_by_block_number_and_index(&self, num: BlockNumber, index: Index) -> Result<Option<Transaction>, Error> {
		try!(self.active());

		self.transaction(TransactionID::Location(num.into(), index.value()))
	}

	fn transaction_receipt(&self, hash: RpcH256) -> Result<Option<Receipt>, Error> {
		try!(self.active());

		let miner = take_weak!(self.miner);
		let best_block = take_weak!(self.client).chain_info().best_block_number;
		let hash: H256 = hash.into();
		match (miner.pending_receipt(best_block, &hash), self.options.allow_pending_receipt_query) {
			(Some(receipt), true) => Ok(Some(receipt.into())),
			_ => {
				let client = take_weak!(self.client);
				let receipt = client.transaction_receipt(TransactionID::Hash(hash));
				Ok(receipt.map(Into::into))
			}
		}
	}

	fn uncle_by_block_hash_and_index(&self, hash: RpcH256, index: Index) -> Result<Option<Block>, Error> {
		try!(self.active());

		self.uncle(UncleID { block: BlockID::Hash(hash.into()), position: index.value() })
	}

	fn uncle_by_block_number_and_index(&self, num: BlockNumber, index: Index) -> Result<Option<Block>, Error> {
		try!(self.active());

		self.uncle(UncleID { block: num.into(), position: index.value() })
	}

	fn compilers(&self) -> Result<Vec<String>, Error> {
		try!(self.active());

		let mut compilers = vec![];
		if Command::new(SOLC).output().is_ok() {
			compilers.push("solidity".to_owned())
		}

		Ok(compilers)
	}

	fn logs(&self, filter: Filter) -> Result<Vec<Log>, Error> {
		let include_pending = filter.to_block == Some(BlockNumber::Pending);
		let filter: EthcoreFilter = filter.into();
		let mut logs = take_weak!(self.client).logs(filter.clone())
			.into_iter()
			.map(From::from)
			.collect::<Vec<Log>>();

		if include_pending {
			let best_block = take_weak!(self.client).chain_info().best_block_number;
			let pending = pending_logs(&*take_weak!(self.miner), best_block, &filter);
			logs.extend(pending);
		}

		let logs = limit_logs(logs, filter.limit);

		Ok(logs)
	}

	fn work(&self, no_new_work_timeout: Trailing<u64>) -> Result<Work, Error> {
		try!(self.active());
		let no_new_work_timeout = no_new_work_timeout.0;

		let client = take_weak!(self.client);
		// check if we're still syncing and return empty strings in that case
		{
			//TODO: check if initial sync is complete here
			//let sync = take_weak!(self.sync);
			if /*sync.status().state != SyncState::Idle ||*/ client.queue_info().total_queue_size() > MAX_QUEUE_SIZE_TO_MINE_ON {
				trace!(target: "miner", "Syncing. Cannot give any work.");
				return Err(errors::no_work());
			}

			// Otherwise spin until our submitted block has been included.
			let timeout = Instant::now() + Duration::from_millis(1000);
			while Instant::now() < timeout && client.queue_info().total_queue_size() > 0 {
				thread::sleep(Duration::from_millis(1));
			}
		}

		let miner = take_weak!(self.miner);
		if miner.author().is_zero() {
			warn!(target: "miner", "Cannot give work package - no author is configured. Use --author to configure!");
			return Err(errors::no_author())
		}
		miner.map_sealing_work(&*client, |b| {
			let pow_hash = b.hash();
			let target = Ethash::difficulty_to_boundary(b.block().header().difficulty());
			let seed_hash = self.seed_compute.lock().get_seedhash(b.block().header().number());

			if no_new_work_timeout > 0 && b.block().header().timestamp() + no_new_work_timeout < get_time().sec as u64 {
				Err(errors::no_new_work())
			} else if self.options.send_block_number_in_get_work {
				let block_number = b.block().header().number();
				Ok(Work {
					pow_hash: pow_hash.into(),
					seed_hash: seed_hash.into(),
					target: target.into(),
					number: Some(block_number),
				})
			} else {
				Ok(Work {
					pow_hash: pow_hash.into(),
					seed_hash: seed_hash.into(),
					target: target.into(),
					number: None
				})
			}
		}).unwrap_or(Err(Error::internal_error()))	// no work found.
	}

	fn submit_work(&self, nonce: RpcH64, pow_hash: RpcH256, mix_hash: RpcH256) -> Result<bool, Error> {
		try!(self.active());

		let nonce: H64 = nonce.into();
		let pow_hash: H256 = pow_hash.into();
		let mix_hash: H256 = mix_hash.into();
		trace!(target: "miner", "submit_work: Decoded: nonce={}, pow_hash={}, mix_hash={}", nonce, pow_hash, mix_hash);

		let miner = take_weak!(self.miner);
		let client = take_weak!(self.client);
		let seal = vec![rlp::encode(&mix_hash).to_vec(), rlp::encode(&nonce).to_vec()];
		Ok(miner.submit_seal(&*client, pow_hash, seal).is_ok())
	}

	fn submit_hashrate(&self, rate: RpcU256, id: RpcH256) -> Result<bool, Error> {
		try!(self.active());
		self.external_miner.submit_hashrate(rate.into(), id.into());
		Ok(true)
	}

	fn send_raw_transaction(&self, raw: Bytes) -> Result<RpcH256, Error> {
		try!(self.active());

		let raw_transaction = raw.to_vec();
		match UntrustedRlp::new(&raw_transaction).as_val() {
			Ok(signed_transaction) => dispatch_transaction(&*take_weak!(self.client), &*take_weak!(self.miner), signed_transaction),
			Err(_) => Ok(RpcH256::from(H256::from(0))),
		}
	}

	fn call(&self, request: CallRequest, num: Trailing<BlockNumber>) -> Result<Bytes, Error> {
		try!(self.active());

		let request = CallRequest::into(request);
		let signed = try!(self.sign_call(request));

		let r = match num.0 {
			BlockNumber::Pending => take_weak!(self.miner).call(&*take_weak!(self.client), &signed, Default::default()),
			num => take_weak!(self.client).call(&signed, num.into(), Default::default()),
		};

		Ok(r.map(|e| Bytes(e.output)).unwrap_or(Bytes::new(vec![])))
	}

	fn estimate_gas(&self, request: CallRequest, num: Trailing<BlockNumber>) -> Result<RpcU256, Error> {
		try!(self.active());

		let request = CallRequest::into(request);
		let signed = try!(self.sign_call(request));
		let r = match num.0 {
			BlockNumber::Pending => take_weak!(self.miner).call(&*take_weak!(self.client), &signed, Default::default()),
			num => take_weak!(self.client).call(&signed, num.into(), Default::default()),
		};

		Ok(RpcU256::from(r.map(|res| res.gas_used + res.refunded).unwrap_or(From::from(0))))
	}

	fn compile_lll(&self, _: String) -> Result<Bytes, Error> {
		try!(self.active());

		rpc_unimplemented!()
	}

	fn compile_serpent(&self, _: String) -> Result<Bytes, Error> {
		try!(self.active());

		rpc_unimplemented!()
	}

	fn compile_solidity(&self, code: String) -> Result<Bytes, Error> {
		try!(self.active());
		let maybe_child = Command::new(SOLC)
			.arg("--bin")
			.arg("--optimize")
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::null())
			.spawn();

		maybe_child
			.map_err(errors::compilation)
			.and_then(|mut child| {
				try!(child.stdin.as_mut()
					.expect("we called child.stdin(Stdio::piped()) before spawn; qed")
					.write_all(code.as_bytes())
					.map_err(errors::compilation));
				let output = try!(child.wait_with_output().map_err(errors::compilation));

				let s = String::from_utf8_lossy(&output.stdout);
				if let Some(hex) = s.lines().skip_while(|ref l| !l.contains("Binary")).skip(1).next() {
					Ok(Bytes::new(hex.from_hex().unwrap_or(vec![])))
				} else {
					Err(errors::compilation("Unexpected output."))
				}
			})
	}
}
