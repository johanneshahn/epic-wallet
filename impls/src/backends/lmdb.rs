// Copyright 2019 The Epic Developers
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

use super::db::{self, Store};
use crate::blake2::blake2b::{Blake2b, Blake2bResult};
use crate::core::core::Transaction;
use crate::core::ser;
use crate::keychain::{ChildNumber, ExtKeychain, Identifier, Keychain, SwitchCommitmentType};
use crate::libwallet::{
	AcctPathMapping, Context, Error, NodeClient, OutputData, OutputStatus, ScannedBlockInfo,
	TxLogEntry, WalletBackend, WalletInitStatus, WalletOutputBatch,
};
use crate::serialization::Serializable;
use crate::store::{to_key, to_key_u64};
use crate::util::secp::constants::SECRET_KEY_SIZE;
use crate::util::secp::key::SecretKey;
use crate::util::{self, secp};
use rand::rng;
use rand::rngs::mock::StepRng;
use std::cell::RefCell;
use std::fs::File;
use std::io::{Read, Write};
use std::marker::PhantomData;
use std::path::Path;
use std::{fs, path};
use uuid::Uuid;

pub const DB_DIR: &'static str = "db";
const SQLITE_DIR: &'static str = "sqlite";
pub const TX_SAVE_DIR: &'static str = "saved_txs";

const OUTPUT_HISTORY_PREFIX: u8 = 'h' as u8;
const OUTPUT_HISTORY_ID_PREFIX: u8 = 'j' as u8;
const OUTPUT_PREFIX: u8 = 'o' as u8;
const DERIV_PREFIX: u8 = 'd' as u8;
const CONFIRMED_HEIGHT_PREFIX: u8 = 'c' as u8;
const PRIVATE_TX_CONTEXT_PREFIX: u8 = 'p' as u8;
const TX_LOG_ENTRY_PREFIX: u8 = 't' as u8;
const TX_LOG_ID_PREFIX: u8 = 'i' as u8;
const ACCOUNT_PATH_MAPPING_PREFIX: u8 = 'a' as u8;
const LAST_SCANNED_BLOCK: u8 = 'l' as u8;
const LAST_SCANNED_KEY: &str = "LAST_SCANNED_KEY";
const WALLET_INIT_STATUS: u8 = 'w' as u8;
const WALLET_INIT_STATUS_KEY: &str = "WALLET_INIT_STATUS";

/// test to see if database files exist in the current directory. If so,
/// use a DB backend for all operations
pub fn wallet_db_exists(data_file_dir: &str) -> bool {
	let db_path = path::Path::new(data_file_dir).join(DB_DIR);
	db_path.exists()
}

/// Helper to derive XOR keys for storing private transaction keys in the DB
/// (blind_xor_key, nonce_xor_key)
fn private_ctx_xor_keys<K>(
	keychain: &K,
	slate_id: &[u8],
) -> Result<([u8; SECRET_KEY_SIZE], [u8; SECRET_KEY_SIZE]), Error>
where
	K: Keychain,
{
	let root_key = keychain.derive_key(0, &K::root_key_id(), &SwitchCommitmentType::Regular)?;

	// derive XOR values for storing secret values in DB
	// h(root_key|slate_id|"blind")
	let mut hasher = Blake2b::new(SECRET_KEY_SIZE);
	hasher.update(&root_key.0[..]);
	hasher.update(&slate_id[..]);
	hasher.update(&"blind".as_bytes()[..]);
	let blind_xor_key = hasher.finalize();
	let mut ret_blind = [0; SECRET_KEY_SIZE];
	ret_blind.copy_from_slice(&blind_xor_key.as_bytes()[0..SECRET_KEY_SIZE]);

	// h(root_key|slate_id|"nonce")
	let mut hasher = Blake2b::new(SECRET_KEY_SIZE);
	hasher.update(&root_key.0[..]);
	hasher.update(&slate_id[..]);
	hasher.update(&"nonce".as_bytes()[..]);
	let nonce_xor_key = hasher.finalize();
	let mut ret_nonce = [0; SECRET_KEY_SIZE];
	ret_nonce.copy_from_slice(&nonce_xor_key.as_bytes()[0..SECRET_KEY_SIZE]);

	Ok((ret_blind, ret_nonce))
}

