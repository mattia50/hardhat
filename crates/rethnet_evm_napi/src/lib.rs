use std::{convert::TryFrom, str::FromStr};

use anyhow::anyhow;
use napi::{
    bindgen_prelude::*,
    tokio::{
        self,
        sync::{
            mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
            oneshot,
        },
    },
};
use napi_derive::napi;
use rethnet_evm::{
    AccountInfo, Bytecode, Bytes, CreateScheme, Database, DatabaseDebug, LayeredDatabase,
    RethnetLayer, State, TransactTo, TxEnv, EVM, H160, H256, U256,
};

#[napi(constructor)]
pub struct Account {
    /// Account balance
    #[napi(readonly)]
    pub balance: BigInt,
    /// Account nonce
    #[napi(readonly)]
    pub nonce: BigInt,
    /// 256-bit code hash
    #[napi(readonly)]
    pub code_hash: Buffer,
}

fn try_u256_from_bigint(mut value: BigInt) -> napi::Result<U256> {
    let num_words = value.words.len();
    match num_words.cmp(&4) {
        std::cmp::Ordering::Less => value.words.append(&mut vec![0u64; 4 - num_words]),
        std::cmp::Ordering::Equal => (),
        std::cmp::Ordering::Greater => {
            return Err(napi::Error::new(
                Status::InvalidArg,
                "BigInt cannot have more than 4 words.".to_owned(),
            ));
        }
    }

    Ok(U256(value.words.try_into().unwrap()))
}

#[napi(object)]
pub struct AccessListItem {
    pub address: String,
    pub storage_keys: Vec<String>,
}

impl TryFrom<AccessListItem> for (H160, Vec<U256>) {
    type Error = napi::Error;

    fn try_from(value: AccessListItem) -> std::result::Result<Self, Self::Error> {
        let address = H160::from_str(&value.address)
            .map_err(|e| napi::Error::new(Status::InvalidArg, e.to_string()))?;

        let storage_keys = value
            .storage_keys
            .into_iter()
            .map(|key| {
                U256::from_str(&key)
                    .map_err(|e| napi::Error::new(Status::InvalidArg, e.to_string()))
            })
            .collect::<std::result::Result<Vec<U256>, _>>()?;

        Ok((address, storage_keys))
    }
}

#[napi(object)]
pub struct Transaction {
    /// 160-bit address for caller
    /// Defaults to `0x00.0` address.
    pub from: Option<Buffer>,
    /// 160-bit address for receiver
    /// Creates a contract if no address is provided.
    pub to: Option<Buffer>,
    /// Maximum gas allowance for the code execution to avoid infinite loops.
    /// Defaults to 2^63.
    pub gas_limit: Option<BigInt>,
    /// Number of wei to pay for each unit of gas during execution.
    /// Defaults to 1 wei.
    pub gas_price: Option<BigInt>,
    /// Maximum tip per gas that's given directly to the forger.
    pub gas_priority_fee: Option<BigInt>,
    /// (Up to) 256-bit unsigned value.
    pub value: Option<BigInt>,
    /// Nonce of sender account.
    pub nonce: Option<BigInt>,
    /// Input byte data
    pub input: Option<Buffer>,
    /// A list of addresses and storage keys that the transaction plans to access.
    pub access_list: Option<Vec<AccessListItem>>,
    /// Transaction is only valid on networks with this chain ID.
    pub chain_id: Option<BigInt>,
}

impl TryFrom<Transaction> for TxEnv {
    type Error = napi::Error;

