# Honkai: Star Rail support / 《崩坏：星穹铁道》支持

`hsr_scanner` is the Star Rail library linked into the normal `GGScannerOCR`
and `GGScanner` Windows applications. It is not a separate product. The
shared applications neither require nor launch an HSR-specific executable;
they keep their existing binary names and let the player switch between
Genshin Impact and Star Rail.

`hsr_scanner` 是正式 `GGScannerOCR` 与 `GGScanner` Windows 程序共用的星穹铁道功能库，
不是独立产品。两个共用程序均不要求或启动额外的 HSR 专用程序；程序名称保持不变，玩家可在应用内
切换《原神》和《崩坏：星穹铁道》。

## Capabilities / 功能

- Screenshot scanning for Characters, Light Cones, Cavern Relics, and Planar
  Ornaments, with explicit category coverage.
- Attended preview/apply management for the reversible lock and discard-mark
  states. Equipped or uncertain gear remains protected, and destructive game
  actions are outside this boundary.
- In-process read-only capture of completed achievements in `GGScanner`.
  This uses Windows `pktmon` on HSR UDP ports `23301` and `23302`; no helper
  download is required.
- Versioned [HSR-Scanner v4](../../docs/HSR_EXPORT.md) scanner and capture export for Fribbels
  and Reliquary clients, with optional achievement IDs and Trailblazer gender/path.

- 通过截图扫描角色、光锥、隧洞遗器和位面饰品，并分别记录各分类的覆盖状态。
- 有人值守的预览/应用管理流程，只处理可逆的锁定与弃置标记。已装备或状态不确定的装备始终受保护，
  分解、删除、消耗、装备等破坏性操作不在功能范围内。
- `GGScanner` 可在进程内只读抓取已完成成就。程序通过 Windows `pktmon` 监听 HSR UDP
  端口 `23301` 和 `23302`，无需额外下载辅助程序。
- 扫描与抓包都导出 [HSR-Scanner v4](../../docs/HSR_EXPORT.md)，供 Fribbels / Reliquary 客户端使用，
  并可附带成就 ID 与开拓者性别/当前命途。

Scanner, Manager, and achievement-capture flows use the complete,
SHA-256-verified GIlore 1.2.0 public reference snapshot embedded in both shared
binaries. No reference-data folder is required. The GUI reference-folder field
is an advanced explicit override; when nonempty, the selected bundle must pass
the same strict completeness checks and an invalid override fails closed.

扫描、管理及成就抓包流程均默认使用内置在两个共用程序中的完整 GIlore 1.2.0 公开参考快照，
并会校验其 SHA-256；无需准备参考数据目录。界面中的参考目录是高级自定义覆盖；一旦填写，所选数据包
必须通过同样严格的完整性校验，无效覆盖会安全拒绝。

## Achievement capture / 成就抓包

Start capture before HSR establishes its session, then enter the game and open
the Achievements screen if the application asks for it. Stop is cancelable
while the packet source is idle. A capture becomes complete only after a
structurally valid full achievement response is recognized against the current
complete GIlore public-ID set.

请在 HSR 建立会话前开始抓包，再进入游戏；若应用提示，请打开“成就”界面。即使暂时没有收到数据包，
“停止”也会立即取消任务。只有当程序依据当前完整的 GIlore 公共成就 ID 集合，确认收到结构有效的完整
成就响应后，覆盖状态才会变为完整。

Only public achievement definition IDs with completed protocol states are
normalized. IDs are sorted and deduplicated. The export retains no account
identifier, packet bytes, command IDs, raw protocol status, or completion
timestamp. A recognized response with no completed achievements is represented
as a present complete snapshot with an empty `entries` array; no recognized
response is represented by omitting `achievements`.

程序只规范化公共成就定义 ID 及“已完成”状态，并对 ID 排序去重。导出不会保留账号标识、数据包字节、
命令 ID、原始协议状态或完成时间。若已确认完整响应但没有已完成成就，`entries` 会是空数组；若尚未
确认成就响应，则完全省略 `achievements`。

The source revision written by this boundary is `auto-reliquary-1.2.0`. The
dependency itself is pinned by commit in `Cargo.toml`; the decoder does not use
auto-reliquary's sentinel-based achievement matcher. Field layout is inferred
from the complete public-ID set and conservative structural checks.

此边界写入的来源修订为 `auto-reliquary-1.2.0`。依赖已在 `Cargo.toml` 固定到具体提交；解码器不会
使用 auto-reliquary 依赖单一哨兵 ID 的成就匹配器，而是根据完整公共 ID 集合和保守的结构检查推断字段。

## Build and deterministic verification / 构建与确定性验证

Run from the GGScanner repository root with Rust 1.89 or newer:

请在 GGScanner 仓库根目录使用 Rust 1.89 或更高版本运行：

```powershell
cargo fmt --all -- --check
cargo test --locked -p hsr_scanner --all-features
cargo clippy --locked -p hsr_scanner --all-targets --all-features -- -D warnings
cargo build --locked --release -p good_tools_app --bin GGScannerOCR
cargo build --locked --release -p good_tools_app --bin GGScanner --features capture
```

The achievement fixtures are synthetic decrypted protobuf-command payloads.
They cover field-number rotation, completed-status normalization, unknown public
IDs, sorting/deduplication, truncation/noise, absence, and a recognized empty
snapshot. They contain no PCAP/ETL data and no real account or session data.
Passing them proves replay/normalization behavior only; it does **not** prove a
live HSR capture.

