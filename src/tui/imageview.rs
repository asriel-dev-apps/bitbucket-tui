//! PR 本文の画像をターミナル内に表示する機能（抽出・デコード・ハーフブロック描画）。
//!
//! # なぜ `ratatui_image::{Image, StatefulImage}` を使っていないか
//!
//! vendor 済みの `ratatui-image` 11.0.6 は内部で `ratatui = "^0.30.1"` に依存する
//! （`vendor/ratatui-image/Cargo.toml`）。一方、本クレートは `ratatui = "0.29"`
//! （`Cargo.toml`）を使っており、この 2 つは semver 上非互換（0.29.x と 0.30.x は別クレート
//! インスタンスとして解決される。`cargo tree -e features -p ratatui-image` で確認済み）。
//!
//! `ratatui_image::Image`/`StatefulImage` は `ratatui::widgets::{Widget, StatefulWidget}`
//! （`ratatui-image` 自身が依存する 0.30 系のトレイト）を実装しており、それらを描画するには
//! `ratatui::buffer::Buffer`/`layout::Rect`（同じく 0.30 系）を直接構築できる必要がある。
//! しかし `ratatui-image` は自身の `ratatui` 依存を `pub use` で再エクスポートしておらず、
//! 本クレートがそれらの型を名指しするには Cargo.toml に `ratatui`（0.30 系）を別名で
//! 追加依存する以外に方法が無い。これは実装ゲート（`Cargo.toml` は `image` 追加以外は
//! 変更しない）に反するため、`StatefulImage`/`Image` ウィジェットは採用しない。
//!
//! 代わりに:
//! - `ratatui_image::picker::Picker::from_query_stdio()` は端末検出のみに使う（`main.rs`）。
//!   `Picker::font_size()`/`protocol_type()` は `ratatui_image` 自身の型（`FontSize`/
//!   `ProtocolType`）を返すため、上記のクレート境界問題を起こさず安全に使える。
//! - 実際のピクセル→端末セル変換は、このファイルで完結する自前のハーフブロック
//!   （`▀`/`▄` + 前景色/背景色）実装で行う（`ratatui-image` の `protocol::halfblocks`
//!   と同じ手法だが、コードは独立している）。Sixel/Kitty/iTerm2 のようなネイティブ画像
//!   プロトコルは使わないため、対応端末でもピクセル解像度そのままの表示にはならないが、
//!   どの端末でも追加依存無しに動作する。
//!
//! `docs/LEDGER.md` にもこの判断を記録している。

use image::{DynamicImage, Rgba, imageops};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// 取得する画像バイト列の上限（これを超える場合は取得/デコードを拒否する）。
pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// デコード後の画素数上限（圧縮率の高い小さなファイルからの解凍爆弾を防ぐ）。
const MAX_IMAGE_PIXELS: u64 = 40_000_000;

/// 半透明とみなさない最小アルファ値未満は「透明」として扱う。
const TRANSPARENT_ALPHA_THRESHOLD: u8 = 16;

/// PR 本文から抽出した画像参照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub alt: String,
    pub url: String,
}

/// PR 本文の Markdown 画像記法 `![alt](url)` を抽出する（コードフェンス内は除外）。
///
/// `ui::replace_image_syntax`（プレースホルダ置換）と同じ簡易走査（`![` `]` `(` `)` の並びの
/// みを見る）で、記法が崩れていても panic しない。`url` が空の記法（`![alt]()`）は無視する。
pub fn extract_image_refs(body: &str) -> Vec<ImageRef> {
    let mut refs = Vec::new();
    let mut in_code_block = false;
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            continue;
        }
        collect_image_refs_in_line(line, &mut refs);
    }
    refs
}

/// [`extract_image_refs`] の 1 行分の走査。
fn collect_image_refs_in_line(line: &str, out: &mut Vec<ImageRef>) {
    let mut rest = line;
    while let Some(start) = rest.find("![") {
        let after_bang = &rest[start + 2..];
        let Some(close_bracket) = after_bang.find(']') else {
            // 閉じ `]` が無ければ画像記法とみなさず走査を打ち切る。
            return;
        };
        let alt = &after_bang[..close_bracket];
        let after_alt = &after_bang[close_bracket + 1..];
        match after_alt
            .strip_prefix('(')
            .and_then(|paren_rest| paren_rest.find(')').map(|end| (paren_rest, end)))
        {
            Some((paren_rest, close_paren)) => {
                let url = &paren_rest[..close_paren];
                if !url.is_empty() {
                    out.push(ImageRef {
                        alt: alt.to_string(),
                        url: url.to_string(),
                    });
                }
                rest = &paren_rest[close_paren + 1..];
            }
            None => {
                // `(url)` が続かない場合は画像記法とみなさず、`![` の次から走査を続ける。
                rest = &rest[start + 2..];
            }
        }
    }
}

