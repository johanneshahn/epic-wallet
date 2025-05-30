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

//! Tor Configuration + Onion (Hidden) Service operations
use crate::util::secp::key::SecretKey;
use crate::Error;
use epic_wallet_libwallet::address;

use ed25519_dalek::ExpandedSecretKey;
use ed25519_dalek::PublicKey as DalekPublicKey;
use ed25519_dalek::SecretKey as DalekSecretKey;

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, MAIN_SEPARATOR};

const SEC_KEY_FILE: &'static str = "hs_ed25519_secret_key";
const PUB_KEY_FILE: &'static str = "hs_ed25519_public_key";
const HOSTNAME_FILE: &'static str = "hostname";
const TORRC_FILE: &'static str = "torrc";
const TOR_DATA_DIR: &'static str = "data";
const AUTH_CLIENTS_DIR: &'static str = "authorized_clients";
const HIDDEN_SERVICES_DIR: &'static str = "onion_service_addresses";

#[cfg(unix)]
fn set_permissions(file_path: &str) -> Result<(), Error> {
	use std::os::unix::prelude::*;
	fs::set_permissions(file_path, fs::Permissions::from_mode(0o700)).map_err(|_| Error::IO)?;
	Ok(())
}

#[cfg(windows)]
fn set_permissions(_file_path: &str) -> Result<(), Error> {
	Ok(())
}

struct TorRcConfigItem {
	pub name: String,
	pub value: String,
}

impl TorRcConfigItem {
	/// Create new
	pub fn new(name: &str, value: &str) -> Self {
		Self {
			name: name.into(),
			value: value.into(),
		}
	}
}

struct TorRcConfig {
	pub items: Vec<TorRcConfigItem>,
}

impl TorRcConfig {
	/// Create new
	pub fn new() -> Self {
		Self { items: vec![] }
	}

	/// add item
	pub fn add_item(&mut self, name: &str, value: &str) {
		self.items.push(TorRcConfigItem::new(name, value));
	}

	/// write to file
	pub fn write_to_file(&self, file_path: &str) -> Result<(), Error> {
		let mut file = File::create(file_path).map_err(|_| Error::IO)?;
		for item in &self.items {
			file.write_all(item.name.as_bytes())
				.map_err(|_| Error::IO)?;
			file.write_all(b" ").map_err(|_| Error::IO)?;
			file.write_all(item.value.as_bytes())
				.map_err(|_| Error::IO)?;
			file.write_all(b"\n").map_err(|_| Error::IO)?;
		}
		Ok(())
	}
}

/// helper to get address
pub fn onion_address_from_seckey(sec_key: &SecretKey) -> Result<String, Error> {
	let (_, d_pub_key) = address::ed25519_keypair(sec_key)?;
	Ok(address::onion_v3_from_pubkey(&d_pub_key)?)
}

pub fn create_onion_service_sec_key_file(
	os_directory: &str,
	sec_key: &DalekSecretKey,
) -> Result<(), Error> {
	let key_file_path = &format!("{}{}{}", os_directory, MAIN_SEPARATOR, SEC_KEY_FILE);
	let mut file = File::create(key_file_path).map_err(|_| Error::IO)?;
	// Tag is always 32 bytes, so pad with null zeroes
	file.write("== ed25519v1-secret: type0 ==\0\0\0".as_bytes())
		.map_err(|_| Error::IO)?;
	let expanded_skey: ExpandedSecretKey = ExpandedSecretKey::from(sec_key);
	file.write_all(&expanded_skey.to_bytes())
		.map_err(|_| Error::IO)?;
	Ok(())
}

pub fn create_onion_service_pub_key_file(
	os_directory: &str,
	pub_key: &DalekPublicKey,
) -> Result<(), Error> {
	let key_file_path = &format!("{}{}{}", os_directory, MAIN_SEPARATOR, PUB_KEY_FILE);
	let mut file = File::create(key_file_path).map_err(|_| Error::IO)?;
	// Tag is always 32 bytes, so pad with null zeroes
	file.write("== ed25519v1-public: type0 ==\0\0\0".as_bytes())
		.map_err(|_| Error::IO)?;
	file.write_all(pub_key.as_bytes()).map_err(|_| Error::IO)?;
	Ok(())
}

pub fn create_onion_service_hostname_file(os_directory: &str, hostname: &str) -> Result<(), Error> {
	let file_path = &format!("{}{}{}", os_directory, MAIN_SEPARATOR, HOSTNAME_FILE);
	let mut file = File::create(file_path).map_err(|_| Error::IO)?;
	file.write_all(&format!("{}.onion\n", hostname).as_bytes())
		.map_err(|_| Error::IO)?;
	Ok(())
}

