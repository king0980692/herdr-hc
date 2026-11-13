# Herdr Native / shared hc 交接清單

> **狀態：WIP checkpoint，尚未部署或完成實機驗收。**
> 本次交接包含目前的實作與回歸測試，不代表可以直接升級共用 core 或切換使用中的 frontend。

## 1. 目標與範圍

保留原本 Herdr TUI 的版面、鍵盤操作、pane 邊框、焦點與儲存布局，改由 hc / herdr-core 提供真正的 terminal、program 與 agent 狀態。Native 應讀取 **hc 預設共用 machine registry**，而不是複製另一份 SSH 機器清單。

- `herdr`：原本 legacy release，保留不動。
- `herdr-native`：目前已安裝的 hc-backed snapshot；**尚未安裝本次 checkpoint**。
- `herdr-hc`（`--features hc-entry`）：只顯示本機 Local hc core，刻意不支援 `machine`／`--machine`／`--remote`／`--session`。遠端（共用機器清單、gx10／leon presenter）與下面的遠端實機驗收都屬於 `herdr-native`，herdr-hc 的驗證不能拿來勾選它們。
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
- [x] **hc 0.1.17 的 automatic response 會偷走控制尺寸（已實測，2026-11-11）**：`hc_multiview_live_qualification`（ignored，隔離 scratch core）在 0.1.18 上 AUTOMATIC_INPUT 不影響尺寸；在 0.1.17 上 Native 退回 DATA，B（80x24）每回一次 automatic reply 就把 A（100x35）控制的尺寸改成 80x24，6/6 回合、兩次執行皆同。共用 gx10 與 leon 目前都是 0.1.17。這是 frame 層的證據，尚未在真實 Native TUI 的 query/reply 上驗證。決定方向：preserving 升級共用 core 至 ≥0.1.18。
- [x] **preserving 升級已在隔離環境演練通過（2026-11-11）**：同一個測試加上 `HERDR_HC_MULTIVIEW_UPDATE_FROM=<build>`（執行檔必須是帶 `.disposable-hc` 標記的可拋棄複本，否則拒絕）。0.1.17 scratch core 上先起 program，再 `hc update --from`：core 永久 ID、program PID、state、尺寸、created、argv 全部不變；**升級前就存在的 program 升級後也改用 AUTOMATIC_INPUT**，尺寸不再被偷。`hc update` 只改寫自己的執行檔（canonicalize 後的 current_exe）並重啟該 session 的 core。
- [x] **release artifact 演練（2026-11-11）**：目前 release 是 **0.1.20**。0.1.17 → 0.1.20 的 preserving 升級在隔離 scratch core 上兩次通過，結果與 debug build 相同（ID／PID／尺寸不變，既有 program 也改用 AUTOMATIC_INPUT）。
- [x] **Native 已對 hc 0.1.20 合格，pin 前移至 0.1.20（2026-11-11）**：0.1.20 會在 pane 的互動 shell 回到前景時清除狀態（herdr-core #166），所以從 shell prompt 用 `printf` 送出的 OSC 7501 不會保留。`support::hc::report_agent` 與 live smoke 改由佔住前景的 stand-in agent 回報。完整套件、`hc_native_live_smoke`、多 viewer probe 在 0.1.20 與 0.1.18 上結果相同（只剩 sigwinch）。真實 agent 本來就是前景 program，但這點沒有用真實 agent 驗證。
  - `client_mode::federated_client_starts_without_local_and_survives_its_restart` 的間歇失敗**不是 0.1.20 特有**：單獨重跑時 0.1.20 為 48 過／3 敗，0.1.18 為 38 過／1 敗；5 個併發迴圈下 55 次中 4 次失敗，落在三個不同斷言。這是一個對時序與負載敏感的長測試。抓到一個具體機制：來源重連期間，client 的鍵盤輸入被 presenter 丟棄（log：`targeted pane key input failed: no available capacity`）。hc pane 的輸入佇列只有 32 筆（`src/pane/hc.rs` 的 `INPUT_QUEUE`），滿了就 `try_send` 失敗並丟掉該筆，不是整筆拒絕也不是排隊；測試每 100ms 重送整行，部分字元被丟後 shell 的輸入行被弄亂。「第二輪重連後畫面沒恢復」那種失敗沒有抓到診斷，原因仍未知。
