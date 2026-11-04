//! Install the vendor icon font (plus the local MiMo logo face) and map
//! codepoints in known terminals.
//!
//! Herdr draws the Agents sidebar inside the host terminal. Private Use Area
//! glyphs only render when that terminal loads `Herdr Agent Icons Max` for
//! `U+E1A0`–`U+E1B6` and `U+E1C0`–`U+E1C5`, and `Herdr Agent MiMo` for
//! `U+E1B7`. Without the map, the cells are tofu — so configure always tries
//! to install both faces.
//!
//! The vendor face is byte-identical to hhdebb/herdr-radar. Do not rewrite it;
//! inject nothing into it. MiMo lives in its own one-glyph TTF so a logo
//! change can never disturb Grok or the rest of the radar set.

use crate::identity::{self, PLUGIN_ID};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

const FONT_FAMILY: &str = "Herdr Agent Icons Max";
const FONT_BASENAME: &str = "HerdrAgentIconsMax";
const FONT_BYTES: &[u8] = include_bytes!("../../assets/fonts/HerdrAgentIconsMax-Regular.ttf");

const MIMO_FONT_FAMILY: &str = "Herdr Agent MiMo";
const MIMO_FONT_BASENAME: &str = "HerdrAgentMiMo";
const MIMO_FONT_BYTES: &[u8] = include_bytes!("../../assets/fonts/HerdrAgentMiMo-Regular.ttf");

/// Vendor + state marks (herdr-radar compatibility). `E1B7` is the MiMo
/// logo face, mapped separately so the vendor TTF never changes.
const VENDOR_RANGES: [(&str, &str); 2] = [("E1A0", "E1B6"), ("E1C0", "E1C5")];
const MIMO_RANGE: (&str, &str) = ("E1B7", "E1B7");
const OWNED_FONT_FILE: &str = "owned-font";
const OWNED_MIMO_FONT_FILE: &str = "owned-mimo-font";

/// Copy both faces into the user font directory and write Ghostty / kitty maps
/// when those configs already exist. Never creates a terminal config from
/// scratch.
pub fn install(state_dir: &Path) -> Result<Vec<String>> {
    let mut notes = install_font(state_dir, FONT_BYTES, FONT_BASENAME, OWNED_FONT_FILE)?;
    notes.extend(install_font(
        state_dir,
        MIMO_FONT_BYTES,
        MIMO_FONT_BASENAME,
        OWNED_MIMO_FONT_FILE,
    )?);
    notes.extend(install_fontconfig()?);
    let mapped = configure_terminals()?;
    if mapped.is_empty() {
        notes.push(
            "font: no Ghostty/kitty config found; map U+E1A0-U+E1B6 and U+E1C0-U+E1C5 to \
             \"Herdr Agent Icons Max\", and U+E1B7 to \"Herdr Agent MiMo\", in your terminal \
             if icons show as boxes"
                .into(),
        );
    } else {
        notes.extend(mapped);
    }
    Ok(notes)
}

/// Ptyxis (and any fontconfig client) has no per-codepoint map. STIX Two Math
/// claims the same PUA, so prepend the owning face per codepoint.
fn install_fontconfig() -> Result<Vec<String>> {
    let Some(dir) = fontconfig_conf_dir() else {
        return Ok(vec!["fontconfig: no user config dir".into()]);
    };
    fs::create_dir_all(&dir).with_context(|| format!("create fontconfig dir {}", dir.display()))?;
    let path = dir.join("79-herdr-agent-usage.conf");
    let body = fontconfig_xml();
    if fs::read_to_string(&path).ok().as_deref() == Some(body.as_str()) {
        return Ok(vec![format!(
            "fontconfig: already up to date ({})",
            path.display()
        )]);
    }
    fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(vec![format!(
        "fontconfig: wrote {} — restart the terminal so it reloads",
        path.display()
    )])
}

fn fontconfig_conf_dir() -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|dirs| dirs.home_dir().join(".config")))?;
    Some(xdg.join("fontconfig/conf.d"))
}