pub fn create_onion_auth_clients_dir(os_directory: &str) -> Result<(), Error> {
	let auth_dir_path = &format!("{}{}{}", os_directory, MAIN_SEPARATOR, AUTH_CLIENTS_DIR);
	fs::create_dir_all(auth_dir_path).map_err(|_| Error::IO)?;
	Ok(())
}
/// output an onion service config for the secret key, and return the address
pub fn output_onion_service_config(
	tor_config_directory: &str,
	sec_key: &SecretKey,
) -> Result<String, Error> {
	let (_, d_pub_key) = address::ed25519_keypair(&sec_key)?;
	let address = address::onion_v3_from_pubkey(&d_pub_key)?;
	let hs_dir_file_path = format!(
		"{}{}{}{}{}",
		tor_config_directory, MAIN_SEPARATOR, HIDDEN_SERVICES_DIR, MAIN_SEPARATOR, address
	);

	// If file already exists, don't overwrite it, just return address
	if Path::new(&hs_dir_file_path).exists() {
		return Ok(address);
	}

	// create directory if it doesn't exist
	fs::create_dir_all(&hs_dir_file_path).map_err(|_| Error::IO)?;

	let (d_sec_key, d_pub_key) = address::ed25519_keypair(&sec_key)?;
	create_onion_service_sec_key_file(&hs_dir_file_path, &d_sec_key)?;
	create_onion_service_pub_key_file(&hs_dir_file_path, &d_pub_key)?;
	create_onion_service_hostname_file(&hs_dir_file_path, &address)?;
	create_onion_auth_clients_dir(&hs_dir_file_path)?;

	set_permissions(&hs_dir_file_path)?;

	Ok(address)
}

/// output torrc file given a list of hidden service directories
pub fn output_torrc(
	tor_config_directory: &str,
	wallet_listener_addr: &str,
	socks_port: &str,
	service_dirs: &Vec<String>,
) -> Result<(), Error> {
	let torrc_file_path = format!("{}{}{}", tor_config_directory, MAIN_SEPARATOR, TORRC_FILE);

	let tor_data_dir = format!("./{}", TOR_DATA_DIR);

	let mut props = TorRcConfig::new();
	props.add_item("SocksPort", socks_port);
	props.add_item("DataDirectory", &tor_data_dir);

	for dir in service_dirs {
		let service_file_name = format!("./{}{}{}", HIDDEN_SERVICES_DIR, MAIN_SEPARATOR, dir);
		props.add_item("HiddenServiceDir", &service_file_name);
		props.add_item("HiddenServicePort", &format!("80 {}", wallet_listener_addr));
	}

	props.write_to_file(&torrc_file_path)?;

	Ok(())
}

/// output entire tor config for a list of secret keys
pub fn output_tor_listener_config(
	tor_config_directory: &str,
	wallet_listener_addr: &str,
	listener_keys: &Vec<SecretKey>,
) -> Result<(), Error> {
	let tor_data_dir = format!("{}{}{}", tor_config_directory, MAIN_SEPARATOR, TOR_DATA_DIR);

	// create data directory if it doesn't exist
	fs::create_dir_all(&tor_data_dir).map_err(|_| Error::IO)?;

	let mut service_dirs = vec![];

	for k in listener_keys {
		let service_dir = output_onion_service_config(tor_config_directory, &k)?;
		service_dirs.push(service_dir);
	}

	// hidden service listener doesn't need a socks port
	output_torrc(
		tor_config_directory,
		wallet_listener_addr,
		"0",
		&service_dirs,
	)?;

	Ok(())
}

/// output tor config for a send
pub fn output_tor_sender_config(
	tor_config_dir: &str,
	socks_listener_addr: &str,
) -> Result<(), Error> {
	// create data directory if it doesn't exist
	fs::create_dir_all(&tor_config_dir).map_err(|_| Error::IO)?;

	output_torrc(tor_config_dir, "", socks_listener_addr, &vec![])?;

	Ok(())
}

pub fn is_tor_address(input: &str) -> Result<(), Error> {
	match address::pubkey_from_onion_v3(input) {
		Ok(_) => Ok(()),
		Err(e) => {
			let msg = format!("{}", e);
			Err(Error::NotOnion(msg).to_owned())?
		}
	}
}

pub fn complete_tor_address(input: &str) -> Result<String, Error> {
	let _ = is_tor_address(input)?;
	let mut input = input.to_uppercase();
	if !input.starts_with("HTTP://") && !input.starts_with("HTTPS://") {
		input = format!("HTTP://{}", input);
	}
	if !input.ends_with(".ONION") {
		input = format!("{}.ONION", input);
	}
	Ok(input.to_lowercase())
}

#[cfg(test)]
mod tests {
	use super::*;

	use rand::rng;
	use rand::rngs::mock::StepRng;

	use crate::util::{self, secp, static_secp_instance};

	pub fn clean_output_dir(test_dir: &str) {
		let _ = fs::remove_dir_all(test_dir);
	}

	pub fn setup(test_dir: &str) {
		util::init_test_logger();
		clean_output_dir(test_dir);
	}

	#[test]
	fn gen_ed25519_pub_key() -> Result<(), Error> {
		let secp_inst = static_secp_instance();
		let secp = secp_inst.lock();
		let mut test_rng = StepRng::new(1234567890u64, 1);
		let sec_key = secp::key::SecretKey::new(&secp, &mut test_rng);
		println!("{:?}", sec_key);
		let (_, d_pub_key) = address::ed25519_keypair(&sec_key)?;
		println!("{:?}", d_pub_key);
		// some randoms
		for _ in 0..1000 {
			let sec_key = secp::key::SecretKey::new(&secp, &mut rng());
			let (_, _) = address::ed25519_keypair(&sec_key)?;
		}
		Ok(())
	}