    fn try_from(value: Transaction) -> std::result::Result<Self, Self::Error> {
        let caller = if let Some(from) = value.from.as_ref() {
            H160::from_slice(from)
        } else {
            H160::default()
        };

        let transact_to = if let Some(to) = value.to.as_ref() {
            TransactTo::Call(H160::from_slice(to))
        } else {
            TransactTo::Create(CreateScheme::Create)
        };

        let data = value
            .input
            .map_or(Bytes::default(), |input| Bytes::copy_from_slice(&input));

        let access_list = value.access_list.map_or(Ok(Vec::new()), |access_list| {
            access_list
                .into_iter()
                .map(|item| item.try_into())
                .collect::<std::result::Result<Vec<(H160, Vec<U256>)>, _>>()
        })?;

        Ok(Self {
            caller,
            gas_limit: value
                .gas_limit
                .map_or(2u64.pow(63), |limit| limit.get_u64().1),
            gas_price: value
                .gas_price
                .map_or(Ok(U256::from(1)), try_u256_from_bigint)?,
            gas_priority_fee: value
                .gas_priority_fee
                .map_or(Ok(None), |price| try_u256_from_bigint(price).map(Some))?,
            transact_to,
            value: value
                .value
                .map_or(Ok(U256::default()), try_u256_from_bigint)?,
            data,
            chain_id: value.chain_id.map(|chain_id| chain_id.get_u64().1),
            nonce: value.nonce.map(|nonce| nonce.get_u64().1),
            access_list,
        })
    }
}

#[napi(object)]
pub struct TransactionOutput {
    /// Return value from Call or Create transactions
    #[napi(readonly)]
    pub output: Option<Buffer>,
    /// Optionally, a 160-bit address from Create transactions
    #[napi(readonly)]
    pub address: Option<Buffer>,
}

impl From<rethnet_evm::TransactOut> for TransactionOutput {
    fn from(value: rethnet_evm::TransactOut) -> Self {
        let (output, address) = match value {
            rethnet_evm::TransactOut::None => (None, None),
            rethnet_evm::TransactOut::Call(output) => (Some(Buffer::from(output.as_ref())), None),
            rethnet_evm::TransactOut::Create(output, address) => (
                Some(Buffer::from(output.as_ref())),
                address.map(|address| Buffer::from(address.as_bytes())),
            ),
        };

        Self { output, address }
    }
}

#[napi(object)]
pub struct ExecutionResult {
    pub exit_code: u8,
    pub output: TransactionOutput,
    pub gas_used: BigInt,
    pub gas_refunded: BigInt,
    pub logs: Vec<serde_json::Value>,
}

impl TryFrom<rethnet_evm::ExecutionResult> for ExecutionResult {
    type Error = napi::Error;

    fn try_from(value: rethnet_evm::ExecutionResult) -> std::result::Result<Self, Self::Error> {
        let logs = value
            .logs
            .into_iter()
            .map(serde_json::to_value)
            .collect::<serde_json::Result<Vec<serde_json::Value>>>()?;

        Ok(Self {
            exit_code: value.exit_reason as u8,
            output: value.out.into(),
            gas_used: BigInt::from(value.gas_used),
            gas_refunded: BigInt::from(value.gas_refunded),
            logs,
        })
    }
}

#[napi(object)]
pub struct TransactionResult {
    pub exec_result: ExecutionResult,
    pub state: serde_json::Value,
}

impl TryFrom<(rethnet_evm::ExecutionResult, rethnet_evm::State)> for TransactionResult {
    type Error = napi::Error;

    fn try_from(
        value: (rethnet_evm::ExecutionResult, rethnet_evm::State),
    ) -> std::result::Result<Self, Self::Error> {
        let exec_result = value.0.try_into()?;
        let state = serde_json::to_value(value.1)?;

        Ok(Self { exec_result, state })
    }
}

#[napi]
pub struct RethnetClient {
    request_sender: UnboundedSender<Request>,
}

#[napi]
impl RethnetClient {
    #[allow(clippy::new_without_default)]
    #[napi(constructor)]
    pub fn new() -> Self {
        let (request_sender, request_receiver) = unbounded_channel();

        tokio::spawn(Rethnet::run(request_receiver));

        Self { request_sender }
    }

    #[napi]
    pub async fn dry_run(&self, transaction: Transaction) -> Result<TransactionResult> {
        let transaction = transaction.try_into()?;

        let (sender, receiver) = oneshot::channel();

        self.request_sender
            .send(Request::DryRun {
                transaction,
                sender,
            })
            .map_err(|_| anyhow!("Failed to send request"))?;

        receiver
            .await
            .expect("Rethnet unexpectedly crashed")
            .try_into()
    }