fn fontconfig_xml() -> String {
    fn prepend(family: &str, codepoints: &[u32]) -> String {
        let ints = codepoints
            .iter()
            .map(|cp| format!("<int>0x{cp:x}</int>"))
            .collect::<Vec<_>>()
            .join("");
        format!(
            "  <match target=\"pattern\">\n\
             \x20   <test name=\"charset\" compare=\"contains\">\n\
             \x20     <charset>{ints}</charset>\n\
             \x20   </test>\n\
             \x20   <edit name=\"family\" mode=\"prepend\" binding=\"strong\"><string>{family}</string></edit>\n\
             \x20 </match>"
        )
    }
    let mut vendor: Vec<u32> = Vec::new();
    for (start, end) in VENDOR_RANGES {
        let start = u32::from_str_radix(start, 16).unwrap_or(0);
        let end = u32::from_str_radix(end, 16).unwrap_or(0);
        vendor.extend(start..=end);
    }
    // One match per codepoint: charset nodes inside a single test are ANDed.
    let mut matches = vec![prepend(MIMO_FONT_FAMILY, &[0xE1B7])];
    for cp in vendor {
        matches.push(prepend(FONT_FAMILY, &[cp]));
    }
    format!(
        "<?xml version=\"1.0\"?>\n\
         <!DOCTYPE fontconfig SYSTEM \"urn:fontconfig:fonts.dtd\">\n\
         <fontconfig>\n\
         \x20 <!-- BEGIN herdr-agent-usage font -->\n\
         \x20 <!-- STIX Two Math also claims this PUA range. -->\n\
         {}\n\
         \x20 <!-- END herdr-agent-usage font -->\n\
         </fontconfig>\n",
        matches.join("\n")
    )
}

fn install_font(
    state_dir: &Path,
    bytes: &[u8],
    basename: &str,
    owned_file: &str,
) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    let dir = user_font_dir();
    fs::create_dir_all(&dir).with_context(|| format!("create font dir {}", dir.display()))?;
    let hash = short_hash(bytes);
    let target = dir.join(format!("{basename}-{hash}.ttf"));
    if target.exists() {
        notes.push(format!("font: already installed ({})", target.display()));
    } else {
        fs::write(&target, bytes).with_context(|| format!("write font {}", target.display()))?;
        fs::write(state_dir.join(owned_file), hash.as_bytes())
            .with_context(|| format!("record {basename} ownership"))?;
        notes.push(format!("font: installed {}", target.display()));
    }
    Ok(notes)
}

/// Remove only config blocks we marked and font files this installation
/// created. A pre-existing shared font must remain available to other tools.
pub fn uninstall(state_dir: &Path) -> Result<()> {
    for target in terminal_targets() {
        if !target.path.exists() {
            continue;
        }
        let original = fs::read_to_string(&target.path)
            .with_context(|| format!("read {} config {}", target.name, target.path.display()))?;
        if let Some(updated) = remove_marked(&original) {
            fs::write(&target.path, updated).with_context(|| {
                format!("write {} config {}", target.name, target.path.display())
            })?;
        }
    }
    if let Some(dir) = fontconfig_conf_dir() {
        let path = dir.join("79-herdr-agent-usage.conf");
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
    }
    remove_owned_font(state_dir, FONT_BYTES, FONT_BASENAME, OWNED_FONT_FILE)?;
    remove_owned_font(
        state_dir,
        MIMO_FONT_BYTES,
        MIMO_FONT_BASENAME,
        OWNED_MIMO_FONT_FILE,
    )?;
    Ok(())
}

fn remove_owned_font(
    state_dir: &Path,
    bytes: &[u8],
    basename: &str,
    owned_file: &str,
) -> Result<()> {
    let hash = short_hash(bytes);
    let marker = state_dir.join(owned_file);
    if fs::read_to_string(&marker).ok().as_deref() == Some(hash.as_str()) {
        let target = user_font_dir().join(format!("{basename}-{hash}.ttf"));
        match fs::remove_file(&target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("remove font {}", target.display()))
            }
        }
        fs::remove_file(marker).with_context(|| format!("remove {basename} ownership marker"))?;
    }
    Ok(())
}