pub struct LMDBBackend<'ck, C, K>
where
	C: NodeClient + 'ck,
	K: Keychain + 'ck,
{
	db: Store,
	data_file_dir: String,
	/// Keychain
	pub keychain: Option<K>,
	/// Check value for XORed keychain seed
	pub master_checksum: Box<Option<Blake2bResult>>,
	/// Parent path to use by default for output operations
	parent_key_id: Identifier,
	/// wallet to node client
	w2n_client: C,
	///phantom
	_phantom: &'ck PhantomData<C>,
}

impl<'ck, C, K> LMDBBackend<'ck, C, K>
where
	C: NodeClient + 'ck,
	K: Keychain + 'ck,
{
	pub fn new(data_file_dir: &str, n_client: C) -> Result<Self, Error> {
		let db_path = path::Path::new(data_file_dir).join(DB_DIR).join(SQLITE_DIR);
		fs::create_dir_all(&db_path).expect("Couldn't create wallet backend directory!");

		let stored_tx_path = path::Path::new(data_file_dir).join(TX_SAVE_DIR);
		fs::create_dir_all(&stored_tx_path)
			.expect("Couldn't create wallet backend tx storage directory!");

		let store = db::Store::new(db_path)?;

		// Make sure default wallet derivation path always exists
		// as well as path (so it can be retrieved by batches to know where to store
		// completed transactions, for reference
		let default_account = AcctPathMapping {
			label: "default".to_owned(),
			path: LMDBBackend::<C, K>::default_path(),
		};
		let acct_key = to_key(
			ACCOUNT_PATH_MAPPING_PREFIX,
			&mut default_account.label.as_bytes().to_vec(),
		);

		{
			let batch = store.batch();
			batch.put(&acct_key, Serializable::AcctPathMapping(default_account))?;
		}

		let res = LMDBBackend {
			db: store,
			data_file_dir: data_file_dir.to_owned(),
			keychain: None,
			master_checksum: Box::new(None),
			parent_key_id: LMDBBackend::<C, K>::default_path(),
			w2n_client: n_client,
			_phantom: &PhantomData,
		};
		Ok(res)
	}

	fn default_path() -> Identifier {
		// return the default parent wallet path, corresponding to the default account
		// in the BIP32 spec. Parent is account 0 at level 2, child output identifiers
		// are all at level 3
		ExtKeychain::derive_key_id(2, 0, 0, 0, 0)
	}

	/// Just test to see if database files exist in the current directory. If
	/// so, use a DB backend for all operations
	pub fn exists(data_file_dir: &str) -> bool {
		let db_path = path::Path::new(data_file_dir).join(DB_DIR);
		db_path.exists()
	}
}

