// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    constants,
    error::Error,
    secure_backend::StorageLocation::{LocalStorage, RemoteStorage},
    SecureBackends,
};
use libra_config::config::HANDSHAKE_VERSION;
use libra_crypto::{ed25519::Ed25519PublicKey, x25519, ValidCryptoMaterial};
use libra_global_constants::{
    CONSENSUS_KEY, FULLNODE_NETWORK_KEY, OPERATOR_ACCOUNT, OPERATOR_KEY, OWNER_ACCOUNT, OWNER_KEY,
    VALIDATOR_NETWORK_KEY,
};
use libra_network_address::{
    encrypted::{
        RawEncNetworkAddress, TEST_SHARED_VAL_NETADDR_KEY, TEST_SHARED_VAL_NETADDR_KEY_VERSION,
    },
    NetworkAddress, RawNetworkAddress,
};
use libra_secure_storage::{CryptoStorage, KVStorage, Storage, Value};
use libra_secure_time::{RealTimeService, TimeService};
use libra_types::{
    account_address::{self, AccountAddress},
    chain_id::ChainId,
    transaction::{RawTransaction, Script, SignedTransaction, Transaction},
};
use std::{convert::TryFrom, str::FromStr, time::Duration};
use structopt::StructOpt;

// TODO(davidiw) add operator_address, since that will eventually be the identity producing this.
#[derive(Debug, StructOpt)]
pub struct ValidatorConfig {
    #[structopt(long)]
    owner_name: String,
    #[structopt(long)]
    validator_address: NetworkAddress,
    #[structopt(long)]
    fullnode_address: NetworkAddress,
    #[structopt(flatten)]
    backends: SecureBackends,
    #[structopt(long)]
    chain_id: ChainId,
}

impl ValidatorConfig {
    pub fn execute(self) -> Result<Transaction, Error> {
        // Fetch the owner key from remote storage using the owner_name and derive an address
        let owner_account = self.fetch_owner_account()?;

        // Create the validator config script for the validator node
        let validator_config_script = self.create_validator_config_script(owner_account)?;

        // Create and sign the validator-config transaction
        let validator_config_tx =
            self.create_validator_config_transaction(validator_config_script)?;

        // Write validator config to local storage to save for verification later on
        let mut local_storage = self.backends.local.create_storage(LocalStorage)?;
        local_storage
            .set(
                constants::VALIDATOR_CONFIG,
                Value::Transaction(validator_config_tx.clone()),
            )
            .map_err(|e| {
                Error::LocalStorageWriteError(constants::VALIDATOR_CONFIG, e.to_string())
            })?;

        // Save the owner account in local storage for deployment later on
        local_storage
            .set(OWNER_ACCOUNT, Value::String(owner_account.to_string()))
            .map_err(|e| Error::LocalStorageWriteError(OWNER_ACCOUNT, e.to_string()))?;

        // Upload the validator config to shared storage
        match self.backends.remote {
            None => return Err(Error::RemoteStorageMissing),
            Some(remote_config) => {
                let mut remote_storage = remote_config.create_storage(RemoteStorage)?;
                remote_storage
                    .set(
                        constants::VALIDATOR_CONFIG,
                        Value::Transaction(validator_config_tx.clone()),
                    )
                    .map_err(|e| {
                        Error::RemoteStorageWriteError(constants::VALIDATOR_CONFIG, e.to_string())
                    })?;
            }
        };

        Ok(validator_config_tx)
    }

