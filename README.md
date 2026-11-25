# FlashArb 側錄器（shadow mode）

這個程式在 Arbitrum 上**只記錄、不交易**：它跟著鏈上每一筆 DEX 交易，找出「兩個池子之間出現的套利空間」，再記下最後是誰、在第幾個區塊、出多少手續費把它吃掉的。

它沒有私鑰，也不會送出任何交易。跑 14 天後，報表會告訴你這個套利計畫值不值得往下做。

## 它在回答什麼問題

計畫第 14 天要看的三個指標，都直接從帳本算出來：

| 指標 | 意思 | 未通過時，計畫的下一步 |
|---|---|---|
| 毛利中位數 ≥ $0.30 | 機會夠不夠大 | 流量問題 → 轉 Base（方案 B） |
| 每天可爭取淨利 ≥ $20 | 被別的套利機器人吃掉、但不是在同一個區塊吃掉的利潤，扣掉 gas | 不夠付成本 → 停掉 |
| 同區塊被吃掉比例 ≤ 50% | 是不是付費 Fast Feed 玩家在壟斷 | 評估 Fast Feed，或轉 B |

門檻都在 `config.toml` 的 `[gate]`。

## 在 Hetzner 上部署

映像檔由 GitHub Actions 編好推到 GHCR（`ghcr.io/aceonaceon/flasharbevm`），主機只拉下來跑，不在主機上編 Rust。

```bash
git clone https://github.com/aceonaceon/flasharbEVM /opt/flasharb
cd /opt/flasharb
cp .env.example .env               # 網頁要對外時再填 TUNNEL_TOKEN（見下面）
docker compose pull && docker compose up -d
docker compose logs -f shadow      # 看進度
```

- 資料存在 `/opt/flasharb/data/`：`ledger.sqlite` 是帳本，`report.md` 每小時更新一次。
- 第一次啟動會往回補抓 6 小時（`backfill_hours`），所以當天就有數據。
- 重新啟動會從上次停下的區塊接著跑，不會重複記錄。
- 容器限制 384MB 記憶體、1 顆 CPU，不會影響同一台主機上的其他網站。
- 如果 `docker compose pull` 顯示沒有權限，到 GitHub 的 Packages 把 `flasharbevm` 設成 Public（repo 本身是公開的，映像檔裡沒有任何秘密）。

## 狀態網頁

側錄器內建一個只讀網頁（port 8088，只在 compose 內部網路開放），對外由 Cloudflare Tunnel 轉進來，主機不開任何 port：

1. Cloudflare Zero Trust → Networks → Tunnels → 建立一個 tunnel，複製 token 寫進 `.env` 的 `TUNNEL_TOKEN`，並加上 `COMPOSE_PROFILES=tunnel`，再 `docker compose up -d`。
2. 在這個 tunnel 加一個 Public Hostname，例如 `arb.你的網域`，Service 填 `http://shadow:8088`。
3. Zero Trust → Access → Applications 新增這個子網域，Policy 只放行你自己的 email。

畫面內容：判決燈號與距離第 14 天的倒數、三個指標的數值與每日趨勢、側錄健康狀態（處理到哪個區塊、落後多久、RPC 錯誤數）、機會怎麼結束的比例、最近 20 筆機會（連到 Arbiscan）、對手排行與對手平均 α。

只想在本機看帳本、不跑側錄時：`flasharb-shadow serve`。

隨時看文字報表：

```bash
docker compose run --rm shadow report        # 印在畫面上
cat data/report.md                           # 或看最近一次自動產生的
```

## 換成更快的 RPC（選用）

預設用免費的 Arbitrum 公開 RPC。遇到速率限制、進度一直落後時，可以去 Alchemy 開一個**免費**帳號，然後在 `/opt/flasharb/.env` 寫：

```
FLASHARB_RPC_URL=https://arb-mainnet.g.alchemy.com/v2/你的key
```

再 `docker compose up -d` 就會換過去。`.env` 不會進 git。

## 它怎麼判斷「機會」

1. 抓所有 Uniswap v3、Sushi v3、Camelot v3（Algebra）、Aero CL 的 `Swap` 事件，以及 Uniswap v2、Sushi v2、Camelot v2、Aero v2 的 `Sync` 事件。
2. 每個池子都要能被它的 factory 反查回來才採用，假池子發的假事件會被排除。
3. 每筆交易結束後，拿同一個交易對的兩個池子來算：在兩邊各付一次手續費之後，最佳投入量下能賺多少（封閉解，見 `src/amm.rs`）。只在交易邊界算，所以路由器一筆交易裡同時動好幾個池子，不會被誤判成機會。
4. 毛利超過 `min_gross_usd` 就開一筆機會；之後第一筆讓毛利掉到一半以下的交易就是「收單者」：
   - 同時動到兩個池子 → 別的套利者吃掉的（`arb`）
   - 只動到一個池子 → 一般交易剛好抹平（`organic`）
   - 10 分鐘都沒人動 → 過期（`stale`，通常是代幣有問題或流動性假象）
5. 抽樣抓一部分套利者的交易收據，算出它們付的 priority fee 佔機會毛利的比例，也就是對手的 α。

## 已知的近似

- CL 池用目前 tick 範圍內的虛擬儲備估算。單筆量大到跨 tick 時會高估，所以價差超過 30% 的會另外標成「可疑」。
- 只看 Swap 事件，沒追 Mint/Burn。流動性變了，要等到下一筆 swap 才會更新。
- Aero CL 的手續費由 hook 決定：這裡只算了 swap 前的費率，swap 後可能另外加收的部分沒算。
- Camelot v3 的動態費率只在發現池子時讀一次。
- 「可爭取利潤」是理論上限，還沒扣我們自己的延遲和勝率。

## 開發

```bash
cargo test          # 27 個單元測試，不需要網路
cargo run -- run    # 本機直接跑
```

| 檔案 | 職責 |
|---|---|
| `src/events.rs` | 解碼 Swap / Sync 事件 |
| `src/pools.rs` | 用 Multicall3 讀池子資訊、向 factory 驗證 |
| `src/amm.rs` | 虛擬儲備與兩腳套利的封閉解 |
| `src/price.rs` | 只用池子本身狀態算出的美元報價 |
| `src/detector.rs` | 機會的開啟、結束與分類（純邏輯，無 I/O） |
| `src/ledger.rs` | SQLite 帳本 |
| `src/report.rs` | 報表與通過門檻 |
| `src/rpc.rs` | 速率限制、重試、自動調整 getLogs 範圍 |
| `src/dashboard.rs`、`src/web.rs`、`web/index.html` | 狀態網頁（只讀） |
