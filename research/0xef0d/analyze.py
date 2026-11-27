import json, sys, os, collections, statistics as st

D = sys.argv[1]
C = "0xef0df9a49025512f0d5e3b45dcc27903af984834"
BEN = "0xd69719a8bda1cc82b2c2fb4ce84493689759cee4"  # hardcoded profit receiver
def L(n):
    p = os.path.join(D, n)
    return json.load(open(p)) if os.path.exists(p) else None
H = lambda x: int(x, 16) if isinstance(x, str) else (x or 0)
out = {}

# ---------- prices ----------
pr = L("prices.json") or {}
def feed(v):
    try: return int(v[2 + 64:2 + 128], 16) / 1e8
    except Exception: return None
ETH = feed(pr.get("ETH")) or 3000; BTC = feed(pr.get("BTC")) or 90000; ARB = feed(pr.get("ARB")) or 0.4
out["prices"] = {"ETH": ETH, "BTC": BTC, "ARB": ARB}
TOK = {
 "0x82af49447d8a07e3bd95bd0d56f35241523fbab1": ("WETH", 18, ETH),
 "0xaf88d065e77c8cc2239327c5edb3a432268e5831": ("USDC", 6, 1),
 "0xff970a61a04b1ca14834a43f5de4533ebddb5cc8": ("USDC.e", 6, 1),
 "0xfd086bc7cd5c481dcc9c85ebe478a1c0b69fcbb9": ("USDT", 6, 1),
 "0x2f2a2543b76a4166549f7aab2e75bef0aefc5b0f": ("WBTC", 8, BTC),
 "0x912ce59144191c1204e64559fe8253a0e49e6548": ("ARB", 18, ARB),
 "0xda10009cbd5d07dd0cecc66161fc93d7c9000da1": ("DAI", 18, 1),
}

# ---------- bytecode ----------
code = (L("code.json") or {}).get("code", "0x")
b = bytes.fromhex(code[2:])
sels, push20, i = [], set(), 0
while i < len(b):
    op = b[i]
    if 0x60 <= op <= 0x7f:
        n = op - 0x5f; arg = b[i+1:i+1+n]
        if n == 4: sels.append(arg.hex())
        if n == 20: push20.add("0x" + arg.hex())
        i += 1 + n
    else: i += 1
KNOWN = {"fa461e33": "uniswapV3SwapCallback(int256,int256,bytes)","23a69e75": "pancakeV3SwapCallback(int256,int256,bytes)","2c8958f6": "algebraSwapCallback(int256,int256,bytes)","10d1e85c": "uniswapV2Call(address,uint256,uint256,bytes)","f04f2707": "receiveFlashLoan(address[],uint256[],uint256[],bytes)","920f5c84": "executeOperation(address[],uint256[],uint256[],address,bytes)","1b11d0ff": "executeOperation(address,uint256,uint256,address,bytes)","91dd7346": "unlockCallback(bytes)","31f57072": "onMorphoFlashLoan(uint256,bytes)","e9cbafb0": "uniswapV3FlashCallback(uint256,uint256,bytes)","9a7bff79": "hook(address,uint256,uint256,bytes)","fb8e85d1": "camelotCall(address,uint256,uint256,bytes)","fa483e72": "swapCallback(int256,int256,bytes)","3a1c453c": "solidlyV3SwapCallback(int256,int256,bytes)","654b6487": "ramsesV2SwapCallback(int256,int256,bytes)","a9059cbb": "transfer(address,uint256)","095ea7b3": "approve(address,uint256)","70a08231": "balanceOf(address)","022c0d9f": "swap(uint256,uint256,address,bytes)","128acb08": "swap(address,bool,int256,uint160,bytes)","0902f1ac": "getReserves()","3850c7bd": "slot0()","e76c01e4": "globalState()","490e6cbc": "flash(address,uint256,uint256,bytes)","5c38449e": "flashLoan(address,address[],uint256[],bytes)","2e1a7d4d": "withdraw(uint256)","d0e30db0": "deposit()","23b872dd": "transferFrom(address,address,uint256)","552079dc": "fallback()","8da5cb5b": "owner()","ac9650d8": "multicall(bytes[])","fff6cae9": "sync()","bc25cf77": "skim(address)","0dfe1681": "token0()","d21220a7": "token1()","ddca3f43": "fee()","1a686502": "liquidity()","8201aa3f": "swapExactAmountIn(address,uint256,address,uint256,uint256)","3df02124": "exchange(int128,int128,uint256,uint256)","52bbbe29": "swap((bytes32,uint8,address,address,uint256,bytes),(address,bool,address,bool),uint256,uint256)","48c89491": "unlock(bytes)","11da60b4": "settle()","0b0d9c09": "take(address,address,uint256)","a5841194": "sync(address)"}
out["bytecode"] = {"size": len(b), "push4": len(set(sels)),
                   "known_selectors": sorted({KNOWN[s] for s in sels if s in KNOWN}),
                   "push20_addrs": sorted(push20)[:40], "n_push20": len(push20),
                   "has_selfdestruct": b"\xff" in b, "starts": code[:80],
                   "is_proxy_like": len(b) < 200}
