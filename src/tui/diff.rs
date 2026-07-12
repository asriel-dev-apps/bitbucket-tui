//! ユニファイド diff のパース（行種別の分類 + 行番号マッピング）。
//!
//! syntect 等の追加クレートを使わず、行頭の記号で種別を判定する。**色の決定は行わない**
//! （`Color` への変換・テーマ適用は `ui` 側の責務。`tui::app::DiffState` の「状態層は
//! `Color::` を含まない」という分離を保つため、本モジュールも `ratatui::style` に依存しない）。
//!
//! 各行には hunk ヘッダ（`@@ -a,b +c,d @@`）から算出した旧/新ファイルの行番号
//! （[`DiffLine::old_no`]/[`DiffLine::new_no`]）を付与する。インラインコメント投稿の
//! アンカー算出（[`ParsedDiff::comment_anchor`]）の土台になる。

use crate::api::models::CommentSide;

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
    /// 旧ファイル側の行番号（文脈行・削除行のみ `Some`。追加行/メタ/ヘッダ/ハンクは `None`）。
    pub old_no: Option<u32>,
    /// 新ファイル側の行番号（文脈行・追加行のみ `Some`。削除行/メタ/ヘッダ/ハンクは `None`）。
    pub new_no: Option<u32>,
}

/// diff 行 1 行分の「コメント可能アンカー」。
///
/// 追加/文脈行は新ファイル側（`to`）、削除行は旧ファイル側（`from`）を指す。
/// ファイルヘッダ/ハンクヘッダ/メタ行はアンカーを持たない（コメント不可）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentAnchor {
    pub path: String,
    pub side: CommentSide,
    pub line: u32,
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
    /// split 表示（左=旧ファイル/右=新ファイル）用の行ペア列。`parse` 時に一度だけ構築する
    /// （`DiffState::rendered_split` キャッシュの元データ。以後の再構築は行わない）。
    pub split_lines: Vec<SplitLine>,
    /// `file_starts` の各要素を `split_lines` 上のインデックスへ変換した並列配列
    /// （`file_starts[i]` のファイル境界が split 表示では `split_lines` の何行目に現れるか。
    /// `n`/`N` ファイルジャンプの split 版が使う）。
    pub split_file_starts: Vec<usize>,
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

    /// 指定行が属するファイルの表示名を返す（`files`/`file_starts` の境界から判定）。
    ///
    /// 範囲外や境界未検出（`files` が空）の場合は `None`。
    pub fn file_for_line(&self, index: usize) -> Option<&str> {
        self.files
            .iter()
            .find(|file| index >= file.start && index < file.end)
            .map(|file| file.name.as_str())
    }

    /// 指定行のインラインコメント投稿アンカーを算出する。
    ///
    /// 追加/文脈行は新ファイル側（`to`）、削除行は旧ファイル側（`from`）。
    /// ファイルヘッダ/ハンクヘッダ/メタ行、および行番号が確定できない行（不正な hunk
    /// ヘッダ等）は `None`（コメント不可）を返す。
    pub fn comment_anchor(&self, index: usize) -> Option<CommentAnchor> {
        let line = self.lines.get(index)?;
        let path = self.file_for_line(index)?.to_string();
        match line.kind {
            DiffLineKind::Added | DiffLineKind::Context => Some(CommentAnchor {
                path,
                side: CommentSide::To,
                line: line.new_no?,
            }),
            DiffLineKind::Removed => Some(CommentAnchor {
                path,
                side: CommentSide::From,
                line: line.old_no?,
            }),
            DiffLineKind::FileHeader | DiffLineKind::Hunk | DiffLineKind::Meta => None,
        }
    }

    /// split 表示の行インデックスからファイルの表示名を解決する（[`Self::file_for_line`] の
    /// split 版。[`SplitLine::anchor_index`] で unified 行インデックスへ変換してから委譲する）。
    pub fn split_file_for_line(&self, split_index: usize) -> Option<&str> {
        let anchor = self.split_lines.get(split_index)?.anchor_index()?;
        self.file_for_line(anchor)
    }

    /// split 表示の行インデックスからインラインコメント投稿アンカーを算出する
    /// （[`Self::comment_anchor`] の split 版）。「行ペアの新側があれば新側、無ければ
    /// 旧側」という規則は [`SplitLine::anchor_index`] が新側優先で unified 行インデックスへ
    /// 変換することで既存の `comment_anchor` にそのまま帰着する。
    pub fn split_comment_anchor(&self, split_index: usize) -> Option<CommentAnchor> {
        let anchor = self.split_lines.get(split_index)?.anchor_index()?;
        self.comment_anchor(anchor)
    }
}

