use alloy::core::{primitives::keccak256, sol_types::SolValue};
use alloy::sol;
use bitcoin::secp256k1::{
    ecdsa::{RecoverableSignature, RecoveryId},
    Message, PublicKey, Secp256k1,
};
use std::u64;

use ed::{Decode, Encode};
use orga::{
    coins::{Address, Coin, Give, Take},
    collections::{ChildMut, Deque, Ref},
    describe::Describe,
    encoding::LengthVec,
    migrate::Migrate,
    orga,
    query::FieldQuery,
    state::State,
    store::Store,
    Error,
};
use serde::{Deserialize, Serialize};

use crate::{
    app::Dest,
    bitcoin::{
        exempt_from_fee,
        signatory::SignatorySet,
        threshold_sig::{Pubkey, Signature, ThresholdSig},
        Nbtc,
    },
    error::Result,
};

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    bridge_contract,
    "src/ethereum/Nomic.json",
);
use bridge_contract::{LogicCallArgs, ValsetArgs};

// TODO: message ttl/pruning
// TODO: multi-token support
// TODO: network muxing
// TODO: remote contract muxing
// TODO: call fees
// TODO: bounceback on failed transfers
// TODO: fallback to address on failed contract calls

pub mod signer;

pub const VALSET_INTERVAL: u64 = 60 * 60 * 24;

pub const WHITELISTED_RELAYER_ADDR: &str = "nomic124j0ky0luh9jzqh9w2dk77cze9v0ckdupk50ny";

#[orga]
pub struct Ethereum {
    pub id: [u8; 32],
    pub bridge_contract: Address,
    pub token_contract: Address,
    pub valset_interval: u64,

    pub message_index: u64,
    pub batch_index: u64,
    pub valset_index: u64,
    pub return_index: u64,

    pub outbox: Deque<OutMessage>,
    pub pending: Deque<(Dest, Coin<Nbtc>)>,
    pub coins: Coin<Nbtc>,
    pub valset: SignatorySet,
}

#[orga]
impl Ethereum {
    pub fn new(
        id: &[u8],
        bridge_contract: Address,
        token_contract: Address,
        mut valset: SignatorySet,
    ) -> Self {
        valset.normalize_vp(u32::MAX as u64);
        Self {
            id: bytes32(id).unwrap(),
            bridge_contract,
            token_contract,
            outbox: Deque::new(),
            message_index: 1,
            batch_index: 0,
            valset_index: 0,
            return_index: 0,
            coins: Coin::default(),
            valset_interval: VALSET_INTERVAL,
            valset,
            pending: Deque::new(),
        }
    }

    // TODO: method to return pending nbtc transfers

    pub fn step(&mut self, active_sigset: &SignatorySet) -> Result<()> {
        if active_sigset.create_time - self.valset.create_time >= self.valset_interval
            && self.valset.index != active_sigset.index
        {
            self.update_valset(active_sigset.clone())?;
        }

        Ok(())
    }

    pub fn transfer(&mut self, dest: Address, coins: Coin<Nbtc>) -> Result<()> {
        // TODO: validation (min amount, etc)

        // TODO: batch transfers
        let transfer = Transfer {
            dest,
            amount: coins.amount.into(),
            fee_amount: 0, // TODO: deduct fee
        };
        let transfers = vec![transfer].try_into().unwrap();
        let timeout = u64::MAX; // TODO: set based on current ethereum height, or let user specify

        self.coins.give(coins)?;
        self.batch_index += 1;
        self.push_outbox(OutMessageArgs::Batch {
            transfers,
            timeout,
            batch_index: self.batch_index,
        })?;

        Ok(())
    }

    pub fn call(&mut self, call: ContractCall, coins: Coin<Nbtc>) -> Result<()> {
        self.coins.give(coins)?;
        self.push_outbox(OutMessageArgs::LogicCall(self.message_index + 1, call))
    }

    fn update_valset(&mut self, mut new_valset: SignatorySet) -> Result<()> {
        new_valset.normalize_vp(u32::MAX as u64);
        self.valset_index += 1;
        self.push_outbox(OutMessageArgs::UpdateValset(
            self.valset_index,
            new_valset.clone(),
        ))?;
        self.valset = new_valset;

        Ok(())
    }

    fn push_outbox(&mut self, msg: OutMessageArgs) -> Result<()> {
        let hash = self.message_hash(&msg);
        let mut sigs = ThresholdSig::from_sigset(&self.valset)?;
        sigs.threshold = u32::MAX as u64 * 2 / 3;
        sigs.set_message(hash);
        let sigset_index = self.valset.index;

        if !self.outbox.is_empty() {
            self.message_index += 1;
        }
        self.outbox.push_back(OutMessage {
            sigs,
            msg,
            sigset_index,
        })?;

        Ok(())
    }

    pub fn take_pending(&mut self) -> Result<Vec<(Dest, Coin<Nbtc>)>> {
        let mut pending = Vec::new();
        while let Some(entry) = self.pending.pop_front()? {
            pending.push(entry.into_inner());
        }
        Ok(pending)
    }

    #[call]
    pub fn sign(&mut self, msg_index: u64, pubkey: Pubkey, sig: Signature) -> Result<()> {
        exempt_from_fee()?;

        let mut msg = self.get_mut(msg_index)?;
        msg.sigs.sign(pubkey, sig)?;
        Ok(())
    }

