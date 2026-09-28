//! ポップアップメニュー項目のツールチップ（全文／先頭部分＋サムネイル）。
//!
//! メニューはネイティブ`TrackPopupMenu`でツールチップ機能を持たないため、
//! `WM_MENUSELECT`（マウス・キーボードどちらの選択変化でも届く）を契機に、
//! 待ち時間のタイマー満了後に自前の小窓を項目の横へ出す（C版`ToolTip.c`相当）。
//! 窓は`WS_EX_NOACTIVATE`でフォーカスを奪わず、マウス操作は素通しにする
//! （メニュー選択後のフォーカス復帰・ペースト送出を妨げないため）。
//!
//! 状態はホットキースレッドのthread_localに持つ。`TrackPopupMenu`のモーダル
//! ループ中は、`show_popup_menu`が持つ`&mut HotkeyContext`をまたいで`wndproc`が
//! 再入するため、コンテキスト経由ではなく自己完結したセッションにしている。
//!
//! 表示内容は選択時に遅延読み込みする（メニュー構築時のI/Oを増やさない）。
//! 画像は一覧と共通のサムネイル、CF_HDROPはファイル名一覧、それ以外の
//! CF_UNICODETEXTは全文（上限まで）。画像とテキストを両方持つエントリは両方出す。

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::Arc;
use std::time::{Duration, Instant};

use uuid::Uuid;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateBitmap, CreateCompatibleDC, CreateFontIndirectW, DeleteDC,
    DeleteObject, DrawTextW, EndPaint, FillRect, FrameRect, GetDC, GetMonitorInfoW, GetStockObject,
    GetSysColor, GetSysColorBrush, GetTextExtentExPointW, InvalidateRect, MonitorFromPoint, ReleaseDC, SelectObject,
    SetBkMode, SetTextColor, COLOR_INFOBK, COLOR_INFOTEXT, COLOR_WINDOWFRAME, DEFAULT_GUI_FONT,
    DT_CALCRECT, DT_EXPANDTABS, DT_LEFT, DT_NOPREFIX, DT_WORDBREAK, HBITMAP, HFONT, HGDIOBJ, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, SRCCOPY, TRANSPARENT, HALFTONE, SetBrushOrgEx,
    SetStretchBltMode, StretchBlt,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, SystemParametersInfoForDpi, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetMenuItemCount, GetMenuItemID,
    GetMenuItemRect, GetWindowLongPtrW, KillTimer, RegisterClassW, SetTimer, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, CS_DROPSHADOW, GWLP_USERDATA, HMENU,
    HWND_TOPMOST, MA_NOACTIVATE, MF_POPUP, MF_SEPARATOR, NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS,
    SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, WM_MOUSEACTIVATE,
    WM_NCDESTROY, WM_NCHITTEST, WM_PAINT, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::config::MenuTooltipConfig;
use crate::ops::Core;
use crate::service::HistoryService;
use crate::store;

/// ツールチップ表示待ちタイマーのID（ホットキー窓上。二度押し判定タイマーは1）
pub const TIMER_ID: usize = 2;

const CLASS_NAME: PCWSTR = w!("CLCLR_MenuTooltip");
/// 窓内側の余白・画像とテキストの間隔（96 DPI 基準の px。表示するモニターの DPI に合わせて使う）
const PAD: i32 = 6;
const GAP: i32 = 6;
/// テキストの折り返し幅（96 DPI 基準の px）
const MAX_TEXT_WIDTH: i32 = 480;
/// 項目の右（左）に空ける隙間（96 DPI 基準の px）
const ANCHOR_GAP: i32 = 4;

/// 96 DPI 基準の長さを `dpi` に合わせる（四捨五入）。
pub(crate) fn scale_for_dpi(v: i32, dpi: u32) -> i32 {
    ((i64::from(v) * i64::from(dpi) + 48) / 96) as i32
}

/// 画面上の点があるモニターの DPI（取れなければ 96）。プロセスは PerMonitorV2 なので、
/// メニューやツールチップを出す位置のモニターに合わせて大きさを決める。
pub(crate) fn dpi_at_point(x: i32, y: i32) -> u32 {
    unsafe {
        let monitor = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let (mut dx, mut dy) = (0u32, 0u32);
        match GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) {
            Ok(()) if dx > 0 => dx,
            _ => 96,
        }
    }
}
/// ファイル一覧で出すファイル数の上限（C版`TOOLTIP_MAX`）
const MAX_FILES: usize = 100;

// --- Content ---

/// ツールチップに出す画像（RGBA、左上原点）。
pub struct TooltipImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// ツールチップの内容。両方`None`のときは表示しない。
pub struct TooltipContent {
    pub text: Option<String>,
    pub image: Option<TooltipImage>,
}

/// メニュー項目に対応するエントリ（コマンドID=インデックス+1）。
#[derive(Clone, Copy)]
pub struct TooltipTarget {
    pub id: Uuid,
    pub pinned: bool,
}

/// 表示用にテキストを切り詰める。`max_lines`行または`max_chars`文字（改行も1文字と
/// 数える）のどちらか先に達した所で切り、切ったら末尾に「…」を付ける。
pub fn truncate_tooltip_text(text: &str, max_chars: usize, max_lines: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    let mut truncated = false;
    for (i, line) in text.lines().enumerate() {
        if i >= max_lines {
            truncated = true;
            break;
        }
        if i > 0 {
            out.push('\n');
            used += 1;
        }
        let remaining = max_chars.saturating_sub(used);
        let len = line.chars().count();
        if len > remaining {
            out.extend(line.chars().take(remaining));
            truncated = true;
            break;
        }
        out.push_str(line);
        used += len;
    }
    if truncated {
        out.push('…');
    }
    out
}

/// ファイルパス一覧を1行1件のテキストにする（`MAX_FILES`件まで、超過は件数を添える）。
fn file_list_text(paths: &[String], max_chars: usize, max_lines: usize) -> String {
    let mut lines: Vec<String> = paths.iter().take(MAX_FILES).cloned().collect();
    if paths.len() > MAX_FILES {
        lines.push(format!("…ほか{}件", paths.len() - MAX_FILES));
    }
    truncate_tooltip_text(&lines.join("\n"), max_chars, max_lines)
}

