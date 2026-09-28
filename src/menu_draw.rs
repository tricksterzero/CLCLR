//! ポップアップメニューを自前で描く（オーナードロー）。ホットキー・トレイのメニュー、ビューアの
//! メニュー（`native/`）が共用する（Windows のダークモードの下で、標準の
//! メニューの選択行が白地に薄い灰色で分かりにくいため、C 版 `Menu.c` と同じくシステム色で描く）。
//!
//! - 項目は `MF_OWNERDRAW`。項目のデータ（`ItemData`）は `Box` に入れて `PopupMenu` が持ち、その
//!   アドレスを `lpNewItem`（`itemData`）として渡す。描画に使う変わらないもの（フォント・寸法）は
//!   `Rc<MenuResources>` を項目どうしで共有する（`PopupMenu` が別の場所へ移っても参照先は動かない）
//! - `PopupMenu` の `Drop` は、先にメニュー（子メニューごと）を破棄してから項目のデータを解放する
//! - 持ち主の窓は `WM_MEASUREITEM`・`WM_DRAWITEM`（`CtlType == ODT_MENU`）で `on_measure_item`・
//!   `on_draw_item` を、`WM_MENUCHAR` で `on_menu_char` を呼ぶ。どれも `itemData` を共有参照で読むだけで、
//!   `RefCell`・サービスのロックを使わない（メニューの表示中は窓のプロシージャが再入するため）
//! - 実機で確かめたこと: 子メニューの矢印はシステムが
//!   描く（選択行の上でも読める）。チェック印はシステムが描かないので自分で描く。システムは測った幅に
//!   固定の幅を足して描かせる（`MNS_NOCHECK` でも変わらない）。区切り線にも描画の呼び出しが来る。
//!   自前描画の項目の `&` をシステムは扱わないので、アクセスキーは `WM_MENUCHAR` で解決する

use std::rc::Rc;

use windows::core::{PCWSTR, Result as WinResult};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AlphaBlend, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontIndirectW,
    CreateSolidBrush, DeleteDC, DeleteObject, DrawFrameControl, DrawTextW, FillRect, GetDC, GetStockObject,
    GetSysColor, GetSysColorBrush, PatBlt, ReleaseDC, SelectObject, SetBkMode, SetTextColor, AC_SRC_ALPHA,
    AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, COLOR_3DHIGHLIGHT, COLOR_3DSHADOW,
    COLOR_GRAYTEXT, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, COLOR_MENU, COLOR_MENUTEXT, DEFAULT_GUI_FONT,
    DFCS_MENUCHECK, DFC_MENU, DIB_RGB_COLORS, DSTINVERT, DT_CALCRECT, DT_HIDEPREFIX, DT_SINGLELINE,
    DT_VCENTER, HBITMAP, HDC, HFONT, HGDIOBJ, ROP_CODE, SRCCOPY, SYS_COLOR_INDEX, TRANSPARENT, RestoreDC, SaveDC,
};
use windows::Win32::UI::Controls::{
    DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODS_CHECKED, ODS_DISABLED, ODS_GRAYED, ODS_NOACCEL, ODS_SELECTED, ODT_MENU,
};
use windows::Win32::UI::HiDpi::SystemParametersInfoForDpi;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateMenu, CreatePopupMenu, DestroyMenu, GetMenuItemCount, GetMenuItemInfoW, HMENU, MENUITEMINFOW,
    MENU_ITEM_FLAGS, MFS_DISABLED, MFS_HILITE, MFT_OWNERDRAW, MFT_SEPARATOR, MF_OWNERDRAW, MF_POPUP, MF_SEPARATOR,
    MIIM_DATA, MIIM_FTYPE, MIIM_STATE, NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS,
};

/// 寸法（96 DPI 基準。メニューを出すモニターの DPI で換算する）。
const ITEM_PAD_X: i32 = 4;
const TEXT_PAD_Y: i32 = 4;
/// 左の欄（チェック印・サムネイル）の最小の幅
const CHECK_COLUMN: i32 = 16;
/// 左の欄と文字の間、文字の右の余白
const COLUMN_GAP: i32 = 6;
const RIGHT_PAD: i32 = 8;
/// サムネイルの上下の余白
const THUMB_PAD: i32 = 2;
const SEPARATOR_HEIGHT: i32 = 9;

/// `WM_MENUCHAR` の戻り値の上位ワード（Microsoft Learn の WM_MENUCHAR の説明）。
const MNC_EXECUTE: u32 = 2;
const MNC_SELECT: u32 = 3;
/// DSPDxax（元が白の所は塗りつぶしの色、黒の所は描き先のまま）
const ROP_DSPDXAX: ROP_CODE = ROP_CODE(0x00E2_0746);

fn scale(v: i32, dpi: u32) -> i32 {
    crate::menu_tooltip::scale_for_dpi(v, dpi)
}

/// 1つのメニュー（子メニューを含む）で共有する、変わらない描画の材料。
pub(crate) struct MenuResources {
    font: HFONT,
    /// 既定の GUI フォントを借りている（解放しない）
    stock_font: bool,
    dpi: u32,
    /// 左の欄の幅（px）。サムネイルを出すメニューはサムネイルの大きさ
    left_column: i32,
}

impl MenuResources {
    /// `dpi` のメニューのフォント（`NONCLIENTMETRICS::lfMenuFont`。その DPI 用の値をそのまま使う）。
    /// `thumb_size` はサムネイルを出すメニューのときの、サムネイルの長辺（px）。
    fn new(dpi: u32, thumb_size: Option<i32>) -> Self {
        let (font, stock_font) = menu_font(dpi);
        let left_column = scale(CHECK_COLUMN, dpi).max(thumb_size.unwrap_or(0));
        Self { font, stock_font, dpi, left_column }
    }
}

impl Drop for MenuResources {
    fn drop(&mut self) {
        if !self.stock_font {
            unsafe {
                let _ = DeleteObject(self.font.into());
            }
        }
    }
}

fn menu_font(dpi: u32) -> (HFONT, bool) {
    unsafe {
        let mut ncm = NONCLIENTMETRICSW { cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
        let ok = SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            ncm.cbSize,
            Some((&mut ncm as *mut NONCLIENTMETRICSW).cast()),
            0,
            dpi,
        )
        .is_ok();
        if ok {
            let font = CreateFontIndirectW(&ncm.lfMenuFont);
            if !font.is_invalid() {
                return (font, false);
            }
        }
        (HFONT(GetStockObject(DEFAULT_GUI_FONT).0), true)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemKind {
    Command,
    Submenu,
    Separator,
}

/// アルファを掛けた 32bpp の DIB セクション（トップダウン、BGRA）。
struct Thumbnail {
    hbmp: HBITMAP,
    width: i32,
    height: i32,
}

impl Thumbnail {
    /// RGBA（アルファを掛けていない）から作る。
    fn from_rgba(width: u32, height: u32, rgba: &[u8]) -> Option<Self> {
        if width == 0 || height == 0 || rgba.len() != (width * height * 4) as usize {
            return None;
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let hbmp = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) }.ok()?;
        if bits.is_null() {
            unsafe {
                let _ = DeleteObject(hbmp.into());
            }
            return None;
        }
        let bgra = premultiplied_bgra(rgba);
        unsafe {
            std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits.cast::<u8>(), bgra.len());
        }
        Some(Self { hbmp, width: width as i32, height: height as i32 })
    }
}