- [x] **leon 共用 core 已 preserving 升級至 hc 0.1.20（2026-11-11 約 12:02 UTC，使用者明確同意該次範圍）**：`hc update --from` 演練過的 release artifact（SHA256 `0e4a0c2f…53c95e`），以 core 原有的乾淨環境執行。before/after：core ID `6130a31a…` 不變；core PID 1208149 → 3787724；holder PID 305673 與 instance 不變（現在標為非 current，符合 hc「更新前的 terminal 維持舊 holder」的說明）；`t12` 的 PID 310752、`/proc` 啟動時間、created、argv、keep、state、尺寸 113x33 全部不變；沒有多出 program；新 core 的 systemd 環境檔與舊的相比只少了 `_`（執行腳本刻意排除；`_` 是啟動 shell 自己設的「上一個指令」變數，不影響 terminal）。**更正**：先前寫「環境變數相同」並不精確，leon 同樣少了 `_`。已連線的遠端 viewer 在數秒內自動重連。備份與 before/after facts 在該主機的 `~/.local/state/herdr-core-upgrade-20261111/`（含 0.1.17 binary 與升級前的 state 目錄）。**沒有直接在這個 core 上量測 automatic reply 是否仍偷尺寸**（依據是同一個 binary 的隔離演練）。hc 0.1.20 另提示：core 會在最後一個登入結束時停止，`sudo loginctl enable-linger` 可避免，未處理。
- [x] **升級後的驗證（2026-11-11）**：Linux 候選版以 headless server 連上升級後的 leon 共用 core（PATH wrapper 固定 endpoint、啟動前驗證 owner、假 `ssh` 擋遠端），首次開啟把 `t12` 投影成 tab 並 attach，畫面可讀、`pane.process_info` 回報 Local PID；停止後只 detach。全程 `t12` 的 PID、`/proc` 啟動時間、尺寸、標籤不變，沒有新 program、沒有 SSH 呼叫。沒有對 `t12` 送任何輸入，所以「另一個 viewer 的 automatic reply 不再偷尺寸」沒有在 `t12` 上直接量測（隔離 probe 已用同一個 binary 驗證）。完整套件、live smoke、雙 viewer probe 以安裝的 0.1.20 在隔離 scratch core 上全數通過（只剩 sigwinch）。
- [x] **gx10 共用 core 已 preserving 升級至 hc 0.1.20（2026-11-11，使用者核准，由 gx10 上的操作者照清單執行）**：aarch64 artifact SHA256 `dd67ad491c51160e405b94e307f69094ed030d3b984f4d3236b391a44664cf16`，`hc update --from` 單一指令。core ID `3431734c…` 不變；core PID 213694 → 3201712；holder PID 213955、instance `e0bda880…` 不變（current 變 false，預期）；`t2`（`keep: true`）的 PID 319766、`/proc` 啟動時間、created、argv、keep、state、113x33 全部不變；沒有新 program。已由 leon 端獨立唯讀複核。備份與 before/after 在 gx10 的 `~/.local/state/herdr-core-upgrade-20261111/`。
- [ ] **真正升級共用 core 前仍未驗證的事**：演練用的是 pinned source 的 debug build，不是 release artifact；program 只有 `/bin/cat`，升級當下沒有 viewer attach、沒有 `keep`、只有一個 holder；沒有驗證已 attach 的 Native／legacy viewer 在 core 重啟後的重連。實際升級仍須先存 before/after facts 並取得該次範圍的明確同意。
- [ ] **saved／shared 來源仍有 `agent start` 啟動競態**：`pane.process_info` 只對 Local 來源回報 hc 事實（遠端 PID 不外露），所以 CLI 對遠端來源不會在 shell 初始化期間重試。
- [ ] **`agent prompt --wait` 會漏掉瞬間的 working**：hc 事件串流有發出連續的 working→idle，但 Herdr 的投影只保留最新狀態。`tests/cli/agents.rs` 的假 agent 以 0.2s 停留避開。
- [ ] `client_mode::sigwinch_refreshes_host_palette_without_resizing` 在遷移前的 `ee5bfb50` 於此環境即失敗，與 hc 無關，原因未查。
- [ ] `cli::sessions::named_sessions_use_separate_servers_and_workspace_state` 不穩定：完整併發執行約每數次失敗一次（最近 4 次中 2 次），beta session 多出標籤 `~` 的 workspace。推測（未驗證）：兩個 named session 共用同一個 hc core，first-open bootstrap 依時序把 alpha 的 program 投影進 beta；若成立，named session 之間不再隔離。

### 候選版（此 WSL2 主機，2026-11-11）