/// RGBAを背景色の上に合成して不透明にする（`CreateBitmap`+`BitBlt`はアルファを
/// 使わないため、透過部分が黒くならないよう先に合成しておく）。
fn flatten_on_background(rgba: &[u8], bg: (u8, u8, u8)) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for px in rgba.chunks_exact(4) {
        let a = u32::from(px[3]);
        let blend = |fg: u8, bg: u8| ((u32::from(fg) * a + u32::from(bg) * (255 - a) + 127) / 255) as u8;
        out.extend_from_slice(&[blend(px[0], bg.0), blend(px[1], bg.1), blend(px[2], bg.2), 255]);
    }
    out
}

/// サムネイルが無い画像（サムネイルを持たない履歴・サムネイル生成失敗・residentの生DIB）を原寸で
/// 展開してよい元データサイズの上限（約360x360px相当の32bpp）。これを超える画像は
/// 出さない（巨大画像の全量読込・復号を避ける）。
const MAX_FALLBACK_DIB_BYTES: usize = 512 * 1024;
/// ファイル一覧（CF_HDROP）で読む先頭バイト数。100件×パス長を十分に含む。
const HDROP_READ_CAP: usize = 256 * 1024;

/// サービスのロック中に取り出す読み込み元の軽量スナップショット。blobの読込や
/// 画像の復号はロックを手放した後で行う（巨大データでメニュー操作・クリップボード
/// 監視・UIをロック待ちで止めないため）。residentは巨大になりうるので、必要な
/// 先頭部分だけを複製する。
struct Snapshot {
    meta: crate::storage::EntryMeta,
    storage: crate::storage::Storage,
    resident_text: Option<Vec<u8>>,
    resident_hdrop: Option<Vec<u8>>,
    /// 原寸展開してよいサイズ以下のresident画像（生DIB）のみ
    resident_dib: Option<Vec<u8>>,
}

fn text_read_cap(cfg: &MenuTooltipConfig) -> usize {
    // 文字数上限までしか使わない（UTF-16では1文字が最大2コードユニット=4バイト）
    (cfg.max_chars as usize).saturating_mul(4).max(4)
}

/// 項目に対応するエントリの読み込み元（サービスのロックの中で取る。resident は `Arc` の写し
/// だけで、先頭の切り出しはロックの外の `Snapshot::from_source` で行う）。
struct SnapshotSource {
    meta: crate::storage::EntryMeta,
    storage: crate::storage::Storage,
    resident: Arc<Vec<crate::data::Format>>,
}

/// 項目に対応するエントリの読み込み元を取り出す（ロック中に呼ぶ。I/Oはしない）。
fn snapshot_source(service: &HistoryService, target: TooltipTarget) -> Option<SnapshotSource> {
    let (meta, resident) = if target.pinned {
        (store::find_item(&service.pinned, target.id)?, Arc::default())
    } else {
        let item = service.history.get_by_id(target.id)?;
        (&item.meta, Arc::clone(&item.resident))
    };
    Some(SnapshotSource { meta: meta.clone(), storage: service.storage_handle(), resident })
}

impl Snapshot {
    /// residentの必要な先頭部分だけを写す（ロックの外で呼ぶ）。
    fn from_source(source: SnapshotSource, cfg: &MenuTooltipConfig) -> Self {
        let resident = &source.resident;
        let head = |name: &str, cap: usize| {
            resident
                .iter()
                .find(|f| f.format_name == name)
                .map(|f| f.data[..f.data.len().min(cap)].to_vec())
        };
        Snapshot {
            resident_text: head("CF_UNICODETEXT", text_read_cap(cfg)),
            resident_hdrop: head("CF_HDROP", HDROP_READ_CAP),
            resident_dib: resident
                .iter()
                .find(|f| f.format_name == "CF_DIB" && f.data.len() <= MAX_FALLBACK_DIB_BYTES)
                .map(|f| f.data.clone()),
            meta: source.meta,
            storage: source.storage,
        }
    }
}

/// ツールチップ内容を読み込む（ロックの外で呼ぶ）。
fn build_content(snap: &Snapshot, cfg: &MenuTooltipConfig) -> Option<TooltipContent> {
    let max_chars = cfg.max_chars as usize;
    let max_lines = cfg.max_lines as usize;
    let format = |name: &str| snap.meta.formats.iter().find(|f| f.format_name == name);
    // persisted形式の先頭だけを読む（residentが先。residentはsave=falseの形式でmetaには無い）
    let head = |resident: &Option<Vec<u8>>, name: &str, cap: usize| {
        resident
            .clone()
            .or_else(|| snap.storage.load_blob_prefix(&format(name)?.blob, cap).ok())
    };

    let image = load_image(snap);
    // CF_HDROPが壊れていて一覧が作れない場合は、テキストがあればそちらへフォールバックする
    let files = head(&snap.resident_hdrop, "CF_HDROP", HDROP_READ_CAP)
        .map(|data| crate::hdrop::parse_hdrop(&data))
        .filter(|paths| !paths.is_empty())
        .map(|paths| file_list_text(&paths, max_chars, max_lines));
    let text = files.or_else(|| {
        head(&snap.resident_text, "CF_UNICODETEXT", text_read_cap(cfg))
            .map(|data| truncate_tooltip_text(&crate::data::utf16_text(&data), max_chars, max_lines))
            .filter(|t| !t.trim().is_empty())
    });
    if text.is_none() && image.is_none() {
        return None;
    }
    Some(TooltipContent { text, image })
}

