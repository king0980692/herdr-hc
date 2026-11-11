# Herdr Native / shared hc 交接清單

> **狀態：WIP checkpoint，尚未部署或完成實機驗收。**
> 本次交接包含目前的實作與回歸測試，不代表可以直接升級共用 core 或切換使用中的 frontend。

## 1. 目標與範圍

保留原本 Herdr TUI 的版面、鍵盤操作、pane 邊框、焦點與儲存布局，改由 hc / herdr-core 提供真正的 terminal、program 與 agent 狀態。Native 應讀取 **hc 預設共用 machine registry**，而不是複製另一份 SSH 機器清單。

- `herdr`：原本 legacy release，保留不動。
- `herdr-native`：目前已安裝的 hc-backed snapshot；**尚未安裝本次 checkpoint**。
- 新來源第一次開啟、且沒有既有 Native layout 時：將該 owner 現存的 live programs 各投影成一個 tab。
- 已儲存 layout 保持原樣；不得為了同步 inventory 重排或覆蓋。
- 明確關閉 pane：終止該 pane 精確綁定的共用 program，其他 viewer 會看到結束。
- 關閉／斷開整個 TUI 或 presenter：只 detach，不能殺 program。
- 原有 explicit legacy SSH profile / optional `hc_path` 保留相容性，但不能成為共用 registry 的權威來源。

## 2. 已完成的實作

- [x] 邊框標題修復：保留 raw OSC title metadata，但不再把它直接提升成有效 border title；保留 explicit/manual/agent label 的優先序。
- [x] `src/hc/machines.rs`：完整、保守驗證 machine graph；保留 offline / unknown 狀態；部分、重複或不安全 inventory 不能當成空清單。
- [x] Local background cache + advertised JSON `runtime.machines`；inventory refresh 不在 render、layout 或每 pane 的熱路徑執行。
- [x] `runtime.source` optional `owner_core_id` / `registry_core_id`；更新現行 API schema，**沒有修改 frozen generation-1 codecs / fixtures**。
- [x] `src/hc/shared.rs`：分別 pin registry 與 owner，驗證真實 owner hello，retarget / identity mismatch fail closed；provider `via` 不支援，不能假裝成 SSH。
- [x] `ClientEndpointId::Hc` + `SharedCoreKey`；來源 key 為 `hc:<registry-core-id>/<owner-core-id>`，不是假造的 legacy ProfileId。
- [x] `src/remote/shared.rs`：有 host-instance fence 的本機 per-owner presenter；不得覆蓋不屬於此來源的既有 presenter。
- [x] TUI background catalog reconciliation、來源選擇、重連、移除／改 route 的取消與 freeze；rename 只改顯示，不改身份。
- [x] 修復快照更新把已選 Shared owner 偷切回 Local 的問題；包含 reconnect 回歸測試。
- [x] Shared 選擇恢復時，Local 先 metadata-only 並凍結輸入，避免打字送到錯誤 owner。**legacy SSH 則維持既有契約：健康的 Local 不因遠端不可用而失去操作能力。**
- [x] 成功且完整的 inventory 若移除 persisted Shared 選擇，即使初始 catalog 同樣為空，也能恢復 Local；offline-but-present 不得當成移除。
- [x] `src/app/hc/bootstrap.rs`：first-open live owner-local tabs；`terminal.list all=false`；排除 qualified aggregate records 與 finished history，驗證 discovery 前後 source generation。沒有 `terminal.run` / replay / adoption。
- [x] 首次 attach 使用來源已有尺寸；沒有 active viewer 的 headless render 不得 resize 既有 active/background tabs。
- [x] CLI `--machine` 即時從預設 registry 解析 core ID / 名稱 / scoped key。registry lookup 失敗不能改走同名 legacy label；explicit legacy profile ID 仍可使用。
- [x] Local 與 shared lookup 共用 default-registry command policy，清除繼承的 `HC_SOCKET` / `HC_SESSION` 等選擇器。
- [x] API preflight / watcher：cancellable connect、nonblocking write、poll-before-read deadline；避免 Windows 不支援 socket timeout 時無限卡住。**這只是結構與 macOS 測試通過，不是 Windows 實機驗收。**
- [x] 共用機器 collapse preferences 與 jump identity 可 round-trip；保留舊 SSH preference strings。
- [x] 修復空來源啟動 retry 不喚醒 headless loop：`Recovery::deadline()` 現在包含 `initial_inventory_due`，四個 pane 的 recovery 併發上限不變。
- [x] EN / JA / ZH-CN draft CLI、connecting-machines、socket API 文件及現行 generated API schema 更新。