成就夹具是人工合成的已解密 protobuf 命令载荷，覆盖字段编号变化、已完成状态规范化、未知公共 ID、
排序去重、截断/噪声、未识别响应及“已识别但结果为空”等情况。夹具不包含 PCAP/ETL，也不包含真实
账号或会话数据。测试通过只证明回放和规范化逻辑，**不能视为 HSR 实机抓包验证**。

Live screenshot scanning and manager application are attended Windows
operations. Keep the chosen game window in the foreground; focus loss, window
geometry drift, the GUI Stop action, or right-click emergency abort prevents
further input. Automated replay tests likewise do not prove live screenshot
recognition or live game mutation.

Character screenshot scans click the nine visible top-bar portraits, drag by
nine portrait gaps, and resume at the first click position. The scanner resets
the bar to its beginning first and deduplicates overlapping entries on the
clamped final page. No virtual-controller driver is needed. Keep the mouse
and keyboard idle during scanning. Completion is detected when the last
visible portrait has been visited and two forward drags leave the bar at its
end. No entered Character total is required; repeated identities alone do not
prove completion.

角色截图扫描会依次点击顶部九个头像，再拖动九个头像间距，从第一处点击位置继续。
扫描前会先将角色栏移回开头，最后一页重叠的角色会自动去重，无需安装虚拟手柄驱动。
扫描期间请勿操作鼠标或键盘。扫描完最后一个可见头像后，连续两次向前拖动仍停留在末端，
即可确认角色扫描完整，无需填写角色总数。仅重复识别到同一角色不能证明扫描完整。

The HSR scanner's **Timing Delays** section exposes total waits shared across
navigation, detail panels, Character paging, screenshot comparisons and manager
status toggles. These values have no hidden per-path additions. Older saved
navigation delays migrate to equivalent total waits.

Capture defaults to **Automatic**: BitBlt for SDR and WGC with FP16 capture and
tone mapping when **HDR mode** is enabled beside the scan targets. Existing saved
backend choices remain overrides. Select Automatic to restore the recommended
behavior. Changing HDR mode also restores Automatic capture. Retired PrintWindow
selections migrate to Automatic.

**Advanced Options** contains the capture override, image dumps and separate scan
caps for Characters, Light Cones, and Relics/Ornaments. `0` scans the whole
category; stopping early produces unknown coverage. Inventory caps sample the
backpack instead of rejecting a larger total. Manager scans always read the
whole inventory. OCR uses automatic per-field routing, including v6 tiny for
evaluated fields; model and pool overrides are not shown.

星穹铁道扫描器的**延迟设置**提供界面操作、详情面板、角色栏翻页、截图对比和管理器标记切换
共用的总等待时间，不再叠加各路径内隐藏的延迟。旧版保存的操作延迟会迁移为等效的总等待时间。

截图方式默认为**自动**：普通模式使用 BitBlt；在扫描目标旁勾选 **HDR** 后，使用 WGC
以 FP16 截图并进行色调映射。旧版保存的截图方式保留为手动覆盖，选择“自动”可恢复推荐设置。
切换 HDR 选项也会恢复自动截图。旧版 PrintWindow 设置会迁移为自动截图。

**高级选项**包含截图方式覆盖、OCR 截图保存，以及角色、光锥、遗器与饰品各自的最大扫描数。
`0` 表示扫描全部；提前达到上限时，覆盖率标记为未知。背包上限用于抽样，不会因为背包总数
更大就拒绝扫描。管理器始终扫描全部库存。OCR 按字段自动选择模型，已验证的字段使用 v6 tiny；
暂不提供模型或识别池的手动设置。

Both shipped executables expose the same scan path as the GUI:

```powershell
.\GGScanner.exe star-rail check --capture-method wgc --log-file wgc-check.log
.\GGScanner.exe star-rail check --capture-method auto --hdr-mode --save-frame hdr.png
.\GGScanner.exe star-rail check --capture-method bitblt --drag-character-bar --save-frame drag.png --log-file drag.log
.\GGScanner.exe star-rail scan --characters --max-characters 2 --dump-images --log-file characters.log
.\GGScanner.exe star-rail scan --light-cones --relics --sample-items 3 --capture-method bitblt --dump-images --log-file inventory.log
```

`scan` loads saved GUI settings beside the executable. Explicit target flags
replace the saved targets for that run; overrides never rewrite the settings.
Sampling keeps inventory coverage incomplete. `check` focuses the game and
captures its client without navigating menus. `--drag-character-bar` tests the
same page-drag function used by the scan; open Character Details first. With
`--save-frame`, it saves both the before and after screenshots.
Commands exit with a nonzero status on failure and retain full errors in
`--log-file`; they do not pause for a keypress. Use these shipped binaries for
live verification instead of claiming success from the example programs.

实机截图扫描和管理器应用属于有人值守的 Windows 操作。请保持选定游戏窗口位于前台；焦点丢失、
窗口位置/尺寸变化、点击界面“停止”或鼠标右键紧急中止，都会阻止后续输入。自动回放测试同样不能证明
实机截图识别或实机游戏状态变更。

Dependency licenses and the embedded protocol-key provenance are recorded in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

依赖许可证与内置协议密钥来源详见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