impl Drop for Thumbnail {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.hbmp.into());
        }
    }
}

/// RGBA（アルファを掛けていない）を、`AlphaBlend`（`AC_SRC_ALPHA`）が求めるアルファを掛けた BGRA にする。
fn premultiplied_bgra(rgba: &[u8]) -> Vec<u8> {
    let mul = |c: u8, a: u8| ((u16::from(c) * u16::from(a) + 127) / 255) as u8;
    rgba.chunks_exact(4).flat_map(|p| [mul(p[2], p[3]), mul(p[1], p[3]), mul(p[0], p[3]), p[3]]).collect()
}

/// 項目の左の欄に出す画像。
pub(crate) enum MenuImage<'a> {
    /// 画像のサムネイル（RGBA、幅・高さ）。その行だけ高くなる
    Thumbnail { width: u32, height: u32, rgba: &'a [u8] },
    /// アイコン（`icons::SOURCE_SIZE` 角の RGBA）。`ICON_SIZE`（DPI で換算）で描き、行の高さは変えない
    Icon(&'static [u8]),
}

/// アイコンの大きさ（96 DPI 基準。C 版の既定 `menu_icon_size` と同じ）。
const ICON_SIZE: i32 = 16;

/// ツリーの線の1段の幅（96 DPI 基準）。
const GUIDE_STEP: i32 = 16;

/// 文字の手前に描くツリーの線の1段（罫線の文字では行の上下の余白で
/// 線が途切れたため、行の高さいっぱいに描く）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TreeGuide {
    /// 上の段から下の段へ続く縦線（│）
    Pipe,
    /// 何も描かない
    Blank,
    /// 下にも兄弟が続く枝（├）
    Branch,
    /// 最後の兄弟の枝（└）
    Last,
}

/// 項目1つ分の描画の材料（`itemData` が指す）。
struct ItemData {
    kind: ItemKind,
    /// 表示の文字列（NUL なし。`&` はアクセスキーの印、`&&` は文字の `&`）
    text: Vec<u16>,
    thumbnail: Option<Thumbnail>,
    /// アイコン（`ICON_SIZE` の大きさで描く。素材を縮めて持つ。同じ素材の行どうしで共有する）
    icon: Option<Rc<Thumbnail>>,
    /// 文字の手前に描くツリーの線（左の段から）
    guide: Vec<TreeGuide>,
    resources: Rc<MenuResources>,
}

/// アイコンの素材（`icons::SOURCE_SIZE` 角の RGBA）を、`size` px 以下へ縮めて DIB にする（大きくはしない。
/// 描くときに `size` へ引き伸ばす）。
fn icon_bitmap(rgba: &[u8], size: i32) -> Option<Thumbnail> {
    let source = crate::icons::SOURCE_SIZE;
    let (w, h, pixels) = crate::dib::scale_rgba(source, source, rgba.to_vec(), size.max(1) as u32);
    Thumbnail::from_rgba(w, h, &pixels)
}

/// 項目のデータと描画の材料（`PopupMenu`・`MenuBar` が持つ）。
struct ItemStore {
    items: Vec<Box<ItemData>>,
    resources: Rc<MenuResources>,
    /// 作ったアイコン（素材の先頭のアドレス → ビットマップ）。行ごとに作らず共有する（履歴の階層表示では
    /// 1つのメニューが千行近くになり、行ごとに作ると GDI のオブジェクトがその数だけ増えるため）
    icons: Vec<(usize, Rc<Thumbnail>)>,
}

impl ItemStore {
    fn new(dpi: u32, thumb_size: Option<i32>) -> Self {
        Self { items: Vec::new(), resources: Rc::new(MenuResources::new(dpi, thumb_size)), icons: Vec::new() }
    }

    /// 素材 `rgba` のアイコンのビットマップ（初めてのときだけ作る）。
    fn icon(&mut self, rgba: &[u8]) -> Option<Rc<Thumbnail>> {
        let key = rgba.as_ptr() as usize;
        if let Some((_, icon)) = self.icons.iter().find(|(k, _)| *k == key) {
            return Some(Rc::clone(icon));
        }
        let icon = Rc::new(icon_bitmap(rgba, scale(ICON_SIZE, self.resources.dpi))?);
        self.icons.push((key, Rc::clone(&icon)));
        Some(icon)
    }

    /// 項目のデータを作り、そのアドレスを返す（`Box` の中身は動かない）。
    fn push(&mut self, kind: ItemKind, text: &str, image: Option<MenuImage<'_>>) -> PCWSTR {
        let (thumbnail, icon) = match image {
            Some(MenuImage::Thumbnail { width, height, rgba }) => (Thumbnail::from_rgba(width, height, rgba), None),
            Some(MenuImage::Icon(rgba)) => (None, self.icon(rgba)),
            None => (None, None),
        };
        let data = Box::new(ItemData {
            kind,
            text: text.encode_utf16().collect(),
            thumbnail,
            icon,
            guide: Vec::new(),
            resources: Rc::clone(&self.resources),
        });
        let ptr = PCWSTR(&*data as *const ItemData as *const u16);
        self.items.push(data);
        ptr
    }

    fn append_command(
        &mut self,
        target: HMENU,
        id: usize,
        text: &str,
        flags: MENU_ITEM_FLAGS,
        image: Option<MenuImage<'_>>,
    ) -> WinResult<()> {
        let data = self.push(ItemKind::Command, text, image);
        unsafe { AppendMenuW(target, flags | MF_OWNERDRAW, id, data) }
    }

    /// 文字の手前にツリーの線（`guide`）を描くコマンドを足す。
    fn append_guided_command(
        &mut self,
        target: HMENU,
        id: usize,
        text: &str,
        flags: MENU_ITEM_FLAGS,
        guide: &[TreeGuide],
    ) -> WinResult<()> {
        let data = self.push(ItemKind::Command, text, None);
        if let Some(item) = self.items.last_mut() {
            item.guide = guide.to_vec();
        }
        unsafe { AppendMenuW(target, flags | MF_OWNERDRAW, id, data) }
    }

    fn append_separator(&mut self, target: HMENU) -> WinResult<()> {
        let data = self.push(ItemKind::Separator, "", None);
        unsafe { AppendMenuW(target, MF_OWNERDRAW | MF_SEPARATOR, 0, data) }
    }
}

/// Win32メニューテキストでは`&`が次の文字のニーモニック指定として解釈されるため、
/// リテラル表示するには`&&`にエスケープする必要がある。エントリのタイトル・
/// プレビューやフォルダ名等、ユーザー由来の任意文字列に`&`が含まれると
/// 表示が欠けたり意図しないキーにニーモニックが割り当たったりする
/// （実機確認: "Q&Amp;A"が"QAmp;A"になり"A"に下線が付く）。
/// ホットキーのメニューとビューアの行のメニュー（ピン留めのフォルダ名）が共用する。
pub(crate) fn escape_menu_text(text: &str) -> String {
    text.replace('&', "&&")
}

