<!-- markdownlint-disable MD013 -- Font commands, URLs and license references are long. -->

# iTerm2: Hack + Nerd Fonts + Herdr icons

[简体中文](#简体中文)

Use **Hack Herdr Nerd Font** when iTerm2 needs both the Herdr sidebar marks
and Nerd Font symbols used by Pi or shell prompts. It is an optional font;
the plugin does not select it or change terminal preferences.

The source repository intentionally does **not** carry the generated TTFs.
A release that includes this feature publishes `HackHerdrNerdFont.zip` as a
separate asset. The same files can be rebuilt from the pinned sources below.

## Install on macOS

1. Open the matching herdr-agent-usage GitHub release and download
   `HackHerdrNerdFont.zip`. If that release predates this feature, use
   [Rebuild](#rebuild) instead.
2. Unzip it and verify the four TTFs against the included `SHA256SUMS`.
3. Copy the four `HackHerdrNerdFont-*.ttf` files into
   `$HOME/Library/Fonts/`.
4. In **iTerm2 Settings → Profiles → Text**, use these settings:

| Setting | Value |
| --- | --- |
| Font | Hack Herdr Nerd Font |
| Style | Regular |
| Size | Keep your existing size; verified at 13 pt |
| Use a different font for non-ASCII text | Off |

Apply this to each profile used for local or SSH sessions. Fonts are rendered
on the Mac, so installing a font only on the SSH server does not fix iTerm2.
To revert, select the previous font; this does not overwrite it.

## Contents and verification

The bundle contains Regular, Bold, Italic, and BoldItalic. Each keeps the
corresponding Hack Nerd Font character mappings, glyph widths, and line
metrics, then adds:

- 23 Herdr logos at `U+E1A0–U+E1B6`;
- 6 Herdr status symbols at `U+E1C0–U+E1C5`.

The icon outlines are scaled from 1000 to 2048 units/em. Their advance is one
Hack cell, with the deliberate side overhang of the source **Max** face. The
base font already contains Pi's Nerd Font glyphs and `▰/▱` gauges.

The original contribution was visually checked in iTerm2 on macOS with all
29 Herdr characters, Pi icons, gauges, bold/italic, Chinese, ASCII, and the
`U+2060–U+2062` state-tag prefixes. This is not a claim about other terminals.

## Rebuild

The release workflow and this command use Nerd Fonts **v3.4.0** and
`fonttools==4.55.0`.

Download `Hack.zip` from the Nerd Fonts v3.4.0 release and verify:

```text
SHA256  8ca33a60c791392d872b80d26c42f2bfa914a480f9eb2d7516d9f84373c36897
```

Unpack it, then run from this repository:

```bash
uv run --no-project --with fonttools==4.55.0 \
  python scripts/build-hack-herdr-font.py \
  --base-dir /path/to/unpacked/Hack
```

Outputs go to `dist/hack-herdr/` and include a generated `SHA256SUMS`.
For the pinned inputs, it should match
`assets/fonts/hack-herdr/EXPECTED_SHA256SUMS`.

The bundled `assets/fonts/HerdrAgentIconsMax-Regular.ttf` remains the icon
input used by the plugin itself. The builder rejects conflicting mappings and
verifies that original mappings, glyph widths, line metrics, and all icon
mappings survive saving.

## Redistribution

Keep `NOTICE.md`, the entire `licenses/` directory, and `SHA256SUMS` with
the four TTFs. The release workflow packages those files together. This is a
locally modified font, not an official Hack, Nerd Fonts, Herdr, or vendor
release; third-party font, icon, and trademark terms remain applicable.

## 简体中文

**Hack Herdr Nerd Font** 把 Hack Nerd Font 与 Herdr 的 29 个图标合到一套字体里，
用于 iTerm2 同时需要 Herdr 侧栏图标、Pi / shell 的 Nerd Font 符号的情况。
它是可选字体，插件不会自动选择或修改终端设置。

源码仓库不再保存生成后的 TTF。包含这个功能的 herdr-agent-usage release 会把
`HackHerdrNerdFont.zip` 作为独立附件发布；较早的 release 没有附件时，可按上面的
Rebuild 步骤从钉死的版本自行构建。

安装时解压附件、核对 `SHA256SUMS`，把四个 TTF 复制到
`$HOME/Library/Fonts/`。然后在 **iTerm2 Settings → Profiles → Text** 中：

1. 选择 **Hack Herdr Nerd Font → Regular**，保留原字号。
2. 关闭 **Use a different font for non-ASCII text**。
3. 每个需要的 Profile 都设置一次；SSH 也使用本地 Mac 字体，无需改服务器。

回退时直接选回原字体。分享这套字体时必须同时带上 NOTICE、licenses 和
SHA256SUMS。
