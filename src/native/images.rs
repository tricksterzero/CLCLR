//! ビューアの画像（一覧のサムネイル・プレビュー）を読み込む専用スレッド。
//!
//! UI スレッドは「この項目の画像を、この大きさに収めて」と頼むだけで、blob の読み込み・WebP の
//! 復号・縮小はこのスレッドで行う（描画中に blob の
//! 読み込みや復号をしない）。結果はアルファを掛けた BGRA（`AlphaBlend` にそのまま渡せる形）で
//! 返し、呼び出し側から受け取った `notify` で UI スレッドを起こす。
//!
//! 依頼には世代番号を付ける。UI 側は、一覧を隠したり画像の大きさを変えたりしたときに世代を
//! 進め、古い世代の結果を捨てる（ここでは世代を見ない）。
//! 一覧のサムネイルの依頼は、UI 側と共有する「今欲しいサムネイル」（`WantedThumbs`）に無い項目
//! （表示対象の切り替えや検索で一覧から消えた行）のものを読まずに捨てる。プレビューの依頼は、
//! 共有する「今欲しいプレビューの番号」（`WantedPreview`）と番号が違うもの（別の項目を選んだ、
//! 読み直しを頼み直した、テキストのプレビューに替えた、隠した）を読まずに捨てる（メモリだけに
//! 持つ画像の参照を早く手放し、古い画像を展開しないため）。スレッドは依頼の送り手がすべて無くなると
//! 終わる。

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use uuid::Uuid;

use crate::data::Format;
use crate::storage::{LoadedWebp, Storage};

/// 展開・縮小してよい画像の大きさの上限（RGBA に換算したバイト数。約 6,700 万画素）。超える画像は展開せず、
/// プレビューに省略を書き添える（`ImageContent::TooLarge`）。
/// ディスクの画像も、メモリだけに持つ画像も同じ上限。
pub const MAX_IMAGE_BYTES: u64 = 256 * 1024 * 1024;

/// 画像の読み込み元。
#[derive(Clone)]
pub enum ImageSource {
    /// 一覧用のサムネイル（`FormatMeta::thumb`、長辺128pxの WebP）
    Thumb(String),
    /// CF_DIB の blob（通常は `.webp`、WebP への変換に失敗したものは元の DIB のまま `.bin`）
    Blob(String),
    /// ディスクに書かない CF_DIB（完全メモリモードなど）。履歴の resident を写さずに共有し、この
    /// スレッドが中の CF_DIB から直接縮小する（メインスレッドで写さない）
    Resident(Arc<Vec<Format>>),
}

// `Format` は `Debug` を持たない（データの中身を出さない）ので、resident は形式の数だけを出す
impl std::fmt::Debug for ImageSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Thumb(name) => f.debug_tuple("Thumb").field(name).finish(),
            Self::Blob(name) => f.debug_tuple("Blob").field(name).finish(),
            Self::Resident(formats) => write!(f, "Resident({} formats)", formats.len()),
        }
    }
}

/// 何に使う画像か（結果の渡し先を UI 側で分けるため）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Purpose {
    ListThumb,
    Preview,
}

#[derive(Debug)]
pub struct ImageRequest {
    pub generation: u64,
    pub id: Uuid,
    pub purpose: Purpose,
    pub source: ImageSource,
    /// この大きさに収まるよう縮小する（拡大はしない）
    pub max_width: u32,
    pub max_height: u32,
    /// プレビューの依頼の番号（`WantedPreview` と比べる。サムネイルの依頼では使わない）
    pub preview_ticket: u64,
}

/// 読み込みの結果。
#[derive(Debug)]
pub struct LoadedImage {
    pub generation: u64,
    pub id: Uuid,
    pub purpose: Purpose,
    /// 依頼の番号をそのまま返す（UI 側は今欲しいプレビューの番号と照合し、同じ項目への古い依頼の
    /// 結果を捨てる）
    pub preview_ticket: u64,
    pub content: ImageContent,
}

