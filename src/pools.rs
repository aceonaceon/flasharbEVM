//! Pool classification. A pool is used only if the factory it claims to come
//! from returns the same address when asked for that token pair. Anyone can
//! deploy a contract that emits fake Swap events; this check filters them out.

use crate::config::{Config, FactoryKind};
use crate::detector::PoolInfo;
use crate::ledger::PoolRow;
use crate::rpc::Rpc;
use alloy::primitives::{aliases::U24, Address, Bytes};
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::Result;

sol! {
    function token0() external view returns (address);
    function token1() external view returns (address);
    function factory() external view returns (address);
    function fee() external view returns (uint24);
    function tickSpacing() external view returns (int24);
    function stable() external view returns (bool);
    function globalState() external view returns (uint160 price, int24 tick, uint16 fee0, uint16 fee1);
    // factory getters
    function getPool(address a, address b, uint24 fee) external view returns (address); // univ3
    function poolByPair(address a, address b) external view returns (address);           // algebra
    function getPair(address a, address b) external view returns (address);              // univ2
    function isPool(address pool) external view returns (bool);                          // aero v2
}

/// Aero CL's getPool takes an int24 tick spacing; same name, different selector.
mod aero {
    alloy::sol! {
        function getPool(address a, address b, int24 tickSpacing) external view returns (address);
    }
}

fn enc<C: SolCall>(c: C) -> Bytes {
    c.abi_encode().into()
}

fn dec_addr(b: &Option<Bytes>) -> Option<Address> {
    b.as_ref().and_then(|b| token0Call::abi_decode_returns(b).ok())
}

/// Algebra pools on Arbitrum ship several globalState layouts. The fee is the
/// third word in all of them; anything above 10% is treated as a layout mismatch.
fn algebra_fee(b: &Option<Bytes>) -> Option<u32> {
    let b = b.as_ref()?;
    let w = b.get(64..96)?;
    let v = u32::from_be_bytes(w[28..32].try_into().ok()?);
    (v > 0 && v <= 100_000 && w[..28].iter().all(|x| *x == 0)).then_some(v)
}

