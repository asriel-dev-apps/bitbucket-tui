//! ユニファイド diff のパース（行種別の分類のみ）。
//!
//! syntect 等の追加クレートを使わず、行頭の記号で種別を判定する。**色の決定は行わない**
//! （`Color` への変換・テーマ適用は `ui` 側の責務。`tui::app::DiffState` の「状態層は
//! `Color::` を含まない」という分離を保つため、本モジュールも `ratatui::style` に依存しない）。

/// diff の 1 行の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    /// `diff --git a/… b/…`（ファイルの区切り）。
    FileHeader,
    /// `@@ -a,b +c,d @@`（ハンクヘッダ）。
    Hunk,
    /// 追加行（`+`）。
    Added,
    /// 削除行（`-`）。
    Removed,
    /// `index`/`+++`/`---`/`new file mode` などのメタ情報。
    Meta,
    /// 変更のない文脈行。
    Context,
}

impl DiffLineKind {
    /// diff の 1 行を種別へ分類する。
    ///
    /// `+++`/`---` はファイルパスヘッダなので、追加/削除より先に判定する。
    pub fn classify(line: &str) -> DiffLineKind {
        if line.starts_with("diff --git") {
            DiffLineKind::FileHeader
        } else if line.starts_with("@@") {
            DiffLineKind::Hunk
        } else if line.starts_with("+++") || line.starts_with("---") || is_meta_prefix(line) {
            DiffLineKind::Meta
        } else if line.starts_with('+') {
            DiffLineKind::Added
        } else if line.starts_with('-') {
            DiffLineKind::Removed
        } else if line.starts_with('\\') {
            // `\ No newline at end of file`
            DiffLineKind::Meta
        } else {
            DiffLineKind::Context
        }
    }
}

/// `git diff` が出力するファイルヘッダ系メタ行かどうか。
fn is_meta_prefix(line: &str) -> bool {
    const META_PREFIXES: [&str; 8] = [
        "index ",
        "new file mode",
        "deleted file mode",
        "old mode",
        "new mode",
        "similarity index",
        "rename ",
        "Binary files",
    ];
    META_PREFIXES.iter().any(|prefix| line.starts_with(prefix))
}

/// パース済みの diff の 1 行。
#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
}

/// パース済みの diff 全体。
#[derive(Debug, Clone, Default)]
pub struct ParsedDiff {
    pub lines: Vec<DiffLine>,
    /// ファイル区切り行（`diff --git` 等）の行インデックス（`n`/`N` ジャンプ用）。
    pub file_starts: Vec<usize>,
}

impl ParsedDiff {
    /// 総行数。
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// 空か。
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// ユニファイド diff テキストをパースする。
///
/// `str::lines()` を使うため末尾の余分な空行は生じない（`\n`/`\r\n` 双方に対応）。
pub fn parse(text: &str) -> ParsedDiff {
    let mut lines = Vec::new();
    let mut file_starts = Vec::new();

    for raw in text.lines() {
        let kind = DiffLineKind::classify(raw);
        if kind == DiffLineKind::FileHeader {
            file_starts.push(lines.len());
        }
        lines.push(DiffLine {
            kind,
            text: raw.to_string(),
        });
    }

    // `diff --git` ヘッダが無い形式（Bitbucket が素の `--- /+++ ` のみを返す等）への保険。
    if file_starts.is_empty() {
        for (index, line) in lines.iter().enumerate() {
            if line.text.starts_with("--- ") {
                file_starts.push(index);
            }
        }
    }

    ParsedDiff { lines, file_starts }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_line_kinds() {
        assert_eq!(
            DiffLineKind::classify("diff --git a/x b/x"),
            DiffLineKind::FileHeader
        );
        assert_eq!(
            DiffLineKind::classify("@@ -1,2 +1,3 @@"),
            DiffLineKind::Hunk
        );
        assert_eq!(DiffLineKind::classify("--- a/x"), DiffLineKind::Meta);
        assert_eq!(DiffLineKind::classify("+++ b/x"), DiffLineKind::Meta);
        assert_eq!(
            DiffLineKind::classify("index abc..def 100644"),
            DiffLineKind::Meta
        );
        assert_eq!(DiffLineKind::classify("+added"), DiffLineKind::Added);
        assert_eq!(DiffLineKind::classify("-removed"), DiffLineKind::Removed);
        assert_eq!(DiffLineKind::classify(" context"), DiffLineKind::Context);
        assert_eq!(
            DiffLineKind::classify("\\ No newline at end of file"),
            DiffLineKind::Meta
        );
    }

    #[test]
    fn parses_unified_diff_and_tracks_file_boundaries() {
        let text = "diff --git a/one.txt b/one.txt\n\
index 111..222 100644\n\
--- a/one.txt\n\
+++ b/one.txt\n\
@@ -1,2 +1,2 @@\n\
 context\n\
-old line\n\
+new line\n\
diff --git a/two.txt b/two.txt\n\
--- a/two.txt\n\
+++ b/two.txt\n\
@@ -0,0 +1 @@\n\
+brand new\n";
        let parsed = parse(text);
        assert!(!parsed.is_empty());
        assert_eq!(parsed.file_starts, vec![0, 8]);
        assert_eq!(parsed.lines[0].kind, DiffLineKind::FileHeader);
        assert_eq!(parsed.lines[6].kind, DiffLineKind::Removed);
        assert_eq!(parsed.lines[7].kind, DiffLineKind::Added);
        // 末尾に空行が付かないこと。
        assert_eq!(
            parsed.lines.last().map(|line| line.text.as_str()),
            Some("+brand new")
        );
    }

    #[test]
    fn falls_back_to_minus_headers_when_no_git_header() {
        let text = "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n";
        let parsed = parse(text);
        assert_eq!(parsed.file_starts, vec![0]);
    }

    #[test]
    fn parse_empty_input_is_empty() {
        let parsed = parse("");
        assert!(parsed.is_empty());
        assert!(parsed.file_starts.is_empty());
    }
}