#[derive(Debug)]
pub enum ImageContent {
    /// 縮小した画像。`bgra` はトップダウンで、アルファを掛けた BGRA。`source_*` は元の大きさ
    Pixels { width: u32, height: u32, bgra: Vec<u8>, source_width: u32, source_height: u32 },
    /// 元の大きさが `MAX_IMAGE_BYTES` を超えるので展開しなかった
    TooLarge { width: u32, height: u32 },
    /// 読めない・壊れている（プレビューの依頼のときだけ返す。UI 側は「（プレビューなし）」を出す）
    Unreadable,
}

/// 今欲しい一覧のサムネイル（項目の ID）。UI 側がサムネイルを頼むときに入れ、一覧から消えた
/// 行・隠したときに除く。読み込みスレッドは、ここに無い項目のサムネイルの依頼を読まずに捨てる。
/// ロックは出し入れと1件の確認の間だけ持つ。
pub type WantedThumbs = Arc<Mutex<HashSet<Uuid>>>;

/// 今欲しいプレビューの依頼の番号（0 は欲しいプレビューなし）。UI 側がプレビューを頼むたびに新しい
/// 番号を入れ、テキストのプレビューに替えたとき・隠したときに 0 にする。読み込みスレッドは、番号が
/// 違うプレビューの依頼を読まずに捨てる。
pub type WantedPreview = Arc<AtomicU64>;

/// 読み込みスレッドを起こす。`requests` の送り手がすべて無くなると終わる。
pub fn spawn(
    storage: Storage,
    requests: Receiver<ImageRequest>,
    results: Sender<LoadedImage>,
    wanted: WantedThumbs,
    wanted_preview: WantedPreview,
    notify: impl Fn() + Send + 'static,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("clclr-images".to_string())
        .spawn(move || {
            while let Ok(request) = requests.recv() {
                let unwanted = match request.purpose {
                    Purpose::ListThumb => !wanted.lock().map(|w| w.contains(&request.id)).unwrap_or(true),
                    Purpose::Preview => request.preview_ticket != wanted_preview.load(Ordering::SeqCst),
                };
                if unwanted {
                    // 捨てる（メモリだけの画像の参照もここで手放す）
                    continue;
                }
                // 読めない・壊れている画像は、サムネイルなら結果を返さない（UI 側は種別アイコンのままにする）。
                // プレビューなら読めなかったことを返す（UI 側は「（プレビューなし）」を出す）
                let image = load(&storage, &request).or_else(|| {
                    (request.purpose == Purpose::Preview).then(|| LoadedImage {
                        generation: request.generation,
                        id: request.id,
                        purpose: request.purpose,
                        preview_ticket: request.preview_ticket,
                        content: ImageContent::Unreadable,
                    })
                });
                if let Some(image) = image {
                    if results.send(image).is_err() {
                        break;
                    }
                    notify();
                }
            }
        })
        .expect("画像の読み込みスレッドを起動できない")
}

fn load(storage: &Storage, request: &ImageRequest) -> Option<LoadedImage> {
    load_limited(storage, request, MAX_IMAGE_BYTES)
}

/// `load` の本体（テストでは上限を小さくして確かめる）。展開・縮小の前に元の大きさを読み、上限を
/// 超えるなら展開しない。展開するときも原寸の RGBA は作らない（`dib::*_downscaled`）。
fn load_limited(storage: &Storage, request: &ImageRequest, limit: u64) -> Option<LoadedImage> {
    let content = match &request.source {
        // WebP は見出しと長さを確かめてから読む（上限を超えるものは全体を読まない。`Storage::load_webp`）
        ImageSource::Thumb(name) => loaded_webp(storage.load_thumbnail(name, limit)?, request, limit)?,
        ImageSource::Blob(name) if name.ends_with(".webp") => loaded_webp(storage.load_webp(name, limit)?, request, limit)?,
        ImageSource::Blob(name) => {
            // WebP にできなかった元の DIB（`.bin`）。先頭のヘッダーで大きさを判定してから全体を読む
            let (w, h) = crate::dib::dib_header_dimensions(&storage.load_blob_prefix(name, 64).ok()?).ok()?;
            if exceeds(w, h, limit) {
                ImageContent::TooLarge { width: w, height: h }
            } else {
                fitted_dib(&storage.load_blob(name).ok()?, request, limit)?
            }
        }
        ImageSource::Resident(formats) => {
            fitted_dib(&formats.iter().find(|f| f.format_name == "CF_DIB")?.data, request, limit)?
        }
    };
    Some(LoadedImage {
        generation: request.generation,
        id: request.id,
        purpose: request.purpose,
        preview_ticket: request.preview_ticket,
        content,
    })
}

