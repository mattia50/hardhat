use anyhow::anyhow;
use hashbrown::HashMap;
use rethnet_eth::{Address, Bytes, H256, U256};
use revm::{Account, AccountInfo, Bytecode, Database, DatabaseCommit, KECCAK_EMPTY};

use crate::DatabaseDebug;

/// A database consisting of layers.
#[derive(Debug)]
pub struct LayeredDatabase<Layer> {
    stack: Vec<Layer>,
}

impl<Layer> LayeredDatabase<Layer> {
    /// Creates a [`LayeredDatabase`] with the provided layer at the bottom.
    pub fn with_layer(layer: Layer) -> Self {
        Self { stack: vec![layer] }
    }

    /// Returns the index of the top layer.
    pub fn last_layer_id(&self) -> usize {
        self.stack.len() - 1
    }

    /// Returns a mutable reference to the top layer.
    pub fn last_layer_mut(&mut self) -> &mut Layer {
        // The `LayeredDatabase` always has at least one layer
        self.stack.last_mut().unwrap()
    }

    /// Adds the provided layer to the top, returning its index and a
    /// mutable reference to the layer.
    pub fn add_layer(&mut self, layer: Layer) -> (usize, &mut Layer) {
        let layer_id = self.stack.len();
        self.stack.push(layer);
        (layer_id, self.stack.last_mut().unwrap())
    }

    /// Reverts to the layer with specified `layer_id`, removing all
    /// layers above it.
    pub fn revert_to_layer(&mut self, layer_id: usize) {
        assert!(layer_id < self.stack.len(), "Invalid layer id.");
        self.stack.truncate(layer_id + 1);
    }

    /// Returns an iterator over the object's layers.
    pub fn iter(&self) -> impl Iterator<Item = &Layer> {
        self.stack.iter().rev()
    }
}

impl<Layer: Default> LayeredDatabase<Layer> {
    /// Adds a default layer to the top, returning its index and a
    /// mutable reference to the layer.
    pub fn add_layer_default(&mut self) -> (usize, &mut Layer) {
        self.add_layer(Layer::default())
    }
}

impl<Layer: Default> Default for LayeredDatabase<Layer> {
    fn default() -> Self {
        Self {
            stack: vec![Layer::default()],
        }
    }
}

/// A layer with information needed for [`Rethnet`].
#[derive(Debug, Default)]
pub struct RethnetLayer {
    /// Address -> AccountInfo
    account_infos: HashMap<Address, AccountInfo>,
    /// Address -> Storage
    storage: HashMap<Address, HashMap<U256, U256>>,
    /// Code hash -> Address
    contracts: HashMap<H256, Bytes>,
    /// Block number -> Block hash
    block_hashes: HashMap<U256, H256>,
}

impl RethnetLayer {
    /// Creates a `RethnetLayer` with the provided genesis accounts.
    pub fn with_genesis_accounts(genesis_accounts: HashMap<Address, AccountInfo>) -> Self {
        Self {
            account_infos: genesis_accounts,
            ..Default::default()
        }
    }

    /// Insert the `AccountInfo` with at the specified `address`.
    pub fn insert_account(&mut self, address: Address, mut account_info: AccountInfo) {
        if let Some(code) = account_info.code.take() {
            if !code.is_empty() {
                account_info.code_hash = code.hash();
                self.contracts.insert(code.hash(), code.bytes().clone());
            }
        }

        if account_info.code_hash.is_zero() {
            account_info.code_hash = KECCAK_EMPTY;
        }

        self.account_infos.insert(address, account_info);
    }
}

impl LayeredDatabase<RethnetLayer> {
    /// Retrieves a reference to the account corresponding to the address, if it exists.
    pub fn account(&self, address: &Address) -> Option<&AccountInfo> {
        self.iter()
            .find_map(|layer| layer.account_infos.get(address))
    }

    /// Retrieves a mutable reference to the account corresponding to the address, if it exists.
    pub fn account_mut(&mut self, address: &Address) -> Option<&mut AccountInfo> {
        // WORKAROUND: https://blog.rust-lang.org/2022/08/05/nll-by-default.html
        if self.last_layer_mut().account_infos.contains_key(address) {
            return Some(
                self.last_layer_mut()
                    .account_infos
                    .get_mut(address)
                    .unwrap(),
            );
        }

        self.account(address).cloned().map(|account_info| {
            self.last_layer_mut()
                .account_infos
                .insert_unique_unchecked(address.clone(), account_info)
                .1
        })
    }

    /// Retrieves a mutable reference to the account corresponding to the address, if it exists.
    /// Otherwise, inserts a new account.
    pub fn account_or_insert_mut(&mut self, address: &Address) -> &mut AccountInfo {
        // WORKAROUND: https://blog.rust-lang.org/2022/08/05/nll-by-default.html
        if self.last_layer_mut().account_infos.contains_key(address) {
            return self
                .last_layer_mut()
                .account_infos
                .get_mut(address)
                .unwrap();
        }

        let account_info = self.account(address).cloned().unwrap_or(AccountInfo {
            balance: U256::ZERO,
            nonce: 0,
            code_hash: KECCAK_EMPTY,
            code: None,
        });

        self.last_layer_mut()
            .account_infos
            .insert_unique_unchecked(address.clone(), account_info)
            .1
    }
}

