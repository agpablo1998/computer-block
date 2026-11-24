//! The saved "normal" state, captured once by the admin with `--setup` or
//! `--capture`. Plain `key=value` text in `%LOCALAPPDATA%\ComputerBlock`.

use super::desktop::DesktopView;
use super::util::data_dir;
use std::fmt::Write as _;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Normal {
    /// Default printer name. `None`: printer not enforced.
    pub printer: Option<String>,
    /// Desktop icon view. `None`: desktop not enforced.
    pub desktop: Option<DesktopView>,
}

fn path() -> Option<PathBuf> {
    data_dir().map(|d| d.join("normal.txt"))
}

impl Normal {
    pub fn load() -> Option<Self> {
        let text = std::fs::read_to_string(path()?).ok()?;
        Some(Self::parse(&text))
    }

    pub fn save(&self) -> std::io::Result<()> {
        let p = path().ok_or_else(|| std::io::Error::other("LOCALAPPDATA not set"))?;
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(p, self.render())
    }

    pub fn delete() {
        if let Some(p) = path() {
            let _ = std::fs::remove_file(p);
        }
    }

    fn parse(text: &str) -> Self {
        let mut n = Self::default();
        let (mut mode, mut size, mut flags) = (None, None, None);
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim();
            match k.trim() {
                "printer" if !v.is_empty() => n.printer = Some(v.to_owned()),
                "desktop_view_mode" => mode = v.parse().ok(),
                "desktop_icon_size" => size = v.parse().ok(),
                "desktop_flags" => flags = v.parse().ok(),
                _ => {}
            }
        }
        if let (Some(view_mode), Some(icon_size), Some(flags)) = (mode, size, flags) {
            n.desktop = Some(DesktopView {
                view_mode,
                icon_size,
                flags,
            });
        }
        n
    }

    fn render(&self) -> String {
        let mut s = String::from("# Saved by computer-block --setup / --capture\n");
        if let Some(p) = &self.printer {
            let _ = writeln!(s, "printer={p}");
        }
        if let Some(d) = &self.desktop {
            let _ = writeln!(s, "desktop_view_mode={}", d.view_mode);
            let _ = writeln!(s, "desktop_icon_size={}", d.icon_size);
            let _ = writeln!(s, "desktop_flags={}", d.flags);
        }
        s
    }
}