## 3. 最新驗證結果

### 已通過

主要穩定性驗證為 **bg175**，依序執行：

1. `just test-one hc_initial_inventory_retry_wakes_an_empty_headless_source`：1 passed。
2. `just ci`：lint / clippy `-D warnings`、完整 Rust 與輔助檢查通過。
3. `just docs-contract-test`：通過。
4. `just ci-tests`：第二輪完整併發測試通過。
5. `just ci-tests`：第三輪完整併發測試通過。

**三輪完整 Rust 測試均為 4068 passed、17 個既有 skipped**；執行時間約 30.864s / 30.302s / 30.436s。沒有新增 ignore 或放寬測試／timeout。

先前啟動問題的處理：

- 一度把 startup freeze 套用到 legacy SSH，破壞既有 Local-direct 契約；已限縮為 Shared，原測試保留。
- Local 啟動在完整測試下曾間歇失敗，單獨重跑 10/10 通過，沒有把重跑通過直接當成修復。
- 找到確定的 timer 缺口：沒有 pane recovery 時，`initial_inventory_due` 不在 next deadline 中。新回歸測試在 bg174 先紅（None vs Some(due)），修正後 bg175 轉綠，並通過上述三輪完整測試。
- 原 startup integration assertion 新增唯讀失敗診斷：`runtime.source`、`pane.list`、隔離 fixture 的 core terminal facts；未放寬斷言。

### 已完成的有限實機證據

- bg160 曾對 gx10 / leon 各建立一個**新擁有的** disposable presenter，驗證真實 owner / registry / target / generation / connected facts，並精確停止這兩個 presenter、證明 socket 不可達。
- 這次是 **control-only**：沒有 attach、run、input、resize、kill，也沒有 core stop；之前／之後的共用 program、PID、holder 與尺寸相同。
- **此證據早於 bootstrap / TUI 整合，不能當成最新 TUI 或多 viewer 驗收。不要直接重跑舊 probe，誤以為新 bootstrap 仍不會 attach。**
- 兩份獨立實作審查與一次 retained fix verification 已完成；發現的 source selection、fallback、namespace、timeout、無 viewer resize、移除恢復問題已修復並加入測試。

### 仍不通過／不能宣稱完成

- `just bench-render-scale`（bg165）：7 個 profile tests passed，1 個失敗：
  `native_file_render_scale_profile` 要求 working CoW retention，不能用 decoded fallback 代替。
- 支援性數據（120x40、修復 review 前的 parser/render fixture，**非 native I/O throughput**）：hc background 1→15 panes median 405→451µs（1.11x）；active 412→477µs（1.16x）。15 panes 時 hc/legacy 為 background 1.02x、active 0.99x。不能把這些數據說成整個 benchmark 通過，也不是最終 checkpoint 的新 benchmark。
- **`just check` 尚未完整通過**：提交前另跑 bg176；lint、4068 Rust passed / 17 skipped 與 CI 輔助測試再次通過，之後 `windows-lint` 因 Windows cross SDK configuration 缺失而停止。沒有安裝 SDK、接受 license、sudo、Xcode 或 cache workaround。使用者已明確確認：以已知仍缺實機／Windows／CoW 驗收的 WIP checkpoint 推送實作與此文件；此確認不是 production-ready 或後續部署的全面授權。
- fresh Linux debug=1 runner 先前在 2GiB 限額下 rustc OOM；沒有新的完整 Linux／Windows／CoW／reboot acceptance。

## 4. 接力的優先檢查清單

### P0：多 viewer / 舊版本相容性，先隔離，後共用

