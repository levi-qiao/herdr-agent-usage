# Agent 安装配置话术

给**正在这台电脑上跑 Herdr** 的编程助手看的操作说明。把本文件，或 README
里的提示词，贴给 Claude、Cursor、Grok、Codex 等即可。

---

你要在这台电脑上把 **herdr-agent-usage** 装到真正能用：Herdr 的 Agent 侧栏
要显示出品牌图标、模型、上下文，以及这台机器上实际存在的那些 agent CLI 的
订阅额度。

**完成标准是侧栏可用**，不是 `./install.sh` 返回 0。图标变成方框或 `?`、没有
额度、Herdr integration 仍是 `not installed`，都算没装完。

若当前工作区已经是本仓库，直接用。否则：

```sh
git clone https://github.com/levi-qiao/herdr-agent-usage.git
cd herdr-agent-usage
```

从本文件开头按步骤做，不要自行缩短流程。

## 硬性规则

- **不要**在 `./install.sh` 结束后停手。读完它的输出，继续做后面的收尾。
- **不要**用 `herdr pane read`（尤其 `--source recent` /
  `recent-unwrapped`）去「验收」侧栏。那会重建 scrollback，把正在跑的 agent
  TUI 重绘一遍。请用插件日志、`herdr integration status`、字体文件、终端
  配置来验证，图标好不好看则问用户。
- **不要**调用名为 `agent` 的裸命令。机器上如果同时装着 Grok，那个名字是
  Grok 的。Cursor CLI 是 `cursor` 或 `cursor-agent`。
- **不要**给 Cursor 装 `statusLine`。那会换掉官方底栏。Cursor 的 cache/cx
  来自 `hooks.json`，`configure` 已经会合并进去。
- Herdr 的插件 **action 看不到你 export 的环境变量**。选择项只能通过
  `./install.sh` 的 flag 传进去。
- 使用 `rust-toolchain.toml` 里的工具链（走 rustup）。不要 `brew install
  rust` 或用发行版 Rust 顶替。
- 只有人必须动手时才问用户：钥匙串点 **Always Allow**、重载终端、重启正在
  跑的 agent 窗格、看一眼图标对不对。

## 1. 前置条件

探测前先把常见 bin 目录放进 `PATH`（`~/.local/bin`、`~/.cargo/bin`、
`/opt/homebrew/bin`、`/usr/local/bin`）。

| 需要 | 怎么查 | 没有时 |
| --- | --- | --- |
| Herdr **0.9.0+** | `herdr --version` | 停下来，让用户先装 Herdr。 |
| rustup + Cargo | `command -v rustup cargo` | 从 https://rustup.rs 装 rustup，不要用 Homebrew/发行版的 `rust` 包。随后 `cargo build` 会按 `rust-toolchain.toml` 拉取钉死的版本（当前 1.95.0）。 |
| macOS 或 Linux | `uname -s` | 其他平台不支持。 |

`herdr plugin link` 需要 Herdr 客户端可用。`herdr` 本身报错就先修这个。

## 2. 探测要启用哪些 agent

取三者的**并集**：（a）`PATH` 上的二进制，（b）常见配置/数据目录，（c）
`herdr agent list` JSON 里的 kind。写入 `--agent` 的必须是下表里的名字，
不要把别名直接塞进去。

| `--agent` 名 | 二进制 | 额外证据 |
| --- | --- | --- |
| `claude` | `claude` | `~/.claude` |
| `codex` | `codex` | `~/.codex` |
| `grok` | `grok` | `~/.grok` |
| `agy` | `agy` | `~/.gemini/antigravity-cli` |
| `opencode` | `opencode` | `~/.config/opencode` |
| `pi` | `pi` | `~/.pi/agent` |
| `omp` | `omp` | `~/.omp` |
| `devin` | `devin` | `~/.local/share/devin` |
| `muse` | `muse`、`muse-code` | `~/.config/muse` |
| `cursor` | `cursor`、`cursor-agent` | `~/.cursor` |
| `kilo` | `kilo` | `~/.local/share/kilo` |

当前支持列表（只许追加，不许重排）：
`claude,codex,grok,agy,opencode,pi,omp,devin,muse,cursor,kilo`。

一个都没探测到：安装 **all**，并明确告诉用户。优先只装探测到的子集——
`configure` 会给选中的 Claude/Agy 写 statusLine、给 Cursor 写 hooks，不要
去动用户没有的 CLI。

安装前先打印探测结果。

## 3. 编译、链接、写入配置

在仓库根目录。若这是已有的 `main` 检出，先 `git pull --ff-only`（分叉了就
停下，不要强推或重置用户的改动）。