/// 自前で描くポップアップメニュー。`Drop` で先にメニュー（子メニューごと）を破棄し、その後で
/// 項目のデータを解放する。メニューを表示している間（`TrackPopupMenu` から戻るまで）は、項目を
/// 足したり、この値を破棄したりしない。
pub(crate) struct PopupMenu {
    menu: HMENU,
    store: ItemStore,
}

impl PopupMenu {
    /// `dpi` はメニューを出すモニターの DPI（子メニューも同じ DPI で描く）。`thumb_size` はサムネイルを
    /// 出すメニューのとき、サムネイルの長辺（px）。
    pub(crate) fn new(dpi: u32, thumb_size: Option<i32>) -> WinResult<Self> {
        let menu = unsafe { CreatePopupMenu()? };
        Ok(Self { menu, store: ItemStore::new(dpi, thumb_size) })
    }

    pub(crate) fn handle(&self) -> HMENU {
        self.menu
    }

    /// `target`（このメニューか、`add_submenu` で作った子メニュー）にコマンドを足す。`flags` は
    /// `MF_CHECKED`・`MF_GRAYED` など（`MF_OWNERDRAW` は付ける）。`image` は左の欄に出す画像。
    pub(crate) fn append_command(
        &mut self,
        target: HMENU,
        id: usize,
        text: &str,
        flags: MENU_ITEM_FLAGS,
        image: Option<MenuImage<'_>>,
    ) {
        // 足せなかった行はメニューに出ないだけ（ポップアップは出すたびに作り直す）
        let _ = self.store.append_command(target, id, text, flags, image);
    }

    /// `append_command` と同じく、文字の手前にツリーの線（`guide`、左の段から）を描くコマンドを足す。
    pub(crate) fn append_guided_command(
        &mut self,
        target: HMENU,
        id: usize,
        text: &str,
        flags: MENU_ITEM_FLAGS,
        guide: &[TreeGuide],
    ) {
        let _ = self.store.append_guided_command(target, id, text, flags, guide);
    }

    pub(crate) fn append_separator(&mut self, target: HMENU) {
        let _ = self.store.append_separator(target);
    }

    /// このメニューに子メニューを足し、そのハンドルを返す（項目は `append_command` で足す）。付けられた
    /// 子メニューは親と一緒に破棄される。付けられなければ破棄して None。`icon` は入口の行のアイコン。
    pub(crate) fn add_submenu(&mut self, text: &str, icon: Option<&'static [u8]>) -> Option<HMENU> {
        self.add_submenu_to(self.menu, text, icon)
    }

    /// `parent`（このメニューか、ここで作った子メニュー）に子メニューを足す（入れ子にする）。ほかは
    /// `add_submenu` と同じ。
    pub(crate) fn add_submenu_to(&mut self, parent: HMENU, text: &str, icon: Option<&'static [u8]>) -> Option<HMENU> {
        let sub = unsafe { CreatePopupMenu() }.ok()?;
        let data = self.store.push(ItemKind::Submenu, text, icon.map(MenuImage::Icon));
        unsafe {
            if AppendMenuW(parent, MF_OWNERDRAW | MF_POPUP, sub.0 as usize, data).is_err() {
                let _ = DestroyMenu(sub);
                return None;
            }
        }
        Some(sub)
    }
}

impl Drop for PopupMenu {
    fn drop(&mut self) {
        // メニュー（と付けた子メニュー）を先に破棄してから、項目のデータ（サムネイルのビットマップ）と
        // 共有の材料（フォント）を解放する（フィールドはこの後に破棄される）
        unsafe {
            let _ = DestroyMenu(self.menu);
        }
    }
}

/// 窓のメニューバー。バーの並び（「ツール」など）は標準の描画のまま、ドロップダウンの項目を自前で描く。
///
/// メニューを破棄する責任は、窓に付けているかで変わる（Microsoft Learn: `DestroyWindow` は窓のメニューを
/// 破棄し、`SetMenu` は外したメニューを破棄しない）。
/// - 窓に付ける前と、`SetMenu` で外した後は、この値の `Drop` がメニューを破棄してから項目のデータを解放する
/// - 窓に付けている間（`mark_attached` の後）は、窓の破棄より前に `SetMenu(None)` で外してから
///   `detached` を呼ぶ。付けたまま破棄された場合は、窓がまだメニューを使うかもしれないので、項目のデータを
///   解放せずに残す（描画の中で解放済みのデータを読まないため。デバッグ時は `debug_assert` で知らせる）
pub(crate) struct MenuBar {
    bar: HMENU,
    store: ItemStore,
    attached: bool,
}

impl MenuBar {
    /// `dpi` は窓の DPI（ドロップダウンの項目をこの DPI で描く）。
    pub(crate) fn new(dpi: u32) -> WinResult<Self> {
        let bar = unsafe { CreateMenu()? };
        Ok(Self { bar, store: ItemStore::new(dpi, None), attached: false })
    }

    pub(crate) fn handle(&self) -> HMENU {
        self.bar
    }

    /// 項目を描く DPI（作ったときの窓の DPI）。
    pub(crate) fn dpi(&self) -> u32 {
        self.store.resources.dpi
    }

    /// バーにドロップダウンを足し、そのハンドルを返す（バーの項目は標準の描画。付けられなければ破棄して Err）。
    /// メニューバーは、すべての項目を足せたときだけ窓に付ける（作り直しで、欠けたバーに差し替えない）ので、
    /// 足す操作はどれも失敗を返す。
    pub(crate) fn add_dropdown(&mut self, text: PCWSTR) -> WinResult<HMENU> {
        let dropdown = unsafe { CreatePopupMenu()? };
        unsafe {
            if let Err(e) = AppendMenuW(self.bar, MF_POPUP, dropdown.0 as usize, text) {
                let _ = DestroyMenu(dropdown);
                return Err(e);
            }
        }
        Ok(dropdown)
    }

    pub(crate) fn append_command(&mut self, target: HMENU, id: usize, text: &str, flags: MENU_ITEM_FLAGS) -> WinResult<()> {
        self.store.append_command(target, id, text, flags, None)
    }

    pub(crate) fn append_separator(&mut self, target: HMENU) -> WinResult<()> {
        self.store.append_separator(target)
    }

    /// 窓に付けた（`CreateWindowExW` の hMenu・`SetMenu` が成功した）。以後、メニューは窓が破棄する。
    pub(crate) fn mark_attached(&mut self) {
        self.attached = true;
    }

    /// 窓から外した（`SetMenu` で別のメニューへ替えた・None にした）。この値の破棄でメニューを破棄する。
    pub(crate) fn detached(mut self) -> Self {
        self.attached = false;
        self
    }
}

impl Drop for MenuBar {
    fn drop(&mut self) {
        if self.attached {
            debug_assert!(false, "窓に付けたままのメニューバーを破棄した（先に SetMenu で外す）");
            // 窓がまだこのメニューを使うかもしれないので、項目のデータを解放しない
            std::mem::forget(std::mem::take(&mut self.store.items));
            return;
        }
        unsafe {
            let _ = DestroyMenu(self.bar);
        }
    }
}

/// 選んだ GDI オブジェクトを、スコープを抜けるときに戻す。
struct Selected {
    hdc: HDC,
    old: HGDIOBJ,
}

impl Selected {
    unsafe fn new(hdc: HDC, obj: HGDIOBJ) -> Self {
        Self { hdc, old: unsafe { SelectObject(hdc, obj) } }
    }
}

