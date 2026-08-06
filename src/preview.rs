use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;

const MAX_PREVIEW_LINES: usize = 500;
const BINARY_CHECK_BYTES: usize = 8192;
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024; // 10MB
const MAX_IMAGE_SIZE: u64 = 20 * 1024 * 1024; // 20MB for images

/// A styled text span for rendering.
#[derive(Debug, Clone)]
pub struct StyledSpan {
    pub text: String,
    pub fg: (u8, u8, u8),
}

/// File preview state.
pub struct Preview {
    pub file_path: Option<PathBuf>,
    pub lines: Vec<String>,
    pub highlighted_lines: Vec<Vec<StyledSpan>>,
    /// Vertical scroll position (line index of the top visible line).
    pub scroll_offset: usize,
    /// Horizontal scroll position (char count dropped from the left of
    /// each rendered line). Enables viewing long lines that exceed the
    /// preview panel width.
    pub h_scroll_offset: usize,
    pub is_binary: bool,
    /// Image preview state (set when an image file is loaded).
    pub image_protocol: Option<StatefulProtocol>,
    syntax_set: SyntaxSet,
    theme_set: ThemeSet,
}

impl Preview {
    pub fn new() -> Self {
        Self {
            file_path: None,
            lines: Vec::new(),
            highlighted_lines: Vec::new(),
            scroll_offset: 0,
            h_scroll_offset: 0,
            is_binary: false,
            image_protocol: None,
            syntax_set: SyntaxSet::load_defaults_newlines(),
            theme_set: ThemeSet::load_defaults(),
        }
    }

    /// Check if the current preview is an image.
    pub fn is_image(&self) -> bool {
        self.image_protocol.is_some()
    }

    /// Load a file for preview. `messages` carries the resolved UI
    /// language so error placeholders ("File too large", etc.) render
    /// in the user's chosen language without `Preview` having to carry
    /// a persistent `Lang` field — a new messages pointer is injected
    /// on every load, which is enough because renga doesn't support
    /// mid-session language switching. If that ever changes, the
    /// `file_path == path` early-return below must also invalidate
    /// any cached error lines so they get re-rendered in the new
    /// language.
    pub fn load(
        &mut self,
        path: &Path,
        picker: Option<&mut Picker>,
        messages: &'static crate::i18n::Messages,
    ) {
        if self.file_path.as_deref() == Some(path) {
            return;
        }

        self.file_path = Some(path.to_path_buf());
        self.scroll_offset = 0;
        self.h_scroll_offset = 0;
        self.lines.clear();
        self.highlighted_lines.clear();
        self.is_binary = false;
        self.image_protocol = None;

        let metadata = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => {
                self.lines = vec![messages.preview_read_failed.to_string()];
                return;
            }
        };

        if !metadata.is_file() {
            self.lines = vec![messages.preview_not_regular.to_string()];
            return;
        }

        // Try loading as image first (by extension)
        if is_image_extension(path) {
            if metadata.len() > MAX_IMAGE_SIZE {
                self.lines = vec![messages.image_too_large(
                    metadata.len() as f64 / 1024.0 / 1024.0,
                    MAX_IMAGE_SIZE as f64 / 1024.0 / 1024.0,
                )];
                return;
            }
            if let Some(picker) = picker {
                match image::ImageReader::open(path)
                    .and_then(|r| r.with_guessed_format())
                    .map_err(|e| e.to_string())
                    .and_then(|r| r.decode().map_err(|e| e.to_string()))
                {
                    Ok(dyn_img) => {
                        self.image_protocol = Some(picker.new_resize_protocol(dyn_img));
                        return;
                    }
                    Err(_) => {
                        // Fall through to text/binary preview
                    }
                }
            }
        }

        if metadata.len() > MAX_FILE_SIZE {
            self.lines = vec![messages.file_too_large(
                metadata.len() as f64 / 1024.0 / 1024.0,
                MAX_FILE_SIZE as f64 / 1024.0 / 1024.0,
            )];
            return;
        }

        if is_binary_file(path) {
            self.is_binary = true;
            return;
        }

