//! CF_DIB（BITMAPINFOHEADER + ピクセル）と WebP ロスレスの相互変換。
//!
//! 保存時は DIB をデコードして WebP ロスレスに変換し（storage.rs が CF_DIB blob に
//! 適用）、読み込み時は 32bpp BI_RGB ボトムアップの DIB を再構築する。元 DIB と
//! バイト同一にはならない（24bpp 入力は 32bpp になり、ヘッダの解像度フィールドは
//! 保持しない）が、ピクセル値は完全に保存される。
//!
//! 対応する DIB は 24/32bpp の BI_RGB と、標準マスク（BGRX）の 32bpp BI_BITFIELDS
//! のみ。パレット形式・16bpp 等は `DibError::Unsupported` を返し、呼び出し側が
//! 生バイナリ保存へフォールバックする（クリップボードの実データはほぼ全て
//! 24/32bpp であり、稀な形式のために変換器を複雑にしない）。

use std::fmt;
use std::io::Cursor;

use image_webp::{ColorType, WebPDecoder, WebPEncoder};

// --- Error ---

#[derive(Debug)]
pub enum DibError {
    /// 対応外のDIB形式。呼び出し側は生バイナリ保存へフォールバックする
    Unsupported(&'static str),
    /// DIBとして壊れている（サイズ不足・不正なヘッダ値）
    Invalid(&'static str),
    Encode(image_webp::EncodingError),
    Decode(image_webp::DecodingError),
}

impl fmt::Display for DibError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(s) => write!(f, "対応していない画像の形式です（{s}）"),
            Self::Invalid(s) => write!(f, "画像のデータが壊れています（{s}）"),
            // `image-webp` の説明は英語のまま（依存クレートの文言は訳さない）
            Self::Encode(e) => write!(f, "画像を WebP に変換できません（{e}）"),
            Self::Decode(e) => write!(f, "WebP の画像を読めません（{e}）"),
        }
    }
}

impl std::error::Error for DibError {}

impl From<image_webp::EncodingError> for DibError {
    fn from(e: image_webp::EncodingError) -> Self {
        Self::Encode(e)
    }
}

impl From<image_webp::DecodingError> for DibError {
    fn from(e: image_webp::DecodingError) -> Self {
        Self::Decode(e)
    }
}

type Result<T> = std::result::Result<T, DibError>;

// --- DIB constants ---

const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;
const INFO_HEADER_SIZE: usize = 40;
/// BGRX配置を表す標準ビットフィールドマスク（R, G, B の順）
const STANDARD_MASKS: [u32; 3] = [0x00FF_0000, 0x0000_FF00, 0x0000_00FF];

// --- Public API ---

/// BITMAPFILEHEADER のサイズ（.bmpファイル先頭の14バイト）
const FILE_HEADER_SIZE: usize = 14;

/// CF_DIB のバイト列を .bmp ファイルのバイト列にする（BITMAPFILEHEADERを前置）。
/// ピクセル変換はしないため、`parse_dib`が非対応の形式（8bpp等）でも劣化なく書き出せる。
/// bfOffBits の計算だけヘッダから行う（パレット・ビットフィールドマスクの分を加算）。
pub fn dib_to_bmp_file(dib: &[u8]) -> Result<Vec<u8>> {
    if dib.len() < INFO_HEADER_SIZE {
        return Err(DibError::Invalid("BITMAPINFOHEADERに満たない長さ"));
    }
    let header_size = u32_le(dib, 0) as usize;
    if header_size < INFO_HEADER_SIZE || header_size > dib.len() {
        return Err(DibError::Invalid("ヘッダサイズが不正"));
    }
    let bpp = u16_le(dib, 14) as usize;
    let compression = u32_le(dib, 16);
    let clr_used = u32_le(dib, 32) as usize;
    // パレット要素数: <=8bppでbiClrUsed=0なら2^bpp、それ以外はbiClrUsedそのまま
    // （>8bppの最適化パレットもbfOffBitsに含める）
    let palette_entries = if clr_used != 0 {
        clr_used
    } else if (1..=8).contains(&bpp) {
        1usize << bpp
    } else {
        0
    };
    // biSize=40のBI_BITFIELDSはヘッダ直後に3DWORDのマスクが付く（parse_dibと同じ規則）
    let masks_extra = if compression == BI_BITFIELDS && header_size == INFO_HEADER_SIZE {
        12
    } else {
        0
    };
    let pixel_offset = FILE_HEADER_SIZE + header_size + masks_extra + palette_entries * 4;

    let mut bmp = Vec::with_capacity(FILE_HEADER_SIZE + dib.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((FILE_HEADER_SIZE + dib.len()) as u32).to_le_bytes()); // bfSize
    bmp.extend_from_slice(&[0; 4]); // bfReserved1/2
    bmp.extend_from_slice(&(pixel_offset as u32).to_le_bytes()); // bfOffBits
    bmp.extend_from_slice(dib);
    Ok(bmp)
}