    /// Creates and returns a validator config script using the keys stored in local storage. The
    /// validator address will be the given owner account address.
    fn create_validator_config_script(
        &self,
        owner_account: AccountAddress,
    ) -> Result<Script, Error> {
        // Retrieve keys from local storage
        let local_storage = self.backends.local.clone().create_storage(LocalStorage)?;
        let consensus_key = ed25519_from_storage(CONSENSUS_KEY, &local_storage)?;
        let fullnode_network_key = x25519_from_storage(FULLNODE_NETWORK_KEY, &local_storage)?;
        let validator_network_key = x25519_from_storage(VALIDATOR_NETWORK_KEY, &local_storage)?;

        // Only supports one address for now
        let addr_idx = 0;

        // Append ln-noise-ik and ln-handshake protocols to base network addresses
        // and encrypt the validator address.
        let validator_address = self
            .validator_address
            .clone()
            .append_prod_protos(validator_network_key, HANDSHAKE_VERSION);
        let raw_validator_address =
            RawNetworkAddress::try_from(&validator_address).map_err(|e| {
                Error::UnexpectedError(format!(
                    "error serializing validator address: \"{}\", error: {}",
                    validator_address, e
                ))
            })?;
        // TODO(davidiw): In genesis this is irrelevant -- afterward we need to obtain the
        // current sequence number by querying the blockchain.
        let sequence_number = 0;
        let enc_validator_address = raw_validator_address.encrypt(
            &TEST_SHARED_VAL_NETADDR_KEY,
            TEST_SHARED_VAL_NETADDR_KEY_VERSION,
            &owner_account,
            sequence_number,
            addr_idx,
        );
        let raw_enc_validator_address = RawEncNetworkAddress::try_from(&enc_validator_address)
            .map_err(|e| {
                Error::UnexpectedError(format!(
                    "error serializing encrypted validator address: {:?}, error: {}",
                    enc_validator_address, e
                ))
            })?;
        let fullnode_address = self
            .fullnode_address
            .clone()
            .append_prod_protos(fullnode_network_key, HANDSHAKE_VERSION);
        let raw_fullnode_address = RawNetworkAddress::try_from(&fullnode_address).map_err(|e| {
            Error::UnexpectedError(format!(
                "error serializing fullnode address: \"{}\", error: {}",
                fullnode_address, e
            ))
        })?;

        // Generate the validator config script
        // TODO(philiphayes): remove network identity pubkey field from struct when
        // transition complete
        Ok(transaction_builder::encode_set_validator_config_script(
            owner_account,
            consensus_key.to_bytes().to_vec(),
            validator_network_key.to_bytes(),
            raw_enc_validator_address.into(),
            fullnode_network_key.to_bytes(),
            raw_fullnode_address.into(),
        ))
    }

    /// Creates and returns a signed validator-config transaction.
    fn create_validator_config_transaction(&self, script: Script) -> Result<Transaction, Error> {
        let mut local_storage = self.backends.local.clone().create_storage(LocalStorage)?;
        let operator_key = ed25519_from_storage(OPERATOR_KEY, &local_storage)?;
        let operator_address_string = local_storage
            .get(OPERATOR_ACCOUNT)
            .and_then(|v| v.value.string())
            .map_err(|e| Error::LocalStorageReadError(OPERATOR_ACCOUNT, e.to_string()))?;
        let operator_address = AccountAddress::from_str(&operator_address_string)
            .map_err(|e| Error::BackendParsingError(e.to_string()))?;

        // TODO(joshlind): In genesis the sequence number is irrelevant. After genesis we need to
        // obtain the current sequence number by querying the blockchain.
        let sequence_number = 0;
        let expiration_time = RealTimeService::new().now() + constants::TXN_EXPIRATION_SECS;
        let raw_transaction = RawTransaction::new_script(
            operator_address,
            sequence_number,
            script,
            constants::MAX_GAS_AMOUNT,
            constants::GAS_UNIT_PRICE,
            constants::GAS_CURRENCY_CODE.to_owned(),
            Duration::from_secs(expiration_time),
            self.chain_id,
        );

        let signature = local_storage
            .sign(OPERATOR_KEY, &raw_transaction)
            .map_err(|e| {
                Error::LocalStorageSigningError("validator-config", OPERATOR_KEY, e.to_string())
            })?;
        let signed_txn = SignedTransaction::new(raw_transaction, operator_key, signature);
        Ok(Transaction::UserTransaction(signed_txn))
    }

    /// Retrieves the owner key from the remote storage using the owner name given by
    /// the validator-config command, and uses this key to derive an owner account address.
    /// If a remote storage path is not specified, returns an error.
    fn fetch_owner_account(&self) -> Result<AccountAddress, Error> {
        match self.backends.remote.clone() {
            None => Err(Error::RemoteStorageMissing),
            Some(owner_config) => {
                let owner_config = owner_config.set_namespace(self.owner_name.clone());

                let owner_storage = owner_config.create_storage(RemoteStorage)?;
                let owner_key = owner_storage
                    .get(OWNER_KEY)
                    .map_err(|e| Error::RemoteStorageReadError(OWNER_KEY, e.to_string()))?
                    .value
                    .ed25519_public_key()
                    .map_err(|e| Error::RemoteStorageReadError(OWNER_KEY, e.to_string()))?;
                Ok(account_address::from_public_key(&owner_key))
            }
        }
    }
}

fn ed25519_from_storage(
    key_name: &'static str,
    storage: &Storage,
) -> Result<Ed25519PublicKey, Error> {
    Ok(storage
        .get_public_key(key_name)
        .map_err(|e| Error::LocalStorageReadError(key_name, e.to_string()))?
        .public_key)
}

fn x25519_from_storage(
    key_name: &'static str,
    storage: &Storage,
) -> Result<x25519::PublicKey, Error> {
    let edkey = ed25519_from_storage(key_name, storage)?;
    x25519::PublicKey::from_ed25519_public_bytes(&edkey.to_bytes())
        .map_err(|e| Error::UnexpectedError(e.to_string()))
}