/// 画像は一覧と共通のサムネイル（長辺128px）を優先する。サムネイルが無い場合は
/// 元データが`MAX_FALLBACK_DIB_BYTES`以下のときだけ復号する。いずれも長辺128pxへ
/// 縮小して返す（窓が画像の原寸で巨大になるのを避ける）。
fn load_image(snap: &Snapshot) -> Option<TooltipImage> {
    let edge = crate::dib::THUMB_LONG_EDGE;
    let decoded = if let Some(webp) = snap
        .meta
        .formats
        .iter()
        .find_map(|f| f.thumb.as_deref())
        .and_then(|name| snap.storage.load_thumbnail(name))
    {
        crate::dib::webp_to_rgba_scaled(&webp, edge)
    } else if let Some(dib) = &snap.resident_dib {
        crate::dib::dib_to_rgba_scaled(dib, edge)
    } else {
        let fm = snap.meta.formats.iter().find(|f| f.format_name == "CF_DIB")?;
        if fm.size > MAX_FALLBACK_DIB_BYTES as u64 {
            return None;
        }
        let raw = snap.storage.load_blob(&fm.blob).ok()?;
        if fm.blob.ends_with(".webp") {
            crate::dib::webp_to_rgba_scaled(&raw, edge)
        } else {
            crate::dib::dib_to_rgba_scaled(&raw, edge)
        }
    };
    decoded
        .ok()
        .map(|(width, height, rgba)| TooltipImage { width, height, rgba })
}

// --- Placement ---

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Bounds {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

/// ツールチップ窓の左上座標を決める。項目の右隣に出し、作業領域の右端を越える
/// なら左隣へ反転し、それでも収まらなければ作業領域内へ寄せる。縦は項目の上端に
/// 揃え、下端を越えるなら押し上げる。
fn place_tooltip(item: Bounds, size: (i32, i32), work: Bounds, anchor_gap: i32) -> (i32, i32) {
    let (w, h) = size;
    let mut x = item.right + anchor_gap;
    if x + w > work.right {
        x = item.left - w - anchor_gap;
    }
    x = x.min(work.right - w).max(work.left);
    let y = item.top.min(work.bottom - h).max(work.top);
    (x, y)
}

// --- Window ---

/// 描画時に参照する状態。窓の`GWLP_USERDATA`に`Box`で持たせ、`WM_NCDESTROY`で解放する。
struct PaintState {
    /// `font` を作った DPI（表示するモニターの DPI が変わったら作り直す）
    dpi: u32,
    font: HFONT,
    /// `dpi` に合わせた余白・間隔・折り返し幅
    pad: i32,
    gap: i32,
    max_text_width: i32,
    text: Vec<u16>,
    text_size: (i32, i32),
    /// 画像のビットマップと、その元の大きさ
    image: Option<(HBITMAP, i32, i32)>,
    /// 画像を描く大きさ（元の大きさを `dpi` に合わせたもの）
    image_draw: (i32, i32),
    size: (i32, i32),
}

impl PaintState {
    fn free_image(&mut self) {
        if let Some((bitmap, _, _)) = self.image.take() {
            unsafe {
                let _ = DeleteObject(bitmap.into());
            }
        }
    }
}

const TEXT_FLAGS: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT =
    windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT(
        DT_LEFT.0 | DT_WORDBREAK.0 | DT_NOPREFIX.0 | DT_EXPANDTABS.0,
    );

/// `dpi` に合わせたステータスバー用のフォント（ツールチップの文字に使う）。
fn create_font(dpi: u32) -> HFONT {
    unsafe {
        let mut ncm = NONCLIENTMETRICSW {
            cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };
        let ok = SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            ncm.cbSize,
            Some((&mut ncm as *mut NONCLIENTMETRICSW).cast::<c_void>()),
            0,
            dpi,
        )
        .is_ok();
        if ok {
            CreateFontIndirectW(&ncm.lfStatusFont)
        } else {
            HFONT(GetStockObject(DEFAULT_GUI_FONT).0)
        }
    }
}