    #[call]
    pub fn relay_return(
        &mut self,
        consensus_proof: (),
        account_proof: (),
        // TODO: storage_proofs: LengthVec<u16, (LengthVec<u8, u8>, u64)>,
        returns: LengthVec<u16, (u64, Dest, u64)>, // TODO: don't include data, just state proof
    ) -> Result<()> {
        exempt_from_fee()?;

        #[cfg(not(test))]
        {
            // TODO: remove whitelisted relaying once we have proper proof verification
            let signer = orga::context::Context::resolve::<orga::plugins::Signer>()
                .ok_or_else(|| Error::Signer("No Signer context available".into()))?
                .signer
                .ok_or_else(|| Error::Coins("Call must be signed".into()))?;
            if signer.to_string().as_str() != WHITELISTED_RELAYER_ADDR {
                return Err(orga::Error::App(
                    "Only whitelisted relayers can relay returns".to_string(),
                )
                .into());
            }
        }

        if returns.len() == 0 {
            return Err(orga::Error::App("Returns must not be empty".to_string()).into());
        }

        // TODO: validate consensus proof
        // TODO: validate state proofs (account proof, storage proofs)

        // TODO: validate return entry indexes
        for (_, dest, amount) in returns.iter().cloned() {
            let coins = self.coins.take(amount)?;
            self.pending.push_back((dest, coins))?;
            self.return_index += 1;
        }

        // TODO: push return queue clear message

        Ok(())
    }

    #[query]
    pub fn get(&self, msg_index: u64) -> Result<Ref<OutMessage>> {
        let index = self.abs_index(msg_index)?;
        Ok(self.outbox.get(index)?.unwrap())
    }

    pub fn get_mut(&mut self, msg_index: u64) -> Result<ChildMut<u64, OutMessage>> {
        let index = self.abs_index(msg_index)?;
        Ok(self.outbox.get_mut(index)?.unwrap())
    }

    fn abs_index(&self, msg_index: u64) -> Result<u64> {
        let start_index = self.message_index + 1 - self.outbox.len();
        if self.outbox.is_empty() || msg_index > self.message_index || msg_index < start_index {
            return Err(Error::App("message index out of bounds".to_string()).into());
        }

        Ok(msg_index - start_index)
    }

    fn message_hash(&self, msg: &OutMessageArgs) -> [u8; 32] {
        sighash(match msg {
            OutMessageArgs::Batch {
                transfers,
                timeout,
                batch_index,
            } => batch_hash(
                self.id,
                *batch_index,
                transfers,
                self.token_contract,
                timeout,
            ),
            OutMessageArgs::LogicCall(index, call) => {
                call.hash(self.id, self.token_contract, *index)
            }
            OutMessageArgs::UpdateValset(index, valset) => checkpoint_hash(self.id, valset, *index),
        })
    }

    // TODO: remove, this is a hack due to enum state issues in client
    #[query]
    pub fn needs_sig(&self, msg_index: u64, pubkey: Pubkey) -> Result<bool> {
        Ok(self.get(msg_index)?.sigs.needs_sig(pubkey)?)
    }
    // TODO: remove, this is a hack due to enum state issues in client
    #[query]
    pub fn get_sigs(&self, msg_index: u64) -> Result<Vec<(Pubkey, Signature)>> {
        Ok(self.get(msg_index)?.sigs.sigs()?)
    }
}

#[orga]
pub struct OutMessage {
    pub sigset_index: u32,
    pub sigs: ThresholdSig,
    pub msg: OutMessageArgs,
}

#[derive(Encode, Decode, Debug, Clone, Serialize)]
pub enum OutMessageArgs {
    Batch {
        transfers: LengthVec<u16, Transfer>,
        timeout: u64,
        batch_index: u64,
    },
    LogicCall(u64, ContractCall),
    UpdateValset(u64, SignatorySet),
}

impl Describe for OutMessageArgs {
    fn describe() -> orga::describe::Descriptor {
        <()>::describe()
    }
}

impl State for OutMessageArgs {
    fn load(_store: Store, bytes: &mut &[u8]) -> orga::Result<Self> {
        Ok(Self::decode(bytes)?)
    }

    fn attach(&mut self, _store: Store) -> orga::Result<()> {
        Ok(())
    }

    fn flush<W: std::io::Write>(self, out: &mut W) -> orga::Result<()> {
        Ok(self.encode_into(out)?)
    }

    fn field_keyop(_field_name: &str) -> Option<orga::describe::KeyOp> {
        todo!()
    }
}

impl FieldQuery for OutMessageArgs {
    type FieldQuery = ();

    fn field_query(&self, _query: Self::FieldQuery) -> orga::Result<()> {
        Ok(())
    }
}

impl Migrate for OutMessageArgs {}

// TODO: we shouldn't require all orga types to have Default
impl Default for OutMessageArgs {
    fn default() -> Self {
        OutMessageArgs::Batch {
            transfers: LengthVec::default(),
            timeout: u64::MAX,
            batch_index: 0,
        }
    }
}

#[derive(Debug, Clone, Encode, Decode, Default, Serialize, Deserialize)]
pub struct ContractCall {
    pub contract: Address,
    pub transfer_amount: u64,
    pub fee_amount: u64,
    pub payload: LengthVec<u16, u8>,
    pub timeout: u64,
}

