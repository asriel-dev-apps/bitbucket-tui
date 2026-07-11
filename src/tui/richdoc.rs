//! PR 概要ペイン向けの Markdown レイアウト。
//!
//! Markdown の画像記法をコードフェンス外だけブロックへ分離し、テキストは描画前に表示幅へ
//! 折り返す。描画側はここで確定した行と画像高を順に配置するだけでよく、スクロール上限と
//! 実際の表示位置がずれない。

use std::ops::Range;

use image::DynamicImage;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Bitbucket 添付画像が API token では取得できない場合の 5.5 共通文言。
pub const BITBUCKET_ATTACHMENT_PLACEHOLDER: &str =
    "この画像（Bitbucket 添付）は API token では取得できません。o でブラウザ表示してください";

/// 概要文書を構成する描画ブロック。
#[derive(Debug, Clone, PartialEq)]
pub enum DocBlock {
    /// Markdown 描画・折り返し済みの視覚行。
    Text(Vec<Line<'static>>),
    /// コードフェンス外にあった Markdown 画像。
    Image { alt: String, url: String },
}

/// 概要内リンクのヒット領域。`urls` が複数なら将来の 5.4 でリンクパレットを開ける。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkPosition {
    pub visual_line: usize,
    pub column_range: Range<u16>,
    pub urls: Vec<String>,
}

/// 画像ブロックのセル単位サイズ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSize {
    pub width: u16,
    pub height: u16,
}

/// 画像ブロックのレイアウト結果。`size == None` は 1 行プレースホルダを表す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePresentation {
    pub size: Option<ImageSize>,
    pub placeholder: String,
}

impl ImagePresentation {
    fn height(&self) -> usize {
        self.size.map_or(1, |size| usize::from(size.height))
    }
}

/// 描画位置まで確定した概要文書。
#[derive(Debug, Clone, PartialEq)]
pub struct RichDocument {
    pub blocks: Vec<DocBlock>,
    pub links: Vec<LinkPosition>,
    pub height: usize,
    block_heights: Vec<usize>,
    image_presentations: Vec<Option<ImagePresentation>>,
}

impl RichDocument {
    pub fn block_height(&self, index: usize) -> usize {
        self.block_heights.get(index).copied().unwrap_or(0)
    }