        // Read text file
        match File::open(path) {
            Ok(file) => {
                let reader = BufReader::new(file);
                self.lines = reader
                    .lines()
                    .take(MAX_PREVIEW_LINES)
                    .filter_map(|l| l.ok())
                    .collect();
            }
            Err(_) => {
                self.lines = vec![messages.preview_read_failed.to_string()];
                return;
            }
        }

        // Apply syntax highlighting
        self.highlight(path);
    }

    /// Apply syntax highlighting to loaded lines.
    fn highlight(&mut self, path: &Path) {
        let syntax = self
            .syntax_set
            .find_syntax_for_file(path)
            .ok()
            .flatten()
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());

        let theme = &self.theme_set.themes["base16-eighties.dark"];
        let mut highlighter = HighlightLines::new(syntax, theme);

        self.highlighted_lines.clear();

        for line in &self.lines {
            let line_with_newline = format!("{}\n", line);
            match highlighter.highlight_line(&line_with_newline, &self.syntax_set) {
                Ok(ranges) => {
                    let spans: Vec<StyledSpan> = ranges
                        .into_iter()
                        .map(|(style, text)| {
                            let fg = style.foreground;
                            StyledSpan {
                                text: text.trim_end_matches('\n').to_string(),
                                fg: (fg.r, fg.g, fg.b),
                            }
                        })
                        .filter(|s| !s.text.is_empty())
                        .collect();
                    self.highlighted_lines.push(spans);
                }
                Err(_) => {
                    // Fallback: plain text
                    self.highlighted_lines.push(vec![StyledSpan {
                        text: line.clone(),
                        fg: (0xe6, 0xed, 0xf3),
                    }]);
                }
            }
        }
    }

    /// Close the preview.
    pub fn close(&mut self) {
        self.file_path = None;
        self.lines.clear();
        self.highlighted_lines.clear();
        self.scroll_offset = 0;
        self.h_scroll_offset = 0;
        self.is_binary = false;
        self.image_protocol = None;
    }

    /// Check if preview is active.
    pub fn is_active(&self) -> bool {
        self.file_path.is_some()
    }

    /// Get the filename for display.
    pub fn filename(&self) -> String {
        self.file_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    }

    /// Scroll up by amount.
    pub fn scroll_up(&mut self, amount: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
    }

    /// Scroll down by amount.
    pub fn scroll_down(&mut self, amount: usize) {
        let max_offset = self.lines.len().saturating_sub(1);
        self.scroll_offset = (self.scroll_offset + amount).min(max_offset);
    }

    /// Scroll left by N chars. Clamps at column 0.
    pub fn scroll_left(&mut self, amount: usize) {
        self.h_scroll_offset = self.h_scroll_offset.saturating_sub(amount);
    }

    /// Scroll right by N chars. Clamped so there's always at least
    /// a bit of text visible (we stop when h_scroll equals the widest
    /// line minus 10 chars — keeps the user from scrolling off into
    /// blank territory).
    pub fn scroll_right(&mut self, amount: usize) {
        let widest = self
            .lines
            .iter()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0);
        let max_h = widest.saturating_sub(10);
        self.h_scroll_offset = (self.h_scroll_offset + amount).min(max_h);
    }
}

/// Check if a file has an image extension. Must stay in sync with the
/// feature flags enabled on the `image` crate in `Cargo.toml` — an
/// extension listed here but missing from the feature set would route
/// the file to the image pipeline and fail to decode at runtime with
/// a format-not-supported error. Upstream PR #7 originally included
/// `ico` / `tiff` / `tif` here but only enabled `png / jpeg / gif /
/// bmp / webp` features; trimmed on sync to match what actually
/// decodes.
fn is_image_extension(path: &Path) -> bool {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp"
    )
}

#[cfg(test)]
mod image_ext_tests {
    use super::is_image_extension;
    use std::path::Path;

    #[test]
    fn detects_enabled_formats() {
        // Keep this list in lockstep with the `image` crate features
        // declared in Cargo.toml.
        for ext in ["png", "jpg", "jpeg", "gif", "bmp", "webp"] {
            let path_lower = format!("x.{ext}");
            assert!(
                is_image_extension(Path::new(&path_lower)),
                "`.{ext}` must be detected as an image"
            );
            // Case-insensitive.
            let path_upper = format!("x.{}", ext.to_uppercase());
            assert!(
                is_image_extension(Path::new(&path_upper)),
                "`.{}` must be detected as an image",
                ext.to_uppercase()
            );
        }
    }