fn configure_terminals() -> Result<Vec<String>> {
    let mut notes = Vec::new();
    for target in terminal_targets() {
        if !target.path.exists() {
            continue;
        }
        let original = fs::read_to_string(&target.path)
            .with_context(|| format!("read {} config {}", target.name, target.path.display()))?;
        let body = marked_block(target.lines);
        let updated = upsert_marked(&original, &body);
        if updated == original {
            notes.push(format!(
                "{}: codepoint map already in {}",
                target.name,
                target.path.display()
            ));
            continue;
        }
        fs::write(&target.path, updated)
            .with_context(|| format!("write {} config {}", target.name, target.path.display()))?;
        notes.push(format!(
            "{}: codepoint map written to {} — {}",
            target.name,
            target.path.display(),
            target.reload
        ));
    }
    Ok(notes)
}

struct TerminalTarget {
    name: &'static str,
    path: PathBuf,
    lines: Vec<String>,
    reload: &'static str,
}

fn terminal_targets() -> Vec<TerminalTarget> {
    let home = directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let ghostty_lines = map_lines(Format::Ghostty);
    let kitty_lines = map_lines(Format::Kitty);
    let mut targets = Vec::new();
    for name in ["config", "config.ghostty"] {
        targets.push(TerminalTarget {
            name: "ghostty",
            path: home
                .join("Library/Application Support/com.mitchellh.ghostty")
                .join(name),
            lines: ghostty_lines.clone(),
            reload: "reload Ghostty config (cmd+shift+,) or reopen the terminal",
        });
        targets.push(TerminalTarget {
            name: "ghostty",
            path: xdg.join("ghostty").join(name),
            lines: ghostty_lines.clone(),
            reload: "reload Ghostty config (cmd+shift+,) or reopen the terminal",
        });
    }
    targets.push(TerminalTarget {
        name: "kitty",
        path: xdg.join("kitty/kitty.conf"),
        lines: kitty_lines,
        reload: "reload kitty (ctrl+shift+f5) or reopen the terminal",
    });
    targets
}

enum Format {
    Ghostty,
    Kitty,
}

fn map_lines(format: Format) -> Vec<String> {
    let mut lines = Vec::new();
    for (start, end) in VENDOR_RANGES {
        match format {
            Format::Ghostty => lines.push(format!(
                "font-codepoint-map = U+{start}-U+{end}=\"{FONT_FAMILY}\""
            )),
            Format::Kitty => lines.push(format!("symbol_map U+{start}-U+{end} {FONT_FAMILY}")),
        }
    }
    let (start, end) = MIMO_RANGE;
    match format {
        Format::Ghostty => lines.push(format!(
            "font-codepoint-map = U+{start}-U+{end}=\"{MIMO_FONT_FAMILY}\""
        )),
        Format::Kitty => lines.push(format!("symbol_map U+{start}-U+{end} {MIMO_FONT_FAMILY}")),
    }
    lines
}