/// CF_DIB のバイト列を WebP ロスレスに変換する。
pub fn dib_to_webp(dib: &[u8]) -> Result<Vec<u8>> {
    let parsed = parse_dib(dib)?;
    let mut out = Vec::new();
    let encoder = WebPEncoder::new(&mut out);
    match &parsed.pixels {
        Pixels::Rgb(data) => encoder.encode(data, parsed.width, parsed.height, ColorType::Rgb8)?,
        Pixels::Rgba(data) => encoder.encode(data, parsed.width, parsed.height, ColorType::Rgba8)?,
    }
    Ok(out)
}

/// サムネイルの長辺ピクセル数。一覧・将来のポップアップメニュー（C版は既定32px）
/// の両方をhidpi込みでカバーする値。
pub const THUMB_LONG_EDGE: u32 = 128;

/// CF_DIB からサムネイル（長辺 `THUMB_LONG_EDGE` px）の WebP を生成する。
/// 元画像が既にそれ以下のサイズなら `None`（フル画像のWebPで足りるため作らない）。
pub fn dib_to_thumbnail_webp(dib: &[u8]) -> Result<Option<Vec<u8>>> {
    let parsed = parse_dib(dib)?;
    let long_edge = parsed.width.max(parsed.height);
    if long_edge <= THUMB_LONG_EDGE {
        return Ok(None);
    }
    let scale = f64::from(THUMB_LONG_EDGE) / f64::from(long_edge);
    let tw = ((f64::from(parsed.width) * scale).round() as u32).max(1);
    let th = ((f64::from(parsed.height) * scale).round() as u32).max(1);

    let (data, channels, color) = match &parsed.pixels {
        Pixels::Rgb(d) => (d, 3, ColorType::Rgb8),
        Pixels::Rgba(d) => (d, 4, ColorType::Rgba8),
    };
    let small = downscale_box(data, parsed.width, parsed.height, channels, tw, th);

    let mut out = Vec::new();
    WebPEncoder::new(&mut out).encode(&small, tw, th, color)?;
    Ok(Some(out))
}

