// Copyright 2020 The Exonum Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Cryptocurrency database schema.

use exonum::crypto::{Hash, PublicKey};
use exonum::merkledb::{
    access::{Access, FromAccess, RawAccessMut},
    Group, ObjectHash, ProofListIndex, RawProofMapIndex,
};
use exonum_derive::{FromAccess, RequireArtifact};

use crate::{wallet::Wallet, INITIAL_BALANCE};

/// Database schema for the cryptocurrency.
///
/// Note that the schema is crate-private, but it has a public part.
#[derive(Debug, FromAccess)]
pub(crate) struct SchemaImpl<T: Access> {
    /// Public part of the schema.
    #[from_access(flatten)]
    pub public: Schema<T>,
    /// History for specific wallets.
    pub wallet_history: Group<T, PublicKey, ProofListIndex<T::Base, Hash>>,
}

/// Public part of the cryptocurrency schema.
#[derive(Debug, FromAccess, RequireArtifact)]
pub struct Schema<T: Access> {
    /// Map of wallet keys to information about the corresponding account.
    pub wallets: RawProofMapIndex<T::Base, PublicKey, Wallet>,
}

impl<T: Access> SchemaImpl<T> {
    pub fn new(access: T) -> Self {
        Self::from_root(access).unwrap()
    }
}

impl<T> SchemaImpl<T>
where
    T: Access,
    T::Base: RawAccessMut,
{
    /// Increase balance of the wallet and append new record to its history.
    ///
    /// Panics if there is no wallet with given public key.
    pub(crate) fn increase_wallet_balance(
        &mut self,
        wallet: Wallet,
        amount: u64,
        transaction: Hash,
    ) {
        let mut history = self.wallet_history.get(&wallet.pub_key);
        history.push(transaction);
        let history_hash = history.object_hash();
        let balance = wallet.balance;
        let wallet = wallet.set_balance(balance + amount, &history_hash);
        let wallet_key = wallet.pub_key;
        self.public.wallets.put(&wallet_key, wallet);
    }

    /// Decrease balance of the wallet and append new record to its history.
    ///
    /// Panics if there is no wallet with given public key.
    pub(crate) fn decrease_wallet_balance(
        &mut self,
        wallet: Wallet,
        amount: u64,
        transaction: Hash,
    ) {
        let mut history = self.wallet_history.get(&wallet.pub_key);
        history.push(transaction);
        let history_hash = history.object_hash();
        let balance = wallet.balance;
        let wallet = wallet.set_balance(balance - amount, &history_hash);
        let wallet_key = wallet.pub_key;
        self.public.wallets.put(&wallet_key, wallet);
    }

    /// Create new wallet and append first record to its history.
    pub(crate) fn create_wallet(&mut self, key: &PublicKey, name: &str, transaction: Hash) {
        let mut history = self.wallet_history.get(key);
        history.push(transaction);
        let history_hash = history.object_hash();
        let wallet = Wallet::new(key, name, INITIAL_BALANCE, history.len(), &history_hash);
        self.public.wallets.put(key, wallet);
    }
}