- [ ] 重新讀取 Local / gx10 / leon 的實際 owner、版本、core PID、holder、program bindings / argv / root PID / created / keep / lifecycle / 尺寸，保存 before / after。
- [ ] 使用全新的 private XDG config/state/runtime directory、空 core 與自建測試 program 做 qualification；第一個 hello 必須證明它**不是**任何使用中的 owner，才能 run / attach / input / resize / kill。
- [ ] 至少兩個 viewer：驗證手動輸入、automatic parser replies、focus、resize、background viewer、detach、重新連接、program PID 與 owner 不變。
- [ ] 特別檢查 hc 0.1.17：目前不宣告 `automatic_input`。Native 的舊相容路徑會把 automatic response 退回 DATA，可能影響 input leadership / geometry；**尚未實機證明，不可直接在工作 program 上試。**
- [ ] 可先做最小幾何檢查：A 控制 100x35、B 保有 80x24；B 送 Native 同樣 capability/fallback framing 的 automatic response，觀察是否偷走 A 的控制尺寸。這不取代真實 query/reply 與重複回覆驗證。
- [ ] 只有證明需要之後，才規劃 preserving hc update / restart；必須保留原 program、root PID、owner、holder、資料與尺寸，**不是 stop shared core**。
- [ ] 不得降版使用者已獨立更新的 normal local hc，也不得修改 pinned herdr-core source。

現有 `src/hc/live_tests.rs` 有隔離 Target、strict SSH、cleanup scaffold；整個舊 smoke 並不是只有上述小測試，執行前先讀完整內容及 resource scope。

本機另有尚未執行的 `.local/prd/qualify_hc_multiview.py` 草稿；它是 ignored 的探索稿，**不在交接 commit，也沒有任何通過證據**。接手者應自行審查／重建最小 probe，不可將草稿當成已驗收測試。

### P0：實際 Native TUI，先候選版，後安裝

- [ ] 建立不可變候選 binary；記錄 SHA，不能覆寫受保護的 `target/release/herdr` 或現有 immutable install。
- [ ] 在新擁有的 Rex tab / named presenter 驗證共用 machine catalog 與 existing live-program tabs；用安裝的 Rex CLI / skill，不直接操作 raw UI socket。
- [ ] 比較已儲存 layout，確認 public workspace/tab/pane ID、bindings 與焦點未被重編／重排。
- [ ] 檢查同一來源兩個 program 地址碰撞、不同 owner 同為 t1、canonical base30（例如 t1b），不能使用 registry ID、alias 或 PID 代替 owner identity。
- [ ] 真正畫面驗證 original border title、manual label / agent label precedence、背景更新與焦點。
- [ ] 驗證來源 rename、offline、移除、route/session/core/registry 改變、stale completion、取消，以及 reconnect UI；不可把失聯當成自然結束。
- [ ] 驗證 CLI 明確 scoped source 與 legacy custom `hc_path`；未知／失敗／不完整 registry 不可改送另一來源。
- [ ] 驗證 explicit pane close 只終止精確 binding；TUI quit / presenter loss 只 detach，不會 replacement shell。
- [ ] 驗證原有自訂快捷鍵，含 source-scoped `HERDR_ACTIVE_PANE_ID`、水平／垂直移動；sidebar/plugin migration 另需使用者核准，不可順便安裝或改 shared shell/ble。

### P1：Linux 驗收後的已知缺口（2026-11-11，WSL2 / Rust 1.96.1 / pinned hc 0.1.18）

macOS 會排除 `tests/cli.rs`、`tests/machine_api.rs` 與部分 `tests/api_ping.rs`，所以 hc 遷移後這些測試從未執行；Linux 首次完整執行為 48 個失敗。目前只剩 1 個（sigwinch，與 hc 無關），另有下列未解項目：