/// ボックス平均による縮小。各出力ピクセルに対応する入力矩形の画素平均を取る。
/// 拡大は想定しない（呼び出し側が縮小時のみ使う）。
fn downscale_box(pixels: &[u8], w: u32, h: u32, channels: usize, tw: u32, th: u32) -> Vec<u8> {
    let (w, h, tw, th) = (w as usize, h as usize, tw as usize, th as usize);
    let mut out = Vec::with_capacity(tw * th * channels);
    for ty in 0..th {
        let y0 = ty * h / th;
        let y1 = ((ty + 1) * h / th).max(y0 + 1);
        for tx in 0..tw {
            let x0 = tx * w / tw;
            let x1 = ((tx + 1) * w / tw).max(x0 + 1);
            let mut acc = [0u64; 4];
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = (y * w + x) * channels;
                    for (c, a) in acc.iter_mut().enumerate().take(channels) {
                        *a += u64::from(pixels[p + c]);
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u64;
            for a in acc.iter().take(channels) {
                out.push((a / n) as u8);
            }
        }
    }
    out
}

/// CF_DIB を RGBA8（トップダウン）に変換する（縮小してから使う: `dib_to_rgba_scaled`・ツールチップなど）。
pub fn dib_to_rgba(dib: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let parsed = parse_dib(dib)?;
    let (w, h) = (parsed.width, parsed.height);
    Ok(match parsed.pixels {
        Pixels::Rgba(data) => (w, h, data),
        Pixels::Rgb(data) => {
            let mut rgba = Vec::with_capacity(data.len() / 3 * 4);
            for px in data.chunks_exact(3) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            (w, h, rgba)
        }
    })
}

/// CF_DIB を長辺 `max_edge` px 以下へ縮小した RGBA8（トップダウン）にする。
/// 元画像が既にそれ以下なら縮小しない（`webp_to_rgba_scaled`のDIB版）。
pub fn dib_to_rgba_scaled(dib: &[u8], max_edge: u32) -> Result<(u32, u32, Vec<u8>)> {
    let (w, h, rgba) = dib_to_rgba(dib)?;
    Ok(scale_rgba(w, h, rgba, max_edge))
}

/// RGBA8を長辺 `max_edge` px 以下へ縮小する（それ以下ならそのまま返す）。
/// ビューアの画像の読み込み（native/images.rs）でも、復号済みの画像を縮小するのに使う。
pub(crate) fn scale_rgba(w: u32, h: u32, rgba: Vec<u8>, max_edge: u32) -> (u32, u32, Vec<u8>) {
    let long_edge = w.max(h);
    if long_edge <= max_edge {
        return (w, h, rgba);
    }
    let scale = f64::from(max_edge) / f64::from(long_edge);
    let tw = ((f64::from(w) * scale).round() as u32).max(1);
    let th = ((f64::from(h) * scale).round() as u32).max(1);
    (tw, th, downscale_box(&rgba, w, h, 4, tw, th))
}

/// `webp_to_rgba_scaled` が展開してよい原寸の大きさ（RGBA に換算したバイト数。2048 × 2048 相当）。
const SCALED_WEBP_MAX_BYTES: u64 = 2048 * 2048 * 4;

/// WebP を長辺 `max_edge` px 以下へ縮小した RGBA8（トップダウン）にデコードする。
/// 元画像が既にそれ以下なら縮小しない（ポップアップメニューのサムネイル用。
/// 一覧の128pxサムネイルをさらにメニュー行サイズへ縮める）。
pub fn webp_to_rgba_scaled(webp: &[u8], max_edge: u32) -> Result<(u32, u32, Vec<u8>)> {
    // 原寸を作ってから縮めるので、展開の前に大きさを確かめる（呼び出し元はホットキーのスレッドで、読むのは
    // 長辺 `THUMB_LONG_EDGE` のサムネイルか小さな画像。壊れた・書き換えられたファイルで大きな確保をしない）
    let (w, h) = webp_dimensions(webp)?;
    if u64::from(w) * u64::from(h) * 4 > SCALED_WEBP_MAX_BYTES {
        return Err(DibError::Invalid("縮小して使う WebP が大きすぎる"));
    }
    let (w, h, rgba) = webp_to_rgba(webp)?;
    Ok(scale_rgba(w, h, rgba, max_edge))
}

/// WebP を RGBA8（トップダウン）にデコードする（原寸を作る。大きさの上限は呼び出し側が先に確かめる）。
pub fn webp_to_rgba(webp: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let mut decoder = WebPDecoder::new(Cursor::new(webp))?;
    let (w, h) = decoder.dimensions();
    let size = decoder
        .output_buffer_size()
        .ok_or(DibError::Invalid("WebP画像が大きすぎる"))?;
    let mut buf = vec![0u8; size];
    decoder.read_image(&mut buf)?;
    if decoder.has_alpha() {
        Ok((w, h, buf))
    } else {
        let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
        for px in buf.chunks_exact(3) {
            rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
        }
        Ok((w, h, rgba))
    }
}

/// CF_DIB のヘッダーだけから幅・高さを読む（画素の長さは確かめない。先頭だけを読んだデータでも
/// よい）。ビューアの画像の読み込みで、展開・縮小の前に大きさを判定するのに使う。
pub fn dib_header_dimensions(dib: &[u8]) -> Result<(u32, u32)> {
    if dib.len() < INFO_HEADER_SIZE {
        return Err(DibError::Invalid("BITMAPINFOHEADERに満たない長さ"));
    }
    let (width_raw, height_raw) = (i32_le(dib, 4), i32_le(dib, 8));
    if width_raw <= 0 || height_raw == 0 {
        return Err(DibError::Invalid("幅・高さが不正"));
    }
    Ok((width_raw as u32, height_raw.unsigned_abs()))
}

/// CF_DIB を `tw` × `th`（元より大きくはしない）へ縮小した RGBA8（トップダウン）にする。原寸の
/// RGBA を作らず、元のデータの行から直接ボックス平均で縮める（大きな画像のプレビューで、原寸の
/// 写しを作らないため）。結果は `dib_to_rgba` の後に同じ大きさへ `downscale_box` したものと
/// 同じ。
pub fn dib_to_rgba_downscaled(dib: &[u8], tw: u32, th: u32) -> Result<(u32, u32, Vec<u8>)> {
    let layout = parse_layout(dib)?;
    let (w, h) = (layout.width as usize, layout.height as usize);
    let (tw, th) = (tw.clamp(1, layout.width) as usize, th.clamp(1, layout.height) as usize);
    if tw == w && th == h {
        return dib_to_rgba(dib);
    }
    let alpha = layout.has_meaningful_alpha(dib);
    let px_size = layout.bpp / 8;
    let columns: Vec<(usize, usize)> =
        (0..tw).map(|tx| (tx * w / tw, ((tx + 1) * w / tw).max(tx * w / tw + 1))).collect();
    let mut out = Vec::with_capacity(tw * th * 4);
    let mut acc = vec![0u64; tw * 4];
    for ty in 0..th {
        let y0 = ty * h / th;
        let y1 = ((ty + 1) * h / th).max(y0 + 1);
        acc.fill(0);
        for y in y0..y1 {
            let base = layout.row_start(y);
            let row = &dib[base..base + w * px_size];
            for (tx, &(x0, x1)) in columns.iter().enumerate() {
                let a = &mut acc[tx * 4..tx * 4 + 4];
                for px in row[x0 * px_size..x1 * px_size].chunks_exact(px_size) {
                    a[0] += u64::from(px[2]);
                    a[1] += u64::from(px[1]);
                    a[2] += u64::from(px[0]);
                    a[3] += if alpha { u64::from(px[3]) } else { 255 };
                }
            }
        }
        for (tx, &(x0, x1)) in columns.iter().enumerate() {
            let n = ((y1 - y0) * (x1 - x0)) as u64;
            out.extend(acc[tx * 4..tx * 4 + 4].iter().map(|a| (a / n) as u8));
        }
    }
    Ok((tw as u32, th as u32, out))
}

/// テスト用: RGBA8 を `tw` × `th` へボックス平均で縮める（原寸を作ってから縮める、今までの方法の
/// 再現。`*_downscaled` の結果と比べる）。
#[cfg(test)]
pub(crate) fn scale_rgba_to(w: u32, h: u32, rgba: &[u8], tw: u32, th: u32) -> Vec<u8> {
    if tw == w && th == h { rgba.to_vec() } else { downscale_box(rgba, w, h, 4, tw, th) }
}

/// WebP のヘッダーから幅・高さを読む（画素は展開しない）。
pub fn webp_dimensions(webp: &[u8]) -> Result<(u32, u32)> {
    Ok(WebPDecoder::new(Cursor::new(webp))?.dimensions())
}

/// WebP を展開し、`tw` × `th`（元より大きくはしない）へ縮小した RGBA8（トップダウン）にする。
/// 展開した画素（RGB または RGBA）から直接縮めるので、原寸の RGBA への写しは作らない。
/// 結果は `webp_to_rgba` の後に同じ大きさへ `downscale_box` したものと同じ。
pub fn webp_to_rgba_downscaled(webp: &[u8], tw: u32, th: u32) -> Result<(u32, u32, Vec<u8>)> {
    let mut decoder = WebPDecoder::new(Cursor::new(webp))?;
    let (w, h) = decoder.dimensions();
    let size = decoder
        .output_buffer_size()
        .ok_or(DibError::Invalid("WebP画像が大きすぎる"))?;
    let mut buf = vec![0u8; size];
    decoder.read_image(&mut buf)?;
    let channels = if decoder.has_alpha() { 4 } else { 3 };
    let (tw, th) = (tw.clamp(1, w.max(1)), th.clamp(1, h.max(1)));
    let small = if tw == w && th == h { buf } else { downscale_box(&buf, w, h, channels, tw, th) };
    let rgba = if channels == 4 {
        small
    } else {
        small.chunks_exact(3).flat_map(|px| [px[0], px[1], px[2], 255]).collect()
    };
    Ok((tw, th, rgba))
}

/// WebP を 32bpp BI_RGB ボトムアップの CF_DIB として再構築する。
pub fn webp_to_dib(webp: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = WebPDecoder::new(Cursor::new(webp))?;
    let (width, height) = decoder.dimensions();
    let buf_size = decoder
        .output_buffer_size()
        .ok_or(DibError::Invalid("WebP画像が大きすぎる"))?;
    let mut pixels = vec![0u8; buf_size];
    decoder.read_image(&mut pixels)?;
    Ok(build_dib_32bpp(width, height, &pixels, decoder.has_alpha()))
}

// --- DIB parsing ---

enum Pixels {
    /// RGB 3バイト/ピクセル、トップダウン
    Rgb(Vec<u8>),
    /// RGBA 4バイト/ピクセル、トップダウン
    Rgba(Vec<u8>),
}

struct ParsedDib {
    width: u32,
    height: u32,
    pixels: Pixels,
}

fn u16_le(d: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([d[off], d[off + 1]])
}

fn u32_le(d: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([d[off], d[off + 1], d[off + 2], d[off + 3]])
}

fn i32_le(d: &[u8], off: usize) -> i32 {
    u32_le(d, off) as i32
}

/// 解析した DIB の配置（画素はまだ写さない）。
struct DibLayout {
    width: u32,
    height: u32,
    top_down: bool,
    bpp: usize,
    stride: usize,
    pixel_offset: usize,
}

impl DibLayout {
    /// 上から `y` 行目（トップダウンに数える）の先頭のバイト位置。
    fn row_start(&self, y: usize) -> usize {
        let row = if self.top_down { y } else { self.height as usize - 1 - y };
        self.pixel_offset + row * self.stride
    }

    /// 32bppの4バイト目はBI_RGBでは「予約」であり、BitBltで作られたスクリーン
    /// ショット等は全ピクセル0になっている。0のままWebPに載せると完全透明画像
    /// として扱われ、エンコーダの透明ピクセル最適化でRGB値が失われ得るため、
    /// 全アルファ0のDIBは不透明RGBとして変換する
    fn has_meaningful_alpha(&self, dib: &[u8]) -> bool {
        let w = self.width as usize;
        self.bpp == 32
            && (0..self.height as usize).any(|y| {
                let base = self.row_start(y);
                dib[base..base + w * 4].chunks_exact(4).any(|px| px[3] != 0)
            })
    }
}

fn parse_dib(dib: &[u8]) -> Result<ParsedDib> {
    let layout = parse_layout(dib)?;
    let (w, h) = (layout.width as usize, layout.height as usize);
    let pixels = if layout.has_meaningful_alpha(dib) {
        let mut data = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            let base = layout.row_start(y);
            for px in dib[base..base + w * 4].chunks_exact(4) {
                data.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
            }
        }
        Pixels::Rgba(data)
    } else {
        let px_size = layout.bpp / 8;
        let mut data = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            let base = layout.row_start(y);
            for px in dib[base..base + w * px_size].chunks_exact(px_size) {
                data.extend_from_slice(&[px[2], px[1], px[0]]);
            }
        }
        Pixels::Rgb(data)
    };
    Ok(ParsedDib { width: layout.width, height: layout.height, pixels })
}

