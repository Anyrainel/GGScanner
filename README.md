<div align="center">

# GGScanner

**中文 | [English](README_EN.md)**

基于 [yas](https://github.com/1803233552/yas) 编写的《原神》与《崩坏：星穹铁道》Windows 扫描、抓包与管理工具

同一套 `GGScannerOCR.exe` 与 `GGScanner.exe` 同时服务两款游戏。《原神》数据继续导出为兼容现有工具的 [GOOD v3](https://frzyc.github.io/genshin-optimizer/#/doc)；《星穹铁道》扫描与抓包导出为 [HSR-Scanner v4](docs/HSR_EXPORT.md)（Fribbels / Reliquary 互通格式，并扩展成就与开拓者性别/命途）。

[![Build](https://github.com/Anyrainel/GGScanner/actions/workflows/rust.yml/badge.svg)](https://github.com/Anyrainel/GGScanner/actions)

</div>

## 功能

- **游戏切换**：窗口顶部以等宽的“原神 / 星穹铁道”选项切换流程，并分别保留两款游戏的设置和当前页面
- **原神扫描与抓包**：角色、武器、圣遗物及现有 GOOD v3 导出能力保持兼容
- **原神成就抓包**：GGScanner 默认导出账号已完成的成就 ID
- **星穹铁道扫描**：角色、光锥、隧洞遗器和位面饰品
- **星穹铁道导出与管理**：扫描与抓包都导出 [HSR-Scanner v4](docs/HSR_EXPORT.md)；遗器管理指令仍使用 `goodscanner.hsr.manager-instructions`
- **星穹铁道成就抓包**：GGScanner 在程序内捕获并导出已完成的成就，无需另行下载 HSR 程序或抓包辅助程序
- **双引擎 OCR**：PPOCRv4（通用）+ PPOCRv5（特殊部分专用），自动选择最优结果
- **副词条验证**：Roll Solver 基于游戏精确度验证词条合法性

## 快速开始

### 下载

发布页提供两个可执行文件；它们都可在窗口顶部切换《原神》和《星穹铁道》：

- **`GGScanner.exe`（推荐）** — 两款游戏的抓包、OCR 扫描、导出与管理
- `GGScannerOCR.exe` — OCR 扫描、导出与管理，不包含抓包代码及依赖

旧的 `GOODCapture.exe` 与 `GOODScanner.exe` 下载名称会在新名称首次发布后保留一个月，供已安装的旧版程序自动更新。过渡期结束后，无法自动更新的用户可从网站重新下载。详见[更名发布流程](docs/RENAME_RELEASE.md)。

请从 [Releases](https://github.com/Anyrainel/GGScanner/releases) 页面下载。**无需也不应寻找单独的 HSR 可执行文件。**

### 成就导出语义

《原神》继续使用向后兼容的 GOOD v3 扩展字段保存成就：

```json
{"achievements":[80001,80002,81001]}
```

该字段是紧凑的已完成成就 ID 数组。字段存在时（包括空数组）表示导入方应替换成就状态；用户关闭“成就”导出选项时字段会省略，导入方应保留原有成就状态。标准 GOOD v3 字段和版本号保持不变。

《星穹铁道》扫描与抓包都导出 [HSR-Scanner v4](docs/HSR_EXPORT.md)：可选的 `achievements` 是紧凑的已完成成就 ID 数组。字段省略表示本次没有观测成就；字段存在（包括 `[]`）表示完整替换。遗器管理指令仍使用 `goodscanner.hsr.manager-instructions`。

### 使用步骤

1. 以**管理员身份**运行所需的可执行文件；不带命令行参数时会打开图形界面
2. 在窗口顶部选择“原神”或“星穹铁道”，再进入当前游戏的扫描、管理或抓包页面
3. 首次运行《原神》时填写旅行者、流浪者等自定义角色名；现有配置仍保存在 `data/good_config.json`
4. 《星穹铁道》设置单独保存在 `data/good_app_config.json`；共用程序已内置并校验 GIlore 参考数据，参考目录仅用于高级自定义覆盖
5. 确保所选游戏正在运行；扫描过程中可按**鼠标右键**终止
6. 《原神》结果仍输出为 `GOODv3.json`；《星穹铁道》输出目录可在其页面中设置

程序内置完整的公开 GIlore `1.2.0` 参考快照，用于校验《星穹铁道》的库存与成就 ID。若明确选择自定义参考目录，它必须是完整且校验和正确的 GIlore 数据包；无效覆盖会安全拒绝，不会静默回退。正常使用无需另行下载参考数据。

### 扫描目标

以下命令行参数继续用于《原神》，其行为和 GOOD v3 输出保持不变。默认扫描全部（角色 + 武器 + 圣遗物），也可以指定：

```shell
GGScannerOCR.exe                    # 扫描全部
GGScannerOCR.exe --characters       # 仅扫描角色
GGScannerOCR.exe --weapons          # 仅扫描武器
GGScannerOCR.exe --artifacts        # 仅扫描圣遗物
GGScannerOCR.exe --characters --weapons  # 组合扫描
```

## Linux 支持（实验性）

Linux 原生程序 `GGScannerOCR` 使用当前共用界面，提供原神 OCR 扫描与管理，以及实验性星穹铁道 OCR 扫描与遗器管理。游戏须通过 Wine/Proton 使用 X11/XWayland 窗口，并关闭 HDR。原生 Wayland 游戏窗口、抓包和自动更新暂不支持。

Ubuntu 构建依赖、XTEST/uinput 输入权限、字体、运行库安装和验证范围见 [Linux 指南](docs/LINUX.md)。无需安装 ydotool。

```shell
cargo build --locked --release --bin GGScannerOCR
./target/release/GGScannerOCR         # 图形界面
./target/release/GGScannerOCR --all   # 原神命令行扫描
./target/release/GGScannerOCR star-rail --help
```

Linux 构建仅上传为 CI 测试产物，不改变正式发布页的 Windows 下载项。当前已验证 Linux 编译、自动测试与隔离图形会话；游戏实机扫描和不同 Wayland 合成器仍需验证。

## 注意事项

- Windows 需要**管理员权限**（用于模拟键鼠输入）；Linux 无需
- 《原神》扫描仅支持**简体中文**客户端；《星穹铁道》识别中英文界面文字
- 推荐 **16:9** 分辨率（1920×1080、2560×1440 等）
- 扫描过程中请勿操作鼠标
- 默认 4 星以下圣遗物不扫描（可通过 `--artifact-min-rarity` 调整）

## 命令行参数

### 通用选项

| 参数 | 说明 |
|------|------|
| `-v, --verbose` | 显示详细扫描信息 |
| `--stop-on-failure` | 识别失败时停止（默认跳过失败条目并记录原因） |
| `--log-progress` | 逐项显示扫描进度 |
| `--output-dir <DIR>` | 输出目录（默认当前目录） |
| `--ocr-backend <NAME>` | 覆盖所有类别的辅助 OCR 后端（角色名/等级校验、武器装备回退、圣遗物等级） |
| `--dump-images` | 保存 OCR 区域截图到 `debug_images/` |

### 扫描器配置

| 参数 | 说明 |
|------|------|
| `--weapon-min-rarity <N>` | 最低武器稀有度（默认 3） |
| `--artifact-min-rarity <N>` | 最低圣遗物稀有度（默认 4） |
| `--char-max-count <N>` | 最大角色数（0 = 不限） |
| `--weapon-max-count <N>` | 最大武器数（0 = 不限） |
| `--artifact-max-count <N>` | 最大圣遗物数（0 = 不限） |
| `--weapon-skip-delay` | 跳过武器面板等待（更快但锁定检测可能不准） |
| `--artifact-skip-delay` | 跳过圣遗物面板等待（更快但锁定/星标检测可能不准） |
| `--char-ocr <NAME>` | 角色 OCR 后端（默认 ppocrv4） |
| `--weapon-ocr <NAME>` | 武器 OCR 后端（默认 ppocrv6tiny） |
| `--artifact-substat-ocr <NAME>` | 圣遗物副词条/通用 OCR 后端（默认 ppocrv6tiny） |

### 配置文件

时序参数和角色名通过 `data/good_config.json` 配置，无需命令行参数：

```json
{
  "traveler_name": "",
  "wanderer_name": "",
  "manekin_name": "",
  "manekina_name": "",
  "char_tab_delay": 400,
  "char_open_delay": 1200,
  "weapon_grid_delay": 60,
  "weapon_scroll_delay": 200,
  "artifact_grid_delay": 60,
  "artifact_scroll_delay": 200
}
```

## 从源码构建

```shell
# rust-toolchain.toml 为本仓库固定 Rust 1.95.0
rustup toolchain install 1.95.0 --profile minimal --component rustfmt

# 确保安装 Git LFS
git lfs pull

# 构建两款游戏共用的普通扫描版
cargo build --locked --release -p good_tools_app --bin GGScannerOCR

# 构建包含两款游戏成就抓包的抓包版
cargo build --locked --release -p good_tools_app --features capture --bin GGScanner

# 编译产物路径：
# target/release/GGScannerOCR.exe
# target/release/GGScanner.exe
```

本地 release 构建关闭 LTO 并启用增量编译，以加快重复构建。GitHub Actions 为 Windows 和 Linux 构建产物启用完整 LTO，并关闭增量编译。两种环境使用相同的构建命令和产物路径。

本地构建默认使用构建当天 UTC 日期的前一天作为版本号（例如 10 月 9 日构建显示 `v2026.10.08`），并正常检查正式版更新。CI 发布时仍使用当天日期和发布构建编号。

夹具与回放测试只能证明解析、导出及模拟交互契约，不能证明当前《星穹铁道》客户端上的实时扫描、抓包或管理操作已经通过实机验证。

## 致谢

- [wormtql/yas](https://github.com/wormtql/yas) — 原始项目，提供核心 OCR 扫描框架
- [1803233552/yas](https://github.com/1803233552/yas) — fork 版本，本项目基于此分支开发
- [Andrewthe13th/Inventory_Kamera](https://github.com/Andrewthe13th/Inventory_Kamera) — GOOD 格式扫描器的参考实现

## 反馈

- [GitHub Issues](https://github.com/Anyrainel/GGScanner/issues)