impl Drop for Selected {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.hdc, self.old);
        }
    }
}

/// 作業用のメモリ DC とビットマップ（`Drop` でビットマップを外して両方を解放する）。
struct MemoryCanvas {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
}

impl MemoryCanvas {
    fn new(reference: HDC, width: i32, height: i32) -> Option<Self> {
        unsafe {
            let dc = CreateCompatibleDC(Some(reference));
            if dc.is_invalid() {
                return None;
            }
            let bitmap = CreateCompatibleBitmap(reference, width.max(1), height.max(1));
            if bitmap.is_invalid() {
                let _ = DeleteDC(dc);
                return None;
            }
            let old = SelectObject(dc, bitmap.into());
            Some(Self { dc, bitmap, old })
        }
    }
}

impl Drop for MemoryCanvas {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

/// 項目の大きさ（px）。`text_size` は文字の幅・高さ（`DrawTextW` の `DT_CALCRECT`）。システムは
/// この幅に固定の幅を足して描かせる（実機で確認）。
fn item_size(kind: ItemKind, text_size: (i32, i32), has_thumbnail: bool, resources: &MenuResources) -> (u32, u32) {
    let dpi = resources.dpi;
    if kind == ItemKind::Separator {
        return (0, scale(SEPARATOR_HEIGHT, dpi) as u32);
    }
    let width = scale(ITEM_PAD_X, dpi) + resources.left_column + scale(COLUMN_GAP, dpi) + text_size.0 + scale(RIGHT_PAD, dpi);
    let text_height = text_size.1 + 2 * scale(TEXT_PAD_Y, dpi);
    // サムネイルのある行だけを高くする（欄の幅は全部の行でそろえ、文字の始まりをそろえる）。
    // アイコンの行は高さを変えない（アイコンは行に収まる大きさで描く。`paint`）
    let height = if has_thumbnail {
        text_height.max(resources.left_column + 2 * scale(THUMB_PAD, dpi))
    } else {
        text_height
    };
    (width.max(0) as u32, height.max(0) as u32)
}

/// 描画と同じフォント・同じ `&` の扱いで文字の大きさを測る。
unsafe fn measure_text(hdc: HDC, data: &ItemData) -> (i32, i32) {
    unsafe {
        let _font = Selected::new(hdc, data.resources.font.into());
        let mut text = data.text.clone();
        if text.is_empty() {
            text.push(u16::from(b' '));
        }
        let mut rc = RECT::default();
        DrawTextW(hdc, &mut text, &mut rc, DT_CALCRECT | DT_SINGLELINE);
        (rc.right - rc.left, rc.bottom - rc.top)
    }
}

/// `WM_MEASUREITEM`。自前描画のメニューの項目なら大きさを埋めて true。
///
/// # Safety
/// `lparam` は `WM_MEASUREITEM` の `MEASUREITEMSTRUCT` へのポインタで、`ODT_MENU` の項目の
/// `itemData` はこのモジュールの `PopupMenu` が作った `ItemData` であること（自前描画のメニューは
/// このモジュールでしか作らない）。
pub(crate) unsafe fn on_measure_item(hwnd: HWND, lparam: LPARAM) -> bool {
    let mis = unsafe { &mut *(lparam.0 as *mut MEASUREITEMSTRUCT) };
    if mis.CtlType != ODT_MENU || mis.itemData == 0 {
        return false;
    }
    let data = unsafe { &*(mis.itemData as *const ItemData) };
    let text_size = if data.kind == ItemKind::Separator {
        (0, 0)
    } else {
        unsafe {
            let hdc = GetDC(Some(hwnd));
            let (w, h) = measure_text(hdc, data);
            ReleaseDC(Some(hwnd), hdc);
            // ツリーの線は文字の手前に並べるので、文字の幅に足す
            (w + guide_width(data), h)
        }
    };
    let (w, h) = item_size(data.kind, text_size, data.thumbnail.is_some(), &data.resources);
    mis.itemWidth = w;
    mis.itemHeight = h;
    #[cfg(test)]
    test_support::count_measure();
    true
}

/// 項目の状態から決める色。
#[derive(Debug, PartialEq, Eq)]
struct ItemColors {
    background: SYS_COLOR_INDEX,
    text: SYS_COLOR_INDEX,
}

fn item_colors(selected: bool, grayed: bool) -> ItemColors {
    let background = if selected { COLOR_HIGHLIGHT } else { COLOR_MENU };
    let text = if grayed {
        COLOR_GRAYTEXT
    } else if selected {
        COLOR_HIGHLIGHTTEXT
    } else {
        COLOR_MENUTEXT
    };
    ItemColors { background, text }
}

/// `WM_DRAWITEM`。自前描画のメニューの項目なら描いて true。
///
/// # Safety
/// `lparam` は `WM_DRAWITEM` の `DRAWITEMSTRUCT` へのポインタで、`ODT_MENU` の項目の `itemData` は
/// このモジュールの `PopupMenu` が作った `ItemData` であること。
pub(crate) unsafe fn on_draw_item(lparam: LPARAM) -> bool {
    let dis = unsafe { &*(lparam.0 as *const DRAWITEMSTRUCT) };
    if dis.CtlType != ODT_MENU || dis.itemData == 0 {
        return false;
    }
    let data = unsafe { &*(dis.itemData as *const ItemData) };
    unsafe { draw_item(data, dis.hDC, dis.rcItem, dis.itemState.0) };
    #[cfg(test)]
    test_support::count_draw();
    true
}

/// 項目を `rect`（`hdc` の座標）に描く。作業用のメモリ DC（原点 (0, 0)）に描いてから写す。メモリ DC が
/// 作れなければ、`hdc` へ直接描く（空白の行にしない）。`rect` の外には描かない。
unsafe fn draw_item(data: &ItemData, hdc: HDC, rect: RECT, state: u32) {
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    if width <= 0 || height <= 0 {
        return;
    }
    let local = RECT { left: 0, top: 0, right: width, bottom: height };
    unsafe {
        match MemoryCanvas::new(hdc, width, height) {
            Some(canvas) => {
                paint(data, canvas.dc, local, state);
                let _ = BitBlt(hdc, rect.left, rect.top, width, height, Some(canvas.dc), 0, 0, SRCCOPY);
            }
            // 直接描くときは、変えた DC の状態（文字色・背景の描き方・選んだもの）を戻す
            // （Microsoft Learn の WM_DRAWITEM の説明）
            None => {
                let saved = SaveDC(hdc);
                paint(data, hdc, rect, state);
                if saved != 0 {
                    let _ = RestoreDC(hdc, saved);
                }
            }
        }
    }
}

/// 項目を `rc` に描く（`rc` の外には描かない）。
unsafe fn paint(data: &ItemData, hdc: HDC, rc: RECT, state: u32) {
    let res = &*data.resources;
    let dpi = res.dpi;
    let selected = state & ODS_SELECTED.0 != 0;
    let grayed = state & (ODS_GRAYED.0 | ODS_DISABLED.0) != 0;
    let colors = item_colors(selected && data.kind != ItemKind::Separator, grayed);
    unsafe {
        FillRect(hdc, &rc, GetSysColorBrush(colors.background));
        if data.kind == ItemKind::Separator {
            // C 版と同じく影と明るい線の2本（太さは DPI で換算）
            let thickness = scale(1, dpi).max(1);
            let mid = (rc.top + rc.bottom) / 2;
            let pad = scale(ITEM_PAD_X, dpi);
            let line = |top: i32, color: SYS_COLOR_INDEX| {
                let r = RECT { left: rc.left + pad, top, right: rc.right - pad, bottom: top + thickness };
                FillRect(hdc, &r, GetSysColorBrush(color));
            };
            line(mid - thickness, COLOR_3DSHADOW);
            line(mid, COLOR_3DHIGHLIGHT);
            return;
        }
        let text_color = COLORREF(GetSysColor(colors.text));
        let column = RECT {
            left: rc.left + scale(ITEM_PAD_X, dpi),
            top: rc.top,
            right: rc.left + scale(ITEM_PAD_X, dpi) + res.left_column,
            bottom: rc.bottom,
        };
        if let Some(thumb) = &data.thumbnail {
            let x = column.left + (res.left_column - thumb.width) / 2;
            let y = rc.top + (rc.bottom - rc.top - thumb.height) / 2;
            draw_thumbnail(hdc, thumb, x, y, thumb.width, thumb.height);
        } else if let Some(icon) = &data.icon {
            // アイコンは ICON_SIZE（DPI で換算）の大きさで、欄の中央に描く（素材より大きい倍率では引き伸ばす）。
            // 行の高さはアイコンで変えないので、行に収まらないときは行の高さに合わせて縮める
            let size = scale(ICON_SIZE, dpi).min(rc.bottom - rc.top - 2 * scale(THUMB_PAD, dpi)).max(1);
            let x = column.left + (res.left_column - size) / 2;
            let y = rc.top + (rc.bottom - rc.top - size) / 2;
            draw_thumbnail(hdc, icon, x, y, size, size);
        } else if state & ODS_CHECKED.0 != 0 {
            let size = scale(CHECK_COLUMN, dpi).min(rc.bottom - rc.top);
            let check = RECT {
                left: column.left + (res.left_column - size) / 2,
                top: rc.top + (rc.bottom - rc.top - size) / 2,
                right: column.left + (res.left_column - size) / 2 + size,
                bottom: rc.top + (rc.bottom - rc.top - size) / 2 + size,
            };
            draw_check(hdc, check, text_color);
        }
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, text_color);
        let _font = Selected::new(hdc, res.font.into());
        let mut text_rc = RECT {
            left: column.right + scale(COLUMN_GAP, dpi),
            top: rc.top,
            right: rc.right - scale(RIGHT_PAD, dpi),
            bottom: rc.bottom,
        };
        if !data.guide.is_empty() {
            draw_guide(hdc, &data.guide, text_rc.left, rc.top, rc.bottom, dpi, colors.text);
            text_rc.left += guide_width(data);
        }
        if !data.text.is_empty() {
            let mut text = data.text.clone();
            let mut flags = DT_SINGLELINE | DT_VCENTER;
            if state & ODS_NOACCEL.0 != 0 {
                flags |= DT_HIDEPREFIX;
            }
            DrawTextW(hdc, &mut text, &mut text_rc, flags);
        }
    }
}