/// ヘッダーを検査して配置を求める（対応する形式・データの長さも確かめる）。
fn parse_layout(dib: &[u8]) -> Result<DibLayout> {
    if dib.len() < INFO_HEADER_SIZE {
        return Err(DibError::Invalid("BITMAPINFOHEADERに満たない長さ"));
    }
    let header_size = u32_le(dib, 0) as usize;
    if header_size < INFO_HEADER_SIZE {
        // BITMAPCOREHEADER(12バイト)等の旧形式。現代のクリップボードには現れない
        return Err(DibError::Unsupported("BITMAPINFOHEADER以前の旧ヘッダ"));
    }
    if header_size > dib.len() {
        return Err(DibError::Invalid("ヘッダサイズがデータ長を超えている"));
    }

    let width_raw = i32_le(dib, 4);
    let height_raw = i32_le(dib, 8);
    let bpp = u16_le(dib, 14) as usize;
    let compression = u32_le(dib, 16);
    let clr_used = u32_le(dib, 32);

    if width_raw <= 0 || height_raw == 0 {
        return Err(DibError::Invalid("幅・高さが不正"));
    }
    let width = width_raw as u32;
    // biHeight負値はトップダウン（行が上から下）を意味する
    let top_down = height_raw < 0;
    let height = height_raw.unsigned_abs();

    // BI_BITFIELDSのマスク位置: biSize=40ならヘッダ直後に3DWORD、
    // V4/V5ヘッダ(biSize>=108)ならヘッダ内のオフセット40に内包される
    let masks_extra: usize = match compression {
        BI_RGB => 0,
        BI_BITFIELDS => {
            if dib.len() < INFO_HEADER_SIZE + 12 {
                return Err(DibError::Invalid("ビットフィールドマスクが欠落"));
            }
            let masks = [u32_le(dib, 40), u32_le(dib, 44), u32_le(dib, 48)];
            if masks != STANDARD_MASKS {
                return Err(DibError::Unsupported("非標準のビットフィールドマスク"));
            }
            if header_size == INFO_HEADER_SIZE { 12 } else { 0 }
        }
        _ => return Err(DibError::Unsupported("BI_RGB/BI_BITFIELDS以外の圧縮形式")),
    };

    match (bpp, compression) {
        (24 | 32, BI_RGB) => {}
        (32, BI_BITFIELDS) => {}
        _ => return Err(DibError::Unsupported("24/32bpp以外のビット深度")),
    }

    // 行は4バイト境界に切り上げ
    let stride = (u64::from(width) * bpp as u64).div_ceil(32) * 4;
    // clr_usedは>8bppでは省略可能な最適化パレットの要素数。ヘッダーはほかのアプリがクリップボードに
    // 置いた信頼できない値なので、必要な長さは桁あふれを確かめて計算する（u64 でも、幅・高さ・clr_used が
    // 極端だと超える。回り込むと長さの確認を通って、後の範囲外の読み取りでパニックする）
    let need = (header_size as u64)
        .checked_add(masks_extra as u64)
        .and_then(|v| v.checked_add(u64::from(clr_used) * 4))
        .and_then(|offset| Some((offset, offset.checked_add(stride.checked_mul(u64::from(height))?)?)));
    let Some((pixel_offset_u64, need)) = need else {
        return Err(DibError::Invalid("ピクセルデータの大きさが不正"));
    };
    if need > dib.len() as u64 {
        return Err(DibError::Invalid("ピクセルデータが不足"));
    }
    // どちらもデータの長さ以下なので usize に収まる
    let pixel_offset = pixel_offset_u64 as usize;
    let stride = stride as usize;

    Ok(DibLayout { width, height, top_down, bpp, stride, pixel_offset })
}