- 來源：分支 `fix/linux-hc-process-facts` 的 `747df3d4`，`cargo build --release --locked`，Rust 1.96.1。
- 位置：`~/.local/opt/herdr-native/643df92db8e4/herdr`，SHA256 `643df92db8e4a1a162b38384aa99e474ba89a3121c98b62d43c0a8264d5c614b`。沒有動 `~/.local/bin/herdr`（0.9.5）與 `~/.local/bin/herdr-core`（0.1.17）。
- 同目錄的 `herdr-native-trial` 是隔離試用啟動腳本：獨立的 XDG config/state/runtime（預設 `~/.local/state/herdr-native-trial`）加上私有的 hc 0.1.20（`trial-bin/hc`），不會 attach 共用 core。已用非互動方式驗證：program 建在私有 core，共用 core 與 `t12` 不變。`herdr-native-trial-hc` 對該私有 core 下 hc 指令。
- 尚未有人實際操作過 TUI；連接真正共用 core 的試用需要另外同意（會 attach 到工作中的 program）。

### 候選版 2（此 WSL2 主機，2026-11-13）

- 來源：分支 `fix/linux-hc-process-facts` 的 `774879e3`（不含 `hc-entry`），`cargo build --release --locked`，Rust 1.96.1，另一個 target dir（沒動 `target/release/herdr`）。
- 位置：`~/.local/opt/herdr-native/5ccedd3ca84a/herdr`，SHA256 `5ccedd3ca84ab137ddd6e581320416761f0143065dcd27c391e34735b253f262`，唯讀；`BUILD_INFO` 記錄來源。舊候選版 `643df92db8e4` 保留。試用腳本與私有 hc 0.1.20（SHA256 `0e4a0c2f…53c95e`）同上。
- 非互動 smoke（私有 core，短路徑 root；scratchpad 路徑會超過 107 bytes 的 socket 上限）：建立 workspace、split 正常；`server stop` 只 detach（兩個 program 仍 live）；共用 leon core ID／PID／版本不變。
- [x] **接 leon 真實共用 core 的第一次試用（2026-11-13，使用者同意範圍：共用 core＋複製的 config，只動自己建的 program）**：
  - 方式：候選版用私有 XDG（`/var/tmp/hl-WuXI7r`，含複製的 `config.toml` 與 `scripts/`，不含 plugins），PATH 上的 `hc` wrapper 以原本的 XDG 連到共用 core（`/run/user/1000/herdr-core/default.sock`），假 `ssh` 擋遠端。herdr 0.9.5（pid 1247）沒動。啟動前確認 owner 為 `6130a31a…`、0 個 program、`machine ls` 為空（leon 沒有登錄遠端機器）。
  - 結果（headless server＋CLI，非互動 TUI）：建立 workspace 後 program 建在共用 core（t18），`pane run` 輸出可讀；split 建出 t19；明確關閉 pane 只結束該 program（t19 `killed`）；`server stop` 只 detach（t18 仍 live，PID 824477）；重啟 server 後還原 layout 並重新 attach 同一 PID，畫面保留先前輸出；最後明確關閉 t18 並 `hc rm` 兩筆紀錄。
  - before／after：core ID、PID 1130、版本、live 0、`machine ls` 相同；唯一差異是 core 為第一個 program 啟動的 PTY holder（`herdr-core hold`，pid 824435，systemd 下，0 terminals），這是 core 的正常行為，保留。
  - [x] 互動 TUI（使用者操作，2026-11-13）：邊框與自訂快捷鍵正常。第二個獨立 viewer（`/var/tmp/hl-WuXI7r-b`）首次開啟顯示 t1b，兩邊輸入同步。
  - [x] **開著的 viewer 自動加入其他 viewer 新開的程式**（`5e938dce`，候選版 `~/.local/opt/herdr-native/046d2246d0dc`）：core-wide `events.subscribe`，只 attach、不搶焦點；放進以該資料夾開的 workspace，否則目前的 workspace；herdr-hc 不變。scratch 兩個 viewer 驗證：A 新 tab 在 B 出現一次、焦點與尺寸不變、`hc run --bg` 兩邊都出現、A 斷線期間建立的程式重連後補上一次。leon 實機：A 新增 t1m、t1n，B 自動出現、焦點不變。viewer 關閉期間建立的程式不會加入（只有首次開啟的 discovery 或 saved layout）。
  - [ ] 待決定：被另一個 viewer 關掉（`killed`）的程式，其他 viewer 目前保留 pane 顯示結束（`exit_tests.rs` 規定）；程式自行 `exit` 才自動關 pane。
  - [ ] 尚未：兩個 viewer 不同尺寸下的 resize／自動回覆、重開 viewer、真實 agent、plugin／sidebar、gx10。

