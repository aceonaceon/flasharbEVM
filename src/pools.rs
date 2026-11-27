//! Pool classification. A pool is used only if the factory it claims to come
//! from returns the same address when asked for that token pair. Anyone can
//! deploy a contract that emits fake Swap events; this check filters them out.
//!
//! Every new pool is probed once with a fixed set of view calls. The probe
//! results decide the pool type, its fee, and which factory getter confirms it.
//! Factories not listed in the config are accepted too when the probe makes the
//! type unambiguous (`[discovery] auto_accept_unknown_factories`), and are
//! labelled `other_<type>_<factory prefix>`.

use crate::config::{Config, FactoryKind};
use crate::detector::PoolInfo;
use crate::ledger::PoolRow;
use crate::rpc::Rpc;
use alloy::primitives::{aliases::I24, aliases::U24, Address, Bytes};
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::Result;

/// Bump when classification or detection rules change; the ledger is then rebuilt
/// from scratch (the old one is kept as ledger.v<N>.sqlite) so numbers never mix.
/// v3: CL pools record tick spacing and trades are capped at the current bucket.
pub const CLASSIFIER_VERSION: u32 = 3;

sol! {
    function token0() external view returns (address);
    function token1() external view returns (address);
    function factory() external view returns (address);
    function fee() external view returns (uint24);
    function tickSpacing() external view returns (int24);
    function stable() external view returns (bool);
    function stableSwap() external view returns (bool); // Camelot v2
    function globalState() external view returns (uint160 price, int24 tick, uint16 fee0, uint16 fee1);
    function getReserves() external view returns (uint112 r0, uint112 r1, uint32 ts);
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

/// The `i`-th 32-byte word as u64, if it fits.
fn word_u64(b: &[u8], i: usize) -> Option<u64> {
    let w = b.get(i * 32..(i + 1) * 32)?;
    w[..24].iter().all(|x| *x == 0).then(|| u64::from_be_bytes(w[24..32].try_into().unwrap()))
}

fn bool_word(b: &Option<Bytes>) -> Option<bool> {
    let b = b.as_ref()?;
    if b.len() != 32 {
        return None;
    }
    word_u64(b, 0).and_then(|v| match v {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    })
}

/// Algebra pools on Arbitrum ship several globalState layouts. The fee is the
/// third word in all of them; anything above 10% is treated as a layout mismatch.
fn algebra_fee(b: &Option<Bytes>) -> Option<u32> {
    let v = word_u64(b.as_ref()?, 2)?;
    (v > 0 && v <= 100_000).then_some(v as u32)
}

/// Results of the one-time probe of a pool.
#[derive(Debug, Clone, Default)]
pub struct Probe {
    pub fee: Option<Bytes>,
    pub tick_spacing: Option<Bytes>,
    pub global_state: Option<Bytes>,
    pub reserves: Option<Bytes>,
    pub stable_swap: Option<Bytes>,
    pub stable: Option<Bytes>,
}

impl Probe {
    fn fee_u24(&self) -> Option<u32> {
        let v = word_u64(self.fee.as_ref()?, 0)?;
        (v <= 1_000_000).then_some(v as u32)
    }
    fn tick_spacing(&self) -> Option<I24> {
        self.tick_spacing.as_ref().and_then(|b| tickSpacingCall::abi_decode_returns(b).ok())
    }
    fn is_stable_curve(&self) -> bool {
        bool_word(&self.stable_swap) == Some(true) || bool_word(&self.stable) == Some(true)
    }
    /// Camelot v2 returns (r0, r1, fee0Percent, fee1Percent) with a 100,000 denominator.
    fn camelot_fee_ppm(&self) -> Option<u32> {
        let b = self.reserves.as_ref()?;
        if b.len() != 4 * 32 {
            return None;
        }
        let f = word_u64(b, 2)?.max(word_u64(b, 3)?);
        (f <= 100_000).then_some(f as u32 * 10)
    }
}

#[derive(Debug, PartialEq)]
pub enum Decision {
    Accept { kind: FactoryKind, venue: String, fee_ppm: u32, verify: Bytes, note: &'static str },
    Reject(&'static str),
}

/// Pure classification from probe results; `configured` is the factory's entry in config.toml, if any.
pub fn classify(
    pool: Address,
    t0: Address,
    t1: Address,
    factory: Address,
    configured: Option<(FactoryKind, &str, Option<u32>)>,
    p: &Probe,
) -> Decision {
    let (kind, venue, fee_bps) = match configured {
        Some((k, name, bps)) => (k, name.to_string(), bps),
        None => {
            let k = if p.fee_u24().is_some() && p.tick_spacing().is_some() {
                FactoryKind::Univ3
            } else if p.tick_spacing().is_some() && algebra_fee(&p.global_state).is_some() {
                FactoryKind::Algebra
            } else if p.reserves.is_some() && p.fee.is_none() && p.tick_spacing.is_none() {
                FactoryKind::Univ2
            } else {
                return Decision::Reject("unknown factory: unsupported pool type");
            };
            let tag = match k {
                FactoryKind::Univ3 => "v3",
                FactoryKind::Algebra => "algebra",
                _ => "v2",
            };
            let hex = format!("{factory:x}");
            let hex = hex.trim_start_matches("0x");
            (k, format!("other_{tag}_{}", &hex[..4]), None)
        }
    };
    let accept = |fee_ppm: u32, verify: Bytes, note: &'static str| Decision::Accept {
        kind,
        venue: venue.clone(),
        fee_ppm,
        verify,
        note,
    };
    match kind {
        FactoryKind::Univ3 => match p.fee_u24() {
            Some(fee) => accept(fee, enc(getPoolCall { a: t0, b: t1, fee: U24::from(fee) }), ""),
            None => Decision::Reject("fee() failed"),
        },
        FactoryKind::AeroCl => match (p.tick_spacing(), p.fee_u24()) {
            (Some(ts), Some(fee)) => accept(
                fee,
                enc(aero::getPoolCall { a: t0, b: t1, tickSpacing: ts }),
                "fee = hook pre-swap fee; post-swap fee not modelled",
            ),
            _ => Decision::Reject("tickSpacing()/fee() failed"),
        },
        FactoryKind::Algebra => match algebra_fee(&p.global_state) {
            Some(fee) => accept(fee, enc(poolByPairCall { a: t0, b: t1 }), "dynamic fee sampled once at discovery"),
            None => Decision::Reject("globalState() fee unreadable"),
        },
        FactoryKind::Univ2 => {
            if p.is_stable_curve() {
                return Decision::Reject("stable-curve pool: not modelled");
            }
            let (fee, note) = match p.camelot_fee_ppm() {
                Some(f) => (f, "fee from getReserves (Camelot)"),
                None => (fee_bps.unwrap_or(30) * 100, "fee from config (default 0.3%)"),
            };
            accept(fee, enc(getPairCall { a: t0, b: t1 }), note)
        }
        FactoryKind::AeroV2 => {
            if p.is_stable_curve() {
                return Decision::Reject("stable-curve pool: not modelled");
            }
            accept(fee_bps.unwrap_or(30) * 100, enc(isPoolCall { pool }), "fee from config")
        }
    }
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
    let mut cands = Vec::new();
    for (i, p) in pools.iter().enumerate() {
        let (t0, t1, f) = (dec_addr(&r1[3 * i]), dec_addr(&r1[3 * i + 1]), dec_addr(&r1[3 * i + 2]));
        match (t0, t1, f) {
            (Some(t0), Some(t1), Some(f))
                if f != Address::ZERO && (cfg.factory(f).is_some() || cfg.discovery.auto_accept_unknown_factories) =>
            {
                cands.push(Stage { pool: *p, t0, t1, factory: f })
            }
            (_, _, f) => rows.push(unverified(*p, f.unwrap_or(Address::ZERO), "unknown factory or not a pool")),
        }
    }

    // Stage 2: the probe.
    const N: usize = 6;
    let mut calls = Vec::new();
    for s in &cands {
        calls.push((s.pool, enc(feeCall {})));
        calls.push((s.pool, enc(tickSpacingCall {})));
        calls.push((s.pool, enc(globalStateCall {})));
        calls.push((s.pool, enc(getReservesCall {})));
        calls.push((s.pool, enc(stableSwapCall {})));
        calls.push((s.pool, enc(stableCall {})));
    }
    let r2 = rpc.multicall(calls).await?;

    // Stage 3: ask the factory to confirm the pool.
    let mut calls = Vec::new();
    let mut pending = Vec::new();
    for (i, s) in cands.iter().enumerate() {
        let probe = Probe {
            fee: r2[N * i].clone(),
            tick_spacing: r2[N * i + 1].clone(),
            global_state: r2[N * i + 2].clone(),
            reserves: r2[N * i + 3].clone(),
            stable_swap: r2[N * i + 4].clone(),
            stable: r2[N * i + 5].clone(),
        };
        let configured = cfg.factory(s.factory).map(|f| (f.kind, f.name.as_str(), f.fee_bps));
        match classify(s.pool, s.t0, s.t1, s.factory, configured, &probe) {
            Decision::Accept { kind, venue, fee_ppm, verify, note } => {
                calls.push((s.factory, verify));
                let ts = probe.tick_spacing().map(|t| t.as_i32()).unwrap_or(0);
                pending.push((i, kind, venue, fee_ppm, note, ts));
            }
            Decision::Reject(note) => rows.push(unverified(s.pool, s.factory, note)),
        }
    }
    let r3 = rpc.multicall(calls).await?;

    for ((i, kind, venue, fee, note, ts), res) in pending.into_iter().zip(r3) {
        let s = &cands[i];
        let ok = match kind {
            FactoryKind::AeroV2 => bool_word(&res) == Some(true),
            _ => dec_addr(&res) == Some(s.pool),
        };
        let info = PoolInfo { address: s.pool, venue, token0: s.t0, token1: s.t1, fee_ppm: fee, is_cl: kind.is_cl(), tick_spacing: if kind.is_cl() { ts } else { 0 } };
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
            is_cl: false, tick_spacing: 0
        },
        factory,
        verified: false,
        note: note.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(v: &[u64]) -> Option<Bytes> {
        let mut b = Vec::new();
        for x in v {
            let mut w = [0u8; 32];
            w[24..].copy_from_slice(&x.to_be_bytes());
            b.extend_from_slice(&w);
        }
        Some(Bytes::from(b))
    }

    fn a(n: u8) -> Address {
        Address::repeat_byte(n)
    }

    #[test]
    fn selectors_differ_between_univ3_and_aero_get_pool() {
        assert_ne!(getPoolCall::SELECTOR, aero::getPoolCall::SELECTOR);
        assert_eq!(getPoolCall::SELECTOR, [0x16, 0x98, 0xee, 0x82]);
        assert_eq!(stableSwapCall::SELECTOR, [0x9e, 0x54, 0x8b, 0x7f]);
        assert_eq!(getReservesCall::SELECTOR, [0x09, 0x02, 0xf1, 0xac]);
    }

    #[test]
    fn algebra_fee_parsing() {
        assert_eq!(algebra_fee(&words(&[7, 1, 500, 0])), Some(500));
        assert_eq!(algebra_fee(&words(&[7, 1, 0, 0])), None);
        assert_eq!(algebra_fee(&None), None);
    }

    #[test]
    fn camelot_stable_pool_is_rejected() {
        // The USDC/USDC.e pool seen on mainnet: stableSwap() = 1, getReserves has 4 words.
        let p = Probe { reserves: words(&[3200, 3359, 1, 1]), stable_swap: words(&[1]), ..Default::default() };
        let d = classify(a(1), a(2), a(3), a(4), Some((FactoryKind::Univ2, "camelot_v2", Some(30))), &p);
        assert_eq!(d, Decision::Reject("stable-curve pool: not modelled"));
    }

    #[test]
    fn camelot_volatile_fee_comes_from_reserves() {
        let p = Probe { reserves: words(&[1000, 2000, 300, 250]), stable_swap: words(&[0]), ..Default::default() };
        match classify(a(1), a(2), a(3), a(4), Some((FactoryKind::Univ2, "camelot_v2", Some(30))), &p) {
            Decision::Accept { fee_ppm, venue, .. } => {
                assert_eq!(fee_ppm, 3000); // max(0.3%, 0.25%)
                assert_eq!(venue, "camelot_v2");
            }
            d => panic!("{d:?}"),
        }
    }

    #[test]
    fn plain_v2_uses_config_fee() {
        let p = Probe { reserves: words(&[1000, 2000, 5]), ..Default::default() };
        match classify(a(1), a(2), a(3), a(4), Some((FactoryKind::Univ2, "uniswap_v2", Some(30))), &p) {
            Decision::Accept { fee_ppm, .. } => assert_eq!(fee_ppm, 3000),
            d => panic!("{d:?}"),
        }
    }

    #[test]
    fn unknown_factories_are_inferred() {
        let f: Address = "0x9B43e504d84aDcB7321DC2551a86bff1084AE2E5".parse().unwrap();
        // v2 fork: only getReserves works.
        let v2 = Probe { reserves: words(&[1, 2, 3]), ..Default::default() };
        match classify(a(1), a(2), a(3), f, None, &v2) {
            Decision::Accept { kind, venue, fee_ppm, .. } => {
                assert_eq!(kind, FactoryKind::Univ2);
                assert_eq!(venue, "other_v2_9b43");
                assert_eq!(fee_ppm, 3000);
            }
            d => panic!("{d:?}"),
        }
        // v3 fork: fee + tickSpacing.
        let v3 = Probe { fee: words(&[50]), tick_spacing: words(&[1]), ..Default::default() };
        assert!(matches!(classify(a(1), a(2), a(3), f, None, &v3), Decision::Accept { kind: FactoryKind::Univ3, fee_ppm: 50, .. }));
        // Algebra fork: tickSpacing + globalState, no fee().
        let alg = Probe { tick_spacing: words(&[60]), global_state: words(&[9, 1, 500, 0]), ..Default::default() };
        assert!(matches!(classify(a(1), a(2), a(3), f, None, &alg), Decision::Accept { kind: FactoryKind::Algebra, fee_ppm: 500, .. }));
        // Only tickSpacing: fee unknown -> reject.
        let odd = Probe { tick_spacing: words(&[1]), ..Default::default() };
        assert!(matches!(classify(a(1), a(2), a(3), f, None, &odd), Decision::Reject(_)));
        // Unknown v2 stable pool -> reject.
        let st = Probe { reserves: words(&[1, 2, 3]), stable: words(&[1]), ..Default::default() };
        assert!(matches!(classify(a(1), a(2), a(3), f, None, &st), Decision::Reject(_)));
    }
}