/// Classify unknown pools. Returns one row per input pool (verified or not).
pub async fn resolve(rpc: &Rpc, cfg: &Config, pools: &[Address]) -> Result<Vec<PoolRow>> {
    // Stage 1: token0, token1, factory.
    let mut calls = Vec::new();
    for p in pools {
        calls.push((*p, enc(token0Call {})));
        calls.push((*p, enc(token1Call {})));
        calls.push((*p, enc(factoryCall {})));
    }
    let r1 = rpc.multicall(calls).await?;

    struct Stage {
        pool: Address,
        t0: Address,
        t1: Address,
        factory: Address,
    }
    let mut rows = Vec::new();
    let mut known = Vec::new();
    for (i, p) in pools.iter().enumerate() {
        let (t0, t1, f) = (dec_addr(&r1[3 * i]), dec_addr(&r1[3 * i + 1]), dec_addr(&r1[3 * i + 2]));
        match (t0, t1, f) {
            (Some(t0), Some(t1), Some(f)) if cfg.factory(f).is_some() => known.push(Stage { pool: *p, t0, t1, factory: f }),
            (_, _, f) => rows.push(unverified(*p, f.unwrap_or(Address::ZERO), "unknown factory or not a pool")),
        }
    }

    // Stage 2: fee / tick spacing / stable flag, depending on the venue kind.
    let mut calls = Vec::new();
    for s in &known {
        let kind = cfg.factory(s.factory).unwrap().kind;
        let c = match kind {
            FactoryKind::Univ3 => enc(feeCall {}),
            FactoryKind::AeroCl => enc(tickSpacingCall {}),
            FactoryKind::Algebra => enc(globalStateCall {}),
            FactoryKind::Univ2 => enc(token0Call {}), // nothing extra needed
            FactoryKind::AeroV2 => enc(stableCall {}),
        };
        calls.push((s.pool, c));
        // Aero CL fee comes from the swap hook; fee() returns the pre-swap fee for an empty swap.
        calls.push((s.pool, enc(feeCall {})));
    }
    let r2 = rpc.multicall(calls).await?;

    // Stage 3: ask the factory to confirm the pool.
    let mut calls = Vec::new();
    let mut pending = Vec::new();
    for (i, s) in known.iter().enumerate() {
        let fcfg = cfg.factory(s.factory).unwrap();
        let extra = &r2[2 * i];
        let fee_raw = r2[2 * i + 1].as_ref().and_then(|b| feeCall::abi_decode_returns(b).ok()).map(|v| v.to::<u32>());
        let (verify, fee_ppm, note): (Option<Bytes>, Option<u32>, &str) = match fcfg.kind {
            FactoryKind::Univ3 => match fee_raw {
                Some(fee) => (
                    Some(enc(getPoolCall { a: s.t0, b: s.t1, fee: U24::from(fee) })),
                    Some(fee),
                    "",
                ),
                None => (None, None, "fee() failed"),
            },
            FactoryKind::AeroCl => {
                let ts = extra.as_ref().and_then(|b| tickSpacingCall::abi_decode_returns(b).ok());
                match ts {
                    Some(ts) => (
                        Some(enc(aero::getPoolCall { a: s.t0, b: s.t1, tickSpacing: ts })),
                        fee_raw,
                        "fee = hook pre-swap fee; post-swap fee not modelled",
                    ),
                    None => (None, None, "tickSpacing() failed"),
                }
            }
            FactoryKind::Algebra => (
                Some(enc(poolByPairCall { a: s.t0, b: s.t1 })),
                algebra_fee(extra),
                "dynamic fee sampled once at discovery",
            ),
            FactoryKind::Univ2 => (
                Some(enc(getPairCall { a: s.t0, b: s.t1 })),
                Some(fcfg.fee_bps.unwrap_or(30) * 100),
                "fee from config",
            ),
            FactoryKind::AeroV2 => {
                let stable = extra.as_ref().and_then(|b| stableCall::abi_decode_returns(b).ok());
                if stable == Some(true) {
                    (None, None, "stable-curve pool: not modelled")
                } else {
                    (Some(enc(isPoolCall { pool: s.pool })), Some(fcfg.fee_bps.unwrap_or(30) * 100), "fee from config")
                }
            }
        };
        match (verify, fee_ppm) {
            (Some(v), Some(fee)) => {
                calls.push((s.factory, v));
                pending.push((i, fee, note));
            }
            _ => rows.push(unverified(s.pool, s.factory, if note.is_empty() { "fee unknown" } else { note })),
        }
    }
    let r3 = rpc.multicall(calls).await?;

    for ((i, fee, note), res) in pending.into_iter().zip(r3) {
        let s = &known[i];
        let fcfg = cfg.factory(s.factory).unwrap();
        let ok = match fcfg.kind {
            FactoryKind::AeroV2 => res.as_ref().and_then(|b| isPoolCall::abi_decode_returns(b).ok()) == Some(true),
            _ => dec_addr(&res) == Some(s.pool),
        };
        let info = PoolInfo {
            address: s.pool,
            venue: fcfg.name.clone(),
            token0: s.t0,
            token1: s.t1,
            fee_ppm: fee,
            is_cl: fcfg.kind.is_cl(),
        };
        rows.push(PoolRow {
            info,
            factory: s.factory,
            verified: ok,
            note: if ok { note.to_string() } else { "factory does not confirm this pool".into() },
        });
    }
    Ok(rows)
}

fn unverified(pool: Address, factory: Address, note: &str) -> PoolRow {
    PoolRow {
        info: PoolInfo {
            address: pool,
            venue: String::new(),
            token0: Address::ZERO,
            token1: Address::ZERO,
            fee_ppm: 0,
            is_cl: false,
        },
        factory,
        verified: false,
        note: note.to_string(),
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors_differ_between_univ3_and_aero_get_pool() {
        assert_ne!(getPoolCall::SELECTOR, aero::getPoolCall::SELECTOR);
        // Well-known Uniswap v3 selector for getPool(address,address,uint24).
        assert_eq!(getPoolCall::SELECTOR, [0x16, 0x98, 0xee, 0x82]);
    }

    #[test]
    fn algebra_fee_parsing() {
        let mut b = vec![0u8; 96];
        b[95] = 0xf4; // 500
        b[94] = 0x01;
        assert_eq!(algebra_fee(&Some(Bytes::from(b.clone()))), Some(500));
        b[64] = 1; // garbage in the high bytes -> rejected
        assert_eq!(algebra_fee(&Some(Bytes::from(b))), None);
        assert_eq!(algebra_fee(&None), None);
    }
}