### macOS 隔離試用結果（2026-11-11，原工作站，另一個 session 執行並回報）

來源 `ed92d877`；`just ci` 與 `just docs-contract-test` 在 macOS 通過（4073 passed／18 skipped），未改任何 source 或測試。候選 Mach-O arm64 SHA256 `f9fc101ee686cb9c1129ac21d1dafe510b2ec309e7b51890497f2ee01b24f03c`，放在 `/var/tmp/ht-oQct5B/`（非長期儲存）。私有 hc 是 macOS 0.1.19 的複本，啟動前驗證 owner 不等於正常 Local。

已由實際 TUI 操作驗證（對應上方「實際 Native TUI」P0 的部分項目）：

- [x] 分割、四向移動、zoom、resize、開／切 tab、輸入與輸出。
- [x] 邊框標題：raw OSC title 不冒充 border label；手動 rename 才顯示。
- [x] 明確關閉 pane 只終止該 binding；TUI 退出與 presenter 停止只 detach；重開後 layout、public ID、program PID／argv／keep 保留，沒有 replacement shell。

仍未驗證：

- [ ] 使用者原本的 layout 與共用 registry（這次是試用自建的 layout）。
- [ ] 兩個真實 Native viewer 同時連線的 query/reply 與重連。
- [ ] 自訂外部指令（含 `HERDR_ACTIVE_PANE_ID` 腳本）、sidebar/plugin、滑鼠、onboarding、真實 agent。
- [ ] ble prompt 顯示字面的 `\e[1;38;…m>\e[m[INS]`：未定位。待對照：一般終端機用同一組私有 XDG 開 Bash，若仍出現即為隔離環境造成。mise 找不到 zoxide 與 trust 提示確定是私有 XDG 造成。

兩個契約區分（不是缺陷）：

- API `terminal_id` 在 presenter 重啟後會重新分配；身份是 permanent owner core ID + local address 與 public workspace/tab/pane ID。
- **最後一個 viewer 離開後，hc 把 live program 的尺寸改成 headless 預設 200x120**，重新顯示時再依 layout resize。0.1.17、0.1.18、0.1.19（macOS）、0.1.20 行為相同；`hc_multiview_live_qualification` 現在斷言這點。後果：沒有其他 viewer 時關掉 Native，全螢幕程式會經歷兩次重排。

### GitHub Actions 與 herdr-hc release（2026-11-11）