fn marked_block(lines: Vec<String>) -> String {
    std::iter::once(identity::font_marker_start(PLUGIN_ID))
        .chain(lines)
        .chain(std::iter::once(identity::font_marker_end(PLUGIN_ID)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn find_marked_span(text: &str) -> Option<(usize, usize)> {
    identity::all_plugin_ids().find_map(|id| {
        let start = identity::font_marker_start(id);
        let end = identity::font_marker_end(id);
        let from = text.find(&start)?;
        let rel_end = text[from..].find(&end)?;
        Some((from, from + rel_end + end.len()))
    })
}

fn upsert_marked(text: &str, body: &str) -> String {
    if let Some((from, to)) = find_marked_span(text) {
        let mut next = String::new();
        next.push_str(text[..from].trim_end());
        next.push_str("\n\n");
        next.push_str(body);
        let rest = strip_marked(text[to..].trim_start_matches('\n'));
        if !rest.is_empty() {
            next.push('\n');
            next.push_str(&rest);
        } else {
            next.push('\n');
        }
        return next;
    }
    let mut next = text.trim_end().to_string();
    if !next.is_empty() {
        next.push_str("\n\n");
    }
    next.push_str(body);
    next.push('\n');
    next
}

fn strip_marked(text: &str) -> String {
    let mut next = text.to_string();
    while let Some((from, to)) = find_marked_span(&next) {
        let before = next[..from].trim_end_matches('\n');
        let after = next[to..].trim_start_matches('\n');
        let mut output = before.to_owned();
        if !output.is_empty() && !after.is_empty() {
            output.push_str("\n\n");
        } else if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(after);
        next = output;
    }
    next
}

fn remove_marked(text: &str) -> Option<String> {
    find_marked_span(text)?;
    Some(strip_marked(text))
}

fn user_font_dir() -> PathBuf {
    let home = directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    if cfg!(target_os = "macos") {
        return home.join("Library/Fonts");
    }
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"))
        .join("fonts")
}

fn short_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_is_idempotent() {
        let body = marked_block(map_lines(Format::Ghostty));
        let first = upsert_marked("# existing\n", &body);
        let again = upsert_marked(&first, &body);
        assert_eq!(first, again);
        assert!(first.contains("font-codepoint-map"));
        assert!(first.contains("Herdr Agent Icons Max"));
        assert!(first.contains("Herdr Agent MiMo"));
        let start = identity::font_marker_start(PLUGIN_ID);
        assert_eq!(first.matches(&start).count(), 1);
        assert_eq!(remove_marked(&first), Some("# existing\n".into()));
    }

    #[test]
    fn upsert_rewrites_an_alias_font_block() {
        let alias = concat!(
            "# existing\n\n",
            "# BEGIN herdr-agent-quota font\n",
            "font-codepoint-map = U+E1A0-U+E1B6=\"old\"\n",
            "# END herdr-agent-quota font\n",
        );
        let body = marked_block(vec!["font-codepoint-map = U+E1A0-U+E1B6=\"new\"".into()]);
        let updated = upsert_marked(alias, &body);
        assert!(updated.contains(&identity::font_marker_start(PLUGIN_ID)));
        assert!(updated.contains("font-codepoint-map = U+E1A0-U+E1B6=\"new\""));
        assert!(!updated.contains("herdr-agent-quota font"));
        assert_eq!(upsert_marked(&updated, &body), updated);
    }

    #[test]
    fn bundled_fonts_are_present_and_distinct() {
        assert!(FONT_BYTES.len() > 1000);
        assert!(MIMO_FONT_BYTES.len() > 500);
        let vendor = short_hash(FONT_BYTES);
        let mimo = short_hash(MIMO_FONT_BYTES);
        assert_eq!(vendor.len(), 8);
        assert_eq!(mimo.len(), 8);
        assert_ne!(vendor, mimo);
    }

    #[test]
    fn mimo_range_is_separate_from_the_vendor_face() {
        let lines = map_lines(Format::Ghostty);
        assert!(lines
            .iter()
            .any(|line| line.contains("U+E1B6=\"Herdr Agent Icons Max\"")));
        assert!(lines
            .iter()
            .any(|line| line.contains("U+E1B7=\"Herdr Agent MiMo\"")));
        assert!(!lines
            .iter()
            .any(|line| line.contains("U+E1B7=\"Herdr Agent Icons Max\"")));
    }

    #[test]
    fn fontconfig_prepends_each_face_for_its_own_codepoints() {
        let xml = fontconfig_xml();
        assert!(xml.contains("<int>0xe1b7</int>"));
        assert!(xml.contains("Herdr Agent MiMo"));
        assert!(xml.contains("Herdr Agent Icons Max"));
        assert!(xml.contains("<charset><int>0xe1b1</int></charset>"));
    }

    #[test]
    fn path_helper_compiles_for_coverage() {
        assert!(!user_font_dir().as_os_str().is_empty());
    }
}