    #[test]
    fn rejects_formats_without_crate_features() {
        // Feature flags we do NOT enable — an `image::open` call on
        // these would fail at runtime. The detector must not route
        // them to the image pipeline. Guard against regressions if
        // Cargo.toml ever drops features out of sync.
        for ext in ["ico", "tiff", "tif", "avif", "heic", "tga"] {
            let path = format!("x.{ext}");
            assert!(
                !is_image_extension(Path::new(&path)),
                "`.{ext}` must not be detected as an image (crate feature not enabled)"
            );
        }
    }

    #[test]
    fn rejects_non_image_extensions() {
        for ext in ["txt", "rs", "md", "json", ""] {
            let path = if ext.is_empty() {
                "Cargo".to_string()
            } else {
                format!("x.{ext}")
            };
            assert!(!is_image_extension(Path::new(&path)));
        }
    }
}

/// Check if a file is likely binary by reading only the first N bytes.
fn is_binary_file(path: &Path) -> bool {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut reader = BufReader::new(file);
    let mut buf = [0u8; BINARY_CHECK_BYTES];
    match reader.read(&mut buf) {
        Ok(n) => buf[..n].contains(&0),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_preview_initial_state() {
        let preview = Preview::new();
        assert!(!preview.is_active());
        assert!(preview.lines.is_empty());
    }

    #[test]
    fn test_preview_load_text_file() {
        let mut preview = Preview::new();
        preview.load(Path::new("Cargo.toml"), None, &crate::i18n::MESSAGES_EN);
        assert!(preview.is_active());
        assert!(!preview.is_binary);
        assert!(!preview.lines.is_empty());
        assert!(!preview.highlighted_lines.is_empty());
    }

    #[test]
    fn test_preview_close() {
        let mut preview = Preview::new();
        preview.load(Path::new("Cargo.toml"), None, &crate::i18n::MESSAGES_EN);
        assert!(preview.is_active());

        preview.close();
        assert!(!preview.is_active());
        assert!(preview.lines.is_empty());
        assert!(preview.highlighted_lines.is_empty());
    }

    #[test]
    fn test_preview_scroll() {
        let mut preview = Preview::new();
        preview.lines = (0..100).map(|i| format!("line {}", i)).collect();
        preview.scroll_down(10);
        assert_eq!(preview.scroll_offset, 10);
        preview.scroll_up(5);
        assert_eq!(preview.scroll_offset, 5);
        preview.scroll_up(100);
        assert_eq!(preview.scroll_offset, 0);
    }

    #[test]
    fn test_preview_highlight_rust() {
        let mut preview = Preview::new();
        preview.load(Path::new("src/main.rs"), None, &crate::i18n::MESSAGES_EN);
        assert!(!preview.highlighted_lines.is_empty());
        // Highlighted lines should have colored spans
        let first = &preview.highlighted_lines[0];
        assert!(!first.is_empty());
    }

    #[test]
    fn test_preview_highlight_actually_uses_the_syntax_dump() {
        // `Preview` reaches syntect only through the binary dumps
        // compiled into the crate (`SyntaxSet::load_defaults_newlines`
        // / `ThemeSet::load_defaults`); the `plist-load` / `yaml-load`
        // source parsers are switched off in Cargo.toml so quick-xml
        // and yaml-rust stay out of the tree. That makes `dump-load`
        // the single supply of syntaxes and themes, so this pins the
        // property the trim depends on: an empty or unreadable syntax
        // dump would silently fall back to plain text, which still
        // yields one span per line and would slip past the assertions
        // in `test_preview_highlight_rust`.
        let mut preview = Preview::new();
        preview.load(Path::new("src/main.rs"), None, &crate::i18n::MESSAGES_EN);

        let colors: std::collections::HashSet<(u8, u8, u8)> = preview
            .highlighted_lines
            .iter()
            .flatten()
            .map(|span| span.fg)
            .collect();
        assert!(
            colors.len() > 1,
            "expected the Rust syntax + theme dumps to produce more than one \
             foreground color; got {colors:?} (a single color means the syntax \
             set fell back to plain text)"
        );
    }
}