/// 画像バイト列をデコードする。サイズ上限（[`MAX_IMAGE_BYTES`]）・画素数上限
/// （`MAX_IMAGE_PIXELS`）を超える場合や、デコードに失敗する場合はエラー文字列を返す
/// （panic しない）。
pub fn decode_image(bytes: &[u8]) -> Result<DynamicImage, String> {
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "画像が大きすぎます（{} bytes、上限 {MAX_IMAGE_BYTES} bytes）",
            bytes.len()
        ));
    }
    let image = image::load_from_memory(bytes)
        .map_err(|error| format!("画像のデコードに失敗しました: {error}"))?;
    let pixels = u64::from(image.width()) * u64::from(image.height());
    if pixels > MAX_IMAGE_PIXELS {
        return Err("画像の解像度が大きすぎます".to_string());
    }
    Ok(image)
}

/// 画像を `max_cols x max_rows` セル以内へ、フォントのピクセル比（`font_w x font_h`）を
/// 考慮してアスペクト比を保ったまま収める際のセル数 `(cols, rows)` を計算する
/// （`ratatui_image` の `Resize::Fit` と同じ考え方の自前実装。拡大はしない）。
///
/// いずれかの引数が 0 の場合は `(0, 0)`（描画対象なし）を返す。
fn fit_cells(
    img_w: u32,
    img_h: u32,
    max_cols: u16,
    max_rows: u16,
    font_w: u16,
    font_h: u16,
) -> (u16, u16) {
    if img_w == 0 || img_h == 0 || max_cols == 0 || max_rows == 0 || font_w == 0 || font_h == 0 {
        return (0, 0);
    }
    let box_w = f64::from(max_cols) * f64::from(font_w);
    let box_h = f64::from(max_rows) * f64::from(font_h);
    // 拡大はしない（Fit と同じ: 画像が枠より小さければ等倍のまま）。
    let scale = (box_w / f64::from(img_w))
        .min(box_h / f64::from(img_h))
        .min(1.0);
    let fit_w = (f64::from(img_w) * scale).max(1.0);
    let fit_h = (f64::from(img_h) * scale).max(1.0);
    let cols = ((fit_w / f64::from(font_w)).ceil().max(1.0) as u16).min(max_cols);
    let rows = ((fit_h / f64::from(font_h)).ceil().max(1.0) as u16).min(max_rows);
    (cols, rows)
}

/// 画像をハーフブロック（`▀`/`▄` + 前景色/背景色の 24bit カラー）で描画する。
///
/// 1 セル = 横 1px x 縦 2px 相当としてサンプリングする（`ratatui_image` の
/// `protocol::halfblocks` と同じ手法）。`max_cols`/`max_rows` に収まるようアスペクト比を保って
/// 縮小し（[`fit_cells`]）、画像・枠のいずれかが 0 なら空を返す。透明度が低い画素は
/// [`TRANSPARENT_ALPHA_THRESHOLD`] 未満を「透明」として扱い、両方透明なら空白文字にする。
pub fn render_halfblocks(
    image: &DynamicImage,
    max_cols: u16,
    max_rows: u16,
    font_size: (u16, u16),
) -> Vec<Line<'static>> {
    let (font_w, font_h) = font_size;
    let (cols, rows) = fit_cells(
        image.width(),
        image.height(),
        max_cols,
        max_rows,
        font_w,
        font_h,
    );
    if cols == 0 || rows == 0 {
        return Vec::new();
    }

    let sample_rows = u32::from(rows) * 2;
    let resized = imageops::resize(
        image,
        u32::from(cols),
        sample_rows,
        imageops::FilterType::Triangle,
    );

    (0..rows)
        .map(|row| {
            let spans: Vec<Span<'static>> = (0..cols)
                .map(|col| {
                    let upper = *resized.get_pixel(u32::from(col), u32::from(row) * 2);
                    let lower = *resized.get_pixel(u32::from(col), u32::from(row) * 2 + 1);
                    half_block_span(upper, lower)
                })
                .collect();
            Line::from(spans)
        })
        .collect()
}