/// ツリーの線の幅（段の数 × `GUIDE_STEP` を DPI で換算）。
fn guide_width(data: &ItemData) -> i32 {
    data.guide.len() as i32 * scale(GUIDE_STEP, data.resources.dpi)
}

/// ツリーの線の各段の矩形（`left` から段ごとに `GUIDE_STEP` ずつ。縦線は段の中央、横線は行の中央から段の
/// 右端の手前まで。縦線は行の上端から下端まで引くので、隣の行の線とすき間なくつながる）。
fn guide_rects(guide: &[TreeGuide], left: i32, top: i32, bottom: i32, dpi: u32) -> Vec<RECT> {
    let step = scale(GUIDE_STEP, dpi);
    let thickness = scale(1, dpi).max(1);
    let mid = (top + bottom) / 2;
    let mut rects = Vec::new();
    for (i, part) in guide.iter().enumerate() {
        let x = left + i as i32 * step + step / 2;
        let vertical = |to: i32| RECT { left: x, top, right: x + thickness, bottom: to };
        let horizontal = RECT { left: x, top: mid, right: left + (i as i32 + 1) * step - thickness, bottom: mid + thickness };
        match part {
            TreeGuide::Blank => {}
            TreeGuide::Pipe => rects.push(vertical(bottom)),
            TreeGuide::Branch => {
                rects.push(vertical(bottom));
                rects.push(horizontal);
            }
            TreeGuide::Last => {
                rects.push(vertical(mid + thickness));
                rects.push(horizontal);
            }
        }
    }
    rects
}

/// ツリーの線を、文字と同じ色（システム色）で描く。
unsafe fn draw_guide(hdc: HDC, guide: &[TreeGuide], left: i32, top: i32, bottom: i32, dpi: u32, color: SYS_COLOR_INDEX) {
    for r in guide_rects(guide, left, top, bottom, dpi) {
        unsafe {
            FillRect(hdc, &r, GetSysColorBrush(color));
        }
    }
}

/// 画像を (x, y) に `width`×`height` で描く（画像の大きさと違えば引き伸ばす）。アルファで下地と重ねる。
unsafe fn draw_thumbnail(hdc: HDC, thumb: &Thumbnail, x: i32, y: i32, width: i32, height: i32) {
    unsafe {
        let src = CreateCompatibleDC(Some(hdc));
        if src.is_invalid() {
            return;
        }
        {
            let _bmp = Selected::new(src, thumb.hbmp.into());
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let _ = AlphaBlend(hdc, x, y, width, height, src, 0, 0, thumb.width, thumb.height, blend);
        }
        let _ = DeleteDC(src);
    }
}

/// チェック印を `color` で描く。`DrawFrameControl` の白地に黒の印を反転して型にし、印の所だけを
/// 塗りつぶしの色にする（C 版 `menu_draw_ckeck` と同じ考え方。寸法は幅・高さで渡す）。
unsafe fn draw_check(hdc: HDC, rc: RECT, color: COLORREF) {
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    if w <= 0 || h <= 0 {
        return;
    }
    unsafe {
        let Some(mask) = MemoryCanvas::new(hdc, w, h) else {
            return;
        };
        let mut local = RECT { left: 0, top: 0, right: w, bottom: h };
        let _ = DrawFrameControl(mask.dc, &mut local, DFC_MENU, DFCS_MENUCHECK);
        let _ = PatBlt(mask.dc, 0, 0, w, h, DSTINVERT);
        let brush = CreateSolidBrush(color);
        if brush.is_invalid() {
            return;
        }
        {
            let _brush = Selected::new(hdc, brush.into());
            let _ = BitBlt(hdc, rc.left, rc.top, w, h, Some(mask.dc), 0, 0, ROP_DSPDXAX);
        }
        let _ = DeleteObject(brush.into());
    }
}

