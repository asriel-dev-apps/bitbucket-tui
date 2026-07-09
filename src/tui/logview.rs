//! ステップログ / ファイル内容のスクロール表示状態と、テキストの整形。
//!
//! M1 の `DiffState` と同じスクロール操作（1 行 / 1 画面 / 先頭 / 末尾）を提供する汎用の
//! ページャ。ログには ANSI エスケープや制御文字が混じり得るため、表示前に除去する。
//! M3 の FileView もこの型を流用し、バイナリ/巨大ファイルは安全に代替表示する。

/// FileView で先頭から表示する最大行数（超過分は打ち切って末尾に注記を付す）。
pub const MAX_FILE_LINES: usize = 5000;

/// ステップログの表示状態。
#[derive(Debug, Clone, Default)]
pub struct LogView {
    /// 対象ステップの uuid（再取得・受信メッセージの照合に使う）。
    pub step_uuid: String,
    /// 見出し（例: `#12 / Build and test`）。
    pub title: String,
    /// 整形済みログ行。
    pub lines: Vec<String>,
    /// 先頭からのスクロール行数。
    pub scroll: usize,
    /// 直近描画時のビューポート高さ（スクロール上限計算に使う。`ui` が毎フレーム更新）。
    pub viewport: usize,
    /// ログが未生成（404 等）だったか。
    pub missing: bool,
}

impl LogView {
    /// ログテキストから表示状態を作る（ANSI/制御文字を除去して行分割）。
    pub fn from_text(step_uuid: String, title: String, text: &str) -> Self {
        Self {
            step_uuid,
            title,
            lines: sanitize_log(text),
            scroll: 0,
            viewport: 0,
            missing: false,
        }
    }

    /// ソースファイル内容の表示状態を作る（M3 FileView 用）。
    ///
    /// mimetype または内容の NUL バイトからバイナリと判定した場合は
    /// 「(バイナリ表示不可)」を表示する。巨大ファイルは先頭 [`MAX_FILE_LINES`] 行で打ち切る。
    /// `key` は取得結果の照合に使うキー（ファイルパス）、`title` は見出し。
    pub fn from_file(key: String, title: String, mimetype: Option<&str>, content: &str) -> Self {
        if looks_binary(mimetype, content) {
            return Self {
                step_uuid: key,
                title,
                lines: vec!["(バイナリ表示不可)".to_string()],
                scroll: 0,
                viewport: 0,
                missing: true,
            };
        }
        let mut lines = sanitize_log(content);
        if lines.len() > MAX_FILE_LINES {
            lines.truncate(MAX_FILE_LINES);
            lines.push(format!("… (先頭 {MAX_FILE_LINES} 行のみ表示・以降を省略)"));
        }
        Self {
            step_uuid: key,
            title,
            lines,
            scroll: 0,
            viewport: 0,
            missing: false,
        }
    }

    /// 「ログなし」表示の状態を作る。
    pub fn missing(step_uuid: String, title: String) -> Self {
        Self {
            step_uuid,
            title,
            lines: vec!["(ログなし)".to_string()],
            scroll: 0,
            viewport: 0,
            missing: true,
        }
    }

    /// スクロール上限（最終行が末尾に来る位置）。
    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(self.viewport.max(1))
    }

    /// スクロール位置をビューポートに合わせてクランプする（描画時に呼ぶ）。
    pub fn clamp_scroll(&mut self) {
        let max = self.max_scroll();
        if self.scroll > max {
            self.scroll = max;
        }
    }

    /// 下へ `amount` 行スクロール。
    pub fn scroll_down(&mut self, amount: usize) {
        self.scroll = (self.scroll + amount).min(self.max_scroll());
    }

    /// 上へ `amount` 行スクロール。
    pub fn scroll_up(&mut self, amount: usize) {
        self.scroll = self.scroll.saturating_sub(amount);
    }

    /// 先頭へ。
    pub fn scroll_to_top(&mut self) {
        self.scroll = 0;
    }

    /// 末尾へ。
    pub fn scroll_to_bottom(&mut self) {
        self.scroll = self.max_scroll();
    }
}

/// ログテキストを表示用に整形する。
///
/// ANSI エスケープシーケンスを除去し、タブ以外の制御文字を落とし、行へ分割する。
/// `str::lines()` を使うため末尾の余分な空行は生じない。
pub fn sanitize_log(text: &str) -> Vec<String> {
    strip_ansi(text).lines().map(str::to_string).collect()
}

/// ANSI エスケープシーケンス（CSI 等）と、タブ以外の制御文字を除去する。
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // ESC。CSI（`ESC [` … 英字終端）はパラメータごと読み飛ばす。
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                // その他のエスケープは次の 1 文字を読み飛ばす。
                chars.next();
            }
        } else if ch == '\n' || ch == '\t' || !ch.is_control() {
            out.push(ch);
        }
        // それ以外の制御文字（`\r` 等）は落とす。
    }
    out
}