/// 上下 2 画素をハーフブロック 1 文字（`▀`/`▄`）に変換する。両方透明なら空白。
fn half_block_span(upper: Rgba<u8>, lower: Rgba<u8>) -> Span<'static> {
    let upper_visible = upper.0[3] >= TRANSPARENT_ALPHA_THRESHOLD;
    let lower_visible = lower.0[3] >= TRANSPARENT_ALPHA_THRESHOLD;
    match (upper_visible, lower_visible) {
        (true, true) => Span::styled("▀", Style::new().fg(rgb_color(upper)).bg(rgb_color(lower))),
        (true, false) => Span::styled("▀", Style::new().fg(rgb_color(upper))),
        (false, true) => Span::styled("▄", Style::new().fg(rgb_color(lower))),
        (false, false) => Span::raw(" "),
    }
}

fn rgb_color(pixel: Rgba<u8>) -> Color {
    Color::Rgb(pixel.0[0], pixel.0[1], pixel.0[2])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // ---- extract_image_refs ----

    #[test]
    fn extract_image_refs_parses_multiple_images() {
        let body = "見て: ![図1](https://example.com/a.png) と ![図2](https://example.com/b.png)";
        let refs = extract_image_refs(body);
        assert_eq!(
            refs,
            vec![
                ImageRef {
                    alt: "図1".to_string(),
                    url: "https://example.com/a.png".to_string(),
                },
                ImageRef {
                    alt: "図2".to_string(),
                    url: "https://example.com/b.png".to_string(),
                },
            ]
        );
    }

    #[test]
    fn extract_image_refs_handles_missing_alt() {
        let refs = extract_image_refs("![](https://example.com/a.png)");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].alt, "");
        assert_eq!(refs[0].url, "https://example.com/a.png");
    }

    #[test]
    fn extract_image_refs_skips_code_fence_content() {
        let body = "```\n![フェンス内](https://example.com/ignored.png)\n```\n![本文](https://example.com/real.png)";
        let refs = extract_image_refs(body);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].url, "https://example.com/real.png");
    }

    #[test]
    fn extract_image_refs_skips_entries_with_empty_url() {
        let refs = extract_image_refs("![alt]()");
        assert!(refs.is_empty());
    }

    #[test]
    fn extract_image_refs_tolerates_malformed_syntax_without_panicking() {
        // 壊れた記法・空文字列・日本語のみで panic しないこと。
        assert!(extract_image_refs("![alt without closing paren(url").is_empty());
        assert!(extract_image_refs("![alt](unterminated").is_empty());
        assert!(extract_image_refs("").is_empty());
        assert!(extract_image_refs("普通のテキストです").is_empty());
        assert!(extract_image_refs("![").is_empty());
    }

    #[test]
    fn extract_image_refs_ignores_unclosed_fence_to_end_of_body() {
        // 閉じないコードフェンスでも panic せず、フェンス後の画像記法は無視され続ける。
        let body = "```\n![a](https://example.com/a.png)";
        assert!(extract_image_refs(body).is_empty());
    }

    // ---- decode_image ----

    fn encode_png(image: &DynamicImage) -> Vec<u8> {
        let mut buffer = Cursor::new(Vec::new());
        image
            .write_to(&mut buffer, image::ImageFormat::Png)
            .expect("PNG エンコードに成功すること");
        buffer.into_inner()
    }

    #[test]
    fn decode_image_roundtrips_a_small_png() {
        let image: DynamicImage =
            image::RgbaImage::from_pixel(3, 2, Rgba([10, 20, 30, 255])).into();
        let bytes = encode_png(&image);
        let decoded = decode_image(&bytes).expect("デコード成功");
        assert_eq!(decoded.width(), 3);
        assert_eq!(decoded.height(), 2);
    }

    #[test]
    fn decode_image_rejects_garbage_bytes_without_panicking() {
        let result = decode_image(b"not an image at all");
        assert!(result.is_err());
    }

    #[test]
    fn decode_image_rejects_oversized_bytes_before_decoding() {
        let bytes = vec![0u8; MAX_IMAGE_BYTES + 1];
        let result = decode_image(&bytes);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("大きすぎます"));
    }

    // ---- fit_cells ----

    #[test]
    fn fit_cells_returns_zero_when_any_dimension_is_zero() {
        assert_eq!(fit_cells(0, 10, 5, 5, 10, 20), (0, 0));
        assert_eq!(fit_cells(10, 10, 0, 5, 10, 20), (0, 0));
        assert_eq!(fit_cells(10, 10, 5, 5, 0, 20), (0, 0));
    }

    #[test]
    fn fit_cells_does_not_upscale_small_images() {
        // 画像 1x1px は、フォント 10x20px の枠内に置くと 1 セルに収まる（拡大しない）。
        let (cols, rows) = fit_cells(1, 1, 100, 100, 10, 20);
        assert_eq!((cols, rows), (1, 1));
    }

    #[test]
    fn fit_cells_preserves_aspect_ratio_when_downscaling() {
        // 200x100px の横長画像をフォント 10x20px・最大 5x5 セルへ収める。
        // box = (50px, 100px)。scale = min(50/200, 100/100) = 0.25 -> fit=(50px,25px)。
        // cols = ceil(50/10) = 5、rows = ceil(25/20) = 2。
        let (cols, rows) = fit_cells(200, 100, 5, 5, 10, 20);
        assert_eq!((cols, rows), (5, 2));
    }

    // ---- render_halfblocks ----

    #[test]
    fn render_halfblocks_returns_empty_for_zero_area() {
        let image: DynamicImage = image::RgbaImage::from_pixel(2, 2, Rgba([255, 0, 0, 255])).into();
        assert!(render_halfblocks(&image, 0, 10, (10, 20)).is_empty());
        assert!(render_halfblocks(&image, 10, 0, (10, 20)).is_empty());
    }

    #[test]
    fn render_halfblocks_maps_exact_2x2_image_without_blending() {
        // font_size=(1,2) で box=(2px,2px) が画像サイズ(2,2)と一致するため resize は等倍コピー
        // になり、フィルタによる混色が起きない（`image::imageops::resize` は同一サイズ時に
        // コピーのみ行う実装のため）。
        let mut image = image::RgbaImage::new(2, 2);
        image.put_pixel(0, 0, Rgba([255, 0, 0, 255])); // 左上: 赤
        image.put_pixel(1, 0, Rgba([0, 255, 0, 255])); // 右上: 緑
        image.put_pixel(0, 1, Rgba([0, 0, 255, 255])); // 左下: 青
        image.put_pixel(1, 1, Rgba([255, 255, 0, 255])); // 右下: 黄
        let image: DynamicImage = image.into();

        let lines = render_halfblocks(&image, 2, 1, (1, 2));
        assert_eq!(lines.len(), 1);
        let spans = &lines[0].spans;
        assert_eq!(spans.len(), 2);

        assert_eq!(spans[0].content, "▀");
        assert_eq!(spans[0].style.fg, Some(Color::Rgb(255, 0, 0)));
        assert_eq!(spans[0].style.bg, Some(Color::Rgb(0, 0, 255)));

        assert_eq!(spans[1].content, "▀");
        assert_eq!(spans[1].style.fg, Some(Color::Rgb(0, 255, 0)));
        assert_eq!(spans[1].style.bg, Some(Color::Rgb(255, 255, 0)));
    }

    #[test]
    fn half_block_span_handles_transparency_combinations() {
        let opaque_blue = Rgba([0, 0, 255, 255]);
        let transparent = Rgba([0, 0, 0, 0]);

        let both_opaque = half_block_span(opaque_blue, opaque_blue);
        assert_eq!(both_opaque.content, "▀");
        assert!(both_opaque.style.fg.is_some());
        assert!(both_opaque.style.bg.is_some());

        let upper_only = half_block_span(opaque_blue, transparent);
        assert_eq!(upper_only.content, "▀");
        assert_eq!(upper_only.style.fg, Some(Color::Rgb(0, 0, 255)));
        assert!(upper_only.style.bg.is_none());

        let lower_only = half_block_span(transparent, opaque_blue);
        assert_eq!(lower_only.content, "▄");
        assert_eq!(lower_only.style.fg, Some(Color::Rgb(0, 0, 255)));
        assert!(lower_only.style.bg.is_none());

        let both_transparent = half_block_span(transparent, transparent);
        assert_eq!(both_transparent.content, " ");
        assert!(both_transparent.style.fg.is_none());
        assert!(both_transparent.style.bg.is_none());
    }
}