out["slots"] = (L("code.json") or {}).get("slots")

# ---------- explorer ----------
ex = L("explorer.json") or {}
ad = ex.get("address") or {}
out["explorer"] = {k_: ad.get(k_) for k_ in ["creator_address_hash", "creation_transaction_hash", "is_verified", "name", "coin_balance", "implementations", "creation_status"]}
out["explorer"]["counters"] = ex.get("counters")
txs = ex.get("recent_txs") or []
out["explorer"]["recent_n"] = len(txs)
if txs:
    stat = collections.Counter(t.get("status") or t.get("result") for t in txs)
    out["explorer"]["status"] = dict(stat)
    out["explorer"]["methods"] = collections.Counter((t.get("method") or (t.get("raw_input") or "")[:10]) for t in txs).most_common(10)
    out["explorer"]["senders"] = collections.Counter((t.get("from") or {}).get("hash") for t in txs).most_common(10)
    ts = sorted(t.get("timestamp") for t in txs if t.get("timestamp"))
    out["explorer"]["time_span"] = [ts[0], ts[-1]] if ts else None
out["sender_info"] = {s: {"counters": v.get("counters"), "balance": (v.get("address") or {}).get("coin_balance"),
                          "is_contract": (v.get("address") or {}).get("is_contract")} for s, v in (ex.get("sender_info") or {}).items()}
out["creator_info"] = {"counters": (ex.get("creator_info") or {}).get("counters")}

# ---------- tx detail ----------
det = [d for d in (L("detail.json") or []) if "rc" in d and d["rc"]]
TOPICS = {
 "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822": "v2",
 "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67": "v3",
 "0x19b47279256b2a23a1665c810c8d55a1758940ee09377d4f8d26497a3577dc83": "pancakev3",
 "0x087f22af9b429eb1558f395356b51eef6c92b05dde10c8790d83f15b7e8d2d78": "v3fee",
 "0x121cb44ee54098b1a04743c487e7460d8dd429b27f88b1f4d4767396e1a59f79": "v3x",
 "0x4e7710fa9f20c2293561bd73d0fe3883d7be61d7d4b505e162242b12869ac5ec": "?",
 "0x40e9cecb9f5f1f1c5b9c97dec2917b7ee92e57ba5563708daca94dd84ad7112f": "univ4",
 "0x2170c741c41531aec20e7c107c24eecfdd15e69c9bb0a8dd37b1840b9e0b207b": "balancer",
 "0x8b3e96f2b889fa771c53c981b40daf005f63f637f1869f707052d15a3dd97140": "curve",
 "0xb2e76ae99761dc136e598d4a629bb347eccb9532a5f8bbd72e18467c3c34cc98": "curve2",
 "0xb3e2773606abfd36b5bd91394b3a54d1398336c65005baf7bf7a05efeffaf75b": "?2",
 "0x660fdc64912f07f47601a41f4b6feff0281bf19e46513e49b442aa27e80667a7": "?3",
}
FLASH = {"0x0d7d75e01ab95780d3cd1c8ec0dd6c2ce19e3a20427eec8bf53283b6fb8e95f0": "balancer_flashloan",
         "0xefefaba5e921573100900a3ad9cf29f222d995fb3b6045797eaea7521bd8d6f0": "aave_flashloan",
         "0xbdbdb71d7860376ba52b25a5028beea23581364a40522f6bcfb86bb1f2dca633": "v3_flash",
         "0x55c5a9b0f439747391f923543c9c732ff39aa1fd13b98cca42c34d2429201669": "?flash"}