impl ContractCall {
    pub fn hash(&self, id: [u8; 32], token_contract: Address, nonce_id: u64) -> [u8; 32] {
        let bytes = (
            id,
            bytes32(b"logicCall").unwrap(),
            vec![self.transfer_amount],
            vec![addr_to_bytes32(token_contract)],
            vec![self.fee_amount],
            vec![addr_to_bytes32(token_contract)],
            addr_to_bytes32(self.contract),
            self.payload.as_slice(),
            self.timeout,
            uint256(nonce_id),
            uint256(1),
        )
            .abi_encode_params();

        keccak256(bytes).0
    }

    pub fn to_abi(&self, token_contract: Address, nonce_id: u64) -> LogicCallArgs {
        LogicCallArgs {
            transferAmounts: vec![alloy::core::primitives::U256::from(self.transfer_amount)],
            transferTokenContracts: vec![alloy::core::primitives::Address::from_slice(
                &token_contract.bytes(),
            )],
            feeAmounts: vec![alloy::core::primitives::U256::from(self.fee_amount)],
            feeTokenContracts: vec![alloy::core::primitives::Address::from_slice(
                &token_contract.bytes(),
            )],
            logicContractAddress: alloy::core::primitives::Address::from_slice(
                &self.contract.bytes(),
            ),
            payload: alloy::core::primitives::Bytes::from(self.payload.to_vec()),
            timeOut: alloy::core::primitives::U256::from(self.timeout),
            invalidationId: alloy::core::primitives::FixedBytes::from(uint256(nonce_id)), /* TODO: set in msg */
            invalidationNonce: alloy::core::primitives::U256::from(1),
        }
    }
}

#[orga]
#[derive(Debug, Clone)]
pub struct Transfer {
    pub dest: Address,
    pub amount: u64,
    pub fee_amount: u64,
}

pub fn checkpoint_hash(id: [u8; 32], valset: &SignatorySet, valset_index: u64) -> [u8; 32] {
    let bytes = (
        id,
        bytes32(b"checkpoint").unwrap(),
        uint256(valset_index),
        valset
            .eth_addresses()
            .iter()
            .cloned()
            .map(addr_to_bytes32)
            .collect::<Vec<_>>(),
        valset
            .signatories
            .iter()
            .map(|s| s.voting_power)
            .collect::<Vec<_>>(),
        [0u8; 20],
        [0u8; 32],
    )
        .abi_encode_params();
    keccak256(bytes).0
}

pub fn batch_hash(
    id: [u8; 32],
    batch_index: u64,
    transfers: &LengthVec<u16, Transfer>,
    token_contract: Address,
    timeout: &u64,
) -> [u8; 32] {
    let dests = transfers
        .iter()
        .map(|t| addr_to_bytes32(t.dest))
        .collect::<Vec<_>>();
    let amounts = transfers.iter().map(|t| t.amount).collect::<Vec<_>>();
    let fees = transfers.iter().map(|t| t.fee_amount).collect::<Vec<_>>();

    let bytes = (
        id,
        bytes32(b"transactionBatch").unwrap(),
        amounts,
        dests,
        fees,
        batch_index,
        addr_to_bytes32(token_contract),
        timeout,
    )
        .abi_encode_params();

    keccak256(bytes).0
}

pub fn sighash(message: [u8; 32]) -> [u8; 32] {
    let mut bytes = b"\x19Ethereum Signed Message:\n32".to_vec();
    bytes.extend_from_slice(&message);

    keccak256(bytes).0
}

pub fn to_eth_sig(
    sig: &bitcoin::secp256k1::ecdsa::Signature,
    pubkey: &PublicKey,
    msg: &Message,
) -> (u8, [u8; 32], [u8; 32]) {
    let secp = Secp256k1::new();

    let rs = sig.serialize_compact();

    let mut recid = None;
    for i in 0..=1 {
        let sig =
            RecoverableSignature::from_compact(&rs, RecoveryId::from_i32(i).unwrap()).unwrap();
        let pk = secp.recover_ecdsa(msg, &sig).unwrap();
        if pk == *pubkey {
            recid = Some(i);
            break;
        }
    }
    let v = recid.unwrap() as u8 + 27;

    let mut r = [0; 32];
    r.copy_from_slice(&rs[0..32]);

    let mut s = [0; 32];
    s.copy_from_slice(&rs[32..]);

    (v, r, s)
}

pub fn bytes32(bytes: &[u8]) -> Result<[u8; 32]> {
    if bytes.len() > 32 {
        return Err(Error::App("bytes too long".to_string()).into());
    }

    let mut padded = [0; 32];
    padded[..bytes.len()].copy_from_slice(bytes);
    Ok(padded)
}

pub fn uint256(n: u64) -> [u8; 32] {
    let mut bytes = [0; 32];
    bytes[24..].copy_from_slice(&n.to_be_bytes());
    bytes
}

pub fn addr_to_bytes32(addr: Address) -> [u8; 32] {
    let mut bytes = [0; 32];
    bytes[12..].copy_from_slice(&addr.bytes());
    bytes
}

impl SignatorySet {
    pub fn eth_addresses(&self) -> Vec<Address> {
        self.signatories
            .iter()
            .map(|s| {
                let pk = PublicKey::from_slice(s.pubkey.as_slice()).unwrap();
                let mut uncompressed = [0; 64];
                uncompressed.copy_from_slice(&pk.serialize_uncompressed()[1..]);
                Address::from_pubkey_eth(uncompressed)
            })
            .collect()
    }