    #[napi]
    pub async fn run(&self, transaction: Transaction) -> Result<ExecutionResult> {
        let transaction = transaction.try_into()?;

        let (sender, receiver) = oneshot::channel();

        self.request_sender
            .send(Request::Run {
                transaction,
                sender,
            })
            .map_err(|_| anyhow!("Failed to send request"))?;

        receiver
            .await
            .expect("Rethnet unexpectedly crashed")
            .try_into()
    }

    #[napi]
    pub async fn insert_account(&self, address: Buffer) -> Result<()> {
        let address = H160::from_slice(&address);

        let (sender, receiver) = oneshot::channel();

        self.request_sender
            .send(Request::InsertAccount { address, sender })
            .map_err(|_| anyhow!("Failed to send request"))?;

        receiver.await.expect("Rethnet unexpectedly crashed");
        Ok(())
    }

    #[napi]
    pub async fn get_account_by_address(&self, address: Buffer) -> Result<Account> {
        let address = H160::from_slice(&address);

        let (sender, receiver) = oneshot::channel();

        self.request_sender
            .send(Request::AccountByAddress { address, sender })
            .map_err(|_| anyhow!("Failed to send request"))?;

        receiver
            .await
            .expect("Rethnet unexpectedly crashed")?
            .map_or_else(
                || {
                    Err(napi::Error::new(
                        Status::GenericFailure,
                        format!(
                            "Database does not contain account with address: {}.",
                            address,
                        ),
                    ))
                },
                |account_info| {
                    Ok(Account {
                        balance: BigInt {
                            sign_bit: false,
                            words: account_info.balance.0.to_vec(),
                        },
                        nonce: BigInt::from(account_info.nonce),
                        code_hash: Buffer::from(account_info.code_hash.as_bytes()),
                    })
                },
            )
    }

    #[napi]
    pub async fn insert_block(&self, block_number: BigInt, block_hash: Buffer) -> Result<()> {
        let block_number = try_u256_from_bigint(block_number)?;
        let block_hash = H256::from_slice(&block_hash);

        let (sender, receiver) = oneshot::channel();

        self.request_sender
            .send(Request::InsertBlock {
                block_number,
                block_hash,
                sender,
            })
            .map_err(|_| anyhow!("Failed to send request"))?;

        receiver.await.expect("Rethnet unexpectedly crashed");
        Ok(())
    }

    #[napi]
    pub async fn set_account_balance(&self, address: Buffer, balance: BigInt) -> Result<()> {
        let address = H160::from_slice(&address);
        let balance = try_u256_from_bigint(balance)?;

        let (sender, receiver) = oneshot::channel();

        self.request_sender
            .send(Request::SetAccountBalance {
                address,
                balance,
                sender,
            })
            .map_err(|_| anyhow!("Failed to send request"))?;

        receiver.await.expect("Rethnet unexpectedly crashed");
        Ok(())
    }

    #[napi]
    pub async fn set_account_code(&self, address: Buffer, code: Buffer) -> Result<()> {
        let address = H160::from_slice(&address);

        let (sender, receiver) = oneshot::channel();

        self.request_sender
            .send(Request::SetAccountCode {
                address,
                bytes: Bytes::copy_from_slice(&code),
                sender,
            })
            .map_err(|_| anyhow!("Failed to send request"))?;

        receiver.await.expect("Rethnet unexpectedly crashed");
        Ok(())
    }

    #[napi]
    pub async fn set_account_nonce(&self, address: Buffer, nonce: BigInt) -> Result<()> {
        let address = H160::from_slice(&address);
        let nonce = nonce.get_u64().1;

        let (sender, receiver) = oneshot::channel();

        self.request_sender
            .send(Request::SetAccountNonce {
                address,
                nonce,
                sender,
            })
            .map_err(|_| anyhow!("Failed to send request"))?;

        receiver.await.expect("Rethnet unexpectedly crashed");
        Ok(())
    }

    #[napi]
    pub async fn set_account_storage_slot(
        &self,
        address: Buffer,
        index: BigInt,
        value: BigInt,
    ) -> Result<()> {
        let address = H160::from_slice(&address);
        let index = try_u256_from_bigint(index)?;
        let value = try_u256_from_bigint(value)?;

        let (sender, receiver) = oneshot::channel();

        self.request_sender
            .send(Request::SetAccountStorageSlot {
                address,
                index,
                value,
                sender,
            })
            .map_err(|_| anyhow!("Failed to send request"))?;

        receiver.await.expect("Rethnet unexpectedly crashed");
        Ok(())
    }
}