/// 文字列のアクセスキー（`&x` の x を小文字で。`&&` は文字の `&`）。最後のものを使う（C 版と同じ）。
fn mnemonic(text: &[u16]) -> Option<char> {
    let mut found = None;
    let mut i = 0;
    while i < text.len() {
        if text[i] == u16::from(b'&') {
            match text.get(i + 1) {
                Some(&next) if next == u16::from(b'&') => i += 1,
                Some(&next) => found = char::from_u32(u32::from(next)).map(|c| c.to_lowercase().next().unwrap_or(c)),
                None => {}
            }
        }
        i += 1;
    }
    found
}

/// `WM_MENUCHAR` の答え。
#[derive(Debug, PartialEq, Eq)]
enum MenuCharAnswer {
    /// その位置の項目を実行する（子メニューの項目なら子メニューが開く。実機で確認）
    Execute(usize),
    /// その位置の項目を選ぶだけ（同じアクセスキーの項目が複数ある）
    Select(usize),
    None,
}

/// 項目ごとのアクセスキー（選べない項目は None）と、今選んでいる位置から、押された文字 `ch` の答えを
/// 決める。今の位置の次から一周探し、候補が1つなら実行、2つ以上なら次の候補を選ぶ（C 版と同じ）。
fn resolve_menu_char(keys: &[Option<char>], hilite: Option<usize>, ch: char) -> MenuCharAnswer {
    let ch = ch.to_lowercase().next().unwrap_or(ch);
    let n = keys.len();
    if n == 0 {
        return MenuCharAnswer::None;
    }
    let start = hilite.map_or(0, |h| h + 1);
    let hits: Vec<usize> = (0..n).map(|k| (start + k) % n).filter(|&i| keys[i] == Some(ch)).collect();
    match hits.as_slice() {
        [] => MenuCharAnswer::None,
        [only] => MenuCharAnswer::Execute(*only),
        [first, ..] => MenuCharAnswer::Select(*first),
    }
}