    pub fn normalize_vp(&mut self, total: u64) {
        let adjust = |n: u64| (n as u128 * total as u128 / self.present_vp as u128) as u64;

        for s in self.signatories.iter_mut() {
            s.voting_power = adjust(s.voting_power);
        }
        self.possible_vp = adjust(self.possible_vp);
        self.present_vp = total;
    }

    pub fn to_abi(&self, nonce: u64) -> ValsetArgs {
        ValsetArgs {
            valsetNonce: alloy::core::primitives::U256::from(nonce),
            validators: self
                .eth_addresses()
                .iter()
                .map(|a| alloy::core::primitives::Address::from_slice(&a.bytes()))
                .collect(),
            powers: self
                .signatories
                .iter()
                .map(|s| alloy::core::primitives::U256::from(s.voting_power))
                .collect(),
            rewardToken: alloy::core::primitives::Address::default(),
            rewardAmount: alloy::core::primitives::U256::default(),
        }
    }
}

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    token_contract,
    "src/ethereum/CosmosERC20.json",
);

#[cfg(test)]
mod tests {
    use alloy::sol_types::SolEvent;
    use alloy::{node_bindings::Anvil, providers::ProviderBuilder};
    use bitcoin::{
        secp256k1::{Message, Secp256k1, SecretKey},
        util::bip32::{ExtendedPrivKey, ExtendedPubKey},
    };
    use orga::{coins::Symbol, context::Context, plugins::Paid};

    use crate::bitcoin::{
        signatory::{derive_pubkey, Signatory},
        threshold_sig::Pubkey,
    };

    use super::*;

    #[test]
    fn checkpoint_fixture() {
        let secp = Secp256k1::new();

        let privkey = SecretKey::from_slice(&bytes32(b"test").unwrap()).unwrap();
        let pubkey = privkey.public_key(&secp);

        let valset = SignatorySet {
            index: 0,
            signatories: vec![Signatory {
                pubkey: pubkey.into(),
                voting_power: 10_000_000_000,
            }],
            create_time: 0,
            present_vp: 10_000_000_000,
            possible_vp: 10_000_000_000,
        };

        let id = bytes32(b"test").unwrap();

        assert_eq!(
            hex::encode(checkpoint_hash(id, &valset, 0)),
            "61fe378d7a8aac20d5882ff4696d9c14c0db93b583fcd25f0616ce5187efae69",
        );

        let valset2 = SignatorySet {
            index: 0,
            signatories: vec![Signatory {
                pubkey: pubkey.into(),
                voting_power: 10_000_000_001,
            }],
            create_time: 0,
            present_vp: 10_000_000_001,
            possible_vp: 10_000_000_001,
        };

        let updated_checkpoint = checkpoint_hash(id, &valset2, 1);
        assert_eq!(
            hex::encode(updated_checkpoint),
            "0b73bc9926c210f36673973a0ecb0a5f337ca1c7f99ba44ecf3624c891a8ab2b",
        );

        let valset_update_sighash = sighash(updated_checkpoint);
        let msg = Message::from_slice(&valset_update_sighash).unwrap();
        let sig = secp.sign_ecdsa(&msg, &privkey);
        let vrs = to_eth_sig(&sig, &pubkey, &msg);

        assert_eq!(vrs.0, 27);
        assert_eq!(
            hex::encode(vrs.1),
            "060215a246c6439b1ba1cf29577936ef20912e9e97b44326fd063b22221f69d8",
        );
        assert_eq!(
            hex::encode(vrs.2),
            "24d9924b969a742b877831a43b14e0ea88886308ecf0e37ee70a096346966a43",
        );
    }

    #[test]
    fn indices() {
        let secp = Secp256k1::new();

        let privkey = SecretKey::from_slice(&bytes32(b"test").unwrap()).unwrap();
        let pubkey = privkey.public_key(&secp);

        let valset = SignatorySet {
            index: 10,
            signatories: vec![Signatory {
                pubkey: pubkey.into(),
                voting_power: 10_000_000_000,
            }],
            create_time: 0,
            present_vp: 10_000_000_000,
            possible_vp: 10_000_000_000,
        };

        let id = bytes32(b"test").unwrap();
        let mut ethereum = Ethereum::new(b"test", Address::NULL, Address::NULL, valset);
        assert_eq!(ethereum.batch_index, 0);
        assert_eq!(ethereum.valset_index, 0);
        assert_eq!(ethereum.message_index, 1);
        assert_eq!(ethereum.outbox.len(), 0);

        let valset2 = SignatorySet {
            index: 11,
            signatories: vec![Signatory {
                pubkey: pubkey.into(),
                voting_power: 10_000_000_001,
            }],
            create_time: 1_000_000_000,
            present_vp: 10_000_000_001,
            possible_vp: 10_000_000_001,
        };
        ethereum.step(&valset2).unwrap();
        assert_eq!(ethereum.batch_index, 0);
        assert_eq!(ethereum.valset_index, 1);
        assert_eq!(ethereum.message_index, 1);
        assert_eq!(ethereum.outbox.len(), 1);

        let valset2 = SignatorySet {
            index: 12,
            signatories: vec![Signatory {
                pubkey: pubkey.into(),
                voting_power: 10_000_000_002,
            }],
            create_time: 2_000_000_000,
            present_vp: 10_000_000_002,
            possible_vp: 10_000_000_002,
        };
        ethereum.step(&valset2).unwrap();
        assert_eq!(ethereum.batch_index, 0);
        assert_eq!(ethereum.valset_index, 2);
        assert_eq!(ethereum.message_index, 2);
        assert_eq!(ethereum.outbox.len(), 2);
    }