enum Request {
    AccountByAddress {
        address: H160,
        sender: oneshot::Sender<anyhow::Result<Option<AccountInfo>>>,
    },
    DryRun {
        transaction: TxEnv,
        sender: oneshot::Sender<(rethnet_evm::ExecutionResult, State)>,
    },
    Run {
        transaction: TxEnv,
        sender: oneshot::Sender<rethnet_evm::ExecutionResult>,
    },
    InsertAccount {
        address: H160,
        sender: oneshot::Sender<()>,
    },
    InsertBlock {
        block_number: U256,
        block_hash: H256,
        sender: oneshot::Sender<()>,
    },
    SetAccountBalance {
        address: H160,
        balance: U256,
        sender: oneshot::Sender<()>,
    },
    SetAccountCode {
        address: H160,
        bytes: Bytes,
        sender: oneshot::Sender<()>,
    },
    SetAccountNonce {
        address: H160,
        nonce: u64,
        sender: oneshot::Sender<()>,
    },
    SetAccountStorageSlot {
        address: H160,
        index: U256,
        value: U256,
        sender: oneshot::Sender<()>,
    },
}

struct Rethnet {
    evm: EVM<LayeredDatabase<RethnetLayer>>,
    request_receiver: UnboundedReceiver<Request>,
}

impl Rethnet {
    pub fn new(request_receiver: UnboundedReceiver<Request>) -> Self {
        let mut evm = EVM::new();
        evm.database(LayeredDatabase::default());

        Self {
            evm,
            request_receiver,
        }
    }

    pub async fn run(request_receiver: UnboundedReceiver<Request>) -> anyhow::Result<()> {
        let mut rethnet = Rethnet::new(request_receiver);

        rethnet.event_loop().await
    }

    async fn event_loop(&mut self) -> anyhow::Result<()> {
        while let Some(request) = self.request_receiver.recv().await {
            let sent_response = match request {
                Request::AccountByAddress { address, sender } => {
                    sender.send(self.evm.db().unwrap().basic(address)).is_ok()
                }
                Request::DryRun {
                    transaction,
                    sender,
                } => {
                    self.evm.env.tx = transaction;
                    sender.send(self.evm.transact()).is_ok()
                }
                Request::Run {
                    transaction,
                    sender,
                } => {
                    self.evm.env.tx = transaction;
                    sender.send(self.evm.transact_commit()).is_ok()
                }
                Request::InsertAccount { address, sender } => {
                    self.evm
                        .db()
                        .unwrap()
                        .insert_account(&address, AccountInfo::default());
                    sender.send(()).is_ok()
                }
                Request::InsertBlock {
                    block_number,
                    block_hash,
                    sender,
                } => {
                    self.evm
                        .db()
                        .unwrap()
                        .insert_block(block_number, block_hash);
                    sender.send(()).is_ok()
                }
                Request::SetAccountBalance {
                    address,
                    balance,
                    sender,
                } => {
                    self.evm.db().unwrap().account_info_mut(&address).balance = balance;
                    sender.send(()).is_ok()
                }
                Request::SetAccountCode {
                    address,
                    bytes,
                    sender,
                } => {
                    self.evm.db().unwrap().account_info_mut(&address).code =
                        Some(Bytecode::new_raw(bytes));
                    sender.send(()).is_ok()
                }
                Request::SetAccountNonce {
                    address,
                    nonce,
                    sender,
                } => {
                    self.evm.db().unwrap().account_info_mut(&address).nonce = nonce;
                    sender.send(()).is_ok()
                }
                Request::SetAccountStorageSlot {
                    address,
                    index,
                    value,
                    sender,
                } => {
                    self.evm
                        .db()
                        .unwrap()
                        .set_storage_slot_at_layer(address, index, value);

                    sender.send(()).is_ok()
                }
            };

            if !sent_response {
                return Err(anyhow!("Failed to send response"));
            }
        }
        Ok(())
    }
}