TR = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
rows = []
pool_ct, venue_ct, topic_unknown, flash_ct, senders, sel_ct, tb = (collections.Counter() for _ in range(7))
for d in det:
    tx, rc, bl = d["tx"], d["rc"], d["block"]
    ok = rc["status"] == "0x1"
    gas = H(rc["gasUsed"]); egp = H(rc.get("effectiveGasPrice") or tx.get("gasPrice"))
    base = H(bl.get("baseFee")); l1 = H(rc.get("gasUsedForL1") or "0x0")
    fee_eth = gas * egp / 1e18
    prio = max(0, egp - base)
    swaps, net = [], collections.defaultdict(int)
    for lg in rc["logs"]:
        t0 = lg["topics"][0] if lg["topics"] else None
        if t0 in TOPICS:
            swaps.append((lg["address"].lower(), TOPICS[t0]))
        elif t0 in FLASH: flash_ct[FLASH[t0]] += 1
        if t0 == TR and len(lg["topics"]) == 3:
            fr = "0x" + lg["topics"][1][26:]; to = "0x" + lg["topics"][2][26:]
            amt = int(lg["data"], 16) if lg["data"] not in ("0x", "") else 0
            if to in (C, BEN): net[lg["address"].lower()] += amt
            if fr in (C, BEN): net[lg["address"].lower()] -= amt
        elif t0 and t0 not in TOPICS and t0 != TR and t0 not in FLASH:
            topic_unknown[t0] += 1
    usd, unpriced = 0.0, {}
    for tok, amt in net.items():
        if amt == 0: continue
        if tok in TOK: usd += amt / 10 ** TOK[tok][1] * TOK[tok][2]
        else: unpriced[tok] = amt
    for p, v in swaps: pool_ct[p] += 1; venue_ct[v] += 1
    senders[tx["from"].lower()] += 1
    sel_ct[(tx.get("input") or "")[:10]] += 1
    tb[str(rc.get("timeboosted"))] += 1
    prev = d.get("prev_detail") or {}
    prev_pools = {l["address"].lower() for l in prev.get("logs", []) if l["topics"] and l["topics"][0] in TOPICS}
    rows.append(dict(hash=tx["hash"], ok=ok, block=H(rc["blockNumber"]), idx=H(rc["transactionIndex"]), ntx=bl["ntx"],
                     ts=H(bl["ts"]), gas=gas, l1gas=l1, egp=egp, base=base, prio=prio, fee_usd=fee_eth * ETH,
                     tip_usd=gas * prio / 1e18 * ETH, profit_usd=usd, unpriced=len(unpriced), nswaps=len(swaps),
                     pools=[p for p, _ in swaps], venues=[v for _, v in swaps], input_len=(len(tx.get("input") or "0x") - 2) // 2,
                     to=(tx.get("to") or "").lower(), tbz=rc.get("timeboosted"), type=tx.get("type"),
                     prev_shares_pool=bool(prev_pools & {p for p, _ in swaps}) if prev else None,
                     prev_to=(prev.get("to") or "").lower() if prev else None, nonce=H(tx["nonce"])))
okr = [r for r in rows if r["ok"]]
def q(xs, ps=(10, 25, 50, 75, 90, 99)):
    xs = sorted(xs)
    if not xs: return None
    return {f"p{p}": round(xs[min(len(xs) - 1, int(len(xs) * p / 100))], 6) for p in ps} | {"mean": round(sum(xs) / len(xs), 6), "sum": round(sum(xs), 4), "n": len(xs)}