/// 元の大きさ（RGBA 換算）が上限を超えるか。
fn exceeds(width: u32, height: u32, limit: u64) -> bool {
    u64::from(width) * u64::from(height) * 4 > limit
}

fn loaded_webp(loaded: LoadedWebp, request: &ImageRequest, limit: u64) -> Option<ImageContent> {
    match loaded {
        LoadedWebp::Data(webp) => fitted_webp(&webp, request, limit),
        LoadedWebp::TooLarge { width, height } => Some(ImageContent::TooLarge { width, height }),
    }
}

fn fitted_webp(webp: &[u8], request: &ImageRequest, limit: u64) -> Option<ImageContent> {
    let (w, h) = crate::dib::webp_dimensions(webp).ok()?;
    if exceeds(w, h, limit) {
        return Some(ImageContent::TooLarge { width: w, height: h });
    }
    let (tw, th) = fit_size(w, h, request.max_width, request.max_height);
    let (width, height, rgba) = crate::dib::webp_to_rgba_downscaled(webp, tw, th).ok()?;
    Some(ImageContent::Pixels { width, height, bgra: premultiplied_bgra(&rgba), source_width: w, source_height: h })
}

fn fitted_dib(dib: &[u8], request: &ImageRequest, limit: u64) -> Option<ImageContent> {
    let (w, h) = crate::dib::dib_header_dimensions(dib).ok()?;
    if exceeds(w, h, limit) {
        return Some(ImageContent::TooLarge { width: w, height: h });
    }
    let (tw, th) = fit_size(w, h, request.max_width, request.max_height);
    let (width, height, rgba) = crate::dib::dib_to_rgba_downscaled(dib, tw, th).ok()?;
    Some(ImageContent::Pixels { width, height, bgra: premultiplied_bgra(&rgba), source_width: w, source_height: h })
}

/// 縦横比を保って `max_width` × `max_height` に収まる大きさ（元より大きくはしない。最小1px）。
pub fn fit_size(w: u32, h: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (w, h);
    }
    let scale = (f64::from(max_width) / f64::from(w)).min(f64::from(max_height) / f64::from(h)).min(1.0);
    (((f64::from(w) * scale).round() as u32).max(1), ((f64::from(h) * scale).round() as u32).max(1))
}