unsafe extern "system" fn tooltip_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            // マウス操作は下のメニューへ素通しし、クリックでメニューが閉じる等の
            // 副作用や、アクティブ化によるフォーカス移動を起こさない
            WM_NCHITTEST => LRESULT(-1), // HTTRANSPARENT
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_PAINT => {
                let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const PaintState;
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                if let Some(st) = state.as_ref() {
                    paint(hdc, st);
                }
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PaintState;
                if !state.is_null() {
                    let mut st = Box::from_raw(state);
                    st.free_image();
                    let _ = DeleteObject(st.font.into());
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

unsafe fn paint(hdc: windows::Win32::Graphics::Gdi::HDC, st: &PaintState) {
    unsafe {
        let frame = RECT { left: 0, top: 0, right: st.size.0, bottom: st.size.1 };
        FillRect(hdc, &frame, GetSysColorBrush(COLOR_INFOBK));
        FrameRect(hdc, &frame, GetSysColorBrush(COLOR_WINDOWFRAME));
        let mut y = st.pad;
        if let Some((bitmap, w, h)) = st.image {
            let (dw, dh) = st.image_draw;
            let mem = CreateCompatibleDC(Some(hdc));
            let old = SelectObject(mem, HGDIOBJ(bitmap.0));
            if (dw, dh) == (w, h) {
                let _ = BitBlt(hdc, st.pad, y, w, h, Some(mem), 0, 0, SRCCOPY);
            } else {
                // 100% より大きい倍率のモニターでは、画像も倍率に合わせて広げる
                SetStretchBltMode(hdc, HALFTONE);
                let _ = SetBrushOrgEx(hdc, 0, 0, None);
                let _ = StretchBlt(hdc, st.pad, y, dw, dh, Some(mem), 0, 0, w, h, SRCCOPY);
            }
            SelectObject(mem, old);
            let _ = DeleteDC(mem);
            y += dh + st.gap;
        }
        if !st.text.is_empty() {
            let old = SelectObject(hdc, HGDIOBJ(st.font.0));
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, COLORREF(GetSysColor(COLOR_INFOTEXT)));
            let mut rect = RECT {
                left: st.pad,
                top: y,
                right: st.pad + st.text_size.0,
                bottom: y + st.text_size.1,
            };
            let mut buf = st.text.clone();
            DrawTextW(hdc, &mut buf, &mut rect, TEXT_FLAGS);
            SelectObject(hdc, old);
        }
    }
}

/// ツールチップ窓。ホットキースレッド上で生成・使用・破棄する。
struct TooltipWindow {
    hwnd: HWND,
}

impl TooltipWindow {
    fn create() -> Option<Self> {
        unsafe {
            let hinstance = GetModuleHandleW(None).ok()?.into();
            let class = WNDCLASSW {
                lpfnWndProc: Some(tooltip_wndproc),
                hInstance: hinstance,
                lpszClassName: CLASS_NAME,
                style: CS_DROPSHADOW,
                ..Default::default()
            };
            RegisterClassW(&class);
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_TRANSPARENT,
                CLASS_NAME,
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(hinstance),
                None,
            )
            .ok()?;
            // 大きさとフォントは、表示するたびにそのモニターの DPI に合わせ直す（show）
            let state = Box::into_raw(Box::new(PaintState {
                dpi: 96,
                font: create_font(96),
                pad: PAD,
                gap: GAP,
                max_text_width: MAX_TEXT_WIDTH,
                text: Vec::new(),
                text_size: (0, 0),
                image: None,
                image_draw: (0, 0),
                size: (0, 0),
            }));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
            Some(Self { hwnd })
        }
    }

    /// 描画状態への排他参照。状態は窓の`GWLP_USERDATA`にあり、`wndproc`（WM_PAINT）も
    /// 読むため、`&mut self`を要求して同時に2本取れないようにする。
    fn state(&mut self) -> Option<&mut PaintState> {
        unsafe { (GetWindowLongPtrW(self.hwnd, GWLP_USERDATA) as *mut PaintState).as_mut() }
    }

    /// 内容を差し替えて、項目`anchor`の横に表示する（フォーカスは奪わない）。
    fn show(&mut self, content: &TooltipContent, anchor: Bounds) {
        let hwnd = self.hwnd;
        let Some(st) = self.state() else {
            return;
        };
        // 項目があるモニターの DPI に合わせる（フォントは DPI が変わったときだけ作り直す）
        let dpi = dpi_at_point((anchor.left + anchor.right) / 2, (anchor.top + anchor.bottom) / 2);
        if st.dpi != dpi {
            let old = std::mem::replace(&mut st.font, create_font(dpi));
            unsafe {
                let _ = DeleteObject(old.into());
            }
            st.dpi = dpi;
        }
        st.pad = scale_for_dpi(PAD, dpi);
        st.gap = scale_for_dpi(GAP, dpi);
        st.max_text_width = scale_for_dpi(MAX_TEXT_WIDTH, dpi);
        let anchor_gap = scale_for_dpi(ANCHOR_GAP, dpi);
        st.free_image();
        st.text = content
            .text
            .as_deref()
            .map(|t| t.encode_utf16().collect())
            .unwrap_or_default();
        st.text_size = Self::measure_text(hwnd, st);

        let bg = unsafe { GetSysColor(COLOR_INFOBK) };
        let bg = ((bg & 0xFF) as u8, ((bg >> 8) & 0xFF) as u8, ((bg >> 16) & 0xFF) as u8);
        if let Some(img) = &content.image {
            let flat = flatten_on_background(&img.rgba, bg);
            let pixels = crate::tray::rgba_to_argb_pixels(&flat);
            let bitmap = unsafe {
                CreateBitmap(img.width as i32, img.height as i32, 1, 32, Some(pixels.as_ptr().cast()))
            };
            if !bitmap.is_invalid() {
                st.image = Some((bitmap, img.width as i32, img.height as i32));
            }
        }
        st.image_draw = st
            .image
            .map_or((0, 0), |(_, w, h)| (scale_for_dpi(w, dpi), scale_for_dpi(h, dpi)));

        let (img_w, img_h) = st.image_draw;
        let inner_w = img_w.max(st.text_size.0);
        let mut inner_h = img_h + st.text_size.1;
        if img_h > 0 && st.text_size.1 > 0 {
            inner_h += st.gap;
        }
        if inner_w == 0 || inner_h == 0 {
            return;
        }
        let size = (inner_w + st.pad * 2, inner_h + st.pad * 2);
        st.size = size;
        // 以降はstを使わない（SetWindowPosが同期でwndprocへメッセージを送っても
        // 描画状態への参照と重ならないようにする）

        let work = work_area(anchor);
        let (x, y) = place_tooltip(anchor, size, work, anchor_gap);
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                x,
                y,
                size.0,
                size.1,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            let _ = InvalidateRect(Some(hwnd), None, true);
        }
    }

    /// テキストの折り返し後の大きさを測る。`DT_WORDBREAK`は単語の途中では折らず、
    /// 空白のない長文（URL・ハッシュ・長いパス等）は`MAX_TEXT_WIDTH`を超えて横に
    /// 伸びる（実機で1024文字が8192pxになるのを確認）ため、先に広すぎる単語へ
    /// 文字単位の改行を入れてから測る。
    fn measure_text(hwnd: HWND, st: &mut PaintState) -> (i32, i32) {
        if st.text.is_empty() {
            return (0, 0);
        }
        unsafe {
            let hdc = GetDC(Some(hwnd));
            let old = SelectObject(hdc, HGDIOBJ(st.font.0));
            st.text = break_long_words(hdc, &st.text, st.max_text_width);
            let mut rect = RECT { left: 0, top: 0, right: st.max_text_width, bottom: 0 };
            let mut buf = st.text.clone();
            DrawTextW(
                hdc,
                &mut buf,
                &mut rect,
                windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT(TEXT_FLAGS.0 | DT_CALCRECT.0),
            );
            SelectObject(hdc, old);
            ReleaseDC(Some(hwnd), hdc);
            (rect.right - rect.left, rect.bottom - rect.top)
        }
    }

    fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }
}