impl Database for LayeredDatabase<RethnetLayer> {
    type Error = anyhow::Error;

    fn basic(&mut self, address: Address) -> anyhow::Result<Option<AccountInfo>> {
        let account = self
            .iter()
            .find_map(|layer| layer.account_infos.get(&address).cloned());

        log::debug!("account with address `{}`: {:?}", address, account);

        // TODO: Move this out of LayeredDatabase when forking
        Ok(account.or(Some(AccountInfo {
            balance: U256::ZERO,
            nonce: 0,
            code_hash: KECCAK_EMPTY,
            code: None,
        })))
    }

    fn code_by_hash(&mut self, code_hash: H256) -> anyhow::Result<Bytecode> {
        self.iter()
            .find_map(|layer| {
                layer.contracts.get(&code_hash).map(|bytecode| unsafe {
                    Bytecode::new_raw_with_hash(bytecode.clone(), code_hash)
                })
            })
            .ok_or_else(|| {
                anyhow!(
                    "Layered database does not contain contract with code hash: {}.",
                    code_hash,
                )
            })
    }

    fn storage(&mut self, address: Address, index: U256) -> anyhow::Result<U256> {
        self.iter()
            .find_map(|layer| {
                layer
                    .storage
                    .get(&address)
                    .and_then(|storage| storage.get(&index))
                    .cloned()
            })
            .ok_or_else(|| {
                anyhow!(
                    "Layered database does not contain storage with address: {}; and index: {}.",
                    address,
                    index
                )
            })
    }

    fn block_hash(&mut self, number: U256) -> anyhow::Result<H256> {
        self.iter()
            .find_map(|layer| layer.block_hashes.get(&number).cloned())
            .ok_or_else(|| {
                anyhow!(
                    "Layered database does not contain block hash with number: {}.",
                    number
                )
            })
    }
}

impl DatabaseCommit for LayeredDatabase<RethnetLayer> {
    fn commit(&mut self, changes: HashMap<Address, Account>) {
        let last_layer = self.last_layer_mut();

        changes.into_iter().for_each(|(address, account)| {
            if account.is_empty() || account.is_destroyed {
                last_layer.account_infos.remove(&address);
            } else {
                last_layer.insert_account(address, account.info);

                let storage = last_layer
                    .storage
                    .entry(address)
                    .and_modify(|storage| {
                        if account.storage_cleared {
                            storage.clear();
                        }
                    })
                    .or_default();

                account.storage.into_iter().for_each(|(index, value)| {
                    let value = value.present_value();
                    if value == U256::ZERO {
                        storage.remove(&index);
                    } else {
                        storage.insert(index, value);
                    }
                });

                if storage.is_empty() {
                    last_layer.storage.remove(&address);
                }
            }
        });
    }
}

impl DatabaseDebug for LayeredDatabase<RethnetLayer> {
    type Error = anyhow::Error;

    fn insert_account(
        &mut self,
        address: Address,
        account_info: AccountInfo,
    ) -> Result<(), Self::Error> {
        self.last_layer_mut()
            .account_infos
            .insert(address, account_info);

        Ok(())
    }

    fn insert_block(&mut self, block_number: U256, block_hash: H256) -> Result<(), Self::Error> {
        self.last_layer_mut()
            .block_hashes
            .insert(block_number, block_hash);

        Ok(())
    }

    fn modify_account(
        &mut self,
        address: Address,
        modifier: Box<dyn Fn(&mut AccountInfo) + Send>,
    ) -> Result<(), Self::Error> {
        // TODO: Move account insertion out of LayeredDatabase when forking
        let account_info = self.account_or_insert_mut(&address);
        modifier(account_info);

        Ok(())
    }

    fn remove_account(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        // We cannot actually remove an account in a layered database, so instead set the empty account
        let empty_account = AccountInfo::default();

        if let Some(account_info) = self.last_layer_mut().account_infos.get_mut(&address) {
            let old_account_info = account_info.clone();

            *account_info = empty_account;

            Ok(Some(old_account_info))
        } else {
            self.last_layer_mut().insert_account(address, empty_account);
            Ok(None)
        }
    }

    fn storage_root(&mut self) -> Result<H256, Self::Error> {
        todo!()
    }

    fn checkpoint(&mut self) -> Result<(), Self::Error> {
        self.add_layer_default();
        Ok(())
    }

    fn revert(&mut self) -> Result<(), Self::Error> {
        let last_layer_id = self.last_layer_id();
        if last_layer_id > 0 {
            self.revert_to_layer(last_layer_id - 1);
            Ok(())
        } else {
            Err(anyhow!("No checkpoints to revert."))
        }
    }
}