/// `WM_MENUCHAR`。今のメニュー（lParam）に自前描画の項目があれば、アクセスキーを解決して戻り値を
/// 返す。自前描画の項目が無いメニュー（標準のメニューバー・窓メニュー）は None（既定の処理へ渡す）。
/// 候補から区切り線と、灰色・選べない項目を除く。
///
/// # Safety
/// 自前描画の項目の `itemData` は、このモジュールの `PopupMenu` が作った `ItemData` であること。
pub(crate) unsafe fn on_menu_char(wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    let Some(ch) = char::from_u32((wparam.0 & 0xFFFF) as u32) else {
        return None;
    };
    let menu = HMENU(lparam.0 as *mut _);
    let count = unsafe { GetMenuItemCount(Some(menu)) };
    if count <= 0 {
        return None;
    }
    let mut keys = Vec::with_capacity(count as usize);
    let mut hilite = None;
    let mut owner_drawn = false;
    for pos in 0..count as u32 {
        let mut info = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE | MIIM_STATE | MIIM_DATA,
            ..Default::default()
        };
        if unsafe { GetMenuItemInfoW(menu, pos, true, &mut info) }.is_err() {
            keys.push(None);
            continue;
        }
        if info.fState.0 & MFS_HILITE.0 != 0 {
            hilite = Some(pos as usize);
        }
        let usable = info.fType.0 & MFT_OWNERDRAW.0 != 0
            && info.fType.0 & MFT_SEPARATOR.0 == 0
            && info.fState.0 & MFS_DISABLED.0 == 0
            && info.dwItemData != 0;
        owner_drawn |= info.fType.0 & MFT_OWNERDRAW.0 != 0;
        let key = usable.then(|| mnemonic(&unsafe { &*(info.dwItemData as *const ItemData) }.text)).flatten();
        keys.push(key);
    }
    if !owner_drawn {
        return None;
    }
    let result = match resolve_menu_char(&keys, hilite, ch) {
        MenuCharAnswer::Execute(pos) => (MNC_EXECUTE << 16) | pos as u32,
        MenuCharAnswer::Select(pos) => (MNC_SELECT << 16) | pos as u32,
        // MNC_IGNORE（システムは短い音を鳴らす。標準のメニューと同じ）
        MenuCharAnswer::None => 0,
    };
    Some(LRESULT(result as isize))
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static MEASURED: AtomicUsize = AtomicUsize::new(0);
    static DRAWN: AtomicUsize = AtomicUsize::new(0);

    pub(super) fn count_measure() {
        MEASURED.fetch_add(1, Ordering::SeqCst);
    }

    pub(super) fn count_draw() {
        DRAWN.fetch_add(1, Ordering::SeqCst);
    }

    /// これまでに自前描画の項目を測った回数・描いた回数（実際のメニューが自前描画の経路を通ったかを
    /// 確かめる。GUI 用のロックの中で、前後の差を見る）。
    pub(crate) fn counts() -> (usize, usize) {
        (MEASURED.load(Ordering::SeqCst), DRAWN.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::EntryKind;
    use windows::Win32::Graphics::Gdi::{GetDIBits, DIB_RGB_COLORS as RGB_COLORS};

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn mnemonic_takes_last_single_ampersand_and_skips_doubled() {
        assert_eq!(mnemonic(&w("&1 abc")), Some('1'));
        assert_eq!(mnemonic(&w("履歴のクリア(&H)...")), Some('h'));
        assert_eq!(mnemonic(&w("Q&&A")), None);
        assert_eq!(mnemonic(&w("&0 Q&&A")), Some('0'));
        assert_eq!(mnemonic(&w("abc&")), None);
        assert_eq!(mnemonic(&w("")), None);
    }

    #[test]
    fn menu_char_executes_single_match_and_cycles_duplicates() {
        let keys = [Some('a'), None, Some('b'), Some('a'), None];
        assert_eq!(resolve_menu_char(&keys, None, 'B'), MenuCharAnswer::Execute(2));
        assert_eq!(resolve_menu_char(&keys, None, 'a'), MenuCharAnswer::Select(0));
        assert_eq!(resolve_menu_char(&keys, Some(0), 'a'), MenuCharAnswer::Select(3));
        assert_eq!(resolve_menu_char(&keys, Some(3), 'a'), MenuCharAnswer::Select(0));
        assert_eq!(resolve_menu_char(&keys, None, 'z'), MenuCharAnswer::None);
        assert_eq!(resolve_menu_char(&[], None, 'a'), MenuCharAnswer::None);
    }

    #[test]
    fn colors_follow_selection_and_gray_state() {
        assert_eq!(item_colors(false, false), ItemColors { background: COLOR_MENU, text: COLOR_MENUTEXT });
        assert_eq!(item_colors(true, false), ItemColors { background: COLOR_HIGHLIGHT, text: COLOR_HIGHLIGHTTEXT });
        assert_eq!(item_colors(false, true).text, COLOR_GRAYTEXT);
        assert_eq!(item_colors(true, true), ItemColors { background: COLOR_HIGHLIGHT, text: COLOR_GRAYTEXT });
    }

    #[test]
    fn sizes_scale_with_dpi_and_thumbnail_sets_row_height() {
        // `MenuResources` はフォントを作り、破棄で消す（GDI の数を数えるテストと重ねない）
        let _gui = crate::tray::lock_gui_resource_tests();
        let plain = MenuResources::new(96, None);
        let (w, h) = item_size(ItemKind::Command, (100, 16), false, &plain);
        assert_eq!(w as i32, ITEM_PAD_X + CHECK_COLUMN + COLUMN_GAP + 100 + RIGHT_PAD);
        assert_eq!(h as i32, 16 + 2 * TEXT_PAD_Y);
        assert_eq!(item_size(ItemKind::Separator, (0, 0), false, &plain), (0, SEPARATOR_HEIGHT as u32));
        let big = MenuResources::new(192, None);
        assert_eq!(item_size(ItemKind::Separator, (0, 0), false, &big).1 as i32, 2 * SEPARATOR_HEIGHT);
        // サムネイルを出すメニューは、全部の行がサムネイルの欄の幅を持つ（文字の始まりをそろえる）。
        // 行の高さをサムネイルに合わせるのは、サムネイルのある行だけ
        let thumbs = MenuResources::new(96, Some(32));
        let (w, h) = item_size(ItemKind::Command, (100, 16), false, &thumbs);
        assert_eq!(w as i32, ITEM_PAD_X + 32 + COLUMN_GAP + 100 + RIGHT_PAD);
        assert_eq!(h as i32, 16 + 2 * TEXT_PAD_Y);
        let (_, h) = item_size(ItemKind::Command, (100, 16), true, &thumbs);
        assert_eq!(h as i32, 32 + 2 * THUMB_PAD);
        // アイコンの行の高さは、アイコンの無い行と同じ（文字だけで決まる。アイコンで行の高さは変えない）
    }

    /// ツリーの線: 縦線は行の上端から下端まで（隣の行とすき間なくつながる。「└」は行の中央まで）、横線は
    /// 行の中央。段の幅・線の太さは DPI で換算し、空白の段は何も描かない。
    #[test]
    fn tree_guide_lines_span_the_row_height() {
        let (top, bottom) = (10, 34);
        let at_96 = guide_rects(&[TreeGuide::Pipe, TreeGuide::Blank, TreeGuide::Branch], 0, top, bottom, 96);
        assert_eq!(
            at_96,
            [
                RECT { left: 8, top, right: 9, bottom },
                RECT { left: 40, top, right: 41, bottom },
                RECT { left: 40, top: 22, right: 47, bottom: 23 },
            ]
        );
        let last = guide_rects(&[TreeGuide::Last], 100, top, bottom, 96);
        assert_eq!(last, [RECT { left: 108, top, right: 109, bottom: 23 }, RECT { left: 108, top: 22, right: 115, bottom: 23 }]);
        // 200%: 段の幅 32、太さ 2
        let at_192 = guide_rects(&[TreeGuide::Branch], 0, 0, 48, 192);
        assert_eq!(at_192, [RECT { left: 16, top: 0, right: 18, bottom: 48 }, RECT { left: 16, top: 24, right: 30, bottom: 26 }]);
    }

    #[test]
    fn premultiplies_and_swaps_to_bgra() {
        assert_eq!(premultiplied_bgra(&[255, 128, 0, 255]), vec![0, 128, 255, 255]);
        assert_eq!(premultiplied_bgra(&[255, 255, 255, 0]), vec![0, 0, 0, 0]);
        assert_eq!(premultiplied_bgra(&[200, 100, 50, 128]), vec![25, 50, 100, 128]);
    }

    /// 32bpp のメモリ DC（`MemoryCanvas` は画面と同じ形式）に描いて画素を読む。
    fn render(data: &ItemData, width: i32, height: i32, rect: RECT, state: u32) -> Vec<u32> {
        unsafe {
            let screen = GetDC(None);
            let canvas = MemoryCanvas::new(screen, width, height).unwrap();
            ReleaseDC(None, screen);
            // 外側を目立つ色で塗っておく（描画が rect の外を書き換えないかを見る）
            let brush = CreateSolidBrush(COLORREF(0x00FF00FF));
            FillRect(canvas.dc, &RECT { left: 0, top: 0, right: width, bottom: height }, brush);
            let _ = DeleteObject(brush.into());
            draw_item(data, canvas.dc, rect, state);
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut px = vec![0u32; (width * height) as usize];
            // GetDIBits は DC に選ばれていないビットマップを読む（Microsoft Learn の説明）
            SelectObject(canvas.dc, canvas.old);
            GetDIBits(canvas.dc, canvas.bitmap, 0, height as u32, Some(px.as_mut_ptr().cast()), &mut info, RGB_COLORS);
            px.iter().map(|p| p & 0x00FF_FFFF).collect()
        }
    }

    /// COLORREF（0x00BBGGRR）を、DIB の画素（0x00RRGGBB）にする。
    fn dib_color(index: SYS_COLOR_INDEX) -> u32 {
        let c = unsafe { GetSysColor(index) };
        ((c & 0xFF) << 16) | (c & 0xFF00) | ((c >> 16) & 0xFF)
    }

    fn command(text: &str, resources: &Rc<MenuResources>, thumbnail: Option<Thumbnail>) -> ItemData {
        ItemData {
            kind: ItemKind::Command,
            text: w(text),
            thumbnail,
            icon: None,
            guide: Vec::new(),
            resources: Rc::clone(resources),
        }
    }

    /// 選んでいる行は `COLOR_HIGHLIGHT`、そうでない行は `COLOR_MENU` で背景を塗る。原点が (0, 0) でない
    /// 矩形でも、矩形の外を書き換えない。チェック印は文字の色で描く。
    #[test]
    fn draws_background_check_and_stays_inside_rect() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let res = Rc::new(MenuResources::new(96, None));
        let item = command("&Alpha", &res, None);
        let (w, h) = (80, 40);
        let rect = RECT { left: 10, top: 12, right: 70, bottom: 34 };
        let at = |px: &[u32], x: i32, y: i32| px[(y * w + x) as usize];
        let magenta = 0x00FF00FF;

        let px = render(&item, w, h, rect, ODS_SELECTED.0);
        assert_eq!(at(&px, rect.right - 2, rect.top + 1), dib_color(COLOR_HIGHLIGHT));
        for (x, y) in [(rect.left - 1, rect.top), (rect.right, rect.top), (rect.left, rect.top - 1), (rect.left, rect.bottom)] {
            assert_eq!(at(&px, x, y), magenta, "矩形の外 ({x},{y}) を書き換えた");
        }
        let px = render(&item, w, h, rect, 0);
        assert_eq!(at(&px, rect.right - 2, rect.top + 1), dib_color(COLOR_MENU));

        // チェック印: 左の欄に文字の色の画素がある（無ければ背景の色だけ）
        let column = |px: &[u32]| {
            let left = rect.left + ITEM_PAD_X;
            (left..left + CHECK_COLUMN).flat_map(|x| (rect.top..rect.bottom).map(move |y| (x, y))).map(|(x, y)| at(px, x, y)).collect::<Vec<_>>()
        };
        let checked = render(&item, w, h, rect, ODS_CHECKED.0);
        assert!(column(&checked).contains(&dib_color(COLOR_MENUTEXT)), "チェック印を文字の色で描いていない");
        let unchecked = render(&item, w, h, rect, 0);
        assert!(column(&unchecked).iter().all(|&p| p == dib_color(COLOR_MENU)), "チェックの無い行の左の欄に何か描いた");
    }

    /// 半透明のサムネイルは背景と重ねて描く（アルファを掛けた画素で `AlphaBlend`）。
    #[test]
    fn thumbnail_blends_with_background() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let res = Rc::new(MenuResources::new(96, Some(8)));
        // 8x8、全部 R=255・G=B=0・アルファ 0（透明）→ 背景のまま
        let clear = Thumbnail::from_rgba(8, 8, &[255, 0, 0, 0].repeat(64)).unwrap();
        let opaque = Thumbnail::from_rgba(8, 8, &[255, 0, 0, 255].repeat(64)).unwrap();
        let rect = RECT { left: 0, top: 0, right: 60, bottom: 20 };
        let x = ITEM_PAD_X + 16 / 2;
        let y = 10;
        let clear_item = command("x", &res, Some(clear));
        let px = render(&clear_item, 60, 20, rect, 0);
        assert_eq!(px[(y * 60 + x) as usize], dib_color(COLOR_MENU));
        let opaque_item = command("x", &res, Some(opaque));
        let px = render(&opaque_item, 60, 20, rect, 0);
        let left = ITEM_PAD_X + (res.left_column - 8) / 2;
        assert_eq!(px[(y * 60 + left + 1) as usize], 0x00FF0000, "不透明なサムネイルを描いていない");

        // アルファ 128 の赤は、背景（選んでいる行の COLOR_HIGHLIGHT）と半分ずつ混ざる（丸めの誤差は 2 まで）
        let half = Thumbnail::from_rgba(8, 8, &[255, 0, 0, 128].repeat(64)).unwrap();
        let half_item = command("x", &res, Some(half));
        let px = render(&half_item, 60, 20, rect, ODS_SELECTED.0);
        let got = px[(y * 60 + left + 1) as usize];
        let bg = dib_color(COLOR_HIGHLIGHT);
        let channel = |c: u32, shift: u32| ((c >> shift) & 0xFF) as i32;
        let expect = |fg: i32, back: i32| fg * 128 / 255 + back * 127 / 255;
        for (shift, fg) in [(16, 255), (8, 0), (0, 0)] {
            let diff = (channel(got, shift) - expect(fg, channel(bg, shift))).abs();
            assert!(diff <= 2, "半透明の合成がずれている: got {got:06x} bg {bg:06x}");
        }
    }

    /// 実際のメニューの `WM_MENUCHAR`: 自前描画の項目のアクセスキーで位置が返る。灰色の項目・区切り線は
    /// 候補にしない。自前描画の項目の無いメニューは既定の処理へ渡す（None）。
    #[test]
    fn menu_char_on_real_menu_skips_grayed_items() {
        use windows::Win32::UI::WindowsAndMessaging::{MF_GRAYED, MF_STRING};
        let _gui = crate::tray::lock_gui_resource_tests();
        let mut menu = PopupMenu::new(96, None).unwrap();
        let root = menu.handle();
        menu.append_command(root, 1, "&1 one", MENU_ITEM_FLAGS(0), None);
        menu.append_separator(root);
        menu.append_command(root, 2, "&2 two", MF_GRAYED, None);
        menu.append_command(root, 3, "&3 three", MENU_ITEM_FLAGS(0), None);
        let ask = |c: char| unsafe { on_menu_char(WPARAM(c as usize | (0x10 << 16)), LPARAM(root.0 as isize)) };
        assert_eq!(ask('1'), Some(LRESULT(((MNC_EXECUTE << 16) | 0) as isize)));
        assert_eq!(ask('3'), Some(LRESULT(((MNC_EXECUTE << 16) | 3) as isize)));
        assert_eq!(ask('2'), Some(LRESULT(0)), "灰色の項目を候補にした");

        let standard = unsafe { CreatePopupMenu() }.unwrap();
        unsafe {
            let _ = AppendMenuW(standard, MF_STRING, 1, windows::core::w!("&x"));
            assert_eq!(on_menu_char(WPARAM('x' as usize), LPARAM(standard.0 as isize)), None);
            let _ = DestroyMenu(standard);
        }
    }

    /// アイコンのビットマップは、同じ素材の行どうしで1つを共有する（千行近いメニューでも GDI の
    /// オブジェクトを行の数だけ作らない）。アイコンは左の欄に `ICON_SIZE` の大きさで描く。
    #[test]
    fn icons_are_shared_and_drawn_in_column() {
        use crate::icons;
        let _gui = crate::tray::lock_gui_resource_tests();
        let mut menu = PopupMenu::new(96, Some(32)).unwrap();
        let root = menu.handle();
        for id in 1..=5 {
            menu.append_command(root, id, "text", MENU_ITEM_FLAGS(0), Some(MenuImage::Icon(icons::TEXT)));
        }
        menu.append_command(root, 6, "file", MENU_ITEM_FLAGS(0), Some(MenuImage::Icon(icons::FILE)));
        let sub = menu.add_submenu("16〜115", Some(icons::FOLDER)).unwrap();
        menu.append_command(sub, 7, "text", MENU_ITEM_FLAGS(0), Some(MenuImage::Icon(icons::for_kind(EntryKind::Text))));
        assert_eq!(menu.store.icons.len(), 3, "同じ素材のアイコンを行ごとに作った");
        assert!(Rc::ptr_eq(menu.store.items[0].icon.as_ref().unwrap(), menu.store.items[4].icon.as_ref().unwrap()));
        assert!(Rc::ptr_eq(menu.store.items[0].icon.as_ref().unwrap(), menu.store.items[7].icon.as_ref().unwrap()));

        // 左の欄（サムネイルの大きさ 32 の中央の 16px）に、背景以外の画素がある
        let rect = RECT { left: 0, top: 0, right: 120, bottom: 24 };
        let px = render(&menu.store.items[0], 120, 24, rect, 0);
        let left = ITEM_PAD_X + (32 - ICON_SIZE) / 2;
        let drawn = (left..left + ICON_SIZE).flat_map(|x| (4..20).map(move |y| (x, y))).any(|(x, y)| px[(y * 120 + x) as usize] != dib_color(COLOR_MENU));
        assert!(drawn, "アイコンを描いていない");
    }

    /// 子メニューを付けると、親と一緒に破棄される（`PopupMenu` の破棄で子メニューのハンドルも無効になる）。
    /// 子メニューの中に足した子メニュー（入れ子）も同じ。
    #[test]
    fn submenu_is_destroyed_with_parent() {
        use windows::Win32::UI::WindowsAndMessaging::IsMenu;
        let _gui = crate::tray::lock_gui_resource_tests();
        let mut menu = PopupMenu::new(96, None).unwrap();
        let sub = menu.add_submenu("子", Some(crate::icons::FOLDER)).unwrap();
        menu.append_command(sub, 1, "a", MENU_ITEM_FLAGS(0), None);
        let nested = menu.add_submenu_to(sub, "孫", Some(crate::icons::FOLDER)).unwrap();
        menu.append_command(nested, 2, "b", MENU_ITEM_FLAGS(0), None);
        let root = menu.handle();
        assert_eq!(unsafe { windows::Win32::UI::WindowsAndMessaging::GetSubMenu(sub, 1) }, nested, "入れ子の子メニューが子メニューに付いていない");
        assert!([root, sub, nested].iter().all(|m| unsafe { IsMenu(*m) }.as_bool()));
        drop(menu);
        assert!([root, sub, nested].iter().all(|m| !unsafe { IsMenu(*m) }.as_bool()));
    }
}
