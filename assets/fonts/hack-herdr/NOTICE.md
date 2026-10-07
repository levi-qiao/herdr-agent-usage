<!-- markdownlint-disable MD013 -- Font commands, URLs and license tables contain long references. -->

# Hack Herdr Nerd Font notices

Generated TTFs are release artifacts and are intentionally not stored in the
source tree. This directory keeps the notices and reproducibility metadata.

This is a renamed, modified build of Hack Nerd Font, not an official Hack,
Nerd Fonts, Herdr, or vendor release. The build adds Herdr icon outlines; it
keeps all original Hack/Nerd glyphs and their metrics. No original logo design
was created by this contribution. Brand names and marks remain with their
owners and do not imply affiliation or endorsement.

## Sources

- Hack v3.003, patched by Nerd Fonts v3.4.0:
  <https://github.com/ryanoasis/nerd-fonts/tree/v3.4.0/patched-fonts/Hack>
- Icon-only input: `../HerdrAgentIconsMax-Regular.ttf`, version 1.8.0,
  SHA-256 `d77910ea4e70a86e4618ba5329f51830f39b830477c1fb809dea704ecc199f66`.
  This is the 29-glyph face already bundled by this plugin, not a newer face
  from herdr-radar. Original sources: qintmb/herdr-icon-agent-ui and
  hhdebb/herdr-radar.
- Herdr license and notices were copied from hhdebb/herdr-radar at
  `1ba8cc4a3900191c9f1f4f850398c7dd10212df2`. Its notices describe newer fonts
  as well; extra marks mentioned there are not added by this build.

## Licenses and attribution

The project's MIT license applies to the build script, not as a replacement
for third-party font, icon, or trademark terms. Keep this file and the entire
`licenses/` directory with every redistribution. Do not sell the font by
itself. The modified family contains neither reserved name "Bitstream" nor
"Vera".

| Component | Source / attribution | License text |
| --- | --- | --- |
| Hack / Bitstream Vera / DejaVu | Source Foundry Authors; Bitstream, Inc.; DejaVu contributors | [Hack MIT / Bitstream Vera / public-domain notice](licenses/HACK.txt) |
| Nerd Fonts | Ryan L McIntyre and contributors | [Nerd Fonts licensing](licenses/NERD-FONTS.txt) |
| Herdr marks | qintmb and herdr-kit contributors; respective vendors | [MIT](licenses/HERDR-ICONS.txt), [third-party notices](licenses/HERDR-THIRD-PARTY-NOTICES.md) |
| Codicons | Microsoft | [CC BY 4.0](licenses/codicons-LICENSE.txt) |
| Font Awesome | Fonticons, Inc. | [Font Awesome licensing](licenses/font-awesome-LICENSE.txt) |
| Material Design Icons | Pictogrammers | [notice](licenses/materialdesign-LICENSE), [Apache 2.0](licenses/APACHE-2.0.txt) |
| Octicons | GitHub, Inc. | [MIT](licenses/octicons-LICENSE) |
| Pomicons | Gabriele Lana | [OFL 1.1](licenses/pomicons-LICENSE) |
| Powerline Extra | Ryan L McIntyre | [MIT](licenses/powerline-extra-LICENSE) |
| Powerline Symbols | Powerline contributors | [license](licenses/powerline-symbols-LICENSE.txt) |
| Weather Icons | Erik Flowers | [OFL 1.1](licenses/weather-icons-OFL.txt) |
| Font Logos | Lukas W and contributors | [Unlicense](licenses/FONT-LOGOS.txt) |

Other inherited Nerd Fonts glyph sources (Devicons, Font Awesome Extension,
IEC Power Symbols, and Seti/original-source) retain their original attribution
and MIT terms documented by the [Nerd Fonts v3.4.0 license audit](https://github.com/ryanoasis/nerd-fonts/blob/v3.4.0/license-audit.md).
No vendor trademark rights are granted by these notices.