impl<'ck, C, K> WalletBackend<'ck, C, K> for LMDBBackend<'ck, C, K>
where
	C: NodeClient + 'ck,
	K: Keychain + 'ck,
{
	/// Set the keychain, which should already have been opened
	fn set_keychain(
		&mut self,
		mut k: Box<K>,
		mask: bool,
		use_test_rng: bool,
	) -> Result<Option<SecretKey>, Error> {
		// store hash of master key, so it can be verified later after unmasking
		let root_key = k.derive_key(0, &K::root_key_id(), &SwitchCommitmentType::Regular)?;
		let mut hasher = Blake2b::new(SECRET_KEY_SIZE);
		hasher.update(&root_key.0[..]);
		self.master_checksum = Box::new(Some(hasher.finalize()));

		let mask_value = {
			match mask {
				true => {
					// Random value that must be XORed against the stored wallet seed
					// before it is used
					let mask_value = match use_test_rng {
						true => {
							let mut test_rng = StepRng::new(1234567890u64, 1);
							secp::key::SecretKey::new(&k.secp(), &mut test_rng)
						}
						false => secp::key::SecretKey::new(&k.secp(), &mut rng()),
					};
					k.mask_master_key(&mask_value)?;
					Some(mask_value)
				}
				false => None,
			}
		};

		self.keychain = Some(*k);
		Ok(mask_value)
	}

	/// Close wallet
	fn close(&mut self) -> Result<(), Error> {
		self.keychain = None;
		Ok(())
	}

	/// Return the keychain being used, cloned with XORed token value
	/// for temporary use
	fn keychain(&self, mask: Option<&SecretKey>) -> Result<K, Error> {
		match self.keychain.as_ref() {
			Some(k) => {
				let mut k_masked = k.clone();
				if let Some(m) = mask {
					k_masked.mask_master_key(m)?;
				}
				// Check if master seed is what is expected (especially if it's been xored)
				let root_key =
					k_masked.derive_key(0, &K::root_key_id(), &SwitchCommitmentType::Regular)?;
				let mut hasher = Blake2b::new(SECRET_KEY_SIZE);
				hasher.update(&root_key.0[..]);
				if *self.master_checksum != Some(hasher.finalize()) {
					error!("Supplied keychain mask is invalid");
					return Err(Error::InvalidKeychainMask.into());
				}
				Ok(k_masked)
			}
			None => Err(Error::KeychainDoesntExist.into()),
		}
	}

	/// Return the node client being used
	fn w2n_client(&mut self) -> &mut C {
		&mut self.w2n_client
	}

	/// return the version of the commit for caching
	fn calc_commit_for_cache(
		&mut self,
		keychain_mask: Option<&SecretKey>,
		amount: u64,
		id: &Identifier,
	) -> Result<Option<String>, Error> {
		//TODO: Check if this is really necessary, it's the only thing
		//preventing removing the need for config in the wallet backend
		/*if self.config.no_commit_cache == Some(true) {
			Ok(None)
		} else {*/
		Ok(Some(util::to_hex(
			self.keychain(keychain_mask)?
				.commit(amount, &id, &SwitchCommitmentType::Regular)?
				.0
				.to_vec(), // TODO: proper support for different switch commitment schemes
		)))
		/*}*/
	}

	/// Set parent path by account name
	fn set_parent_key_id_by_name(&mut self, label: &str) -> Result<(), Error> {
		let label = label.to_owned();
		let res = self.acct_path_iter().find(|l| l.label == label);

		if let Some(a) = res {
			self.set_parent_key_id(a.path);
			Ok(())
		} else {
			return Err(Error::UnknownAccountLabel(label.clone()).into());
		}
	}

	/// set parent path
	fn set_parent_key_id(&mut self, id: Identifier) {
		self.parent_key_id = id;
	}

	fn parent_key_id(&mut self) -> Identifier {
		self.parent_key_id.clone()
	}

	fn get(&self, id: &Identifier, mmr_index: &Option<u64>) -> Result<OutputData, Error> {
		let key = match mmr_index {
			Some(i) => to_key_u64(OUTPUT_PREFIX, &mut id.to_bytes().to_vec(), *i),
			None => to_key(OUTPUT_PREFIX, &mut id.to_bytes().to_vec()),
		};

		Ok(self
			.db
			.get_ser(&key)
			.ok_or(Error::NotFoundErr(format!("Key Id: {}", id)))?
			.as_output_data()
			.unwrap())
	}

	fn iter<'a>(&'a self) -> Box<dyn Iterator<Item = OutputData> + 'a> {
		// new vec/enum implementation
		let serializables: Vec<_> = self
			.db
			.iter(&[OUTPUT_PREFIX])
			.into_iter()
			.filter_map(Serializable::as_output_data)
			.collect();
		Box::new(serializables.into_iter().map(|x| x))
	}

	fn history_iter<'a>(&'a self) -> Box<dyn Iterator<Item = OutputData> + 'a> {
		// new vec/enum implementation
		let serializables: Vec<_> = self
			.db
			.iter(&[OUTPUT_HISTORY_PREFIX])
			.into_iter()
			.filter_map(Serializable::as_output_data)
			.collect();
		Box::new(serializables.into_iter().map(|x| x))
	}

	fn get_tx_log_entry(&self, u: &Uuid) -> Result<Option<TxLogEntry>, Error> {
		let key = to_key(TX_LOG_ENTRY_PREFIX, &mut u.as_bytes().to_vec());

		Ok(match self.db.get(&key) {
			Some(s) => Serializable::as_txlogentry(s),
			None => None,
		})
	}

	fn tx_log_iter<'a>(&'a self) -> Box<dyn Iterator<Item = TxLogEntry> + 'a> {
		let serializables: Vec<_> = self
			.db
			.iter(&[TX_LOG_ENTRY_PREFIX])
			.into_iter()
			.filter_map(Serializable::as_txlogentry)
			.collect();
		Box::new(serializables.into_iter().map(|x| x))
	}

	fn get_private_context(
		&mut self,
		keychain_mask: Option<&SecretKey>,
		slate_id: &[u8],
		participant_id: usize,
	) -> Result<Context, Error> {
		let ctx_key = to_key_u64(
			PRIVATE_TX_CONTEXT_PREFIX,
			&mut slate_id.to_vec(),
			participant_id as u64,
		);
		let (blind_xor_key, nonce_xor_key) =
			private_ctx_xor_keys(&self.keychain(keychain_mask)?, slate_id)?;

		let mut ctx = self
			.db
			.get(&ctx_key)
			.ok_or(Error::NotFoundErr(format!(
				"Slate id: {:x?}",
				slate_id.to_vec()
			)))?
			.as_context()
			.unwrap();

		for i in 0..SECRET_KEY_SIZE {
			ctx.sec_key.0[i] = ctx.sec_key.0[i] ^ blind_xor_key[i];
			ctx.sec_nonce.0[i] = ctx.sec_nonce.0[i] ^ nonce_xor_key[i];
		}

		Ok(ctx)
	}

	fn acct_path_iter<'a>(&'a self) -> Box<dyn Iterator<Item = AcctPathMapping> + 'a> {
		//iter
		//pattern-match
		// vec of APM

		let serializables: Vec<_> = self
			.db
			.iter(&[ACCOUNT_PATH_MAPPING_PREFIX])
			.into_iter()
			.filter_map(Serializable::as_acct_path_mapping)
			.collect();
		Box::new(serializables.into_iter().map(|x| x))
	}

	fn get_acct_path(&self, label: String) -> Result<Option<AcctPathMapping>, Error> {
		let acct_key = to_key(ACCOUNT_PATH_MAPPING_PREFIX, &mut label.as_bytes().to_vec());

		Ok(match self.db.get_ser(&acct_key) {
			Some(s) => Serializable::as_acct_path_mapping(s),
			None => None,
		})
	}

	fn store_tx(&self, uuid: &str, tx: &Transaction) -> Result<(), Error> {
		let filename = format!("{}.epictx", uuid);
		let path = path::Path::new(&self.data_file_dir)
			.join(TX_SAVE_DIR)
			.join(filename);
		let path_buf = Path::new(&path).to_path_buf();
		let mut stored_tx = File::create(path_buf)?;
		let tx_hex = util::to_hex(ser::ser_vec(tx, ser::ProtocolVersion(1)).unwrap());
		stored_tx.write_all(&tx_hex.as_bytes())?;
		stored_tx.sync_all()?;
		Ok(())
	}

	fn get_stored_tx(&self, entry: &TxLogEntry) -> Result<Option<Transaction>, Error> {
		let filename = match entry.stored_tx.clone() {
			Some(f) => f,
			None => return Ok(None),
		};
		let path = path::Path::new(&self.data_file_dir)
			.join(TX_SAVE_DIR)
			.join(filename);
		let tx_file = Path::new(&path).to_path_buf();
		let mut tx_f = File::open(tx_file)?;
		let mut content = String::new();
		tx_f.read_to_string(&mut content)?;
		let tx_bin = util::from_hex(content).unwrap();
		Ok(Some(
			ser::deserialize::<Transaction>(&mut &tx_bin[..], ser::ProtocolVersion(1)).unwrap(),
		))
	}

	fn batch<'a>(
		&'a mut self,
		keychain_mask: Option<&SecretKey>,
	) -> Result<Box<dyn WalletOutputBatch<K> + 'a>, Error> {
		Ok(Box::new(Batch {
			_store: self,
			db: RefCell::new(Some(self.db.batch())),
			keychain: Some(self.keychain(keychain_mask)?),
		}))
	}

	fn batch_no_mask<'a>(&'a mut self) -> Result<Box<dyn WalletOutputBatch<K> + 'a>, Error> {
		Ok(Box::new(Batch {
			_store: self,
			db: RefCell::new(Some(self.db.batch())),
			keychain: None,
		}))
	}

	fn current_child_index<'a>(&mut self, parent_key_id: &Identifier) -> Result<u32, Error> {
		let index = {
			let batch = self.db.batch();
			let deriv_key = to_key(DERIV_PREFIX, &mut parent_key_id.to_bytes().to_vec());
			match batch.get_ser(&deriv_key) {
				Some(s) => match s {
					Serializable::Numeric(n) => n as u32,
					_ => 0,
				},
				None => 0,
			}
		};
		Ok(index)
	}

	fn next_child<'a>(&mut self, keychain_mask: Option<&SecretKey>) -> Result<Identifier, Error> {
		let parent_key_id = self.parent_key_id.clone();
		let mut deriv_idx = {
			let batch = self.db.batch();
			let deriv_key = to_key(DERIV_PREFIX, &mut self.parent_key_id.to_bytes().to_vec());
			match batch.get_ser(&deriv_key) {
				Some(s) => match s {
					Serializable::Numeric(n) => n as u32,
					_ => 0,
				},
				None => 0,
			}
		};
		let mut return_path = self.parent_key_id.to_path();
		return_path.depth = return_path.depth + 1;
		return_path.path[return_path.depth as usize - 1] = ChildNumber::from(deriv_idx);
		deriv_idx = deriv_idx + 1;
		let mut batch = self.batch(keychain_mask)?;
		batch.save_child_index(&parent_key_id, deriv_idx)?;
		batch.commit()?;
		Ok(Identifier::from_path(&return_path))
	}

	fn last_confirmed_height<'a>(&mut self) -> Result<u64, Error> {
		let batch = self.db.batch();
		let height_key = to_key(
			CONFIRMED_HEIGHT_PREFIX,
			&mut self.parent_key_id.to_bytes().to_vec(),
		);
		let last_confirmed_height = match batch.get_ser(&height_key) {
			Some(s) => match s {
				Serializable::Numeric(n) => n,
				_ => 0,
			},
			None => 0,
		};
		Ok(last_confirmed_height)
	}

	fn last_scanned_block<'a>(&mut self) -> Result<ScannedBlockInfo, Error> {
		let batch = self.db.batch();
		let scanned_block_key = to_key(
			LAST_SCANNED_BLOCK,
			&mut LAST_SCANNED_KEY.as_bytes().to_vec(),
		);
		let last_scanned_block = match batch.get_ser(&scanned_block_key) {
			Some(s) => match s {
				Serializable::ScannedBlockInfo(s) => s,
				_ => ScannedBlockInfo {
					height: 0,
					hash: "".to_owned(),
					start_pmmr_index: 0,
					last_pmmr_index: 0,
				},
			},
			None => ScannedBlockInfo {
				height: 0,
				hash: "".to_owned(),
				start_pmmr_index: 0,
				last_pmmr_index: 0,
			},
		};
		Ok(last_scanned_block)
	}

	fn init_status<'a>(&mut self) -> Result<WalletInitStatus, Error> {
		let batch = self.db.batch();
		let init_status_key = to_key(
			WALLET_INIT_STATUS,
			&mut WALLET_INIT_STATUS_KEY.as_bytes().to_vec(),
		);
		let status = match batch.get_ser(&init_status_key) {
			Some(s) => match s {
				Serializable::WalletInitStatus(w) => w,
				_ => WalletInitStatus::InitComplete,
			},
			None => WalletInitStatus::InitComplete,
		};
		Ok(status)
	}
}