- [ ] **`worktree remove` 在 Native 無法使用**：workspace 內只要有 hc pane 就回 `unsupported_runtime`（`src/app/api.rs`、`src/app/api/worktrees/deferred.rs`），而 Native 的 pane 全是 hc pane。三個 CLI 測試目前斷言「被拒且 program／checkout 未被動到」。要恢復功能需實作以 `terminal.kill` 終止精確 binding 的 hc 版移除。
- [ ] **`--machine` 的政策待確認**：PATH 沒有 `hc` 時，以標籤選 legacy SSH 機器會 fail closed，只有完整 profile ID 可用（`tests/machine_api.rs` 與 `src/cli/target.rs` 的單元測試目前都固定此行為）。
- [ ] **`tests/machine_api.rs` 已改寫為 hc 架構，但失去部分覆蓋**：舊的 SSH API bridge 測試（遠端 herdr 探索／metadata 快取、stale path 與 ping timeout 復原、舊 bridge 拒絕）對應的程式路徑已不存在，沒有移植。來源離線（SSH 失敗）時 `--machine` 指令的行為目前沒有測試。
- [ ] **saved／shared 來源仍有 `agent start` 啟動競態**：`pane.process_info` 只對 Local 來源回報 hc 事實（遠端 PID 不外露），所以 CLI 對遠端來源不會在 shell 初始化期間重試。
- [ ] **`agent prompt --wait` 會漏掉瞬間的 working**：hc 事件串流有發出連續的 working→idle，但 Herdr 的投影只保留最新狀態。`tests/cli/agents.rs` 的假 agent 以 0.2s 停留避開。
- [ ] `client_mode::sigwinch_refreshes_host_palette_without_resizing` 在遷移前的 `ee5bfb50` 於此環境即失敗，與 hc 無關，原因未查。
- [ ] `cli::sessions::named_sessions_use_separate_servers_and_workspace_state` 曾在完整併發執行中失敗一次（beta session 多出標籤 `~` 的 workspace），之後多輪未重現；可能與 first-open bootstrap 有關，未查。

### P1：驗證與發布前 gate

- [ ] 每次修改後都要有一次完整的 **Linux** 執行（`cargo nextest run --locked --no-fail-fast`，pinned hc 0.1.18 置於 PATH 最前）：`tests/cli.rs`、`tests/machine_api.rs` 與部分 `tests/api_ping.rs` 在 macOS 不編譯，私人 repo 的 Actions 也已停用，沒有任何自動化會執行它們。
- [ ] 重跑接手後修改涉及的測試，以及 `just ci` / docs contracts；確認 frozen generation-1 codecs、wire-tag / digest / method-shape fixtures 未變。
- [ ] 解決 CoW retention 環境後重跑完整 `just bench-render-scale`（至少 1 / 15 populated panes、固定 geometry、active/background）；不能新加 ignore 或降低要求。
- [ ] 若需要完整 Windows check，先取得 SDK/license/setup 核准；不能捏造 fresh Windows／Linux acceptance。
- [ ] 若硬體／平台檢查仍受阻，明確提出範圍與風險，取得**本次 scope**的較窄驗證對齊；WIP checkpoint 不代表生產切換授權。
- [ ] 真正切換前再次備份 config、catalog、endpoint selection、layouts、Rex/frontend/presenter/core/holder/program facts；只更換 Native presentation。
- [ ] 成功切換後，才處理使用者已授權退役的舊 isolated gx10。先重新驗證 owner / program / process / cgroup；只終止該隔離環境精確擁有的資源，**保留資料**。
- [ ] 最終 production-ready commit 訊息重新對齊；只推私人 remote，絕不推 upstream / 直接發 release / merge。

## 5. 身份、機器與保護界線

以下為最近唯讀觀察，**不是 kill、stop 或覆寫的權威，操作前必須重讀**：

| 來源 | permanent owner core ID | 最近版本 |
| --- | --- | --- |
| Local/default registry | `baccf53f89810cf8ed1e77ca9a4b6e9c` | 0.1.19，使用者獨立更新 |
| 共用 gx10 | `3431734c422fe905bd487f2f495bdb4f` | 0.1.17 |
| 共用 leon | `6130a31a167522145c275e2185e6d395` | 0.1.17 |
| 舊 isolated gx10 | `67646f8cf08976eb03d60f93461983c5` | 必須重讀 |

重要規則：

- program binding = **permanent owner core ID + canonical source-local address**；registry、presentation host instance、legacy profile ID、SSH alias、PID、qualified aggregate address 都是不同東西。
- 舊 isolated gx10 與共用 gx10 是不同 owner，絕不能 relabel / adopt / replay 綁定。
- Direct SSH 支援；provider `via` 仍 fail closed。
- 不做 broad cleanup、`pkill`、猜 PID、replacement shell、unsupported live PTY import、local Git/filesystem fallback。
- cleanup：client → presenter → source，只處理本次新擁有的資源；有明確 stop confirmation 與 bounded 15s socket proof；無法確認就保留證據／資料。
- 舊 private resource policy 保持 2GiB / swap0 / 1CPU / affinity19 / tasks512；不能為了測試繞過。