/// mimetype または内容の NUL バイト有無からバイナリかどうかを判定する。
///
/// `reqwest` の `text()` は lossy な UTF-8 変換を行うため、NUL バイト（`\0`）は文字として
/// 残る。NUL を含めば無条件でバイナリ扱い。mimetype がある場合は既知のバイナリ種別
/// （image/audio/video/octet-stream/font/zip/pdf/wasm 等、テキスト種別を除く）ならバイナリ。
pub fn looks_binary(mimetype: Option<&str>, content: &str) -> bool {
    if content.contains('\0') {
        return true;
    }
    match mimetype {
        Some(mime) => {
            let mime = mime.to_ascii_lowercase();
            !is_textual_mime(&mime) && is_binary_mime(&mime)
        }
        None => false,
    }
}

/// テキストとして表示してよい mimetype か。
fn is_textual_mime(mime: &str) -> bool {
    mime.starts_with("text/")
        || mime.contains("json")
        || mime.contains("xml")
        || mime.contains("javascript")
        || mime.contains("ecmascript")
        || mime.contains("yaml")
        || mime.contains("toml")
        || mime.contains("x-sh")
        || mime.contains("x-www-form")
}

/// 既知のバイナリ mimetype か。
fn is_binary_mime(mime: &str) -> bool {
    mime.starts_with("image/")
        || mime.starts_with("audio/")
        || mime.starts_with("video/")
        || mime.starts_with("application/octet-stream")
        || mime.contains("font")
        || mime.contains("zip")
        || mime.contains("gzip")
        || mime.contains("pdf")
        || mime.contains("wasm")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_color_codes() {
        let input = "\x1b[31mred\x1b[0m normal";
        assert_eq!(strip_ansi(input), "red normal");
    }

    #[test]
    fn drops_carriage_returns_but_keeps_tabs() {
        assert_eq!(strip_ansi("a\r\n\tb"), "a\n\tb");
    }

    #[test]
    fn sanitize_splits_into_lines_without_trailing_blank() {
        let lines = sanitize_log("line1\nline2\n");
        assert_eq!(lines, vec!["line1".to_string(), "line2".to_string()]);
    }

    #[test]
    fn from_text_starts_at_top() {
        let view = LogView::from_text("{s}".to_string(), "t".to_string(), "a\nb\nc\n");
        assert_eq!(view.lines.len(), 3);
        assert_eq!(view.scroll, 0);
        assert!(!view.missing);
    }

    #[test]
    fn missing_marks_flag_and_placeholder() {
        let view = LogView::missing("{s}".to_string(), "t".to_string());
        assert!(view.missing);
        assert_eq!(view.lines, vec!["(ログなし)".to_string()]);
    }

    #[test]
    fn scroll_operations_respect_bounds() {
        let mut view = LogView::from_text("{s}".to_string(), "t".to_string(), "1\n2\n3\n4\n5\n");
        view.viewport = 2; // 5 行、表示 2 行 → 上限 3
        view.scroll_down(10);
        assert_eq!(view.scroll, 3);
        view.scroll_up(1);
        assert_eq!(view.scroll, 2);
        view.scroll_to_top();
        assert_eq!(view.scroll, 0);
        view.scroll_to_bottom();
        assert_eq!(view.scroll, 3);
    }

    #[test]
    fn looks_binary_detects_nul_and_mimetypes() {
        // NUL バイトは無条件でバイナリ。
        assert!(looks_binary(None, "hello\0world"));
        // 既知バイナリ mimetype。
        assert!(looks_binary(Some("image/png"), "not really png"));
        assert!(looks_binary(Some("application/octet-stream"), "abc"));
        // テキスト種別・NUL 無しはテキスト。
        assert!(!looks_binary(Some("text/x-rust"), "fn main() {}"));
        assert!(!looks_binary(Some("application/json"), "{}"));
        // mimetype 不明・NUL 無しはテキスト扱い（表示を試みる）。
        assert!(!looks_binary(None, "plain text"));
    }

    #[test]
    fn from_file_shows_placeholder_for_binary() {
        let view = LogView::from_file(
            "src/logo.png".to_string(),
            "src/logo.png".to_string(),
            Some("image/png"),
            "\0\0binary",
        );
        assert!(view.missing);
        assert_eq!(view.lines, vec!["(バイナリ表示不可)".to_string()]);
    }

    #[test]
    fn from_file_keeps_text_content() {
        let view = LogView::from_file(
            "a.rs".to_string(),
            "a.rs".to_string(),
            Some("text/x-rust"),
            "line1\nline2\n",
        );
        assert!(!view.missing);
        assert_eq!(view.lines, vec!["line1".to_string(), "line2".to_string()]);
    }

    #[test]
    fn from_file_truncates_large_content() {
        let content = "x\n".repeat(MAX_FILE_LINES + 10);
        let view = LogView::from_file(
            "big.txt".to_string(),
            "big.txt".to_string(),
            Some("text/plain"),
            &content,
        );
        // 先頭 MAX_FILE_LINES 行 + 打切り注記の 1 行。
        assert_eq!(view.lines.len(), MAX_FILE_LINES + 1);
        assert!(view.lines.last().expect("marker line").contains("省略"));
    }

    #[test]
    fn clamp_scroll_reduces_when_viewport_grows() {
        let mut view = LogView::from_text("{s}".to_string(), "t".to_string(), "1\n2\n3\n");
        view.viewport = 1;
        view.scroll_to_bottom();
        assert_eq!(view.scroll, 2);
        view.viewport = 10; // 全行が収まる → スクロール 0 へ
        view.clamp_scroll();
        assert_eq!(view.scroll, 0);
    }
}
