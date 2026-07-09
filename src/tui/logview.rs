//! ステップログのスクロール表示状態と、ログテキストの整形。
//!
//! M1 の `DiffState` と同じスクロール操作（1 行 / 1 画面 / 先頭 / 末尾）を提供する汎用の
//! ログビューア。ログには ANSI エスケープや制御文字が混じり得るため、表示前に除去する。

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