/// split 表示（左=旧ファイル/右=新ファイル）用の 1 行ペア。
///
/// `left`/`right` は元の unified 行列（[`ParsedDiff::lines`]）のインデックス。存在しない側は
/// `None`（filler、空行として表示する）。文脈行・ファイルヘッダ/ハンクヘッダ/メタ行は
/// 左右とも同じインデックスを指す（同じ [`DiffLine`] を両側に表示する。メタ行は左右で内容を
/// 分けようがなく、文脈行は 1 つの `DiffLine` が旧/新両方の行番号を保持しているため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitLine {
    /// 旧ファイル側（左ペイン）に対応する [`ParsedDiff::lines`] のインデックス。
    pub left: Option<usize>,
    /// 新ファイル側（右ペイン）に対応する [`ParsedDiff::lines`] のインデックス。
    pub right: Option<usize>,
}

impl SplitLine {
    /// コメントアンカー算出・ファイル境界判定の基準にする unified 行インデックス。
    /// 新側（`right`）があればそちらを優先する（「追加/文脈行は新側、削除行は旧側」という
    /// [`ParsedDiff::comment_anchor`] の既存規則へ、変換なしにそのまま委譲するため）。
    fn anchor_index(&self) -> Option<usize> {
        self.right.or(self.left)
    }
}

/// unified diff の行列から split 表示用の行ペア列を作る純関数。
///
/// 仕様:
/// - 文脈行・ファイルヘッダ/ハンクヘッダ/メタ行は左右に同じ行を割り当てる（1 行 = 1 ペア）。
/// - 削除行の連続ブロックと、それに続く追加行の連続ブロックは、i 番目の削除行と i 番目の
///   追加行を同じペアにする（`git diff` は 1 箇所の変更を「削除ブロック→追加ブロック」の
///   順で出力するため、この隣接関係だけで対応付けが取れる）。
/// - 削除/追加の件数が異なる場合、多い方の余りは反対側を filler（`None`）にする。
/// - 対応する削除ブロックを伴わない追加行のみのブロック（純粋な新規行）は左を filler にする。
pub fn build_split_lines(lines: &[DiffLine]) -> Vec<SplitLine> {
    let mut result = Vec::with_capacity(lines.len());
    let mut index = 0;
    while index < lines.len() {
        match lines[index].kind {
            DiffLineKind::Removed => {
                let removed_start = index;
                let removed_end = consume_run(lines, removed_start, DiffLineKind::Removed);
                let added_start = removed_end;
                let added_end = consume_run(lines, added_start, DiffLineKind::Added);
                let removed_count = removed_end - removed_start;
                let added_count = added_end - added_start;
                for offset in 0..removed_count.max(added_count) {
                    result.push(SplitLine {
                        left: (offset < removed_count).then_some(removed_start + offset),
                        right: (offset < added_count).then_some(added_start + offset),
                    });
                }
                index = added_end;
            }
            DiffLineKind::Added => {
                // 直前に削除ブロックが無い（純粋な新規行の）追加ブロック。
                let added_start = index;
                let added_end = consume_run(lines, added_start, DiffLineKind::Added);
                for line_index in added_start..added_end {
                    result.push(SplitLine {
                        left: None,
                        right: Some(line_index),
                    });
                }
                index = added_end;
            }
            _ => {
                result.push(SplitLine {
                    left: Some(index),
                    right: Some(index),
                });
                index += 1;
            }
        }
    }
    result
}