/// RGBA（アルファを掛けていない）を、アルファを掛けた BGRA にする（`AlphaBlend` の
/// `AC_SRC_ALPHA` はアルファを掛けた値を前提とする）。
pub fn premultiplied_bgra(rgba: &[u8]) -> Vec<u8> {
    rgba.chunks_exact(4)
        .flat_map(|p| {
            let a = u32::from(p[3]);
            let mul = |c: u8| ((u32::from(c) * a + 127) / 255) as u8;
            [mul(p[2]), mul(p[1]), mul(p[0]), p[3]]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Entry, Format};
    use std::sync::mpsc;

    #[test]
    fn fit_size_keeps_aspect_and_never_enlarges() {
        assert_eq!(fit_size(1920, 1080, 480, 480), (480, 270));
        assert_eq!(fit_size(1080, 1920, 480, 480), (270, 480));
        assert_eq!(fit_size(1920, 1080, 800, 200), (356, 200));
        assert_eq!(fit_size(16, 16, 32, 32), (16, 16));
        assert_eq!(fit_size(4000, 1, 32, 32), (32, 1));
    }

    #[test]
    fn premultiplied_bgra_swaps_channels_and_multiplies_alpha() {
        assert_eq!(premultiplied_bgra(&[10, 20, 30, 255]), vec![30, 20, 10, 255]);
        assert_eq!(premultiplied_bgra(&[200, 100, 50, 0]), vec![0, 0, 0, 0]);
        assert_eq!(premultiplied_bgra(&[255, 255, 255, 128]), vec![128, 128, 128, 128]);
    }

    fn dib_32bpp(w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend(40u32.to_le_bytes());
        v.extend((w as i32).to_le_bytes());
        v.extend((h as i32).to_le_bytes());
        v.extend(1u16.to_le_bytes());
        v.extend(32u16.to_le_bytes());
        v.extend(0u32.to_le_bytes());
        v.extend((w * h * 4).to_le_bytes());
        v.extend([0u8; 16]);
        v.extend(std::iter::repeat([10u8, 20, 30, 255]).take((w * h) as usize).flatten());
        v
    }

    /// 実際に保存した画像を、サムネイルとプレビューの両方で読み込める。
    #[test]
    fn loader_reads_thumbnail_and_blob_and_fits_them() {
        let dir = std::env::temp_dir().join(format!("clclr-test-{}", Uuid::new_v4()));
        let storage = Storage::open(dir.clone()).unwrap();
        let entry = Entry::new(vec![Format { format_name: "CF_DIB".into(), format_id: 8, data: dib_32bpp(400, 200) }]);
        let meta = storage.save_entry(&entry).unwrap();
        let thumb = meta.formats[0].thumb.clone().expect("長辺128px超なのでサムネイルがある");
        let blob = meta.formats[0].blob.clone();

        let (req_tx, req_rx) = mpsc::channel();
        let (res_tx, res_rx) = mpsc::channel();
        let wanted: WantedThumbs = Arc::new(Mutex::new(HashSet::from([meta.id])));
        let handle = spawn(storage, req_rx, res_tx, wanted, Arc::new(AtomicU64::new(1)), || {});
        let request = |purpose, source, max_width, max_height| ImageRequest {
            generation: 7,
            id: meta.id,
            purpose,
            source,
            max_width,
            max_height,
            preview_ticket: 1,
        };
        req_tx.send(request(Purpose::ListThumb, ImageSource::Thumb(thumb), 32, 32)).unwrap();
        req_tx.send(request(Purpose::Preview, ImageSource::Blob(blob), 300, 300)).unwrap();
        req_tx.send(request(Purpose::ListThumb, ImageSource::Thumb("missing.webp".into()), 32, 32)).unwrap();
        req_tx.send(request(Purpose::Preview, ImageSource::Blob("missing.webp".into()), 300, 300)).unwrap();
        drop(req_tx);
        handle.join().unwrap();

        let results: Vec<LoadedImage> = res_rx.try_iter().collect();
        assert_eq!(results.len(), 3, "読めないサムネイルは結果を返さない");
        assert_eq!(results[0].purpose, Purpose::ListThumb);
        assert_eq!(pixels(&results[0]), (32, 16, 128, 64), "サムネイル（長辺128px）の大きさが元");
        assert_eq!(results[1].purpose, Purpose::Preview);
        assert_eq!(pixels(&results[1]), (300, 150, 400, 200));
        assert_eq!(results[2].purpose, Purpose::Preview);
        assert!(
            matches!(results[2].content, ImageContent::Unreadable),
            "読めないプレビューは読めなかったことを返す: {:?}",
            results[2].content
        );
        assert!(results.iter().all(|r| r.generation == 7 && r.id == meta.id));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 縮小した画像の（幅, 高さ, 元の幅, 元の高さ）。画素のバイト数も確かめる。
    fn pixels(image: &LoadedImage) -> (u32, u32, u32, u32) {
        let ImageContent::Pixels { width, height, bgra, source_width, source_height } = &image.content else {
            panic!("縮小した画像ではない: {:?}", image.content);
        };
        assert_eq!(bgra.len(), (width * height * 4) as usize);
        (*width, *height, *source_width, *source_height)
    }

    fn request(source: ImageSource, max: u32) -> ImageRequest {
        ImageRequest {
            generation: 1,
            id: Uuid::new_v4(),
            purpose: Purpose::Preview,
            source,
            max_width: max,
            max_height: max,
            preview_ticket: 0,
        }
    }

    /// 元の大きさ（RGBA 換算）が上限を超える画像は、WebP・元の DIB の blob・メモリだけの DIB の
    /// どれも展開せず、元の大きさだけを返す。上限ちょうどなら展開する。
    #[test]
    fn images_over_the_limit_are_not_decoded() {
        let dir = std::env::temp_dir().join(format!("clclr-test-{}", Uuid::new_v4()));
        let storage = Storage::open(dir.clone()).unwrap();
        let dib = dib_32bpp(400, 200);
        let webp_blob = storage.save_entry(&Entry::new(vec![Format { format_name: "CF_DIB".into(), format_id: 8, data: dib.clone() }]))
            .unwrap()
            .formats[0]
            .blob
            .clone();
        assert!(webp_blob.ends_with(".webp"));
        std::fs::write(dir.join("blobs").join("raw.bin"), &dib).unwrap();
        let resident = Arc::new(vec![Format { format_name: "CF_DIB".into(), format_id: 8, data: dib }]);
        let exact = 400 * 200 * 4;
        for source in [ImageSource::Blob(webp_blob), ImageSource::Blob("raw.bin".into()), ImageSource::Resident(resident)] {
            let over = load_limited(&storage, &request(source.clone(), 100), exact - 1).unwrap();
            assert!(
                matches!(over.content, ImageContent::TooLarge { width: 400, height: 200 }),
                "{source:?}: {:?}",
                over.content
            );
            let within = load_limited(&storage, &request(source, 100), exact).unwrap();
            assert_eq!(pixels(&within), (100, 50, 400, 200));
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 縮小しながら読む関数は、原寸を作ってから縮める今までの方法と同じ画素を返す（DIB はトップ
    /// ダウン・ボトムアップ、24/32bpp、アルファの有無。WebP はアルファの有無）。
    #[test]
    fn downscaled_decoding_matches_decode_then_scale() {
        let mut state = 12345u32;
        let mut next = || {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (state >> 16) as u8
        };
        let dib = |w: u32, h: u32, bpp: u16, top_down: bool, alpha: bool, next: &mut dyn FnMut() -> u8| {
            let stride = (w as usize * bpp as usize).div_ceil(32) * 4;
            let mut v = Vec::new();
            v.extend(40u32.to_le_bytes());
            v.extend((w as i32).to_le_bytes());
            v.extend((if top_down { -(h as i32) } else { h as i32 }).to_le_bytes());
            v.extend(1u16.to_le_bytes());
            v.extend(bpp.to_le_bytes());
            v.extend([0u8; 24]);
            for _ in 0..h {
                let mut row: Vec<u8> = (0..stride).map(|_| next()).collect();
                if bpp == 32 && !alpha {
                    row.chunks_exact_mut(4).for_each(|px| px[3] = 0);
                }
                v.extend(row);
            }
            v
        };
        for (bpp, top_down, alpha) in [(24, false, false), (24, true, false), (32, false, false), (32, true, true)] {
            let data = dib(37, 23, bpp, top_down, alpha, &mut next);
            let (w, h, full) = crate::dib::dib_to_rgba(&data).unwrap();
            for (tw, th) in [(37, 23), (10, 6), (1, 1), (36, 22)] {
                let expected = crate::dib::scale_rgba_to(w, h, &full, tw, th);
                assert_eq!(
                    crate::dib::dib_to_rgba_downscaled(&data, tw, th).unwrap(),
                    (tw, th, expected),
                    "DIB {bpp}bpp top_down={top_down} alpha={alpha} → {tw}×{th}"
                );
            }
            let webp = crate::dib::dib_to_webp(&data).unwrap();
            let (w, h, full) = crate::dib::webp_to_rgba(&webp).unwrap();
            for (tw, th) in [(37, 23), (10, 6)] {
                let expected = crate::dib::scale_rgba_to(w, h, &full, tw, th);
                assert_eq!(crate::dib::webp_to_rgba_downscaled(&webp, tw, th).unwrap(), (tw, th, expected), "WebP → {tw}×{th}");
            }
        }
    }

    /// メモリだけに持つ画像は、履歴の resident を写さずに共有したまま読む。
    #[test]
    fn resident_image_is_read_without_copy() {
        let dir = std::env::temp_dir().join(format!("clclr-test-{}", Uuid::new_v4()));
        let storage = Storage::open(dir.clone()).unwrap();
        let resident = Arc::new(vec![Format { format_name: "CF_DIB".into(), format_id: 8, data: dib_32bpp(64, 32) }]);
        let loaded = load(&storage, &request(ImageSource::Resident(Arc::clone(&resident)), 16)).unwrap();
        assert_eq!(pixels(&loaded), (16, 8, 64, 32));
        assert_eq!(Arc::strong_count(&resident), 1, "依頼が終わった後も resident の参照が残っている");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 今欲しいサムネイルに無い項目のサムネイルの依頼は、読まずに捨てる（結果を返さない）。
    /// プレビューの依頼は、欲しいサムネイルに無くても読むが、今欲しいプレビューの番号と違うもの
    /// （別の項目を選んだ・頼み直した後の古い依頼）は読まずに捨てる。
    #[test]
    fn loader_skips_thumbnails_and_previews_no_longer_wanted() {
        let dir = std::env::temp_dir().join(format!("clclr-test-{}", Uuid::new_v4()));
        let storage = Storage::open(dir.clone()).unwrap();
        let save = || {
            let entry = Entry::new(vec![Format { format_name: "CF_DIB".into(), format_id: 8, data: dib_32bpp(400, 200) }]);
            storage.save_entry(&entry).unwrap()
        };
        let (kept, dropped) = (save(), save());
        let request = |meta: &crate::storage::EntryMeta, purpose, preview_ticket| ImageRequest {
            generation: 1,
            id: meta.id,
            purpose,
            source: match purpose {
                Purpose::ListThumb => ImageSource::Thumb(meta.formats[0].thumb.clone().unwrap()),
                Purpose::Preview => ImageSource::Blob(meta.formats[0].blob.clone()),
            },
            max_width: 32,
            max_height: 32,
            preview_ticket,
        };
        let (req_tx, req_rx) = mpsc::channel();
        let (res_tx, res_rx) = mpsc::channel();
        let wanted: WantedThumbs = Arc::new(Mutex::new(HashSet::from([kept.id])));
        // 今欲しいプレビューは番号 2 の依頼（番号 1 は、その前に頼んで取り替えられた依頼）
        let wanted_preview: WantedPreview = Arc::new(AtomicU64::new(2));
        req_tx.send(request(&dropped, Purpose::ListThumb, 0)).unwrap();
        req_tx.send(request(&kept, Purpose::ListThumb, 0)).unwrap();
        req_tx.send(request(&kept, Purpose::Preview, 1)).unwrap();
        req_tx.send(request(&dropped, Purpose::Preview, 2)).unwrap();
        drop(req_tx);
        spawn(storage.clone(), req_rx, res_tx, wanted, wanted_preview, || {}).join().unwrap();

        let got: Vec<(Uuid, Purpose)> = res_rx.try_iter().map(|r| (r.id, r.purpose)).collect();
        assert_eq!(got, vec![(kept.id, Purpose::ListThumb), (dropped.id, Purpose::Preview)]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
