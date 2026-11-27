//! Log decoding. Only the fields the detector needs are decoded.

use alloy::primitives::{b256, Address, B256, U256};
use alloy::rpc::types::Log;

/// Swap(address,address,int256,int256,uint160,uint128,int24): Uniswap v3, Algebra (Camelot v3), Aero CL.
pub const SWAP_V3: B256 = b256!("c42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67");
/// Sync(uint112,uint112): Uniswap v2 forks.
pub const SYNC_V2: B256 = b256!("1c411e9a96e071241c2f21f7726b17ae89e3cab4c78be50e062b03a9fffbbad1");
/// Sync(uint256,uint256): Velodrome-style v2 pools (Aero v2).
pub const SYNC_U256: B256 = b256!("cf2aa50876cdfbb541206f89af0ee78d44a2abf8d328e37fa4917f982149848a");

/// Swap(address,address,int256,int256,uint160,uint128,int24,uint128,uint128): PancakeSwap v3
/// (two extra protocol-fee fields at the end; the fields we read sit at the same offsets).
pub const SWAP_PANCAKE_V3: B256 = b256!("19b47279256b2a23a1665c810c8d55a1758940ee09377d4f8d26497a3577dc83");

pub const TOPICS: [B256; 4] = [SWAP_V3, SWAP_PANCAKE_V3, SYNC_V2, SYNC_U256];

#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    /// Post-swap state of a concentrated-liquidity pool. `sender` is the address
    /// that called `swap` on the pool (a router or a bot contract).
    Cl { sqrt_price_x96: f64, liquidity: f64, sender: Address },
    /// New reserves of a v2 pool.
    V2 { r0: f64, r1: f64 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct PoolEvent {
    pub block: u64,
    pub tx_index: u64,
    pub log_index: u64,
    pub tx_hash: B256,
    pub pool: Address,
    pub update: Update,
}

pub fn u256_to_f64(v: U256) -> f64 {
    v.as_limbs()
        .iter()
        .enumerate()
        .map(|(i, limb)| *limb as f64 * 2f64.powi(64 * i as i32))
        .sum()
}

fn word(data: &[u8], i: usize) -> Option<U256> {
    data.get(i * 32..(i + 1) * 32).map(U256::from_be_slice)
}

/// Decode one RPC log. Returns None for logs we don't track or that are malformed.
pub fn decode(log: &Log) -> Option<PoolEvent> {
    let topics = log.topics();
    let t0 = *topics.first()?;
    let data = log.data().data.as_ref();
    let update = if t0 == SWAP_V3 || t0 == SWAP_PANCAKE_V3 {
        if topics.len() < 3 || data.len() < 5 * 32 {
            return None;
        }
        let sender = Address::from_word(topics[1]);
        Update::Cl {
            sqrt_price_x96: u256_to_f64(word(data, 2)?),
            liquidity: u256_to_f64(word(data, 3)?),
            sender,
        }
    } else if t0 == SYNC_V2 || t0 == SYNC_U256 {
        if data.len() < 2 * 32 {
            return None;
        }
        Update::V2 { r0: u256_to_f64(word(data, 0)?), r1: u256_to_f64(word(data, 1)?) }
    } else {
        return None;
    };
    Some(PoolEvent {
        block: log.block_number?,
        tx_index: log.transaction_index?,
        log_index: log.log_index?,
        tx_hash: log.transaction_hash?,
        pool: log.address(),
        update,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{keccak256, Bytes, LogData};

    #[test]
    fn topic_constants_match_signatures() {
        assert_eq!(keccak256("Swap(address,address,int256,int256,uint160,uint128,int24)"), SWAP_V3);
        assert_eq!(
            keccak256("Swap(address,address,int256,int256,uint160,uint128,int24,uint128,uint128)"),
            SWAP_PANCAKE_V3
        );
        assert_eq!(keccak256("Sync(uint112,uint112)"), SYNC_V2);
        assert_eq!(keccak256("Sync(uint256,uint256)"), SYNC_U256);
    }

    fn mk_log(topics: Vec<B256>, data: Vec<u8>) -> Log {
        let pool: Address = "0x00000000000000000000000000000000000000aa".parse().unwrap();
        Log {
            inner: alloy::primitives::Log {
                address: pool,
                data: LogData::new_unchecked(topics, Bytes::from(data)),
            },
            block_hash: None,
            block_number: Some(7),
            block_timestamp: None,
            transaction_hash: Some(B256::repeat_byte(1)),
            transaction_index: Some(3),
            log_index: Some(9),
            removed: false,
        }
    }

    fn be(v: U256) -> [u8; 32] {
        v.to_be_bytes()
    }

    #[test]
    fn decodes_v3_swap() {
        let sender: Address = "0x00000000000000000000000000000000000000bb".parse().unwrap();
        let mut data = Vec::new();
        // amount0 = -5 (two's complement), amount1 = 10
        data.extend_from_slice(&be(U256::MAX - U256::from(4)));
        data.extend_from_slice(&be(U256::from(10)));
        data.extend_from_slice(&be(U256::from(1u128 << 96)));
        data.extend_from_slice(&be(U256::from(123_456u64)));
        data.extend_from_slice(&be(U256::from(0)));
        let log = mk_log(vec![SWAP_V3, sender.into_word(), B256::ZERO], data);
        let ev = decode(&log).unwrap();
        assert_eq!(ev.block, 7);
        assert_eq!(ev.tx_index, 3);
        match ev.update {
            Update::Cl { sqrt_price_x96, liquidity, sender: s } => {
                assert_eq!(sqrt_price_x96, 2f64.powi(96));
                assert_eq!(liquidity, 123_456.0);
                assert_eq!(s, sender);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn decodes_sync_and_rejects_short_data() {
        let mut data = Vec::new();
        data.extend_from_slice(&be(U256::from(1000)));
        data.extend_from_slice(&be(U256::from(2000)));
        let ev = decode(&mk_log(vec![SYNC_V2], data)).unwrap();
        assert_eq!(ev.update, Update::V2 { r0: 1000.0, r1: 2000.0 });
        assert!(decode(&mk_log(vec![SYNC_V2], vec![0u8; 10])).is_none());
        assert!(decode(&mk_log(vec![B256::ZERO], vec![0u8; 64])).is_none());
    }

    #[test]
    fn u256_conversion_handles_high_limbs() {
        let v = U256::from(1u8) << 200;
        assert_eq!(u256_to_f64(v), 2f64.powi(200));
    }
}
