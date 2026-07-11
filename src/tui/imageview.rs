//! PR 本文の画像をターミナル内に表示する機能（抽出・デコード）。
//!
//! # 描画方式（`ratatui-image` 8.1.1 のネイティブプロトコル）
//!
//! 以前 vendor していた `ratatui-image` 11.0.6 は内部で `ratatui = "^0.30.1"` に依存しており、
//! 本クレートの `ratatui = "0.29"` とは semver 上非互換（別クレートインスタンスとして解決される）
//! だったため、`StatefulImage`/`Image` ウィジェットを直接描画に使えなかった（自前のハーフブロック
//! 描画で代替していた。詳細は `docs/LEDGER.md` の「画像表示 実装メモ」参照）。
//!
//! **`ratatui-image` を 8.1.1 へ差し替えた（`Cargo.toml`）ことで解消済み**: 8.1.1 は
//! `ratatui = "^0.29"` に依存するため本クレートの `ratatui` と同一インスタンスに解決され、
//! `ratatui_image::StatefulImage`/`protocol::StatefulProtocol` をそのまま
//! `Frame::render_stateful_widget` へ渡せる。実際の描画（`Picker` の生成・
//! `StatefulProtocol` の保持・`StatefulImage` での描画）は `src/tui/app.rs`（状態）と
//! `src/tui/ui.rs::render_image_view`（描画）が担い、このファイルは端末プロトコルに
//! 依存しない部分（本文からの画像参照抽出・バイト列のデコード）のみを扱う。
//! 端末が画像プロトコル未対応でも、`ratatui-image` 自身が内蔵のハーフブロック
//! （`protocol::halfblocks`）へ自動フォールバックするため、本クレート側で自前のフォール
//! バック描画を持つ必要はない。

use image::DynamicImage;

/// 取得する画像バイト列の上限（これを超える場合は取得/デコードを拒否する）。
pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// デコード後の画素数上限（圧縮率の高い小さなファイルからの解凍爆弾を防ぐ）。
const MAX_IMAGE_PIXELS: u64 = 40_000_000;

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

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;
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

    // ---- decode_image + StatefulProtocol 生成（ratatui-image 8.1.1 ネイティブ描画） ----
    //
    // `Picker::from_query_stdio()` は実端末への問い合わせを行うためテスト環境では使えない。
    // 代わりに `Picker::from_fontsize` （端末問い合わせなしの固定値生成 API）でテストする。

    #[test]
    fn decode_then_new_resize_protocol_does_not_panic_for_a_valid_image() {
        let image: DynamicImage =
            image::RgbaImage::from_pixel(3, 2, Rgba([10, 20, 30, 255])).into();
        let bytes = encode_png(&image);
        let decoded = decode_image(&bytes).expect("デコード成功");

        let picker = ratatui_image::picker::Picker::from_fontsize((10, 20));
        // 生成そのものが panic しないことを確認する（実際のリサイズ・エンコードは描画時に
        // 遅延されるため、ここでは `StatefulProtocol` を作れることのみ確認する）。
        let _protocol = picker.new_resize_protocol(decoded);
    }

    #[test]
    fn decode_rejects_corrupt_bytes_before_protocol_creation() {
        // 壊れたバイト列は `decode_image` の時点でエラーになり、`StatefulProtocol` 生成には
        // 到達しない（`Msg::ImageLoaded` ハンドラの `result.and_then(decode_image)` と同じ経路）。
        let result = decode_image(b"not a real image");
        assert!(result.is_err());
    }
}