```sh
./install.sh --agent claude,codex,grok   # 换成探测到的名字，逗号分隔
```

只有在你确实要装全部 collector 时才省略 `--agent`。

`install.sh` 会编译 release、`herdr plugin link --enabled`、把偏好写进插件
配置目录，然后**等到 configure action 结束**。sidebar 行、图标字体、
statusLine/hooks，以及（选了 omp 且缺失时）Herdr 的 omp integration，都是
这一步做的。

**读完脚本的全部输出。** `font:`、缺失 integration、跳过 omp 这些行是还没
做完的工作，不是可以忽略的提示。

已经装过再跑同一条命令就是升级/修复。不要删缓存，也不要去杀 watcher。

## 4. 脚本做不到的收尾

### 确认插件已启用

```sh
herdr plugin list
herdr plugin log list --plugin herdr-agent-usage --limit 20
```

必须能看到已启用的 `herdr-agent-usage`。configure/startup 日志失败就是
阻塞项：读日志，修好后再跑 `./install.sh`，或：

```sh
herdr plugin action invoke configure --plugin herdr-agent-usage
```

等到 `herdr plugin log list` 显示 **succeeded**。`invoke` 立刻返回时状态
仍是 `running`，那不算出成功。

### Herdr integration

多数 agent 的额度归属依赖 Herdr 的 session integration。Agy 和 Muse 不需要。

```sh
herdr integration status
```

对每个**已探测到**、且状态为 `not installed` 的 id 执行
`herdr integration install <id>`：

`claude`、`codex`、`grok`、`opencode`、`pi`、`omp`、`devin`、`cursor`、`kilo`。

用户没有的不要装。选了 omp 时 `configure` 会尝试自动装；机器上没有 omp
应跳过，而不是让整次配置失败。

刚装上的 integration：告诉用户**重启已经在跑的对应窗格**。

### 图标字体（方框、空白、问号）

`configure` 会把 **Herdr Agent Icons Max** 拷到 `~/Library/Fonts`（macOS）
或 `~/.local/share/fonts`（Linux）；若 Ghostty / kitty 的配置文件**已经
存在**，再写入带标记的 `U+E1A0–U+E1B6` / `U+E1C0–U+E1C5` 映射。它**不会**
从零创建终端配置，也不会给 WezTerm、iTerm、Alacritty、Terminal.app、Warp、
VS Code / Cursor 集成终端写映射。

PUA 字符不会像普通缺字那样自动回退。没有显式映射时，格子就是方框、空白
或 `?`。

1. 确认字体文件在（字体目录里的 `HerdrAgentIconsMax-*.ttf`）。
2. Linux 上执行 `fc-cache -f "${XDG_DATA_HOME:-$HOME/.local/share}/fonts"`。
3. 判断**当前**终端（`TERM_PROGRAM`、`KITTY_WINDOW_ID`、`WEZTERM_EXECUTABLE`、`TERM`）。
4. 若已经写入 Ghostty/kitty 映射，让用户重载配置（Ghostty：`cmd+shift+,`；
   kitty：`ctrl+shift+f5`）或重开终端。
5. 若当前终端有配置文件但没有映射，补上（Ghostty/kitty 请保留插件的标记
   块，卸载时才能删掉）：

Ghostty：

```
# BEGIN herdr-agent-usage font
font-codepoint-map = U+E1A0-U+E1B6="Herdr Agent Icons Max"
font-codepoint-map = U+E1C0-U+E1C5="Herdr Agent Icons Max"
# END herdr-agent-usage font
```

常见路径：`~/Library/Application Support/com.mitchellh.ghostty/config`、
`~/.config/ghostty/config`。

kitty（`~/.config/kitty/kitty.conf`）：

```
# BEGIN herdr-agent-usage font
symbol_map U+E1A0-U+E1B6 Herdr Agent Icons Max
symbol_map U+E1C0-U+E1C5 Herdr Agent Icons Max
# END herdr-agent-usage font
```

WezTerm：把 `{ family = "Herdr Agent Icons Max" }` 加进已有的
`font_with_fallback`，不要换掉用户的主字体。

VS Code / Cursor 集成终端：在 `terminal.integrated.fontFamily` 末尾加上
`'Herdr Agent Icons Max'`。