/// An atomic batch in which all changes can be committed all at once or
/// discarded on error.
pub struct Batch<'a, C, K>
where
	C: NodeClient,
	K: Keychain,
{
	_store: &'a LMDBBackend<'a, C, K>,
	db: RefCell<Option<db::Batch<'a>>>,
	/// Keychain
	keychain: Option<K>,
}

#[allow(missing_docs)]
impl<'a, C, K> WalletOutputBatch<K> for Batch<'a, C, K>
where
	C: NodeClient,
	K: Keychain,
{
	fn keychain(&mut self) -> &mut K {
		self.keychain.as_mut().unwrap()
	}

	fn save(&mut self, out: OutputData) -> Result<(), Error> {
		// Save the previous output data to the db.
		if let Ok(previous_output) = self.get(&out.key_id, &out.mmr_index) {
			if previous_output != out {
				self.save_output_history(previous_output)?;
			}
		}
		// Save the updated output data to the db.
		{
			let key = match out.mmr_index {
				Some(i) => to_key_u64(OUTPUT_PREFIX, &mut out.key_id.to_bytes().to_vec(), i),
				None => to_key(OUTPUT_PREFIX, &mut out.key_id.to_bytes().to_vec()),
			};
			self.db
				.borrow()
				.as_ref()
				.unwrap()
				.put_ser(&key, Serializable::OutputData(out))?;
		}

		Ok(())
	}

	fn save_output_history(&mut self, out: OutputData) -> Result<(), Error> {
		// Ensure that the previous_output has not been registered in the output history table yet.
		let outputs_in_history_table = self.history_iter().collect::<Vec<_>>();
		let mut output_already_registered = false;

		for mut o in outputs_in_history_table {
			o.key_id = out.key_id.clone();
			if o == out {
				output_already_registered = true;
				break;
			}
		}

		// Save the previous output data to the db.
		if !output_already_registered {
			if let Ok(output_history_id) = self.next_output_history_id() {
				let output_history_key = to_key(
					OUTPUT_HISTORY_PREFIX,
					&mut output_history_id.to_le_bytes().to_vec(),
				);
				self.db
					.borrow()
					.as_ref()
					.unwrap()
					.put_ser(&output_history_key, Serializable::OutputData(out))?;
			}
		}

		Ok(())
	}

	fn get(&self, id: &Identifier, mmr_index: &Option<u64>) -> Result<OutputData, Error> {
		let key = match mmr_index {
			Some(i) => to_key_u64(OUTPUT_PREFIX, &mut id.to_bytes().to_vec(), *i),
			None => to_key(OUTPUT_PREFIX, &mut id.to_bytes().to_vec()),
		};
		Ok(self
			.db
			.borrow()
			.as_ref()
			.unwrap()
			.get_ser(&key)
			.ok_or(Error::NotFoundErr(format!("Key Id: {}", id)))?
			.as_output_data()
			.unwrap())
	}

	fn iter(&self) -> Box<dyn Iterator<Item = OutputData>> {
		let serializables: Vec<_> = self
			.db
			.borrow()
			.as_ref()
			.unwrap()
			.iter(&[OUTPUT_PREFIX])
			.into_iter()
			.filter_map(Serializable::as_output_data)
			.collect();

		Box::new(serializables.into_iter().map(|x| x))
	}

	fn history_iter(&self) -> Box<dyn Iterator<Item = OutputData>> {
		let serializables: Vec<_> = self
			.db
			.borrow()
			.as_ref()
			.unwrap()
			.iter(&[OUTPUT_HISTORY_PREFIX])
			.into_iter()
			.filter_map(Serializable::as_output_data)
			.collect();

		Box::new(serializables.into_iter().map(|x| x))
	}

	fn delete(
		&mut self,
		id: &Identifier,
		mmr_index: &Option<u64>,
		tx_id: &Option<u32>,
	) -> Result<(), Error> {
		// Save the previous output data to the db.
		if let Ok(mut previous_output) = self.get(&id, &mmr_index) {
			self.save_output_history(previous_output.clone())?;
			// Save the output with a deleted status in the output history table.
			previous_output.status = OutputStatus::Deleted;
			previous_output.tx_log_entry = *tx_id;
			self.save_output_history(previous_output)?;
		}

		// Delete the output data.
		{
			let key = match mmr_index {
				Some(i) => to_key_u64(OUTPUT_PREFIX, &mut id.to_bytes().to_vec(), *i),
				None => to_key(OUTPUT_PREFIX, &mut id.to_bytes().to_vec()),
			};
			let _ = self.db.borrow().as_ref().unwrap().delete(&key);
		}

		Ok(())
	}

	fn next_output_history_id(&mut self) -> Result<u32, Error> {
		let mut first_output_history_id = vec![0];
		let output_history_key_id = to_key(OUTPUT_HISTORY_ID_PREFIX, &mut first_output_history_id);
		let last_output_history_id = match self
			.db
			.borrow()
			.as_ref()
			.unwrap()
			.get_ser(&output_history_key_id)
		{
			Some(s) => match s {
				Serializable::Numeric(n) => n as u32,
				_ => 0,
			},
			None => 0,
		};
		self.db.borrow().as_ref().unwrap().put_ser(
			&output_history_key_id,
			Serializable::Numeric((last_output_history_id + 1).into()),
		)?;
		Ok(last_output_history_id)
	}

	fn next_tx_log_id(&mut self, parent_key_id: &Identifier) -> Result<u32, Error> {
		let tx_id_key = to_key(TX_LOG_ID_PREFIX, &mut parent_key_id.to_bytes().to_vec());
		let last_tx_log_id = match self.db.borrow().as_ref().unwrap().get_ser(&tx_id_key) {
			Some(s) => match s {
				Serializable::Numeric(n) => n as u32,
				_ => 0,
			},
			None => 0,
		};
		self.db.borrow().as_ref().unwrap().put_ser(
			&tx_id_key,
			Serializable::Numeric((last_tx_log_id + 1).into()),
		)?;
		Ok(last_tx_log_id)
	}

	fn tx_log_iter(&self) -> Box<dyn Iterator<Item = TxLogEntry>> {
		let serializables: Vec<_> = self
			.db
			.borrow()
			.as_ref()
			.unwrap()
			.iter(&[TX_LOG_ENTRY_PREFIX])
			.into_iter()
			.filter_map(Serializable::as_txlogentry)
			.collect();

		Box::new(serializables.into_iter().map(|x| x))
	}

	fn save_last_confirmed_height(
		&mut self,
		parent_key_id: &Identifier,
		height: u64,
	) -> Result<(), Error> {
		let height_key = to_key(
			CONFIRMED_HEIGHT_PREFIX,
			&mut parent_key_id.to_bytes().to_vec(),
		);
		self.db
			.borrow()
			.as_ref()
			.unwrap()
			.put_ser(&height_key, Serializable::Numeric(height))?;
		Ok(())
	}

	fn save_last_scanned_block(&mut self, block_info: ScannedBlockInfo) -> Result<(), Error> {
		let pmmr_index_key = to_key(
			LAST_SCANNED_BLOCK,
			&mut LAST_SCANNED_KEY.as_bytes().to_vec(),
		);
		self.db
			.borrow()
			.as_ref()
			.unwrap()
			.put_ser(&pmmr_index_key, Serializable::ScannedBlockInfo(block_info))?;
		Ok(())
	}

	fn save_init_status(&mut self, value: WalletInitStatus) -> Result<(), Error> {
		let init_status_key = to_key(
			WALLET_INIT_STATUS,
			&mut WALLET_INIT_STATUS_KEY.as_bytes().to_vec(),
		);
		self.db
			.borrow()
			.as_ref()
			.unwrap()
			.put_ser(&init_status_key, Serializable::WalletInitStatus(value))?;
		Ok(())
	}

	fn save_child_index(&mut self, parent_id: &Identifier, child_n: u32) -> Result<(), Error> {
		let deriv_key = to_key(DERIV_PREFIX, &mut parent_id.to_bytes().to_vec());
		self.db
			.borrow()
			.as_ref()
			.unwrap()
			.put_ser(&deriv_key, Serializable::Numeric(child_n.into()))?;
		Ok(())
	}

	fn save_tx_log_entry(
		&mut self,
		tx_in: TxLogEntry,
		parent_id: &Identifier,
	) -> Result<(), Error> {
		let tx_log_key = to_key_u64(
			TX_LOG_ENTRY_PREFIX,
			&mut parent_id.to_bytes().to_vec(),
			tx_in.id as u64,
		);
		self.db
			.borrow()
			.as_ref()
			.unwrap()
			.put_ser(&tx_log_key, Serializable::TxLogEntry(tx_in))?;
		Ok(())
	}

	fn save_acct_path(&mut self, mapping: AcctPathMapping) -> Result<(), Error> {
		let acct_key = to_key(
			ACCOUNT_PATH_MAPPING_PREFIX,
			&mut mapping.label.as_bytes().to_vec(),
		);
		self.db
			.borrow()
			.as_ref()
			.unwrap()
			.put_ser(&acct_key, Serializable::AcctPathMapping(mapping))?;
		Ok(())
	}

	fn acct_path_iter(&self) -> Box<dyn Iterator<Item = AcctPathMapping>> {
		let serializables: Vec<_> = self
			.db
			.borrow()
			.as_ref()
			.unwrap()
			.iter(&[ACCOUNT_PATH_MAPPING_PREFIX])
			.into_iter()
			.filter_map(Serializable::as_acct_path_mapping)
			.collect();

		Box::new(serializables.into_iter().map(|x| x))
	}

	fn lock_output(&mut self, out: &mut OutputData) -> Result<(), Error> {
		out.lock();
		self.save(out.clone())
	}

	fn save_private_context(
		&mut self,
		slate_id: &[u8],
		participant_id: usize,
		ctx: &Context,
	) -> Result<(), Error> {
		let ctx_key = to_key_u64(
			PRIVATE_TX_CONTEXT_PREFIX,
			&mut slate_id.to_vec(),
			participant_id as u64,
		);
		let (blind_xor_key, nonce_xor_key) = private_ctx_xor_keys(self.keychain(), slate_id)?;

		let mut s_ctx = ctx.clone();
		for i in 0..SECRET_KEY_SIZE {
			s_ctx.sec_key.0[i] = s_ctx.sec_key.0[i] ^ blind_xor_key[i];
			s_ctx.sec_nonce.0[i] = s_ctx.sec_nonce.0[i] ^ nonce_xor_key[i];
		}

		self.db
			.borrow()
			.as_ref()
			.unwrap()
			.put_ser(&ctx_key, Serializable::Context(s_ctx))?;
		Ok(())
	}

	fn delete_private_context(
		&mut self,
		slate_id: &[u8],
		participant_id: usize,
	) -> Result<(), Error> {
		let ctx_key = to_key_u64(
			PRIVATE_TX_CONTEXT_PREFIX,
			&mut slate_id.to_vec(),
			participant_id as u64,
		);
		self.db
			.borrow()
			.as_ref()
			.unwrap()
			.delete(&ctx_key)
			.map_err(|e| Error::Backend(format!("{}", e)))
	}

	fn commit(&self) -> Result<(), Error> {
		self.db.replace(None);
		Ok(())
	}
}