    #[test]
    fn ss_normalize_vp() {
        let mut valset = SignatorySet {
            index: 0,
            signatories: vec![
                Signatory {
                    pubkey: Pubkey::new([2; 33]).unwrap(),
                    voting_power: 10,
                },
                Signatory {
                    pubkey: Pubkey::new([2; 33]).unwrap(),
                    voting_power: 20,
                },
                Signatory {
                    pubkey: Pubkey::new([2; 33]).unwrap(),
                    voting_power: 30,
                },
            ],
            create_time: 0,
            present_vp: 60,
            possible_vp: 60,
        };

        valset.normalize_vp(6);
        assert_eq!(valset.signatories[0].voting_power, 1);
        assert_eq!(valset.signatories[1].voting_power, 2);
        assert_eq!(valset.signatories[2].voting_power, 3);
        assert_eq!(valset.possible_vp, 6);
        assert_eq!(valset.present_vp, 6);

        valset.normalize_vp(u32::MAX as u64);
        assert_eq!(valset.signatories[0].voting_power, 715_827_882);
        assert_eq!(valset.signatories[1].voting_power, 1_431_655_765);
        assert_eq!(valset.signatories[2].voting_power, 2_147_483_647);
        assert_eq!(valset.possible_vp, u32::MAX as u64);
        assert_eq!(valset.present_vp, u32::MAX as u64);
    }

    #[ignore]
    #[tokio::test]
    #[serial_test::serial]
    async fn valset_update() {
        Context::add(Paid::default());

        let secp = Secp256k1::new();

        let xpriv = ExtendedPrivKey::new_master(bitcoin::Network::Regtest, &[0]).unwrap();
        let xpub = ExtendedPubKey::from_priv(&secp, &xpriv);

        let valset = SignatorySet {
            index: 0,
            signatories: vec![Signatory {
                pubkey: derive_pubkey(&secp, xpub.into(), 0).unwrap().into(),
                voting_power: 10_000_000_000,
            }],
            create_time: 0,
            present_vp: 10_000_000_000,
            possible_vp: 10_000_000_000,
        };

        let bridge_addr = {
            let decoded = hex::decode("5FbDB2315678afecb367f032d93F642f64180aa3").unwrap();
            let mut data = [0; 20];
            data.copy_from_slice(decoded.as_slice());
            Address::from(data)
        };
        // TODO: token contract
        let mut ethereum = Ethereum::new(b"test", bridge_addr, bridge_addr, valset);
        let valset = ethereum.valset.clone();

        let new_valset = SignatorySet {
            index: 1,
            signatories: vec![Signatory {
                pubkey: derive_pubkey(&secp, xpub.into(), 1).unwrap().into(),
                voting_power: 10_000_000_000,
            }],
            create_time: 1_000_000_000,
            present_vp: 10_000_000_000,
            possible_vp: 10_000_000_000,
        };
        ethereum.update_valset(new_valset).unwrap();
        let new_valset = ethereum.valset.clone();
        assert_eq!(ethereum.outbox.len(), 1);
        assert_eq!(ethereum.message_index, 1);

        let msg = ethereum.get(1).unwrap().sigs.message;
        let sig = crate::bitcoin::signer::sign(&Secp256k1::signing_only(), &xpriv, &[(msg, 0)])
            .unwrap()[0];
        let pubkey = derive_pubkey(&secp, xpub.into(), 0).unwrap();
        ethereum.sign(1, pubkey.into(), sig).unwrap();
        assert!(ethereum.get(1).unwrap().sigs.signed());

        let anvil = Anvil::new().try_spawn().unwrap();
        let rpc_url = anvil.endpoint().parse().unwrap();
        let provider = ProviderBuilder::new().on_http(rpc_url);

        let contract = bridge_contract::deploy(
            provider,
            ethereum.id.into(),
            valset
                .eth_addresses()
                .iter()
                .map(|a| alloy::core::primitives::Address::from_slice(&a.bytes()))
                .collect(),
            valset
                .signatories
                .iter()
                .map(|s| alloy::core::primitives::U256::from(s.voting_power))
                .collect(),
        )
        .await
        .unwrap();

        let sigs: Vec<_> = ethereum
            .get(1)
            .unwrap()
            .sigs
            .sigs()
            .unwrap()
            .into_iter()
            .map(|(pk, sig)| {
                let (v, r, s) = to_eth_sig(
                    &bitcoin::secp256k1::ecdsa::Signature::from_compact(&sig.0).unwrap(),
                    &bitcoin::secp256k1::PublicKey::from_slice(pk.as_slice()).unwrap(),
                    &Message::from_slice(&msg).unwrap(),
                );
                bridge_contract::Signature {
                    v,
                    r: r.into(),
                    s: s.into(),
                }
            })
            .collect();

        dbg!(contract
            .updateValset(new_valset.to_abi(1), valset.to_abi(0), sigs.clone())
            .into_transaction_request());
        dbg!(contract
            .updateValset(new_valset.to_abi(1), valset.to_abi(0), sigs)
            .send()
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap());

        Context::remove::<Paid>();
    }