out["tx"] = {
 "n_detail": len(rows), "n_ok": len(okr), "fail_rate": 1 - len(okr) / len(rows) if rows else None,
 "to_counts": collections.Counter(r["to"] for r in rows).most_common(3),
 "senders": senders.most_common(10), "selectors": sel_ct.most_common(10), "timeboosted": dict(tb),
 "types": collections.Counter(r["type"] for r in rows).most_common(),
 "gas_used": q([r["gas"] for r in okr]), "gas_failed": q([r["gas"] for r in rows if not r["ok"]]),
 "egp_gwei": q([r["egp"] / 1e9 for r in rows]), "base_gwei": q([r["base"] / 1e9 for r in rows]),
 "prio_gwei": q([r["prio"] / 1e9 for r in rows]), "fee_usd": q([r["fee_usd"] for r in rows]),
 "fee_usd_failed": q([r["fee_usd"] for r in rows if not r["ok"]]),
 "tip_usd": q([r["tip_usd"] for r in rows]),
 "profit_usd_ok": q([r["profit_usd"] for r in okr]),
 "net_after_fee_ok": q([r["profit_usd"] - r["fee_usd"] for r in okr]),
 "n_with_unpriced": sum(1 for r in okr if r["unpriced"]),
 "nswaps": collections.Counter(r["nswaps"] for r in okr).most_common(),
 "input_len": q([r["input_len"] for r in rows]),
 "idx_in_block": q([r["idx"] for r in rows]), "ntx_block": q([r["ntx"] for r in rows]),
 "idx0_share": sum(1 for r in rows if r["idx"] <= 1) / len(rows) if rows else None,
 "prev_shares_pool": collections.Counter(r["prev_shares_pool"] for r in rows if r["prev_shares_pool"] is not None),
 "prev_to_top": collections.Counter(r["prev_to"] for r in rows if r["prev_to"]).most_common(8),
 "venues": venue_ct.most_common(), "flash_events": dict(flash_ct),
 "unknown_topics": topic_unknown.most_common(12),
 "top_pools": pool_ct.most_common(30), "n_pools": len(pool_ct),
}
if rows:
    ts = [r["ts"] for r in rows]; span_h = (max(ts) - min(ts)) / 3600 or 1
    out["tx"]["span_hours"] = span_h
    out["tx"]["tx_per_hour"] = len(rows) / span_h
    out["tx"]["ok_per_hour"] = len(okr) / span_h
    out["tx"]["profit_per_day_est"] = sum(r["profit_usd"] for r in okr) / span_h * 24
    out["tx"]["fees_per_day_est"] = sum(r["fee_usd"] for r in rows) / span_h * 24
    hours = collections.Counter((r["ts"] // 3600) % 24 for r in okr)
    out["tx"]["by_hour_utc"] = sorted(hours.items())
    blk = collections.Counter(r["block"] for r in rows)
    out["tx"]["txs_per_block"] = collections.Counter(blk.values()).most_common(5)
    out["tx"]["nonce_range"] = [min(r["nonce"] for r in rows), max(r["nonce"] for r in rows)]
    big = sorted(okr, key=lambda r: -r["profit_usd"])[:10]
    out["tx"]["top_profit"] = [(r["hash"], round(r["profit_usd"], 2), r["venues"], r["idx"], r["ntx"]) for r in big]
    # distribution of profit buckets
    bk = collections.Counter()
    for r in okr:
        p = r["profit_usd"]
        bk["<0" if p < 0 else "<0.1" if p < .1 else "<1" if p < 1 else "<10" if p < 10 else "<100" if p < 100 else ">=100"] += 1
    out["tx"]["profit_buckets"] = dict(bk)
    sh = sorted((r["profit_usd"] for r in okr), reverse=True); tot = sum(sh) or 1
    out["tx"]["top10pct_profit_share"] = sum(sh[: max(1, len(sh) // 10)]) / tot

# ---------- timeboost auction ----------
auc = L("auction.json")
AR = "0xba33ca01270fa7225ca41c533185e889de0c16c5ea0174c72d32cf71649622ae"
SC = "0xb59adc820ca642dad493a0a6e0bdf979dcae037dea114b70d5c66b1c0b791c4b"
if isinstance(auc, list):
    res = []
    for lg in auc:
        if lg["topics"][0] == AR:
            dd = lg["data"][2:]
            words = [dd[i:i+64] for i in range(0, len(dd), 64)]
            # indexed: isMulti? round? bidder? — fall back to raw
            res.append({"topics": lg["topics"], "words": words})
    ctrl = collections.Counter()
    bidders = collections.Counter()
    prices = []
    for r in res:
        t = r["topics"]; w = r["words"]
        # AuctionResolved(bool isMultiBidAuction, uint64 indexed round, address indexed firstPriceBidder, address indexed firstPriceExpressLaneController, uint256 price, uint64 roundStart, uint64 roundEnd)
        if len(t) == 4:
            bidders["0x" + t[2][26:]] += 1; ctrl["0x" + t[3][26:]] += 1
            prices.append(int(w[1], 16) / 1e18 if len(w) > 1 else None)
    out["timeboost"] = {"n_logs": len(auc), "n_resolved": len(res), "controllers": ctrl.most_common(10),
                        "bidders": bidders.most_common(10),
                        "price_eth": q([p for p in prices if p is not None]),
                        "topics0": collections.Counter(l["topics"][0] for l in auc).most_common(6)}
    bot_eoas = {s for s, _ in senders.most_common(10)}
    out["timeboost"]["bot_is_controller"] = [c for c in ctrl if c in bot_eoas or c == C]

# ---------- our ledger vs bot ----------
led = L("ledger.json"); cto = L("closer_to.json")
if led and isinstance(led.get("opps"), list):
    opps = led["opps"]
    botpools = set(pool_ct)
    first_ts = min((r["ts"] for r in rows), default=0)
    w = [o for o in opps if o.get("open_ts", 0) >= first_ts]
    pair_hit = [o for o in w if o["pool_a"].lower() in botpools or o["pool_b"].lower() in botpools]
    both_hit = [o for o in w if o["pool_a"].lower() in botpools and o["pool_b"].lower() in botpools]
    out["ledger"] = {"n_opps_total": len(opps), "n_opps_in_window": len(w),
                     "opps_touching_bot_pool": len(pair_hit), "opps_both_pools_bot": len(both_hit),
                     "close_kinds_window": collections.Counter(o["close_kind"] for o in w).most_common(),
                     "gross_window": q([o["gross_usd"] for o in w if not o["suspect"]]),
                     "our_pools": len(led.get("pools") or [])}
    ourpools = {p["address"].lower() for p in led.get("pools") or []}
    out["ledger"]["bot_pools_we_track"] = sum(1 for p in botpools if p in ourpools)
    out["ledger"]["bot_pool_swaps_we_track_share"] = sum(c for p, c in pool_ct.items() if p in ourpools) / (sum(pool_ct.values()) or 1)
    out["ledger"]["bot_top_pools_untracked"] = [(p, c) for p, c in pool_ct.most_common(40) if p not in ourpools][:15]
    if cto:
        tm = cto["to_map"]
        tos = collections.Counter((v.get("to") or "").lower() for v in tm.values() if "to" in v)
        out["ledger"]["closer_to_top"] = tos.most_common(12)
        out["ledger"]["closer_is_bot"] = tos.get(C, 0); out["ledger"]["closer_n"] = sum(tos.values())
        out["ledger"]["bot_hash_overlap"] = len(cto.get("bot_hash_overlap", []))
    # closers by sender, fee
    cl = led.get("closers") or []
    if isinstance(cl, list) and cl:
        out["ledger"]["closer_fee_usd"] = q([c["total_fee_usd"] for c in cl])
        out["ledger"]["closer_prio_usd"] = q([c["priority_usd"] for c in cl])
        out["ledger"]["closer_senders"] = collections.Counter(c["from_addr"].lower() for c in cl).most_common(8)
    out["ledger"]["bot_senders_in_closers"] = sum(1 for c in cl if c["from_addr"].lower() in {s for s, _ in senders.most_common(10)}) if isinstance(cl, list) else None
tr = L("traces.json") or {}
out["traces_n"] = len(tr)
json.dump(rows, open(os.path.join(D, "rows.json"), "w"))
print(json.dumps(out, indent=1, default=str))