// --- DIB rebuilding ---

/// RGB/RGBA（トップダウン）から32bpp BI_RGBボトムアップのDIBを組み立てる。
/// アルファ無し入力の予約バイトは仕様どおり0にする（全アルファ0の32bpp DIBが
/// 入力だった場合、ピクセル部はバイト同一で復元されることになる）。
fn build_dib_32bpp(width: u32, height: u32, pixels: &[u8], has_alpha: bool) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let stride = w * 4; // 32bppは常に4バイト境界

    let mut dib = Vec::with_capacity(INFO_HEADER_SIZE + stride * h);
    dib.extend_from_slice(&(INFO_HEADER_SIZE as u32).to_le_bytes()); // biSize
    dib.extend_from_slice(&(width as i32).to_le_bytes()); // biWidth
    dib.extend_from_slice(&(height as i32).to_le_bytes()); // biHeight（正=ボトムアップ）
    dib.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    dib.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    dib.extend_from_slice(&BI_RGB.to_le_bytes()); // biCompression
    dib.extend_from_slice(&((stride * h) as u32).to_le_bytes()); // biSizeImage
    dib.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter（元DIBの解像度は保持しない）
    dib.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    let px_size = if has_alpha { 4 } else { 3 };
    for y in (0..h).rev() {
        let row = &pixels[y * w * px_size..(y + 1) * w * px_size];
        for px in row.chunks_exact(px_size) {
            let a = if has_alpha { px[3] } else { 0 };
            dib.extend_from_slice(&[px[2], px[1], px[0], a]);
        }
    }
    dib
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ヘッダーの幅・高さ・`biClrUsed` が極端な値でも、必要な長さの計算が桁あふれせず、パニックせずに
    /// 誤りを返す（ほかのアプリがクリップボードに置く信頼できない入力）。
    #[test]
    fn extreme_header_values_are_rejected_without_panic() {
        let header = |width: i32, height: i32, clr_used: u32| {
            let mut v = Vec::new();
            v.extend(40u32.to_le_bytes());
            v.extend(width.to_le_bytes());
            v.extend(height.to_le_bytes());
            v.extend(1u16.to_le_bytes());
            v.extend(32u16.to_le_bytes());
            v.extend(BI_RGB.to_le_bytes());
            v.extend([0u8; 12]);
            v.extend(clr_used.to_le_bytes());
            v.extend([0u8; 4]);
            v
        };
        for dib in [
            header(i32::MAX, i32::MIN, 0x8000_0000),
            header(i32::MAX, i32::MIN, u32::MAX),
            header(i32::MAX, i32::MAX, 0),
            header(1, i32::MIN, u32::MAX),
        ] {
            assert!(matches!(dib_to_webp(&dib), Err(DibError::Invalid(_))));
            assert!(dib_to_rgba(&dib).is_err());
            assert!(dib_to_rgba_downscaled(&dib, 8, 8).is_err());
            assert!(dib_to_thumbnail_webp(&dib).is_err());
        }
    }

    /// 画面に出る文言は日本語（頭の英語の「invalid DIB:」などは付けない）。
    #[test]
    fn dib_error_messages_are_japanese() {
        assert_eq!(dib_to_webp(&[]).unwrap_err().to_string(), "画像のデータが壊れています（BITMAPINFOHEADERに満たない長さ）");
        assert_eq!(
            DibError::Unsupported("24/32bpp以外のビット深度").to_string(),
            "対応していない画像の形式です（24/32bpp以外のビット深度）"
        );
    }

    /// テスト用: 2x2の32bpp BI_RGBボトムアップDIB（build_dib_32bppと同じ正規形）。
    /// ピクセルは左上から R, G, B, 白
    fn canonical_32bpp_dib() -> Vec<u8> {
        // トップダウンRGBA: R G / B 白（アルファ無し扱い、予約=0）
        let rgb: Vec<u8> = vec![
            255, 0, 0, 0, 255, 0, // 上段: R, G
            0, 0, 255, 255, 255, 255, // 下段: B, 白
        ];
        build_dib_32bpp(2, 2, &rgb, false)
    }

    #[test]
    fn bmp_file_prepends_header_for_32bpp() {
        let dib = canonical_32bpp_dib();
        let bmp = dib_to_bmp_file(&dib).unwrap();
        assert_eq!(&bmp[0..2], b"BM");
        assert_eq!(u32_le(&bmp, 2) as usize, 14 + dib.len()); // bfSize
        assert_eq!(u32_le(&bmp, 10) as usize, 14 + 40); // bfOffBits（パレットなし）
        assert_eq!(&bmp[14..], &dib[..]);
    }

    #[test]
    fn bmp_file_offset_includes_palette_for_8bpp() {
        // 8bpp・biClrUsed=0 → パレット256色ぶんをbfOffBitsに含める
        // （dib_to_bmp_fileはピクセルを読まないためヘッダ40バイトのみで足りる）
        let mut dib = vec![0u8; 40];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes()); // biSize
        dib[14..16].copy_from_slice(&8u16.to_le_bytes()); // biBitCount
        let bmp = dib_to_bmp_file(&dib).unwrap();
        assert_eq!(u32_le(&bmp, 10) as usize, 14 + 40 + 256 * 4);
    }

    #[test]
    fn roundtrip_32bpp_opaque_is_byte_identical() {
        let original = canonical_32bpp_dib();
        let webp = dib_to_webp(&original).unwrap();
        // WebPのRIFFマジックを確認
        assert_eq!(&webp[0..4], b"RIFF");
        assert_eq!(&webp[8..12], b"WEBP");
        let restored = webp_to_dib(&webp).unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn roundtrip_24bpp_preserves_pixels() {
        // 3x2 24bpp: stride=12（9バイト+パディング3）、ボトムアップ
        let mut dib = Vec::new();
        dib.extend_from_slice(&40u32.to_le_bytes());
        dib.extend_from_slice(&3i32.to_le_bytes());
        dib.extend_from_slice(&2i32.to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&24u16.to_le_bytes());
        dib.extend_from_slice(&0u32.to_le_bytes());
        dib.extend_from_slice(&24u32.to_le_bytes()); // biSizeImage = 12*2
        dib.extend_from_slice(&[0u8; 16]); // 解像度・パレット関連
        // 下段（ボトムアップなので先に書く）: BGR = 青緑赤
        dib.extend_from_slice(&[255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0]);
        // 上段: 白黒白
        dib.extend_from_slice(&[255, 255, 255, 0, 0, 0, 255, 255, 255, 0, 0, 0]);

        let restored = webp_to_dib(&dib_to_webp(&dib).unwrap()).unwrap();

        // 出力はトップダウン相当で 白黒白 / 青緑赤 の32bpp（予約0）になるはず
        let expected = build_dib_32bpp(
            3,
            2,
            &[
                255, 255, 255, 0, 0, 0, 255, 255, 255, // 上段 RGB
                0, 0, 255, 0, 255, 0, 255, 0, 0, // 下段 RGB
            ],
            false,
        );
        assert_eq!(restored, expected);
    }

    #[test]
    fn roundtrip_32bpp_with_alpha_preserves_all_channels() {
        // アルファ 255/128/7/0 の混在。アルファ0のピクセルのRGB値が
        // エンコーダに破壊されないこと（ロスレス保証）もここで検証する
        let rgba: Vec<u8> = vec![
            10, 20, 30, 255, 40, 50, 60, 128, //
            70, 80, 90, 7, 100, 110, 120, 0,
        ];
        let original = build_dib_32bpp(2, 2, &rgba, true);
        let restored = webp_to_dib(&dib_to_webp(&original).unwrap()).unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn bitfields_standard_masks_accepted() {
        // biSize=40 + BI_BITFIELDS + 標準マスク: ピクセルはマスクの12バイト後
        let mut dib = Vec::new();
        dib.extend_from_slice(&40u32.to_le_bytes());
        dib.extend_from_slice(&1i32.to_le_bytes());
        dib.extend_from_slice(&1i32.to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&32u16.to_le_bytes());
        dib.extend_from_slice(&3u32.to_le_bytes()); // BI_BITFIELDS
        dib.extend_from_slice(&[0u8; 20]);
        for mask in STANDARD_MASKS {
            dib.extend_from_slice(&mask.to_le_bytes());
        }
        dib.extend_from_slice(&[1, 2, 3, 0]); // BGRX 1ピクセル

        let restored = webp_to_dib(&dib_to_webp(&dib).unwrap()).unwrap();
        // ピクセル部（ヘッダ40バイトの後）がBGR=1,2,3で復元されること
        assert_eq!(&restored[40..], &[1, 2, 3, 0]);
    }

    #[test]
    fn thumbnail_downscales_long_edge_to_128() {
        // 256x64 の単色画像 → 128x32 のサムネイルになること
        let rgb = vec![100u8; 256 * 64 * 3];
        let dib = build_dib_32bpp(256, 64, &rgb, false);
        let thumb = dib_to_thumbnail_webp(&dib).unwrap().unwrap();

        let mut decoder = WebPDecoder::new(Cursor::new(&thumb[..])).unwrap();
        assert_eq!(decoder.dimensions(), (128, 32));
        // 単色はボックス平均でも単色のまま
        let mut pixels = vec![0u8; decoder.output_buffer_size().unwrap()];
        decoder.read_image(&mut pixels).unwrap();
        assert!(pixels.iter().all(|&p| p == 100));
    }

    #[test]
    fn thumbnail_box_average_blends_pixels() {
        // 2x1ブロックの白黒縞（256x1）→ 128x1 で全ピクセルが平均値になること
        let mut rgb = Vec::with_capacity(256 * 3);
        for x in 0..256 {
            let v = if x % 2 == 0 { 0u8 } else { 255u8 };
            rgb.extend_from_slice(&[v, v, v]);
        }
        let dib = build_dib_32bpp(256, 1, &rgb, false);
        let thumb = dib_to_thumbnail_webp(&dib).unwrap().unwrap();

        let mut decoder = WebPDecoder::new(Cursor::new(&thumb[..])).unwrap();
        assert_eq!(decoder.dimensions(), (128, 1));
        let mut pixels = vec![0u8; decoder.output_buffer_size().unwrap()];
        decoder.read_image(&mut pixels).unwrap();
        assert!(pixels.iter().all(|&p| p == 127)); // (0+255)/2 切り捨て
    }

    #[test]
    fn thumbnail_skipped_for_small_images() {
        let rgb = vec![50u8; 128 * 64 * 3];
        let dib = build_dib_32bpp(128, 64, &rgb, false);
        assert!(dib_to_thumbnail_webp(&dib).unwrap().is_none());
    }

    #[test]
    fn webp_to_rgba_scaled_downscales_long_edge() {
        // 128x64のWebP → メニュー用32px指定で32x16に縮む
        let rgb = vec![80u8; 128 * 64 * 3];
        let dib = build_dib_32bpp(128, 64, &rgb, false);
        let webp = dib_to_webp(&dib).unwrap();

        let (w, h, rgba) = webp_to_rgba_scaled(&webp, 32).unwrap();
        assert_eq!((w, h), (32, 16));
        assert_eq!(rgba.len(), (32 * 16 * 4) as usize);
    }

    /// 縮小して使う WebP は、展開の前に寸法を確かめ、大きすぎれば展開せずに誤りを返す（壊れた・書き換えられた
    /// サムネイルのファイル）。ヘッダーだけの VP8L（4000 × 4000）で確かめる。
    #[test]
    fn webp_to_rgba_scaled_rejects_huge_dimensions_before_decoding() {
        let (w, h) = (4000u32, 4000u32);
        let bits = (w - 1) | ((h - 1) << 14);
        let mut chunk = vec![0x2f];
        chunk.extend(bits.to_le_bytes());
        let mut webp = b"RIFF".to_vec();
        webp.extend((4 + 8 + chunk.len() as u32 + 1).to_le_bytes());
        webp.extend(b"WEBPVP8L");
        webp.extend((chunk.len() as u32).to_le_bytes());
        webp.extend(&chunk);
        webp.push(0); // 奇数の長さの詰め物
        assert_eq!(webp_dimensions(&webp).unwrap(), (w, h), "前提: ヘッダーから寸法を読めない");
        assert!(matches!(webp_to_rgba_scaled(&webp, 32), Err(DibError::Invalid(_))));
    }

    #[test]
    fn webp_to_rgba_scaled_leaves_small_images_unchanged() {
        let rgb = vec![80u8; 20 * 10 * 3];
        let dib = build_dib_32bpp(20, 10, &rgb, false);
        let webp = dib_to_webp(&dib).unwrap();

        let (w, h, rgba) = webp_to_rgba_scaled(&webp, 32).unwrap();
        assert_eq!((w, h), (20, 10));
        assert_eq!(rgba.len(), (20 * 10 * 4) as usize);
    }

    #[test]
    fn unsupported_and_invalid_dibs_return_err_without_panic() {
        // 8bppパレット形式 → Unsupported
        let mut paletted = vec![0u8; 40 + 4 + 4];
        paletted[0..4].copy_from_slice(&40u32.to_le_bytes());
        paletted[4..8].copy_from_slice(&1i32.to_le_bytes());
        paletted[8..12].copy_from_slice(&1i32.to_le_bytes());
        paletted[14..16].copy_from_slice(&8u16.to_le_bytes());
        assert!(matches!(
            dib_to_webp(&paletted),
            Err(DibError::Unsupported(_))
        ));

        // ピクセル不足 → Invalid
        let truncated = &canonical_32bpp_dib()[..48];
        assert!(matches!(dib_to_webp(truncated), Err(DibError::Invalid(_))));

        // 全くの空・ゴミ入力 → panicせずErr
        assert!(dib_to_webp(&[]).is_err());
        assert!(dib_to_webp(&[0xFF; 39]).is_err());
        assert!(webp_to_dib(&[0xFF; 64]).is_err());
    }
}