impl Drop for TooltipWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// `hdc`に選択済みのフォントで`max_width`pxを超える「単語」（空白・タブ・改行で
/// 区切った塊）に、収まる文字数ごとの改行を入れる。それ以外はそのまま。
/// サロゲートペアの途中では折らない。
unsafe fn break_long_words(hdc: windows::Win32::Graphics::Gdi::HDC, text: &[u16], max_width: i32) -> Vec<u16> {
    let is_space = |u: u16| matches!(u, 0x20 | 0x09 | 0x0A | 0x0D);
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if is_space(text[i]) {
            out.push(text[i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < text.len() && !is_space(text[i]) {
            i += 1;
        }
        let mut word = &text[start..i];
        while !word.is_empty() {
            let mut fit = 0i32;
            let mut size = SIZE::default();
            unsafe {
                let _ = GetTextExtentExPointW(
                    hdc,
                    PCWSTR(word.as_ptr()),
                    word.len() as i32,
                    max_width,
                    Some(&mut fit),
                    None,
                    &mut size,
                );
            }
            let mut n = (fit.max(1) as usize).min(word.len());
            // 高位サロゲートで切らない（1文字が2コードユニット）
            if n < word.len() && (0xD800..0xDC00).contains(&word[n - 1]) && n > 1 {
                n -= 1;
            }
            out.extend_from_slice(&word[..n]);
            word = &word[n..];
            if !word.is_empty() {
                out.push(0x0A);
            }
        }
    }
    out
}

/// `anchor`があるモニタの作業領域（取れなければ`anchor`を含む大きな領域）。
fn work_area(anchor: Bounds) -> Bounds {
    unsafe {
        let center = POINT {
            x: (anchor.left + anchor.right) / 2,
            y: (anchor.top + anchor.bottom) / 2,
        };
        let monitor = MonitorFromPoint(center, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            Bounds {
                left: info.rcWork.left,
                top: info.rcWork.top,
                right: info.rcWork.right,
                bottom: info.rcWork.bottom,
            }
        } else {
            // 負座標のモニタでも`anchor`が領域内に入るよう、原点0ではなくanchorに合わせる
            Bounds {
                left: anchor.left.min(0),
                top: anchor.top.min(0),
                right: i32::MAX / 2,
                bottom: i32::MAX / 2,
            }
        }
    }
}

// --- Session (ポップアップメニュー1回の表示中) ---

struct Session {
    /// タイマーを張る窓（ホットキー窓）
    owner: HWND,
    core: Core,
    cfg: MenuTooltipConfig,
    targets: Vec<TooltipTarget>,
    /// 初回表示時に生成する
    window: Option<TooltipWindow>,
    /// タイマー満了待ちの選択項目（コマンドID, 属するメニュー）
    pending: Option<(u32, HMENU)>,
    /// `pending`の待ちを始めた時刻。`KillTimer`はすでにキューへ入った古い`WM_TIMER`を
    /// 取り除かないため、直前の項目由来のタイマーが新しい項目の待ち時間を飛ばして
    /// 表示させてしまうのを、経過時間で見分けて防ぐ
    armed_at: Option<Instant>,
}

/// タイマー満了の判定で許す早着の余裕（`SetTimer`の分解能は約15.6ms）。
const TIMER_SLACK: Duration = Duration::from_millis(20);

/// `WM_TIMER`を受けた時点で、待ち時間`delay`に対して`elapsed`しか経っていなければ
/// 残りの待ち時間を返す（=古い項目由来のタイマーなので、張り直して待つ）。
/// 分解能ぶんの早着（`TIMER_SLACK`以内）は満了とみなして`None`。
fn remaining_wait(delay: Duration, elapsed: Duration) -> Option<Duration> {
    delay.checked_sub(elapsed).filter(|r| *r > TIMER_SLACK)
}

impl Session {
    fn cancel(&mut self) {
        self.pending = None;
        self.armed_at = None;
        unsafe {
            let _ = KillTimer(Some(self.owner), TIMER_ID);
        }
        if let Some(window) = &self.window {
            window.hide();
        }
    }
}

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
}

/// メニュー表示の直前に呼ぶ。設定が無効なら何もしない。
pub fn begin_session(
    owner: HWND,
    core: Core,
    cfg: MenuTooltipConfig,
    targets: Vec<TooltipTarget>,
) {
    if !cfg.enabled {
        return;
    }
    SESSION.with(|s| {
        *s.borrow_mut() = Some(Session {
            owner,
            core,
            cfg,
            targets,
            window: None,
            pending: None,
            armed_at: None,
        });
    });
}

/// `TrackPopupMenu`から戻った直後に呼ぶ（タイマー停止・窓破棄）。
pub fn end_session() {
    let session = SESSION.with(|s| s.borrow_mut().take());
    if let Some(mut session) = session {
        session.cancel();
        // Dropで窓を破棄する
    }
}

/// `WM_MENUSELECT`を受けたときに呼ぶ。
pub fn on_menu_select(wparam: WPARAM, lparam: LPARAM) {
    let command = (wparam.0 & 0xFFFF) as u32;
    let flags = ((wparam.0 >> 16) & 0xFFFF) as u32;
    SESSION.with(|s| {
        let mut guard = s.borrow_mut();
        let Some(session) = guard.as_mut() else {
            return;
        };
        session.cancel();
        // メニューが閉じた通知（flags=0xFFFF・lparam=0）、サブメニュー行、区切り線は
        // ツールチップの対象外
        if (flags == 0xFFFF && lparam.0 == 0)
            || flags & MF_POPUP.0 != 0
            || flags & MF_SEPARATOR.0 != 0
        {
            return;
        }
        let Some(index) = command.checked_sub(1) else {
            return;
        };
        if session.targets.get(index as usize).is_none() {
            return;
        }
        session.pending = Some((command, HMENU(lparam.0 as *mut c_void)));
        session.armed_at = Some(Instant::now());
        unsafe {
            SetTimer(Some(session.owner), TIMER_ID, session.cfg.delay_ms, None);
        }
    });
}

/// 待ち時間のタイマー満了（`WM_TIMER`, `TIMER_ID`）で呼ぶ。
pub fn on_timer() {
    // 内容の読み込み中もRefCellを借用し続けると、読み込み中に再入したメッセージが
    // あった場合に二重借用になるため、必要な値を取り出して手放す
    let job = SESSION.with(|s| {
        let mut guard = s.borrow_mut();
        let session = guard.as_mut()?;
        let (command, menu) = session.pending?;
        // 古い項目由来の`WM_TIMER`が、新しい項目の待ち時間より早く届いた場合は、
        // 残り時間でタイマーを張り直して待つ
        let delay = Duration::from_millis(u64::from(session.cfg.delay_ms));
        let elapsed = session.armed_at.map_or(delay, |t| t.elapsed());
        if let Some(remaining) = remaining_wait(delay, elapsed) {
            unsafe {
                SetTimer(Some(session.owner), TIMER_ID, remaining.as_millis() as u32, None);
            }
            return None;
        }
        unsafe {
            let _ = KillTimer(Some(session.owner), TIMER_ID);
        }
        session.pending = None;
        session.armed_at = None;
        let target = *session.targets.get(command as usize - 1)?;
        Some((session.owner, session.core.clone(), session.cfg.clone(), target, command, menu))
    });
    let Some((owner, core, cfg, target, command, menu)) = job else {
        return;
    };

    let Some(anchor) = menu_item_bounds(owner, menu, command) else {
        return;
    };
    // サービスのロックはメタデータと resident の `Arc` を写す間だけ持ち、先頭の切り出し・I/O・
    // 復号はロックの外で行う
    let snap = core.read(|service| snapshot_source(service, target)).flatten().map(|s| Snapshot::from_source(s, &cfg));
    let Some(content) = snap.and_then(|snap| build_content(&snap, &cfg)) else {
        return;
    };
    SESSION.with(|s| {
        let mut guard = s.borrow_mut();
        let Some(session) = guard.as_mut() else {
            return;
        };
        // 読み込み中に選択が変わっていたら（pendingが別の項目に更新済み）出さない
        if session.pending.is_some() {
            return;
        }
        if session.window.is_none() {
            session.window = TooltipWindow::create();
        }
        if let Some(window) = &mut session.window {
            window.show(&content, anchor);
        }
    });
}

/// コマンドIDに対応するメニュー項目の画面座標矩形。
fn menu_item_bounds(owner: HWND, menu: HMENU, command: u32) -> Option<Bounds> {
    unsafe {
        let count = GetMenuItemCount(Some(menu));
        for pos in 0..count.max(0) {
            if GetMenuItemID(menu, pos) != command {
                continue;
            }
            let mut rect = RECT::default();
            GetMenuItemRect(Some(owner), menu, pos as u32, &mut rect).ok()?;
            return Some(Bounds {
                left: rect.left,
                top: rect.top,
                right: rect.right,
                bottom: rect.bottom,
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Entry, Format};
    use crate::storage::Storage;

    fn temp_storage() -> (Storage, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("clclr-tooltip-test-{}", Uuid::new_v4()));
        (Storage::open(dir.clone()).unwrap(), dir)
    }

    /// 32bpp BI_RGBボトムアップの単色DIB。
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

    fn dropfiles_wide(paths: &[&str]) -> Vec<u8> {
        let mut v = 20u32.to_le_bytes().to_vec();
        v.extend([0u8; 8]); // pt
        v.extend(0u32.to_le_bytes()); // fNC
        v.extend(1u32.to_le_bytes()); // fWide
        for p in paths {
            v.extend(p.encode_utf16().flat_map(u16::to_le_bytes));
            v.extend([0u8, 0]);
        }
        v.extend([0u8, 0]);
        v
    }

    fn format(name: &str, id: u32, data: Vec<u8>) -> Format {
        Format { format_name: name.to_string(), format_id: id, data }
    }

    /// 形式をStorageへ保存し、そのメタから（residentなしの）スナップショットを作る。
    fn snapshot_of(storage: &Storage, formats: Vec<Format>) -> Snapshot {
        let meta = storage.save_entry(&Entry::new(formats)).unwrap();
        Snapshot {
            meta,
            storage: storage.clone(),
            resident_text: None,
            resident_hdrop: None,
            resident_dib: None,
        }
    }

    #[test]
    fn build_content_of_huge_text_is_truncated_from_the_prefix() {
        let (storage, dir) = temp_storage();
        let text = "あ".repeat(200_000);
        let snap = snapshot_of(&storage, vec![format("CF_UNICODETEXT", 13, crate::data::utf16_bytes(&text))]);
        let content = build_content(&snap, &MenuTooltipConfig::default()).unwrap();
        let shown = content.text.unwrap();
        assert_eq!(shown.chars().count(), 1024 + 1); // 上限1024文字＋省略記号
        assert!(shown.ends_with('…'));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn build_content_lists_files() {
        let (storage, dir) = temp_storage();
        let drop = dropfiles_wide(&[r"C:\a.txt", r"D:\b.png"]);
        let snap = snapshot_of(&storage, vec![format("CF_HDROP", 15, drop)]);
        let content = build_content(&snap, &MenuTooltipConfig::default()).unwrap();
        assert_eq!(content.text.as_deref(), Some("C:\\a.txt\nD:\\b.png"));
        assert!(content.image.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn build_content_falls_back_to_text_when_file_list_is_broken() {
        let (storage, dir) = temp_storage();
        let snap = snapshot_of(
            &storage,
            vec![
                format("CF_HDROP", 15, vec![1, 2, 3]),
                format("CF_UNICODETEXT", 13, crate::data::utf16_bytes("本文")),
            ],
        );
        let content = build_content(&snap, &MenuTooltipConfig::default()).unwrap();
        assert_eq!(content.text.as_deref(), Some("本文"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn build_content_is_none_for_blank_text_only() {
        let (storage, dir) = temp_storage();
        let snap = snapshot_of(&storage, vec![format("CF_UNICODETEXT", 13, crate::data::utf16_bytes(" \n "))]);
        assert!(build_content(&snap, &MenuTooltipConfig::default()).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn small_image_without_thumbnail_is_decoded_as_is() {
        let (storage, dir) = temp_storage();
        // 128px以下の画像はサムネイルが作られない（thumb=None）
        let snap = snapshot_of(&storage, vec![format("CF_DIB", 8, dib_32bpp(64, 48))]);
        assert!(snap.meta.formats[0].thumb.is_none());
        let img = build_content(&snap, &MenuTooltipConfig::default()).unwrap().image.unwrap();
        assert_eq!((img.width, img.height), (64, 48));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn image_without_thumbnail_is_scaled_down_to_thumbnail_edge() {
        let (storage, dir) = temp_storage();
        // 300x300（約360KB、上限以下）。サムネイルを持たない履歴を想定して外す
        let mut snap = snapshot_of(&storage, vec![format("CF_DIB", 8, dib_32bpp(300, 300))]);
        snap.meta.formats[0].thumb = None;
        let img = build_content(&snap, &MenuTooltipConfig::default()).unwrap().image.unwrap();
        assert!(img.width.max(img.height) <= crate::dib::THUMB_LONG_EDGE);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn image_without_thumbnail_over_size_limit_is_skipped() {
        let (storage, dir) = temp_storage();
        // 600x600（約1.4MB）は原寸展開の上限超過。サムネイルも無ければ画像は出さない
        let mut snap = snapshot_of(&storage, vec![format("CF_DIB", 8, dib_32bpp(600, 600))]);
        snap.meta.formats[0].thumb = None;
        assert!(snap.meta.formats[0].size > MAX_FALLBACK_DIB_BYTES as u64);
        assert!(build_content(&snap, &MenuTooltipConfig::default()).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn large_image_uses_thumbnail_regardless_of_size_limit() {
        let (storage, dir) = temp_storage();
        let snap = snapshot_of(&storage, vec![format("CF_DIB", 8, dib_32bpp(600, 600))]);
        assert!(snap.meta.formats[0].thumb.is_some());
        let img = build_content(&snap, &MenuTooltipConfig::default()).unwrap().image.unwrap();
        assert!(img.width.max(img.height) <= crate::dib::THUMB_LONG_EDGE);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resident_data_takes_precedence_over_persisted_blobs() {
        let (storage, dir) = temp_storage();
        // save=false相当（resident）のみのエントリ: metaにはformatsが無い
        let mut snap = snapshot_of(&storage, vec![]);
        snap.resident_text = Some(crate::data::utf16_bytes("resident"));
        let content = build_content(&snap, &MenuTooltipConfig::default()).unwrap();
        assert_eq!(content.text.as_deref(), Some("resident"));

        snap.resident_text = None;
        snap.resident_dib = Some(dib_32bpp(32, 32));
        let img = build_content(&snap, &MenuTooltipConfig::default()).unwrap().image.unwrap();
        assert_eq!((img.width, img.height), (32, 32));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn remaining_wait_ignores_timer_within_slack_and_after_delay() {
        let delay = Duration::from_millis(500);
        // 待ち時間を過ぎている・分解能ぶん以内の早着は満了扱い
        assert_eq!(remaining_wait(delay, Duration::from_millis(500)), None);
        assert_eq!(remaining_wait(delay, Duration::from_millis(600)), None);
        assert_eq!(remaining_wait(delay, Duration::from_millis(485)), None);
    }

    #[test]
    fn remaining_wait_returns_remainder_for_stale_timer() {
        // 項目を切り替えた直後（50ms経過）に届いた古いタイマーは、残り450msで張り直す
        let delay = Duration::from_millis(500);
        assert_eq!(
            remaining_wait(delay, Duration::from_millis(50)),
            Some(Duration::from_millis(450))
        );
    }

    /// 広すぎる単語だけに文字単位の改行が入り、通常の単語・空白はそのまま残ること。
    /// 改行を除けば元の文字列に戻り、絵文字（サロゲートペア）が途中で割れないこと。
    #[test]
    fn break_long_words_breaks_only_overlong_words_and_keeps_pairs_intact() {
        let _guard = crate::tray::lock_gui_resource_tests();
        let mut window = TooltipWindow::create().expect("ツールチップ窓を作れる");
        let hwnd = window.hwnd;
        let font = window.state().unwrap().font;
        let long_ascii = "A".repeat(200);
        let emoji = "😀".repeat(100);
        let original = format!("short words {long_ascii} tail {emoji}");
        let units: Vec<u16> = original.encode_utf16().collect();
        let out = unsafe {
            let hdc = GetDC(Some(hwnd));
            let old = SelectObject(hdc, HGDIOBJ(font.0));
            let out = break_long_words(hdc, &units, 100);
            SelectObject(hdc, old);
            ReleaseDC(Some(hwnd), hdc);
            out
        };
        // 孤立サロゲートがあればここでErrになる
        let broken = String::from_utf16(&out).expect("サロゲートペアが途中で割れていない");
        assert!(broken.starts_with("short words "), "通常の単語は折らない: {broken:?}");
        assert!(broken.contains('\n'));
        assert_eq!(broken.replace('\n', ""), original);
    }

    #[test]
    fn break_long_words_leaves_text_that_fits_untouched() {
        let _guard = crate::tray::lock_gui_resource_tests();
        let mut window = TooltipWindow::create().expect("ツールチップ窓を作れる");
        let hwnd = window.hwnd;
        let font = window.state().unwrap().font;
        let units: Vec<u16> = "a b\tc\nd  e\r\nf".encode_utf16().collect();
        let out = unsafe {
            let hdc = GetDC(Some(hwnd));
            let old = SelectObject(hdc, HGDIOBJ(font.0));
            let out = break_long_words(hdc, &units, 480);
            SelectObject(hdc, old);
            ReleaseDC(Some(hwnd), hdc);
            out
        };
        assert_eq!(out, units);
    }

    /// 空白のない長文（URL・ハッシュ等）でも、計測幅が折り返し幅を超えないこと。
    /// 実際の窓とDCで`DT_CALCRECT`を測る（`DT_WORDBREAK`は最長単語が矩形より長いと
    /// 幅を広げるという指摘の実機確認）。
    #[test]
    fn unbreakable_long_text_stays_within_max_width() {
        let _guard = crate::tray::lock_gui_resource_tests();
        let mut window = TooltipWindow::create().expect("ツールチップ窓を作れる");
        let hwnd = window.hwnd;
        let st = window.state().unwrap();
        st.text = "A".repeat(1024).encode_utf16().collect();
        let (w, h) = TooltipWindow::measure_text(hwnd, st);
        assert!(w > 0 && h > 0);
        assert!(w <= MAX_TEXT_WIDTH, "計測幅{w}が折り返し幅{MAX_TEXT_WIDTH}を超えた");
    }

    #[test]
    fn scale_for_dpi_rounds_to_nearest_pixel() {
        assert_eq!(scale_for_dpi(PAD, 96), 6);
        assert_eq!(scale_for_dpi(PAD, 120), 8); // 125%: 7.5 → 8
        assert_eq!(scale_for_dpi(PAD, 144), 9); // 150%
        assert_eq!(scale_for_dpi(MAX_TEXT_WIDTH, 192), 960); // 200%
        assert_eq!(scale_for_dpi(ANCHOR_GAP, 168), 7); // 175%
    }

    #[test]
    fn truncate_keeps_short_text_as_is() {
        assert_eq!(truncate_tooltip_text("abc\ndef", 100, 10), "abc\ndef");
    }

    #[test]
    fn truncate_by_lines_appends_ellipsis() {
        assert_eq!(truncate_tooltip_text("a\nb\nc\nd", 100, 2), "a\nb…");
    }

    #[test]
    fn truncate_does_not_mark_exact_line_limit() {
        assert_eq!(truncate_tooltip_text("a\nb", 100, 2), "a\nb");
    }

    #[test]
    fn truncate_by_chars_appends_ellipsis() {
        assert_eq!(truncate_tooltip_text("abcdef", 3, 10), "abc…");
    }

    #[test]
    fn truncate_does_not_mark_exact_char_limit() {
        assert_eq!(truncate_tooltip_text("abc", 3, 10), "abc");
    }

    #[test]
    fn truncate_counts_chars_not_bytes() {
        assert_eq!(truncate_tooltip_text("あいうえお", 3, 10), "あいう…");
    }

    #[test]
    fn truncate_counts_newline_as_a_char() {
        // "ab" + 改行 = 3文字で上限に達し、2行目は空のまま省略記号だけが付く
        assert_eq!(truncate_tooltip_text("ab\ncd", 3, 10), "ab\n…");
    }

    #[test]
    fn truncate_handles_crlf() {
        assert_eq!(truncate_tooltip_text("a\r\nb", 100, 10), "a\nb");
    }

    #[test]
    fn truncate_with_zero_lines_yields_only_ellipsis() {
        assert_eq!(truncate_tooltip_text("abc", 100, 0), "…");
    }

    #[test]
    fn file_list_caps_at_max_files_with_remainder_count() {
        let paths: Vec<String> = (0..MAX_FILES + 3).map(|i| format!("f{i}")).collect();
        let text = file_list_text(&paths, usize::MAX, usize::MAX);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), MAX_FILES + 1);
        assert_eq!(lines[MAX_FILES - 1], format!("f{}", MAX_FILES - 1));
        assert_eq!(lines[MAX_FILES], "…ほか3件");
    }

    #[test]
    fn flatten_blends_transparent_pixel_into_background() {
        // 完全透過 → 背景色そのもの、不透明 → 元の色
        let rgba = [10, 20, 30, 0, 200, 100, 50, 255];
        let out = flatten_on_background(&rgba, (255, 255, 225));
        assert_eq!(out, vec![255, 255, 225, 255, 200, 100, 50, 255]);
    }

    #[test]
    fn flatten_blends_half_alpha_halfway() {
        let out = flatten_on_background(&[0, 0, 0, 128], (255, 255, 255));
        // 128/255 ≒ 0.502 の黒 → 255*(127/255) ≒ 127
        assert_eq!(out[0], 127);
        assert_eq!(out[3], 255);
    }

    fn b(left: i32, top: i32, right: i32, bottom: i32) -> Bounds {
        Bounds { left, top, right, bottom }
    }

    #[test]
    fn place_puts_tooltip_right_of_item_aligned_to_top() {
        let work = b(0, 0, 1920, 1080);
        let (x, y) = place_tooltip(b(100, 200, 300, 220), (200, 100), work, ANCHOR_GAP);
        assert_eq!((x, y), (300 + ANCHOR_GAP, 200));
    }

    #[test]
    fn place_flips_to_left_when_right_edge_overflows() {
        let work = b(0, 0, 1000, 1000);
        let (x, _) = place_tooltip(b(700, 100, 900, 120), (200, 50), work, ANCHOR_GAP);
        assert_eq!(x, 700 - 200 - ANCHOR_GAP);
    }

    #[test]
    fn place_clamps_into_work_area_when_neither_side_fits() {
        let work = b(0, 0, 500, 500);
        let (x, _) = place_tooltip(b(50, 100, 450, 120), (400, 50), work, ANCHOR_GAP);
        assert!(x >= work.left && x + 400 <= work.right);
    }

    #[test]
    fn place_pushes_up_when_bottom_edge_overflows() {
        let work = b(0, 0, 1920, 1080);
        let (_, y) = place_tooltip(b(100, 1000, 300, 1020), (200, 300), work, ANCHOR_GAP);
        assert_eq!(y, 1080 - 300);
    }

    #[test]
    fn place_respects_work_area_origin_offset() {
        // 2枚目のモニタ（左上が負座標）でも作業領域内に収まる
        let work = b(-1920, 0, 0, 1080);
        let (x, y) = place_tooltip(b(-300, 10, -100, 30), (400, 50), work, ANCHOR_GAP);
        assert!(x >= work.left && x + 400 <= work.right);
        assert_eq!(y, 10);
    }
}