iTerm2：如果用户还需要 Nerd Font 符号，可用
[Hack Herdr Nerd Font](font-iterm2.md#简体中文)。从 release 附件安装到本地
Mac，在 Profiles → Text 里选它，并关闭独立 Non-ASCII 字体；SSH 会话同样
使用本地 Mac 的字体。

Terminal.app、Alacritty、Warp：本文没有经验证的按码位映射方案，应如实说明，
不要声称已经修复。Ghostty 或 kitty 仍是可靠的字体映射路径。Muse 故意用文本
标记 `◈`（仅图标字体里没有它的 glyph）。

6. 宽侧栏里，同一厂商多出来的嵌套子行**本来就没有**品牌图标，只有表头那
   一行有。这是设计，不是漏装。
7. 1.6.1 之后品牌格仍是**黄色 `?`**，多半是当前终端没有把
   U+E1A0–U+E1B6 交给 Herdr Agent Icons Max。1.6.1 之前工作态用了 ZWNJ，
   图标字体就会画出 `?` —— 先升级再重载终端。

### macOS 钥匙串（Cursor 和 Muse）

后台 hook 不会弹出授权框。没有一次性的 **Always Allow** 时，Cursor CLI
登录（钥匙串 `cursor-access-token` / `cursor-user`）和 Muse
`storage: "keychain"` 登录会表现为「没有额度」。

**Cursor**：`~/.cursor/cli-config.json` 里有 `authInfo`，且还没有
`~/.cursor/.herdr-keychain-approved`：

```sh
./target/release/herdr-agent-usage refresh --provider cursor --keychain-approve --force
```

**Muse**：登录走钥匙串，且 Muse 配置目录旁还没有批准标记：

```sh
./target/release/herdr-agent-usage refresh --provider muse --keychain-approve --force
```

**先告诉用户**再跑：系统会弹窗，必须点 **Always Allow**，不要点 Allow。
点了 Allow 就再跑一次。

只要 CLI 自己还有 `authInfo`，不要改用 Cursor 桌面端 `state.vscdb` 里的
token。

### 拉一次额度

```sh
herdr plugin action invoke refresh --plugin herdr-agent-usage
```

等到对应 log id 成功。`invoke` 立刻返回不等于成功。

### 必须重启的会话

Hook 和 integration 只在会话启动时加载：

- Cursor：改过 `hooks.json` 后要重启窗格，再发一轮才会有 cache（headless
  `--print` 不会触发这些 hook）。`cx` 仍来自该会话的 `store.db`。
- Claude / Agy：在该会话里发一轮，StatusLine 才会留下观测。
- 刚装上 Herdr integration 的 agent：重启对应窗格。

## 5. 还是不对时

| 现象 | 做什么 |
| --- | --- |
| 插件没有 / 侧栏行缺失 | 再跑 `./install.sh`，然后 configure。不要手改 Herdr `config.toml` 里的额度行。 |
| integration 仍是 `not installed` | `herdr integration install <id>`，再重启该窗格。 |
| Claude/Agy 没有额度 | 在该会话发一轮。 |
| OMP 没有额度 | `omp usage --json --redact --provider <id>` 必须能跑通。 |
| Cursor 没有额度或仍是上一账号 | `cursor login` / `cursor-agent login`，再按上面做钥匙串 **Always Allow**。 |
| Kilo 没有额度 | `kilo auth list` 里要有 **Kilo Gateway** 的 `oauth` 登录，且该 pane 的会话后端是 Kilo Gateway。Kilo 没有 5h/7d：只有 Kilo Pass 套餐才有可读的额度（30d）。用共享余额的账号，或跑在 OpenRouter／OpenCode Go 上的 pane，都没有 Kilo 额度可显示。 |
| Muse 没有额度 | `muse login`（API key 登录没有订阅额度）；`storage` 为 `keychain` 时要批准钥匙串。 |
| 图标是方框 / `?` | 字体 + 终端映射 + 重载；见第 4 节。 |
| gauges 没有进度条 | 侧栏大约窄于 24 列是预期；拉宽后 `prefix+shift+r`。 |
| 拉宽后 gauges 仍是旧长度 | `prefix+shift+r`。没有随拖动实时发布的路径。 |

```sh
herdr plugin action invoke refresh --plugin herdr-agent-usage
herdr plugin action invoke configure --plugin herdr-agent-usage
```

之后改设置：`prefix+shift+q`，或
`herdr plugin pane open --plugin herdr-agent-usage --entrypoint settings --focus`。

## 6. 向用户汇报

- 探测到哪些 agent、实际启用了哪些
- 装了或仍缺哪些 integration
- 字体路径、给哪个终端写了映射、如何重载
- 有没有跑钥匙串批准，以及必须点 **Always Allow**
- 哪些正在跑的窗格要重启，哪些还要再发一轮
- 仍然坏着的地方，以及你用了哪条检查

不要在用户没看过图标、你也只验证了字体/映射/日志的情况下，声称侧栏已经
正确。glyph 本身需要人看一眼。