    #[ignore]
    #[tokio::test]
    #[serial_test::serial]
    async fn transfer() {
        Context::add(Paid::default());

        let secp = Secp256k1::new();

        let xpriv = ExtendedPrivKey::new_master(bitcoin::Network::Regtest, &[0]).unwrap();
        let xpub = ExtendedPubKey::from_priv(&secp, &xpriv);

        let mut valset = SignatorySet {
            index: 0,
            signatories: vec![Signatory {
                pubkey: derive_pubkey(&secp, xpub.into(), 0).unwrap().into(),
                voting_power: 10_000_000_000,
            }],
            create_time: 0,
            present_vp: 10_000_000_000,
            possible_vp: 10_000_000_000,
        };
        valset.normalize_vp(u32::MAX as u64);

        let anvil = Anvil::new().try_spawn().unwrap();
        let provider = ProviderBuilder::new()
            .with_recommended_fillers()
            .on_anvil_with_wallet();

        let contract = bridge_contract::deploy(
            provider,
            bytes32(b"test").unwrap().into(),
            valset
                .eth_addresses()
                .iter()
                .map(|a| alloy::core::primitives::Address::from_slice(&a.bytes()))
                .collect(),
            valset
                .signatories
                .iter()
                .map(|s| alloy::core::primitives::U256::from(s.voting_power))
                .collect(),
        )
        .await
        .unwrap();

        let receipt = dbg!(contract
            .deployERC20(
                "usat".to_string(),
                "nBTC".to_string(),
                "nBTC".to_string(),
                14,
            )
            .send()
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap());
        let mut token_contract_addr = None;
        for log in receipt.inner.logs().into_iter() {
            let res = bridge_contract::ERC20DeployedEvent::decode_log_data(log.data(), true);
            if let Ok(e) = res {
                token_contract_addr = Some(e._tokenContract);
                println!("{}", e._tokenContract);
            }
        }

        let mut ethereum = Ethereum::new(
            b"test",
            contract.address().0 .0.into(),
            token_contract_addr.unwrap().0 .0.into(),
            valset,
        );
        println!(
            "{} {}",
            hex::encode(ethereum.valset.eth_addresses()[0].bytes()),
            ethereum.valset.signatories[0].voting_power
        );

        ethereum
            .transfer(anvil.addresses()[0].0 .0.into(), Nbtc::mint(1_000_000))
            .unwrap();
        assert_eq!(ethereum.outbox.len(), 1);
        assert_eq!(ethereum.batch_index, 1);
        assert_eq!(ethereum.message_index, 1);

        let msg = ethereum.get(1).unwrap().sigs.message;
        let data = ethereum.get(1).unwrap().msg.clone();
        let sig = crate::bitcoin::signer::sign(&Secp256k1::signing_only(), &xpriv, &[(msg, 0)])
            .unwrap()[0];
        let pubkey = derive_pubkey(&secp, xpub.into(), 0).unwrap();
        ethereum.sign(1, pubkey.into(), sig).unwrap();
        assert!(ethereum.get(1).unwrap().sigs.signed());

        let sigs: Vec<_> = ethereum
            .get(1)
            .unwrap()
            .sigs
            .sigs()
            .unwrap()
            .into_iter()
            .map(|(pk, sig)| {
                let (v, r, s) = to_eth_sig(
                    &bitcoin::secp256k1::ecdsa::Signature::from_compact(&sig.0).unwrap(),
                    &bitcoin::secp256k1::PublicKey::from_slice(pk.as_slice()).unwrap(),
                    &Message::from_slice(&msg).unwrap(),
                );
                bridge_contract::Signature {
                    v,
                    r: r.into(),
                    s: s.into(),
                }
            })
            .collect();

        //submitBatch(currentValset, sigs, amounts, destinations, fees, batchNonce,
        // tokenContract, batchTimeout)
        if let OutMessageArgs::Batch {
            transfers,
            timeout,
            batch_index,
        } = data
        {
            dbg!(contract
                .submitBatch(
                    ethereum.valset.to_abi(ethereum.valset_index),
                    sigs.clone(),
                    transfers
                        .iter()
                        .map(|t| alloy::core::primitives::U256::from(t.amount))
                        .collect(),
                    transfers
                        .iter()
                        .map(|t| alloy::core::primitives::Address::from_slice(&t.dest.bytes()))
                        .collect(),
                    transfers
                        .iter()
                        .map(|t| alloy::core::primitives::U256::from(t.fee_amount))
                        .collect(),
                    alloy::core::primitives::U256::from(batch_index),
                    alloy::core::primitives::Address::from_slice(&ethereum.token_contract.bytes()),
                    alloy::core::primitives::U256::from(timeout),
                )
                .into_transaction_request());
            dbg!(contract
                .submitBatch(
                    ethereum.valset.to_abi(ethereum.valset_index),
                    sigs,
                    transfers
                        .iter()
                        .map(|t| alloy::core::primitives::U256::from(t.amount))
                        .collect(),
                    transfers
                        .iter()
                        .map(|t| alloy::core::primitives::Address::from_slice(&t.dest.bytes()))
                        .collect(),
                    transfers
                        .iter()
                        .map(|t| alloy::core::primitives::U256::from(t.fee_amount))
                        .collect(),
                    alloy::core::primitives::U256::from(batch_index),
                    alloy::core::primitives::Address::from_slice(&ethereum.token_contract.bytes()),
                    alloy::core::primitives::U256::from(timeout),
                )
                .send()
                .await
                .unwrap()
                .get_receipt()
                .await
                .unwrap());
        } else {
            unreachable!();
        };

        Context::remove::<Paid>();
    }

