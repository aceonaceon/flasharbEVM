#!/usr/bin/env python3
"""Read-only data collector for an Arbitrum arbitrage contract.
Stdlib only. No keys, no signing, no transactions sent.
Usage: python3 collect_0xef0d.py --out DIR [--hours 24] [--max-detail 1500] [--ledger ledger.sqlite]
"""
import argparse, json, os, sys, time, urllib.request, urllib.error, sqlite3, tarfile

C = "0xef0df9a49025512f0d5e3b45dcc27903af984834"
RPCS = [
    "https://arb1.arbitrum.io/rpc",
    "https://arbitrum-one-rpc.publicnode.com",
    "https://arbitrum.drpc.org",
    "https://arbitrum.meowrpc.com",
]
BLOCKSCOUT = "https://arbitrum.blockscout.com/api/v2"
TRANSFER = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
AUCTION = "0x5fcb496a31b7ae91e7c9078ec662bd7a55cd3079"  # Timeboost ExpressLaneAuction (Arbitrum One)
FEEDS = {  # Chainlink USD feeds on Arbitrum One
    "ETH": "0x639fe6ab55c921f74e7fac1ee960c0b6293ba612",
    "BTC": "0x6ce185860a4963106506c203335a2910413708e9",
    "ARB": "0xb2a824043730fe05f3da2efafa1cbbe83fa548d6",
}
UA = {"User-Agent": "research-script/0.1", "Content-Type": "application/json"}

def log(*a):
    print(time.strftime("%H:%M:%S"), *a, file=sys.stderr, flush=True)

_last = [0.0]
def throttle(rps=6):
    dt = time.time() - _last[0]
    if dt < 1.0 / rps:
        time.sleep(1.0 / rps - dt)
    _last[0] = time.time()

def http_json(url, body=None, timeout=40):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, headers=UA, method="POST" if data else "GET")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read().decode())