/// `start` から同じ種別 `kind` が連続する区間の終端（排他的）インデックスを返す。
fn consume_run(lines: &[DiffLine], start: usize, kind: DiffLineKind) -> usize {
    let mut end = start;
    while end < lines.len() && lines[end].kind == kind {
        end += 1;
    }
    end
}

/// 各 unified 行インデックスが属する split 行インデックスへの逆引き表を作る
/// （`split_lines` を 1 回走査するだけの O(rows) 実装。`parse` から一度だけ呼ばれる）。
fn split_row_index_for_unified(split_lines: &[SplitLine], lines_len: usize) -> Vec<usize> {
    let mut index = vec![0usize; lines_len];
    for (row_index, row) in split_lines.iter().enumerate() {
        if let Some(left) = row.left {
            index[left] = row_index;
        }
        if let Some(right) = row.right {
            index[right] = row_index;
        }
    }
    index
}

/// ユニファイド diff テキストをパースする。
///
/// `str::lines()` を使うため末尾の余分な空行は生じない（`\n`/`\r\n` 双方に対応）。
///
/// 各行の旧/新ファイル行番号は hunk ヘッダ（`@@ -a,b +c,d @@`）から `old_cursor`/
/// `new_cursor` を初期化し、以降の文脈/追加/削除行ごとにインクリメントして求める
/// （ハンクヘッダが壊れていて解析できない場合はそのハンク区間の行番号を `None` のままにする。
/// 誤った番号を捏造してコメント先を誤らせないため）。
pub fn parse(text: &str) -> ParsedDiff {
    let mut lines = Vec::new();
    let mut file_starts = Vec::new();
    let mut old_cursor: Option<u32> = None;
    let mut new_cursor: Option<u32> = None;

    for raw in text.lines() {
        let kind = DiffLineKind::classify(raw);
        if kind == DiffLineKind::FileHeader {
            file_starts.push(lines.len());
        }

        let (old_no, new_no) = match kind {
            DiffLineKind::Hunk => {
                let starts = parse_hunk_header(raw);
                old_cursor = starts.map(|(old, _)| old);
                new_cursor = starts.map(|(_, new)| new);
                (None, None)
            }
            DiffLineKind::Context => {
                let assigned = (old_cursor, new_cursor);
                old_cursor = old_cursor.map(|value| value + 1);
                new_cursor = new_cursor.map(|value| value + 1);
                assigned
            }
            DiffLineKind::Added => {
                let assigned = (None, new_cursor);
                new_cursor = new_cursor.map(|value| value + 1);
                assigned
            }
            DiffLineKind::Removed => {
                let assigned = (old_cursor, None);
                old_cursor = old_cursor.map(|value| value + 1);
                assigned
            }
            DiffLineKind::FileHeader | DiffLineKind::Meta => (None, None),
        };

        lines.push(DiffLine {
            kind,
            text: raw.to_string(),
            old_no,
            new_no,
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

    let split_lines = build_split_lines(&lines);
    let unified_to_split = split_row_index_for_unified(&split_lines, lines.len());
    let split_file_starts: Vec<usize> = file_starts
        .iter()
        .map(|&start| unified_to_split.get(start).copied().unwrap_or(0))
        .collect();

    ParsedDiff {
        lines,
        file_starts,
        files,
        split_lines,
        split_file_starts,
    }
}

/// hunk ヘッダ `@@ -a[,b] +c[,d] @@ ...` から旧/新ファイルの開始行番号 `(a, c)` を取り出す。
///
/// `,b`/`,d`（行数）は省略され得る（1 行だけの hunk は `@@ -1 +1 @@` のように書かれる）ため、
/// 開始行番号のみを見る。パースに失敗した場合（想定外の形式）は `None` を返し、呼び出し側
/// （`parse`）はそのハンク区間の行番号を捏造せず `None` のままにする。
fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    let mut parts = line.split_whitespace();
    parts.next().filter(|token| *token == "@@")?;
    let old_part = parts.next()?;
    let new_part = parts.next()?;
    let old_start = parse_hunk_start(old_part, '-')?;
    let new_start = parse_hunk_start(new_part, '+')?;
    Some((old_start, new_start))
}

/// hunk ヘッダの `-a,b`/`+c,d`（または `-a`/`+c`）トークンから開始行番号のみを取り出す。
fn parse_hunk_start(token: &str, prefix: char) -> Option<u32> {
    let stripped = token.strip_prefix(prefix)?;
    let start = stripped.split(',').next()?;
    start.parse::<u32>().ok()
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

    // ---- 行番号マッピング（hunk ヘッダ由来の old_no/new_no） ----

    #[test]
    fn hunk_assigns_old_and_new_line_numbers_to_context_added_removed() {
        let text = "diff --git a/x b/x\n\
--- a/x\n\
+++ b/x\n\
@@ -10,3 +20,4 @@\n\
 context a\n\
-removed b\n\
+added c\n\
+added d\n";
        let parsed = parse(text);

        // インデックス: 0 diff--git / 1 --- / 2 +++ / 3 @@ / 4 context / 5 removed / 6,7 added。
        assert_eq!(parsed.lines[4].old_no, Some(10));
        assert_eq!(parsed.lines[4].new_no, Some(20));
        assert_eq!(parsed.lines[5].old_no, Some(11));
        assert_eq!(parsed.lines[5].new_no, None);
        assert_eq!(parsed.lines[6].old_no, None);
        assert_eq!(parsed.lines[6].new_no, Some(21));
        assert_eq!(parsed.lines[7].old_no, None);
        assert_eq!(parsed.lines[7].new_no, Some(22));
    }

    #[test]
    fn multiple_hunks_in_same_file_reset_line_number_cursors() {
        let text = "diff --git a/x b/x\n\
--- a/x\n\
+++ b/x\n\
@@ -1,2 +1,2 @@\n\
 a\n\
-b\n\
@@ -10,1 +9,2 @@\n\
 c\n\
+d\n";
        let parsed = parse(text);

        // インデックス: 0 diff--git / 1 --- / 2 +++ / 3 @@(1) / 4 a / 5 -b / 6 @@(2) / 7 c / 8 +d。
        assert_eq!(parsed.lines[4].old_no, Some(1));
        assert_eq!(parsed.lines[4].new_no, Some(1));
        assert_eq!(parsed.lines[5].old_no, Some(2));
        // 2 つ目のハンクヘッダで old/new カーソルが再設定される（1 つ目の続きにならない）。
        assert_eq!(parsed.lines[7].old_no, Some(10));
        assert_eq!(parsed.lines[7].new_no, Some(9));
        assert_eq!(parsed.lines[8].old_no, None);
        assert_eq!(parsed.lines[8].new_no, Some(10));
    }

    #[test]
    fn multiple_files_each_number_lines_from_their_own_hunk() {
        let text = "diff --git a/one.txt b/one.txt\n\
--- a/one.txt\n\
+++ b/one.txt\n\
@@ -1,1 +1,1 @@\n\
-old one\n\
+new one\n\
diff --git a/two.txt b/two.txt\n\
--- a/two.txt\n\
+++ b/two.txt\n\
@@ -1,1 +1,1 @@\n\
-old two\n\
+new two\n";
        let parsed = parse(text);

        // ファイル境界を跨いでも 2 つ目のファイルの行番号は 1 から始まる（1 つ目の
        // カーソルが漏れ出さない）。
        assert_eq!(parsed.lines[4].old_no, Some(1)); // "-old one"
        assert_eq!(parsed.lines[5].new_no, Some(1)); // "+new one"
        assert_eq!(parsed.lines[10].old_no, Some(1)); // "-old two"
        assert_eq!(parsed.lines[11].new_no, Some(1)); // "+new two"
    }

    #[test]
    fn renamed_file_without_hunk_has_no_line_numbers() {
        let text = "diff --git a/old.txt b/new.txt\n\
similarity index 100%\n\
rename from old.txt\n\
rename to new.txt\n";
        let parsed = parse(text);
        assert!(
            parsed
                .lines
                .iter()
                .all(|line| line.old_no.is_none() && line.new_no.is_none())
        );
    }

    #[test]
    fn malformed_hunk_header_leaves_line_numbers_none_without_panicking() {
        let text = "diff --git a/x b/x\n@@ garbage @@\n context\n-removed\n+added\n";
        let parsed = parse(text);
        assert!(
            parsed
                .lines
                .iter()
                .all(|line| line.old_no.is_none() && line.new_no.is_none())
        );
    }

    #[test]
    fn single_line_hunk_header_without_comma_counts_parses() {
        // `@@ -1 +1 @@`（1 行だけの hunk はカンマ無しの行数省略形になる）。
        let text = "diff --git a/x b/x\n@@ -1 +1 @@\n-old\n+new\n";
        let parsed = parse(text);
        assert_eq!(parsed.lines[2].old_no, Some(1));
        assert_eq!(parsed.lines[3].new_no, Some(1));
    }

    // ---- コメントアンカー算出 ----

    fn sample_diff_for_anchor() -> ParsedDiff {
        let text = "diff --git a/x b/x\n\
--- a/x\n\
+++ b/x\n\
@@ -10,3 +20,4 @@\n\
 context a\n\
-removed b\n\
+added c\n\
+added d\n";
        parse(text)
    }

    #[test]
    fn comment_anchor_for_context_line_uses_new_side() {
        let parsed = sample_diff_for_anchor();
        let anchor = parsed.comment_anchor(4).expect("context line has anchor");
        assert_eq!(anchor.path, "x");
        assert_eq!(anchor.side, CommentSide::To);
        assert_eq!(anchor.line, 20);
    }

    #[test]
    fn comment_anchor_for_removed_line_uses_old_side() {
        let parsed = sample_diff_for_anchor();
        let anchor = parsed.comment_anchor(5).expect("removed line has anchor");
        assert_eq!(anchor.path, "x");
        assert_eq!(anchor.side, CommentSide::From);
        assert_eq!(anchor.line, 11);
    }

    #[test]
    fn comment_anchor_for_added_line_uses_new_side() {
        let parsed = sample_diff_for_anchor();
        let anchor = parsed.comment_anchor(6).expect("added line has anchor");
        assert_eq!(anchor.path, "x");
        assert_eq!(anchor.side, CommentSide::To);
        assert_eq!(anchor.line, 21);
    }

    #[test]
    fn comment_anchor_is_none_for_meta_header_and_hunk_lines() {
        let parsed = sample_diff_for_anchor();
        assert_eq!(parsed.comment_anchor(0), None); // diff --git（ファイルヘッダ）
        assert_eq!(parsed.comment_anchor(1), None); // --- a/x（メタ）
        assert_eq!(parsed.comment_anchor(2), None); // +++ b/x（メタ）
        assert_eq!(parsed.comment_anchor(3), None); // @@ ...（ハンク）
    }

    #[test]
    fn comment_anchor_out_of_range_index_is_none() {
        let parsed = sample_diff_for_anchor();
        assert_eq!(parsed.comment_anchor(9999), None);
    }

    #[test]
    fn file_for_line_resolves_path_across_multiple_files() {
        let parsed = multiple_files_diff_for_file_lookup();
        assert_eq!(parsed.file_for_line(0), Some("one.txt"));
        assert_eq!(parsed.file_for_line(5), Some("one.txt"));
        assert_eq!(parsed.file_for_line(6), Some("two.txt"));
        assert_eq!(parsed.file_for_line(parsed.len() - 1), Some("two.txt"));
        assert_eq!(parsed.file_for_line(parsed.len()), None);
    }

    fn multiple_files_diff_for_file_lookup() -> ParsedDiff {
        let text = "diff --git a/one.txt b/one.txt\n\
--- a/one.txt\n\
+++ b/one.txt\n\
@@ -1,1 +1,1 @@\n\
-old one\n\
+new one\n\
diff --git a/two.txt b/two.txt\n\
--- a/two.txt\n\
+++ b/two.txt\n\
@@ -1,1 +1,1 @@\n\
-old two\n\
+new two\n";
        parse(text)
    }

    // ---- split 表示用の行ペアリング（build_split_lines） ----

    /// テスト用の最小 `DiffLine`（行番号は使わないテストでは `None` のまま）。
    fn diff_line(kind: DiffLineKind, text: &str) -> DiffLine {
        DiffLine {
            kind,
            text: text.to_string(),
            old_no: None,
            new_no: None,
        }
    }

    #[test]
    fn build_split_lines_context_row_maps_both_sides_to_same_index() {
        let lines = vec![diff_line(DiffLineKind::Context, " ctx")];
        let split = build_split_lines(&lines);
        assert_eq!(
            split,
            vec![SplitLine {
                left: Some(0),
                right: Some(0)
            }]
        );
    }

    #[test]
    fn build_split_lines_meta_and_header_rows_map_both_sides_to_same_index() {
        let lines = vec![
            diff_line(DiffLineKind::FileHeader, "diff --git a/x b/x"),
            diff_line(DiffLineKind::Meta, "index 111..222 100644"),
            diff_line(DiffLineKind::Hunk, "@@ -1,1 +1,1 @@"),
        ];
        let split = build_split_lines(&lines);
        assert_eq!(
            split,
            vec![
                SplitLine {
                    left: Some(0),
                    right: Some(0)
                },
                SplitLine {
                    left: Some(1),
                    right: Some(1)
                },
                SplitLine {
                    left: Some(2),
                    right: Some(2)
                },
            ]
        );
    }

    #[test]
    fn build_split_lines_pairs_removed_and_added_blocks_by_offset() {
        let lines = vec![
            diff_line(DiffLineKind::Removed, "-a"),
            diff_line(DiffLineKind::Removed, "-b"),
            diff_line(DiffLineKind::Added, "+a2"),
            diff_line(DiffLineKind::Added, "+b2"),
        ];
        let split = build_split_lines(&lines);
        assert_eq!(
            split,
            vec![
                SplitLine {
                    left: Some(0),
                    right: Some(2)
                },
                SplitLine {
                    left: Some(1),
                    right: Some(3)
                },
            ]
        );
    }

    #[test]
    fn build_split_lines_fills_right_side_when_removed_block_is_longer() {
        let lines = vec![
            diff_line(DiffLineKind::Removed, "-a"),
            diff_line(DiffLineKind::Removed, "-b"),
            diff_line(DiffLineKind::Removed, "-c"),
            diff_line(DiffLineKind::Added, "+a2"),
        ];
        let split = build_split_lines(&lines);
        assert_eq!(
            split,
            vec![
                SplitLine {
                    left: Some(0),
                    right: Some(3)
                },
                SplitLine {
                    left: Some(1),
                    right: None
                },
                SplitLine {
                    left: Some(2),
                    right: None
                },
            ]
        );
    }

    #[test]
    fn build_split_lines_fills_left_side_when_added_block_is_longer() {
        let lines = vec![
            diff_line(DiffLineKind::Removed, "-a"),
            diff_line(DiffLineKind::Added, "+a2"),
            diff_line(DiffLineKind::Added, "+b2"),
        ];
        let split = build_split_lines(&lines);
        assert_eq!(
            split,
            vec![
                SplitLine {
                    left: Some(0),
                    right: Some(1)
                },
                SplitLine {
                    left: None,
                    right: Some(2)
                },
            ]
        );
    }

    #[test]
    fn build_split_lines_pure_addition_block_without_preceding_removal_fills_left() {
        let lines = vec![
            diff_line(DiffLineKind::Context, " ctx"),
            diff_line(DiffLineKind::Added, "+new"),
        ];
        let split = build_split_lines(&lines);
        assert_eq!(
            split,
            vec![
                SplitLine {
                    left: Some(0),
                    right: Some(0)
                },
                SplitLine {
                    left: None,
                    right: Some(1)
                },
            ]
        );
    }

    #[test]
    fn split_lines_span_multiple_hunks_within_same_file() {
        let text = "diff --git a/x b/x\n\
--- a/x\n\
+++ b/x\n\
@@ -1,1 +1,1 @@\n\
-old1\n\
+new1\n\
@@ -10,1 +9,1 @@\n\
-old2\n\
+new2\n";
        let parsed = parse(text);
        // インデックス: 0 diff--git/1 ---/2 +++/3 @@(1)/4 -old1/5 +new1/6 @@(2)/7 -old2/8 +new2。
        let paired: Vec<&SplitLine> = parsed
            .split_lines
            .iter()
            .filter(|row| row.left != row.right)
            .collect();
        assert_eq!(paired.len(), 2);
        assert_eq!(paired[0].left, Some(4));
        assert_eq!(paired[0].right, Some(5));
        assert_eq!(paired[1].left, Some(7));
        assert_eq!(paired[1].right, Some(8));
    }

    #[test]
    fn split_file_starts_map_file_boundaries_across_multiple_files() {
        let parsed = multiple_files_diff_for_file_lookup();
        assert_eq!(parsed.split_file_starts.len(), parsed.file_starts.len());

        let first_file_row = parsed.split_file_starts[0];
        let second_file_row = parsed.split_file_starts[1];
        assert_eq!(parsed.split_file_for_line(first_file_row), Some("one.txt"));
        assert_eq!(parsed.split_file_for_line(second_file_row), Some("two.txt"));
    }

    /// 逆引き（split 行 → unified 行インデックス）: 各 unified 行はちょうど 1 つの split 行
    /// （の左または右）にのみ現れること（取りこぼし・重複が無いこと）を確認する。
    #[test]
    fn build_split_lines_every_unified_index_maps_back_from_exactly_one_split_row() {
        let text = "diff --git a/x b/x\n\
--- a/x\n\
+++ b/x\n\
@@ -1,3 +1,4 @@\n\
 context a\n\
-removed b\n\
+added c\n\
+added d\n";
        let parsed = parse(text);
        let mut seen = vec![false; parsed.lines.len()];
        for row in &parsed.split_lines {
            if let Some(left) = row.left {
                assert!(!seen[left], "unified index {left} counted twice (left)");
                seen[left] = true;
            }
            if let Some(right) = row.right
                && row.left != Some(right)
            {
                assert!(!seen[right], "unified index {right} counted twice (right)");
                seen[right] = true;
            }
        }
        assert!(
            seen.iter().all(|&marked| marked),
            "every unified line must map back from exactly one split row"
        );
    }

    // ---- split 表示のコメントアンカー / ファイル名解決 ----

    fn sample_diff_for_split_anchor() -> ParsedDiff {
        let text = "diff --git a/x b/x\n\
--- a/x\n\
+++ b/x\n\
@@ -10,3 +20,4 @@\n\
 context a\n\
-removed b\n\
+added c\n\
+added d\n";
        parse(text)
    }

    #[test]
    fn split_comment_anchor_uses_new_side_when_right_is_present() {
        let parsed = sample_diff_for_split_anchor();
        // split_lines[5] は「-removed b」と「+added c」のペア行（右側優先）。
        let anchor = parsed
            .split_comment_anchor(5)
            .expect("paired row has anchor");
        assert_eq!(anchor.side, CommentSide::To);
        assert_eq!(anchor.line, 21);
    }

    #[test]
    fn split_comment_anchor_falls_back_to_old_side_when_right_is_filler() {
        // 追加ブロックを伴わない削除のみのケース（右側が filler）。
        let text = "diff --git a/x b/x\n@@ -1,1 +0,0 @@\n-removed only\n";
        let parsed = parse(text);
        let anchor = parsed
            .split_comment_anchor(2)
            .expect("removed-only row has anchor");
        assert_eq!(anchor.side, CommentSide::From);
        assert_eq!(anchor.line, 1);
    }

    #[test]
    fn split_comment_anchor_out_of_range_index_is_none() {
        let parsed = sample_diff_for_split_anchor();
        assert_eq!(parsed.split_comment_anchor(9999), None);
    }

    #[test]
    fn split_file_for_line_out_of_range_index_is_none() {
        let parsed = sample_diff_for_split_anchor();
        assert_eq!(parsed.split_file_for_line(9999), None);
    }
}