舊 private profile：`697e765c959741aea71d055cfdd31d7b`；hc session：`hh-wg4gzdzp`。`/home/leon/.local/share/herdr-gx10/native-hc` 可能是 private namespace wrapper，不是可直接部署到共用 core 的 standalone binary。先讀實際檔案、版本、artifact hash、namespace；不能拿它直接覆蓋 normal hc。

## 6. 建置與環境

已驗證的本機 qualification environment：

```sh
export PATH="/opt/homebrew/opt/python@3.14/libexec/bin:/Users/lychang/.local/share/mise/installs/zig/0.16.0/bin:$PATH"
export ZIG=/Users/lychang/.local/share/mise/installs/zig/0.16.0/bin/zig
export RUSTUP_TOOLCHAIN=1.96.1
export CARGO_BUILD_JOBS=2
export CARGO_PROFILE_DEV_DEBUG=1
export CARGO_PROFILE_TEST_DEBUG=1
just ci
just docs-contract-test
```

路徑僅代表原工作站；接手環境先核對工具版本，不可為了記憶體省略 debug、弱化測試、增加 ignore 或做未核准 SDK/toolchain/cache workaround。

- herdr-core source pin：`9ce385d9c8abb041dd4437476d770365b3fb5282`；原工作站 `/Users/lychang/herdr-core` 保持乾淨，**本次只修改 Herdr**。
- 最近 leon 非互動 PATH 沒有 cargo，但 `$HOME/.cargo/bin` 有 cargo/rustc 1.91.1；不是上述 Herdr qualification toolchain，不能聲稱已完成 fresh Linux 1.96.1 驗證。
- Shared default remote 的非互動 PATH 沒有 literal `hc`；bootstrap 使用 `command -v herdr-core` 或 `$HOME/.local/bin/herdr-core`，不能改成 local fallback。

## 7. 安裝／版本與交接來源

交接前 base：`e39e21d3c37e66d1e374ef593d9b89f92b1e8108`（`fix: close native panes after shell exits`）。

原工作站受保護安裝：

- 已安裝 Native：`~/.local/opt/herdr-native/7fb655bb1ffe/herdr`。
- Native SHA256：`7fb655bb1ffe62bcef0596ae69ac404ecca7790fac9b16d57ec0ec8ba39ba122`。
- `target/release/herdr` SHA256：`d7e05645f3266c13310c81b525e33d468c183465da7523bed647830b5f107c42`；release TEST benchmark 前後保持不變。
- Legacy release、config、shared Bash / ble、自訂 hotkeys、無關 sessions/programs 與現有資料均須保留。

私人發佈目的地：`personal` → `git@github.com:king0980692/herdr-hc.git`，branch `master`。已查驗 acting account `king0980692` 對該私人 repository 有 admin/write 權限，Actions disabled。

**不要 push `origin`（`herdrdev/herdr`），不要 force-push，不要把 checkpoint 當作已部署 release。**

本機詳細調查資料在 `.local/prd/` 與 `/var/tmp/`，它們不會隨此 commit 推送。可攜交接依據是此檔、已提交的 source/tests/docs；若需要原始 logs / screenshots / evidence，向原工作站持有人索取。

最近重要 logs（僅原工作站）：

- bg160：control-only owner identity proof，`/var/tmp/hcshare-iTy9lWOC/`；其 probe 不是新 bootstrap 的安全測試入口。
- bg165：完整 render-scale 的 CoW failure 與支援性 scaling 數據。
- bg174：inventory deadline regression 修復前的紅燈。
- bg175：修復後 targeted test + 三輪完整 Rust 測試與輔助檢查。
- bg176：提交前 `just check`，CI 再次通過後，被缺失的 Windows SDK gate 阻擋。
- bg logs 根目錄：`/var/folders/j1/ypg10_vx54x3zgsxfp0th9gh0000gn/T/pi-bg/`，檔名前綴 `83219-`。

## 8. 這個 checkpoint 沒有做的事情

- 沒有執行新 multiview 草稿、沒有在現有工作 program 上 attach / input / resize / kill。
- 沒有更新或停止共用 core，沒有降版 normal hc。
- 沒有安裝本次 Native candidate，也沒有退役舊 isolated gx10。
- 沒有完成 fresh Windows / Linux / CoW / reboot 驗收。
- 沒有安裝 sidebar/plugin 或修改 shared shell 設定。
- 這是供工程師接力的 **WIP code + handoff**，不是 production-ready 宣告。