def rpc(method, params, prefer=None, tries=6):
    order = ([prefer] if prefer else []) + [u for u in RPCS if u != prefer]
    err = None
    for attempt in range(tries):
        url = order[attempt % len(order)]
        throttle()
        try:
            res = http_json(url, {"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
            if "error" in res:
                err = res["error"]
                msg = json.dumps(err).lower()
                if any(s in msg for s in ["range", "too many", "limit", "exceed", "10000", "response size"]):
                    raise ValueError("RANGE:" + msg)
                if "rate" in msg or "429" in msg:
                    time.sleep(2 + attempt * 2); continue
                # method not supported etc: try next provider
                continue
            return res["result"]
        except ValueError:
            raise
        except urllib.error.HTTPError as e:
            err = f"HTTP {e.code}"
            time.sleep(1.5 + attempt * 2)
        except Exception as e:
            err = repr(e)
            time.sleep(1 + attempt)
    raise RuntimeError(f"{method} failed: {err}")

def save(out, name, obj):
    with open(os.path.join(out, name), "w") as f:
        json.dump(obj, f)
    log("saved", name)

def get_logs(frm, to, address=None, topics=None, chunk=20000):
    res, cur = [], frm
    while cur <= to:
        end = min(cur + chunk - 1, to)
        flt = {"fromBlock": hex(cur), "toBlock": hex(end)}
        if address: flt["address"] = address
        if topics: flt["topics"] = topics
        try:
            r = rpc("eth_getLogs", [flt])
            res.extend(r); cur = end + 1
            if chunk < 20000 and len(r) < 2000: chunk = min(20000, chunk * 2)
        except ValueError:
            chunk = max(500, chunk // 4); log("shrink chunk", chunk)
        except RuntimeError as e:
            log("getLogs error, shrinking", e); chunk = max(500, chunk // 4); time.sleep(3)
    return res

def blockscout(path, params=""):
    url = f"{BLOCKSCOUT}{path}{params}"
    for a in range(4):
        try:
            time.sleep(0.4)
            return http_json(url)
        except Exception as e:
            log("blockscout err", path, e); time.sleep(2 + a * 2)
    return {"error": "failed", "url": url}

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--hours", type=float, default=24)
    ap.add_argument("--max-detail", type=int, default=1500)
    ap.add_argument("--prev-sample", type=int, default=300)
    ap.add_argument("--ledger", default=None)
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)

    # 1. bytecode + proxy slots
    code = rpc("eth_getCode", [C, "latest"])
    slots = {}
    for name, s in [("eip1967_impl", "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc"),
                    ("eip1967_admin", "0xb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103")] + \
                   [(f"slot{i}", hex(i)) for i in range(8)]:
        try: slots[name] = rpc("eth_getStorageAt", [C, s, "latest"])
        except Exception as e: slots[name] = str(e)
    save(a.out, "code.json", {"address": C, "code": code, "slots": slots,
                              "balance": rpc("eth_getBalance", [C, "latest"])})

    # 2. explorer metadata + recent txs (includes failed ones)
    meta = {"address": blockscout(f"/addresses/{C}"),
            "counters": blockscout(f"/addresses/{C}/counters"),
            "smart_contract": blockscout(f"/smart-contracts/{C}")}
    txs, params = [], ""
    for page in range(30):
        r = blockscout(f"/addresses/{C}/transactions", params)
        items = r.get("items") or []
        txs.extend(items)
        nxt = r.get("next_page_params")
        if not items or not nxt: break
        params = "?" + "&".join(f"{k}={v}" for k, v in nxt.items() if v is not None)
    meta["recent_txs"] = txs
    senders = {}
    for t in txs:
        f = (t.get("from") or {}).get("hash")
        if f: senders[f] = senders.get(f, 0) + 1
    meta["sender_info"] = {}
    for s, _ in sorted(senders.items(), key=lambda x: -x[1])[:5]:
        meta["sender_info"][s] = {"address": blockscout(f"/addresses/{s}"),
                                  "counters": blockscout(f"/addresses/{s}/counters")}
    creator = (meta["address"] or {}).get("creator_address_hash")
    if creator:
        meta["creator_info"] = {"address": blockscout(f"/addresses/{creator}"),
                                "counters": blockscout(f"/addresses/{creator}/counters")}
    save(a.out, "explorer.json", meta)

    # 3. transfer logs to/from the contract over the window
    head = int(rpc("eth_blockNumber", []), 16)
    frm = max(0, head - int(a.hours * 14400))
    pad = "0x" + "0" * 24 + C[2:]
    log("window blocks", frm, head)
    tin = get_logs(frm, head, topics=[TRANSFER, None, pad])
    tout = get_logs(frm, head, topics=[TRANSFER, pad])
    save(a.out, "transfers.json", {"from_block": frm, "to_block": head, "in": tin, "out": tout})

    # 4. Timeboost auction logs over the same window
    try:
        auc = get_logs(frm, head, address=AUCTION)
    except Exception as e:
        auc = {"error": str(e)}
    save(a.out, "auction.json", auc)

    # 5. prices
    prices = {}
    for k, addr in FEEDS.items():
        try: prices[k] = rpc("eth_call", [{"to": addr, "data": "0xfeaf968c"}, "latest"])
        except Exception as e: prices[k] = str(e)
    save(a.out, "prices.json", prices)

    # 6. per-tx detail
    hashes = {}
    for l in tin + tout:
        hashes[l["transactionHash"]] = int(l["blockNumber"], 16)
    for t in txs:
        bn = t.get("block_number") or t.get("block")
        if t.get("hash") and bn:
            hashes.setdefault(t["hash"], int(bn))
    ordered = sorted(hashes.items(), key=lambda x: -x[1])[: a.max_detail]
    log("detail txs", len(ordered), "of", len(hashes))
    blocks, detail = {}, []
    for i, (h, bn) in enumerate(ordered):
        try:
            tx = rpc("eth_getTransactionByHash", [h])
            rc = rpc("eth_getTransactionReceipt", [h])
            if bn not in blocks:
                b = rpc("eth_getBlockByNumber", [hex(bn), False])
                blocks[bn] = {"ts": b["timestamp"], "baseFee": b.get("baseFeePerGas"),
                              "txs": b["transactions"], "l1": b.get("l1BlockNumber")}
            b = blocks[bn]
            idx = int(rc["transactionIndex"], 16)
            d = {"tx": tx, "rc": rc, "block": {"ts": b["ts"], "baseFee": b["baseFee"], "ntx": len(b["txs"]), "l1": b["l1"]},
                 "prev": b["txs"][idx - 1] if idx > 0 else None,
                 "next": b["txs"][idx + 1] if idx + 1 < len(b["txs"]) else None}
            if i < a.prev_sample and d["prev"]:
                try:
                    p = rpc("eth_getTransactionReceipt", [d["prev"]])
                    pt = rpc("eth_getTransactionByHash", [d["prev"]])
                    d["prev_detail"] = {"from": p["from"], "to": p["to"], "status": p["status"],
                                        "gasUsed": p["gasUsed"], "effectiveGasPrice": p.get("effectiveGasPrice"),
                                        "input4": (pt.get("input") or "")[:10],
                                        "logs": [{"address": l["address"], "topics": l["topics"], "data": l["data"]} for l in p["logs"]]}
                except Exception as e:
                    d["prev_detail"] = {"error": str(e)}
            detail.append(d)
        except Exception as e:
            detail.append({"hash": h, "error": str(e)})
        if i % 100 == 0:
            log("detail", i)
            save(a.out, "detail.json", detail)
    save(a.out, "detail.json", detail)

    # 7. call traces for a few txs (if any public RPC supports it)
    traces = {}
    ok = [d for d in detail if "rc" in d and d["rc"]["status"] == "0x1"][:12]
    for d in ok:
        h = d["tx"]["hash"]
        for url in RPCS:
            try:
                throttle()
                r = http_json(url, {"jsonrpc": "2.0", "id": 1, "method": "debug_traceTransaction",
                                    "params": [h, {"tracer": "callTracer"}]}, timeout=60)
                if "result" in r:
                    traces[h] = {"rpc": url, "trace": r["result"]}; break
            except Exception:
                pass
    save(a.out, "traces.json", traces)

    # 8. our own shadow ledger (optional, read-only)
    if a.ledger and os.path.exists(a.ledger):
        con = sqlite3.connect(f"file:{a.ledger}?mode=ro", uri=True)
        con.row_factory = sqlite3.Row
        def q(sql):
            try: return [dict(r) for r in con.execute(sql)]
            except Exception as e: return {"error": str(e)}
        led = {
            "coverage": q("SELECT * FROM coverage"),
            "opps": q("SELECT id,pool_a,pool_b,venue_a,venue_b,token0,token1,major,open_block,open_tx,gross_usd,peak_usd,gap_pct,suspect,min_depth_usd,close_block,close_tx,close_kind,closer_sender,delay_blocks,same_block,open_ts FROM opportunities"),
            "closers": q("SELECT * FROM closer_txs"),
            "pools": q("SELECT address,venue,token0,token1,fee_ppm,is_cl FROM pools"),
        }
        save(a.out, "ledger.json", led)
        # which closers were this contract?
        closes = [o["close_tx"] for o in led["opps"] if isinstance(led["opps"], list) and o.get("close_tx")] if isinstance(led["opps"], list) else []
        ours = set(hashes.keys())
        # look up `to` for the most recent 600 distinct close txs not already known
        uniq = list(dict.fromkeys(reversed(closes)))[:600]
        to_map = {}
        for h in uniq:
            try:
                t = rpc("eth_getTransactionByHash", [h])
                to_map[h] = {"to": t.get("to"), "from": t.get("from"), "block": t.get("blockNumber")}
            except Exception as e:
                to_map[h] = {"error": str(e)}
        save(a.out, "closer_to.json", {"to_map": to_map, "bot_hash_overlap": sorted(ours.intersection(closes))})

    tgz = a.out.rstrip("/") + ".tar.gz"
    with tarfile.open(tgz, "w:gz") as t:
        t.add(a.out, arcname=os.path.basename(a.out.rstrip("/")))
    log("DONE", tgz)

if __name__ == "__main__":
    main()
