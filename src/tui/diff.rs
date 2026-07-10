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

/// パース済みの 1 ファイル区分（サイドバー表示・ファイル境界ジャンプ用）。
///
/// `file_starts` と同じ境界から導出するため要素数・並びが一致する
/// （`files[i].start == file_starts[i]`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFile {
    /// 表示用のファイル名（新側のパスを優先。抽出できない場合は `"(unknown)"`）。
    pub name: String,
    /// このファイルの最初の行インデックス（`lines` 内、境界行そのものを含む）。
    pub start: usize,
    /// このファイルの終端（次ファイルの `start`、末尾なら `lines.len()`）。
    pub end: usize,
}

/// ファイル名を抽出できなかった場合のプレースホルダ（renamed/binary 等の想定外形式でも
/// パニックせずに退避するため）。
const UNKNOWN_FILE_NAME: &str = "(unknown)";

/// パース済みの diff 全体。
#[derive(Debug, Clone, Default)]
pub struct ParsedDiff {
    pub lines: Vec<DiffLine>,
    /// ファイル区切り行（`diff --git` 等）の行インデックス（`n`/`N` ジャンプ用）。
    pub file_starts: Vec<usize>,
    /// ファイルごとの区分（名前・行範囲）。サイドバー描画・選択に使う。
    pub files: Vec<DiffFile>,
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
    let mut used_fallback = false;
    if file_starts.is_empty() {
        for (index, line) in lines.iter().enumerate() {
            if line.text.starts_with("--- ") {
                file_starts.push(index);
            }
        }
        used_fallback = !file_starts.is_empty();
    }

    let files = build_files(&lines, &file_starts, used_fallback);

    ParsedDiff {
        lines,
        file_starts,
        files,
    }
}

/// `file_starts` の各境界からファイル区分（名前・行範囲）を組み立てる。
fn build_files(lines: &[DiffLine], file_starts: &[usize], used_fallback: bool) -> Vec<DiffFile> {
    file_starts
        .iter()
        .enumerate()
        .map(|(index, &start)| {
            let end = file_starts.get(index + 1).copied().unwrap_or(lines.len());
            let name = if used_fallback {
                fallback_file_name(lines, start)
            } else {
                git_header_file_name(&lines[start].text)
            };
            DiffFile { name, start, end }
        })
        .collect()
}

/// `diff --git a/OLD b/NEW` からファイル名（新側 `NEW`）を取り出す。
///
/// パス中に空白を含む稀なケースでも「最後に現れる ` b/`」を境界とみなすベストエフォート
/// （抽出できなければ [`UNKNOWN_FILE_NAME`] に退避し、パニックしない）。
fn git_header_file_name(header: &str) -> String {
    header
        .rfind(" b/")
        .map(|index| &header[index + 3..])
        .filter(|name| !name.is_empty())
        .unwrap_or(UNKNOWN_FILE_NAME)
        .to_string()
}

/// `diff --git` ヘッダが無い形式でのファイル名抽出。
///
/// `--- ` 行の次行にある `+++ b/NEW` を優先し、無ければ `--- ` 行自体（`a/` 接頭辞を除く）に
/// フォールバックする。新規ファイル（`--- /dev/null`）等で抽出できなければ
/// [`UNKNOWN_FILE_NAME`] に退避する。
fn fallback_file_name(lines: &[DiffLine], start: usize) -> String {
    if let Some(next) = lines.get(start + 1)
        && let Some(path) = next.text.strip_prefix("+++ ")
    {
        let path = path.trim();
        if !path.is_empty() && path != "/dev/null" {
            return path.strip_prefix("b/").unwrap_or(path).to_string();
        }
    }

    let path = lines[start].text.strip_prefix("--- ").unwrap_or("").trim();
    if path.is_empty() || path == "/dev/null" {
        UNKNOWN_FILE_NAME.to_string()
    } else {
        path.strip_prefix("a/").unwrap_or(path).to_string()
    }
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

        // `files` は `file_starts` と同じ境界から導出され、要素数・start が一致する。
        assert_eq!(parsed.files.len(), 2);
        assert_eq!(parsed.files[0].name, "one.txt");
        assert_eq!(parsed.files[0].start, 0);
        assert_eq!(parsed.files[0].end, 8);
        assert_eq!(parsed.files[1].name, "two.txt");
        assert_eq!(parsed.files[1].start, 8);
        assert_eq!(parsed.files[1].end, parsed.len());
    }

    #[test]
    fn falls_back_to_minus_headers_when_no_git_header() {
        let text = "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n";
        let parsed = parse(text);
        assert_eq!(parsed.file_starts, vec![0]);
        assert_eq!(parsed.files.len(), 1);
        assert_eq!(parsed.files[0].name, "x");
        assert_eq!(parsed.files[0].start, 0);
        assert_eq!(parsed.files[0].end, parsed.len());
    }

    #[test]
    fn parse_empty_input_is_empty() {
        let parsed = parse("");
        assert!(parsed.is_empty());
        assert!(parsed.file_starts.is_empty());
        assert!(parsed.files.is_empty());
    }

    #[test]
    fn extracts_file_names_with_nested_paths_from_git_headers() {
        let text = "diff --git a/src/tui/app.rs b/src/tui/app.rs\n\
--- a/src/tui/app.rs\n\
+++ b/src/tui/app.rs\n\
@@ -1 +1 @@\n\
-old\n\
+new\n";
        let parsed = parse(text);
        assert_eq!(parsed.files.len(), 1);
        assert_eq!(parsed.files[0].name, "src/tui/app.rs");
    }

    #[test]
    fn extracts_file_names_with_nested_paths_from_fallback_headers() {
        let text = "--- a/src/tui/lib.rs\n+++ b/src/tui/lib.rs\n@@ -1 +1 @@\n-old\n+new\n";
        let parsed = parse(text);
        assert_eq!(parsed.files.len(), 1);
        assert_eq!(parsed.files[0].name, "src/tui/lib.rs");
    }

    #[test]
    fn single_file_diff_spans_from_start_to_end() {
        let text = "diff --git a/only.txt b/only.txt\n\
--- a/only.txt\n\
+++ b/only.txt\n\
@@ -1 +1 @@\n\
-a\n\
+b\n";
        let parsed = parse(text);
        assert_eq!(parsed.files.len(), 1);
        assert_eq!(parsed.files[0].start, 0);
        assert_eq!(parsed.files[0].end, parsed.len());
    }

    #[test]
    fn binary_file_diff_does_not_panic_and_names_the_file() {
        let text = "diff --git a/img.png b/img.png\n\
index 111..222 100644\n\
Binary files a/img.png and b/img.png differ\n";
        let parsed = parse(text);
        assert_eq!(parsed.files.len(), 1);
        assert_eq!(parsed.files[0].name, "img.png");
        assert_eq!(parsed.files[0].end, parsed.len());
    }

    #[test]
    fn renamed_file_diff_uses_new_path_from_git_header() {
        let text = "diff --git a/old.txt b/new.txt\n\
similarity index 100%\n\
rename from old.txt\n\
rename to new.txt\n";
        let parsed = parse(text);
        assert_eq!(parsed.files.len(), 1);
        assert_eq!(parsed.files[0].name, "new.txt");
    }

    #[test]
    fn no_boundary_detected_yields_no_files_without_panicking() {
        // `diff --git` も `--- ` も無い（想定外の）入力: ファイル境界は検出できないが
        // パニックはしない。
        let parsed = parse(" just some context\n+added\n-removed\n");
        assert!(parsed.file_starts.is_empty());
        assert!(parsed.files.is_empty());
    }
}