    pub fn image_presentation(&self, index: usize) -> Option<&ImagePresentation> {
        self.image_presentations.get(index).and_then(Option::as_ref)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SourceBlock {
    Text(String),
    Image { alt: String, url: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceLink {
    label: String,
    url: String,
    source_line: usize,
}

/// ヘッダと Markdown 本文から、指定幅の仮想文書を作る。
pub fn build_document<F>(
    leading_lines: Vec<Line<'static>>,
    body: &str,
    width: u16,
    mut image_presentation: F,
) -> RichDocument
where
    F: FnMut(&str, &str) -> ImagePresentation,
{
    let mut blocks = Vec::new();
    let mut links = Vec::new();
    let mut block_heights = Vec::new();
    let mut image_presentations = Vec::new();
    let mut height = 0usize;

    let leading = wrap_lines(&leading_lines, width);
    height += leading.len();
    block_heights.push(leading.len());
    image_presentations.push(None);
    blocks.push(DocBlock::Text(leading));

    for source in split_source_blocks(body) {
        match source {
            SourceBlock::Text(markdown) => {
                let rendered = markdown_to_lines(&markdown);
                let wrapped = wrap_lines(&rendered, width);
                let source_links = extract_source_links(&markdown);
                links.extend(position_links(
                    &source_links,
                    &wrapped,
                    &markdown,
                    height,
                    width,
                ));
                height += wrapped.len();
                block_heights.push(wrapped.len());
                image_presentations.push(None);
                blocks.push(DocBlock::Text(wrapped));
            }
            SourceBlock::Image { alt, url } => {
                let presentation = image_presentation(&alt, &url);
                let block_height = presentation.height();
                height += block_height;
                block_heights.push(block_height);
                image_presentations.push(Some(presentation));
                blocks.push(DocBlock::Image { alt, url });
            }
        }
    }

    RichDocument {
        blocks,
        links,
        height,
        block_heights,
        image_presentations,
    }
}

/// 画像の画素寸法と端末フォント寸法から、自然サイズを超えないセル寸法を求める。
/// 画像・picker・表示幅のいずれかが無ければプレースホルダになる。
pub fn image_presentation(
    alt: &str,
    result: Option<&Result<DynamicImage, String>>,
    font_size: Option<(u16, u16)>,
    pane_width: u16,
) -> ImagePresentation {
    let placeholder = match result {
        Some(Err(message)) if message == BITBUCKET_ATTACHMENT_PLACEHOLDER => message.clone(),
        Some(Err(_)) => format!("[画像: {alt}]（取得失敗: o でブラウザ）"),
        Some(Ok(_)) if font_size.is_none() => {
            format!("[画像: {alt}]（この端末は画像表示に未対応です: o でブラウザ）")
        }
        _ => format!("[画像: {alt}]（読み込み中… / i で表示 / o でブラウザ）"),
    };

    let size = result
        .and_then(|result| result.as_ref().ok())
        .zip(font_size)
        .and_then(|(image, font_size)| image_size(image, font_size, pane_width));
    ImagePresentation { size, placeholder }
}

fn image_size(image: &DynamicImage, font_size: (u16, u16), pane_width: u16) -> Option<ImageSize> {
    let (font_width, font_height) = font_size;
    if image.width() == 0
        || image.height() == 0
        || font_width == 0
        || font_height == 0
        || pane_width == 0
    {
        return None;
    }

    let natural_width = ceil_div_u64(u64::from(image.width()), u64::from(font_width))
        .min(u64::from(u16::MAX)) as u16;
    let width = natural_width.max(1).min(pane_width);
    let height_numerator = u64::from(image.height())
        .saturating_mul(u64::from(width))
        .saturating_mul(u64::from(font_width));
    let height_denominator = u64::from(image.width()).saturating_mul(u64::from(font_height));
    let height = ceil_div_u64(height_numerator, height_denominator)
        .clamp(1, 20)
        .min(u64::from(u16::MAX)) as u16;
    Some(ImageSize { width, height })
}

fn ceil_div_u64(numerator: u64, denominator: u64) -> u64 {
    if denominator == 0 {
        return 0;
    }
    numerator / denominator + u64::from(!numerator.is_multiple_of(denominator))
}

/// 仮想高さと viewport から有効なスクロール位置へクランプする。
pub fn clamp_scroll(scroll: u16, document_height: usize, viewport: usize) -> u16 {
    let max = document_height
        .saturating_sub(viewport.max(1))
        .min(u16::MAX as usize) as u16;
    scroll.min(max)
}

/// `replace_image_syntax_outside_code_fences` と同じフェンス規則・画像記法走査で分割する。
fn split_source_blocks(body: &str) -> Vec<SourceBlock> {
    let mut blocks = Vec::new();
    let mut plain_lines = Vec::new();
    let mut in_code_block = false;

    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            plain_lines.push(line.to_string());
            in_code_block = !in_code_block;
        } else if in_code_block {
            plain_lines.push(line.to_string());
        } else {
            let line_blocks = split_image_syntax(line);
            let contains_image = line_blocks
                .iter()
                .any(|block| matches!(block, SourceBlock::Image { .. }));
            if contains_image {
                flush_plain_lines(&mut plain_lines, &mut blocks);
                blocks.extend(line_blocks);
            } else {
                plain_lines.push(line.to_string());
            }
        }
    }
    flush_plain_lines(&mut plain_lines, &mut blocks);
    blocks
}

fn flush_plain_lines(lines: &mut Vec<String>, blocks: &mut Vec<SourceBlock>) {
    if !lines.is_empty() {
        blocks.push(SourceBlock::Text(lines.join("\n")));
        lines.clear();
    }
}

/// `ui::replace_image_syntax` と同じ簡易パーサで 1 行を分割する。
fn split_image_syntax(line: &str) -> Vec<SourceBlock> {
    let mut blocks = Vec::new();
    let mut text = String::new();
    let mut rest = line;
    while let Some(start) = rest.find("![") {
        text.push_str(&rest[..start]);
        let after_bang = &rest[start + 2..];
        let Some(close_bracket) = after_bang.find(']') else {
            text.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let alt = &after_bang[..close_bracket];
        let after_alt = &after_bang[close_bracket + 1..];
        match after_alt
            .strip_prefix('(')
            .and_then(|paren_rest| paren_rest.find(')').map(|end| (paren_rest, end)))
        {
            Some((paren_rest, close_paren)) => {
                if !text.is_empty() {
                    blocks.push(SourceBlock::Text(std::mem::take(&mut text)));
                }
                blocks.push(SourceBlock::Image {
                    alt: alt.to_string(),
                    url: paren_rest[..close_paren].to_string(),
                });
                rest = &paren_rest[close_paren + 1..];
            }
            None => {
                text.push_str("![");
                rest = &rest[start + 2..];
            }
        }
    }
    text.push_str(rest);
    if !text.is_empty() || blocks.is_empty() {
        blocks.push(SourceBlock::Text(text));
    }
    blocks
}

/// `tui_markdown` の Text を、このクレートが使う ratatui の行へ変換する。
pub fn markdown_to_lines(body: &str) -> Vec<Line<'static>> {
    let source = tui_markdown::from_str(body);
    source
        .lines
        .into_iter()
        .map(|line| {
            let line_style = convert_markdown_style(
                line.style.fg,
                line.style.bg,
                line.style.add_modifier,
                line.style.sub_modifier,
            );
            let spans = line
                .spans
                .into_iter()
                .map(|span| {
                    let style = convert_markdown_style(
                        span.style.fg,
                        span.style.bg,
                        span.style.add_modifier,
                        span.style.sub_modifier,
                    );
                    Span::styled(span.content.into_owned(), style)
                })
                .collect::<Vec<_>>();
            Line::from(spans).style(line_style)
        })
        .collect()
}

fn convert_markdown_style<C, M>(
    fg: Option<C>,
    bg: Option<C>,
    add_modifier: M,
    sub_modifier: M,
) -> Style
where
    C: std::fmt::Display,
    M: std::fmt::Binary,
{
    Style {
        fg: fg.map(convert_markdown_color),
        bg: bg.map(convert_markdown_color),
        add_modifier: convert_markdown_modifier(add_modifier),
        sub_modifier: convert_markdown_modifier(sub_modifier),
        ..Style::default()
    }
}

fn convert_markdown_color<C: std::fmt::Display>(color: C) -> Color {
    color.to_string().parse().unwrap_or(Color::Reset)
}

fn convert_markdown_modifier<M: std::fmt::Binary>(modifier: M) -> Modifier {
    let bits = u16::from_str_radix(&format!("{modifier:b}"), 2).unwrap_or(0);
    Modifier::from_bits_truncate(bits)
}

#[derive(Debug, Clone)]
struct GraphemeToken {
    text: String,
    width: usize,
    style: Style,
    span_id: usize,
    whitespace: bool,
}

/// Span 境界と style を保持したまま、行を表示セル幅へ greedy wrap する。
pub fn wrap_lines(lines: &[Line<'static>], width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut output = Vec::new();
    for line in lines {
        let tokens = line_tokens(line);
        if tokens.is_empty() {
            output.push(empty_line_like(line));
            continue;
        }

        let mut pending = tokens.as_slice();
        while !pending.is_empty() {
            let consumed = wrapped_token_count(pending, width)
                .max(1)
                .min(pending.len());
            output.push(tokens_to_line(&pending[..consumed], line));
            pending = &pending[consumed..];
        }
    }
    output
}

fn line_tokens(line: &Line<'static>) -> Vec<GraphemeToken> {
    let mut tokens = Vec::new();
    for (span_id, span) in line.spans.iter().enumerate() {
        for grapheme in span.content.graphemes(true) {
            tokens.push(GraphemeToken {
                text: grapheme.to_string(),
                width: UnicodeWidthStr::width(grapheme),
                style: span.style,
                span_id,
                whitespace: grapheme.chars().all(char::is_whitespace),
            });
        }
    }
    tokens
}

fn wrapped_token_count(tokens: &[GraphemeToken], width: usize) -> usize {
    let mut used = 0usize;
    let mut last_break = None;
    for (index, token) in tokens.iter().enumerate() {
        if used.saturating_add(token.width) > width && index > 0 {
            let take = last_break.filter(|break_at| *break_at > 0).unwrap_or(index);
            return take;
        }
        used = used.saturating_add(token.width);
        if token.whitespace {
            last_break = Some(index + 1);
        }
        if used > width {
            return index + 1;
        }
    }
    tokens.len()
}

fn tokens_to_line(tokens: &[GraphemeToken], template: &Line<'static>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut current_span = None;
    for token in tokens {
        if current_span == Some(token.span_id) {
            if let Some(span) = spans.last_mut() {
                span.content.to_mut().push_str(&token.text);
            }
        } else {
            spans.push(Span::styled(token.text.clone(), token.style));
            current_span = Some(token.span_id);
        }
    }
    let mut line = Line::from(spans).style(template.style);
    line.alignment = template.alignment;
    line
}

fn empty_line_like(template: &Line<'static>) -> Line<'static> {
    let mut line = Line::default().style(template.style);
    line.alignment = template.alignment;
    line
}

fn extract_source_links(markdown: &str) -> Vec<SourceLink> {
    let mut links = Vec::new();
    let mut in_fence = false;
    for (source_line, line) in markdown.lines().enumerate() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }

        let mut masked = line.to_string();
        let mut cursor = 0;
        while let Some(open_relative) = line[cursor..].find('[') {
            let open = cursor + open_relative;
            let is_image = open > 0 && line.as_bytes().get(open - 1) == Some(&b'!');
            let Some(close_relative) = line[open + 1..].find("](") else {
                cursor = open + 1;
                continue;
            };
            let close = open + 1 + close_relative;
            let url_start = close + 2;
            let Some(end_relative) = line[url_start..].find(')') else {
                cursor = url_start;
                continue;
            };
            let end = url_start + end_relative;
            let url = &line[url_start..end];
            if !is_image && is_http_url(url) {
                links.push(SourceLink {
                    label: line[open + 1..close].to_string(),
                    url: url.to_string(),
                    source_line,
                });
            }
            let mask_start = if is_image { open - 1 } else { open };
            masked.replace_range(mask_start..=end, &" ".repeat(end + 1 - mask_start));
            cursor = end + 1;
        }

        for token in masked.split_whitespace() {
            let Some(start) = token.find("http://").or_else(|| token.find("https://")) else {
                continue;
            };
            let url = token[start..].trim_end_matches(|character: char| {
                matches!(
                    character,
                    '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}'
                )
            });
            if is_http_url(url) {
                links.push(SourceLink {
                    label: url.to_string(),
                    url: url.to_string(),
                    source_line,
                });
            }
        }
    }
    links
}

fn is_http_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

fn position_links(
    source_links: &[SourceLink],
    lines: &[Line<'static>],
    markdown: &str,
    line_offset: usize,
    width: u16,
) -> Vec<LinkPosition> {
    let rendered = lines.iter().map(line_text).collect::<Vec<_>>();
    let mut positions = Vec::new();
    let mut search_line = 0usize;
    let mut search_byte = 0usize;
    for link in source_links {
        if let Some((line_index, byte_start)) =
            find_link_text(&rendered, &link.label, search_line, search_byte)
        {
            let text = &rendered[line_index];
            let start =
                UnicodeWidthStr::width(&text[..byte_start]).min(usize::from(u16::MAX)) as u16;
            let link_width =
                UnicodeWidthStr::width(link.label.as_str()).min(usize::from(u16::MAX)) as u16;
            positions.push(LinkPosition {
                visual_line: line_offset + line_index,
                column_range: start..start.saturating_add(link_width),
                urls: vec![link.url.clone()],
            });
            search_line = line_index;
            search_byte = byte_start.saturating_add(link.label.len());
            continue;
        }

        // Markdown 変換や wrap でリンク文字列が複数行へ分断された場合の仕様許容 fallback。
        let fallback_line = fallback_visual_line(markdown, link.source_line, lines, width);
        let fallback_width = rendered
            .get(fallback_line)
            .map_or(width, |text| {
                UnicodeWidthStr::width(text.as_str()).min(usize::from(width)) as u16
            })
            .max(1);
        if let Some(existing) = positions.iter_mut().find(|position| {
            position.visual_line == line_offset + fallback_line
                && position.column_range == (0..fallback_width)
        }) {
            existing.urls.push(link.url.clone());
        } else {
            positions.push(LinkPosition {
                visual_line: line_offset + fallback_line,
                column_range: 0..fallback_width,
                urls: vec![link.url.clone()],
            });
        }
        search_line = fallback_line;
        search_byte = rendered.get(fallback_line).map_or(0, String::len);
    }
    positions
}

fn find_link_text(
    rendered: &[String],
    label: &str,
    start_line: usize,
    start_byte: usize,
) -> Option<(usize, usize)> {
    rendered
        .iter()
        .enumerate()
        .skip(start_line)
        .find_map(|(line_index, text)| {
            let byte = if line_index == start_line {
                start_byte.min(text.len())
            } else {
                0
            };
            text[byte..]
                .find(label)
                .map(|relative| (line_index, byte + relative))
        })
}

/// exact 対応ができない場合、そのリンクより前の Markdown を単独で描画・wrap した行数から
/// fallback 行を推定する。少なくとも複数行 block の先頭へ全リンクを誤集約しない。
fn fallback_visual_line(
    markdown: &str,
    source_line: usize,
    rendered: &[Line<'static>],
    width: u16,
) -> usize {
    if rendered.is_empty() {
        return 0;
    }
    let prefix = markdown
        .lines()
        .take(source_line)
        .collect::<Vec<_>>()
        .join("\n");
    let predicted = wrap_lines(&markdown_to_lines(&prefix), width)
        .len()
        .min(rendered.len() - 1);
    (predicted..rendered.len())
        .find(|index| !line_text(&rendered[*index]).is_empty())
        .or_else(|| {
            (0..=predicted)
                .rev()
                .find(|index| !line_text(&rendered[*index]).is_empty())
        })
        .unwrap_or(predicted)
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;
    use ratatui::style::{Color, Modifier};

    fn texts(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(line_text).collect()
    }

    fn placeholder(_: &str, _: &str) -> ImagePresentation {
        ImagePresentation {
            size: None,
            placeholder: "placeholder".to_string(),
        }
    }

    #[test]
    fn wrap_full_width_characters_by_display_columns() {
        let wrapped = wrap_lines(&[Line::raw("日本語ABC")], 5);
        assert_eq!(texts(&wrapped), vec!["日本", "語ABC"]);
    }

    #[test]
    fn wrap_keeps_combining_grapheme_cluster_together() {
        let wrapped = wrap_lines(&[Line::raw("e\u{301}e\u{301}")], 1);
        assert_eq!(texts(&wrapped), vec!["e\u{301}", "e\u{301}"]);
    }

    #[test]
    fn wrap_preserves_empty_visual_lines() {
        let wrapped = wrap_lines(&[Line::raw(""), Line::raw("x")], 4);
        assert_eq!(texts(&wrapped), vec!["", "x"]);
    }

    #[test]
    fn wrap_splits_extremely_long_url_without_break_points() {
        let wrapped = wrap_lines(&[Line::raw("https://example.com/abcdefghijkl")], 8);
        assert!(wrapped.len() > 1);
        assert!(
            wrapped
                .iter()
                .all(|line| UnicodeWidthStr::width(line_text(line).as_str()) <= 8)
        );
        assert_eq!(texts(&wrapped).join(""), "https://example.com/abcdefghijkl");
    }

    #[test]
    fn wrap_preserves_span_boundaries_and_styles_across_wraps() {
        let bold = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);
        let italic = Style::new().fg(Color::Blue).add_modifier(Modifier::ITALIC);
        let line = Line::from(vec![Span::styled("abc", bold), Span::styled("def", italic)]);
        let wrapped = wrap_lines(&[line], 4);
        assert_eq!(texts(&wrapped), vec!["abcd", "ef"]);
        assert_eq!(wrapped[0].spans.len(), 2);
        assert_eq!(wrapped[0].spans[0].style, bold);
        assert_eq!(wrapped[0].spans[1].style, italic);
        assert_eq!(wrapped[1].spans[0].style, italic);
    }

    #[test]
    fn split_does_not_make_code_fence_image_an_image_block() {
        let document = build_document(
            Vec::new(),
            "```\n![inside](https://example.com/no.png)\n```\n![outside](https://example.com/yes.png)",
            80,
            placeholder,
        );
        let images = document
            .blocks
            .iter()
            .filter_map(|block| match block {
                DocBlock::Image { url, .. } => Some(url.as_str()),
                DocBlock::Text(_) => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(images, vec!["https://example.com/yes.png"]);
        let rendered_text = document
            .blocks
            .iter()
            .filter_map(|block| match block {
                DocBlock::Text(lines) => Some(texts(lines).join("\n")),
                DocBlock::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered_text.contains("![inside](https://example.com/no.png)"));
    }

    #[test]
    fn virtual_height_sums_text_and_image_rows_and_clamps_scroll() {
        let document = build_document(
            vec![Line::raw("header")],
            "body\n![img](url)",
            20,
            |_, _| ImagePresentation {
                size: Some(ImageSize {
                    width: 5,
                    height: 6,
                }),
                placeholder: String::new(),
            },
        );
        assert_eq!(document.height, 8);
        assert_eq!(clamp_scroll(99, document.height, 3), 5);
        assert_eq!(clamp_scroll(4, document.height, 20), 0);
    }

    #[test]
    fn link_positions_use_exact_rendered_columns_when_possible() {
        let document = build_document(
            Vec::new(),
            "before [example](https://example.com) after",
            80,
            placeholder,
        );
        let link = document.links.first().expect("link position");
        assert_eq!(link.visual_line, 0);
        assert_eq!(link.column_range, 7..14);
        assert_eq!(link.urls, vec!["https://example.com"]);
    }

    #[test]
    fn link_positions_fall_back_to_whole_line_when_label_wraps() {
        let document = build_document(
            Vec::new(),
            "[abcdefghij](https://example.com)",
            4,
            placeholder,
        );
        let link = document.links.first().expect("fallback link position");
        assert_eq!(link.visual_line, 0);
        assert_eq!(link.column_range, 0..4);
        assert_eq!(link.urls, vec!["https://example.com"]);
    }

    #[test]
    fn repeated_link_labels_map_to_successive_visual_occurrences() {
        let document = build_document(
            Vec::new(),
            "[same](https://one.example) and [same](https://two.example)",
            80,
            placeholder,
        );
        assert_eq!(document.links.len(), 2);
        assert_eq!(document.links[0].column_range, 0..4);
        assert!(
            document.links[1].column_range.start > document.links[0].column_range.end,
            "second repeated label must not map back to the first occurrence"
        );
        assert_eq!(document.links[0].urls, vec!["https://one.example"]);
        assert_eq!(document.links[1].urls, vec!["https://two.example"]);
    }

    #[test]
    fn multiline_link_fallback_uses_its_source_line_not_block_start() {
        let document = build_document(
            Vec::new(),
            "first line\n\n[abcdefghij](https://example.com)",
            4,
            placeholder,
        );
        let link = document.links.first().expect("fallback link position");
        assert!(link.visual_line > 0);
        assert_eq!(link.urls, vec!["https://example.com"]);
    }

    #[test]
    fn image_size_uses_font_aspect_pane_width_and_twenty_row_cap() {
        let image: DynamicImage =
            image::RgbaImage::from_pixel(1000, 2000, Rgba([0, 0, 0, 255])).into();
        let result = Ok(image);
        let presentation = image_presentation("tall", Some(&result), Some((10, 20)), 40);
        assert_eq!(
            presentation.size,
            Some(ImageSize {
                width: 40,
                height: 20
            })
        );
    }

    #[test]
    fn image_without_picker_uses_single_placeholder_row() {
        let image: DynamicImage =
            image::RgbaImage::from_pixel(100, 50, Rgba([0, 0, 0, 255])).into();
        let result = Ok(image);
        let presentation = image_presentation("alt", Some(&result), None, 40);
        assert_eq!(presentation.size, None);
        assert!(presentation.placeholder.contains("未対応"));
        assert_eq!(presentation.height(), 1);
    }

    #[test]
    fn bitbucket_fetch_failure_keeps_section_five_five_placeholder() {
        let result = Err(BITBUCKET_ATTACHMENT_PLACEHOLDER.to_string());
        let presentation = image_presentation("alt", Some(&result), Some((10, 20)), 40);
        assert_eq!(presentation.size, None);
        assert_eq!(presentation.placeholder, BITBUCKET_ATTACHMENT_PLACEHOLDER);
    }
}