    #[ignore]
    #[tokio::test]
    #[serial_test::serial]
    #[should_panic]
    async fn contract_call() {
        Context::add(Paid::default());

        let secp = Secp256k1::new();

        let xpriv = ExtendedPrivKey::new_master(bitcoin::Network::Regtest, &[0]).unwrap();
        let xpub = ExtendedPubKey::from_priv(&secp, &xpriv);

        let mut valset = SignatorySet {
            index: 0,
            signatories: vec![Signatory {
                pubkey: derive_pubkey(&secp, xpub.into(), 0).unwrap().into(),
                voting_power: 10_000_000_000,
            }],
            create_time: 0,
            present_vp: 10_000_000_000,
            possible_vp: 10_000_000_000,
        };
        valset.normalize_vp(u32::MAX as u64);

        let anvil = Anvil::new().try_spawn().unwrap();
        let provider = ProviderBuilder::new()
            .with_recommended_fillers()
            .on_anvil_with_wallet();

        let contract = bridge_contract::deploy(
            provider,
            bytes32(b"test").unwrap().into(),
            valset
                .eth_addresses()
                .iter()
                .map(|a| alloy::core::primitives::Address::from_slice(&a.bytes()))
                .collect(),
            valset
                .signatories
                .iter()
                .map(|s| alloy::core::primitives::U256::from(s.voting_power))
                .collect(),
        )
        .await
        .unwrap();

        let receipt = dbg!(contract
            .deployERC20(
                "usat".to_string(),
                "nBTC".to_string(),
                "nBTC".to_string(),
                14,
            )
            .send()
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap());
        let mut token_contract_addr = None;
        for log in receipt.inner.logs().into_iter() {
            let res = bridge_contract::ERC20DeployedEvent::decode_log_data(log.data(), true);
            if let Ok(e) = res {
                token_contract_addr = Some(e._tokenContract);
                println!("{}", e._tokenContract);
            }
        }
        let token_contract_addr = token_contract_addr.unwrap().0 .0.into();

        let mut ethereum = Ethereum::new(
            b"test",
            contract.address().0 .0.into(),
            token_contract_addr,
            valset,
        );

        let call = ContractCall {
            contract: token_contract_addr,
            fee_amount: 0,
            timeout: u64::MAX,
            transfer_amount: 1_000_000,
            payload: bytes32(hex::decode("73b20547").unwrap().as_slice())
                .unwrap()
                .to_vec()
                .try_into()
                .unwrap(),
        };
        ethereum.call(call, Nbtc::mint(1_000_000)).unwrap();
        assert_eq!(ethereum.outbox.len(), 1);
        assert_eq!(ethereum.message_index, 1);

        let msg = ethereum.get(1).unwrap().sigs.message;
        let data = ethereum.get(1).unwrap().msg.clone();
        let sig = crate::bitcoin::signer::sign(&Secp256k1::signing_only(), &xpriv, &[(msg, 0)])
            .unwrap()[0];
        let pubkey = derive_pubkey(&secp, xpub.into(), 0).unwrap();
        ethereum.sign(1, pubkey.into(), sig).unwrap();
        assert!(ethereum.get(1).unwrap().sigs.signed());

        let sigs: Vec<_> = ethereum
            .get(1)
            .unwrap()
            .sigs
            .sigs()
            .unwrap()
            .into_iter()
            .map(|(pk, sig)| {
                let (v, r, s) = to_eth_sig(
                    &bitcoin::secp256k1::ecdsa::Signature::from_compact(&sig.0).unwrap(),
                    &bitcoin::secp256k1::PublicKey::from_slice(pk.as_slice()).unwrap(),
                    &Message::from_slice(&msg).unwrap(),
                );
                bridge_contract::Signature {
                    v,
                    r: r.into(),
                    s: s.into(),
                }
            })
            .collect();

        if let OutMessageArgs::LogicCall(index, call) = data {
            dbg!(contract
                .submitLogicCall(
                    ethereum.valset.to_abi(ethereum.valset_index),
                    sigs,
                    call.to_abi(ethereum.token_contract, index),
                )
                .send()
                .await
                .unwrap()
                .get_receipt()
                .await
                .unwrap());
        } else {
            unreachable!();
        };

        Context::remove::<Paid>();
    }