- 私人 repo 已開啟 Actions（以 `king0980692` 操作）。上游 workflow（stale-close、website-deploy、preview、release 等）尚未被 GitHub 登錄所以無法停用；它們只會在 `master` 有新 push 時被登錄，**push `master` 前必須先停用**。
- `herdr-hc-release.yml`：`herdr-hc-*` tag → 完整 CI → 以 `--features hc-entry` build Linux／macOS／Windows → 私人 repo prerelease。
- **Linux CI 通過**。
- **macOS 已修好**：`server_headless` 的三個短測試在 hc 0.1.18～0.1.20 都會失敗。原因是 server 在一秒內被關掉時，正在經由 launchd 啟動 core 的 `hc` 也被結束，狀態傳到一半（core.log：`launchd detach failed: EOF while parsing`），core 從沒啟動。fixture 現在把沒有 `core.json` 的 session 視為從未啟動。以 hc 0.1.20 在 macOS runner 跑完整檢查：4053 passed／18 skipped，lint 與輔助檢查通過。hc 上游在這種中斷後會留下未清除的 launchd job 與 `/tmp/hc-launchd-*`。
- [x] **第一個 herdr-hc release（2026-11-12）**：tag `herdr-hc-20261111-6b346218`，Linux 與 macOS 測試通過後 build 五個平台並發布為 prerelease：https://github.com/king0980692/herdr-hc/releases/tag/herdr-hc-20261111-6b346218 。過程中修正：Windows 編譯錯誤、macOS fixture、`named_sessions` 測試共用 hc core 的時序競態、Linux build 刪掉 runner 預設 apt 鏡像後解析不到 ubuntu.com。release 門檻刻意不含 Windows。
- [x] **issue #3（2026-11-12）**：herdr-hc 建立的 program 帶 herdr.app 的 placement 標籤（`herdr.space`、`herdr.tab`、`herdr.seen=0`），分割沿用所在 tab 的 placement、新 tab 用同 space 的新 tab id、新 workspace 用新 space。依 herdr-app `defaults/AGENTS.md`，herdr.app 只顯示帶 `herdr.space` 的 terminal，收編時會設 `keep: true`（使用者同意此例外）。program 自行結束時改記 info。重新 attach 改要求 2000 行歷史。issue 的第 2、3、5b、5c 項未修改。
- [x] **issue #3 第二輪（2026-11-12）**：
  - herdr-hc 的分割會寫進 herdr.app 共用文件 `herdr-app`（`doc.get`／`doc.set`，版本衝突重試）：新 program 放在來源旁、同方向同比例；兩秒後再檢查一次，若 herdr.app 已把它收編成新欄就移回。規則照 herdr-app `layout.rs`（tab root 一律是 id 等於 tab id 的容器，不收合根）。
  - 被 keep 的 program 以 exit code 結束也算結束，herdr-hc 關掉該 pane（只有遺失的 program 會被 hc 換成 shell）。
  - 首次開啟（沒有既有 session）時依該文件建立 workspace／tab／分割樹，保留名稱與比例；文件沒顯示的 program 放進其 `herdr.space` 對應的 workspace 的新 tab，否則放到另一個 workspace；讀不到文件時行為不變。stack 只取顯示中的子節點。
  - 只在 scratch core 驗證（模擬 herdr.app 的晚收編）；真正的 herdr.app 對這些寫入的反應要在 Mac 上確認。已有 herdr-hc session 的使用者不會套用首次開啟的版面。
  - release `herdr-hc-20261112-2b6b6517`（Linux／macOS 測試通過）。Mac 實機（2026-11-12）：沒有 session 時照 herdr.app 版面重建、先往下再往右 split（herdr.app 顯示相同，使用者親自確認）、herdr.app 收編過的 pane 打 `exit` 後 herdr-hc 關掉 pane，三項通過（issue #3 comment 6830236077）。
  - [x] **Test 2 通過**（issue #3 comment 6831836820）：herdr.app ⌘Q 後在 herdr-hc split，重開排列相同。Mac 四項全過。
  - [x] herdr.app 關閉期間 split 出的程式 keep 一直是 false：herdr.app 只對不在版面的 terminal 收編並設 keep，而 herdr-hc 已先把它寫進版面。使用者決定由 herdr-hc 在寫入新程式後自己設 `keep: true`。release `herdr-hc-20261113-b9761aff`，Mac 實機驗證通過（issue #3 comment 6833207642）：herdr.app 關閉時 split 出的 t1n 為 keep:true、重開後排列一致；舊版建立的 t1m 仍為 false（預期，不回溯）。issue #3 尚未關閉。
  - Mac 上的變動：舊 `~/.config/herdr-hc/session.json` 改名為 `session.json.bak-949d`；`~/.local/bin/herdr-hc` 指向新版；新的 herdr-hc 跑在 herdr-hc-test3 session。
- [ ] `client_mode::federated_client_starts_without_local_and_survives_its_restart` 在此機器上約三成偶發失敗（重新 attach 歷史 0 行 8/12、2000 行 7/12 通過，無差別），未查。
- [x] **leon 在 2026-11-12 重開機，`t12`（keep:false）因 core 停止而結束（`stopped_by_restart`）**。使用者已於 2026-11-12 執行 `sudo loginctl enable-linger lychang`（`Linger=yes`），core 不再隨最後一個登入結束而停止。
- [ ] **Windows CI 仍失敗**：dead code 已修，但 Native 建立 pane 需要 hc，而 Windows runner 沒有 build hc；ConPTY smoke test 與需要 hc 的整合測試都會失敗。這從 hc 遷移起就存在。

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
| 共用 gx10 | `3431734c422fe905bd487f2f495bdb4f` | 0.1.20（2026-11-11 升級） |
| 共用 leon | `6130a31a167522145c275e2185e6d395` | 0.1.20（2026-11-11 升級；2026-11-12 重開機後 `t12` 已結束） |
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

- herdr-core source pin：`de97ea5ba5d6877a5e082316b4a1a5b4918e861f`（hc 0.1.20，2026-11-11 由 0.1.18 的 `9ce385d9…` 前移）；原工作站 `/Users/lychang/herdr-core` 保持乾淨，**本次只修改 Herdr**。
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