	#[test]
	fn gen_onion_address() -> Result<(), Error> {
		let secp_inst = static_secp_instance();
		let secp = secp_inst.lock();
		let mut test_rng = StepRng::new(1234567890u64, 1);
		let sec_key = secp::key::SecretKey::new(&secp, &mut test_rng);
		println!("{:?}", sec_key);
		let (_, d_pub_key) = address::ed25519_keypair(&sec_key)?;
		let address = address::onion_v3_from_pubkey(&d_pub_key)?;
		assert_eq!(
			"kcgiy5g6m76nzlzz4vyqmgdv34f6yokdqwfhdhaafanpo5p4fceibyid",
			address
		);
		println!("{}", address);
		Ok(())
	}

	#[test]
	fn test_service_config() -> Result<(), Error> {
		let test_dir = "target/test_output/onion_service";
		setup(test_dir);
		let secp_inst = static_secp_instance();
		let secp = secp_inst.lock();
		let mut test_rng = StepRng::new(1234567890u64, 1);
		let sec_key = secp::key::SecretKey::new(&secp, &mut test_rng);
		output_onion_service_config(test_dir, &sec_key)?;
		clean_output_dir(test_dir);
		Ok(())
	}

	#[test]
	fn test_output_tor_config() -> Result<(), Error> {
		let test_dir = "./target/test_output/tor";
		setup(test_dir);
		let secp_inst = static_secp_instance();
		let secp = secp_inst.lock();
		let mut test_rng = StepRng::new(1234567890u64, 1);
		let sec_key = secp::key::SecretKey::new(&secp, &mut test_rng);
		output_tor_listener_config(test_dir, "127.0.0.1:3415", &vec![sec_key])?;
		clean_output_dir(test_dir);
		Ok(())
	}

	#[test]
	fn test_is_tor_address() -> Result<(), Error> {
		assert!(is_tor_address("2a6at2obto3uvkpkitqp4wxcg6u36qf534eucbskqciturczzc5suyid").is_ok());
		assert!(is_tor_address("2a6at2obto3uvkpkitqp4wxcg6u36qf534eucbskqciturczzc5suyid").is_ok());
		assert!(is_tor_address("kcgiy5g6m76nzlzz4vyqmgdv34f6yokdqwfhdhaafanpo5p4fceibyid").is_ok());
		assert!(is_tor_address(
			"http://kcgiy5g6m76nzlzz4vyqmgdv34f6yokdqwfhdhaafanpo5p4fceibyid.onion"
		)
		.is_ok());
		assert!(is_tor_address(
			"https://kcgiy5g6m76nzlzz4vyqmgdv34f6yokdqwfhdhaafanpo5p4fceibyid.onion"
		)
		.is_ok());
		assert!(
			is_tor_address("http://kcgiy5g6m76nzlzz4vyqmgdv34f6yokdqwfhdhaafanpo5p4fceibyid")
				.is_ok()
		);
		assert!(
			is_tor_address("kcgiy5g6m76nzlzz4vyqmgdv34f6yokdqwfhdhaafanpo5p4fceibyid.onion")
				.is_ok()
		);
		// address too short
		assert!(is_tor_address(
			"http://kcgiy5g6m76nzlz4vyqmgdv34f6yokdqwfhdhaafanpo5p4fceibyid.onion"
		)
		.is_err());
		assert!(is_tor_address("kcgiy5g6m76nzlz4vyqmgdv34f6yokdqwfhdhaafanpo5p4fceibyid").is_err());
		Ok(())
	}

	#[test]
	fn test_complete_tor_address() -> Result<(), Error> {
		assert_eq!(
			"http://2a6at2obto3uvkpkitqp4wxcg6u36qf534eucbskqciturczzc5suyid.onion",
			complete_tor_address("2a6at2obto3uvkpkitqp4wxcg6u36qf534eucbskqciturczzc5suyid")
				.unwrap()
		);
		assert_eq!(
			"http://2a6at2obto3uvkpkitqp4wxcg6u36qf534eucbskqciturczzc5suyid.onion",
			complete_tor_address("http://2a6at2obto3uvkpkitqp4wxcg6u36qf534eucbskqciturczzc5suyid")
				.unwrap()
		);
		assert_eq!(
			"http://2a6at2obto3uvkpkitqp4wxcg6u36qf534eucbskqciturczzc5suyid.onion",
			complete_tor_address("2a6at2obto3uvkpkitqp4wxcg6u36qf534eucbskqciturczzc5suyid.onion")
				.unwrap()
		);
		assert!(
			complete_tor_address("2a6at2obto3uvkpkitqp4wxcg6u36qf534eucbskqciturczzc5suyi")
				.is_err()
		);
		Ok(())
	}
}