    #[ignore]
    #[tokio::test]
    #[serial_test::serial]
    async fn return_queue() {
        Context::add(Paid::default());

        let secp = Secp256k1::new();

        let xpriv = ExtendedPrivKey::new_master(bitcoin::Network::Regtest, &[0]).unwrap();
        let xpub = ExtendedPubKey::from_priv(&secp, &xpriv);

        let mut valset = SignatorySet {
            index: 0,
            signatories: vec![Signatory {
                pubkey: derive_pubkey(&secp, xpub.into(), 0).unwrap().into(),
                voting_power: 10_000_000_000,
            }],
            create_time: 0,
            present_vp: 10_000_000_000,
            possible_vp: 10_000_000_000,
        };
        valset.normalize_vp(u32::MAX as u64);

        let anvil = Anvil::new().try_spawn().unwrap();
        let provider = ProviderBuilder::new()
            .with_recommended_fillers()
            .on_anvil_with_wallet();

        let contract = bridge_contract::deploy(
            &provider,
            bytes32(b"test").unwrap().into(),
            valset
                .eth_addresses()
                .iter()
                .map(|a| alloy::core::primitives::Address::from_slice(&a.bytes()))
                .collect(),
            valset
                .signatories
                .iter()
                .map(|s| alloy::core::primitives::U256::from(s.voting_power))
                .collect(),
        )
        .await
        .unwrap();

        let receipt = dbg!(contract
            .deployERC20(
                "usat".to_string(),
                "nBTC".to_string(),
                "nBTC".to_string(),
                14,
            )
            .send()
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap());
        let mut token_contract_addr = None;
        for log in receipt.inner.logs().into_iter() {
            let res = bridge_contract::ERC20DeployedEvent::decode_log_data(log.data(), true);
            if let Ok(e) = res {
                token_contract_addr = Some(e._tokenContract);
                println!("{}", e._tokenContract);
            }
        }
        let token_contract_addr = token_contract_addr.unwrap().0 .0.into();

        let mut ethereum = Ethereum::new(
            b"test",
            contract.address().0 .0.into(),
            token_contract_addr,
            valset,
        );

        ethereum
            .transfer(anvil.addresses()[0].0 .0.into(), Nbtc::mint(1_000_000))
            .unwrap();
        assert_eq!(ethereum.outbox.len(), 1);
        assert_eq!(ethereum.batch_index, 1);
        assert_eq!(ethereum.message_index, 1);
        assert_eq!(ethereum.coins.amount, 1_000_000);

        let msg = ethereum.get(1).unwrap().sigs.message;
        let data = ethereum.get(1).unwrap().msg.clone();
        let sig = crate::bitcoin::signer::sign(&Secp256k1::signing_only(), &xpriv, &[(msg, 0)])
            .unwrap()[0];
        let pubkey = derive_pubkey(&secp, xpub.into(), 0).unwrap();
        ethereum.sign(1, pubkey.into(), sig).unwrap();
        assert!(ethereum.get(1).unwrap().sigs.signed());

        let sigs: Vec<_> = ethereum
            .get(1)
            .unwrap()
            .sigs
            .sigs()
            .unwrap()
            .into_iter()
            .map(|(pk, sig)| {
                let (v, r, s) = to_eth_sig(
                    &bitcoin::secp256k1::ecdsa::Signature::from_compact(&sig.0).unwrap(),
                    &bitcoin::secp256k1::PublicKey::from_slice(pk.as_slice()).unwrap(),
                    &Message::from_slice(&msg).unwrap(),
                );
                bridge_contract::Signature {
                    v,
                    r: r.into(),
                    s: s.into(),
                }
            })
            .collect();

        if let OutMessageArgs::Batch {
            transfers,
            timeout,
            batch_index,
        } = data
        {
            dbg!(contract
                .submitBatch(
                    ethereum.valset.to_abi(ethereum.valset_index),
                    sigs,
                    transfers
                        .iter()
                        .map(|t| alloy::core::primitives::U256::from(t.amount))
                        .collect(),
                    transfers
                        .iter()
                        .map(|t| alloy::core::primitives::Address::from_slice(&t.dest.bytes()))
                        .collect(),
                    transfers
                        .iter()
                        .map(|t| alloy::core::primitives::U256::from(t.fee_amount))
                        .collect(),
                    alloy::core::primitives::U256::from(batch_index),
                    alloy::core::primitives::Address::from_slice(&ethereum.token_contract.bytes()),
                    alloy::core::primitives::U256::from(timeout),
                )
                .send()
                .await
                .unwrap()
                .get_receipt()
                .await
                .unwrap());
        } else {
            unreachable!();
        };

        let token_contract_client = token_contract::new(
            alloy::core::primitives::Address::from_slice(&token_contract_addr.bytes()),
            &provider,
        );

        dbg!(token_contract_client
            .approve(
                alloy::core::primitives::Address::from_slice(&ethereum.bridge_contract.bytes()),
                alloy::core::primitives::U256::from(u64::MAX),
            )
            .send()
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap());

        dbg!(contract
            .sendToNomic(
                alloy::core::primitives::Address::from_slice(&ethereum.token_contract.bytes()),
                Address::from_pubkey([0; 33]).to_string(),
                alloy::core::primitives::U256::from(500_000),
            )
            .send()
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap());

        assert_eq!(ethereum.return_index, 0);
        // TODO
        ethereum
            .relay_return(
                (),
                (),
                vec![(
                    0,
                    Dest::NativeAccount {
                        address: Address::from_pubkey([0; 33]),
                    },
                    500_000,
                )]
                .try_into()
                .unwrap(),
            )
            .unwrap();
        assert_eq!(ethereum.return_index, 1);
        assert_eq!(ethereum.coins.amount, 500_000);

        Context::remove::<Paid>();
    }
}
