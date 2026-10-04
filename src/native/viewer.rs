//! ビューアのトップレベル窓。
//!
//! メインスレッドで作り、メインスレッドのメッセージループで動かす。トレイ・ホットキー・クリップボード監視の各スレッドは
//! `Waker` で `WM_APP_WAKE` を投げてこの窓を起こすだけで、実際の処理は `ViewerHandler`
//! （`native::app::App`）がメインスレッド上で行う。
//!
//! 子コントロールは、ツリー（`SysTreeView32`、表示対象の選択）・一覧（`SysListView32`、
//! OwnerData + OwnerDrawFixed の仮想一覧）・プレビュー（EDIT）。
//! 一覧は窓が持つ行の写し（`ViewState::rows`）だけを読んで描く（描画中にサービスの
//! ロックや blob の読み込みをしない）。
//!
//! 再入の規則: 一覧やツリーを変えるメッセージ（`LVM_SETITEMCOUNT`・`TVM_INSERTITEMW` など）は、
//! 送った側へ `WM_DRAWITEM`・`WM_NOTIFY` を返してくる。`ViewState` の借用はそれらを送る前に
//! 必ず手放す。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};

use windows::Win32::Graphics::Gdi::{
    AlphaBlend, BeginPaint, CreateCompatibleDC, CreateDIBSection, DeleteDC, EndPaint, AC_SRC_ALPHA,
    AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC,
    PAINTSTRUCT,
};
use windows::Win32::UI::Controls::LVM_REDRAWITEMS;
use windows::Win32::UI::WindowsAndMessaging::{GetParent, WM_ERASEBKGND, WM_PAINT};

use crate::native::images::{ImageContent, ImageRequest, ImageSource, LoadedImage, Purpose, WantedPreview, WantedThumbs};

use windows::core::{w, HSTRING, PCWSTR, PWSTR, Result as WinResult};
use windows::Win32::Foundation::{
    GetLastError, ERROR_CLASS_ALREADY_EXISTS, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, DeleteObject, DrawTextW, FillRect, GetDC, GetStockObject, GetSysColor,
    GetSysColorBrush, GetTextMetricsW, InvalidateRect, ReleaseDC, SelectObject, SetBkMode,
    SetTextColor, COLOR_GRAYTEXT, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT,
    COLOR_WINDOW, COLOR_WINDOWTEXT, DEFAULT_GUI_FONT, DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE,
    DT_VCENTER, FW_BOLD, HBRUSH, HFONT, TEXTMETRICW, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, DRAWITEMSTRUCT, HTREEITEM, ICC_LISTVIEW_CLASSES, ICC_TREEVIEW_CLASSES,
    INITCOMMONCONTROLSEX, LVCF_WIDTH, LVCOLUMNW, LVIS_FOCUSED, LVIS_SELECTED, LVITEMW,
    LVM_ENSUREVISIBLE, LVM_GETNEXTITEM, LVM_INSERTCOLUMNW, LVM_SETCOLUMNWIDTH, LVM_SETITEMCOUNT,
    LVM_SETITEMSTATE, LVNI_SELECTED, LVSICF_NOSCROLL, LVS_NOCOLUMNHEADER,
    LVS_OWNERDATA, LVS_OWNERDRAWFIXED, LVS_REPORT, LVS_SHOWSELALWAYS,
    MEASUREITEMSTRUCT, NMHDR, NMTREEVIEWW, ODS_SELECTED, ODT_LISTVIEW, TVE_EXPAND,
    TVGN_CARET, TVIF_PARAM, TVIF_TEXT, TVINSERTSTRUCTW, TVINSERTSTRUCTW_0, TVITEMW, TVI_LAST,
    TVI_ROOT, TVM_DELETEITEM, TVM_EXPAND, TVM_INSERTITEMW, TVM_SELECTITEM, TVN_SELCHANGEDW,
    TVS_HASBUTTONS, TVS_HASLINES, TVS_LINESATROOT, TVS_SHOWSELALWAYS, WC_LISTVIEWW, WC_TREEVIEWW,
    EM_SETCUEBANNER, LVIF_STATE, LVN_ITEMCHANGED, LVN_ODSTATECHANGED, NMLISTVIEW, NMLVODSTATECHANGE,
};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Controls::{ImageList_Create, ImageList_Destroy, HIMAGELIST, ILC_COLOR32, LVM_SETIMAGELIST, LVSIL_SMALL};
use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER, WM_DPICHANGED, WM_ENDSESSION};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus, VK_DELETE};
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
use windows::Win32::UI::Controls::{
    LVHITTESTINFO, LVIR_BOUNDS, LVM_GETITEMRECT, LVM_HITTEST, LVN_KEYDOWN, NMITEMACTIVATE, NMLVKEYDOWN, NM_DBLCLK,
    NM_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DrawMenuBar, EndMenu, SetMenu, TrackPopupMenu, IDOK, MF_STRING, WM_MENUCHAR,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_CONTEXTMENU, WM_CTLCOLORSTATIC, WM_DESTROY,
};
use windows::Win32::UI::WindowsAndMessaging::{IsChild, IsWindow, WA_INACTIVE, WM_ACTIVATE};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::core::HRESULT;
use windows::Win32::Foundation::S_OK;
use windows::Win32::UI::Controls::{
    ImageList_ReplaceIcon, TaskDialogIndirect, TASKDIALOGCONFIG, TASKDIALOGCONFIG_0, TASKDIALOG_BUTTON,
    TASKDIALOG_NOTIFICATIONS, TDCBF_CANCEL_BUTTON, TDCBF_CLOSE_BUTTON, TDF_ALLOW_DIALOG_CANCELLATION, TDF_POSITION_RELATIVE_TO_WINDOW,
    TDM_CLICK_BUTTON, TDN_CREATED, TDN_DESTROYED, TD_WARNING_ICON, TVIF_HANDLE, TVIF_IMAGE, TVIF_SELECTEDIMAGE,
    TVM_SETIMAGELIST, TVM_SETITEMW, TVSIL_NORMAL, EM_SETSEL,
};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetSystemMetricsForDpi};
use windows::Win32::UI::Controls::LoadIconWithScaleDown;
use windows::Win32::UI::WindowsAndMessaging::{ICON_BIG, ICON_SMALL, SM_CXICON, SM_CXSMICON, WM_SETICON};
use windows::Win32::UI::Controls::{EM_LIMITTEXT, TD_INFORMATION_ICON};
use windows::Win32::UI::WindowsAndMessaging::IDCLOSE;
use crate::datacheck::{CleanResult, DataReport, TempPlace};
use windows::Win32::UI::WindowsAndMessaging::{
    DialogBoxParamW, EndDialog, WINDOW_LONG_PTR_INDEX, WM_INITDIALOG,
};
use windows::Win32::Graphics::Gdi::COLOR_BTNFACE;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetCapture, ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, SetCursor, HTCLIENT, IDC_SIZENS, IDC_SIZEWE, WM_CAPTURECHANGED, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_SETCURSOR,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CheckMenuItem, CreateAcceleratorTableW, DestroyAcceleratorTable, GetMenu, TranslateAcceleratorW,
    ACCEL, FCONTROL, FVIRTKEY, HACCEL, HWND_NOTOPMOST, HWND_TOPMOST, MF_BYCOMMAND, MF_CHECKED, MF_GRAYED,
    MF_UNCHECKED, MINMAXINFO, MSG, SIZE_RESTORED, SWP_NOMOVE, SWP_NOSIZE, WM_ENTERMENULOOP,
    WM_EXITMENULOOP, WM_GETMINMAXINFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow,
    DrawIconEx, FindWindowW, GetClientRect, GetDlgItem, GetWindowLongPtrW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, LoadCursorW, MoveWindow, PostMessageW,
    RegisterClassW, SendMessageW, SetForegroundWindow, SetWindowLongPtrW, ShowWindow,
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, DI_NORMAL, ES_AUTOVSCROLL,
    ES_MULTILINE, ES_READONLY, GWLP_USERDATA, HICON, HMENU, IDC_ARROW, NONCLIENTMETRICSW,
    SPI_GETNONCLIENTMETRICS, SW_HIDE, SW_RESTORE, SW_SHOW, 
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_DRAWITEM, WM_MEASUREITEM, WM_NCDESTROY,
    WM_NOTIFY, WM_SETFONT, WM_SHOWWINDOW, WM_SIZE, WNDCLASSW, WS_CHILD, WS_CLIPCHILDREN,
    WS_EX_CLIENTEDGE, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
    EN_CHANGE, ES_AUTOHSCROLL, GetWindowTextLengthW, GetWindowTextW, IDCANCEL, KillTimer, SetTimer,
    SetWindowTextW, WM_COMMAND, WM_TIMER, DLGC_WANTALLKEYS, DLGC_WANTTAB, WM_GETDLGCODE,
};

use uuid::Uuid;

// 【読み上げ】ここから — 一覧の読み上げ用の文字列（要件ではない）。
// この印の対（開始と終了の行を含む）の範囲をすべて消すと外れる（印はこのファイルの中だけ）
use windows::Win32::UI::Controls::{LVIF_TEXT, LVN_GETDISPINFOW, NMLVDISPINFOW};
// 【読み上げ】ここまで

use crate::native::model::{
    delete_folder_message, EntryKind, Row, RowCommand, RowMenu, RowTarget, Source, ToolCommand, TreeCommand, TreeMenu,
    TreeNode, HISTORY_LABEL,
};
use windows::Win32::UI::WindowsAndMessaging::{WM_RBUTTONDBLCLK, WM_RBUTTONDOWN, WM_RBUTTONUP};
use windows::Win32::UI::Controls::{
    TVGN_DROPHILITE, TVHITTESTINFO, TVHT_ONITEM, TVM_ENSUREVISIBLE, TVM_GETITEMRECT, TVM_GETITEMW, TVM_GETNEXTITEM,
    TVM_HITTEST,
};
use windows::Win32::UI::Controls::{
    NMTVDISPINFOW, NMTVKEYDOWN, TVC_BYKEYBOARD, TVC_BYMOUSE, TVGN_CHILD, TVGN_NEXT, TVGN_ROOT, TVM_EDITLABELW,
    TVM_ENDEDITLABELNOW, TVN_BEGINLABELEDITW, TVN_ENDLABELEDITW, TVN_KEYDOWN, TVS_EDITLABELS,
};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_F2;
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_DOWN, VK_UP};
use windows::Win32::UI::WindowsAndMessaging::FALT;
use crate::store::Direction;
use windows::Win32::Graphics::Gdi::{CreateCompatibleBitmap, MapWindowPoints, UpdateWindow, DT_CALCRECT};
use windows::Win32::UI::Controls::{LVN_BEGINDRAG, TVGN_PARENT, TVN_BEGINDRAGW};
use windows::Win32::UI::WindowsAndMessaging::IDC_NO;
use crate::icons;
use crate::menu_draw::{self, MenuBar, PopupMenu};
use crate::tools::text::TextTransform;

/// 他スレッドからの起床要求。受けたら `ViewerHandler::on_wake` を呼ぶ。
pub const WM_APP_WAKE: u32 = WM_APP + 1;
/// 二重起動した側のプロセスからの表示要求（`request_show_existing`）。
pub const WM_APP_SHOW_REQUEST: u32 = WM_APP + 2;
/// 画像の読み込みスレッドから結果が届いた（`ViewerWindow::image_notifier`）。
const WM_APP_IMAGE: u32 = WM_APP + 3;
/// 一覧の選択の変化をまとめて1回だけハンドラへ伝える（`report_selection`）。
const WM_APP_SELECTION: u32 = WM_APP + 4;
/// ツリーの名前の編集の後始末（`TVN_ENDLABELEDIT` から戻った後に行う）
const WM_APP_EDIT_DONE: u32 = WM_APP + 5;
/// 一覧のフォルダの行を開く（一覧の通知の処理から戻った後に、ツリーでそのフォルダを選ぶ。`activate_row`）
const WM_APP_OPEN_FOLDER: u32 = WM_APP + 6;

/// ツリーの仮の項目（フォルダの作成で名前を入れる間だけ置く）の lParam。`tree_tokens` に無いので、
/// 表示元として扱われない。
const TEMP_TOKEN: usize = usize::MAX;
/// 仮の項目の最初の名前。
const NEW_FOLDER_LABEL: &str = "新しいフォルダ";

/// ツリーの名前の編集の対象。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditTarget {
    /// ピン留めのフォルダの名前の変更
    Rename { id: Uuid },
    /// `parent`（None はピン留めの根）の中にフォルダを作る（名前は仮の項目で入れる）
    Create { parent: Option<Uuid> },
}

/// ツリーの名前の編集の状態。`None` 以外の間は、ツリーの作り直しを保留し、新しい編集・
/// 右クリックメニュー・確認ダイアログ・失敗の通知を止め、選択の変化を表示元の切り替えとして伝えない。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditState {
    None,
    /// `TVM_EDITLABEL` を送る前から、`TVN_BEGINLABELEDIT` が来るまで（取り消しの印は `edit_cancel`）
    Starting(EditTarget),
    Editing(EditTarget),
    /// `TVN_ENDLABELEDIT`（または開始の失敗）から、後始末（`WM_APP_EDIT_DONE`）が終わるまで
    Finishing,
}

/// プレビューの画像を描く子窓のクラス名。
const IMAGE_CLASS_NAME: PCWSTR = w!("CLCLR_ImagePreview");

/// ビューア窓のクラス名。二重起動時に既存の窓を探すのにも使う（タイトルで探すと、
/// 同じ名前のフォルダを開いているエクスプローラーの窓に当たるため、クラス名で探す）。
pub const CLASS_NAME: PCWSTR = w!("CLCLR_Viewer");

const ID_TREE: i32 = 100;
const ID_LIST: i32 = 101;
const ID_PREVIEW: i32 = 102;
const ID_SEARCH: i32 = 103;
/// プレビューの画像の子窓（画像のときだけ EDIT の代わりに出す）
const ID_IMAGE: i32 = 104;

/// メニューバーとアクセラレータのコマンド ID（子コントロールの ID・IDOK・IDCANCEL と重ならない値）。
const CMD_SETTINGS: u16 = 200;
const CMD_CLEAR_HISTORY: u16 = 201;
const CMD_CLEAR_CLIPBOARD: u16 = 202;
const CMD_TOPMOST: u16 = 203;
const CMD_ABOUT: u16 = 204;
/// Ctrl+F で検索欄へ（アクセラレータだけで、メニューには出さない）
const CMD_FIND: u16 = 205;
/// データのチェック
const CMD_CHECK_DATA: u16 = 206;
/// Alt+↑・Alt+↓ で、一覧で選んでいるピン留めの行を上・下へ（アクセラレータだけで、メニューバーには出さない）
const CMD_MOVE_UP: u16 = 207;
const CMD_MOVE_DOWN: u16 = 208;

/// 履歴のクリアの確認の「削除」ボタンの ID（`IDCANCEL` などの共通ボタンと重ならない値）。
const CONFIRM_DELETE: i32 = 1000;

/// ビューア窓のスタイル（大きさの換算 `AdjustWindowRectExForDpi` でも使う）。
const VIEWER_STYLE: WINDOW_STYLE = WINDOW_STYLE(WS_OVERLAPPEDWINDOW.0 | WS_CLIPCHILDREN.0);

/// 検索欄の入力が止まってから絞り込むまでの待ち（打鍵ごとに全件を調べないため）。
/// IME の変換中は EDIT の中身が変わらないので、確定してから絞り込まれる。
const SEARCH_TIMER_ID: usize = 1;
const SEARCH_DELAY_MS: u32 = 150;

/// 表示している間、一覧の2行目の経過時間（「N分前」、分の刻み）を古くしないための描き直しの間隔。
const AGE_TIMER_ID: usize = 2;
const AGE_REFRESH_MS: u32 = 30_000;

/// 終了の要求の後もメニュー・確認ダイアログが開いているときに、閉じる要求をやり直すタイマー
/// （`retry_modal_close`）
const CLOSE_RETRY_TIMER_ID: usize = 3;
const CLOSE_RETRY_MS: u32 = 500;

/// プレビュー欄が広がって、今の画像より大きく出せるようになったときの読み直しの待ち（窓の大きさや
/// 境目をドラッグしている間に何度も頼まないため）。
const PREVIEW_REFIT_TIMER_ID: usize = 4;
const PREVIEW_REFIT_DELAY_MS: u32 = 200;

/// 種別アイコン・サムネイルの大きさと、行の上下左右の余白（96 DPI 基準。実際の大きさは
/// 窓の DPI に合わせる。`Metrics`）。
const ICON_SIZE: i32 = 32;
const ROW_PAD: i32 = 4;
/// ツリーのアイコンの大きさ（96 DPI 基準。32px の素材を縮めて使う）。
const TREE_ICON_SIZE: i32 = 16;

/// 窓の DPI に合わせたフォントと寸法。作成時と `WM_DPICHANGED` で作り直す（`apply_dpi`）。
struct Metrics {
    font: HFONT,
    bold_font: HFONT,
    /// 既定の GUI フォントを借りている（取得に失敗したとき。解放しない）
    stock_font: bool,
    icon_size: i32,
    pad: i32,
    row_height: i32,
    search_height: i32,
    /// ツリーのアイコン（履歴・フォルダ・ピン留めの順。`tree_image`）。ツリーはこれを破棄しない
    /// （Microsoft Learn の TVM_SETIMAGELIST の説明）ので、差し替えた後に Drop で破棄する
    tree_images: HIMAGELIST,
    /// 窓のアイコン（exe のアイコンのリソース。小＝タイトルバー、大＝Alt+Tab）。窓は WM_SETICON で
    /// 渡したアイコンを破棄しないので、差し替えた後に Drop で破棄する（`set_window_icons`）
    window_icons: [HICON; 2],
}

impl Metrics {
    fn new(hwnd: HWND, dpi: u32) -> Self {
        let (font, bold_font, stock_font) = create_fonts(dpi);
        let icon_size = crate::menu_tooltip::scale_for_dpi(ICON_SIZE, dpi);
        let pad = crate::menu_tooltip::scale_for_dpi(ROW_PAD, dpi);
        Self {
            font,
            bold_font,
            stock_font,
            icon_size,
            pad,
            row_height: row_height(hwnd, font, bold_font, icon_size, pad),
            search_height: search_height(hwnd, font, pad),
            tree_images: create_tree_images(crate::menu_tooltip::scale_for_dpi(TREE_ICON_SIZE, dpi)),
            window_icons: [SM_CXSMICON, SM_CXICON].map(|metric| load_app_icon(unsafe { GetSystemMetricsForDpi(metric, dpi) })),
        }
    }
}

impl Drop for Metrics {
    fn drop(&mut self) {
        unsafe {
            if !self.tree_images.is_invalid() {
                let _ = ImageList_Destroy(Some(self.tree_images));
            }
            for icon in self.window_icons {
                if !icon.is_invalid() {
                    let _ = DestroyIcon(icon);
                }
            }
        }
        if !self.stock_font {
            unsafe {
                let _ = DeleteObject(self.font.into());
                let _ = DeleteObject(self.bold_font.into());
            }
        }
    }
}


/// ツリーのアイコンのイメージリストを作る（`size` px 角。32px のアイコンはイメージリストが
/// 縮めて入れる）。作れなければ無効なハンドル（アイコンなしで表示する）。
fn create_tree_images(size: i32) -> HIMAGELIST {
    unsafe {
        let images = ImageList_Create(size, size, ILC_COLOR32, 3, 0);
        if images.is_invalid() {
            return images;
        }
        for rgba in [icons::HISTORY, icons::FOLDER, icons::PINNED] {
            let icon = crate::tray::icon_from_rgba(rgba, ICON_SIZE).unwrap_or_default();
            // 失敗した分は -1 が返るだけ（後の添字がずれないよう、失敗しても続けない）
            if icon.is_invalid() || ImageList_ReplaceIcon(images, -1, icon) < 0 {
                if !icon.is_invalid() {
                    let _ = DestroyIcon(icon);
                }
                let _ = ImageList_Destroy(Some(images));
                return HIMAGELIST::default();
            }
            // イメージリストは写しを持つので、元のアイコンは破棄してよい
            let _ = DestroyIcon(icon);
        }
        images
    }
}

/// exe のアイコンのリソース（`res/app.rc` の ID 1）を `size` px 角で読む。リソースにその大きさが
/// 無ければ、大きい方を縮めて作る（Microsoft Learn の LoadIconWithScaleDown）。読めなければ無効な
/// ハンドル（既定のアイコンのまま）。
fn load_app_icon(size: i32) -> HICON {
    unsafe {
        let Ok(module) = GetModuleHandleW(None) else {
            return HICON::default();
        };
        LoadIconWithScaleDown(Some(module.into()), PCWSTR(1 as *const u16), size, size).unwrap_or_default()
    }
}

/// `Metrics` の窓のアイコンを窓に付ける（作成時と DPI の変更の後。古いアイコンは呼び出し側が
/// `Metrics` ごと破棄する）。
fn set_window_icons(hwnd: HWND) {
    let Some(ctx) = (unsafe { ctx_ref(hwnd) }) else {
        return;
    };
    let [small, big] = ctx.metrics.borrow().window_icons;
    unsafe {
        SendMessageW(hwnd, WM_SETICON, Some(WPARAM(ICON_SMALL as usize)), Some(LPARAM(small.0 as isize)));
        SendMessageW(hwnd, WM_SETICON, Some(WPARAM(ICON_BIG as usize)), Some(LPARAM(big.0 as isize)));
    }
}

/// ツリーの項目のアイコンの添字（`create_tree_images` の順）。
fn tree_image(source: Source) -> i32 {
    match source {
        Source::History => 0,
        Source::Pinned(None) => 2,
        Source::HistoryGroup(_) | Source::Pinned(Some(_)) => 1,
    }
}

/// ツリーの項目の表示名。「履歴」には件数を添える（「履歴 (件数)」）。
fn tree_label(node: &TreeNode, history_count: usize) -> String {
    match node.source {
        Source::History => format!("{} ({history_count})", node.label),
        _ => node.label.clone(),
    }
}


/// ビューア窓の出来事を受け取る側（メインスレッドで呼ばれる）。
pub trait ViewerHandler {
    /// 他スレッドから `WM_APP_WAKE` が届いた。
    fn on_wake(&self, hwnd: HWND);
    /// 閉じる操作（×・Alt+F4・`WM_CLOSE`）。窓を隠すか終了するかは受け取った側が決める。
    /// 窓はここでは破棄しない（`DefWindowProcW` の既定処理は `DestroyWindow` を呼ぶため流さない）。
    fn on_close(&self, hwnd: HWND);
    /// 窓が表示された（非表示の間に溜まった変更を反映する契機）。
    fn on_shown(&self, hwnd: HWND);
    /// ツリーで表示対象が選ばれた（ユーザーの操作。`set_tree` による再構築中は呼ばない）。
    fn on_source_selected(&self, hwnd: HWND, source: Source);
    /// 窓が隠れた（プレビューや検索用のキャッシュを捨てる契機）。
    fn on_hidden(&self, hwnd: HWND);
    /// 一覧の選択が変わった（ユーザーの操作。`set_rows` による変化は戻り値で返し、ここでは呼ばない）。
    fn on_selection_changed(&self, hwnd: HWND, id: Option<Uuid>);
    /// 検索欄の入力が変わった（入力が止まってから `SEARCH_DELAY_MS` 後）。
    fn on_search_changed(&self, hwnd: HWND, text: String);
    /// 窓の DPI が変わり、フォント・寸法を作り直した（プレビューの画像は捨ててあるので出し直す）。
    fn on_metrics_changed(&self, hwnd: HWND);
    /// Windows のセッションが終わる（サインアウト・再起動・シャットダウン。`WM_ENDSESSION` の
    /// wParam が TRUE）。この通知から戻るとプロセスはいつ終了させられてもおかしくなく、
    /// メッセージループを抜けた後の終了処理は行われない。保存などはここで済ませる。
    fn on_end_session(&self, hwnd: HWND);
    /// 一覧の行を送る（一覧にフォーカスがあるときの Enter・ダブルクリック）。対象は、その行を
    /// 作ったときの表示元の項目（`RowTarget`）。フォルダの行では呼ばない（ビューアがツリーでそのフォルダを選ぶ）。
    fn on_activate(&self, hwnd: HWND, target: RowTarget);
    /// 一覧の行を消す（一覧にフォーカスがあるときの Delete）。フォルダの行では呼ばない（ビューアが確認してから
    /// `on_tree_command` で伝える）。
    fn on_delete(&self, hwnd: HWND, target: RowTarget);
    /// 行の右クリックメニューに出す項目。None ならメニューを出さない（項目が見つからないなど）。
    /// サービスのロックは写しを取る間だけ持つ。
    fn row_menu(&self, target: RowTarget) -> Option<RowMenu>;
    /// 右クリックメニューで選ばれた操作（メニューが閉じた後に呼ぶ。対象はメニューを出す前に決めたもの）。
    /// ドラッグで行を落としたとき（`RowCommand::Place`。マウスを放した後）もここへ来る。
    fn on_row_command(&self, hwnd: HWND, target: RowTarget, command: RowCommand);
    /// 一覧の行をドラッグできるか（ドラッグを始める前に聞く）。ピン留めの行は移動、履歴の行はツリーのピン留めへの
    /// ピン留めになる。
    fn can_drag_row(&self, target: RowTarget) -> bool;
    /// メニューバーの「ツール」の操作（メニューが閉じた後に呼ぶ。履歴のクリアは確認で「削除」を
    /// 選んだときだけ）。ツリーの「履歴」の右クリックメニューの「履歴のクリア...」もここへ来る。
    fn on_tool_command(&self, hwnd: HWND, command: ToolCommand);
    /// ツリーの項目の右クリックメニューに出すもの。None ならメニューを出さない。サービスのロックは
    /// メタデータを数える間だけ持つ。
    fn tree_menu(&self, source: Source) -> Option<TreeMenu>;
    /// ツリーの右クリックメニューで選ばれた操作（メニューと確認が閉じた後に、確認で「削除」を
    /// 選んだときだけ呼ぶ）。
    fn on_tree_command(&self, hwnd: HWND, command: TreeCommand);
    /// メニューバーの「ツール」→「設定...」（メニューが閉じた後に呼ぶ）。
    fn on_open_settings(&self, _hwnd: HWND) {}
    /// ピン留めのアイテムの今の名前（名前の変更のダイアログに入れる。外の None はアイテムが無い＝
    /// ダイアログを出さない、内の None は名前が無い）。サービスのロックは写しを取る間だけ持つ。
    fn pinned_title(&self, id: Uuid) -> Option<Option<String>>;
    /// 名前の変更のダイアログで「OK」を押した（空なら自動の名前に戻す。ダイアログが閉じた後に呼ぶ）。
    fn on_rename_pinned(&self, hwnd: HWND, id: Uuid, title: String);
}

/// 他スレッドからビューア窓を起こすための値。HWND は `Send` でないため整数で持つ。
#[derive(Clone, Copy)]
pub struct Waker(isize);

impl Waker {
    pub fn wake(&self) {
        unsafe {
            let _ = PostMessageW(Some(HWND(self.0 as *mut _)), WM_APP_WAKE, WPARAM(0), LPARAM(0));
        }
    }
}

/// アルファを掛けた 32bpp の DIB セクション（トップダウン）。Drop で解放する。
struct Bitmap {
    hbmp: HBITMAP,
    width: i32,
    height: i32,
}

impl Bitmap {
    /// `images::LoadedImage` の BGRA（アルファを掛けたもの）から作る。
    fn from_bgra(width: u32, height: u32, bgra: &[u8]) -> Option<Self> {
        if width == 0 || height == 0 || bgra.len() != (width * height * 4) as usize {
            return None;
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                // 負の高さでトップダウン
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
        unsafe {
            std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits.cast::<u8>(), bgra.len());
        }
        Some(Self { hbmp, width: width as i32, height: height as i32 })
    }

    /// `hdc` の (x, y) に w × h で描く（アルファで下地と重ねる）。
    unsafe fn draw(&self, hdc: HDC, x: i32, y: i32, w: i32, h: i32) {
        unsafe {
            let mem = CreateCompatibleDC(Some(hdc));
            let old = SelectObject(mem, self.hbmp.into());
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let _ = AlphaBlend(hdc, x, y, w, h, mem, 0, 0, self.width, self.height, blend);
            SelectObject(mem, old);
            let _ = DeleteDC(mem);
        }
    }
}

impl Drop for Bitmap {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.hbmp.into());
        }
    }
}

/// 画像の読み込みスレッドとのつなぎ（`attach_image_loader` で渡される）。
struct ImageLink {
    requests: Sender<ImageRequest>,
    results: Receiver<LoadedImage>,
    /// 今欲しいサムネイル（読み込みスレッドと共有。`ViewState::thumbs` で読み込みを待っている
    /// 項目と同じにそろえる）
    wanted: WantedThumbs,
    /// 今欲しいプレビューの依頼の番号（読み込みスレッドと共有。0 は無し）
    wanted_preview: WantedPreview,
    /// 最後に付けたプレビューの依頼の番号
    last_preview_ticket: Cell<u64>,
}

impl ImageLink {
    /// プレビューの新しい依頼の番号を付け、それを今欲しいものにする（前の依頼は読まずに捨てられる）。
    fn next_preview_ticket(&self) -> u64 {
        let ticket = self.last_preview_ticket.get() + 1;
        self.last_preview_ticket.set(ticket);
        self.wanted_preview.store(ticket, std::sync::atomic::Ordering::SeqCst);
        ticket
    }

    /// 欲しいプレビューを無しにする（待っている依頼は読まずに捨てられる）。
    fn clear_wanted_preview(&self) {
        self.wanted_preview.store(0, std::sync::atomic::Ordering::SeqCst);
    }

    fn with_wanted(&self, f: impl FnOnce(&mut std::collections::HashSet<Uuid>)) {
        if let Ok(mut wanted) = self.wanted.lock() {
            f(&mut wanted);
        }
    }
}

/// 窓が持つ描画用の状態。
struct ViewState {
    /// 一覧の写し。描画はここだけを読む
    rows: Vec<Row>,
    /// ツリー項目の lParam（添字）→ 表示対象
    tree_tokens: Vec<Source>,
    /// 一覧のサムネイル（None は読み込みを頼んだところ）。一覧にある項目の分だけ持ち、
    /// 隠したら全部捨てる
    thumbs: HashMap<Uuid, Option<Bitmap>>,
    /// 画像の依頼の世代。隠したときに進め、古い世代の結果を捨てる
    generation: u64,
    /// プレビューで画像を待っている項目
    preview_wanted: Option<Uuid>,
    /// プレビューの画像（プレビュー欄の大きさに収めたもの）
    preview_image: Option<Bitmap>,
    /// プレビューの画像の読み込み元（欄が広がったときに読み直す）
    preview_source: Option<ImageSource>,
    /// プレビューの画像の元の大きさ（届いた結果で分かる。読み直すと大きく出せるかの判定に使う）
    preview_source_size: Option<(u32, u32)>,
}

/// `GWLP_USERDATA` に置く窓のコンテキスト。フォント・アイコンは作成時に作り、破棄時に解放する。
struct WindowCtx {
    handler: Rc<dyn ViewerHandler>,
    view: RefCell<ViewState>,
    /// `set_tree` でツリーを作り直している間（選択の変化をハンドラへ伝えない）
    tree_rebuilding: Cell<bool>,
    /// `set_rows` で一覧を差し替えている間（選択の変化をハンドラへ伝えない）
    rows_updating: Cell<bool>,
    /// 選択の変化を伝える `WM_APP_SELECTION` を送ってあり、まだ処理していない
    selection_posted: Cell<bool>,
    /// 窓が非アクティブになったときにフォーカスがあった子コントロール（Alt+Tab などで戻った
    /// ときに戻す。HWND は `Send` でないので整数で持つ。0 は無し）
    last_focus: Cell<isize>,
    images: RefCell<Option<ImageLink>>,
    /// メニューを表示している（行の右クリックメニューの `TrackPopupMenu`、メニューバー・窓メニューの
    /// モーダルループの中）。この間に隠す・終了の要求が来たら `EndMenu` で閉じる（`cancel_modal`）。
    /// 窓の破棄はこの間に起こさない
    menu_open: Cell<bool>,
    /// 表示中のダイアログ（確認・バージョン情報の TaskDialog、名前の変更の入力ダイアログの窓。0 は無し）。
    /// モーダルループの中なので、隠す・終了の要求が来たら「キャンセル」で閉じる（`cancel_modal`。閉じ方は
    /// `dialog_kind` で変える）。窓の破棄はこの間に起こさない
    dialog: Cell<isize>,
    /// 表示中（または開きかけ）のダイアログの種類。開く前に立てる
    dialog_kind: Cell<DialogKind>,
    /// TaskDialog を呼んでから `TDN_CREATED`（`dialog` が立つ）までの間。この間もモーダルの表示中として扱う
    /// （`modal_is_open`。終了の要求の後の閉じるやり直しのタイマーを張らせる）
    dialog_opening: Cell<bool>,
    /// 開きかけの TaskDialog に閉じる要求が来た印。`TDN_CREATED` で立っていれば、すぐにキャンセルで閉じる
    dialog_cancel: Cell<bool>,
    /// DPI に合わせたフォントと寸法（`WM_DPICHANGED` で差し替える。借用は描画・配置の間だけ）
    metrics: RefCell<Metrics>,
    /// 一覧の種別アイコン（`EntryKind` の順）と、ピン留めのフォルダの行のアイコン（最後）
    icons: [HICON; 5],
    /// 開くフォルダの行（`WM_APP_OPEN_FOLDER` を処理するときにツリーで選ぶ。`activate_row`）
    open_folder: Cell<Option<Uuid>>,
    /// Ctrl+F などのキー操作（メインのメッセージループが `translate_accelerator` で使う）
    accel: HACCEL,
    /// 最小化・最大化していないときのクライアント領域の大きさ（96 DPI 基準の px）。隠すとき・終了時に
    /// 設定へ保存する（`normal_size`）
    normal_size: Cell<(u32, u32)>,
    /// ツリーの「履歴」の項目（件数の表示を書き換える。0 は無し）と、表示している件数
    history_item: Cell<isize>,
    history_count: Cell<usize>,
    /// 境目をドラッグして決めたツリーの幅・プレビューの高さ（96 DPI 基準）。None の間は既定の比率の
    /// 配置（`compute_layout`）。保存はしない
    tree_width: Cell<Option<u32>>,
    preview_height: Cell<Option<u32>>,
    /// ドラッグ中の境目（マウスを捕まえている間）
    drag: Cell<Option<Drag>>,
    /// ドラッグ中の一覧の行（マウスを捕まえている間。`begin_row_drag`）。描画は落とす先の目印をここから読む
    row_drag: Cell<Option<RowDrag>>,
    /// 行のドラッグを右ボタンで取り消し、右ボタンを離すのを待っている（その間もマウスを捕まえておく。
    /// `cancel_row_drag_by_right_button`）
    row_drag_right_up: Cell<bool>,
    /// ドラッグ中の行の絵の小窓（`drag_image`。HWND は `Send` でないので整数で持つ。0 は無し）
    drag_image: Cell<isize>,
    /// 窓に付けているメニューバー（ドロップダウンは自前描画。`menu_draw::MenuBar` の説明）。窓の破棄の前に
    /// 外す（`ViewerWindow` の `Drop`）。借用はメニューバーを差し替える間だけ
    menu_bar: RefCell<Option<MenuBar>>,
    /// 窓の DPI が変わったが、メニュー・確認ダイアログの表示中なので、メニューバーの作り直しを保留している
    /// （閉じた後の起床で行う）
    menu_bar_pending: Cell<bool>,
    /// 最前面に表示しているか（メニューバーを作り直すときにチェックを引き継ぐ）
    topmost: Cell<bool>,
    /// ツリーの右クリックメニューの間だけ強調している項目（`show_tree_menu`）。メニューの間にツリーが
    /// 作り直されたら、`set_tree` が新しい項目へ強調を付け直す
    tree_hilite: Cell<Option<Source>>,
    /// ツリーで右ボタンを押している間の、押した項目の表示元（項目の上でなければ内の None）。
    /// 離したとき・マウスを失ったときに下ろす（`tree_subclass_proc`）
    tree_rpress: Cell<Option<Option<Source>>>,
    /// ツリーの名前の編集の状態（`begin_edit`・`finish_edit`・`complete_edit`）
    edit: Cell<EditState>,
    /// 編集を始めている途中（`Starting`）に隠す・終了の要求が来た印。`begin_edit` の最初に下ろし、
    /// `begin_edit` から戻るまで保持する（`TVN_BEGINLABELEDIT` も見る）
    edit_cancel: Cell<bool>,
    /// 編集を始めたときに選んでいた表示元（後始末で選び直す候補）
    edit_restore: Cell<Option<Source>>,
    /// 編集の間にユーザーがマウス・キーボードで選んだ表示元（編集を終わらせたクリックなど。後始末で採る）
    edit_user_choice: Cell<Option<Source>>,
    /// 作成の仮の項目（`HTREEITEM` の値。0 は無し）
    edit_temp: Cell<isize>,
    /// 編集の結果（対象と、確定なら入力した文字）。`finish_edit` が置き、後始末が取り出す
    edit_done: RefCell<Option<(EditTarget, Option<String>)>>,
    /// 編集の間に保留したツリーの構成と、アプリが選ぶように指定した表示元（最後のものだけ）
    tree_pending: RefCell<Option<(Vec<TreeNode>, Source)>>,
    /// 終了の後片付け中（`begin_teardown`）。送られたメッセージが来ても、アプリの処理（ハンドラ）を呼ばない
    teardown: Cell<bool>,
    /// プレビューの EDIT に「（プレビューなし）」を出している（薄い文字で描く。`WM_CTLCOLORSTATIC`）
    preview_none: Cell<bool>,
}

impl WindowCtx {
    fn icon(&self, kind: EntryKind) -> HICON {
        self.icons[match kind {
            EntryKind::Text => 0,
            EntryKind::Image => 1,
            EntryKind::File => 2,
            EntryKind::Other => 3,
        }]
    }

    /// 一覧の行のアイコン（フォルダの行はフォルダ、ほかは種別）。
    fn row_icon(&self, row: &Row) -> HICON {
        if row.folder.is_some() { self.icons[4] } else { self.icon(row.kind) }
    }

    /// 画像の読み込みを頼む（読み込みスレッドがまだ無ければ何もしない）。プレビューの依頼は新しい番号を
    /// 付け、それより前のプレビューの依頼を読み込みスレッドに捨てさせる。
    fn request_image(&self, id: Uuid, purpose: Purpose, source: ImageSource, max_width: u32, max_height: u32) {
        let generation = self.view.borrow().generation;
        if let Some(link) = self.images.borrow().as_ref() {
            let preview_ticket = if purpose == Purpose::Preview { link.next_preview_ticket() } else { 0 };
            let _ = link.requests.send(ImageRequest { generation, id, purpose, source, max_width, max_height, preview_ticket });
        }
    }
}

/// ビューア窓。Drop で窓を破棄する（メッセージループを抜けた後に破棄すること）。
pub struct ViewerWindow {
    hwnd: HWND,
}

impl ViewerWindow {
    /// 非表示のビューア窓と子コントロールを作る。`logical_size` はクライアント領域の大きさ
    /// （96 DPI 基準の px。設定の `viewer_width`・`viewer_height` と同じく、枠とメニューバーを除いた大きさ）。
    pub fn create(title: &str, logical_size: (u32, u32), handler: Rc<dyn ViewerHandler>) -> WinResult<Self> {
        init_common_controls();
        register_class()?;
        let mut menu = create_menu_bar(unsafe { GetDpiForSystem() })?;
        let (width, height) = window_size_for_client(logical_size, unsafe { GetDpiForSystem() });
        let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        let created = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                CLASS_NAME,
                PCWSTR(title.as_ptr()),
                VIEWER_STYLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                width,
                height,
                None,
                Some(menu.handle()),
                Some(GetModuleHandleW(None)?.into()),
                None,
            )
        };
        // 窓に付かなかったメニューは、`menu` の破棄が破棄する（窓に付いた後は、窓の破棄の前に外す。`Drop`）
        let hwnd = created?;
        menu.mark_attached();
        // コンテキストは作成中のメッセージ（WM_SIZE など）では使わないため、作成に成功してから渡す。
        // 以降は Drop（DestroyWindow → WM_NCDESTROY）で解放される
        let window = Self { hwnd };
        // 窓が出るモニターの DPI（PerMonitorV2 では窓ごとに異なりうる）に合わせる
        let metrics = Metrics::new(hwnd, unsafe { GetDpiForWindow(hwnd) });
        let font = metrics.font;
        let ctx = Box::new(WindowCtx {
            handler,
            view: RefCell::new(ViewState {
                rows: Vec::new(),
                tree_tokens: Vec::new(),
                thumbs: HashMap::new(),
                generation: 0,
                preview_wanted: None,
                preview_image: None,
                preview_source: None,
                preview_source_size: None,
            }),
            tree_rebuilding: Cell::new(false),
            rows_updating: Cell::new(false),
            selection_posted: Cell::new(false),
            last_focus: Cell::new(0),
            images: RefCell::new(None),
            menu_open: Cell::new(false),
            dialog: Cell::new(0),
            dialog_kind: Cell::new(DialogKind::Task),
            dialog_opening: Cell::new(false),
            dialog_cancel: Cell::new(false),
            metrics: RefCell::new(metrics),
            icons: create_icons(),
            open_folder: Cell::new(None),
            accel: create_accelerators(),
            normal_size: Cell::new(logical_size),
            history_item: Cell::new(0),
            history_count: Cell::new(0),
            tree_width: Cell::new(None),
            preview_height: Cell::new(None),
            drag: Cell::new(None),
            row_drag: Cell::new(None),
            row_drag_right_up: Cell::new(false),
            drag_image: Cell::new(0),
            menu_bar: RefCell::new(Some(menu)),
            menu_bar_pending: Cell::new(false),
            topmost: Cell::new(false),
            tree_hilite: Cell::new(None),
            tree_rpress: Cell::new(None),
            edit: Cell::new(EditState::None),
            edit_cancel: Cell::new(false),
            edit_restore: Cell::new(None),
            edit_user_choice: Cell::new(None),
            edit_temp: Cell::new(0),
            edit_done: RefCell::new(None),
            tree_pending: RefCell::new(None),
            teardown: Cell::new(false),
            preview_none: Cell::new(false),
        });
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(ctx) as isize);
        }
        // 一覧は作成時に WM_MEASUREITEM で行の高さを問い合わせるため、コンテキストの後に作る
        create_children(hwnd, font)?;
        // 行の高さは、後から変えられるよう空のイメージリストの高さでも指定する（apply_dpi）
        set_row_height(hwnd);
        set_tree_images(hwnd);
        set_window_icons(hwnd);
        // 窓が出るモニターの DPI がシステムの DPI と違えば、メニューバーをその DPI で作り直し、大きさを合わせ直す
        if let Some(ctx) = unsafe { ctx_ref(hwnd) } {
            let bar_dpi = ctx.menu_bar.borrow().as_ref().map(MenuBar::dpi);
            if bar_dpi != Some(unsafe { GetDpiForWindow(hwnd) }) {
                rebuild_menu_bar(hwnd, ctx, unsafe { GetDpiForWindow(hwnd) });
            }
        }
        let (width, height) = window_size_for_client(logical_size, unsafe { GetDpiForWindow(hwnd) });
        unsafe {
            let _ = SetWindowPos(hwnd, None, 0, 0, width, height, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
        layout(hwnd);
        Ok(window)
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    pub fn waker(&self) -> Waker {
        Waker(self.hwnd.0 as isize)
    }

    /// 画像の読み込みスレッドが結果を送った後に呼ぶ通知（`images::spawn` に渡す）。
    pub fn image_notifier(&self) -> impl Fn() + Send + 'static {
        let hwnd = self.hwnd.0 as isize;
        move || unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_APP_IMAGE, WPARAM(0), LPARAM(0));
        }
    }
}

/// 画像の読み込みスレッドとつなぐ。依頼の送り手は窓が持ち、窓の破棄で手放す
/// （読み込みスレッドはそれで終わる）。`wanted` は読み込みスレッドに渡したものと同じもの。
pub fn attach_image_loader(
    hwnd: HWND,
    requests: Sender<ImageRequest>,
    results: Receiver<LoadedImage>,
    wanted: WantedThumbs,
    wanted_preview: WantedPreview,
) {
    if let Some(ctx) = unsafe { ctx_ref(hwnd) } {
        *ctx.images.borrow_mut() =
            Some(ImageLink { requests, results, wanted, wanted_preview, last_preview_ticket: Cell::new(0) });
    }
}

/// プレビューに画像を出す。EDIT を隠して画像の子窓を出し、プレビュー欄の大きさに収めた画像の
/// 読み込みを頼む（届くまでは空）。
pub fn show_preview_image(hwnd: HWND, id: Uuid, source: ImageSource) {
    let Some(ctx) = (unsafe { ctx_ref(hwnd) }) else {
        return;
    };
    let (Ok(edit), Ok(image)) = (unsafe { GetDlgItem(Some(hwnd), ID_PREVIEW) }, unsafe { GetDlgItem(Some(hwnd), ID_IMAGE) })
    else {
        return;
    };
    ctx.preview_none.set(false);
    {
        let mut view = ctx.view.borrow_mut();
        view.preview_wanted = Some(id);
        view.preview_image = None;
        view.preview_source = Some(source.clone());
        view.preview_source_size = None;
    }
    let mut rc = RECT::default();
    unsafe {
        let _ = SetWindowTextW(edit, w!(""));
        let _ = ShowWindow(edit, SW_HIDE);
        let _ = ShowWindow(image, SW_SHOW);
        let _ = InvalidateRect(Some(image), None, true);
        let _ = GetClientRect(image, &mut rc);
    }
    ctx.request_image(id, Purpose::Preview, source, rc.right.max(1) as u32, rc.bottom.max(1) as u32);
}

impl Drop for ViewerWindow {
    fn drop(&mut self) {
        unsafe {
            // メニューバーを外してから破棄し、項目のデータを解放する（窓の破棄とデータの解放の順番に頼らない）。
            // 外せなかったときは窓がメニューを破棄するので、項目のデータは解放せずに残す
            if let Some(ctx) = ctx_ref(self.hwnd) {
                // 借用は取り出すだけで終える（`SetMenu` がメッセージを送る間に借用を持たない）
                let bar = ctx.menu_bar.borrow_mut().take();
                if let Some(bar) = bar {
                    if SetMenu(self.hwnd, None).is_ok() {
                        drop(bar.detached());
                    } else {
                        std::mem::forget(bar);
                    }
                }
            }
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// 窓を表示して前面へ出す（最小化中なら元に戻す）。キーボードのフォーカスは一覧に置く。
pub fn show(hwnd: HWND) {
    unsafe {
        let cmd = if IsIconic(hwnd).as_bool() { SW_RESTORE } else { SW_SHOW };
        let _ = ShowWindow(hwnd, cmd);
        let _ = SetForegroundWindow(hwnd);
    }
    focus_list(hwnd);
}

/// 窓を隠す（閉じる = トレイへ隠す）。メニュー・確認ダイアログを表示していれば先に閉じる。
pub fn hide(hwnd: HWND) {
    cancel_modal(hwnd);
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

/// メニュー・確認ダイアログを表示しているか（その間は失敗の通知を出さずに保留する。閉じると窓は
/// 自分を起こすので、そのときに出る）。
pub fn modal_is_open(hwnd: HWND) -> bool {
    unsafe { ctx_ref(hwnd) }.is_some_and(|ctx| ctx.menu_open.get() || dialog_active(ctx))
}

/// TaskDialog を表示している、または開きかけ（呼んだが `TDN_CREATED` がまだ）か。
fn dialog_active(ctx: &WindowCtx) -> bool {
    ctx.dialog.get() != 0 || ctx.dialog_opening.get()
}

/// メニュー・確認ダイアログを表示していれば閉じる（隠す・終了の要求のとき）。閉じたメニューで
/// 選ばれた操作は無く（`TrackPopupMenu` は 0 を返す）、確認は「キャンセル」になる。ツリーの名前の
/// 編集中・行のドラッグ中なら取り消す（`cancel_edit`・`cancel_row_drag`）。表示していなければ何もしない。
pub fn cancel_modal(hwnd: HWND) {
    let Some(ctx) = (unsafe { ctx_ref(hwnd) }) else {
        return;
    };
    // ツリーの名前の編集・行のドラッグはモーダルではないが、隠す・終了のときは取り消す
    cancel_edit(hwnd, ctx);
    cancel_row_drag(hwnd, ctx);
    unsafe {
        if ctx.menu_open.get() {
            let _ = EndMenu();
        }
        let dialog = ctx.dialog.get();
        if dialog != 0 {
            cancel_dialog(HWND(dialog as *mut _), ctx.dialog_kind.get());
        } else if ctx.dialog_opening.get() {
            // まだ窓が無い（`TDN_CREATED`・`WM_INITDIALOG` の前）。窓ができたらすぐ閉じるよう印を立てる
            // （`confirm_callback`・`name_dialog_proc`）
            ctx.dialog_cancel.set(true);
        }
    }
}

/// 追跡しているダイアログの種類（`cancel_modal` が閉じる要求の送り方を変える）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DialogKind {
    /// TaskDialog（確認・バージョン情報）。`TDM_CLICK_BUTTON`（IDCANCEL）で閉じる
    Task,
    /// 入力ダイアログ（名前の変更。`DialogBoxParamW`）。`WM_COMMAND`（IDCANCEL）で閉じる
    Input,
}

/// ダイアログを「キャンセル」で閉じる（確認はキャンセル、入力は捨てる）。
unsafe fn cancel_dialog(dialog: HWND, kind: DialogKind) {
    unsafe {
        match kind {
            DialogKind::Task => {
                SendMessageW(dialog, TDM_CLICK_BUTTON.0 as u32, Some(WPARAM(IDCANCEL.0 as usize)), Some(LPARAM(0)));
            }
            DialogKind::Input => {
                SendMessageW(dialog, WM_COMMAND, Some(WPARAM(IDCANCEL.0 as usize)), Some(LPARAM(0)));
            }
        }
    }
}

/// 終了の要求の後（`App::begin_exit` の `cancel_modal` の後）に呼ぶ。まだメニュー・確認ダイアログが
/// 開いていれば、`CLOSE_RETRY_MS` ごとのタイマーで閉じる要求（`cancel_modal`）をやり直す（`EndMenu` が
/// 一度で閉じなかった場合に備える）。閉じたら・後片付けに入ったら止まる。上限は無く、
/// やり直しでも閉じなければ終了は進まない（残る危険）。
pub fn retry_modal_close(hwnd: HWND) {
    if modal_is_open(hwnd) {
        unsafe {
            SetTimer(Some(hwnd), CLOSE_RETRY_TIMER_ID, CLOSE_RETRY_MS, None);
        }
    }
}

/// 終了の後片付けに入る（メインのループを抜け、最後の保存をした後。ホットキー・トレイのスレッドを
/// 止める前に呼ぶ）。以後、送られたメッセージが来ても、アプリの処理（ハンドラ）を呼ばない。`WM_CLOSE` は
/// 何もしない。窓の破棄（`WM_DESTROY`・`WM_NCDESTROY`）はそのまま。既定の処理（活性化など）と子コントロール
/// の処理は走る。やり直しのタイマーと、プレビューの読み直しのタイマーも止める。
pub fn begin_teardown(hwnd: HWND) {
    if let Some(ctx) = unsafe { ctx_ref(hwnd) } {
        ctx.teardown.set(true);
        unsafe {
            let _ = KillTimer(Some(hwnd), CLOSE_RETRY_TIMER_ID);
            let _ = KillTimer(Some(hwnd), PREVIEW_REFIT_TIMER_ID);
        }
    }
}

/// 最前面に表示するかを変え、「ツール」の「最前面に表示」のチェックを合わせる。
pub fn set_topmost(hwnd: HWND, on: bool) {
    if let Some(ctx) = unsafe { ctx_ref(hwnd) } {
        ctx.topmost.set(on);
    }
    unsafe {
        let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
        let _ = SetWindowPos(hwnd, Some(after), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        let check = if on { MF_CHECKED } else { MF_UNCHECKED };
        CheckMenuItem(GetMenu(hwnd), CMD_TOPMOST as u32, (MF_BYCOMMAND | check).0);
    }
}

/// 最小化・最大化していないときのクライアント領域の大きさ（96 DPI 基準の px）。
pub fn normal_size(hwnd: HWND) -> Option<(u32, u32)> {
    unsafe { ctx_ref(hwnd) }.map(|ctx| ctx.normal_size.get())
}

/// ビューア窓（とその子コントロール）宛てのキー操作をアクセラレータ（Ctrl+F）で変換する。
/// 変換したら true（メインのメッセージループは、そのメッセージをほかへ渡さない）。
pub fn translate_accelerator(hwnd: HWND, msg: &MSG) -> bool {
    let Some(ctx) = (unsafe { ctx_ref(hwnd) }) else {
        return false;
    };
    if ctx.accel.is_invalid() || (msg.hwnd != hwnd && !unsafe { IsChild(hwnd, msg.hwnd) }.as_bool()) {
        return false;
    }
    unsafe { TranslateAcceleratorW(hwnd, ctx.accel, msg) != 0 }
}

/// ツリーの「履歴」の件数を変える（変わったときだけ項目の表示名を書き換える）。
pub fn set_history_count(hwnd: HWND, count: usize) {
    let Some(ctx) = (unsafe { ctx_ref(hwnd) }) else {
        return;
    };
    if ctx.history_count.replace(count) == count {
        return;
    }
    let (item, Ok(tree)) = (ctx.history_item.get(), unsafe { GetDlgItem(Some(hwnd), ID_TREE) }) else {
        return;
    };
    if item == 0 {
        return;
    }
    let history = TreeNode { label: HISTORY_LABEL.to_string(), source: Source::History, children: Vec::new() };
    let mut text: Vec<u16> = tree_label(&history, count).encode_utf16().chain(std::iter::once(0)).collect();
    let tv = TVITEMW { mask: TVIF_HANDLE | TVIF_TEXT, hItem: HTREEITEM(item), pszText: PWSTR(text.as_mut_ptr()), ..Default::default() };
    unsafe {
        SendMessageW(tree, TVM_SETITEMW, None, Some(LPARAM(&tv as *const _ as isize)));
    }
}

pub fn is_visible(hwnd: HWND) -> bool {
    unsafe { IsWindowVisible(hwnd).as_bool() }
}

/// 二重起動したときに、先に起動したインスタンスの窓を探し続ける上限と間隔。
const SHOW_REQUEST_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);
const SHOW_REQUEST_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// 二重起動したとき、既存のインスタンスのビューア窓へ表示要求を送る。
/// 前面化は既存のプロセス側で行うため、このプロセス（ユーザーが起動した前面のプロセス）が
/// 前面化の権利を相手に渡しておく。先に起動した側がまだ窓を作っていない（起動の途中）ことが
/// あるので、見つかって投稿できるまで `SHOW_REQUEST_LIMIT` の間やり直す。届けられなければ false。
/// 先に起動した側が終了の途中なら、投稿しても処理されずに消えることがある（残る制限）。
pub fn request_show_existing() -> bool {
    deliver_show_request(
        || unsafe { FindWindowW(CLASS_NAME, None).ok() },
        |hwnd| unsafe {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid != 0 {
                let _ = AllowSetForegroundWindow(pid);
            }
            PostMessageW(Some(hwnd), WM_APP_SHOW_REQUEST, WPARAM(0), LPARAM(0)).is_ok()
        },
        SHOW_REQUEST_LIMIT,
        SHOW_REQUEST_INTERVAL,
    )
}

/// `find` で窓が見つかり `post` で投稿できるまで、`limit` の間 `interval` ごとにやり直す。投稿できたら true。
fn deliver_show_request(
    mut find: impl FnMut() -> Option<HWND>,
    mut post: impl FnMut(HWND) -> bool,
    limit: std::time::Duration,
    interval: std::time::Duration,
) -> bool {
    let deadline = std::time::Instant::now() + limit;
    loop {
        if let Some(hwnd) = find() {
            if post(hwnd) {
                return true;
            }
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(interval);
    }
}

/// 一覧の行を差し替える。前に選んでいた項目は UUID で探して選び直す。見つからないときは、
/// `select_first_if_missing` なら先頭を選ぶ。そうでなければ、前に選んでいた項目が消えた
/// （削除・押し出し）なら同じ位置の行（はみ出すなら最後の行）を選び、何も選んでいなかった
/// なら何も選ばない（Delete を続けて押せる）。
/// 差し替え後に選ばれている項目を返す（この間の選択の変化はハンドラへ伝えない）。
pub fn set_rows(hwnd: HWND, rows: Vec<Row>, select_first_if_missing: bool) -> Option<Uuid> {
    let ctx = unsafe { ctx_ref(hwnd) }?;
    let list = unsafe { GetDlgItem(Some(hwnd), ID_LIST) }.ok()?;
    let previous_index = selected_index(list);
    let previous = previous_index.and_then(|i| ctx.view.borrow().rows.get(i).map(|r| r.id));
    let new_index = match previous.and_then(|id| crate::native::model::index_of(&rows, id)) {
        Some(i) => Some(i),
        None if rows.is_empty() => None,
        None if select_first_if_missing => Some(0),
        None => previous_index.map(|i| i.min(rows.len() - 1)),
    };
    let selected = new_index.map(|i| rows[i].id);
    let count = rows.len();
    {
        // 一覧から消えた項目のサムネイルは捨てる（GDI オブジェクトを増やし続けないため）
        // 読み込みを待っているサムネイルも同じく絞る（読み込みスレッドは一覧から消えた行の
        // 依頼を読まずに捨てる）
        let ids: std::collections::HashSet<Uuid> = rows.iter().map(|r| r.id).collect();
        let mut view = ctx.view.borrow_mut();
        view.thumbs.retain(|id, _| ids.contains(id));
        view.rows = rows;
        if let Some(link) = ctx.images.borrow().as_ref() {
            link.with_wanted(|wanted| wanted.retain(|id| ids.contains(id)));
        }
    }
    // 借用はここで手放す。以降のメッセージは WM_DRAWITEM・WM_NOTIFY を返してくる
    ctx.rows_updating.set(true);
    unsafe {
        SendMessageW(list, LVM_SETITEMCOUNT, Some(WPARAM(count)), Some(LPARAM(LVSICF_NOSCROLL as isize)));
        set_item_state(list, -1, 0);
        if let Some(i) = new_index {
            set_item_state(list, i as i32, LVIS_SELECTED.0 | LVIS_FOCUSED.0);
            SendMessageW(list, LVM_ENSUREVISIBLE, Some(WPARAM(i)), Some(LPARAM(0)));
        }
        let _ = InvalidateRect(Some(list), None, true);
    }
    ctx.rows_updating.set(false);
    selected
}

/// プレビューの EDIT の中身を差し替える（`model::preview_text` で整えた文字列を渡す）。
/// 画像を出していたら、画像の子窓を隠して画像を解放する。
pub fn set_preview_text(hwnd: HWND, text: &str) {
    set_preview(hwnd, text, false);
}

/// プレビューに出すものが無いことを、薄い文字の「（プレビューなし）」で出す。
pub fn set_preview_none(hwnd: HWND) {
    set_preview(hwnd, crate::native::model::NO_PREVIEW, true);
}

fn set_preview(hwnd: HWND, text: &str, none: bool) {
    let Ok(edit) = (unsafe { GetDlgItem(Some(hwnd), ID_PREVIEW) }) else {
        return;
    };
    if let Some(ctx) = unsafe { ctx_ref(hwnd) } {
        // 文字を差し替えると描き直すので、その前に色を決める
        ctx.preview_none.set(none);
        let mut view = ctx.view.borrow_mut();
        view.preview_wanted = None;
        view.preview_image = None;
        view.preview_source = None;
        view.preview_source_size = None;
        drop(view);
        if let Some(link) = ctx.images.borrow().as_ref() {
            link.clear_wanted_preview();
        }
    }
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if let Ok(image) = GetDlgItem(Some(hwnd), ID_IMAGE) {
            let _ = ShowWindow(image, SW_HIDE);
        }
        let _ = ShowWindow(edit, SW_SHOW);
        let _ = SetWindowTextW(edit, PCWSTR(wide.as_ptr()));
    }
}

/// 一覧で選ばれている項目。
pub fn selected_id(hwnd: HWND) -> Option<Uuid> {
    let ctx = unsafe { ctx_ref(hwnd) }?;
    let list = unsafe { GetDlgItem(Some(hwnd), ID_LIST) }.ok()?;
    let index = selected_index(list)?;
    ctx.view.borrow().rows.get(index).map(|r| r.id)
}

/// 一覧に入っている行の ID（テスト用）。
#[cfg(test)]
pub fn row_ids(hwnd: HWND) -> Vec<Uuid> {
    unsafe { ctx_ref(hwnd) }.map(|ctx| ctx.view.borrow().rows.iter().map(|r| r.id).collect()).unwrap_or_default()
}

/// 一覧の行の高さを `Metrics::row_height` にする。一覧（オーナードロー固定）は作成時にしか
/// `WM_MEASUREITEM` を送らないため、その高さの空のイメージリストを小アイコン用に設定して
/// 高さを変える。前のイメージリストはこちらで破棄する（今のものは一覧の破棄時に一覧が破棄する。
/// Microsoft Learn の LVM_SETIMAGELIST の説明による）。
fn set_row_height(hwnd: HWND) {
    let (Some(ctx), Ok(list)) = (unsafe { ctx_ref(hwnd) }, unsafe { GetDlgItem(Some(hwnd), ID_LIST) }) else {
        return;
    };
    let height = ctx.metrics.borrow().row_height;
    unsafe {
        let images = ImageList_Create(1, height, ILC_COLOR32, 0, 0);
        let previous = SendMessageW(list, LVM_SETIMAGELIST, Some(WPARAM(LVSIL_SMALL as usize)), Some(LPARAM(images.0 as isize)));
        if previous.0 != 0 {
            let _ = ImageList_Destroy(Some(HIMAGELIST(previous.0)));
        }
    }
}

/// ツリーに今の `Metrics` のアイコンを設定する（前のイメージリストは `Metrics` の Drop が破棄する）。
fn set_tree_images(hwnd: HWND) {
    let (Some(ctx), Ok(tree)) = (unsafe { ctx_ref(hwnd) }, unsafe { GetDlgItem(Some(hwnd), ID_TREE) }) else {
        return;
    };
    let images = ctx.metrics.borrow().tree_images;
    unsafe {
        SendMessageW(tree, TVM_SETIMAGELIST, Some(WPARAM(TVSIL_NORMAL as usize)), Some(LPARAM(images.0)));
    }
}

/// クライアント領域を `logical`（96 DPI 基準の px）にするための窓の大きさ（`dpi` で換算し、
/// 枠・タイトルバー・1行のメニューバーを足す）。
fn window_size_for_client(logical: (u32, u32), dpi: u32) -> (i32, i32) {
    let scale = |v: u32| crate::menu_tooltip::scale_for_dpi(v as i32, dpi);
    let mut rc = RECT { left: 0, top: 0, right: scale(logical.0), bottom: scale(logical.1) };
    unsafe {
        let _ = AdjustWindowRectExForDpi(&mut rc, VIEWER_STYLE, true, WINDOW_EX_STYLE::default(), dpi);
    }
    (rc.right - rc.left, rc.bottom - rc.top)
}

/// px を 96 DPI 基準へ戻す（四捨五入）。
fn unscale(px: i32, dpi: u32) -> u32 {
    let dpi = dpi.max(1) as i64;
    ((px.max(0) as i64 * 96 + dpi / 2) / dpi) as u32
}

/// メニューバー（ツール・ヘルプ）を作る。バーの並びは標準の描画、ドロップダウンの項目は自前で描く
/// （`dpi` で描く）。1つでも足せなければ Err（作りかけのバーは破棄する。欠けたバーを窓に付けない）。
fn create_menu_bar(dpi: u32) -> WinResult<MenuBar> {
    let mut bar = MenuBar::new(dpi)?;
    let tools = bar.add_dropdown(w!("ツール(&T)"))?;
    bar.append_command(tools, CMD_SETTINGS as usize, "設定(&S)...", MF_STRING)?;
    bar.append_separator(tools)?;
    bar.append_command(tools, CMD_CLEAR_HISTORY as usize, "履歴のクリア(&H)...", MF_STRING)?;
    bar.append_command(tools, CMD_CLEAR_CLIPBOARD as usize, "クリップボードのクリア(&C)", MF_STRING)?;
    bar.append_command(tools, CMD_CHECK_DATA as usize, "データのチェック(&D)...", MF_STRING)?;
    bar.append_separator(tools)?;
    bar.append_command(tools, CMD_TOPMOST as usize, "最前面に表示(&T)", MF_STRING)?;
    let help = bar.add_dropdown(w!("ヘルプ(&H)"))?;
    bar.append_command(help, CMD_ABOUT as usize, "バージョン情報(&A)", MF_STRING)?;
    Ok(bar)
}

/// 窓の DPI（`dpi`）でメニューバーを作り直して付け替える（ドロップダウンの項目の大きさはシステムが初めに
/// 測った値を覚えるため、DPI が変わったら作り直す）。メニュー・確認ダイアログの表示中は保留し、閉じた後の
/// 起床（`WM_APP_WAKE`）で行う（表示中のメニューを差し替え・解放しない）。作るか付け替えるのに失敗したら
/// 今のメニューバーを使い続ける。「最前面に表示」のチェックは引き継ぐ。
fn rebuild_menu_bar(hwnd: HWND, ctx: &WindowCtx, dpi: u32) {
    if ctx.menu_open.get() || dialog_active(ctx) {
        ctx.menu_bar_pending.set(true);
        return;
    }
    ctx.menu_bar_pending.set(false);
    let Ok(mut bar) = create_menu_bar(dpi) else {
        return;
    };
    unsafe {
        if ctx.topmost.get() {
            CheckMenuItem(bar.handle(), CMD_TOPMOST as u32, (MF_BYCOMMAND | MF_CHECKED).0);
        }
        // 失敗したら新しいメニューバーは付いていないので、`bar` の破棄で破棄される
        if SetMenu(hwnd, Some(bar.handle())).is_err() {
            return;
        }
    }
    bar.mark_attached();
    let old = ctx.menu_bar.borrow_mut().replace(bar);
    // 外れた古いメニューバーは、ここで破棄する（`SetMenu` は外したメニューを破棄しない）
    drop(old.map(MenuBar::detached));
    unsafe {
        let _ = DrawMenuBar(hwnd);
    }
}

/// キー操作の表（Ctrl+F、Alt+↑、Alt+↓）。作れなければ無効なハンドル（これらのキーが効かないだけ）。
fn create_accelerators() -> HACCEL {
    let table = [
        ACCEL { fVirt: FVIRTKEY | FCONTROL, key: u16::from(b'F'), cmd: CMD_FIND },
        ACCEL { fVirt: FVIRTKEY | FALT, key: VK_UP.0, cmd: CMD_MOVE_UP },
        ACCEL { fVirt: FVIRTKEY | FALT, key: VK_DOWN.0, cmd: CMD_MOVE_DOWN },
    ];
    unsafe { CreateAcceleratorTableW(&table) }.unwrap_or_default()
}

/// 検索欄へフォーカスを移し、入力済みの文字を全部選ぶ（続けて打てば置き換わる）。
fn focus_search(hwnd: HWND) {
    unsafe {
        if let Ok(search) = GetDlgItem(Some(hwnd), ID_SEARCH) {
            let _ = SetFocus(Some(search));
            SendMessageW(search, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
        }
    }
}

/// 確認ダイアログ（TaskDialog）の通知。表示中のダイアログの窓を `WindowCtx::dialog` に置く
/// （`lpCallbackData` は `WindowCtx` を指す。ダイアログの表示中は窓を破棄しないので有効）。開きかけの間に
/// 閉じる要求が来ていれば（`dialog_cancel`）、窓ができた時点でキャンセルで閉じる。
unsafe extern "system" fn confirm_callback(
    dialog: HWND,
    msg: TASKDIALOG_NOTIFICATIONS,
    _wparam: WPARAM,
    _lparam: LPARAM,
    data: isize,
) -> HRESULT {
    let ctx = unsafe { &*(data as *const WindowCtx) };
    match msg {
        TDN_CREATED => {
            ctx.dialog.set(dialog.0 as isize);
            ctx.dialog_opening.set(false);
            if ctx.dialog_cancel.replace(false) {
                unsafe {
                    SendMessageW(dialog, TDM_CLICK_BUTTON.0 as u32, Some(WPARAM(IDCANCEL.0 as usize)), Some(LPARAM(0)));
                }
            }
        }
        TDN_DESTROYED => ctx.dialog.set(0),
        _ => {}
    }
    S_OK
}

/// 確認ダイアログ・バージョン情報を開いてよいか（メニュー・ほかのダイアログの表示中、ツリーの名前の編集中は開かない。
/// 編集中に開くと、ダイアログの中で編集の後始末が進む）。
fn can_open_dialog(ctx: &WindowCtx) -> bool {
    !ctx.menu_open.get() && !dialog_active(ctx) && ctx.edit.get() == EditState::None
}

/// TaskDialog を出す（`pfCallback`・`lpCallbackData` はここで入れる）。呼んでから `TDN_CREATED` までを開きかけとして
/// 追跡し（`dialog_opening`。その間の閉じる要求は `dialog_cancel` に残り、窓ができたら閉じる）、閉じた後に追跡を解いて
/// 自分を起こす（表示中に保留した処理を行わせる）。
fn run_task_dialog(hwnd: HWND, ctx: &WindowCtx, mut config: TASKDIALOGCONFIG, pressed: Option<&mut i32>) -> WinResult<()> {
    config.pfCallback = Some(confirm_callback);
    config.lpCallbackData = ctx as *const WindowCtx as isize;
    ctx.dialog_kind.set(DialogKind::Task);
    ctx.dialog_cancel.set(false);
    ctx.dialog_opening.set(true);
    let result = unsafe { TaskDialogIndirect(&config, pressed.map(|p| p as *mut i32), None, None) };
    ctx.dialog_opening.set(false);
    ctx.dialog_cancel.set(false);
    ctx.dialog.set(0);
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_APP_WAKE, WPARAM(0), LPARAM(0));
    }
    result
}

/// 履歴のクリアの確認（「削除」「キャンセル」。既定はキャンセル）。「削除」を
/// 選んだときだけ true。表示中に隠す・終了の要求が来たら `cancel_modal` がキャンセルで閉じる。
/// メニュー・ほかのダイアログの表示中は出さない（false）。
fn confirm_clear_history(hwnd: HWND, ctx: &WindowCtx) -> bool {
    confirm_delete(hwnd, ctx, "履歴のクリア", "すべての履歴を削除します。よろしいですか？")
}

/// 削除の確認（「削除」「キャンセル」。既定はキャンセル）。「削除」を選んだときだけ true。表示中の
/// 扱いは `confirm_clear_history` と同じ（ツリーのフォルダの削除も使う）。
fn confirm_delete(hwnd: HWND, ctx: &WindowCtx, title: &str, content: &str) -> bool {
    if !can_open_dialog(ctx) {
        return false;
    }
    let title = HSTRING::from(title);
    let content = HSTRING::from(content);
    let buttons = [TASKDIALOG_BUTTON { nButtonID: CONFIRM_DELETE, pszButtonText: w!("削除") }];
    let config = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: hwnd,
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION | TDF_POSITION_RELATIVE_TO_WINDOW,
        dwCommonButtons: TDCBF_CANCEL_BUTTON,
        pszWindowTitle: PCWSTR(title.as_ptr()),
        Anonymous1: TASKDIALOGCONFIG_0 { pszMainIcon: TD_WARNING_ICON },
        pszContent: PCWSTR(content.as_ptr()),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: IDCANCEL.0,
        ..Default::default()
    };
    let mut pressed = 0i32;
    let result = run_task_dialog(hwnd, ctx, config, Some(&mut pressed));
    if let Err(e) = &result {
        eprintln!("{title}の確認を出せませんでした: {e}");
    }
    result.is_ok() && pressed == CONFIRM_DELETE
}

/// バージョン情報。「閉じる」・×・Esc で閉じる。
/// 表示中の扱い（`dialog` の追跡、隠す・終了の要求での `cancel_modal`）は確認ダイアログと同じ。
/// メニュー・ほかのダイアログの表示中は出さない。
fn show_about(hwnd: HWND, ctx: &WindowCtx) {
    if !can_open_dialog(ctx) {
        return;
    }
    let name = HSTRING::from(crate::APP_DISPLAY_NAME);
    let content = HSTRING::from(about_content());
    let config = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: hwnd,
        // アイコンは exe のリソースのアプリのアイコン（`res/app.rc` の ID 1）を、TaskDialog が DPI に合わせて読む
        hInstance: unsafe { GetModuleHandleW(None) }.map(Into::into).unwrap_or_default(),
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION | TDF_POSITION_RELATIVE_TO_WINDOW,
        dwCommonButtons: TDCBF_CLOSE_BUTTON,
        pszWindowTitle: w!("バージョン情報"),
        Anonymous1: TASKDIALOGCONFIG_0 { pszMainIcon: PCWSTR(1 as *const u16) },
        pszMainInstruction: PCWSTR(name.as_ptr()),
        pszContent: PCWSTR(content.as_ptr()),
        // 1行では TaskDialog の幅に入らず途中で折り返すので、区切りのよいところで2行に分ける（実機で確認）
        pszFooter: w!("アイコン: Material Symbols (Sharp)\nCopyright Google LLC (Apache License 2.0)"),
        ..Default::default()
    };
    let result = run_task_dialog(hwnd, ctx, config, None);
    if let Err(e) = &result {
        eprintln!("バージョン情報を出せませんでした: {e}");
    }
}

/// データのチェックの結果。見つかったものがあれば「削除」「キャンセル」（既定はキャンセル）で聞き、
/// 「削除」を選んだときだけ true。何も無ければ知らせるだけ。表示中の扱いは確認ダイアログと同じ（`dialog` の追跡、
/// 隠す・終了の要求での `cancel_modal`）。出せないとき（メニュー・ほかのダイアログの表示中など）は false。
pub fn show_data_report(hwnd: HWND, report: &DataReport) -> bool {
    let Some(ctx) = (unsafe { ctx_ref(hwnd) }) else {
        return false;
    };
    if !can_open_dialog(ctx) {
        eprintln!("データのチェックの結果を出せませんでした（ほかの表示中）");
        return false;
    }
    let (content, details) = data_report_text(report);
    let content = HSTRING::from(content);
    let details = HSTRING::from(details);
    let buttons = [TASKDIALOG_BUTTON { nButtonID: CONFIRM_DELETE, pszButtonText: w!("削除") }];
    let clean = report.is_clean();
    let config = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: hwnd,
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION | TDF_POSITION_RELATIVE_TO_WINDOW,
        dwCommonButtons: if clean { TDCBF_CLOSE_BUTTON } else { TDCBF_CANCEL_BUTTON },
        pszWindowTitle: w!("データのチェック"),
        Anonymous1: TASKDIALOGCONFIG_0 { pszMainIcon: if clean { TD_INFORMATION_ICON } else { TD_WARNING_ICON } },
        pszContent: PCWSTR(content.as_ptr()),
        pszExpandedInformation: if details.is_empty() { PCWSTR::null() } else { PCWSTR(details.as_ptr()) },
        cButtons: if clean { 0 } else { buttons.len() as u32 },
        pButtons: if clean { std::ptr::null() } else { buttons.as_ptr() },
        nDefaultButton: if clean { IDCLOSE.0 } else { IDCANCEL.0 },
        ..Default::default()
    };
    let mut pressed = 0i32;
    let result = run_task_dialog(hwnd, ctx, config, Some(&mut pressed));
    if let Err(e) = &result {
        eprintln!("データのチェックの結果を出せませんでした: {e}");
    }
    !clean && result.is_ok() && pressed == CONFIRM_DELETE
}

/// データの削除の結果。表示中の扱いは `show_data_report` と同じ。
pub fn show_clean_result(hwnd: HWND, result: &CleanResult) {
    let Some(ctx) = (unsafe { ctx_ref(hwnd) }) else {
        return;
    };
    if !can_open_dialog(ctx) {
        eprintln!("データの削除の結果を出せませんでした（ほかの表示中）");
        return;
    }
    let (content, details) = clean_result_text(result);
    let content = HSTRING::from(content);
    let details = HSTRING::from(details);
    let warn = result.interrupted || result.index_error.is_some() || !result.failures.is_empty();
    let config = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: hwnd,
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION | TDF_POSITION_RELATIVE_TO_WINDOW,
        dwCommonButtons: TDCBF_CLOSE_BUTTON,
        pszWindowTitle: w!("データのチェック"),
        Anonymous1: TASKDIALOGCONFIG_0 { pszMainIcon: if warn { TD_WARNING_ICON } else { TD_INFORMATION_ICON } },
        pszContent: PCWSTR(content.as_ptr()),
        pszExpandedInformation: if details.is_empty() { PCWSTR::null() } else { PCWSTR(details.as_ptr()) },
        ..Default::default()
    };
    if let Err(e) = run_task_dialog(hwnd, ctx, config, None) {
        eprintln!("データの削除の結果を出せませんでした: {e}");
    }
}

/// 結果の「詳細」に並べる名前の数（種類ごと）。
const REPORT_LIST_MAX: usize = 20;

/// 大きさの表示（1024 未満はバイト、以上は KB・MB）。
fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    match bytes {
        0..1024 => format!("{bytes} バイト"),
        1024..1_048_576 => format!("{:.1} KB", bytes as f64 / KB),
        _ => format!("{:.1} MB", bytes as f64 / (KB * KB)),
    }
}

/// 名前の一覧（先頭 `REPORT_LIST_MAX` 件と、残りの数）。名前は出す分だけ作る（`names` は遅延の写像で渡す。
/// 件数が多くてもメインスレッドで全件の文字列を作らない）。
fn list_names(title: &str, names: impl ExactSizeIterator<Item = String>) -> String {
    let total = names.len();
    let mut text = format!("{title}:\n");
    for name in names.take(REPORT_LIST_MAX) {
        text.push_str(&format!("  {name}\n"));
    }
    if total > REPORT_LIST_MAX {
        text.push_str(&format!("  ほか {} 件\n", total - REPORT_LIST_MAX));
    }
    text
}

/// 大きさの合計（上限で止める。極端な値でも debug でパニックしない）。
fn total_size(sizes: impl Iterator<Item = u64>) -> u64 {
    sizes.fold(0, u64::saturating_add)
}

/// チェックの結果の本文と「詳細」（名前の一覧）。
fn data_report_text(report: &DataReport) -> (String, String) {
    let ignored = if report.ignored.is_empty() {
        String::new()
    } else {
        format!("\n対象外のファイル: {} 件（CLCLR のファイルの形でないため、消しません）", report.ignored.len())
    };
    let mut details = Vec::new();
    if report.is_clean() {
        if !report.ignored.is_empty() {
            details.push(list_names("対象外のファイル", report.ignored.iter().cloned()));
        }
        let content = format!(
            "ファイルの有無と参照を調べ、問題は見つかりませんでした（ファイルの中身の破損は調べていません）。{ignored}"
        );
        return (content, details.join("\n"));
    }
    let size = |sizes: &mut dyn Iterator<Item = u64>| format_size(total_size(sizes));
    let history = report.missing.iter().filter(|m| !m.pinned).count();
    let content = format!(
        "次のものが見つかりました。削除しますか？\n\n\
         参照されないファイル: {} 件（{}）\n\
         データが欠けた項目: 履歴 {} 件・ピン留め {} 件（一覧から消します）\n\
         書き込みの途中で残った一時ファイル: {} 件（{}）{ignored}\n\n\
         （ファイルの有無と参照だけを調べました。ファイルの中身の破損は調べていません）",
        report.orphans.len(),
        size(&mut report.orphans.iter().map(|f| f.size)),
        history,
        report.missing.len() - history,
        report.temps.len(),
        size(&mut report.temps.iter().map(|t| t.size)),
    );
    if !report.orphans.is_empty() {
        details.push(list_names("参照されないファイル", report.orphans.iter().map(|f| format!("blobs\\{}", f.name))));
    }
    if !report.missing.is_empty() {
        let names = report.missing.iter().map(|m| format!("{}: {}", if m.pinned { "ピン留め" } else { "履歴" }, m.label));
        details.push(list_names("データが欠けた項目", names));
    }
    if !report.temps.is_empty() {
        let names = report.temps.iter().map(|t| match t.place {
            TempPlace::Blobs => format!("blobs\\{}", t.name),
            TempPlace::DataDir => t.name.clone(),
        });
        details.push(list_names("一時ファイル", names));
    }
    if !report.ignored.is_empty() {
        details.push(list_names("対象外のファイル", report.ignored.iter().cloned()));
    }
    (content, details.join("\n"))
}

/// 削除の結果の本文と「詳細」（消せなかったもの）。
fn clean_result_text(result: &CleanResult) -> (String, String) {
    let mut content = format!(
        "削除しました。\n\nファイル: {} 件（{}）\n一覧から消した項目: {} 件",
        result.files_removed,
        format_size(result.bytes_removed),
        result.items_removed
    );
    if result.interrupted {
        content.push_str("\n\n終了するため、途中でやめました。");
    }
    if let Some(e) = &result.index_error {
        content.push_str(&format!("\n\n履歴のファイルを書き直せませんでした（あとで書き直します）: {e}"));
    }
    if !result.failures.is_empty() {
        content.push_str(&format!("\n\n消せなかったものが {} 件あります（詳細）。", result.failures.len()));
    }
    let details =
        if result.failures.is_empty() { String::new() } else { list_names("消せなかったもの", result.failures.iter().cloned()) };
    (content, details)
}

/// ダイアログの `DWLP_USER`（`DWLP_MSGRESULT`・`DWLP_DLGPROC` の後。どちらもポインタの大きさ。設定画面と同じ）。
const DWLP_USER: WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX((2 * std::mem::size_of::<isize>()) as i32);

/// 名前の変更の入力の上限（UTF-16 の単位。打ち込み・貼り付けの上限で、初めに入れた今の名前は縮めない）。
/// メニューに出すのは先頭の 50 文字まで。
const NAME_MAX_LEN: usize = 256;

/// 名前の変更のダイアログの状態（`DialogBoxParamW` の `dwInitParam` で渡し、`DWLP_USER` に置く。モーダルの
/// 呼び出しが戻るまで生きている）。
struct NameDialog<'a> {
    ctx: &'a WindowCtx,
    initial: &'a str,
    /// 「OK」で閉じたときの入力
    result: Option<String>,
}

/// ピン留めの行の名前の変更: 今の名前を入れたダイアログを出し、「OK」なら入力をハンドラへ伝える。
/// 履歴の行・メニューやほかのダイアログの表示中・ツリーの名前の編集中・行のドラッグ中は何もしない。今の名前は
/// 出す直前に取り直し、アイテムが無くなっていれば出さない。フォルダの行は、フォルダの名前を聞き、前後の空白を
/// 除いて空でなければ `on_tree_command` で伝える（空なら何もしない。ツリーの名前の編集と同じ）。
fn rename_pinned_row(hwnd: HWND, ctx: &WindowCtx, target: RowTarget) {
    if !target.pinned || !can_open_dialog(ctx) || row_drag_active(ctx) {
        return;
    }
    let handler = Rc::clone(&ctx.handler);
    if target.folder {
        let Some(TreeMenu::PinnedFolder { title, .. }) = handler.tree_menu(Source::Pinned(Some(target.id))) else {
            return;
        };
        if let Some(name) = ask_name(hwnd, ctx, &title) {
            let title = name.trim().to_string();
            if !title.is_empty() {
                handler.on_tree_command(hwnd, TreeCommand::RenameFolder { id: target.id, title });
            }
        }
        return;
    }
    let Some(current) = handler.pinned_title(target.id) else {
        return;
    };
    if let Some(name) = ask_name(hwnd, ctx, current.as_deref().unwrap_or("")) {
        handler.on_rename_pinned(hwnd, target.id, name);
    }
}

/// 名前を聞くダイアログ（`IDD_RENAME`）を出し、「OK」で閉じたら入力を返す。表示中の追跡は TaskDialog と同じ
/// （`run_task_dialog`）: 呼んでから `WM_INITDIALOG` までを開きかけとし、閉じる要求はキャンセルにする
/// （`cancel_modal`）。閉じた後に追跡を解いて自分を起こす。
fn ask_name(hwnd: HWND, ctx: &WindowCtx, initial: &str) -> Option<String> {
    let mut state = NameDialog { ctx, initial, result: None };
    ctx.dialog_kind.set(DialogKind::Input);
    ctx.dialog_cancel.set(false);
    ctx.dialog_opening.set(true);
    let pressed = unsafe {
        DialogBoxParamW(
            GetModuleHandleW(None).ok().map(Into::into),
            PCWSTR(crate::native::settings::IDD_RENAME as usize as *const u16),
            Some(hwnd),
            Some(name_dialog_proc),
            LPARAM(&mut state as *mut NameDialog as isize),
        )
    };
    ctx.dialog_opening.set(false);
    ctx.dialog_cancel.set(false);
    ctx.dialog.set(0);
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_APP_WAKE, WPARAM(0), LPARAM(0));
    }
    if pressed <= 0 {
        if pressed == -1 {
            eprintln!("名前の変更のダイアログを出せませんでした: {}", windows::core::Error::from_thread());
        }
        return None;
    }
    // キャンセル（閉じる要求を含む）で閉じたときは入力を使わない
    if pressed == IDOK.0 as isize { state.result } else { None }
}

/// 名前の変更のダイアログのプロシージャ。窓ができたら `WindowCtx::dialog` に置き（開きかけの間に閉じる要求が
/// 来ていれば、すぐキャンセルで閉じる）、破棄で外す。
unsafe extern "system" fn name_dialog_proc(dialog: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> isize {
    unsafe {
        let state = || (GetWindowLongPtrW(dialog, DWLP_USER) as *mut NameDialog).as_mut();
        match msg {
            WM_INITDIALOG => {
                SetWindowLongPtrW(dialog, DWLP_USER, lparam.0);
                let Some(state) = state() else {
                    return 1;
                };
                let ctx = state.ctx;
                ctx.dialog.set(dialog.0 as isize);
                ctx.dialog_opening.set(false);
                if let Ok(edit) = GetDlgItem(Some(dialog), crate::native::settings::IDC_RENAME_NAME) {
                    SendMessageW(edit, EM_LIMITTEXT, Some(WPARAM(NAME_MAX_LEN)), Some(LPARAM(0)));
                    let _ = SetWindowTextW(edit, &HSTRING::from(state.initial));
                    SendMessageW(edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
                }
                if ctx.dialog_cancel.replace(false) {
                    let _ = EndDialog(dialog, IDCANCEL.0 as isize);
                }
                // 既定のフォーカス（最初のタブ止まり＝入力欄）
                1
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                if id == IDOK.0 {
                    if let (Some(state), Ok(edit)) = (state(), GetDlgItem(Some(dialog), crate::native::settings::IDC_RENAME_NAME)) {
                        // 長さを測ってから全部読む（`EM_LIMITTEXT` は初めに入れた今の名前を縮めないので、上限より
                        // 長い今の名前を、変えずに OK しただけで切り詰めない）
                        let mut buf = vec![0u16; GetWindowTextLengthW(edit).max(0) as usize + 1];
                        let len = GetWindowTextW(edit, &mut buf).max(0) as usize;
                        state.result = Some(String::from_utf16_lossy(&buf[..len]));
                    }
                    let _ = EndDialog(dialog, IDOK.0 as isize);
                    1
                } else if id == IDCANCEL.0 {
                    let _ = EndDialog(dialog, IDCANCEL.0 as isize);
                    1
                } else {
                    0
                }
            }
            WM_DESTROY => {
                if let Some(state) = state() {
                    state.ctx.dialog.set(0);
                }
                0
            }
            _ => 0,
        }
    }
}

/// バージョン情報の本文（名前は見出しに出す）。
fn about_content() -> String {
    format!(
        "バージョン {}\nWindows クリップボード履歴マネージャ\n\nCopyright (c) 2026 tsZ\nMIT License\nBased on CLCL by Ohno Tomoaki (nakkag/CLCL)",
        env!("CARGO_PKG_VERSION")
    )
}

/// 窓の DPI が変わったとき（別のモニターへの移動、表示倍率の変更）に、フォント・寸法を
/// 作り直して子コントロールへ反映する。サムネイルとプレビューの画像は大きさが変わるので捨てる
/// （ハンドラの `on_metrics_changed` でプレビューを出し直す）。
fn apply_dpi(hwnd: HWND, ctx: &WindowCtx, dpi: u32) {
    let metrics = Metrics::new(hwnd, dpi);
    let font = metrics.font;
    // 差し替えた古いフォント・ツリーのアイコンは、子コントロールが新しいものへ移ってから解放する
    let old = ctx.metrics.replace(metrics);
    unsafe {
        for id in [ID_TREE, ID_SEARCH, ID_LIST, ID_PREVIEW] {
            if let Ok(child) = GetDlgItem(Some(hwnd), id) {
                SendMessageW(child, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
            }
        }
    }
    set_tree_images(hwnd);
    set_window_icons(hwnd);
    drop(old);
    set_row_height(hwnd);
    release_images(ctx);
    layout(hwnd);
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
}

/// 一覧へキーボードのフォーカスを移す（表示したときの初期位置）。
pub fn focus_list(hwnd: HWND) {
    unsafe {
        if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
            let _ = SetFocus(Some(list));
        }
    }
}

fn window_text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

/// ツリーを作り直し、`selected` の項目を選ぶ。作り直しの間の選択の変化はハンドラへ伝えない。
/// 名前の編集中（`EditState::None` 以外）は作り直さず、構成と `selected` を保留する（最後のものだけ。
/// 編集の後始末 `complete_edit` が作り直す）。
pub fn set_tree(hwnd: HWND, nodes: &[TreeNode], selected: Source) {
    let Some(ctx) = (unsafe { ctx_ref(hwnd) }) else {
        return;
    };
    let Ok(tree) = (unsafe { GetDlgItem(Some(hwnd), ID_TREE) }) else {
        return;
    };
    if ctx.edit.get() != EditState::None {
        *ctx.tree_pending.borrow_mut() = Some((nodes.to_vec(), selected));
        return;
    }
    ctx.tree_rebuilding.set(true);
    rebuild_tree_now(ctx, tree, nodes, selected);
    ctx.tree_rebuilding.set(false);
}

/// ツリーを作り直して `selected` の項目を選ぶ（保留を見ない）。選択の通知を止める印（`tree_rebuilding`）は
/// 呼び出し側が立てて下ろす（編集の後始末は、後始末の初めから終わりまで立てておくため）。
fn rebuild_tree_now(ctx: &WindowCtx, tree: HWND, nodes: &[TreeNode], selected: Source) {
    // 表示対象の表は先に作って差し替える（挿入中に通知が来ても借用と衝突しない）
    let mut tokens = Vec::new();
    collect_tokens(nodes, &mut tokens);
    ctx.view.borrow_mut().tree_tokens = tokens;

    ctx.history_item.set(0);
    let mut insert = TreeInsert {
        tree,
        next_token: 0,
        selected,
        to_select: None,
        hilite: ctx.tree_hilite.get(),
        to_hilite: None,
        history_count: ctx.history_count.get(),
        history_item: None,
    };
    unsafe {
        SendMessageW(tree, TVM_DELETEITEM, Some(WPARAM(0)), Some(LPARAM(TVI_ROOT.0 as isize)));
        insert_tree_nodes(&mut insert, TVI_ROOT, nodes);
        if let Some(item) = insert.to_select {
            SendMessageW(tree, TVM_SELECTITEM, Some(WPARAM(TVGN_CARET as usize)), Some(LPARAM(item.0 as isize)));
        }
        if let Some(item) = insert.to_hilite {
            SendMessageW(tree, TVM_SELECTITEM, Some(WPARAM(TVGN_DROPHILITE as usize)), Some(LPARAM(item.0)));
        }
    }
    ctx.history_item.set(insert.history_item.map_or(0, |item| item.0));
}

/// 構成に `source` の項目があるか。
fn nodes_contain(nodes: &[TreeNode], source: Source) -> bool {
    nodes.iter().any(|n| n.source == source || nodes_contain(&n.children, source))
}

/// ツリーの `source` の項目（今のツリーを深さ優先で探す）。
unsafe fn find_tree_item(ctx: &WindowCtx, tree: HWND, source: Source) -> Option<HTREEITEM> {
    unsafe fn walk(ctx: &WindowCtx, tree: HWND, first: HTREEITEM, source: Source) -> Option<HTREEITEM> {
        let next = |flag: u32, from: HTREEITEM| unsafe {
            HTREEITEM(SendMessageW(tree, TVM_GETNEXTITEM, Some(WPARAM(flag as usize)), Some(LPARAM(from.0))).0)
        };
        let mut item = first;
        while item.0 != 0 {
            if unsafe { tree_item_source(ctx, tree, item) } == Some(source) {
                return Some(item);
            }
            if let Some(found) = unsafe { walk(ctx, tree, next(TVGN_CHILD, item), source) } {
                return Some(found);
            }
            item = next(TVGN_NEXT, item);
        }
        None
    }
    let root = HTREEITEM(unsafe { SendMessageW(tree, TVM_GETNEXTITEM, Some(WPARAM(TVGN_ROOT as usize)), None).0 });
    unsafe { walk(ctx, tree, root, source) }
}

/// 選んでいる項目の表示元。
unsafe fn selected_tree_source(ctx: &WindowCtx, tree: HWND) -> Option<Source> {
    let item = HTREEITEM(unsafe { SendMessageW(tree, TVM_GETNEXTITEM, Some(WPARAM(TVGN_CARET as usize)), None).0 });
    if item.0 == 0 { None } else { unsafe { tree_item_source(ctx, tree, item) } }
}

#[cfg(test)]
thread_local! {
    /// テストだけで差し込む仕掛け（名前の編集を始める途中の決まった所で呼ぶ。`edit_hook`）
    static EDIT_HOOK: RefCell<Option<Box<dyn Fn(HWND, &'static str)>>> = RefCell::new(None);
}

/// テストの仕掛け（`EDIT_HOOK`）を呼ぶ。テスト以外では何もしない。
fn edit_hook(hwnd: HWND, point: &'static str) {
    #[cfg(test)]
    EDIT_HOOK.with(|hook| {
        if let Some(f) = hook.borrow().as_ref() {
            f(hwnd, point);
        }
    });
    #[cfg(not(test))]
    let _ = (hwnd, point);
}

/// ツリーの名前の編集を始める。ほかの編集・メニュー・確認ダイアログの表示中は
/// 始めない。名前の変更は対象のフォルダの項目、作成は親の項目の子の末尾に仮の項目（`TEMP_TOKEN`）を
/// 挿入してその項目を編集する。取り消しの印（`edit_cancel`）はここから戻るまで保持し、ツリーへ
/// フォーカスを移した後・`TVN_BEGINLABELEDIT` で確かめる。後始末の投稿は `finish_edit` だけが行う。
unsafe fn begin_edit(hwnd: HWND, ctx: &WindowCtx, target: EditTarget) {
    unsafe {
        if ctx.edit.get() != EditState::None || ctx.menu_open.get() || dialog_active(ctx) {
            return;
        }
        let Ok(tree) = GetDlgItem(Some(hwnd), ID_TREE) else {
            return;
        };
        let restore = selected_tree_source(ctx, tree);
        let item = match target {
            EditTarget::Rename { id } => find_tree_item(ctx, tree, Source::Pinned(Some(id))),
            EditTarget::Create { parent } => find_tree_item(ctx, tree, Source::Pinned(parent)).and_then(|parent_item| {
                let mut text: Vec<u16> = NEW_FOLDER_LABEL.encode_utf16().chain(std::iter::once(0)).collect();
                let image = tree_image(Source::Pinned(Some(Uuid::nil())));
                let insert = TVINSERTSTRUCTW {
                    hParent: parent_item,
                    hInsertAfter: TVI_LAST,
                    Anonymous: TVINSERTSTRUCTW_0 {
                        item: TVITEMW {
                            mask: TVIF_TEXT | TVIF_PARAM | TVIF_IMAGE | TVIF_SELECTEDIMAGE,
                            pszText: PWSTR(text.as_mut_ptr()),
                            iImage: image,
                            iSelectedImage: image,
                            lParam: LPARAM(TEMP_TOKEN as isize),
                            ..Default::default()
                        },
                    },
                };
                let temp =
                    HTREEITEM(SendMessageW(tree, TVM_INSERTITEMW, None, Some(LPARAM(&insert as *const _ as isize))).0);
                if temp.0 == 0 {
                    return None;
                }
                SendMessageW(tree, TVM_EXPAND, Some(WPARAM(TVE_EXPAND.0 as usize)), Some(LPARAM(parent_item.0)));
                SendMessageW(tree, TVM_ENSUREVISIBLE, None, Some(LPARAM(temp.0)));
                ctx.edit_temp.set(temp.0);
                Some(temp)
            }),
        };
        let Some(item) = item else {
            return;
        };
        ctx.edit_cancel.set(false);
        ctx.edit_user_choice.set(None);
        ctx.edit_restore.set(restore);
        ctx.edit.set(EditState::Starting(target));
        // TVM_EDITLABEL の前にツリーへフォーカスを移す（Microsoft Learn の TreeView_EditLabel）
        let _ = SetFocus(Some(tree));
        edit_hook(hwnd, "after_focus");
        if ctx.edit_cancel.get() {
            finish_edit(hwnd, ctx, None);
            return;
        }
        let edit = SendMessageW(tree, TVM_EDITLABELW, None, Some(LPARAM(item.0))).0;
        match ctx.edit.get() {
            // 始まらなかった（TVN_BEGINLABELEDIT が来なかった、または来て断った。断ったときは
            // TVN_ENDLABELEDIT が来ない。実機で確かめた）
            EditState::Starting(_) if edit == 0 => finish_edit(hwnd, ctx, None),
            // 防御の分岐（今の規則では、印が立っていれば TVN_BEGINLABELEDIT で断るので、ふつうは起きない）
            EditState::Editing(_) if ctx.edit_cancel.get() => {
                SendMessageW(tree, TVM_ENDEDITLABELNOW, Some(WPARAM(1)), None);
            }
            _ => {}
        }
    }
}

/// 名前の編集を終わらせ、後始末を投げる。`Starting`・`Editing` から `Finishing` へ変えるのはここだけで、
/// ほかの状態から呼ばれたら何もしない（開始の失敗と `TVN_ENDLABELEDIT` が重なっても、後始末は一度だけ）。
/// `text` は確定なら入力した文字、取り消しなら None。
fn finish_edit(hwnd: HWND, ctx: &WindowCtx, text: Option<String>) {
    let target = match ctx.edit.get() {
        EditState::Starting(target) | EditState::Editing(target) => target,
        EditState::None | EditState::Finishing => return,
    };
    ctx.edit.set(EditState::Finishing);
    *ctx.edit_done.borrow_mut() = Some((target, text));
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_APP_EDIT_DONE, WPARAM(0), LPARAM(0));
    }
}

/// 隠す・終了の要求で名前の編集を取り消す（`cancel_modal` から）。始めている途中なら印を立て（`begin_edit`・
/// `TVN_BEGINLABELEDIT` が見る）、編集中なら取り消す（後始末は `TVN_ENDLABELEDIT` → `finish_edit`）。
fn cancel_edit(hwnd: HWND, ctx: &WindowCtx) {
    match ctx.edit.get() {
        EditState::Starting(_) => ctx.edit_cancel.set(true),
        EditState::Editing(_) => unsafe {
            if let Ok(tree) = GetDlgItem(Some(hwnd), ID_TREE) {
                SendMessageW(tree, TVM_ENDEDITLABELNOW, Some(WPARAM(1)), None);
            }
        },
        EditState::None | EditState::Finishing => {}
    }
}

/// `IsDialogMessageW` が Enter・Esc から作った `IDOK`・`IDCANCEL` で、名前の編集を確定・取り消しする
/// （編集中の Enter・Esc は編集欄に届かない。実機で確かめた）。始めている途中・後始末を待つ間は何も
/// しないが、true を返して一覧への送出・検索欄の消去へは流さない。編集して
/// いなければ false。
fn end_edit_by_key(hwnd: HWND, ctx: &WindowCtx, cancel: bool) -> bool {
    match ctx.edit.get() {
        EditState::None => false,
        EditState::Editing(_) => {
            unsafe {
                if let Ok(tree) = GetDlgItem(Some(hwnd), ID_TREE) {
                    SendMessageW(tree, TVM_ENDEDITLABELNOW, Some(WPARAM(cancel as usize)), None);
                }
            }
            true
        }
        EditState::Starting(_) | EditState::Finishing => true,
    }
}

/// 名前の編集の後始末（`WM_APP_EDIT_DONE`）。初めから終わりまで選択の通知を止め、仮の項目を
/// 消し、表示元を選び（編集を終わらせたユーザーの選択 → 保留の `selected` → 編集を始めたときの選択 →
/// 「履歴」のうち、作り直す構成にある最初のもの）、保留の構成があれば作り直して保留を消費する。選んだ
/// 表示元がアプリの今の表示元と違えば1回伝え、確定した名前をハンドラへ頼む（保存が済んだ後の起床で
/// ツリーに出る）。最後に自分を起こす（保留した失敗の通知を出させる）。
unsafe fn complete_edit(hwnd: HWND, ctx: &WindowCtx) {
    unsafe {
        if ctx.edit.get() != EditState::Finishing {
            return;
        }
        let done = ctx.edit_done.borrow_mut().take();
        let Ok(tree) = GetDlgItem(Some(hwnd), ID_TREE) else {
            ctx.edit.set(EditState::None);
            return;
        };
        ctx.tree_rebuilding.set(true);
        let temp = ctx.edit_temp.replace(0);
        if temp != 0 {
            SendMessageW(tree, TVM_DELETEITEM, Some(WPARAM(0)), Some(LPARAM(temp)));
        }
        let pending = ctx.tree_pending.borrow_mut().take();
        let restore = ctx.edit_restore.take();
        let user_choice = ctx.edit_user_choice.take();
        let exists = |source: Source| match &pending {
            Some((nodes, _)) => nodes_contain(nodes, source),
            None => ctx.view.borrow().tree_tokens.contains(&source),
        };
        let chosen = [user_choice, pending.as_ref().map(|(_, selected)| *selected), restore, Some(Source::History)]
            .into_iter()
            .flatten()
            .find(|source| exists(*source))
            .unwrap_or(Source::History);
        // アプリの今の表示元: 保留があればアプリが最後に指定したもの、無ければ編集を始めたときのもの
        // （編集の間は選択の変化を伝えていない）
        let app_source = pending.as_ref().map(|(_, selected)| *selected).or(restore);
        match &pending {
            Some((nodes, _)) => rebuild_tree_now(ctx, tree, nodes, chosen),
            None => {
                if let Some(item) = find_tree_item(ctx, tree, chosen) {
                    SendMessageW(tree, TVM_SELECTITEM, Some(WPARAM(TVGN_CARET as usize)), Some(LPARAM(item.0)));
                }
            }
        }
        ctx.tree_rebuilding.set(false);
        ctx.edit.set(EditState::None);
        let handler = Rc::clone(&ctx.handler);
        if app_source != Some(chosen) {
            handler.on_source_selected(hwnd, chosen);
        }
        if let Some((target, Some(text))) = done {
            let title = text.trim().to_string();
            if !title.is_empty() {
                let command = match target {
                    EditTarget::Rename { id } => TreeCommand::RenameFolder { id, title },
                    EditTarget::Create { parent } => TreeCommand::CreateFolder { parent, title },
                };
                handler.on_tree_command(hwnd, command);
            }
        }
        let _ = PostMessageW(Some(hwnd), WM_APP_WAKE, WPARAM(0), LPARAM(0));
    }
}

/// 失敗の通知を出してはいけない間か（メニュー・確認ダイアログの表示中、ツリーの名前の編集中。編集中に
/// 出すとフォーカスが外れて編集が確定するため。編集はモーダルではないので `modal_is_open` とは分ける）。
pub fn notice_blocked(hwnd: HWND) -> bool {
    modal_is_open(hwnd) || unsafe { ctx_ref(hwnd) }.is_some_and(|ctx| ctx.edit.get() != EditState::None)
}

fn collect_tokens(nodes: &[TreeNode], out: &mut Vec<Source>) {
    for node in nodes {
        out.push(node.source);
        collect_tokens(&node.children, out);
    }
}

/// ツリーへ挿入している間の状態（`insert_tree_nodes`）。
struct TreeInsert {
    tree: HWND,
    /// 次の項目の lParam（`collect_tokens` の順の添字）
    next_token: usize,
    selected: Source,
    /// `selected` の項目（挿入後に選ぶ）
    to_select: Option<HTREEITEM>,
    /// 右クリックメニューの間だけ強調している表示対象と、その新しい項目（挿入後に強調し直す）
    hilite: Option<Source>,
    to_hilite: Option<HTREEITEM>,
    /// 「履歴」に添える件数と、「履歴」の項目（件数の表示を後から書き換える）
    history_count: usize,
    history_item: Option<HTREEITEM>,
}

/// `collect_tokens` と同じ順（深さ優先）で挿入し、lParam にその順の添字を置く。
unsafe fn insert_tree_nodes(state: &mut TreeInsert, parent: HTREEITEM, nodes: &[TreeNode]) {
    for node in nodes {
        let mut text: Vec<u16> =
            tree_label(node, state.history_count).encode_utf16().chain(std::iter::once(0)).collect();
        let image = tree_image(node.source);
        let insert = TVINSERTSTRUCTW {
            hParent: parent,
            hInsertAfter: TVI_LAST,
            Anonymous: TVINSERTSTRUCTW_0 {
                item: TVITEMW {
                    mask: TVIF_TEXT | TVIF_PARAM | TVIF_IMAGE | TVIF_SELECTEDIMAGE,
                    pszText: PWSTR(text.as_mut_ptr()),
                    iImage: image,
                    iSelectedImage: image,
                    lParam: LPARAM(state.next_token as isize),
                    ..Default::default()
                },
            },
        };
        state.next_token += 1;
        let item = unsafe {
            HTREEITEM(SendMessageW(state.tree, TVM_INSERTITEMW, None, Some(LPARAM(&insert as *const _ as isize))).0)
        };
        if node.source == state.selected {
            state.to_select = Some(item);
        }
        if Some(node.source) == state.hilite {
            state.to_hilite = Some(item);
        }
        if node.source == Source::History {
            state.history_item = Some(item);
        }
        unsafe {
            insert_tree_nodes(state, item, &node.children);
            SendMessageW(state.tree, TVM_EXPAND, Some(WPARAM(TVE_EXPAND.0 as usize)), Some(LPARAM(item.0 as isize)));
        }
    }
}

fn selected_index(list: HWND) -> Option<usize> {
    let i = unsafe {
        SendMessageW(list, LVM_GETNEXTITEM, Some(WPARAM(usize::MAX)), Some(LPARAM(LVNI_SELECTED as isize))).0
    };
    (i >= 0).then_some(i as usize)
}

/// `index` = -1 は全項目。
unsafe fn set_item_state(list: HWND, index: i32, state: u32) {
    let item = LVITEMW {
        stateMask: windows::Win32::UI::Controls::LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0),
        state: windows::Win32::UI::Controls::LIST_VIEW_ITEM_STATE_FLAGS(state),
        ..Default::default()
    };
    unsafe {
        SendMessageW(list, LVM_SETITEMSTATE, Some(WPARAM(index as isize as usize)), Some(LPARAM(&item as *const _ as isize)));
    }
}

fn init_common_controls() {
    let icc = INITCOMMONCONTROLSEX {
        dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_LISTVIEW_CLASSES | ICC_TREEVIEW_CLASSES,
    };
    unsafe {
        let _ = InitCommonControlsEx(&icc);
    }
}

fn register_class() -> WinResult<()> {
    unsafe {
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: GetModuleHandleW(None)?.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            // 見えるのは子コントロールの間の境目のすき間だけ（境目が分かるよう、窓の地の色にする）
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as usize as *mut _),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        // テストなどで2回目以降に呼ばれたときは登録済みでよい
        if RegisterClassW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            return Err(windows::core::Error::from_thread());
        }
        let image = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(image_wndproc),
            hInstance: GetModuleHandleW(None)?.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: IMAGE_CLASS_NAME,
            ..Default::default()
        };
        if RegisterClassW(&image) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            return Err(windows::core::Error::from_thread());
        }
    }
    Ok(())
}

/// `dpi` に合わせたシステムの UI フォント（メッセージ用）と、その太字。取れなければ既定の
/// GUI フォント（3つ目が true。解放しない）。
fn create_fonts(dpi: u32) -> (HFONT, HFONT, bool) {
    unsafe {
        let mut ncm = NONCLIENTMETRICSW {
            cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };
        let ok = SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            ncm.cbSize,
            Some((&mut ncm as *mut NONCLIENTMETRICSW).cast()),
            0,
            dpi,
        )
        .is_ok();
        if !ok {
            let f = HFONT(GetStockObject(DEFAULT_GUI_FONT).0);
            return (f, f, true);
        }
        let normal = CreateFontIndirectW(&ncm.lfMessageFont);
        let mut bold = ncm.lfMessageFont;
        bold.lfWeight = FW_BOLD.0 as i32;
        (normal, CreateFontIndirectW(&bold), false)
    }
}

fn create_icons() -> [HICON; 5] {
    [icons::TEXT, icons::IMAGE, icons::FILE, icons::OTHER, icons::FOLDER]
        .map(|rgba| crate::tray::icon_from_rgba(rgba, ICON_SIZE).unwrap_or_default())
}

/// 1行の EDIT（検索欄）の高さ: 文字の高さに枠と内側の余白を足す。
fn search_height(hwnd: HWND, font: HFONT, pad: i32) -> i32 {
    unsafe {
        let hdc = GetDC(Some(hwnd));
        let mut tm = TEXTMETRICW::default();
        let old = SelectObject(hdc, font.into());
        let _ = GetTextMetricsW(hdc, &mut tm);
        SelectObject(hdc, old);
        ReleaseDC(Some(hwnd), hdc);
        tm.tmHeight + 2 * pad + 2
    }
}

/// 行の高さ: 2行分の文字（太字1行 + 通常1行）とアイコンの大きい方に、上下の余白を足す。
fn row_height(hwnd: HWND, font: HFONT, bold_font: HFONT, icon_size: i32, pad: i32) -> i32 {
    unsafe {
        let hdc = GetDC(Some(hwnd));
        let mut tm = TEXTMETRICW::default();
        let old = SelectObject(hdc, bold_font.into());
        let _ = GetTextMetricsW(hdc, &mut tm);
        let bold_h = tm.tmHeight;
        SelectObject(hdc, font.into());
        let _ = GetTextMetricsW(hdc, &mut tm);
        let normal_h = tm.tmHeight;
        SelectObject(hdc, old);
        ReleaseDC(Some(hwnd), hdc);
        (bold_h + normal_h + pad).max(icon_size) + 2 * pad
    }
}

fn create_children(parent: HWND, font: HFONT) -> WinResult<()> {
    let child = |ex: WINDOW_EX_STYLE, class: PCWSTR, style: WINDOW_STYLE, id: i32| -> WinResult<HWND> {
        let hwnd = unsafe {
            CreateWindowExW(
                ex,
                class,
                None,
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | style,
                0,
                0,
                0,
                0,
                Some(parent),
                Some(HMENU(id as usize as *mut _)),
                Some(GetModuleHandleW(None)?.into()),
                None,
            )?
        };
        unsafe {
            SendMessageW(hwnd, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(0)));
        }
        Ok(hwnd)
    };
    // Tab での移動順は作った順（ツリー → 検索欄 → 一覧 → プレビュー）
    let tree = child(
        WS_EX_CLIENTEDGE,
        WC_TREEVIEWW,
        // TVS_EDITLABELS はフォルダの名前の編集。編集を始めてよいのは begin_edit からだけで、
        // 文字のクリックで始まる編集は TVN_BEGINLABELEDIT で断る
        WINDOW_STYLE(TVS_HASLINES | TVS_HASBUTTONS | TVS_LINESATROOT | TVS_SHOWSELALWAYS | TVS_EDITLABELS),
        ID_TREE,
    )?;
    unsafe {
        let _ = SetWindowSubclass(tree, Some(tree_subclass_proc), TREE_SUBCLASS_ID, 0);
    }
    let search = child(WS_EX_CLIENTEDGE, w!("EDIT"), WINDOW_STYLE(ES_AUTOHSCROLL as u32), ID_SEARCH)?;
    unsafe {
        // 空のときに薄く出す案内（Common Controls v6 の機能）
        SendMessageW(
            search,
            EM_SETCUEBANNER,
            Some(WPARAM(0)),
            Some(LPARAM(w!("検索（タイトルとテキスト）").as_ptr() as isize)),
        );
    }
    // 複数選択は実装していないが、将来の複数選択と衝突させないため LVS_SINGLESEL は
    // 付けない。選択は先頭の1件として扱う
    let list = child(
        WS_EX_CLIENTEDGE,
        WC_LISTVIEWW,
        WINDOW_STYLE(LVS_REPORT | LVS_OWNERDATA | LVS_OWNERDRAWFIXED | LVS_SHOWSELALWAYS | LVS_NOCOLUMNHEADER),
        ID_LIST,
    )?;
    // レポート表示は列が無いと何も描かないため、全幅の1列を置く（幅は layout で合わせる）
    let column = LVCOLUMNW { mask: LVCF_WIDTH, cx: 100, ..Default::default() };
    unsafe {
        SendMessageW(list, LVM_INSERTCOLUMNW, Some(WPARAM(0)), Some(LPARAM(&column as *const _ as isize)));
    }
    let preview = child(
        WS_EX_CLIENTEDGE,
        w!("EDIT"),
        WS_VSCROLL | WINDOW_STYLE((ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32),
        ID_PREVIEW,
    )?;
    unsafe {
        let _ = SetWindowSubclass(preview, Some(preview_subclass_proc), PREVIEW_SUBCLASS_ID, 0);
        // 画像のプレビューは、画像の行を選んだときだけ EDIT の代わりに出す（初めは隠しておく）。
        // Tab での移動の対象にはしない
        CreateWindowExW(
            WS_EX_CLIENTEDGE,
            IMAGE_CLASS_NAME,
            None,
            WS_CHILD,
            0,
            0,
            0,
            0,
            Some(parent),
            Some(HMENU(ID_IMAGE as usize as *mut _)),
            Some(GetModuleHandleW(None)?.into()),
            None,
        )?;
    }
    Ok(())
}

const PREVIEW_SUBCLASS_ID: usize = 1;

/// プレビュー（複数行の EDIT）のサブクラス。複数行の EDIT は `WM_GETDLGCODE` で
/// `DLGC_WANTALLKEYS` を返し、Tab も自分で受け取るため、`IsDialogMessageW` による Tab での
/// 移動がプレビューで止まる（2026-09-23 に実機で確認）。読み取り専用で Tab・Enter・Esc を
/// 使わないので、それらを求めないと答えさせる。
unsafe extern "system" fn preview_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _ref: usize,
) -> LRESULT {
    unsafe {
        match msg {
            WM_GETDLGCODE => {
                let code = DefSubclassProc(hwnd, msg, wparam, lparam).0 as u32;
                LRESULT((code & !(DLGC_WANTALLKEYS | DLGC_WANTTAB)) as isize)
            }
            WM_NCDESTROY => {
                let _ = RemoveWindowSubclass(hwnd, Some(preview_subclass_proc), PREVIEW_SUBCLASS_ID);
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            _ => DefSubclassProc(hwnd, msg, wparam, lparam),
        }
    }
}

const TREE_SUBCLASS_ID: usize = 2;

/// ツリーのサブクラス。マウスの右ボタンはツリーの既定の処理へ渡さず、ここで扱う:
/// 押したらマウスを捕まえて押した項目（`TVHT_ONITEM` の項目）の表示元を覚え、離したら、押したのと
/// 同じ表示元の項目の上のときだけ、その位置の `WM_CONTEXTMENU` を親へ送る（`show_tree_menu`。メニューが
/// 無い項目なら出ない）。ほかの場所で離したら何もしない。
///
/// 経緯: 既定の処理は押した項目を一時的に強調し、メニューが出ないとすぐ戻す（一瞬フォーカスが移った
/// ように見えた）。また、押したまま動かすと右ドラッグとみなして追跡をやめるため、一覧の上で離すと一覧の
/// 右クリックメニューが出ていた（ユーザーの実機確認）。キーボード（Shift+F10・アプリケーションキー）の
/// `WM_CONTEXTMENU` は今までどおり既定の処理から来る。
unsafe extern "system" fn tree_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _ref: usize,
) -> LRESULT {
    unsafe {
        let ctx = || GetParent(hwnd).ok().and_then(|parent| ctx_ref(parent));
        match msg {
            // クラスが CS_DBLCLKS を持つと、素早い2回目の押下は WM_RBUTTONDBLCLK で届くので同じく扱う
            WM_RBUTTONDOWN | WM_RBUTTONDBLCLK => {
                // 名前の編集中は何もしない（フォーカスが移らず編集が続く。押下を覚えないので、離す操作も
                // 下で捨てられる）
                if let Some(ctx) = ctx().filter(|ctx| ctx.edit.get() == EditState::None) {
                    let (x, y) = mouse_point(lparam);
                    // 項目はハンドルではなく表示元で覚える（押している間にツリーが作り直されても、同じ項目で
                    // 離せばメニューが出て、使い回されたハンドルの別の項目と取り違えない）
                    ctx.tree_rpress.set(Some(tree_source_at_point(ctx, hwnd, POINT { x, y })));
                    // 押したままツリーの外で離しても、離す操作がここへ届くようにする（離したら外す）
                    SetCapture(hwnd);
                }
                LRESULT(0)
            }
            // 押下を覚えていない離す操作（ほかのコントロールで押して、ここで離した）も既定の処理へ渡さない
            // （渡すと既定の処理が離した位置で WM_CONTEXTMENU を出す）
            WM_RBUTTONUP => {
                if let Some(ctx) = ctx() {
                    // 印を先に下ろす（外すと WM_CAPTURECHANGED が来る）
                    let pressed = ctx.tree_rpress.take();
                    if GetCapture() == hwnd {
                        let _ = ReleaseCapture();
                    }
                    let (x, y) = mouse_point(lparam);
                    let released = tree_source_at_point(ctx, hwnd, POINT { x, y });
                    if pressed.is_some_and(|source| source.is_some() && source == released) {
                        let mut screen = POINT { x, y };
                        let _ = ClientToScreen(hwnd, &mut screen);
                        let at = ((screen.y as u16 as u32) << 16 | screen.x as u16 as u32) as isize;
                        if let Ok(parent) = GetParent(hwnd) {
                            SendMessageW(parent, WM_CONTEXTMENU, Some(WPARAM(hwnd.0 as usize)), Some(LPARAM(at)));
                        }
                    }
                }
                LRESULT(0)
            }
            // 離す前に捕まえたマウスを失った（Alt+Tab・別の窓がキャプチャした など）: 押下は終わったものとする
            WM_CAPTURECHANGED => {
                if let Some(ctx) = ctx() {
                    ctx.tree_rpress.set(None);
                }
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            WM_NCDESTROY => {
                let _ = RemoveWindowSubclass(hwnd, Some(tree_subclass_proc), TREE_SUBCLASS_ID);
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            _ => DefSubclassProc(hwnd, msg, wparam, lparam),
        }
    }
}

/// ツリーの `point`（クライアント座標）にある項目（文字・アイコンの上）の表示元。無ければ None。
unsafe fn tree_source_at_point(ctx: &WindowCtx, tree: HWND, point: POINT) -> Option<Source> {
    let item = unsafe { tree_item_at_point(tree, point) };
    if item.0 == 0 { None } else { unsafe { tree_item_source(ctx, tree, item) } }
}

/// ツリーの `point`（クライアント座標）にある項目（文字・アイコンの上。`TVHT_ONITEM`）。無ければ 0。
unsafe fn tree_item_at_point(tree: HWND, point: POINT) -> HTREEITEM {
    let mut hit = TVHITTESTINFO { pt: point, ..Default::default() };
    let item = HTREEITEM(unsafe { SendMessageW(tree, TVM_HITTEST, None, Some(LPARAM(&mut hit as *mut _ as isize))).0 });
    if item.0 == 0 || (hit.flags.0 & TVHT_ONITEM.0) == 0 { HTREEITEM(0) } else { item }
}

/// 境目（スプリッタ）の太さと、境目で動かせる範囲の最小値（96 DPI 基準の px）。
const SPLITTER_SIZE: i32 = 4;
/// ツリーの幅の最小
const TREE_MIN_WIDTH: i32 = 80;
/// 右側（検索欄・一覧・プレビュー）の幅の最小
const RIGHT_MIN_WIDTH: i32 = 200;
/// 一覧・プレビューの高さの最小
const PANE_MIN_HEIGHT: i32 = 60;

/// 境目の種類。縦の境目はツリーと右側の間、横の境目は一覧とプレビューの間。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Splitter {
    Vertical,
    Horizontal,
}

/// ドラッグ中の境目と、押した位置の境目の端からのずれ（px）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Drag {
    splitter: Splitter,
    offset: i32,
}

/// 子コントロールと境目の配置（クライアント座標の px）。
#[derive(Debug, PartialEq)]
struct PaneLayout {
    tree: RECT,
    search: RECT,
    list: RECT,
    preview: RECT,
    vertical_bar: RECT,
    horizontal_bar: RECT,
}

/// `v` を `lo` 以上 `hi` 以下にする。`hi` が `lo` より小さい（窓が狭すぎる）ときは `lo` を優先する。
fn clamp_low_first(v: i32, lo: i32, hi: i32) -> i32 {
    v.min(hi).max(lo)
}

/// ツリーの幅（px）。`tree_width` はドラッグで決めた幅（96 DPI 基準）で、None ならクライアント領域の幅の 1/4
/// （既定の配置）。最小値で制限し、右側の最小の幅を残す。
fn tree_width_px(w: i32, dpi: u32, tree_width: Option<u32>) -> i32 {
    let s = |v: i32| crate::menu_tooltip::scale_for_dpi(v, dpi);
    let desired = tree_width.map_or(w / 4, |v| s(v as i32));
    clamp_low_first(desired, s(TREE_MIN_WIDTH), w - s(SPLITTER_SIZE) - s(RIGHT_MIN_WIDTH))
}

/// プレビューの高さ（px）。`preview_height` はドラッグで決めた高さ（96 DPI 基準）で、None なら既定の配置
/// （一覧の下端がクライアント領域の高さの 6 割）。最小値で制限し、一覧の最小の高さを残す。
fn preview_height_px(h: i32, search_h: i32, dpi: u32, preview_height: Option<u32>) -> i32 {
    let s = |v: i32| crate::menu_tooltip::scale_for_dpi(v, dpi);
    let bar = s(SPLITTER_SIZE);
    let desired = preview_height.map_or(h - (h * 6 / 10).max(search_h) - bar, |v| s(v as i32));
    clamp_low_first(desired, s(PANE_MIN_HEIGHT), h - search_h - bar - s(PANE_MIN_HEIGHT))
}

/// クライアント領域 `w`×`h` での配置。左にツリー（窓の高さいっぱい）、縦の境目、右の一番上に検索欄、
/// その下に一覧、横の境目、プレビュー。
fn compute_layout(w: i32, h: i32, search_h: i32, dpi: u32, tree_width: Option<u32>, preview_height: Option<u32>) -> PaneLayout {
    let bar = crate::menu_tooltip::scale_for_dpi(SPLITTER_SIZE, dpi);
    let rect = |left: i32, top: i32, width: i32, height: i32| RECT {
        left,
        top,
        right: left + width.max(0),
        bottom: top + height.max(0),
    };
    let tree_w = tree_width_px(w, dpi, tree_width);
    let right_x = tree_w + bar;
    let right_w = w - right_x;
    let preview_h = preview_height_px(h, search_h, dpi, preview_height);
    let list_h = h - search_h - bar - preview_h;
    let bar_y = search_h + list_h.max(0);
    PaneLayout {
        tree: rect(0, 0, tree_w, h),
        vertical_bar: rect(tree_w, 0, bar, h),
        search: rect(right_x, 0, right_w, search_h),
        list: rect(right_x, search_h, right_w, list_h),
        horizontal_bar: rect(right_x, bar_y, right_w, bar),
        preview: rect(right_x, bar_y + bar, right_w, h - bar_y - bar),
    }
}

/// クライアント座標の点がどの境目の上か。
fn splitter_at(layout: &PaneLayout, x: i32, y: i32) -> Option<Splitter> {
    let inside = |r: &RECT| x >= r.left && x < r.right && y >= r.top && y < r.bottom;
    if inside(&layout.vertical_bar) {
        Some(Splitter::Vertical)
    } else if inside(&layout.horizontal_bar) {
        Some(Splitter::Horizontal)
    } else {
        None
    }
}

/// 今の窓の配置（コンテキストを置く前の WM_SIZE では、検索欄の高さは仮の値で、境目は動かしていない扱い）。
fn current_layout(hwnd: HWND) -> Option<PaneLayout> {
    let mut rc = RECT::default();
    unsafe { GetClientRect(hwnd, &mut rc) }.ok()?;
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let (search_h, tree_width, preview_height) = match unsafe { ctx_ref(hwnd) } {
        Some(ctx) => (ctx.metrics.borrow().search_height, ctx.tree_width.get(), ctx.preview_height.get()),
        None => (24, None, None),
    };
    Some(compute_layout(rc.right, rc.bottom, search_h, dpi, tree_width, preview_height))
}

/// ドラッグ中の境目を、マウスの位置（クライアント座標）に合わせて動かす。動かした境目の大きさは、
/// 最小値で制限したものを 96 DPI 基準で覚える（窓を狭めて表示の上で縮めた分は覚えない）。
fn drag_to(hwnd: HWND, ctx: &WindowCtx, drag: Drag, x: i32, y: i32) {
    let mut rc = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut rc) }.is_err() {
        return;
    }
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let bar = crate::menu_tooltip::scale_for_dpi(SPLITTER_SIZE, dpi);
    match drag.splitter {
        Splitter::Vertical => {
            let px = tree_width_px(rc.right, dpi, Some(unscale(x - drag.offset, dpi)));
            ctx.tree_width.set(Some(unscale(px, dpi)));
        }
        Splitter::Horizontal => {
            let search_h = ctx.metrics.borrow().search_height;
            // 境目の上端が (y - offset) になるプレビューの高さ
            let desired = rc.bottom - (y - drag.offset) - bar;
            let px = preview_height_px(rc.bottom, search_h, dpi, Some(unscale(desired, dpi)));
            ctx.preview_height.set(Some(unscale(px, dpi)));
        }
    }
    layout(hwnd);
}

/// マウスのメッセージの lParam のクライアント座標（符号付き。捕まえている間は窓の外で負になりうる）。
fn mouse_point(lparam: LPARAM) -> (i32, i32) {
    ((lparam.0 & 0xFFFF) as u16 as i16 as i32, ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32)
}

/// 境目の上のカーソル（左右・上下の矢印）。
fn set_splitter_cursor(splitter: Splitter) {
    let id = match splitter {
        Splitter::Vertical => IDC_SIZEWE,
        Splitter::Horizontal => IDC_SIZENS,
    };
    unsafe {
        if let Ok(cursor) = LoadCursorW(None, id) {
            SetCursor(Some(cursor));
        }
    }
}

/// 子コントロールを `compute_layout` の配置に置く。境目のすき間には窓の背景（`COLOR_BTNFACE`）が見える。
fn layout(hwnd: HWND) {
    let Some(pane) = current_layout(hwnd) else {
        return;
    };
    unsafe {
        let place = |id: i32, r: &RECT| {
            if let Ok(child) = GetDlgItem(Some(hwnd), id) {
                let _ = MoveWindow(child, r.left, r.top, r.right - r.left, r.bottom - r.top, true);
            }
        };
        place(ID_TREE, &pane.tree);
        place(ID_SEARCH, &pane.search);
        place(ID_LIST, &pane.list);
        place(ID_PREVIEW, &pane.preview);
        place(ID_IMAGE, &pane.preview);
        // 1列の幅を一覧の内側の幅に合わせる
        if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
            let mut lrc = RECT::default();
            if GetClientRect(list, &mut lrc).is_ok() {
                SendMessageW(list, LVM_SETCOLUMNWIDTH, Some(WPARAM(0)), Some(LPARAM(lrc.right as isize)));
            }
        }
    }
    // プレビュー欄が広がったら、画像を大きく読み直す（窓の大きさ・境目・DPI の変更のどれでも通る）
    schedule_preview_refit(hwnd);
}

/// 文字列を描く。空なら何もしない（空の `Vec` の指す先は中身のないポインタで、長さ 0 のまま
/// `DrawTextW` に渡すと、`DT_END_ELLIPSIS` などの処理で読みに行って落ちる。先頭が改行のテキストは
/// タイトルが空になる）。
unsafe fn draw_text(hdc: HDC, text: &str, rc: &mut RECT, flags: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT) {
    if text.is_empty() {
        return;
    }
    let mut buf: Vec<u16> = text.encode_utf16().collect();
    unsafe {
        DrawTextW(hdc, &mut buf, rc, flags);
    }
}

/// 一覧の1行を描く: 左に種別アイコン、右に太字のタイトルと、経過時間・形式の行。
/// サムネイルがあればそれを、なければ種別アイコンを描く。サムネイルがまだ無い画像の行は、
/// 描き終えて借用を手放してから読み込みを頼む。
unsafe fn draw_row(ctx: &WindowCtx, dis: &DRAWITEMSTRUCT) {
    let request = unsafe { draw_row_contents(ctx, dis) };
    if let Some((id, thumb)) = request {
        ctx.view.borrow_mut().thumbs.insert(id, None);
        if let Some(link) = ctx.images.borrow().as_ref() {
            link.with_wanted(|wanted| {
                wanted.insert(id);
            });
        }
        let size = ctx.metrics.borrow().icon_size as u32;
        ctx.request_image(id, Purpose::ListThumb, ImageSource::Thumb(thumb), size, size);
    }
}

/// 行を描き、読み込みを頼むべきサムネイル（項目とファイル名）があれば返す。
unsafe fn draw_row_contents(ctx: &WindowCtx, dis: &DRAWITEMSTRUCT) -> Option<(Uuid, String)> {
    let view = ctx.view.borrow();
    let m = ctx.metrics.borrow();
    let (icon, pad) = (m.icon_size, m.pad);
    let row = view.rows.get(dis.itemID as usize)?;
    // 行のドラッグの落とす先: フォルダの中なら選んだ行と同じ色で強調し、行の間なら境に線を引く（最後の行の下は
    // その行の下端）
    let mark = ctx.row_drag.get().and_then(|drag| drag.mark);
    let selected = dis.itemState.0 & ODS_SELECTED.0 != 0 || mark == Some(DropMark::Into(row.id));
    let insert_line = match mark {
        Some(DropMark::Before(Some(id))) => id == row.id,
        Some(DropMark::Before(None)) => dis.itemID as usize + 1 == view.rows.len(),
        _ => false,
    };
    let rc = dis.rcItem;
    let hdc = dis.hDC;
    let icon_left = rc.left + pad;
    let icon_top = rc.top + (rc.bottom - rc.top - icon) / 2;
    let mut request = None;
    unsafe {
        let bg = if selected { COLOR_HIGHLIGHT } else { COLOR_WINDOW };
        FillRect(hdc, &rc, GetSysColorBrush(bg));
        match (row.thumb.as_ref(), view.thumbs.get(&row.id)) {
            (Some(_), Some(Some(bmp))) => {
                // サムネイルは長辺 icon に収めてあるので、アイコンの枠の中央に原寸で描く
                let x = icon_left + (icon - bmp.width) / 2;
                let y = icon_top + (icon - bmp.height) / 2;
                bmp.draw(hdc, x, y, bmp.width, bmp.height);
            }
            (thumb, state) => {
                // 種別アイコンは 32px の素材を DPI に合わせた大きさへ伸縮して描く
                let _ = DrawIconEx(hdc, icon_left, icon_top, ctx.row_icon(row), icon, icon, 0, None, DI_NORMAL);
                if let (Some(name), None) = (thumb, state) {
                    request = Some((row.id, name.clone()));
                }
            }
        }
        SetBkMode(hdc, TRANSPARENT);
        let text_left = rc.left + pad * 2 + icon;
        let mid = (rc.top + rc.bottom) / 2;
        let flags = DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX | DT_VCENTER;

        let old = SelectObject(hdc, m.bold_font.into());
        SetTextColor(hdc, windows::Win32::Foundation::COLORREF(GetSysColor(if selected { COLOR_HIGHLIGHTTEXT } else { COLOR_WINDOWTEXT })));
        let mut title_rc = RECT { left: text_left, top: rc.top + pad, right: rc.right - pad, bottom: mid };
        draw_text(hdc, &row.label, &mut title_rc, flags);

        SelectObject(hdc, m.font.into());
        SetTextColor(hdc, windows::Win32::Foundation::COLORREF(GetSysColor(if selected { COLOR_HIGHLIGHTTEXT } else { COLOR_GRAYTEXT })));
        let mut detail_rc = RECT { left: text_left, top: mid, right: rc.right - pad, bottom: rc.bottom - pad };
        draw_text(hdc, &row.detail(crate::native::model::unix_now()), &mut detail_rc, flags);
        SelectObject(hdc, old);
        if insert_line {
            let thick = crate::menu_tooltip::scale_for_dpi(2, GetDpiForWindow(dis.hwndItem)).max(1);
            let top = if matches!(mark, Some(DropMark::Before(None))) { rc.bottom - thick } else { rc.top };
            let line = RECT { left: rc.left, top, right: rc.right, bottom: top + thick };
            FillRect(hdc, &line, GetSysColorBrush(COLOR_WINDOWTEXT));
        }
    }
    request
}

/// 読み込みスレッドから届いた画像を受け取る。世代が違うもの、一覧にもう無い項目の
/// サムネイル、待っていないプレビューは捨てる。
unsafe fn receive_images(hwnd: HWND, ctx: &WindowCtx) {
    let results: Vec<LoadedImage> =
        ctx.images.borrow().as_ref().map(|link| link.results.try_iter().collect()).unwrap_or_default();
    // 届いたサムネイルは、もう待っていない。ただし今の世代の結果だけ（隠す前の古い世代の結果で、表示し直した後の
    // 同じ項目の新しい依頼まで外すと、読み込みスレッドがその依頼を捨て、行が種別アイコンのままになる）
    let generation = ctx.view.borrow().generation;
    if let Some(link) = ctx.images.borrow().as_ref() {
        link.with_wanted(|wanted| {
            for image in results.iter().filter(|i| i.purpose == Purpose::ListThumb && i.generation == generation) {
                wanted.remove(&image.id);
            }
        });
    }
    let mut redraw = Vec::new();
    let mut preview_changed = false;
    // 大きすぎて展開しなかったプレビューの画像（借用を手放してから、省略の注記に切り替える）
    let mut preview_too_large = None;
    // 読めなかったプレビューの画像（借用を手放してから、「（プレビューなし）」に切り替える）
    let mut preview_unreadable = false;
    // 今欲しいプレビューの依頼の番号（0 は無し）。同じ項目を選び直したとき・DPI が変わったとき・読み直す
    // ときは同じ項目への依頼が重なるので、項目の ID だけでなく番号でも照合し、古い依頼の結果を捨てる
    let wanted_ticket = ctx
        .images
        .borrow()
        .as_ref()
        .map_or(0, |link| link.wanted_preview.load(std::sync::atomic::Ordering::SeqCst));
    {
        let mut view = ctx.view.borrow_mut();
        for image in results {
            if image.generation != view.generation {
                continue;
            }
            match (image.purpose, image.content) {
                (Purpose::ListThumb, ImageContent::Pixels { width, height, bgra, .. }) => {
                    let index = crate::native::model::index_of(&view.rows, image.id);
                    if let (Some(slot @ None), Some(i)) = (view.thumbs.get_mut(&image.id), index) {
                        *slot = Bitmap::from_bgra(width, height, &bgra);
                        redraw.push(i);
                    }
                }
                // サムネイル（長辺128px）が上限を超えることはなく、読めないときは結果が来ない。
                // 来ても種別アイコンのままにする
                (Purpose::ListThumb, ImageContent::TooLarge { .. } | ImageContent::Unreadable) => {}
                (Purpose::Preview, content)
                    if view.preview_wanted == Some(image.id) && wanted_ticket != 0 && image.preview_ticket == wanted_ticket =>
                {
                    match content {
                        ImageContent::Pixels { width, height, bgra, source_width, source_height } => {
                            view.preview_image = Bitmap::from_bgra(width, height, &bgra);
                            view.preview_source_size = Some((source_width, source_height));
                            preview_changed = true;
                        }
                        ImageContent::TooLarge { width, height } => preview_too_large = Some((width, height)),
                        ImageContent::Unreadable => preview_unreadable = true,
                    }
                }
                (Purpose::Preview, _) => {}
            }
        }
    }
    if let Some((width, height)) = preview_too_large {
        set_preview_text(hwnd, &crate::native::model::image_too_large_note(width, height));
    } else if preview_unreadable {
        set_preview_none(hwnd);
    }
    // 借用を手放してから描き直しを頼む
    unsafe {
        if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
            for i in redraw {
                SendMessageW(list, LVM_REDRAWITEMS, Some(WPARAM(i)), Some(LPARAM(i as isize)));
            }
        }
        if preview_changed {
            if let Ok(image) = GetDlgItem(Some(hwnd), ID_IMAGE) {
                let _ = InvalidateRect(Some(image), None, true);
            }
            // 頼んだ後に欄が広がっていたら、届いたものは小さい。読み直すかを確かめる
            schedule_preview_refit(hwnd);
        }
    }
}

/// プレビューの画像を読み直すかの確かめを、少し待ってから行う（待つ間にもう一度呼ばれたら、そこから
/// 待ち直す）。画像を出していなければ何もしない。
fn schedule_preview_refit(hwnd: HWND) {
    let Some(ctx) = (unsafe { ctx_ref(hwnd) }) else {
        return;
    };
    if ctx.view.borrow().preview_source.is_some() {
        unsafe { SetTimer(Some(hwnd), PREVIEW_REFIT_TIMER_ID, PREVIEW_REFIT_DELAY_MS, None) };
    }
}

/// 今のプレビュー欄に収まる大きさが、出している画像より大きければ、同じ読み込み元を今の欄の大きさで
/// 読み直す（届くまでは今の画像を出したまま）。元の大きさより大きくはしない（`fit_size`）。
fn refit_preview(hwnd: HWND, ctx: &WindowCtx) {
    let (id, source, (source_w, source_h), (shown_w, shown_h)) = {
        let view = ctx.view.borrow();
        match (view.preview_wanted, &view.preview_source, view.preview_source_size, &view.preview_image) {
            (Some(id), Some(source), Some(size), Some(bmp)) => (id, source.clone(), size, (bmp.width as u32, bmp.height as u32)),
            _ => return,
        }
    };
    let Ok(image) = (unsafe { GetDlgItem(Some(hwnd), ID_IMAGE) }) else {
        return;
    };
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(image, &mut rc);
    }
    let (area_w, area_h) = (rc.right.max(1) as u32, rc.bottom.max(1) as u32);
    let (fit_w, fit_h) = crate::native::images::fit_size(source_w, source_h, area_w, area_h);
    if fit_w > shown_w || fit_h > shown_h {
        ctx.request_image(id, Purpose::Preview, source, area_w, area_h);
    }
}

/// 隠したときの後片付け: 世代を進め（読み込み中の結果を捨てる）、サムネイルとプレビューの
/// 画像を解放する。
fn release_images(ctx: &WindowCtx) {
    let mut view = ctx.view.borrow_mut();
    view.generation += 1;
    view.thumbs.clear();
    view.preview_wanted = None;
    view.preview_image = None;
    view.preview_source = None;
    view.preview_source_size = None;
    if let Some(link) = ctx.images.borrow().as_ref() {
        link.with_wanted(|wanted| wanted.clear());
        link.clear_wanted_preview();
    }
}

/// プレビューの画像の子窓。画像は親窓のコンテキスト（`ViewState::preview_image`）にあり、
/// 子窓より大きければ縦横比を保って縮めて、中央に描く。
unsafe extern "system" fn image_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                FillRect(hdc, &rc, GetSysColorBrush(COLOR_WINDOW));
                if let Some(ctx) = GetParent(hwnd).ok().and_then(|p| ctx_ref(p)) {
                    let view = ctx.view.borrow();
                    if let Some(bmp) = &view.preview_image {
                        let (w, h) = crate::native::images::fit_size(
                            bmp.width as u32,
                            bmp.height as u32,
                            rc.right.max(1) as u32,
                            rc.bottom.max(1) as u32,
                        );
                        let (w, h) = (w as i32, h as i32);
                        bmp.draw(hdc, (rc.right - w) / 2, (rc.bottom - h) / 2, w, h);
                    }
                }
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

// 【読み上げ】ここから
/// 自前で描いている一覧でも、読み上げソフト（Narrator 等）は項目のテキストを
/// LVN_GETDISPINFOW で取りに来るため、画面の2行（タイトルと、経過時間・形式）をつないで返す。
/// このテキストは画面には描かない（行は WM_DRAWITEM で描く）。
unsafe fn fill_dispinfo(ctx: &WindowCtx, info: &mut NMLVDISPINFOW) {
    if (info.item.mask & LVIF_TEXT).0 == 0 || info.item.pszText.is_null() || info.item.cchTextMax <= 0 {
        return;
    }
    let view = ctx.view.borrow();
    let now = crate::native::model::unix_now();
    let text = view.rows.get(info.item.iItem as usize).map_or(String::new(), |row| accessible_text(row, now));
    let max = info.item.cchTextMax as usize - 1;
    let units: Vec<u16> = text.encode_utf16().take(max).collect();
    unsafe {
        std::ptr::copy_nonoverlapping(units.as_ptr(), info.item.pszText.0, units.len());
        *info.item.pszText.0.add(units.len()) = 0;
    }
}

/// 読み上げ用の項目のテキスト（2行目が空なら1行目だけ）。`now` は UNIX 秒。
fn accessible_text(row: &Row, now: f64) -> String {
    let detail = row.detail(now);
    if detail.is_empty() {
        row.label.clone()
    } else {
        format!("{}、{}", row.label, detail)
    }
}
// 【読み上げ】ここまで

/// 一覧の選択が変わったことを、ハンドラへ伝える予約をする。1回のクリックや範囲選択でも
/// 通知は「全部の選択を外す」「選ぶ」など複数届くため、その場では伝えず、自分宛てに
/// `WM_APP_SELECTION` を1回だけ送っておき、それを処理するときに選択を数え直して1回だけ伝える
/// （`deliver_selection`）。
fn report_selection(hwnd: HWND, ctx: &WindowCtx) {
    if ctx.rows_updating.get() || ctx.selection_posted.replace(true) {
        return;
    }
    unsafe {
        if PostMessageW(Some(hwnd), WM_APP_SELECTION, WPARAM(0), LPARAM(0)).is_err() {
            ctx.selection_posted.set(false);
        }
    }
}

/// 予約した選択の変化を、今の選択（先頭の選択項目）で1回だけハンドラへ伝える。
fn deliver_selection(hwnd: HWND, ctx: &WindowCtx) {
    ctx.selection_posted.set(false);
    let id = unsafe { GetDlgItem(Some(hwnd), ID_LIST) }
        .ok()
        .and_then(selected_index)
        .and_then(|i| ctx.view.borrow().rows.get(i).map(|r| r.id));
    let handler = Rc::clone(&ctx.handler);
    handler.on_selection_changed(hwnd, id);
}

/// 一覧で選ばれている先頭の行（複数選んでいるときも先頭だけ）。
fn selected_row(hwnd: HWND, ctx: &WindowCtx) -> Option<RowTarget> {
    let list = unsafe { GetDlgItem(Some(hwnd), ID_LIST) }.ok()?;
    let index = selected_index(list)?;
    ctx.view.borrow().rows.get(index).map(Row::target)
}

/// 一覧にキーボードのフォーカスがあるか。
fn list_has_focus(hwnd: HWND) -> bool {
    unsafe { GetDlgItem(Some(hwnd), ID_LIST) }.is_ok_and(|list| unsafe { GetFocus() } == list)
}

/// 行のドラッグ中か（右ボタンでの取り消しの後、右ボタンを離すのを待つ間を含む）。ドラッグ中もキー入力は一覧・
/// ツリーに届くが、この間はキー操作で行を送る・開く・消す・名前を変える・並べ替えることをしない（ドラッグと
/// 並行して変えると、取り消しても変更が残り、落とす先の表示ともずれるため）。
fn row_drag_active(ctx: &WindowCtx) -> bool {
    ctx.row_drag.get().is_some() || ctx.row_drag_right_up.get()
}

/// 一覧の行を送る・開く（Enter・ダブルクリック・右クリックメニュー）。フォルダの行は、一覧の通知の処理から
/// 戻った後（`WM_APP_OPEN_FOLDER`）に、ツリーでそのフォルダを選んで開く（一覧の通知の中で一覧を作り直さない）。
/// 行のドラッグ中は何もしない（`row_drag_active`）。
fn activate_row(hwnd: HWND, ctx: &WindowCtx, target: RowTarget) {
    if row_drag_active(ctx) {
        return;
    }
    if target.folder {
        ctx.open_folder.set(Some(target.id));
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_APP_OPEN_FOLDER, WPARAM(0), LPARAM(0));
        }
    } else {
        let handler = Rc::clone(&ctx.handler);
        handler.on_activate(hwnd, target);
    }
}

/// ピン留めのフォルダをツリーで選ぶ（閉じている親は開き、見える位置へスクロールする）。表示元の切り替えは、
/// ツリーの選択の変化としてハンドラへ伝わる。ツリーの名前の編集中・メニューやダイアログの表示中・行のドラッグ中・
/// ツリーにそのフォルダが無いときは何もしない。
fn open_folder(hwnd: HWND, ctx: &WindowCtx, id: Uuid) {
    if !can_open_dialog(ctx) || row_drag_active(ctx) {
        return;
    }
    let Ok(tree) = (unsafe { GetDlgItem(Some(hwnd), ID_TREE) }) else {
        return;
    };
    unsafe {
        if let Some(item) = find_tree_item(ctx, tree, Source::Pinned(Some(id))) {
            SendMessageW(tree, TVM_SELECTITEM, Some(WPARAM(TVGN_CARET as usize)), Some(LPARAM(item.0)));
            SendMessageW(tree, TVM_ENSUREVISIBLE, None, Some(LPARAM(item.0)));
        }
    }
}

/// 一覧の行を消す（Delete・右クリックメニュー）。フォルダの行は確認してから（`delete_folder_after_confirm`）。
/// 行のドラッグ中は何もしない（`row_drag_active`）。
fn delete_row(hwnd: HWND, ctx: &WindowCtx, target: RowTarget) {
    if row_drag_active(ctx) {
        return;
    }
    if target.folder {
        delete_folder_after_confirm(hwnd, ctx, target.id);
    } else {
        let handler = Rc::clone(&ctx.handler);
        handler.on_delete(hwnd, target);
    }
}

/// ピン留めのフォルダを、確認で「削除」を選んだときだけ消す（ツリー・一覧の右クリックメニュー、一覧の
/// Delete）。確認に出す名前・件数は確認の直前に取り直し、取れなければ（その間に消えた）確認を出さない。
fn delete_folder_after_confirm(hwnd: HWND, ctx: &WindowCtx, id: Uuid) {
    let handler = Rc::clone(&ctx.handler);
    if let Some(TreeMenu::PinnedFolder { title, items, folders, .. }) = handler.tree_menu(Source::Pinned(Some(id))) {
        if confirm_delete(hwnd, ctx, "フォルダの削除", &delete_folder_message(&title, items, folders)) {
            handler.on_tree_command(hwnd, TreeCommand::DeleteFolder(id));
        }
    }
}

/// Alt+↑・Alt+↓: 一覧にフォーカスがあるとき、選んでいる先頭のピン留めの行を上・下へ動かすよう伝える（動かせるか
/// はハンドラが決める）。ツリーの名前の編集中・行のドラッグ中は何もしない。
fn reorder_selected_row(hwnd: HWND, ctx: &WindowCtx, direction: Direction) {
    if !list_has_focus(hwnd) || ctx.edit.get() != EditState::None || row_drag_active(ctx) {
        return;
    }
    if let Some(target) = selected_row(hwnd, ctx).filter(|t| t.pinned) {
        let handler = Rc::clone(&ctx.handler);
        handler.on_row_command(hwnd, target, RowCommand::Reorder(direction));
    }
}

// --- 一覧の行のドラッグ（ピン留めの行の移動、履歴の行のピン留め） ---

/// ドラッグで落とす先（目印を出す所）。一覧の行は添字ではなく ID で持つ（ドラッグの間に一覧が作り直されても、
/// 目印が別の行へずれない）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DropMark {
    /// 一覧の行の間（`before` の行の上。None は最後の行の下）
    Before(Option<Uuid>),
    /// 一覧のフォルダの行の中（そのフォルダの末尾）
    Into(Uuid),
    /// ツリーのピン留めのフォルダ（None はルート）の末尾
    Tree(Option<Uuid>),
}

/// ドラッグ中の一覧の行・ツリーのピン留めのフォルダ。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RowDrag {
    target: RowTarget,
    /// ピン留めの行・フォルダが今いるフォルダ（None はルート）。ツリーのここへは落とせない（落としても動かない）。
    /// 履歴の行では使わない（None）
    parent: Option<Uuid>,
    /// 一覧の上に落とせるときの、一覧に出しているピン留めのフォルダ（内の None はルート）。行の間へ落としたときの
    /// 入れる先。外の None は一覧の上に落とせない（履歴の行、検索中・自分の中を出しているときのツリーのフォルダ）
    list_folder: Option<Option<Uuid>>,
    /// 今の落とす先（None は落とせない所）
    mark: Option<DropMark>,
}

impl RowDrag {
    /// 落とす先への操作。ピン留めの行・フォルダは移動（`RowCommand::Place`）、履歴の行はツリーのピン留めへの
    /// ピン留め（`RowCommand::Pin`）。一覧の上の落とす先は、一覧に落とせないドラッグには None。
    fn command(&self, mark: DropMark) -> Option<RowCommand> {
        match (self.target.pinned, mark) {
            (true, DropMark::Before(before)) => self.list_folder.map(|to| RowCommand::Place { to, before }),
            (true, DropMark::Into(folder)) => {
                self.list_folder.map(|_| RowCommand::Place { to: Some(folder), before: None })
            }
            (true, DropMark::Tree(folder)) => Some(RowCommand::Place { to: folder, before: None }),
            (false, DropMark::Tree(folder)) => Some(RowCommand::Pin(folder)),
            (false, DropMark::Before(_) | DropMark::Into(_)) => None,
        }
    }
}

/// 一覧の上の位置から落とす先を決める。`hit` は行の添字と、行の上端からの距離・行の高さ（None は最後の行より
/// 下）。フォルダの行は上下の 1/4 を行の間、残りの中央をそのフォルダの中とし、項目の行（とドラッグしている
/// フォルダ自身）は上半分を前の行の間、下半分を後ろの行の間とする。落としても動かない所（ドラッグしている行の
/// すぐ上・すぐ下の間）は None。
fn list_drop_mark(rows: &[Row], dragged: Uuid, hit: Option<(usize, i32, i32)>) -> Option<DropMark> {
    let gap_before = |i: usize| DropMark::Before(rows.get(i).map(|r| r.id));
    let mark = match hit {
        None => DropMark::Before(None),
        Some((i, y, height)) => {
            let row = rows.get(i)?;
            if row.folder.is_some() && row.id != dragged {
                let band = height / 4;
                if y < band {
                    gap_before(i)
                } else if y >= height - band {
                    gap_before(i + 1)
                } else {
                    DropMark::Into(row.id)
                }
            } else if y < height / 2 {
                gap_before(i)
            } else {
                gap_before(i + 1)
            }
        }
    };
    let at = rows.iter().position(|r| r.id == dragged);
    let unchanged = at.is_some_and(|d| mark == gap_before(d) || mark == gap_before(d + 1));
    (!unchanged).then_some(mark)
}

/// ツリーの `point`（ツリーのクライアント座標）の項目へ落とす先（ピン留めのルート・フォルダ）。ピン留めでない
/// 項目は None。ピン留めの行・フォルダでは、ドラッグしているフォルダ自身とその中、今いるフォルダも None
/// （履歴の行はどのピン留めへも入れられる）。
unsafe fn tree_drop_mark(ctx: &WindowCtx, tree: HWND, point: POINT, drag: &RowDrag) -> Option<DropMark> {
    let item = unsafe { tree_item_at_point(tree, point) };
    if item.0 == 0 {
        return None;
    }
    let Some(Source::Pinned(folder)) = (unsafe { tree_item_source(ctx, tree, item) }) else {
        return None;
    };
    if drag.target.pinned && folder == drag.parent {
        return None;
    }
    if drag.target.folder && unsafe { tree_item_within(ctx, tree, item, drag.target.id) } {
        return None;
    }
    Some(DropMark::Tree(folder))
}

/// ツリーの項目の親の項目（無ければ 0）。
unsafe fn tree_parent_item(tree: HWND, item: HTREEITEM) -> HTREEITEM {
    HTREEITEM(unsafe { SendMessageW(tree, TVM_GETNEXTITEM, Some(WPARAM(TVGN_PARENT as usize)), Some(LPARAM(item.0))).0 })
}

/// ツリーの項目が、ピン留めのフォルダ `folder` 自身かその中か（項目から根へ辿って探す）。
unsafe fn tree_item_within(ctx: &WindowCtx, tree: HWND, item: HTREEITEM, folder: Uuid) -> bool {
    let wanted = Source::Pinned(Some(folder));
    let mut at = item;
    while at.0 != 0 {
        if unsafe { tree_item_source(ctx, tree, at) } == Some(wanted) {
            return true;
        }
        at = unsafe { tree_parent_item(tree, at) };
    }
    false
}

/// 窓のクライアント座標 `point` が `child` の中にあれば、`child` のクライアント座標を返す。
unsafe fn point_in_child(hwnd: HWND, child: HWND, point: POINT) -> Option<POINT> {
    let mut points = [point];
    unsafe {
        MapWindowPoints(Some(hwnd), Some(child), &mut points);
    }
    let mut rc = RECT::default();
    unsafe { GetClientRect(child, &mut rc) }.ok()?;
    let p = points[0];
    (p.x >= rc.left && p.x < rc.right && p.y >= rc.top && p.y < rc.bottom).then_some(p)
}

/// 窓のクライアント座標 `point` で落とす先（一覧の上なら行の間・フォルダの行、ツリーの上ならピン留めの項目）。
/// 一覧の上は、一覧に落とせるドラッグ（`RowDrag::list_folder`）で、一覧がまだそのフォルダを出しているときだけ
/// （`list_still_droppable`）。
unsafe fn drop_mark_at(hwnd: HWND, ctx: &WindowCtx, drag: &RowDrag, point: POINT) -> Option<DropMark> {
    unsafe {
        if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
            if let Some(p) = point_in_child(hwnd, list, point) {
                if !list_still_droppable(hwnd, ctx, drag) {
                    return None;
                }
                let mut hit = LVHITTESTINFO { pt: p, ..Default::default() };
                let index = SendMessageW(list, LVM_HITTEST, Some(WPARAM(0)), Some(LPARAM(&mut hit as *mut _ as isize))).0;
                let hit = match usize::try_from(index) {
                    Ok(index) => {
                        let mut rc = RECT { left: LVIR_BOUNDS as i32, ..Default::default() };
                        SendMessageW(list, LVM_GETITEMRECT, Some(WPARAM(index)), Some(LPARAM(&mut rc as *mut _ as isize)));
                        Some((index, p.y - rc.top, (rc.bottom - rc.top).max(1)))
                    }
                    Err(_) => None,
                };
                return list_drop_mark(&ctx.view.borrow().rows, drag.target.id, hit);
            }
        }
        if let Ok(tree) = GetDlgItem(Some(hwnd), ID_TREE) {
            if let Some(p) = point_in_child(hwnd, tree, point) {
                return tree_drop_mark(ctx, tree, p, drag);
            }
        }
    }
    None
}

/// 一覧の上に落とせるか: 一覧に落とせるドラッグ（`RowDrag::list_folder`）で、ツリーが今もそのフォルダを選んでいて、
/// ハンドラが今もドラッグを許すとき（検索を始めていない）だけ。ドラッグの間にツリーの選択が変わる（キー操作・
/// 表示中のフォルダが消えた後の作り直し）・検索を始めると、一覧が始めたときと別のものを出すので、一覧の上の
/// 位置と、行の間へ落としたときの入れる先（始めたときのフォルダ）がずれる。
unsafe fn list_still_droppable(hwnd: HWND, ctx: &WindowCtx, drag: &RowDrag) -> bool {
    let Some(folder) = drag.list_folder else {
        return false;
    };
    let Ok(tree) = (unsafe { GetDlgItem(Some(hwnd), ID_TREE) }) else {
        return false;
    };
    if unsafe { selected_tree_source(ctx, tree) } != Some(Source::Pinned(folder)) {
        return false;
    }
    let handler = Rc::clone(&ctx.handler);
    handler.can_drag_row(drag.target)
}

/// 落とす先の目印を出し直す（`on` なら出し、そうでなければ消す）。一覧の行は描き直す（描画が `row_drag` の目印を
/// 読むので、先に `row_drag` を変えておく）。ツリーは項目を強調する（`TVGN_DROPHILITE`）。
unsafe fn refresh_drop_mark(hwnd: HWND, ctx: &WindowCtx, mark: Option<DropMark>, on: bool) {
    let Some(mark) = mark else {
        return;
    };
    unsafe {
        match mark {
            DropMark::Before(_) | DropMark::Into(_) => {
                let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) else {
                    return;
                };
                let index = {
                    let view = ctx.view.borrow();
                    match mark {
                        DropMark::Before(Some(id)) | DropMark::Into(id) => crate::native::model::index_of(&view.rows, id),
                        _ => view.rows.len().checked_sub(1),
                    }
                };
                if let Some(i) = index {
                    SendMessageW(list, LVM_REDRAWITEMS, Some(WPARAM(i)), Some(LPARAM(i as isize)));
                    let _ = UpdateWindow(list);
                }
            }
            DropMark::Tree(folder) => {
                let Ok(tree) = GetDlgItem(Some(hwnd), ID_TREE) else {
                    return;
                };
                let item = if on { find_tree_item(ctx, tree, Source::Pinned(folder)).map_or(0, |i| i.0) } else { 0 };
                SendMessageW(tree, TVM_SELECTITEM, Some(WPARAM(TVGN_DROPHILITE as usize)), Some(LPARAM(item)));
            }
        }
    }
}

/// ドラッグ中のカーソル（落とせる所は矢印、落とせない所は禁止の印）。マウスを捕まえている間は `WM_SETCURSOR`
/// が来ないので、動くたびに合わせる。
fn set_drag_cursor(droppable: bool) {
    unsafe {
        if let Ok(cursor) = LoadCursorW(None, if droppable { IDC_ARROW } else { IDC_NO }) {
            SetCursor(Some(cursor));
        }
    }
}

/// 一覧の行のドラッグを始める（`LVN_BEGINDRAG`）。ドラッグできる行（ハンドラが決める）だけで、始めない場合は
/// `can_begin_drag`。ピン留めの行は、一覧に出しているフォルダ（今いるフォルダ）をツリーで選んでいる項目から
/// 決める（ピン留めでなければ始めない）。履歴の行は一覧の上には落とせない。始めたら窓がマウスを捕まえる
/// （`start_drag`。境目のドラッグと同じ）。
fn begin_row_drag(hwnd: HWND, ctx: &WindowCtx, index: i32) {
    if !can_begin_drag(ctx) {
        return;
    }
    let Some((target, look)) = usize::try_from(index).ok().and_then(|i| {
        let view = ctx.view.borrow();
        view.rows.get(i).map(|row| {
            let look = DragLook { icon: ctx.row_icon(row), thumb: row.thumb.as_ref().map(|_| row.id), label: row.label.clone() };
            (row.target(), look)
        })
    }) else {
        return;
    };
    let folder = if target.pinned {
        let Ok(tree) = (unsafe { GetDlgItem(Some(hwnd), ID_TREE) }) else {
            return;
        };
        let Some(Source::Pinned(folder)) = (unsafe { selected_tree_source(ctx, tree) }) else {
            return;
        };
        folder
    } else {
        None
    };
    let handler = Rc::clone(&ctx.handler);
    if !handler.can_drag_row(target) {
        return;
    }
    let list_folder = target.pinned.then_some(folder);
    start_drag(hwnd, ctx, RowDrag { target, parent: folder, list_folder, mark: None }, &look);
}

/// ツリーのピン留めのフォルダのドラッグを始める（`TVN_BEGINDRAG`）。ルートの「ピン留め」・履歴の項目は
/// ドラッグしない。始めない場合は一覧の行と同じ（`can_begin_drag`）。今いるフォルダはツリーの親の項目から
/// 決める。一覧の上に落とせるのは、一覧にピン留めのフォルダを出していて、行を動かせるとき（ハンドラが決める。
/// 検索中は動かせない）で、それがドラッグしているフォルダ自身でもその中でもないときだけ。
fn begin_tree_drag(hwnd: HWND, ctx: &WindowCtx, item: HTREEITEM) {
    if !can_begin_drag(ctx) {
        return;
    }
    let Ok(tree) = (unsafe { GetDlgItem(Some(hwnd), ID_TREE) }) else {
        return;
    };
    let Some(Source::Pinned(Some(id))) = (unsafe { tree_item_source(ctx, tree, item) }) else {
        return;
    };
    let Some(Source::Pinned(parent)) = (unsafe { tree_item_source(ctx, tree, tree_parent_item(tree, item)) }) else {
        return;
    };
    let target = RowTarget { id, pinned: true, folder: true };
    let handler = Rc::clone(&ctx.handler);
    let selected = HTREEITEM(unsafe { SendMessageW(tree, TVM_GETNEXTITEM, Some(WPARAM(TVGN_CARET as usize)), None).0 });
    let list_folder = match unsafe { tree_item_source(ctx, tree, selected) } {
        Some(Source::Pinned(folder))
            if handler.can_drag_row(target) && !unsafe { tree_item_within(ctx, tree, selected, id) } =>
        {
            Some(folder)
        }
        _ => None,
    };
    let look = DragLook { icon: ctx.icons[4], thumb: None, label: unsafe { tree_item_text(tree, item) } };
    start_drag(hwnd, ctx, RowDrag { target, parent, list_folder, mark: None }, &look);
}

/// ツリーの項目の文字（取れなければ空）。
unsafe fn tree_item_text(tree: HWND, item: HTREEITEM) -> String {
    let mut buf = [0u16; 512];
    let mut tv = TVITEMW {
        mask: TVIF_HANDLE | TVIF_TEXT,
        hItem: item,
        pszText: PWSTR(buf.as_mut_ptr()),
        cchTextMax: buf.len() as i32,
        ..Default::default()
    };
    unsafe {
        SendMessageW(tree, TVM_GETITEMW, None, Some(LPARAM(&mut tv as *mut _ as isize)));
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// ドラッグを始めてよいか（名前の編集中・メニューやダイアログの表示中・境目や行のドラッグ中・右ボタンでの
/// 取り消しの後の右ボタンを離す待ちの間は始めない）。
fn can_begin_drag(ctx: &WindowCtx) -> bool {
    can_open_dialog(ctx) && ctx.drag.get().is_none() && ctx.row_drag.get().is_none() && !ctx.row_drag_right_up.get()
}

/// ドラッグを始める: 状態を置いて、窓がマウスを捕まえ（離すまで `WM_MOUSEMOVE`・`WM_LBUTTONUP` を受け取る）、
/// 行の絵を出す。
fn start_drag(hwnd: HWND, ctx: &WindowCtx, drag: RowDrag, look: &DragLook) {
    ctx.row_drag.set(Some(drag));
    unsafe {
        SetCapture(hwnd);
    }
    set_drag_cursor(false);
    let mut cursor = POINT::default();
    if unsafe { GetCursorPos(&mut cursor) }.is_ok() {
        show_drag_image(hwnd, ctx, look, cursor);
    }
}

/// ドラッグ中の行の絵の文字の幅の上限と、カーソルから右へずらす量（96 DPI 基準の px）。
const DRAG_LABEL_MAX: i32 = 240;
const DRAG_IMAGE_OFFSET_X: i32 = 16;

/// ドラッグ中の行の絵の中身: アイコン、サムネイルを持つ項目（読み込んであれば、アイコンの代わりに描く）、名前。
struct DragLook {
    icon: HICON,
    thumb: Option<Uuid>,
    label: String,
}

/// ドラッグ中の行の絵を描く: 選んだ行と同じ色の地に、アイコン（`look.thumb` のサムネイルを読み込んであれば
/// それ）と太字の名前を1行で。寸法は一覧の行と同じ（`Metrics`）で、名前の幅は `DRAG_LABEL_MAX` まで（長ければ
/// 末尾を省く）。ビットマップとその大きさを返す。借用はこの中だけ。
unsafe fn render_drag_image(hwnd: HWND, ctx: &WindowCtx, look: &DragLook) -> Option<(HBITMAP, i32, i32)> {
    let view = ctx.view.borrow();
    let m = ctx.metrics.borrow();
    let (icon, pad) = (m.icon_size, m.pad);
    unsafe {
        let max_label = crate::menu_tooltip::scale_for_dpi(DRAG_LABEL_MAX, GetDpiForWindow(hwnd));
        let screen = GetDC(Some(hwnd));
        let mem = CreateCompatibleDC(Some(screen));
        let old_font = SelectObject(mem, m.bold_font.into());
        let mut label: Vec<u16> = look.label.encode_utf16().collect();
        let label_width = if label.is_empty() {
            0
        } else {
            let mut rc = RECT::default();
            DrawTextW(mem, &mut label, &mut rc, DT_SINGLELINE | DT_NOPREFIX | DT_CALCRECT);
            (rc.right - rc.left).min(max_label)
        };
        let width = pad * 2 + icon + if label_width > 0 { pad + label_width } else { 0 };
        let height = icon + pad * 2;
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        ReleaseDC(Some(hwnd), screen);
        if bitmap.is_invalid() {
            SelectObject(mem, old_font);
            let _ = DeleteDC(mem);
            return None;
        }
        let old_bitmap = SelectObject(mem, bitmap.into());
        FillRect(mem, &RECT { left: 0, top: 0, right: width, bottom: height }, GetSysColorBrush(COLOR_HIGHLIGHT));
        match look.thumb.and_then(|id| view.thumbs.get(&id)) {
            // サムネイルは長辺 icon に収めてあるので、アイコンの枠の中央に原寸で描く（一覧の行と同じ）
            Some(Some(bmp)) => bmp.draw(mem, pad + (icon - bmp.width) / 2, pad + (icon - bmp.height) / 2, bmp.width, bmp.height),
            _ => {
                let _ = DrawIconEx(mem, pad, pad, look.icon, icon, icon, 0, None, DI_NORMAL);
            }
        }
        SetBkMode(mem, TRANSPARENT);
        SetTextColor(mem, windows::Win32::Foundation::COLORREF(GetSysColor(COLOR_HIGHLIGHTTEXT)));
        let mut rc = RECT { left: pad * 2 + icon, top: 0, right: width - pad, bottom: height };
        draw_text(mem, &look.label, &mut rc, DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX | DT_VCENTER);
        SelectObject(mem, old_bitmap);
        SelectObject(mem, old_font);
        let _ = DeleteDC(mem);
        Some((bitmap, width, height))
    }
}

/// 行の絵の左上（画面座標）: カーソルの右へ `DRAG_IMAGE_OFFSET_X`、下へ行の高さの半分と余白。一覧の行の間の線は
/// カーソルから行の高さの半分（と線の太さ）までの所に出るので、絵がその線に重ならないようにする。
fn drag_image_origin(hwnd: HWND, ctx: &WindowCtx, cursor: POINT) -> (i32, i32) {
    let m = ctx.metrics.borrow();
    let dx = crate::menu_tooltip::scale_for_dpi(DRAG_IMAGE_OFFSET_X, unsafe { GetDpiForWindow(hwnd) });
    (cursor.x + dx, cursor.y + m.row_height / 2 + m.pad)
}

/// 行の絵を、カーソル（画面座標）の右下に出す。作れなければ出さずにドラッグを続ける。
fn show_drag_image(hwnd: HWND, ctx: &WindowCtx, look: &DragLook, cursor: POINT) {
    hide_drag_image(ctx);
    let Some((bitmap, width, height)) = (unsafe { render_drag_image(hwnd, ctx, look) }) else {
        return;
    };
    let (x, y) = drag_image_origin(hwnd, ctx, cursor);
    if let Some(image) = crate::native::drag_image::show(hwnd, bitmap, width, height, x, y) {
        ctx.drag_image.set(image.0 as isize);
    }
}

/// 行の絵を、カーソル（窓のクライアント座標）に合わせて動かす。
fn move_drag_image(hwnd: HWND, ctx: &WindowCtx, x: i32, y: i32) {
    let image = ctx.drag_image.get();
    if image == 0 {
        return;
    }
    let mut cursor = POINT { x, y };
    unsafe {
        let _ = ClientToScreen(hwnd, &mut cursor);
    }
    let (x, y) = drag_image_origin(hwnd, ctx, cursor);
    crate::native::drag_image::move_to(HWND(image as *mut _), x, y);
}

/// 行の絵を消す。出していなければ何もしない（ドラッグの終わり方が重なっても、破棄は1回だけ）。
fn hide_drag_image(ctx: &WindowCtx) {
    let image = ctx.drag_image.replace(0);
    if image != 0 {
        crate::native::drag_image::destroy(HWND(image as *mut _));
    }
}

/// ドラッグ中にマウスが動いた（窓のクライアント座標）: 落とす先を決め直し、変わったら目印を出し直す。ツリーの
/// 強調は変わらなくても付け直す（ドラッグの間に起床でツリーが作り直されると消えるため）。
fn drag_row_to(hwnd: HWND, ctx: &WindowCtx, x: i32, y: i32) {
    let Some(drag) = ctx.row_drag.get() else {
        return;
    };
    move_drag_image(hwnd, ctx, x, y);
    let mark = unsafe { drop_mark_at(hwnd, ctx, &drag, POINT { x, y }) };
    if mark != drag.mark {
        ctx.row_drag.set(Some(RowDrag { mark, ..drag }));
        unsafe {
            refresh_drop_mark(hwnd, ctx, drag.mark, false);
            refresh_drop_mark(hwnd, ctx, mark, true);
        }
    } else if matches!(mark, Some(DropMark::Tree(_))) {
        unsafe { refresh_drop_mark(hwnd, ctx, mark, true) };
    }
    set_drag_cursor(mark.is_some());
}

/// ドラッグ中にマウスを離した（窓のクライアント座標）: 目印を消してマウスを放し、落とせる所ならハンドラへ移動・
/// ピン留めを伝える（落とす先は離した位置で決め直す）。
fn finish_row_drag(hwnd: HWND, ctx: &WindowCtx, x: i32, y: i32) {
    let Some(drag) = ctx.row_drag.get() else {
        return;
    };
    let mark = unsafe { drop_mark_at(hwnd, ctx, &drag, POINT { x, y }) };
    // 印を先に下ろす（放すと WM_CAPTURECHANGED が来る）。行の絵はハンドラへ伝える前に消す
    ctx.row_drag.set(None);
    hide_drag_image(ctx);
    unsafe {
        refresh_drop_mark(hwnd, ctx, drag.mark, false);
        if GetCapture() == hwnd {
            let _ = ReleaseCapture();
        }
    }
    if let Some(command) = mark.and_then(|mark| drag.command(mark)) {
        let handler = Rc::clone(&ctx.handler);
        handler.on_row_command(hwnd, drag.target, command);
    }
}

/// ドラッグを取り消す（Esc・マウスを失った・隠す・終了）。目印と行の絵を消し、まだ捕まえていればマウスを放す（右
/// ボタンでの取り消しの後、右ボタンを離すのを待っている間も放す）。どちらでもなければ何もしない。
fn cancel_row_drag(hwnd: HWND, ctx: &WindowCtx) {
    let drag = ctx.row_drag.take();
    let waiting = ctx.row_drag_right_up.replace(false);
    hide_drag_image(ctx);
    if drag.is_none() && !waiting {
        return;
    }
    unsafe {
        if let Some(drag) = drag {
            refresh_drop_mark(hwnd, ctx, drag.mark, false);
        }
        if GetCapture() == hwnd {
            let _ = ReleaseCapture();
        }
    }
}

/// ドラッグ中に右ボタンを押した: ドラッグを取り消して目印と行の絵を消す。マウスは右ボタンを離すまで捕まえたままに
/// し、離す操作は窓が受け取って捨てる（`WM_RBUTTONUP`）。すぐに放すと、離す操作が下の一覧・ツリーへ届き、右クリック
/// のメニューが出うるため。ドラッグしていなければ何もしない（false）。
fn cancel_row_drag_by_right_button(hwnd: HWND, ctx: &WindowCtx) -> bool {
    let Some(drag) = ctx.row_drag.take() else {
        return false;
    };
    ctx.row_drag_right_up.set(true);
    hide_drag_image(ctx);
    unsafe {
        refresh_drop_mark(hwnd, ctx, drag.mark, false);
    }
    set_drag_cursor(true);
    true
}

/// 右クリックメニューのコマンド ID。テキスト変換は `MENU_TRANSFORM_BASE` + `TextTransform::ALL` の添字、
/// ピン留めの入れる先（「ピン留めに追加」「移動」の子メニュー）は `MENU_TARGET_BASE` + `RowMenu::pin_targets`
/// の添字（テキスト変換の範囲と重ならない）。
const MENU_SEND: usize = 1;
const MENU_PIN: usize = 2;
const MENU_OPEN_IMAGE: usize = 3;
const MENU_DELETE: usize = 4;
const MENU_RENAME: usize = 5;
const MENU_OPEN_IMAGE_LOCATION: usize = 6;
const MENU_MOVE_UP: usize = 7;
const MENU_MOVE_DOWN: usize = 8;
const MENU_TRANSFORM_BASE: usize = 100;
const MENU_TARGET_BASE: usize = 1000;
/// `TrackPopupMenu` が返すコマンド ID は 16 ビットに収める
const MENU_TARGET_MAX: usize = u16::MAX as usize;

/// 入れる先の子メニューに、`pin_targets` をツリーの線（`PinTarget::guide`。自前描画で行の高さいっぱいに
/// 描く）を付けて並べる（名前の `&` は `&&` にする）。`current` の所は灰色。ID が 16 ビットを超える分は出さない。
fn append_pin_targets(menu: &mut PopupMenu, sub: HMENU, spec: &RowMenu, current: Option<Option<Uuid>>) {
    for (i, target) in spec.pin_targets.iter().enumerate() {
        let Some(id) = MENU_TARGET_BASE.checked_add(i).filter(|id| *id <= MENU_TARGET_MAX) else {
            break;
        };
        let text = menu_draw::escape_menu_text(&target.title);
        let flags = if current == Some(target.folder) { MF_STRING | MF_GRAYED } else { MF_STRING };
        menu.append_guided_command(sub, id, &text, flags, &target.guide);
    }
}

/// 「上へ」「下へ」を足す（`spec.reorder` があるときだけ。先頭・末尾で選べない方は灰色）。
fn append_reorder(menu: &mut PopupMenu, root: HMENU, spec: &RowMenu) {
    if let Some(reorder) = spec.reorder {
        let flags = |enabled: bool| if enabled { MF_STRING } else { MF_STRING | MF_GRAYED };
        menu.append_command(root, MENU_MOVE_UP, "上へ", flags(reorder.up), None);
        menu.append_command(root, MENU_MOVE_DOWN, "下へ", flags(reorder.down), None);
    }
}

/// 右クリックメニューを作る（自前描画で、`dpi` はメニューを出す位置のモニターの DPI）。
/// フォルダがあれば、履歴の行の「ピン留めに追加」は入れる先の子メニューにし、ピン留めの行には「移動」の
/// 子メニューを出す。フォルダの行は「開く」・「移動」（ほかに入れる先のフォルダがあるとき）・並べ替え・
/// 「名前の変更...」・「削除...」だけ。破棄は戻り値の `PopupMenu` の破棄（子メニューごと）。
fn build_row_menu(spec: &RowMenu, dpi: u32) -> WinResult<PopupMenu> {
    let mut menu = PopupMenu::new(dpi, None)?;
    let root = menu.handle();
    let has_folders = spec.pin_targets.len() > 1;
    let append_move = |menu: &mut PopupMenu| {
        if let (Some(current), true) = (spec.current, has_folders) {
            if let Some(sub) = menu.add_submenu("移動", None) {
                append_pin_targets(menu, sub, spec, Some(current));
            }
        }
    };
    if spec.folder {
        menu.append_command(root, MENU_SEND, "開く", MF_STRING, None);
        append_move(&mut menu);
        append_reorder(&mut menu, root, spec);
        menu.append_command(root, MENU_RENAME, "名前の変更...", MF_STRING, None);
        menu.append_command(root, MENU_DELETE, "削除...", MF_STRING, None);
        return Ok(menu);
    }
    menu.append_command(root, MENU_SEND, "クリップボードへ送る", MF_STRING, None);
    if spec.can_pin {
        if has_folders {
            if let Some(sub) = menu.add_submenu("ピン留めに追加", None) {
                append_pin_targets(&mut menu, sub, spec, None);
            }
        } else {
            menu.append_command(root, MENU_PIN, "ピン留めに追加", MF_STRING, None);
        }
    }
    append_move(&mut menu);
    append_reorder(&mut menu, root, spec);
    // ピン留めの行の名前の変更（名前を聞くダイアログを出す）
    if spec.current.is_some() {
        menu.append_command(root, MENU_RENAME, "名前の変更...", MF_STRING, None);
    }
    if spec.has_image {
        menu.append_command(root, MENU_OPEN_IMAGE, "画像を関連付けで開く", MF_STRING, None);
        // 一時フォルダに書き出した写しを選んだ状態でフォルダを開く
        menu.append_command(root, MENU_OPEN_IMAGE_LOCATION, "画像を書き出してフォルダで表示", MF_STRING, None);
    }
    if spec.has_text {
        if let Some(sub) = menu.add_submenu("テキスト変換", None) {
            for (i, transform) in TextTransform::ALL.iter().enumerate() {
                menu.append_command(sub, MENU_TRANSFORM_BASE + i, transform.label(), MF_STRING, None);
            }
        }
    }
    menu.append_command(root, MENU_DELETE, "削除", MF_STRING, None);
    Ok(menu)
}

/// 右クリックメニューのコマンド ID から操作へ（0・知らない ID・灰色の「上へ」「下へ」は None）。入れる先の
/// ID は、メニューを作ったときと同じ `spec` の `pin_targets` で引く（ピン留めの行なら移動、履歴の行ならピン留め）。
fn row_command(id: usize, spec: &RowMenu) -> Option<RowCommand> {
    match id {
        MENU_SEND => Some(RowCommand::Send),
        MENU_PIN => Some(RowCommand::Pin(None)),
        MENU_OPEN_IMAGE => Some(RowCommand::OpenImage),
        MENU_OPEN_IMAGE_LOCATION => Some(RowCommand::OpenImageLocation),
        MENU_DELETE => Some(RowCommand::Delete),
        MENU_RENAME if spec.current.is_some() || spec.folder => Some(RowCommand::Rename),
        MENU_MOVE_UP if spec.reorder.is_some_and(|r| r.up) => Some(RowCommand::Reorder(Direction::Up)),
        MENU_MOVE_DOWN if spec.reorder.is_some_and(|r| r.down) => Some(RowCommand::Reorder(Direction::Down)),
        _ if id >= MENU_TARGET_BASE => {
            let target = spec.pin_targets.get(id - MENU_TARGET_BASE)?;
            Some(if spec.current.is_some() { RowCommand::Move(target.folder) } else { RowCommand::Pin(target.folder) })
        }
        _ => id
            .checked_sub(MENU_TRANSFORM_BASE)
            .and_then(|i| TextTransform::ALL.get(i))
            .map(|t| RowCommand::Transform(*t)),
    }
}

/// `menu_open` を、スコープを抜ける（早期 return を含む）ときに必ず下ろす（hotkey・tray と同じ）。
struct BusyGuard<'a>(&'a Cell<bool>);

impl<'a> BusyGuard<'a> {
    fn enter(flag: &'a Cell<bool>) -> Self {
        flag.set(true);
        Self(flag)
    }
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// 一覧の `WM_CONTEXTMENU`（右クリック・Shift+F10・アプリケーションキー）で、行の右クリック
/// メニューを出す。マウスなら右クリックした行を先に選び、行の無い所ならメニューを
/// 出さない。キーボード（座標が -1, -1）なら選んでいる先頭の行の位置に出す。
/// 選ばれた操作は、メニューが閉じた後にハンドラへ伝える（`TPM_RETURNCMD`）。メニューの
/// モーダルループの間は `menu_open` を立て、`ViewState` の借用を持たない（モーダルの間は
/// 窓のプロシージャが再入するため）。
unsafe fn show_row_menu(hwnd: HWND, ctx: &WindowCtx, list: HWND, lparam: LPARAM) {
    unsafe {
        // ツリーの名前の編集の後始末が済むまでは出さない（一覧の右クリックでフォーカスが移って
        // 編集が終わった直後など）。確認ダイアログの表示中も出さない（ツリーの側と同じ）
        if ctx.menu_open.get() || dialog_active(ctx) || ctx.edit.get() != EditState::None {
            return;
        }
        let x = (lparam.0 & 0xFFFF) as u16 as i16 as i32;
        let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
        let (index, point) = if x == -1 && y == -1 {
            let Some(index) = selected_index(list) else {
                return;
            };
            SendMessageW(list, LVM_ENSUREVISIBLE, Some(WPARAM(index)), Some(LPARAM(0)));
            let mut rc = RECT { left: LVIR_BOUNDS as i32, ..Default::default() };
            SendMessageW(list, LVM_GETITEMRECT, Some(WPARAM(index)), Some(LPARAM(&mut rc as *mut _ as isize)));
            let pad = ctx.metrics.borrow().pad;
            let mut point = POINT { x: rc.left + pad, y: rc.bottom };
            let _ = ClientToScreen(list, &mut point);
            (index, point)
        } else {
            let mut hit = LVHITTESTINFO { pt: POINT { x, y }, ..Default::default() };
            let _ = ScreenToClient(list, &mut hit.pt);
            let index = SendMessageW(list, LVM_HITTEST, Some(WPARAM(0)), Some(LPARAM(&mut hit as *mut _ as isize))).0;
            if index < 0 {
                return;
            }
            let index = index as usize;
            // 右クリックした行だけを選ぶ（選択の変化はいつもどおりハンドラへ伝わる）
            set_item_state(list, -1, 0);
            set_item_state(list, index as i32, LVIS_SELECTED.0 | LVIS_FOCUSED.0);
            (index, POINT { x, y })
        };
        let Some(target) = ctx.view.borrow().rows.get(index).map(Row::target) else {
            return;
        };
        let handler = Rc::clone(&ctx.handler);
        let Some(spec) = handler.row_menu(target) else {
            return;
        };
        // メニュー全体を、出す位置のモニターの DPI で描く（窓の DPI とは違うことがある）
        let Ok(menu) = build_row_menu(&spec, crate::menu_tooltip::dpi_at_point(point.x, point.y)) else {
            return;
        };
        let command = {
            let _busy = BusyGuard::enter(&ctx.menu_open);
            TrackPopupMenu(menu.handle(), TPM_RETURNCMD | TPM_RIGHTBUTTON, point.x, point.y, Some(0), hwnd, None)
        };
        // メニュー（子メニューごと）を破棄してから、項目の描画の材料を解放する
        drop(menu);
        // メニューの表示中に保留した処理（失敗の通知など）を、閉じた後の起床で行わせる
        let _ = PostMessageW(Some(hwnd), WM_APP_WAKE, WPARAM(0), LPARAM(0));
        match row_command(command.0 as usize, &spec) {
            // 名前はメニューが閉じた後にダイアログで聞く（`menu_open` は下りている）
            Some(RowCommand::Rename) => rename_pinned_row(hwnd, ctx, target),
            // フォルダの行の「開く」と、確認してからの削除
            Some(RowCommand::Send) if target.folder => activate_row(hwnd, ctx, target),
            Some(RowCommand::Delete) if target.folder => delete_row(hwnd, ctx, target),
            Some(command) => handler.on_row_command(hwnd, target, command),
            None => {}
        }
    }
}

/// ツリーの右クリックメニューのコマンド ID。
const TREE_MENU_CLEAR_HISTORY: usize = 1;
const TREE_MENU_DELETE_FOLDER: usize = 2;
const TREE_MENU_CREATE_FOLDER: usize = 3;
const TREE_MENU_RENAME_FOLDER: usize = 4;

/// ツリーの右クリックメニューを作る（自前描画。`dpi` はメニューを出す位置のモニターの DPI）。
fn build_tree_menu(spec: &TreeMenu, dpi: u32) -> WinResult<PopupMenu> {
    let mut menu = PopupMenu::new(dpi, None)?;
    let root = menu.handle();
    match spec {
        TreeMenu::History => menu.append_command(root, TREE_MENU_CLEAR_HISTORY, "履歴のクリア...", MF_STRING, None),
        TreeMenu::PinnedRoot => menu.append_command(root, TREE_MENU_CREATE_FOLDER, "フォルダの作成", MF_STRING, None),
        TreeMenu::PinnedFolder { .. } => {
            menu.append_command(root, TREE_MENU_CREATE_FOLDER, "フォルダの作成", MF_STRING, None);
            menu.append_command(root, TREE_MENU_RENAME_FOLDER, "名前の変更", MF_STRING, None);
            menu.append_separator(root);
            menu.append_command(root, TREE_MENU_DELETE_FOLDER, "削除...", MF_STRING, None);
        }
    }
    Ok(menu)
}

/// ツリーの項目の文字の部分の矩形（ツリーのクライアント座標）。TVM_GETITEMRECT は、渡す RECT の先頭に
/// 項目のハンドルを置く（RECT は 4 バイト境界なので、整列を仮定せずに書く）。取れなければ None（そのとき
/// RECT には矩形が入らない。Microsoft Learn の TVM_GETITEMRECT の説明）。
unsafe fn tree_item_rect(tree: HWND, item: HTREEITEM) -> Option<RECT> {
    let mut rc = RECT::default();
    let ok = unsafe {
        std::ptr::write_unaligned(&mut rc as *mut RECT as *mut isize, item.0);
        SendMessageW(tree, TVM_GETITEMRECT, Some(WPARAM(1)), Some(LPARAM(&mut rc as *mut _ as isize))).0
    };
    (ok != 0).then_some(rc)
}

/// ツリーの項目の表示対象（lParam に置いた `tree_tokens` の添字から引く）。
unsafe fn tree_item_source(ctx: &WindowCtx, tree: HWND, item: HTREEITEM) -> Option<Source> {
    let mut tv = TVITEMW { mask: TVIF_HANDLE | TVIF_PARAM, hItem: item, ..Default::default() };
    let ok = unsafe { SendMessageW(tree, TVM_GETITEMW, None, Some(LPARAM(&mut tv as *mut _ as isize))).0 };
    if ok == 0 {
        return None;
    }
    ctx.view.borrow().tree_tokens.get(tv.lParam.0 as usize).copied()
}

/// ツリーの `WM_CONTEXTMENU`（右クリック・Shift+F10・アプリケーションキー）で、項目の右クリック
/// メニューを出す。マウスなら右クリックした項目が対象で、選択
/// （表示元）は変えず、メニューの間だけその項目を強調する（C 版と同じ見え方。一覧を入れ替えない）。
/// 項目の無い所ならメニューを出さない。キーボード（座標が -1, -1）なら選んでいる項目が対象。
/// 削除を伴う操作は、メニューが閉じた後に確認を出し、「削除」のときだけハンドラへ伝える。
/// モーダルの扱いは `show_row_menu` と同じ。
unsafe fn show_tree_menu(hwnd: HWND, ctx: &WindowCtx, tree: HWND, lparam: LPARAM) {
    unsafe {
        if ctx.menu_open.get() || dialog_active(ctx) || ctx.edit.get() != EditState::None {
            return;
        }
        let x = (lparam.0 & 0xFFFF) as u16 as i16 as i32;
        let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
        let (item, point, by_mouse) = if x == -1 && y == -1 {
            let item =
                HTREEITEM(SendMessageW(tree, TVM_GETNEXTITEM, Some(WPARAM(TVGN_CARET as usize)), Some(LPARAM(0))).0);
            if item.0 == 0 {
                return;
            }
            SendMessageW(tree, TVM_ENSUREVISIBLE, None, Some(LPARAM(item.0)));
            let Some(rc) = tree_item_rect(tree, item) else {
                return;
            };
            let mut point = POINT { x: rc.left, y: rc.bottom };
            let _ = ClientToScreen(tree, &mut point);
            (item, point, false)
        } else {
            let mut hit = TVHITTESTINFO { pt: POINT { x, y }, ..Default::default() };
            let _ = ScreenToClient(tree, &mut hit.pt);
            let item = HTREEITEM(SendMessageW(tree, TVM_HITTEST, None, Some(LPARAM(&mut hit as *mut _ as isize))).0);
            if item.0 == 0 || (hit.flags.0 & TVHT_ONITEM.0) == 0 {
                return;
            }
            (item, POINT { x, y }, true)
        };
        let Some(source) = tree_item_source(ctx, tree, item) else {
            return;
        };
        let handler = Rc::clone(&ctx.handler);
        let Some(spec) = handler.tree_menu(source) else {
            return;
        };
        let Ok(menu) = build_tree_menu(&spec, crate::menu_tooltip::dpi_at_point(point.x, point.y)) else {
            return;
        };
        if by_mouse {
            // メニューの間にツリーが作り直されたら、`set_tree` がこの表示対象の新しい項目へ付け直す
            ctx.tree_hilite.set(Some(source));
            SendMessageW(tree, TVM_SELECTITEM, Some(WPARAM(TVGN_DROPHILITE as usize)), Some(LPARAM(item.0)));
        }
        let command = {
            let _busy = BusyGuard::enter(&ctx.menu_open);
            TrackPopupMenu(menu.handle(), TPM_RETURNCMD | TPM_RIGHTBUTTON, point.x, point.y, Some(0), hwnd, None)
        };
        // 強調を外す（今の項目がどれでも、外すだけなので古いハンドルは使わない）
        if by_mouse {
            ctx.tree_hilite.set(None);
            SendMessageW(tree, TVM_SELECTITEM, Some(WPARAM(TVGN_DROPHILITE as usize)), Some(LPARAM(0)));
        }
        drop(menu);
        // メニューの表示中に保留した処理（失敗の通知など）を、閉じた後の起床で行わせる
        let _ = PostMessageW(Some(hwnd), WM_APP_WAKE, WPARAM(0), LPARAM(0));
        match (command.0 as usize, &spec) {
            (TREE_MENU_CLEAR_HISTORY, TreeMenu::History) => {
                if confirm_clear_history(hwnd, ctx) {
                    handler.on_tool_command(hwnd, ToolCommand::ClearHistory);
                }
            }
            // 確認に出す名前・件数は、メニューを出したときの ID で、確認の直前に取り直す
            (TREE_MENU_DELETE_FOLDER, TreeMenu::PinnedFolder { id, .. }) => delete_folder_after_confirm(hwnd, ctx, *id),
            (TREE_MENU_CREATE_FOLDER, TreeMenu::PinnedRoot) => begin_edit(hwnd, ctx, EditTarget::Create { parent: None }),
            (TREE_MENU_CREATE_FOLDER, TreeMenu::PinnedFolder { id, .. }) => {
                begin_edit(hwnd, ctx, EditTarget::Create { parent: Some(*id) })
            }
            (TREE_MENU_RENAME_FOLDER, TreeMenu::PinnedFolder { id, .. }) => {
                begin_edit(hwnd, ctx, EditTarget::Rename { id: *id })
            }
            _ => {}
        }
    }
}

unsafe fn ctx_ref<'a>(hwnd: HWND) -> Option<&'a WindowCtx> {
    unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowCtx).as_ref() }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        // 終了の後片付け中（`begin_teardown`）は、アプリの処理を呼ばない。`WM_CLOSE` は既定の処理が
        // `DestroyWindow` を呼ぶので流さない。窓の破棄は下の今の処理へ通す
        if ctx_ref(hwnd).is_some_and(|ctx| ctx.teardown.get()) {
            match msg {
                WM_CLOSE => return LRESULT(0),
                WM_DESTROY | WM_NCDESTROY => {}
                _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
        match msg {
            WM_SIZE => {
                // 最小化・最大化していないときの大きさだけを覚える（保存は隠すとき・終了時）
                if wparam.0 as u32 == SIZE_RESTORED {
                    if let Some(ctx) = ctx_ref(hwnd) {
                        let dpi = GetDpiForWindow(hwnd);
                        let (w, h) = ((lparam.0 & 0xFFFF) as i32, ((lparam.0 >> 16) & 0xFFFF) as i32);
                        ctx.normal_size.set((unscale(w, dpi), unscale(h, dpi)));
                    }
                }
                layout(hwnd);
                LRESULT(0)
            }
            // 境目（子コントロールの間のすき間。窓自身のクライアント領域）の上なら矢印のカーソルにする
            WM_SETCURSOR if HWND(wparam.0 as *mut _) == hwnd && (lparam.0 & 0xFFFF) as u32 == HTCLIENT => {
                let mut pt = POINT::default();
                if GetCursorPos(&mut pt).is_ok() && ScreenToClient(hwnd, &mut pt).as_bool() {
                    if let Some(splitter) = current_layout(hwnd).and_then(|l| splitter_at(&l, pt.x, pt.y)) {
                        set_splitter_cursor(splitter);
                        return LRESULT(1);
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            // 境目を押したらマウスを捕まえてドラッグを始める（子コントロールの上の押下はここへ来ない）
            WM_LBUTTONDOWN => {
                let (x, y) = mouse_point(lparam);
                if let (Some(ctx), Some(pane)) = (ctx_ref(hwnd), current_layout(hwnd)) {
                    let drag = match splitter_at(&pane, x, y) {
                        Some(Splitter::Vertical) => Some(Drag { splitter: Splitter::Vertical, offset: x - pane.vertical_bar.left }),
                        Some(Splitter::Horizontal) => {
                            Some(Drag { splitter: Splitter::Horizontal, offset: y - pane.horizontal_bar.top })
                        }
                        None => None,
                    };
                    if let Some(drag) = drag {
                        ctx.drag.set(Some(drag));
                        SetCapture(hwnd);
                        set_splitter_cursor(drag.splitter);
                        return LRESULT(0);
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_MOUSEMOVE => {
                if let Some(ctx) = ctx_ref(hwnd) {
                    let (x, y) = mouse_point(lparam);
                    if let Some(drag) = ctx.drag.get() {
                        set_splitter_cursor(drag.splitter);
                        drag_to(hwnd, ctx, drag, x, y);
                        return LRESULT(0);
                    }
                    if ctx.row_drag.get().is_some() {
                        drag_row_to(hwnd, ctx, x, y);
                        return LRESULT(0);
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            // 離したらマウスを放す（行のドラッグなら、その位置へ落とす）。捕まえが外れたら（放した・ほかの窓が
            // 捕まえた・隠れた）ドラッグを終える（行のドラッグは落とさずに取り消す）
            WM_LBUTTONUP => {
                if let Some(ctx) = ctx_ref(hwnd) {
                    if ctx.drag.take().is_some() {
                        let _ = ReleaseCapture();
                        return LRESULT(0);
                    }
                    if ctx.row_drag.get().is_some() {
                        let (x, y) = mouse_point(lparam);
                        finish_row_drag(hwnd, ctx, x, y);
                        return LRESULT(0);
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            // 行のドラッグ中の右ボタンは取り消し（マウスを捕まえているので、どこで押してもここへ来る）
            WM_RBUTTONDOWN => {
                if ctx_ref(hwnd).is_some_and(|ctx| cancel_row_drag_by_right_button(hwnd, ctx)) {
                    return LRESULT(0);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            // 右ボタンでの取り消しの後の離す操作は、既定の処理へ渡さずに捨て（渡すと WM_CONTEXTMENU になる）、
            // マウスを放す
            WM_RBUTTONUP => {
                if let Some(ctx) = ctx_ref(hwnd).filter(|ctx| ctx.row_drag_right_up.get()) {
                    cancel_row_drag(hwnd, ctx);
                    return LRESULT(0);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_CAPTURECHANGED => {
                if let Some(ctx) = ctx_ref(hwnd) {
                    ctx.drag.set(None);
                    cancel_row_drag(hwnd, ctx);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            // 最小の大きさ（クライアント領域で 400×300、窓の DPI で換算）
            WM_GETMINMAXINFO => {
                let info = &mut *(lparam.0 as *mut MINMAXINFO);
                let (w, h) = window_size_for_client(
                    (crate::config::VIEWER_MIN_WIDTH, crate::config::VIEWER_MIN_HEIGHT),
                    GetDpiForWindow(hwnd),
                );
                info.ptMinTrackSize = POINT { x: w, y: h };
                LRESULT(0)
            }
            // メニューバー・窓メニュー（Alt+Space）のモーダルループ（wParam が FALSE。TrackPopupMenu の
            // ときは TRUE で、その間は `show_row_menu` が `menu_open` を立てている。Microsoft Learn の
            // WM_ENTERMENULOOP の説明）。この間も隠す・終了の要求で閉じられるようにする
            WM_ENTERMENULOOP if wparam.0 == 0 => {
                if let Some(ctx) = ctx_ref(hwnd) {
                    ctx.menu_open.set(true);
                }
                LRESULT(0)
            }
            WM_EXITMENULOOP if wparam.0 == 0 => {
                if let Some(ctx) = ctx_ref(hwnd) {
                    ctx.menu_open.set(false);
                    // メニューの表示中に保留した処理（失敗の通知など）を、閉じた後の起床で行わせる
                    let _ = PostMessageW(Some(hwnd), WM_APP_WAKE, WPARAM(0), LPARAM(0));
                }
                LRESULT(0)
            }
            // 右クリックメニュー・メニューバーのドロップダウンの項目（自前描画。`ODT_MENU` のときだけ処理する。
            // メニューのモーダル中に再入して届く。項目のデータを共有参照で読むだけ）
            WM_MEASUREITEM if menu_draw::on_measure_item(hwnd, lparam) => LRESULT(1),
            WM_DRAWITEM if menu_draw::on_draw_item(lparam) => LRESULT(1),
            WM_MENUCHAR => match menu_draw::on_menu_char(wparam, lparam) {
                Some(result) => result,
                None => DefWindowProcW(hwnd, msg, wparam, lparam),
            },
            WM_MEASUREITEM => {
                let mis = &mut *(lparam.0 as *mut MEASUREITEMSTRUCT);
                if mis.CtlType == ODT_LISTVIEW {
                    if let Some(ctx) = ctx_ref(hwnd) {
                        mis.itemHeight = ctx.metrics.borrow().row_height as u32;
                        return LRESULT(1);
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            // 読み取り専用の EDIT はこのメッセージで色を問い合わせる。「（プレビューなし）」のときだけ、
            // 既定の処理（背景・ブラシ）の後で文字をシステムの薄い文字の色にする
            WM_CTLCOLORSTATIC => {
                let result = DefWindowProcW(hwnd, msg, wparam, lparam);
                let is_preview = GetDlgItem(Some(hwnd), ID_PREVIEW).is_ok_and(|edit| edit.0 == lparam.0 as *mut _);
                if is_preview && ctx_ref(hwnd).is_some_and(|ctx| ctx.preview_none.get()) {
                    SetTextColor(HDC(wparam.0 as *mut _), windows::Win32::Foundation::COLORREF(GetSysColor(COLOR_GRAYTEXT)));
                }
                result
            }
            WM_DRAWITEM => {
                let dis = &*(lparam.0 as *const DRAWITEMSTRUCT);
                if dis.CtlType == ODT_LISTVIEW {
                    if let Some(ctx) = ctx_ref(hwnd) {
                        draw_row(ctx, dis);
                        return LRESULT(1);
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_NOTIFY => {
                let hdr = &*(lparam.0 as *const NMHDR);
                let Some(ctx) = ctx_ref(hwnd) else {
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                };
                match (hdr.idFrom as i32, hdr.code) {
                    // 【読み上げ】ここから（外すと、この通知は下の既定の処理へ流れ、項目の文字列は空になる）
                    (ID_LIST, LVN_GETDISPINFOW) => {
                        fill_dispinfo(ctx, &mut *(lparam.0 as *mut NMLVDISPINFOW));
                        LRESULT(0)
                    }
                    // 【読み上げ】ここまで
                    (ID_LIST, LVN_ITEMCHANGED) => {
                        let nm = &*(lparam.0 as *const NMLISTVIEW);
                        let selection_changed = (nm.uChanged.0 & LVIF_STATE.0) != 0
                            && ((nm.uNewState ^ nm.uOldState) & LVIS_SELECTED.0) != 0;
                        if selection_changed {
                            report_selection(hwnd, ctx);
                        }
                        LRESULT(0)
                    }
                    // 仮想一覧で Shift を押したまま範囲を選ぶと、LVN_ITEMCHANGED は項目ごとには
                    // 届かず、この通知が1回だけ届く（Microsoft Learn の LVN_ITEMCHANGED の説明）
                    (ID_LIST, LVN_ODSTATECHANGED) => {
                        let nm = &*(lparam.0 as *const NMLVODSTATECHANGE);
                        if ((nm.uNewState.0 ^ nm.uOldState.0) & LVIS_SELECTED.0) != 0 {
                            report_selection(hwnd, ctx);
                        }
                        LRESULT(0)
                    }
                    // ダブルクリックで送る（フォルダの行は開く）。行の上のときだけで、対象は Enter と同じく
                    // 選んでいる先頭の行（複数選択は先頭だけ）
                    (ID_LIST, NM_DBLCLK) => {
                        let nm = &*(lparam.0 as *const NMITEMACTIVATE);
                        let on_row = usize::try_from(nm.iItem).is_ok_and(|i| i < ctx.view.borrow().rows.len());
                        if let Some(target) = selected_row(hwnd, ctx).filter(|_| on_row) {
                            activate_row(hwnd, ctx, target);
                        }
                        LRESULT(0)
                    }
                    // 一覧が Enter を自分で受け取った場合（ふつうは IsDialogMessageW が IDOK にする）
                    (ID_LIST, NM_RETURN) => {
                        if let Some(target) = selected_row(hwnd, ctx) {
                            activate_row(hwnd, ctx, target);
                        }
                        LRESULT(0)
                    }
                    // 左ボタンでの行のドラッグ（ピン留めの行の移動、履歴の行のピン留め）。マウスは窓が捕まえる
                    (ID_LIST, LVN_BEGINDRAG) => {
                        let nm = &*(lparam.0 as *const NMLISTVIEW);
                        begin_row_drag(hwnd, ctx, nm.iItem);
                        LRESULT(0)
                    }
                    // 左ボタンでのツリーの項目のドラッグ（ピン留めのフォルダの移動）。マウスは窓が捕まえる
                    (ID_TREE, TVN_BEGINDRAGW) => {
                        let nm = &*(lparam.0 as *const NMTREEVIEWW);
                        begin_tree_drag(hwnd, ctx, nm.itemNew.hItem);
                        LRESULT(0)
                    }
                    (ID_LIST, LVN_KEYDOWN) => {
                        let nm = &*(lparam.0 as *const NMLVKEYDOWN);
                        if nm.wVKey == VK_DELETE.0 {
                            if let Some(target) = selected_row(hwnd, ctx) {
                                delete_row(hwnd, ctx, target);
                            }
                        } else if nm.wVKey == VK_F2.0 {
                            // ピン留めの行の名前の変更（履歴の行では何もしない）
                            if let Some(target) = selected_row(hwnd, ctx) {
                                rename_pinned_row(hwnd, ctx, target);
                            }
                        }
                        LRESULT(0)
                    }
                    (ID_TREE, TVN_SELCHANGEDW) => {
                        if !ctx.tree_rebuilding.get() {
                            let nm = &*(lparam.0 as *const NMTREEVIEWW);
                            // 借用は表示対象を読むだけの間にとどめ、ハンドラ呼び出しの前に手放す
                            // （仮の項目の TEMP_TOKEN は表に無いので None）
                            let source = ctx.view.borrow().tree_tokens.get(nm.itemNew.lParam.0 as usize).copied();
                            if ctx.edit.get() != EditState::None {
                                // 名前の編集の間は伝えない（TVM_EDITLABEL が対象を暗黙に選ぶことがある）。ユーザーが
                                // マウス・キーボードで選んだものだけ覚え、後始末で採る
                                let by_user = nm.action == TVC_BYMOUSE || nm.action == TVC_BYKEYBOARD;
                                if let Some(source) = source.filter(|_| by_user) {
                                    ctx.edit_user_choice.set(Some(source));
                                }
                            } else if let Some(source) = source {
                                let handler = Rc::clone(&ctx.handler);
                                handler.on_source_selected(hwnd, source);
                            }
                        }
                        LRESULT(0)
                    }
                    // 名前の編集を始めてよいのは、begin_edit が始めた（Starting）ときだけ。取り消しの印が立って
                    // いれば断る（このとき TVN_ENDLABELEDIT は来ない）。文字のクリックで始まる編集も断る
                    (ID_TREE, TVN_BEGINLABELEDITW) => {
                        let allowed = match ctx.edit.get() {
                            EditState::Starting(target) => {
                                edit_hook(hwnd, "before_begin");
                                if ctx.edit_cancel.get() {
                                    false
                                } else {
                                    ctx.edit.set(EditState::Editing(target));
                                    true
                                }
                            }
                            _ => false,
                        };
                        LRESULT(if allowed { 0 } else { 1 })
                    }
                    // 文字（確定なら）を写し、後始末を投げて、FALSE を返す（ツリーの文字は変えない。保存が
                    // 済んだ後の起床で作り直されて新しい名前が出る）。ここでは項目の削除・作り直し・ハンドラの
                    // 呼び出しをしない
                    (ID_TREE, TVN_ENDLABELEDITW) => {
                        let nm = &*(lparam.0 as *const NMTVDISPINFOW);
                        let text = (!nm.item.pszText.is_null()).then(|| nm.item.pszText.to_string().unwrap_or_default());
                        finish_edit(hwnd, ctx, text);
                        LRESULT(0)
                    }
                    // F2 で、選んでいるピン留めのフォルダの名前の変更を始める
                    (ID_TREE, TVN_KEYDOWN) => {
                        let nm = &*(lparam.0 as *const NMTVKEYDOWN);
                        // F2 で名前の編集を始める（行のドラッグ中は始めない。`row_drag_active`）
                        if nm.wVKey == VK_F2.0 && !row_drag_active(ctx) {
                            if let Ok(tree) = GetDlgItem(Some(hwnd), ID_TREE) {
                                if let Some(Source::Pinned(Some(id))) = selected_tree_source(ctx, tree) {
                                    begin_edit(hwnd, ctx, EditTarget::Rename { id });
                                }
                            }
                        }
                        LRESULT(0)
                    }
                    _ => DefWindowProcW(hwnd, msg, wparam, lparam),
                }
            }
            // lParam が 0 のときだけが ShowWindow による表示・非表示（0 以外は所有者の窓の
            // 最小化・復元などによるもの）。なお ShowWindow に SW_SHOWNORMAL を渡すとこの
            // メッセージは送られないため、show() は SW_SHOW・SW_RESTORE を使う
            WM_SHOWWINDOW if lparam.0 == 0 => {
                if let Some(ctx) = ctx_ref(hwnd) {
                    let handler = Rc::clone(&ctx.handler);
                    if wparam.0 != 0 {
                        SetTimer(Some(hwnd), AGE_TIMER_ID, AGE_REFRESH_MS, None);
                        handler.on_shown(hwnd);
                    } else {
                        let _ = KillTimer(Some(hwnd), AGE_TIMER_ID);
                        release_images(ctx);
                        handler.on_hidden(hwnd);
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            // 非アクティブになるときにフォーカスのあった子コントロールを覚え、アクティブに戻った
            // ときに戻す（Alt+Tab などで戻ったとき、キー操作がそのまま続けられるように）。
            // 最小化からの復元中（HIWORD が 0 でない）は既定の処理に任せる
            WM_ACTIVATE => {
                if let Some(ctx) = ctx_ref(hwnd) {
                    if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE {
                        let focus = GetFocus();
                        if !focus.is_invalid() && IsChild(hwnd, focus).as_bool() {
                            ctx.last_focus.set(focus.0 as isize);
                        }
                    } else if (wparam.0 >> 16) & 0xFFFF == 0 && ctx.last_focus.get() != 0 {
                        let saved = HWND(ctx.last_focus.get() as *mut _);
                        if IsWindow(Some(saved)).as_bool() && IsChild(hwnd, saved).as_bool() {
                            let _ = SetFocus(Some(saved));
                            return LRESULT(0);
                        }
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_APP_SELECTION => {
                if let Some(ctx) = ctx_ref(hwnd) {
                    deliver_selection(hwnd, ctx);
                }
                LRESULT(0)
            }
            WM_APP_EDIT_DONE => {
                if let Some(ctx) = ctx_ref(hwnd) {
                    complete_edit(hwnd, ctx);
                }
                LRESULT(0)
            }
            WM_APP_OPEN_FOLDER => {
                if let Some(ctx) = ctx_ref(hwnd) {
                    if let Some(id) = ctx.open_folder.take() {
                        open_folder(hwnd, ctx, id);
                    }
                }
                LRESULT(0)
            }
            // 終了の要求の後もメニュー・確認ダイアログが開いていれば、閉じる要求をやり直す（メニューの
            // モーダルループの中でも届く）。閉じていれば止める
            WM_TIMER if wparam.0 == CLOSE_RETRY_TIMER_ID => {
                if modal_is_open(hwnd) {
                    #[cfg(test)]
                    tests::CLOSE_RETRIES.with(|n| n.set(n.get() + 1));
                    cancel_modal(hwnd);
                } else {
                    let _ = KillTimer(Some(hwnd), CLOSE_RETRY_TIMER_ID);
                }
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == PREVIEW_REFIT_TIMER_ID => {
                let _ = KillTimer(Some(hwnd), PREVIEW_REFIT_TIMER_ID);
                if let Some(ctx) = ctx_ref(hwnd) {
                    refit_preview(hwnd, ctx);
                }
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == AGE_TIMER_ID => {
                // 描き直すだけ（経過時間は `Row::detail` が描くたびに作る）
                if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
                    let _ = InvalidateRect(Some(list), None, false);
                }
                LRESULT(0)
            }
            WM_APP_IMAGE => {
                if let Some(ctx) = ctx_ref(hwnd) {
                    receive_images(hwnd, ctx);
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                let code = ((wparam.0 >> 16) & 0xFFFF) as u32;
                // メニューバー（code 0）とアクセラレータ（code 1）。lParam は 0（コントロールからの
                // 通知ではない）。メニューのモーダルループは抜けた後に届く
                if lparam.0 == 0 && code <= 1 {
                    if let Some(ctx) = ctx_ref(hwnd) {
                        let tool = match id as u16 {
                            CMD_FIND => {
                                focus_search(hwnd);
                                return LRESULT(0);
                            }
                            CMD_MOVE_UP | CMD_MOVE_DOWN => {
                                let direction = if id as u16 == CMD_MOVE_UP { Direction::Up } else { Direction::Down };
                                reorder_selected_row(hwnd, ctx, direction);
                                return LRESULT(0);
                            }
                            CMD_ABOUT => {
                                show_about(hwnd, ctx);
                                return LRESULT(0);
                            }
                            CMD_SETTINGS => {
                                // メニュー・ダイアログの表示中は開かない（設定画面はモードレスで、ほかのモーダルの
                                // 中から開くと、そのモーダルの間も操作できてしまうため）
                                if !modal_is_open(hwnd) {
                                    let handler = Rc::clone(&ctx.handler);
                                    handler.on_open_settings(hwnd);
                                }
                                return LRESULT(0);
                            }
                            CMD_CLEAR_HISTORY => confirm_clear_history(hwnd, ctx).then_some(ToolCommand::ClearHistory),
                            CMD_CLEAR_CLIPBOARD => Some(ToolCommand::ClearClipboard),
                            CMD_CHECK_DATA => Some(ToolCommand::CheckData),
                            CMD_TOPMOST => Some(ToolCommand::ToggleTopmost),
                            _ => None,
                        };
                        if let Some(tool) = tool {
                            let handler = Rc::clone(&ctx.handler);
                            handler.on_tool_command(hwnd, tool);
                            return LRESULT(0);
                        }
                        if id as u16 == CMD_CLEAR_HISTORY {
                            return LRESULT(0);
                        }
                    }
                }
                if id == ID_SEARCH && code == EN_CHANGE {
                    // 入力が止まるまで待つ（打鍵のたびにタイマーを張り直す）
                    SetTimer(Some(hwnd), SEARCH_TIMER_ID, SEARCH_DELAY_MS, None);
                    return LRESULT(0);
                }
                // IsDialogMessageW が Enter・Esc から作るのは code 0・lParam 0 の IDOK・IDCANCEL。ツリーは
                // 名前の編集欄の通知（id 1 の EN_CHANGE など、lParam が 0 でない）も親へ回してくるので、
                // それと取り違えない（実機で確かめた）
                let from_dialog_manager = code == 0 && lparam.0 == 0;
                // 行のドラッグ中の Esc は、ドラッグの取り消し（検索欄は消さない）
                if from_dialog_manager && id == IDCANCEL.0 {
                    if let Some(ctx) = ctx_ref(hwnd).filter(|ctx| ctx.row_drag.get().is_some()) {
                        cancel_row_drag(hwnd, ctx);
                        return LRESULT(0);
                    }
                }
                // 名前の編集中の Enter・Esc は、編集の確定・取り消し（編集欄には届かない）
                if from_dialog_manager && (id == IDOK.0 || id == IDCANCEL.0) {
                    if let Some(ctx) = ctx_ref(hwnd) {
                        if end_edit_by_key(hwnd, ctx, id == IDCANCEL.0) {
                            return LRESULT(0);
                        }
                    }
                }
                // Enter（IsDialogMessageW が IDOK にする。Microsoft Learn の Dialog Box Keyboard
                // Interface）。一覧にフォーカスがあるときだけ、選んでいる行を送る（検索欄では
                // 何もしない）
                if id == IDOK.0 && from_dialog_manager {
                    if list_has_focus(hwnd) {
                        if let Some(ctx) = ctx_ref(hwnd) {
                            if let Some(row) = selected_row(hwnd, ctx) {
                                activate_row(hwnd, ctx, row);
                            }
                        }
                    }
                    return LRESULT(0);
                }
                // Esc（IsDialogMessageW が IDCANCEL にする）で検索を取り消す
                if id == IDCANCEL.0 && from_dialog_manager {
                    if let Ok(search) = GetDlgItem(Some(hwnd), ID_SEARCH) {
                        if GetWindowTextLengthW(search) > 0 {
                            let _ = SetWindowTextW(search, w!(""));
                        }
                    }
                    return LRESULT(0);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_TIMER if wparam.0 == SEARCH_TIMER_ID => {
                let _ = KillTimer(Some(hwnd), SEARCH_TIMER_ID);
                if let (Some(ctx), Ok(search)) = (ctx_ref(hwnd), GetDlgItem(Some(hwnd), ID_SEARCH)) {
                    let text = window_text(search);
                    let handler = Rc::clone(&ctx.handler);
                    handler.on_search_changed(hwnd, text);
                }
                LRESULT(0)
            }
            WM_APP_WAKE => {
                // ハンドラの Rc を複製してから呼ぶ（呼び出し中に窓が破棄されても解放されない）
                if let Some(ctx) = ctx_ref(hwnd) {
                    // メニュー・確認ダイアログを閉じた後の起床で、保留していたメニューバーの作り直しを行う
                    if ctx.menu_bar_pending.get() {
                        rebuild_menu_bar(hwnd, ctx, GetDpiForWindow(hwnd));
                    }
                    let handler = Rc::clone(&ctx.handler);
                    handler.on_wake(hwnd);
                }
                LRESULT(0)
            }
            WM_APP_SHOW_REQUEST => {
                show(hwnd);
                LRESULT(0)
            }
            // 一覧・ツリーの右クリック・Shift+F10・アプリケーションキー（コントロールの既定の処理が親へ回す）
            WM_CONTEXTMENU => {
                let from = HWND(wparam.0 as *mut _);
                if let Some(ctx) = ctx_ref(hwnd) {
                    if GetDlgItem(Some(hwnd), ID_LIST).is_ok_and(|list| from == list) {
                        // ツリーで押したまま一覧の上で離した分がここへ来たら（ツリーがマウスを捕まえ
                        // 損ねた場合）、一覧の右クリックとして扱わない。キーボード（-1, -1）は扱う
                        if lparam.0 as u32 != u32::MAX && ctx.tree_rpress.take().is_some() {
                            return LRESULT(0);
                        }
                        show_row_menu(hwnd, ctx, from, lparam);
                        return LRESULT(0);
                    }
                    if GetDlgItem(Some(hwnd), ID_TREE).is_ok_and(|tree| from == tree) {
                        show_tree_menu(hwnd, ctx, from, lparam);
                        return LRESULT(0);
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            // メニューのモーダルループの中で窓を破棄しない（ビューア窓の破棄はメッセージループを抜けた後の
            // `main` だけ）。デバッグ時の検出で、
            // 安全を保つのは破棄の経路のほう
            WM_DESTROY => {
                if let Some(ctx) = ctx_ref(hwnd) {
                    debug_assert!(!ctx.menu_open.get(), "ビューア窓をメニューの表示中に破棄した");
                    debug_assert!(ctx.dialog.get() == 0, "ビューア窓を確認ダイアログの表示中に破棄した");
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_CLOSE => {
                if let Some(ctx) = ctx_ref(hwnd) {
                    let handler = Rc::clone(&ctx.handler);
                    handler.on_close(hwnd);
                }
                LRESULT(0)
            }
            // セッションの終了（Microsoft Learn の WM_ENDSESSION の説明）。WM_QUERYENDSESSION は
            // 既定処理（終了してよい）に任せる。取りやめ（wParam = FALSE）では何もしない
            WM_ENDSESSION => {
                if wparam.0 != 0 {
                    if let Some(ctx) = ctx_ref(hwnd) {
                        let handler = Rc::clone(&ctx.handler);
                        handler.on_end_session(hwnd);
                    }
                }
                LRESULT(0)
            }
            WM_NCDESTROY => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowCtx;
                if !ptr.is_null() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    let ctx = Box::from_raw(ptr);
                    for icon in ctx.icons {
                        if !icon.is_invalid() {
                            let _ = DestroyIcon(icon);
                        }
                    }
                    if !ctx.accel.is_invalid() {
                        let _ = DestroyAcceleratorTable(ctx.accel);
                    }
                    // フォントは Metrics の Drop で解放する
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            // 別のモニターへの移動・表示倍率の変更。フォントと寸法を作り直し、Windows が勧める
            // 位置と大きさへ動かす（Microsoft Learn の WM_DPICHANGED の説明どおり）
            WM_DPICHANGED => {
                if let Some(ctx) = ctx_ref(hwnd) {
                    // ドラッグ中なら終える（押した位置のずれは古い DPI の px のため）
                    if ctx.drag.take().is_some() {
                        let _ = ReleaseCapture();
                    }
                    let dpi = (wparam.0 & 0xFFFF) as u32;
                    apply_dpi(hwnd, ctx, dpi);
                    // メニューバーのドロップダウンも新しい DPI で作り直す（表示中なら閉じた後に）
                    rebuild_menu_bar(hwnd, ctx, dpi);
                    let rc = &*(lparam.0 as *const RECT);
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        rc.left,
                        rc.top,
                        rc.right - rc.left,
                        rc.bottom - rc.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                    let handler = Rc::clone(&ctx.handler);
                    handler.on_metrics_changed(hwnd);
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::model::EntryKind;
    use std::cell::Cell;
    use uuid::Uuid;
    // 【読み上げ】ここから
    use windows::Win32::UI::Controls::{LVM_GETITEMCOUNT, LVM_GETITEMTEXTW};
    // 【読み上げ】ここまで
    use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, IsWindow, PeekMessageW, MSG, PM_REMOVE};

    #[derive(Default)]
    struct Recorder {
        wakes: Cell<u32>,
        closes: Cell<u32>,
        selected: RefCell<Vec<Source>>,
        row_selections: RefCell<Vec<Option<Uuid>>>,
        searches: RefCell<Vec<String>>,
        metrics_changes: Cell<u32>,
        end_sessions: Cell<u32>,
        activated: RefCell<Vec<Uuid>>,
        deleted: RefCell<Vec<Uuid>>,
        /// 右クリックメニューを求められた行
        menu_requests: RefCell<Vec<Uuid>>,
        /// `row_menu` が返すもの（None ならメニューを出さない）
        menu: RefCell<Option<RowMenu>>,
        commands: RefCell<Vec<(Uuid, RowCommand)>>,
        tools: RefCell<Vec<ToolCommand>>,
        /// 閉じる操作で窓を隠す（アプリのトレイがあるときと同じ）
        hide_on_close: Cell<bool>,
        /// ツリーの右クリックメニューを求められた項目
        tree_menu_requests: RefCell<Vec<Source>>,
        /// `tree_menu` が返すもの（None ならメニューを出さない）
        tree_menu: RefCell<Option<TreeMenu>>,
        tree_commands: RefCell<Vec<TreeCommand>>,
        /// `pinned_title` が返すもの（外の None はアイテムが無い＝名前の変更のダイアログを出さない）
        pinned_title: RefCell<Option<Option<String>>>,
        /// 名前の変更のダイアログの「OK」で伝えられたもの
        renamed: RefCell<Vec<(Uuid, String)>>,
        /// 行をドラッグできなくする（アプリの検索中のピン留めの行と同じ）。立っていなければどの行もドラッグできる
        no_drag: Cell<bool>,
    }

    impl ViewerHandler for Recorder {
        fn pinned_title(&self, _id: Uuid) -> Option<Option<String>> {
            self.pinned_title.borrow().clone()
        }
        fn on_rename_pinned(&self, _hwnd: HWND, id: Uuid, title: String) {
            self.renamed.borrow_mut().push((id, title));
        }
        fn tree_menu(&self, source: Source) -> Option<TreeMenu> {
            self.tree_menu_requests.borrow_mut().push(source);
            self.tree_menu.borrow().clone()
        }
        fn on_tree_command(&self, _hwnd: HWND, command: TreeCommand) {
            self.tree_commands.borrow_mut().push(command);
        }
        fn on_tool_command(&self, _hwnd: HWND, command: ToolCommand) {
            self.tools.borrow_mut().push(command);
        }
        fn on_activate(&self, _hwnd: HWND, target: RowTarget) {
            self.activated.borrow_mut().push(target.id);
        }
        fn on_delete(&self, _hwnd: HWND, target: RowTarget) {
            self.deleted.borrow_mut().push(target.id);
        }
        fn row_menu(&self, target: RowTarget) -> Option<RowMenu> {
            self.menu_requests.borrow_mut().push(target.id);
            self.menu.borrow().clone()
        }
        fn on_row_command(&self, _hwnd: HWND, target: RowTarget, command: RowCommand) {
            self.commands.borrow_mut().push((target.id, command));
        }
        fn can_drag_row(&self, _target: RowTarget) -> bool {
            !self.no_drag.get()
        }
        fn on_wake(&self, _hwnd: HWND) {
            self.wakes.set(self.wakes.get() + 1);
        }
        fn on_close(&self, hwnd: HWND) {
            self.closes.set(self.closes.get() + 1);
            if self.hide_on_close.get() {
                hide(hwnd);
            }
        }
        fn on_shown(&self, _hwnd: HWND) {}
        fn on_source_selected(&self, _hwnd: HWND, source: Source) {
            self.selected.borrow_mut().push(source);
        }
        fn on_hidden(&self, _hwnd: HWND) {}
        fn on_selection_changed(&self, _hwnd: HWND, id: Option<Uuid>) {
            self.row_selections.borrow_mut().push(id);
        }
        fn on_search_changed(&self, _hwnd: HWND, text: String) {
            self.searches.borrow_mut().push(text);
        }
        fn on_metrics_changed(&self, _hwnd: HWND) {
            self.metrics_changes.set(self.metrics_changes.get() + 1);
        }
        fn on_end_session(&self, _hwnd: HWND) {
            self.end_sessions.set(self.end_sessions.get() + 1);
        }
    }

    /// 指定の時間だけ、この窓宛てのメッセージ（タイマーを含む）を処理する。
    fn pump(hwnd: HWND, ms: u64) {
        let until = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        while std::time::Instant::now() < until {
            unsafe {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, Some(hwnd), 0, 0, PM_REMOVE).as_bool() {
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn search(window: &ViewerWindow) -> HWND {
        unsafe { GetDlgItem(Some(window.hwnd()), ID_SEARCH).unwrap() }
    }

    fn create_test_window() -> (ViewerWindow, Rc<Recorder>) {
        let recorder = Rc::new(Recorder::default());
        let handler: Rc<dyn ViewerHandler> = recorder.clone();
        let window = ViewerWindow::create("CLCLR viewer test", (400, 300), handler).unwrap();
        (window, recorder)
    }

    fn row(label: &str) -> Row {
        Row {
            id: Uuid::new_v4(),
            label: label.to_string(),
            modified: 0.0,
            formats: String::new(),
            kind: EntryKind::Text,
            thumb: None,
            pinned: false,
            folder: None,
        }
    }

    fn list(window: &ViewerWindow) -> HWND {
        unsafe { GetDlgItem(Some(window.hwnd()), ID_LIST).unwrap() }
    }

    // 【読み上げ】ここから
    fn item_text(list: HWND, index: usize) -> String {
        let mut buf = [0u16; 256];
        let item = LVITEMW { pszText: PWSTR(buf.as_mut_ptr()), cchTextMax: buf.len() as i32, ..Default::default() };
        let len = unsafe {
            SendMessageW(list, LVM_GETITEMTEXTW, Some(WPARAM(index)), Some(LPARAM(&item as *const _ as isize))).0
        };
        String::from_utf16_lossy(&buf[..len as usize])
    }
    // 【読み上げ】ここまで

    /// 窓は非表示で作られ、一覧・ツリー・プレビューの子コントロールを持つ。
    #[test]
    fn create_makes_hidden_window_with_child_controls() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        assert!(!is_visible(window.hwnd()));
        for id in [ID_TREE, ID_LIST, ID_PREVIEW] {
            assert!(unsafe { GetDlgItem(Some(window.hwnd()), id) }.is_ok(), "子コントロール {id} がない");
        }
    }

    /// 閉じる操作はハンドラへ渡すだけで、窓を破棄しない（DefWindowProcW へ流さない）。
    #[test]
    fn wm_close_calls_handler_and_keeps_window() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder) = create_test_window();
        unsafe {
            SendMessageW(window.hwnd(), WM_CLOSE, None, None);
            assert_eq!(recorder.closes.get(), 1);
            assert!(IsWindow(Some(window.hwnd())).as_bool());
        }
    }

    /// セッションの終了（wParam = TRUE）だけをハンドラへ伝え、取りやめ（FALSE）は伝えない。
    /// 窓は破棄しない。
    #[test]
    fn end_session_is_reported_only_when_session_really_ends() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder) = create_test_window();
        unsafe {
            SendMessageW(window.hwnd(), WM_ENDSESSION, Some(WPARAM(0)), Some(LPARAM(0)));
            assert_eq!(recorder.end_sessions.get(), 0);
            SendMessageW(window.hwnd(), WM_ENDSESSION, Some(WPARAM(1)), Some(LPARAM(0)));
            assert_eq!(recorder.end_sessions.get(), 1);
            assert!(IsWindow(Some(window.hwnd())).as_bool());
        }
    }

    /// `Waker::wake` が投げた起床要求は、メッセージを処理するとハンドラへ届く。
    #[test]
    fn waker_posts_wake_to_handler() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder) = create_test_window();
        let waker = window.waker();
        std::thread::spawn(move || waker.wake()).join().unwrap();
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, Some(window.hwnd()), 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&msg);
            }
        }
        assert_eq!(recorder.wakes.get(), 1);
    }

    /// 窓を破棄するとハンドラの参照が解放される（WM_NCDESTROY）。
    #[test]
    fn destroying_window_releases_handler() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder) = create_test_window();
        assert_eq!(Rc::strong_count(&recorder), 2);
        drop(window);
        assert_eq!(Rc::strong_count(&recorder), 1);
    }

    // 【読み上げ】ここから
    /// 自前で描く一覧でも、項目のテキスト（読み上げソフトが読む）は画面の2行をつないで返す。
    /// 2行目の経過時間は取り出したときの時刻で作る。2行目が空ならタイトルだけ。
    #[test]
    fn list_reports_row_label_and_detail_as_item_text() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let with_detail = Row {
            modified: crate::native::model::unix_now() - 150.0,
            formats: "CF_UNICODETEXT".to_string(),
            ..row("二件目")
        };
        set_rows(window.hwnd(), vec![row("一件目"), with_detail, row("三件目")], false);
        let list = list(&window);
        assert_eq!(unsafe { SendMessageW(list, LVM_GETITEMCOUNT, None, None).0 }, 3);
        assert_eq!(item_text(list, 0), "一件目");
        assert_eq!(item_text(list, 1), "二件目、2分前 — CF_UNICODETEXT");
        assert_eq!(item_text(list, 2), "三件目");
    }
    // 【読み上げ】ここまで

    /// 先頭に新しい行が入っても、選んでいた項目を UUID で選び直す。
    #[test]
    fn set_rows_keeps_selection_by_id_when_new_row_is_inserted_at_front() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let (a, b) = (row("a"), row("b"));
        set_rows(window.hwnd(), vec![a.clone(), b.clone()], false);
        unsafe { set_item_state(list(&window), 1, LVIS_SELECTED.0 | LVIS_FOCUSED.0) };
        assert_eq!(selected_index(list(&window)), Some(1));

        set_rows(window.hwnd(), vec![row("new"), a, b.clone()], false);
        assert_eq!(selected_index(list(&window)), Some(2));
        assert_eq!(row_ids(window.hwnd())[2], b.id);
    }

    /// 選んでいた項目が消えたら、指定があれば先頭を選び、無ければ同じ位置の行を選ぶ（位置を保つ場合は
    /// `set_rows_keeps_position_when_selected_row_disappears`）。
    #[test]
    fn set_rows_selects_first_only_when_requested_if_previous_is_gone() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        set_rows(window.hwnd(), vec![row("x")], true);
        assert_eq!(selected_index(list(&window)), Some(0));
        let rows = vec![row("w"), row("y"), row("z")];
        set_rows(window.hwnd(), rows.clone(), false);
        assert_eq!(selected_index(list(&window)), Some(0), "同じ位置の行を選んでいない");
        unsafe { set_item_state(list(&window), 2, LVIS_SELECTED.0 | LVIS_FOCUSED.0) };
        set_rows(window.hwnd(), vec![row("p"), row("q"), row("r")], true);
        assert_eq!(selected_index(list(&window)), Some(0), "先頭を選んでいない");
    }

    /// `set_rows` は選んだ項目を戻り値で返し、その間の選択の変化はハンドラへ伝えない。
    /// その後のユーザーによる選択の変化（ここではテストから直接変える）は、メッセージを処理した
    /// ときに、何回変わっても1回だけ、最後の選択で伝える。
    #[test]
    fn row_selection_is_returned_by_set_rows_and_reported_once_for_user_changes() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder) = create_test_window();
        let (a, b) = (row("a"), row("b"));
        let selected = set_rows(window.hwnd(), vec![a.clone(), b.clone()], true);
        assert_eq!(selected, Some(a.id));
        pump(window.hwnd(), 30);
        assert!(recorder.row_selections.borrow().is_empty());

        unsafe {
            set_item_state(list(&window), -1, 0);
            set_item_state(list(&window), 1, LVIS_SELECTED.0 | LVIS_FOCUSED.0);
        }
        assert!(recorder.row_selections.borrow().is_empty(), "メッセージを処理する前に伝えている");
        pump(window.hwnd(), 30);
        assert_eq!(recorder.row_selections.borrow().as_slice(), [Some(b.id)], "1回にまとまっていない");
    }

    /// Shift を押したままの範囲選択（仮想一覧では LVN_ITEMCHANGED の代わりに
    /// LVN_ODSTATECHANGED が届く）でも、選択の変化をハンドラへ伝える。
    #[test]
    fn shift_range_selection_is_reported() {
        use windows::Win32::UI::Controls::LVM_GETITEMSTATE;
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardState, SetKeyboardState, VK_SHIFT, VK_UP};
        use windows::Win32::UI::WindowsAndMessaging::WM_KEYDOWN;
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder) = create_test_window();
        let rows = vec![row("a"), row("b"), row("c")];
        set_rows(window.hwnd(), rows.clone(), false);
        let list = list(&window);
        unsafe { set_item_state(list, 2, LVIS_SELECTED.0 | LVIS_FOCUSED.0) };
        pump(window.hwnd(), 30);
        assert_eq!(recorder.row_selections.borrow().last().copied(), Some(Some(rows[2].id)));
        recorder.row_selections.borrow_mut().clear();

        // このスレッドのキーボードの状態で Shift を押したことにして、↑ を送る（一覧は
        // GetKeyState で Shift を見る）
        let mut saved = [0u8; 256];
        unsafe {
            GetKeyboardState(&mut saved).unwrap();
            let mut pressed = saved;
            pressed[VK_SHIFT.0 as usize] = 0x80;
            SetKeyboardState(&pressed).unwrap();
            SendMessageW(list, WM_KEYDOWN, Some(WPARAM(VK_UP.0 as usize)), Some(LPARAM(0)));
            SetKeyboardState(&saved).unwrap();
        }
        let selected: Vec<usize> = (0..3)
            .filter(|&i| unsafe {
                SendMessageW(list, LVM_GETITEMSTATE, Some(WPARAM(i)), Some(LPARAM(LVIS_SELECTED.0 as isize))).0 != 0
            })
            .collect();
        assert_eq!(selected, [1, 2], "範囲選択になっていない（テストの前提）");
        pump(window.hwnd(), 30);
        // 「全部の選択を外す」と「範囲を選ぶ」の通知が届くが、伝えるのは最後の選択で1回だけ
        assert_eq!(recorder.row_selections.borrow().as_slice(), [Some(rows[1].id)]);
    }

    /// 非アクティブになるときにフォーカスのあった子コントロールを覚え、アクティブに戻ったときに
    /// 戻す（Alt+Tab で戻ったとき）。
    #[test]
    fn focus_returns_to_the_last_focused_child_on_reactivation() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let hwnd = window.hwnd();
        let search = search(&window);
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE;
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = SetFocus(Some(search));
            assert_eq!(GetFocus(), search, "前提: 検索欄にフォーカスを置けない");
            SendMessageW(hwnd, WM_ACTIVATE, Some(WPARAM(WA_INACTIVE as usize)), Some(LPARAM(0)));
            // よそへ移ったことにする（窓そのものにフォーカス）
            let _ = SetFocus(Some(hwnd));
            assert_eq!(GetFocus(), hwnd);
            const WA_ACTIVE: usize = 1;
            SendMessageW(hwnd, WM_ACTIVATE, Some(WPARAM(WA_ACTIVE)), Some(LPARAM(0)));
            assert_eq!(GetFocus(), search, "フォーカスを戻していない");
        }
    }

    /// 検索欄の入力は、止まってから遅れてハンドラへ届く。Esc（IDCANCEL）で検索欄が空になる。
    #[test]
    fn search_input_is_reported_after_delay_and_cleared_by_cancel() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder) = create_test_window();
        unsafe {
            let _ = SetWindowTextW(search(&window), w!("クリップ"));
        }
        assert!(recorder.searches.borrow().is_empty(), "遅延の前に届いている");
        pump(window.hwnd(), SEARCH_DELAY_MS as u64 + 200);
        assert_eq!(recorder.searches.borrow().as_slice(), ["クリップ".to_string()]);

        unsafe {
            SendMessageW(window.hwnd(), WM_COMMAND, Some(WPARAM(IDCANCEL.0 as usize)), Some(LPARAM(0)));
        }
        assert_eq!(window_text(search(&window)), "");
        pump(window.hwnd(), SEARCH_DELAY_MS as u64 + 200);
        assert_eq!(recorder.searches.borrow().last().map(String::as_str), Some(""));
    }

    fn attach_test_link(window: &ViewerWindow) -> (Receiver<ImageRequest>, Sender<LoadedImage>) {
        let (req_rx, res_tx, _wanted) = attach_test_link_with_wanted(window);
        (req_rx, res_tx)
    }

    fn attach_test_link_with_wanted(window: &ViewerWindow) -> (Receiver<ImageRequest>, Sender<LoadedImage>, WantedThumbs) {
        let (req_tx, req_rx) = std::sync::mpsc::channel();
        let (res_tx, res_rx) = std::sync::mpsc::channel();
        let wanted = WantedThumbs::default();
        attach_image_loader(window.hwnd(), req_tx, res_rx, std::sync::Arc::clone(&wanted), WantedPreview::default());
        (req_rx, res_tx, wanted)
    }

    /// 窓をアクティブにせずに表示し、一覧を今すぐ描かせる（`WM_DRAWITEM` を通す）。
    fn show_and_paint(window: &ViewerWindow) {
        use windows::Win32::Graphics::Gdi::UpdateWindow;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE;
        unsafe {
            let _ = ShowWindow(window.hwnd(), SW_SHOWNOACTIVATE);
            let _ = UpdateWindow(list(window));
        }
    }

    /// タイトル・2行目が空の行も描ける（先頭が改行のテキストはタイトルが空になる。空の文字列を
    /// そのまま `DrawTextW` に渡すと、ウィンドウプロシージャの中で落ちていた）。
    #[test]
    fn rows_with_empty_title_or_detail_can_be_drawn() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let empty_title = Row { formats: "CF_UNICODETEXT".into(), modified: crate::native::model::unix_now(), ..row("") };
        // フォルダの行（フォルダのアイコンと中の数）も描ける
        let folder = Row {
            pinned: true,
            folder: Some(crate::native::model::FolderCounts { items: 1, folders: 0 }),
            ..row("箱")
        };
        set_rows(window.hwnd(), vec![empty_title, row(""), folder], false);
        show_and_paint(&window);
        assert!(unsafe { IsWindow(Some(window.hwnd())) }.as_bool());
    }

    /// 今欲しいサムネイルは、描いて頼んだ行の分だけ持ち、一覧から消えた行・届いたもの・隠したときに
    /// 除く（読み込みスレッドは、ここに無い項目のサムネイルの依頼を捨てる）。
    #[test]
    fn wanted_thumbnails_follow_rows_results_and_hiding() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let (req_rx, res_tx, wanted) = attach_test_link_with_wanted(&window);
        let (a, b) = (
            Row { thumb: Some("a.thumb.webp".into()), ..row("a") },
            Row { thumb: Some("b.thumb.webp".into()), ..row("b") },
        );
        set_rows(window.hwnd(), vec![a.clone(), b.clone()], false);
        show_and_paint(&window);
        let requested: std::collections::HashSet<Uuid> = req_rx.try_iter().map(|r| r.id).collect();
        assert_eq!(requested, [a.id, b.id].into_iter().collect(), "描いた行のサムネイルを頼んでいない");
        assert_eq!(*wanted.lock().unwrap(), requested);

        // 一覧から消えた行は除く
        set_rows(window.hwnd(), vec![a.clone()], false);
        assert_eq!(*wanted.lock().unwrap(), [a.id].into_iter().collect());

        // 届いたものは除く
        let generation = unsafe { ctx_ref(window.hwnd()).unwrap() }.view.borrow().generation;
        res_tx.send(loaded(generation, a.id, Purpose::ListThumb)).unwrap();
        unsafe { SendMessageW(window.hwnd(), WM_APP_IMAGE, None, None) };
        assert!(wanted.lock().unwrap().is_empty());

        // 隠したら全部除く
        wanted.lock().unwrap().insert(b.id);
        unsafe {
            let _ = ShowWindow(window.hwnd(), SW_HIDE);
        }
        assert!(wanted.lock().unwrap().is_empty());
    }

    /// 隠す前の古い世代の結果が、表示し直した後に同じ項目を頼み直した「欲しいサムネイル」を外さない（外すと読み込み
    /// スレッドが新しい依頼を捨て、行が種別アイコンのままになる）。
    #[test]
    fn old_generation_result_keeps_wanted_of_new_request() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let (req_rx, res_tx, wanted) = attach_test_link_with_wanted(&window);
        let a = Row { thumb: Some("a.thumb.webp".into()), ..row("a") };
        set_rows(window.hwnd(), vec![a.clone()], false);
        show_and_paint(&window);
        let old = unsafe { ctx_ref(window.hwnd()).unwrap() }.view.borrow().generation;
        unsafe {
            let _ = ShowWindow(window.hwnd(), SW_HIDE);
        }
        let _ = req_rx.try_iter().count();
        // 表示し直すと、同じ項目を新しい世代で頼み直す
        show_and_paint(&window);
        assert!(req_rx.try_iter().any(|r| r.id == a.id), "前提: 頼み直していない");
        assert!(wanted.lock().unwrap().contains(&a.id));
        // 隠す前の依頼の結果が遅れて届く
        res_tx.send(loaded(old, a.id, Purpose::ListThumb)).unwrap();
        unsafe { SendMessageW(window.hwnd(), WM_APP_IMAGE, None, None) };
        assert!(wanted.lock().unwrap().contains(&a.id), "古い世代の結果で、新しい依頼を外した");
    }

    /// TaskDialog を呼んでから窓ができる（`TDN_CREATED`）までの開きかけも、モーダルの表示中として扱う。その間の
    /// 閉じる要求は印に残る（窓ができたら閉じる）。
    #[test]
    fn opening_dialog_counts_as_modal_and_keeps_cancel_request() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let ctx = unsafe { ctx_ref(window.hwnd()).unwrap() };
        assert!(!modal_is_open(window.hwnd()));
        ctx.dialog_opening.set(true);
        assert!(modal_is_open(window.hwnd()), "開きかけをモーダルとして扱っていない");
        assert!(!can_open_dialog(ctx), "開きかけの間に別のダイアログを開ける");
        cancel_modal(window.hwnd());
        assert!(ctx.dialog_cancel.get(), "開きかけの間の閉じる要求を残していない");
        ctx.dialog_opening.set(false);
        ctx.dialog_cancel.set(false);
    }

    fn loaded(generation: u64, id: Uuid, purpose: Purpose) -> LoadedImage {
        let content = ImageContent::Pixels { width: 2, height: 1, bgra: vec![255; 8], source_width: 2, source_height: 1 };
        LoadedImage { generation, id, purpose, preview_ticket: 0, content }
    }

    /// 読み込みの結果は、今の世代で、頼んだ（一覧にある）項目のものだけを受け取る。
    #[test]
    fn image_results_of_old_generation_are_discarded() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let (_req_rx, res_tx) = attach_test_link(&window);
        let r = Row { thumb: Some("t.thumb.webp".into()), ..row("image") };
        set_rows(window.hwnd(), vec![r.clone()], false);
        let ctx = unsafe { ctx_ref(window.hwnd()).unwrap() };
        ctx.view.borrow_mut().thumbs.insert(r.id, None);
        let generation = ctx.view.borrow().generation;

        res_tx.send(loaded(generation + 1, r.id, Purpose::ListThumb)).unwrap();
        res_tx.send(loaded(generation, Uuid::new_v4(), Purpose::ListThumb)).unwrap();
        unsafe { SendMessageW(window.hwnd(), WM_APP_IMAGE, None, None) };
        assert!(matches!(ctx.view.borrow().thumbs.get(&r.id), Some(None)), "古い世代の結果を受け取っている");
        assert_eq!(ctx.view.borrow().thumbs.len(), 1, "頼んでいない項目の結果を受け取っている");

        res_tx.send(loaded(generation, r.id, Purpose::ListThumb)).unwrap();
        unsafe { SendMessageW(window.hwnd(), WM_APP_IMAGE, None, None) };
        assert!(matches!(ctx.view.borrow().thumbs.get(&r.id), Some(Some(_))));
    }

    /// 隠すと世代が進み、サムネイルとプレビューの画像を解放する。
    #[test]
    fn hiding_releases_images_and_advances_generation() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let ctx = unsafe { ctx_ref(window.hwnd()).unwrap() };
        let id = Uuid::new_v4();
        {
            let mut view = ctx.view.borrow_mut();
            view.thumbs.insert(id, Bitmap::from_bgra(2, 1, &[255; 8]));
            view.preview_wanted = Some(id);
            view.preview_image = Bitmap::from_bgra(2, 1, &[255; 8]);
        }
        let before = ctx.view.borrow().generation;
        unsafe { SendMessageW(window.hwnd(), WM_SHOWWINDOW, Some(WPARAM(0)), Some(LPARAM(0))) };
        let view = ctx.view.borrow();
        assert!(view.thumbs.is_empty() && view.preview_image.is_none() && view.preview_wanted.is_none());
        assert_eq!(view.generation, before + 1);
    }

    /// 画像のプレビューは EDIT の代わりに画像の子窓を出し、その大きさに収めた読み込みを頼む。
    /// テキストのプレビューに戻すと画像の子窓を隠す。
    #[test]
    fn preview_image_swaps_edit_for_image_window_and_requests_fitting_size() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let (req_rx, _res_tx) = attach_test_link(&window);
        let id = Uuid::new_v4();
        show_preview_image(window.hwnd(), id, ImageSource::Blob("x.webp".into()));
        let (edit, image) = unsafe {
            (GetDlgItem(Some(window.hwnd()), ID_PREVIEW).unwrap(), GetDlgItem(Some(window.hwnd()), ID_IMAGE).unwrap())
        };
        let visible = |h: HWND| unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowLongW(h, windows::Win32::UI::WindowsAndMessaging::GWL_STYLE) as u32 & WS_VISIBLE.0 != 0 };
        assert!(!visible(edit) && visible(image));
        let request = req_rx.try_recv().expect("読み込みを頼んでいない");
        let mut rc = RECT::default();
        unsafe { GetClientRect(image, &mut rc).unwrap() };
        assert_eq!((request.id, request.purpose), (id, Purpose::Preview));
        assert_eq!((request.max_width, request.max_height), (rc.right as u32, rc.bottom as u32));
        // 頼んだプレビューの番号が、読み込みスレッドと共有する「今欲しいもの」になる
        let wanted_preview = || {
            let ctx = unsafe { ctx_ref(window.hwnd()).unwrap() };
            let images = ctx.images.borrow();
            images.as_ref().unwrap().wanted_preview.load(std::sync::atomic::Ordering::SeqCst)
        };
        assert!(request.preview_ticket != 0 && wanted_preview() == request.preview_ticket);

        set_preview_text(window.hwnd(), "text");
        assert!(visible(edit) && !visible(image));
        assert!(unsafe { ctx_ref(window.hwnd()).unwrap() }.view.borrow().preview_wanted.is_none());
        assert_eq!(wanted_preview(), 0, "テキストに替えても、待っているプレビューの依頼を取り消していない");
    }

    /// 今のプレビュー欄より小さい画像が届いたら（頼んだ後に欄が広がった場合など）、読み直しの確かめで
    /// 同じ読み込み元を今の欄の大きさで頼み直す。届くまでは今の画像を出したまま。欄に収まる最大の
    /// 大きさで届いていれば頼み直さない。
    #[test]
    fn preview_is_reloaded_when_area_allows_larger_image() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let (req_rx, res_tx) = attach_test_link(&window);
        let ctx = unsafe { ctx_ref(window.hwnd()).unwrap() };
        let id = Uuid::new_v4();
        show_preview_image(window.hwnd(), id, ImageSource::Blob("x.webp".into()));
        let first = req_rx.try_recv().expect("読み込みを頼んでいない");
        let (area_w, area_h) = (first.max_width, first.max_height);
        let generation = ctx.view.borrow().generation;
        let deliver = |width: u32, height: u32, preview_ticket: u64| {
            let bgra = vec![255; (width * height * 4) as usize];
            let content = ImageContent::Pixels { width, height, bgra, source_width: 4000, source_height: 3000 };
            res_tx.send(LoadedImage { generation, id, purpose: Purpose::Preview, preview_ticket, content }).unwrap();
            unsafe {
                SendMessageW(window.hwnd(), WM_APP_IMAGE, None, None);
                SendMessageW(window.hwnd(), WM_TIMER, Some(WPARAM(PREVIEW_REFIT_TIMER_ID)), None);
            }
        };
        let (fit_w, fit_h) = crate::native::images::fit_size(4000, 3000, area_w, area_h);
        assert!(fit_w > 40 && fit_h > 30, "前提: テストの窓の欄が小さすぎる（{area_w}×{area_h}）");

        deliver(40, 30, first.preview_ticket);
        let again = req_rx.try_recv().expect("読み直しを頼んでいない");
        assert_eq!((again.id, again.purpose, again.max_width, again.max_height), (id, Purpose::Preview, area_w, area_h));
        assert!(matches!(&again.source, ImageSource::Blob(name) if name == "x.webp"), "{:?}", again.source);
        assert!(ctx.view.borrow().preview_image.is_some(), "届くまで今の画像を出したままにしていない");

        deliver(fit_w, fit_h, again.preview_ticket);
        assert!(req_rx.try_recv().is_err(), "欄に収まる最大の大きさなのに頼み直した");
    }

    /// 大きすぎて展開しなかったプレビューの画像は、画像の子窓の代わりに省略の注記を出す。
    /// 待っていない項目の結果なら何もしない。
    #[test]
    fn too_large_preview_image_shows_note_instead() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let (req_rx, res_tx) = attach_test_link(&window);
        let (edit, image) = unsafe {
            (GetDlgItem(Some(window.hwnd()), ID_PREVIEW).unwrap(), GetDlgItem(Some(window.hwnd()), ID_IMAGE).unwrap())
        };
        let visible = |h: HWND| unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowLongW(h, windows::Win32::UI::WindowsAndMessaging::GWL_STYLE) as u32 & WS_VISIBLE.0 != 0 };
        let text = |h: HWND| {
            let mut buf = [0u16; 256];
            let n = unsafe { GetWindowTextW(h, &mut buf) } as usize;
            String::from_utf16_lossy(&buf[..n])
        };
        let ctx = unsafe { ctx_ref(window.hwnd()).unwrap() };
        let id = Uuid::new_v4();
        show_preview_image(window.hwnd(), id, ImageSource::Blob("x.webp".into()));
        let preview_ticket = req_rx.try_recv().expect("読み込みを頼んでいない").preview_ticket;
        let generation = ctx.view.borrow().generation;
        let too_large = |id| LoadedImage {
            generation,
            id,
            purpose: Purpose::Preview,
            preview_ticket,
            content: ImageContent::TooLarge { width: 20000, height: 15000 },
        };

        res_tx.send(too_large(Uuid::new_v4())).unwrap();
        unsafe { SendMessageW(window.hwnd(), WM_APP_IMAGE, None, None) };
        assert!(!visible(edit) && visible(image), "待っていない項目の結果で切り替えた");

        res_tx.send(too_large(id)).unwrap();
        unsafe { SendMessageW(window.hwnd(), WM_APP_IMAGE, None, None) };
        assert!(visible(edit) && !visible(image));
        assert_eq!(text(edit), crate::native::model::image_too_large_note(20000, 15000));
    }

    /// 読めなかったプレビューの画像は、画像の子窓の代わりに「（プレビューなし）」を薄い文字で出す。
    /// 同じ項目を頼み直した後に届いた古い依頼の結果では切り替えない。
    /// ほかの文字に替えると、文字の色を既定に戻す。
    #[test]
    fn unreadable_preview_image_shows_no_preview_in_gray() {
        use windows::Win32::Graphics::Gdi::{CreateCompatibleDC, DeleteDC, GetTextColor};
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let (req_rx, res_tx) = attach_test_link(&window);
        let (edit, image) = unsafe {
            (GetDlgItem(Some(window.hwnd()), ID_PREVIEW).unwrap(), GetDlgItem(Some(window.hwnd()), ID_IMAGE).unwrap())
        };
        let visible = |h: HWND| unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowLongW(h, windows::Win32::UI::WindowsAndMessaging::GWL_STYLE) as u32 & WS_VISIBLE.0 != 0 };
        let text = |h: HWND| {
            let mut buf = [0u16; 256];
            let n = unsafe { GetWindowTextW(h, &mut buf) } as usize;
            String::from_utf16_lossy(&buf[..n])
        };
        // プレビューの EDIT が問い合わせたときに返す文字の色
        let text_color = || unsafe {
            let dc = CreateCompatibleDC(None);
            assert!(!dc.is_invalid());
            SendMessageW(window.hwnd(), WM_CTLCOLORSTATIC, Some(WPARAM(dc.0 as usize)), Some(LPARAM(edit.0 as isize)));
            let color = GetTextColor(dc);
            let _ = DeleteDC(dc);
            color.0
        };
        let gray = unsafe { GetSysColor(COLOR_GRAYTEXT) };
        let ctx = unsafe { ctx_ref(window.hwnd()).unwrap() };
        let id = Uuid::new_v4();
        let generation = ctx.view.borrow().generation;
        let unreadable = |preview_ticket| LoadedImage {
            generation,
            id,
            purpose: Purpose::Preview,
            preview_ticket,
            content: ImageContent::Unreadable,
        };
        // 同じ項目を2回頼む（選び直し・DPI の変更・読み直しで起こる）
        show_preview_image(window.hwnd(), id, ImageSource::Blob("missing.webp".into()));
        let old = req_rx.try_recv().expect("読み込みを頼んでいない").preview_ticket;
        show_preview_image(window.hwnd(), id, ImageSource::Blob("missing.webp".into()));
        let current = req_rx.try_recv().expect("読み込みを頼んでいない").preview_ticket;
        assert_ne!(old, current);

        res_tx.send(unreadable(old)).unwrap();
        unsafe { SendMessageW(window.hwnd(), WM_APP_IMAGE, None, None) };
        assert!(!visible(edit) && visible(image), "古い依頼の結果で切り替えた");
        assert!(!ctx.preview_none.get());

        res_tx.send(unreadable(current)).unwrap();
        unsafe { SendMessageW(window.hwnd(), WM_APP_IMAGE, None, None) };
        assert!(visible(edit) && !visible(image));
        assert_eq!(text(edit), crate::native::model::NO_PREVIEW);
        assert_eq!(text_color(), gray);

        set_preview_text(window.hwnd(), "text");
        assert_eq!(text(edit), "text");
        assert!(!ctx.preview_none.get(), "ほかの文字に替えても薄い文字のまま");
        assert_ne!(text_color(), gray, "前提: 既定の文字の色がシステムの薄い文字の色と同じ");

        set_preview_none(window.hwnd());
        assert_eq!(text(edit), crate::native::model::NO_PREVIEW);
        assert_eq!(text_color(), gray);
    }

    fn item_height(list: HWND) -> i32 {
        use windows::Win32::UI::Controls::{LVIR_BOUNDS, LVM_GETITEMRECT};
        let mut rc = RECT { left: LVIR_BOUNDS as i32, ..Default::default() };
        unsafe { SendMessageW(list, LVM_GETITEMRECT, Some(WPARAM(0)), Some(LPARAM(&mut rc as *mut _ as isize))) };
        rc.bottom - rc.top
    }

    /// DPI の変更（ここでは 144 DPI = 150% を作って送る）で、フォント・行の高さ・検索欄の高さを
    /// 作り直し、画像を捨てて世代を進め、ハンドラへ知らせる。
    #[test]
    fn dpi_change_rebuilds_metrics_and_row_height() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder) = create_test_window();
        set_rows(window.hwnd(), vec![row("a"), row("b")], false);
        let ctx = unsafe { ctx_ref(window.hwnd()).unwrap() };
        let before_dpi = unsafe { GetDpiForWindow(window.hwnd()) };
        let (before_row, before_search) = {
            let m = ctx.metrics.borrow();
            (m.row_height, m.search_height)
        };
        assert_eq!(item_height(list(&window)), before_row, "作成時の行の高さ");
        ctx.view.borrow_mut().thumbs.insert(Uuid::new_v4(), None);
        let generation = ctx.view.borrow().generation;

        let dpi = before_dpi * 3 / 2;
        let suggested = RECT { left: 100, top: 100, right: 700, bottom: 550 };
        unsafe {
            SendMessageW(
                window.hwnd(),
                WM_DPICHANGED,
                Some(WPARAM((dpi | (dpi << 16)) as usize)),
                Some(LPARAM(&suggested as *const _ as isize)),
            );
        }
        let m = ctx.metrics.borrow();
        assert_eq!(m.icon_size, crate::menu_tooltip::scale_for_dpi(ICON_SIZE, dpi));
        assert!(m.row_height > before_row && m.search_height > before_search);
        assert_eq!(item_height(list(&window)), m.row_height, "一覧の行の高さが変わっていない");
        assert!(ctx.view.borrow().thumbs.is_empty());
        assert_eq!(ctx.view.borrow().generation, generation + 1);
        assert_eq!(recorder.metrics_changes.get(), 1);
    }

    /// 窓のアイコンの一辺（px）。`GetIconInfo` の色のビットマップ（無ければマスク）の幅。
    fn icon_width(icon: HICON) -> i32 {
        use windows::Win32::Graphics::Gdi::{GetObjectW, BITMAP};
        use windows::Win32::UI::WindowsAndMessaging::{GetIconInfo, ICONINFO};
        let mut info = ICONINFO::default();
        unsafe { GetIconInfo(icon, &mut info) }.expect("アイコンの情報を取れない");
        let bitmap = if info.hbmColor.is_invalid() { info.hbmMask } else { info.hbmColor };
        let mut bm = BITMAP::default();
        unsafe {
            GetObjectW(bitmap.into(), std::mem::size_of::<BITMAP>() as i32, Some((&mut bm as *mut BITMAP).cast()));
            for b in [info.hbmColor, info.hbmMask] {
                if !b.is_invalid() {
                    let _ = DeleteObject(b.into());
                }
            }
        }
        bm.bmWidth
    }

    /// 窓に exe のアイコン（小・大）が付き、その大きさは窓の DPI のシステムの寸法に合う。DPI が
    /// 変わると、新しい大きさのアイコンへ差し替わる。
    #[test]
    fn window_icons_are_app_icon_sized_for_dpi() {
        use windows::Win32::UI::WindowsAndMessaging::WM_GETICON;
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let hwnd = window.hwnd();
        let icons = || {
            [ICON_SMALL, ICON_BIG]
                .map(|kind| HICON(unsafe { SendMessageW(hwnd, WM_GETICON, Some(WPARAM(kind as usize)), None) }.0 as *mut _))
        };
        let expected = |dpi: u32| [SM_CXSMICON, SM_CXICON].map(|metric| unsafe { GetSystemMetricsForDpi(metric, dpi) });
        let base = unsafe { GetDpiForWindow(hwnd) };
        let before = icons();
        assert!(before.iter().all(|i| !i.is_invalid()), "窓にアイコンが付いていない");
        assert_eq!(before.map(icon_width), expected(base));

        let dpi = base * 3 / 2;
        let suggested = RECT { left: 100, top: 100, right: 700, bottom: 550 };
        unsafe {
            SendMessageW(hwnd, WM_DPICHANGED, Some(WPARAM((dpi | (dpi << 16)) as usize)), Some(LPARAM(&suggested as *const _ as isize)));
        }
        let after = icons();
        assert!(after.iter().all(|i| !i.is_invalid()));
        assert_ne!(after, before, "DPI の変更でアイコンが差し替わっていない");
        assert_eq!(after.map(icon_width), expected(dpi));
    }

    /// DPI の変更を繰り返しても、フォント・イメージリストの GDI オブジェクトが増え続けない
    /// （差し替えた古いフォントと、外したイメージリストを解放している）。窓のアイコン（USER
    /// オブジェクト）も増え続けない。
    #[test]
    fn repeated_dpi_changes_do_not_leak_gdi_objects() {
        use windows::Win32::System::Threading::{GetCurrentProcess, GetGuiResources, GR_GDIOBJECTS, GR_USEROBJECTS};
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        set_rows(window.hwnd(), vec![row("a")], false);
        let base = unsafe { GetDpiForWindow(window.hwnd()) };
        let suggested = RECT { left: 100, top: 100, right: 700, bottom: 550 };
        let change = |dpi: u32| unsafe {
            SendMessageW(
                window.hwnd(),
                WM_DPICHANGED,
                Some(WPARAM((dpi | (dpi << 16)) as usize)),
                Some(LPARAM(&suggested as *const _ as isize)),
            );
        };
        // 1往復目で DC のキャッシュなどが落ち着いてから数える
        change(base * 3 / 2);
        change(base);
        let before = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
        let before_user = unsafe { GetGuiResources(GetCurrentProcess(), GR_USEROBJECTS) };
        for _ in 0..5 {
            change(base * 3 / 2);
            change(base * 2);
            change(base);
        }
        let after = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
        let after_user = unsafe { GetGuiResources(GetCurrentProcess(), GR_USEROBJECTS) };
        assert_eq!(after, before, "DPI の変更で GDI オブジェクトが増えた: {before} → {after}");
        assert_eq!(after_user, before_user, "DPI の変更で USER オブジェクト（アイコン）が増えた: {before_user} → {after_user}");
    }

    /// 回帰: どの子コントロールも Tab（と全キー）を自分で求めない。求めると IsDialogMessageW の
    /// Tab での移動がそこで止まる（複数行の EDIT のプレビューで起きていた）。
    #[test]
    fn no_child_control_claims_tab_so_tab_navigation_can_pass() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        for id in [ID_TREE, ID_SEARCH, ID_LIST, ID_PREVIEW] {
            let child = unsafe { GetDlgItem(Some(window.hwnd()), id).unwrap() };
            let code = unsafe { SendMessageW(child, WM_GETDLGCODE, None, None).0 as u32 };
            assert_eq!(code & (DLGC_WANTALLKEYS | DLGC_WANTTAB), 0, "子コントロール {id} が Tab を求めている: {code:#x}");
        }
    }

    /// ツリーの作り直しでは選択の変化をハンドラへ伝えず、ユーザーの選択だけを伝える。
    #[test]
    fn set_tree_does_not_report_its_own_selection_changes() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder) = create_test_window();
        let folder = Uuid::new_v4();
        let nodes = vec![
            TreeNode { label: "履歴".into(), source: Source::History, children: vec![] },
            TreeNode {
                label: "ピン留め".into(),
                source: Source::Pinned(None),
                children: vec![TreeNode { label: "f".into(), source: Source::Pinned(Some(folder)), children: vec![] }],
            },
        ];
        set_tree(window.hwnd(), &nodes, Source::Pinned(Some(folder)));
        set_tree(window.hwnd(), &nodes, Source::History);
        assert!(recorder.selected.borrow().is_empty());
    }

    // --- 一覧の操作 ---

    /// 行を入れ、`index` の行を選んで、窓を（前面にせずに）表示する。
    fn window_with_rows(labels: &[&str], index: usize) -> (ViewerWindow, Rc<Recorder>, Vec<Uuid>) {
        let (window, recorder) = create_test_window();
        let rows: Vec<Row> = labels.iter().map(|l| row(l)).collect();
        let ids = rows.iter().map(|r| r.id).collect();
        set_rows(window.hwnd(), rows, false);
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE;
            let _ = ShowWindow(window.hwnd(), SW_SHOWNOACTIVATE);
            set_item_state(list(&window), index as i32, LVIS_SELECTED.0 | LVIS_FOCUSED.0);
        }
        pump(window.hwnd(), 20);
        (window, recorder, ids)
    }

    /// キーの押下をダイアログの規則（`IsDialogMessageW`）に通す（メインのメッセージループと同じ）。
    fn press_through_dialog_manager(window: &ViewerWindow, target: HWND, vk: u16) {
        use windows::Win32::UI::WindowsAndMessaging::{IsDialogMessageW, WM_KEYDOWN};
        let msg = MSG { hwnd: target, message: WM_KEYDOWN, wParam: WPARAM(vk as usize), ..Default::default() };
        unsafe {
            let _ = IsDialogMessageW(window.hwnd(), &msg);
        }
    }

    #[test]
    fn enter_in_list_activates_selected_row_but_not_in_search() {
        use windows::Win32::UI::Input::KeyboardAndMouse::VK_RETURN;
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, ids) = window_with_rows(&["a", "b"], 1);
        let list = list(&window);
        unsafe {
            let _ = SetFocus(Some(list));
            assert_eq!(GetFocus(), list, "前提: 一覧にフォーカスを置けない");
        }
        press_through_dialog_manager(&window, list, VK_RETURN.0);
        assert_eq!(recorder.activated.borrow().as_slice(), [ids[1]]);
        let search = search(&window);
        unsafe {
            let _ = SetFocus(Some(search));
        }
        press_through_dialog_manager(&window, search, VK_RETURN.0);
        assert_eq!(recorder.activated.borrow().len(), 1, "検索欄の Enter で送った");
    }

    #[test]
    fn delete_key_in_list_reports_selected_row() {
        use windows::Win32::UI::WindowsAndMessaging::WM_KEYDOWN;
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, ids) = window_with_rows(&["a", "b", "c"], 2);
        let list = list(&window);
        unsafe {
            let _ = SetFocus(Some(list));
            SendMessageW(list, WM_KEYDOWN, Some(WPARAM(VK_DELETE.0 as usize)), Some(LPARAM(0)));
        }
        assert_eq!(recorder.deleted.borrow().as_slice(), [ids[2]]);
    }

    /// 行の上のダブルクリックは、Enter と同じく選んでいる先頭の行を送る（複数選択は先頭だけ）。
    /// 行の無い所のダブルクリックでは送らない。
    #[test]
    fn double_click_on_row_activates_first_selected_row_but_not_on_empty_area() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, ids) = window_with_rows(&["a", "b", "c"], 1);
        let list = list(&window);
        unsafe { set_item_state(list, 2, LVIS_SELECTED.0) };
        for item in [2, -1] {
            let mut nm = NMITEMACTIVATE {
                hdr: NMHDR { hwndFrom: list, idFrom: ID_LIST as usize, code: NM_DBLCLK },
                iItem: item,
                ..Default::default()
            };
            unsafe {
                SendMessageW(window.hwnd(), WM_NOTIFY, Some(WPARAM(ID_LIST as usize)), Some(LPARAM(&mut nm as *mut _ as isize)));
            }
        }
        assert_eq!(recorder.activated.borrow().as_slice(), [ids[1]]);
    }

    /// 削除・押し出しで選んでいた行が消えたら、同じ位置の行を選ぶ（はみ出すなら最後の行）。
    /// 何も選んでいなければ何も選ばない。
    #[test]
    fn set_rows_keeps_position_when_selected_row_disappears() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let rows: Vec<Row> = ["a", "b", "c"].iter().map(|l| row(l)).collect();
        let [a, b, c] = [rows[0].clone(), rows[1].clone(), rows[2].clone()];
        set_rows(window.hwnd(), rows, false);
        unsafe { set_item_state(list(&window), 1, LVIS_SELECTED.0 | LVIS_FOCUSED.0) };
        assert_eq!(set_rows(window.hwnd(), vec![a.clone(), c.clone()], false), Some(c.id), "次の行を選んでいない");
        assert_eq!(set_rows(window.hwnd(), vec![a.clone()], false), Some(a.id), "最後の行を選んでいない");
        assert_eq!(set_rows(window.hwnd(), vec![], false), None);
        assert_eq!(set_rows(window.hwnd(), vec![b, a], false), None, "何も選んでいなかったのに選んだ");
    }

    #[test]
    fn row_menu_items_follow_spec_and_commands_round_trip() {
        let _gui = crate::tray::lock_gui_resource_tests();
        use windows::Win32::UI::WindowsAndMessaging::{GetMenuItemCount, GetSubMenu};
        let minimal = build_row_menu(&RowMenu { can_pin: false, has_image: false, has_text: false, ..RowMenu::default() }, 96).unwrap();
        let full = build_row_menu(&RowMenu { can_pin: true, has_image: true, has_text: true, ..RowMenu::default() }, 96).unwrap();
        unsafe {
            assert_eq!(GetMenuItemCount(Some(minimal.handle())), 2, "送る・削除だけのはず");
            // 送る・ピン留めに追加・画像を関連付けで開く・画像を書き出してフォルダで表示・テキスト変換・削除
            assert_eq!(GetMenuItemCount(Some(full.handle())), 6);
            let sub = GetSubMenu(full.handle(), 4);
            assert_eq!(GetMenuItemCount(Some(sub)), TextTransform::ALL.len() as i32);
        }
        let spec = RowMenu::default();
        assert_eq!(row_command(MENU_SEND, &spec), Some(RowCommand::Send));
        assert_eq!(row_command(MENU_PIN, &spec), Some(RowCommand::Pin(None)));
        assert_eq!(row_command(MENU_OPEN_IMAGE, &spec), Some(RowCommand::OpenImage));
        assert_eq!(row_command(MENU_OPEN_IMAGE_LOCATION, &spec), Some(RowCommand::OpenImageLocation));
        assert_eq!(row_command(MENU_DELETE, &spec), Some(RowCommand::Delete));
        // 名前の変更はピン留めの行のメニューだけ
        assert_eq!(row_command(MENU_RENAME, &spec), None);
        let pinned = RowMenu { current: Some(None), ..RowMenu::default() };
        assert_eq!(row_command(MENU_RENAME, &pinned), Some(RowCommand::Rename));
        let pinned_menu = build_row_menu(&pinned, 96).unwrap();
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::GetMenuItemID;
            let ids: Vec<u32> = (0..GetMenuItemCount(Some(pinned_menu.handle()))).map(|i| GetMenuItemID(pinned_menu.handle(), i)).collect();
            assert_eq!(ids, [MENU_SEND as u32, MENU_RENAME as u32, MENU_DELETE as u32]);
        }
        for (i, t) in TextTransform::ALL.iter().enumerate() {
            assert_eq!(row_command(MENU_TRANSFORM_BASE + i, &spec), Some(RowCommand::Transform(*t)));
        }
        assert_eq!(row_command(0, &spec), None);
        assert_eq!(row_command(MENU_TRANSFORM_BASE + TextTransform::ALL.len(), &spec), None);
        assert!(MENU_TRANSFORM_BASE + TextTransform::ALL.len() <= MENU_TARGET_BASE, "テキスト変換と入れる先の ID が重なる");
    }

    /// 入れる先（ルートと、入れ子のフォルダ。名前に `&` を含むもの）。
    fn targets_with_folders() -> (Vec<crate::native::model::PinTarget>, Uuid, Uuid) {
        use crate::native::model::PinTarget;
        let (work, nested) = (Uuid::new_v4(), Uuid::new_v4());
        use crate::menu_draw::TreeGuide;
        let targets = vec![
            PinTarget { folder: None, title: crate::native::model::PIN_ROOT_LABEL.into(), depth: 0, guide: vec![] },
            PinTarget { folder: Some(work), title: "仕事&遊び".into(), depth: 1, guide: vec![TreeGuide::Last] },
            PinTarget {
                folder: Some(nested),
                title: "中".into(),
                depth: 2,
                guide: vec![TreeGuide::Blank, TreeGuide::Last],
            },
        ];
        (targets, work, nested)
    }

    /// フォルダがあると、履歴の行の「ピン留めに追加」は入れる先の子メニュー、ピン留めの行には「移動」の
    /// 子メニュー（今いる所は灰色）が出る。子メニューの項目の ID から、ピン留め・移動の入れる先へ戻る。
    /// フォルダが無ければ「ピン留めに追加」は1項目のまま、「移動」は出さない。
    #[test]
    fn row_menu_offers_pin_targets_and_move_when_folders_exist() {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetMenuItemCount, GetMenuItemID, GetMenuState, GetSubMenu, MF_BYPOSITION,
        };
        let _gui = crate::tray::lock_gui_resource_tests();
        let (targets, work, nested) = targets_with_folders();
        let root_only = vec![targets[0].clone()];

        let history = RowMenu { can_pin: true, pin_targets: targets.clone(), ..RowMenu::default() };
        let pinned = RowMenu { can_pin: false, pin_targets: targets.clone(), current: Some(Some(work)), ..RowMenu::default() };
        let history_no_folders = RowMenu { can_pin: true, pin_targets: root_only.clone(), ..RowMenu::default() };
        let pinned_no_folders = RowMenu { can_pin: false, pin_targets: root_only, current: Some(None), ..RowMenu::default() };
        unsafe {
            // 送る・ピン留めに追加（子メニュー）・削除
            let menu = build_row_menu(&history, 96).unwrap();
            assert_eq!(GetMenuItemCount(Some(menu.handle())), 3);
            let sub = GetSubMenu(menu.handle(), 1);
            assert_eq!(GetMenuItemCount(Some(sub)), 3);
            let ids: Vec<usize> = (0..3).map(|i| GetMenuItemID(sub, i) as usize).collect();
            assert_eq!(ids, [MENU_TARGET_BASE, MENU_TARGET_BASE + 1, MENU_TARGET_BASE + 2]);
            for pos in 0..3 {
                assert_eq!(GetMenuState(sub, pos, MF_BYPOSITION) & MF_GRAYED.0, 0, "ピン留めに追加に灰色がある");
            }
            assert_eq!(row_command(ids[0], &history), Some(RowCommand::Pin(None)));
            assert_eq!(row_command(ids[2], &history), Some(RowCommand::Pin(Some(nested))));

            // 送る・移動（子メニュー）・名前の変更・削除。今いる「仕事&遊び」は灰色
            let menu = build_row_menu(&pinned, 96).unwrap();
            assert_eq!(GetMenuItemCount(Some(menu.handle())), 4);
            assert_eq!(GetMenuItemID(menu.handle(), 2) as usize, MENU_RENAME);
            let sub = GetSubMenu(menu.handle(), 1);
            assert_eq!(GetMenuItemCount(Some(sub)), 3);
            assert_eq!(GetMenuState(sub, 0, MF_BYPOSITION) & MF_GRAYED.0, 0);
            assert_ne!(GetMenuState(sub, 1, MF_BYPOSITION) & MF_GRAYED.0, 0, "今いる所が灰色でない");
            assert_eq!(row_command(MENU_TARGET_BASE, &pinned), Some(RowCommand::Move(None)));
            assert_eq!(row_command(MENU_TARGET_BASE + 2, &pinned), Some(RowCommand::Move(Some(nested))));
            assert_eq!(row_command(MENU_TARGET_BASE + 3, &pinned), None, "並びの外の ID を受け付けた");

            let menu = build_row_menu(&history_no_folders, 96).unwrap();
            assert_eq!(GetMenuItemCount(Some(menu.handle())), 3);
            assert_eq!(GetMenuItemID(menu.handle(), 1) as usize, MENU_PIN, "フォルダが無いのに子メニューにした");
            let menu = build_row_menu(&pinned_no_folders, 96).unwrap();
            assert_eq!(GetMenuItemCount(Some(menu.handle())), 3, "フォルダが無いのに移動を出した");

            // フォルダの行: 開く・移動（子メニュー）・名前の変更...・削除...。今いる所は灰色で、選ぶと移動になる
            let folder = RowMenu { folder: true, ..pinned.clone() };
            let menu = build_row_menu(&folder, 96).unwrap();
            assert_eq!(GetMenuItemCount(Some(menu.handle())), 4);
            assert_eq!(GetMenuItemID(menu.handle(), 0) as usize, MENU_SEND);
            assert_eq!(GetMenuItemID(menu.handle(), 2) as usize, MENU_RENAME);
            let sub = GetSubMenu(menu.handle(), 1);
            assert_eq!(GetMenuItemCount(Some(sub)), 3);
            assert_ne!(GetMenuState(sub, 1, MF_BYPOSITION) & MF_GRAYED.0, 0, "今いる所が灰色でない");
            assert_eq!(row_command(MENU_TARGET_BASE + 2, &folder), Some(RowCommand::Move(Some(nested))));
            // ほかに入れる先が無ければ（ルートだけ）出さない
            let folder_alone = RowMenu { folder: true, ..pinned_no_folders.clone() };
            assert_eq!(GetMenuItemCount(Some(build_row_menu(&folder_alone, 96).unwrap().handle())), 3);
        }
    }

    thread_local! {
        /// メニューのモーダルループの中でタイマーから行うこと（テスト用）
        static IN_MENU: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
        /// 予備のタイマー（メニューが閉じなかったときに閉じてテストを止めない）の ID と、使われたか
        static FALLBACK: Cell<(usize, bool)> = const { Cell::new((0, false)) };
        /// `IN_MENU` を行うタイマーの ID（メニューを出さなかったとき、後のテストの仕掛けを奪わないよう片付ける）
        static PRIMARY: Cell<usize> = const { Cell::new(0) };
    }

    unsafe extern "system" fn run_in_menu(_hwnd: HWND, _msg: u32, id: usize, _time: u32) {
        unsafe {
            let _ = KillTimer(None, id);
        }
        if let Some(f) = IN_MENU.with(|slot| slot.borrow_mut().take()) {
            f();
        }
    }

    unsafe extern "system" fn fallback_close(_hwnd: HWND, _msg: u32, id: usize, _time: u32) {
        unsafe {
            let _ = KillTimer(None, id);
            let _ = EndMenu();
        }
        FALLBACK.with(|f| f.set((0, true)));
    }

    /// メニューを出す前に、モーダルループの中で `f` を行う仕掛けを置く。あわせて、3秒たっても
    /// メニューが閉じていなければ閉じる予備のタイマーを置く（`finish_menu_test` で片付ける）。
    fn during_menu(f: impl FnOnce() + 'static) {
        IN_MENU.with(|slot| *slot.borrow_mut() = Some(Box::new(f)));
        unsafe {
            PRIMARY.with(|p| p.set(SetTimer(None, 0, 150, Some(run_in_menu))));
            let fallback = SetTimer(None, 0, 3000, Some(fallback_close));
            FALLBACK.with(|f| f.set((fallback, false)));
        }
    }

    /// タイマー（予備と、`IN_MENU` を行うもの）を片付け、予備が使われた（= 仕掛けではメニューが閉じ
    /// なかった）かを返す。使われなかった `IN_MENU` は残す（呼び出し側が確かめて片付ける）。
    fn finish_menu_test() -> bool {
        let (id, used) = FALLBACK.with(|f| f.replace((0, false)));
        let primary = PRIMARY.with(|p| p.replace(0));
        unsafe {
            for timer in [id, primary] {
                if timer != 0 {
                    let _ = KillTimer(None, timer);
                }
            }
        }
        used
    }

    fn context_menu_at(window: &ViewerWindow, x: i32, y: i32) {
        let lparam = ((y as u16 as u32) << 16 | (x as u16 as u32)) as isize;
        unsafe {
            SendMessageW(window.hwnd(), WM_CONTEXTMENU, Some(WPARAM(list(window).0 as usize)), Some(LPARAM(lparam)));
        }
    }

    /// キーボード（Shift+F10 など）から開いたメニューは、隠す・終了の要求（`cancel_modal`）で閉じ、
    /// 操作は伝えない。メニューの表示中フラグは下りる。
    #[test]
    fn keyboard_menu_is_closed_by_cancel_without_command() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, ids) = window_with_rows(&["a", "b"], 1);
        *recorder.menu.borrow_mut() = Some(RowMenu { can_pin: true, has_image: false, has_text: true, ..RowMenu::default() });
        let hwnd = window.hwnd();
        let (measured, drawn) = crate::menu_draw::test_support::counts();
        during_menu(move || {
            assert!(unsafe { ctx_ref(hwnd) }.unwrap().menu_open.get(), "メニューの表示中になっていない");
            cancel_modal(hwnd);
        });
        context_menu_at(&window, -1, -1);
        assert!(!finish_menu_test(), "cancel_modal でメニューが閉じなかった");
        // 右クリックメニュー（送る・ピン留め・テキスト変換・削除の4行）は自前描画の経路を通る
        let (measured_after, drawn_after) = crate::menu_draw::test_support::counts();
        assert!(measured_after >= measured + 4 && drawn_after >= drawn + 4, "自前描画の経路を通っていない");
        assert_eq!(recorder.menu_requests.borrow().as_slice(), [ids[1]]);
        assert!(recorder.commands.borrow().is_empty());
        assert!(!modal_is_open(hwnd));
        // 閉じた後に自分を起こす（メニューの表示中に保留した通知を出させる）
        let before = recorder.wakes.get();
        pump(hwnd, 30);
        assert_eq!(recorder.wakes.get(), before + 1, "閉じた後に起こしていない");
    }

    /// メニューのモーダルループの中で終了の要求が来たとき（トレイの「終了」・トレイなしで閉じる）、
    /// アプリは `cancel_modal` でメニューを閉じてから `PostQuitMessage` を呼ぶ（`App::begin_exit`）。
    /// この順で、メニューは閉じ、WM_QUIT が残る（メインのメッセージループが抜けられる）。
    /// `PostQuitMessage` だけではメニューは閉じない（2026-09-23 にこのテストの初版で確かめた。
    /// メッセージボックスとは違う）ので、`cancel_modal` を省かない。
    #[test]
    fn menu_closed_by_cancel_before_quit_leaves_wm_quit() {
        use windows::Win32::UI::WindowsAndMessaging::{PostQuitMessage, WM_QUIT};
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, _ids) = window_with_rows(&["a"], 0);
        *recorder.menu.borrow_mut() = Some(RowMenu { can_pin: false, has_image: false, has_text: false, ..RowMenu::default() });
        let hwnd = window.hwnd();
        during_menu(move || {
            cancel_modal(hwnd);
            unsafe { PostQuitMessage(0) };
        });
        context_menu_at(&window, -1, -1);
        assert!(!finish_menu_test(), "メニューが閉じなかった");
        assert!(recorder.commands.borrow().is_empty());
        // メインのメッセージループと同じく、残っているメッセージを順に処理すると WM_QUIT が来る
        // （PostQuitMessage の WM_QUIT は、ほかの投稿されたメッセージ（閉じた後の起床など）の後に届く）
        let mut quit = false;
        for _ in 0..100 {
            let mut msg = MSG::default();
            if !unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                break;
            }
            if msg.message == WM_QUIT {
                quit = true;
                break;
            }
            unsafe { DispatchMessageW(&msg) };
        }
        assert!(quit, "WM_QUIT が残っていない");
    }

    /// マウスで行の上を右クリックすると、その行だけを選んでからメニューを出す。行の無い所では
    /// メニューを出さない。
    #[test]
    fn right_click_selects_row_first_and_empty_area_shows_no_menu() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, ids) = window_with_rows(&["a", "b"], 0);
        *recorder.menu.borrow_mut() = Some(RowMenu { can_pin: true, has_image: false, has_text: false, ..RowMenu::default() });
        let list = list(&window);
        let hwnd = window.hwnd();
        let screen = |index: usize, below: bool| unsafe {
            let mut rc = RECT { left: LVIR_BOUNDS as i32, ..Default::default() };
            SendMessageW(list, LVM_GETITEMRECT, Some(WPARAM(index)), Some(LPARAM(&mut rc as *mut _ as isize)));
            let height = rc.bottom - rc.top;
            let mut pt = POINT { x: rc.left + 10, y: if below { rc.bottom + height } else { rc.top + height / 2 } };
            let _ = ClientToScreen(list, &mut pt);
            pt
        };
        let on_second = screen(1, false);
        during_menu(move || cancel_modal(hwnd));
        context_menu_at(&window, on_second.x, on_second.y);
        assert!(!finish_menu_test(), "cancel_modal でメニューが閉じなかった");
        assert_eq!(recorder.menu_requests.borrow().as_slice(), [ids[1]]);
        assert_eq!(selected_index(list), Some(1), "右クリックした行を選んでいない");

        let below_last = screen(1, true);
        // 誤って出た場合に止まらないよう、仕掛けは置いておく
        during_menu(move || cancel_modal(hwnd));
        context_menu_at(&window, below_last.x, below_last.y);
        finish_menu_test();
        IN_MENU.with(|slot| slot.borrow_mut().take());
        assert_eq!(recorder.menu_requests.borrow().len(), 1, "行の無い所でメニューを出した");
    }

    // --- 窓とメニューバー ---

    /// メニューバーは「ツール」「ヘルプ」。どの項目も選べる。最前面に表示の
    /// 切り替えは、窓の最前面の属性とメニューのチェックを合わせる。
    #[test]
    fn menu_bar_and_topmost() {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetMenuItemCount, GetMenuState, GetSubMenu, GetWindowLongW, GWL_EXSTYLE, WS_EX_TOPMOST,
        };
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let hwnd = window.hwnd();
        unsafe {
            let bar = GetMenu(hwnd);
            assert_eq!(GetMenuItemCount(Some(bar)), 2);
            let tools = GetSubMenu(bar, 0);
            let state = |menu: HMENU, id: u16| GetMenuState(menu, id as u32, MF_BYCOMMAND);
            assert_eq!(state(tools, CMD_SETTINGS) & MF_GRAYED.0, 0, "設定が選べない");
            assert_eq!(state(tools, CMD_CLEAR_HISTORY) & MF_GRAYED.0, 0);
            assert_eq!(state(tools, CMD_CLEAR_CLIPBOARD) & MF_GRAYED.0, 0);
            assert_eq!(state(GetSubMenu(bar, 1), CMD_ABOUT) & MF_GRAYED.0, 0, "バージョン情報が選べない");

            let topmost = || GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 != 0;
            assert!(!topmost());
            set_topmost(hwnd, true);
            assert!(topmost());
            assert_ne!(state(tools, CMD_TOPMOST) & MF_CHECKED.0, 0);
            set_topmost(hwnd, false);
            assert!(!topmost());
            assert_eq!(state(tools, CMD_TOPMOST) & MF_CHECKED.0, 0);
        }
    }

    /// メニューバーのドロップダウン（「ツール」）を開くと、項目を自前で描く。ドロップダウンのアクセスキー
    /// （`(&H)` など）は `WM_MENUCHAR` で位置が返る。
    #[test]
    fn menu_bar_dropdown_is_owner_drawn_and_mnemonics_resolve() {
        use windows::Win32::UI::WindowsAndMessaging::{GetSubMenu, SW_SHOWNOACTIVATE};
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let hwnd = window.hwnd();
        let tools = unsafe { GetSubMenu(GetMenu(hwnd), 0) };
        let ask = |c: char| unsafe {
            SendMessageW(hwnd, WM_MENUCHAR, Some(WPARAM(c as usize | (0x10 << 16))), Some(LPARAM(tools.0 as isize)))
        };
        // 位置: 0 設定・1 区切り線・2 履歴のクリア・3 クリップボードのクリア・4 データのチェック・5 区切り線・6 最前面
        assert_eq!(ask('h'), LRESULT((2 << 16) | 2));
        assert_eq!(ask('C'), LRESULT((2 << 16) | 3));
        assert_eq!(ask('d'), LRESULT((2 << 16) | 4));
        assert_eq!(ask('t'), LRESULT((2 << 16) | 6));
        assert_eq!(ask('s'), LRESULT((2 << 16) | 0), "「設定」を候補にしていない");

        // ドロップダウンのメニューを持ち主（ビューア窓）の下で出し、項目が自前描画の経路を通ることを見る
        // （テストの窓はアクティブにしないので、メニューバーからは開かず、ドロップダウンを直接出す）
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        let (measured, drawn) = crate::menu_draw::test_support::counts();
        during_menu(|| unsafe {
            let _ = EndMenu();
        });
        unsafe {
            let _ = TrackPopupMenu(tools, TPM_RETURNCMD, 100, 100, Some(0), hwnd, None);
        }
        assert!(!finish_menu_test(), "ドロップダウンが閉じなかった");
        let (measured_after, drawn_after) = crate::menu_draw::test_support::counts();
        assert!(measured_after >= measured + 6 && drawn_after >= drawn + 6, "ドロップダウンを自前で描いていない");
    }

    /// DPI が変わるとメニューバーを作り直して付け替え、古いメニューバーは破棄する。「最前面に表示」の
    /// チェックは引き継ぐ。メニューの表示中は作り直さずに保留し、閉じた後の起床で行う。窓の破棄では、
    /// メニューバーを外してから破棄する。
    #[test]
    fn menu_bar_is_rebuilt_on_dpi_change_and_released_with_window() {
        use windows::Win32::UI::WindowsAndMessaging::{GetMenuState, GetSubMenu, IsMenu};
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let hwnd = window.hwnd();
        let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
        set_topmost(hwnd, true);
        let first = unsafe { GetMenu(hwnd) };
        let dpi = unsafe { GetDpiForWindow(hwnd) };

        rebuild_menu_bar(hwnd, ctx, dpi * 2);
        let second = unsafe { GetMenu(hwnd) };
        assert_ne!(first, second, "付け替えていない");
        assert!(!unsafe { IsMenu(first) }.as_bool(), "古いメニューバーを破棄していない");
        assert_eq!(ctx.menu_bar.borrow().as_ref().map(MenuBar::dpi), Some(dpi * 2));
        let checked = unsafe { GetMenuState(GetSubMenu(second, 0), CMD_TOPMOST as u32, MF_BYCOMMAND) } & MF_CHECKED.0;
        assert_ne!(checked, 0, "最前面のチェックを引き継いでいない");

        let last = unsafe { GetMenu(hwnd) };
        drop(window);
        assert!(!unsafe { IsMenu(last) }.as_bool(), "窓の破棄でメニューバーを破棄していない");
    }

    /// 右クリックメニュー・確認ダイアログの表示中に DPI が変わっても、メニューバーは付け替えない（表示中の
    /// メニューを差し替え・解放しない）。閉じた後に、製品側が投げる起床（`WM_APP_WAKE`）で付け替える。
    #[test]
    fn menu_bar_rebuild_waits_until_menu_or_dialog_closes() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, _ids) = window_with_rows(&["a"], 0);
        *recorder.menu.borrow_mut() = Some(RowMenu { can_pin: false, has_image: false, has_text: false, ..RowMenu::default() });
        let hwnd = window.hwnd();
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
        // 表示中に DPI の変更（同じ DPI で、今の位置と大きさを勧める）を届け、そのときの様子を記録する
        let seen: Rc<Cell<Option<(bool, bool)>>> = Rc::default();
        let during = |seen: Rc<Cell<Option<(bool, bool)>>>, before: isize| {
            move || unsafe {
                let mut rc = RECT::default();
                let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut rc);
                let param = WPARAM(((dpi as usize) << 16) | dpi as usize);
                SendMessageW(hwnd, WM_DPICHANGED, Some(param), Some(LPARAM(&rc as *const _ as isize)));
                let pending = ctx_ref(hwnd).is_some_and(|c| c.menu_bar_pending.get());
                seen.set(Some((pending, GetMenu(hwnd).0 as isize == before)));
                cancel_modal(hwnd);
            }
        };

        let before = unsafe { GetMenu(hwnd) };
        during_menu(during(Rc::clone(&seen), before.0 as isize));
        context_menu_at(&window, -1, -1);
        assert!(!finish_menu_test(), "右クリックメニューが閉じなかった");
        assert_eq!(seen.take(), Some((true, true)), "右クリックメニューの表示中に保留しなかった（保留, 付け替えていない）");
        assert_eq!(unsafe { GetMenu(hwnd) }, before, "起床の前に付け替えた");
        pump(hwnd, 30);
        assert!(!ctx.menu_bar_pending.get(), "閉じた後の起床で作り直していない");
        assert_ne!(unsafe { GetMenu(hwnd) }, before);

        let before = unsafe { GetMenu(hwnd) };
        during_dialog(hwnd, during(Rc::clone(&seen), before.0 as isize));
        send_command(hwnd, CMD_CLEAR_HISTORY);
        assert!(!finish_menu_test(), "確認ダイアログが閉じなかった");
        assert_eq!(seen.take(), Some((true, true)), "確認ダイアログの表示中に保留しなかった（保留, 付け替えていない）");
        pump(hwnd, 30);
        assert!(!ctx.menu_bar_pending.get(), "閉じた後の起床で作り直していない");
        assert_ne!(unsafe { GetMenu(hwnd) }, before);
    }

    /// 窓の大きさ: クライアント領域が指定の大きさ（96 DPI 基準）になり、最小の大きさは
    /// クライアント領域で 400×300。最大化・最小化の大きさは覚えない。
    #[test]
    fn client_size_minimum_and_normal_size() {
        use windows::Win32::UI::WindowsAndMessaging::{SIZE_MAXIMIZED, SIZE_MINIMIZED};
        let _gui = crate::tray::lock_gui_resource_tests();
        let recorder = Rc::new(Recorder::default());
        let handler: Rc<dyn ViewerHandler> = recorder.clone();
        let window = ViewerWindow::create("CLCLR viewer size test", (640, 480), handler).unwrap();
        let hwnd = window.hwnd();
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(hwnd, &mut rc);
        }
        assert_eq!((unscale(rc.right, dpi), unscale(rc.bottom, dpi)), (640, 480));
        assert_eq!(normal_size(hwnd), Some((640, 480)));

        let mut info = MINMAXINFO::default();
        unsafe {
            SendMessageW(hwnd, WM_GETMINMAXINFO, None, Some(LPARAM(&mut info as *mut _ as isize)));
        }
        let (w, h) = window_size_for_client((crate::config::VIEWER_MIN_WIDTH, crate::config::VIEWER_MIN_HEIGHT), dpi);
        assert_eq!((info.ptMinTrackSize.x, info.ptMinTrackSize.y), (w, h));

        for kind in [SIZE_MAXIMIZED, SIZE_MINIMIZED] {
            let lparam = (2000 << 16 | 3000) as isize;
            unsafe {
                SendMessageW(hwnd, WM_SIZE, Some(WPARAM(kind as usize)), Some(LPARAM(lparam)));
            }
            assert_eq!(normal_size(hwnd), Some((640, 480)), "最大化・最小化の大きさを覚えた");
        }
        let (w, h) = window_size_for_client((800, 600), dpi);
        unsafe {
            let _ = SetWindowPos(hwnd, None, 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
        assert_eq!(normal_size(hwnd), Some((800, 600)));
    }

    /// ツリーの「履歴」は件数を添えて表示し、件数だけが変わったときは項目の表示名だけを
    /// 書き換える（作り直さない）。項目にはアイコンを付ける。
    #[test]
    fn tree_shows_history_count_and_icons() {
        use windows::Win32::UI::Controls::{TVGN_CHILD, TVGN_NEXT, TVGN_ROOT, TVM_GETITEMW, TVM_GETNEXTITEM};
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let hwnd = window.hwnd();
        let tree = unsafe { GetDlgItem(Some(hwnd), ID_TREE).unwrap() };
        let folder = Uuid::new_v4();
        let nodes = vec![
            TreeNode { label: HISTORY_LABEL.into(), source: Source::History, children: vec![] },
            TreeNode {
                label: "ピン留め".into(),
                source: Source::Pinned(None),
                children: vec![TreeNode { label: "f".into(), source: Source::Pinned(Some(folder)), children: vec![] }],
            },
        ];
        let item = |flag: u32, from: isize| unsafe {
            SendMessageW(tree, TVM_GETNEXTITEM, Some(WPARAM(flag as usize)), Some(LPARAM(from))).0
        };
        let read = |handle: isize| -> (String, i32) {
            let mut buf = [0u16; 64];
            let mut tv = TVITEMW {
                mask: TVIF_HANDLE | TVIF_TEXT | TVIF_IMAGE,
                hItem: HTREEITEM(handle),
                pszText: PWSTR(buf.as_mut_ptr()),
                cchTextMax: buf.len() as i32,
                ..Default::default()
            };
            unsafe {
                SendMessageW(tree, TVM_GETITEMW, None, Some(LPARAM(&mut tv as *mut _ as isize)));
            }
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            (String::from_utf16_lossy(&buf[..len]), tv.iImage)
        };
        set_history_count(hwnd, 3);
        set_tree(hwnd, &nodes, Source::History);
        let history = item(TVGN_ROOT, 0);
        let pinned = item(TVGN_NEXT, history);
        let child = item(TVGN_CHILD, pinned);
        assert_eq!(read(history), ("履歴 (3)".to_string(), 0));
        assert_eq!(read(pinned), ("ピン留め".to_string(), 2));
        assert_eq!(read(child), ("f".to_string(), 1));

        set_history_count(hwnd, 12);
        assert_eq!(item(TVGN_ROOT, 0), history, "件数の変化でツリーを作り直した");
        assert_eq!(read(history).0, "履歴 (12)");
        assert!(!unsafe { ctx_ref(hwnd) }.unwrap().metrics.borrow().tree_images.is_invalid());
    }

    thread_local! {
        /// 確認ダイアログのテストの予備のタイマーが閉じる窓（ビューア）
        static DIALOG_OWNER: Cell<isize> = const { Cell::new(0) };
    }

    unsafe extern "system" fn fallback_cancel_dialog(_hwnd: HWND, _msg: u32, id: usize, _time: u32) {
        unsafe {
            let _ = KillTimer(None, id);
        }
        cancel_modal(HWND(DIALOG_OWNER.with(Cell::get) as *mut _));
        FALLBACK.with(|f| f.set((0, true)));
    }

    /// 確認ダイアログを出す前に、そのモーダルループの中で `f` を行う仕掛けを置く（3秒たっても
    /// 閉じていなければキャンセルで閉じる予備のタイマーも置く。`finish_menu_test` で片付ける）。
    fn during_dialog(owner: HWND, f: impl FnOnce() + 'static) {
        DIALOG_OWNER.with(|o| o.set(owner.0 as isize));
        IN_MENU.with(|slot| *slot.borrow_mut() = Some(Box::new(f)));
        unsafe {
            PRIMARY.with(|p| p.set(SetTimer(None, 0, 300, Some(run_in_menu))));
            let fallback = SetTimer(None, 0, 3000, Some(fallback_cancel_dialog));
            FALLBACK.with(|f| f.set((fallback, false)));
        }
    }

    fn send_command(hwnd: HWND, id: u16) {
        unsafe {
            SendMessageW(hwnd, WM_COMMAND, Some(WPARAM(id as usize)), Some(LPARAM(0)));
        }
    }

    /// 履歴のクリアは確認で「削除」を選んだときだけハンドラへ伝える。表示中は `modal_is_open`。
    /// 隠す要求（閉じる）・`cancel_modal` ではキャンセルになり、伝えない。
    #[test]
    fn clear_history_is_reported_only_after_confirmation() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder) = create_test_window();
        let hwnd = window.hwnd();

        during_dialog(hwnd, move || {
            let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
            assert!(modal_is_open(hwnd), "確認の表示中になっていない");
            unsafe {
                SendMessageW(
                    HWND(ctx.dialog.get() as *mut _),
                    TDM_CLICK_BUTTON.0 as u32,
                    Some(WPARAM(CONFIRM_DELETE as usize)),
                    Some(LPARAM(0)),
                );
            }
        });
        send_command(hwnd, CMD_CLEAR_HISTORY);
        assert!(!finish_menu_test(), "「削除」で閉じなかった");
        assert_eq!(recorder.tools.borrow().as_slice(), [ToolCommand::ClearHistory]);
        assert!(!modal_is_open(hwnd));

        during_dialog(hwnd, move || cancel_modal(hwnd));
        send_command(hwnd, CMD_CLEAR_HISTORY);
        assert!(!finish_menu_test(), "cancel_modal で閉じなかった");

        recorder.hide_on_close.set(true);
        during_dialog(hwnd, move || unsafe {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        });
        send_command(hwnd, CMD_CLEAR_HISTORY);
        assert!(!finish_menu_test(), "閉じる要求で確認が閉じなかった");
        assert_eq!(recorder.tools.borrow().len(), 1, "キャンセルなのに伝えた");
        // 閉じた後に自分を起こす（表示中に保留した通知を出させる）
        let before = recorder.wakes.get();
        pump(hwnd, 30);
        assert!(recorder.wakes.get() > before, "閉じた後に起こしていない");
    }

    /// データのチェックの結果: 見つかったものがあれば「削除」のときだけ true。「キャンセル」・
    /// `cancel_modal` では false。表示中は `modal_is_open`。何も無ければ知らせるだけ（false）。削除の結果も同じく
    /// 追跡して閉じられる。
    #[test]
    fn data_report_dialog_returns_true_only_for_delete() {
        use crate::datacheck::FileEntry;
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let hwnd = window.hwnd();
        let found = DataReport { orphans: vec![FileEntry { name: "x".into(), size: 1 }], ..DataReport::default() };
        let click = move |id: i32| {
            move || {
                assert!(modal_is_open(hwnd), "結果の表示中になっていない");
                let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
                unsafe {
                    SendMessageW(HWND(ctx.dialog.get() as *mut _), TDM_CLICK_BUTTON.0 as u32, Some(WPARAM(id as usize)), Some(LPARAM(0)));
                }
            }
        };
        during_dialog(hwnd, click(CONFIRM_DELETE));
        assert!(show_data_report(hwnd, &found));
        assert!(!finish_menu_test());
        during_dialog(hwnd, click(IDCANCEL.0));
        assert!(!show_data_report(hwnd, &found));
        assert!(!finish_menu_test());
        during_dialog(hwnd, move || cancel_modal(hwnd));
        assert!(!show_data_report(hwnd, &found));
        assert!(!finish_menu_test(), "cancel_modal で閉じなかった");
        during_dialog(hwnd, click(IDCLOSE.0));
        assert!(!show_data_report(hwnd, &DataReport::default()));
        assert!(!finish_menu_test());
        during_dialog(hwnd, move || cancel_modal(hwnd));
        show_clean_result(hwnd, &CleanResult::default());
        assert!(!finish_menu_test(), "削除の結果が cancel_modal で閉じなかった");
        assert!(!modal_is_open(hwnd));
    }

    /// 結果の本文: 件数・大きさ・履歴とピン留めの内訳・対象外・調べていないこと（中身の破損）を書き、名前は「詳細」に
    /// 先頭 20 件ずつ（残りは数）。
    #[test]
    fn data_report_and_clean_result_texts() {
        use crate::datacheck::{FileEntry, MissingItem, TempFile};
        let (content, details) = data_report_text(&DataReport::default());
        assert!(content.contains("問題は見つかりませんでした") && content.contains("中身の破損は調べていません"), "{content}");
        assert!(details.is_empty());
        let report = DataReport {
            orphans: (0..25).map(|i| FileEntry { name: format!("o{i}"), size: 100 }).collect(),
            missing: vec![
                MissingItem { id: Uuid::new_v4(), pinned: false, label: "欠けた履歴".into() },
                MissingItem { id: Uuid::new_v4(), pinned: true, label: "欠けたピン".into() },
            ],
            temps: vec![TempFile { place: TempPlace::DataDir, name: "history.toml.tmp".into(), size: 2048 }],
            ignored: vec!["blobs\\desktop.ini".into()],
        };
        let (content, details) = data_report_text(&report);
        for part in ["25 件（2.4 KB）", "履歴 1 件・ピン留め 1 件", "1 件（2.0 KB）", "対象外のファイル: 1 件", "中身の破損は調べていません"] {
            assert!(content.contains(part), "{part} が無い: {content}");
        }
        for part in ["blobs\\o0", "blobs\\o19", "ほか 5 件", "履歴: 欠けた履歴", "ピン留め: 欠けたピン", "history.toml.tmp", "blobs\\desktop.ini"] {
            assert!(details.contains(part), "{part} が無い: {details}");
        }
        assert!(!details.contains("blobs\\o20"));
        let (content, details) = clean_result_text(&CleanResult {
            files_removed: 3,
            bytes_removed: 3 * 1_048_576,
            items_removed: 2,
            failures: vec!["blobs\\a: 使用中".into()],
            interrupted: true,
            index_error: Some("書けない".into()),
        });
        for part in ["3 件（3.0 MB）", "項目: 2 件", "途中でやめました", "書き直せませんでした", "1 件あります"] {
            assert!(content.contains(part), "{part} が無い: {content}");
        }
        assert!(details.contains("blobs\\a: 使用中"));
        assert_eq!(format_size(1023), "1023 バイト");

        // 名前は出す分（先頭 20 件）だけ作り、合計は上限で止める
        let made = Cell::new(0usize);
        let text = list_names("x", (0..100_000).map(|i| {
            made.set(made.get() + 1);
            format!("n{i}")
        }));
        assert_eq!(made.get(), REPORT_LIST_MAX, "出さない名前まで作った");
        assert!(text.contains("ほか 99980 件"), "{text}");
        assert_eq!(total_size([u64::MAX, 1].into_iter()), u64::MAX);
        let huge = DataReport { orphans: vec![FileEntry { name: "a".into(), size: u64::MAX }, FileEntry { name: "b".into(), size: 1 }], ..DataReport::default() };
        assert!(data_report_text(&huge).0.contains("MB"));
    }

    /// ピン留めの行（先頭）と履歴の行を入れ、先頭を選んで表示する。
    fn window_with_pinned_row() -> (ViewerWindow, Rc<Recorder>, Uuid) {
        let (window, recorder) = create_test_window();
        let pinned = Row { pinned: true, ..row("p") };
        let id = pinned.id;
        set_rows(window.hwnd(), vec![pinned, row("h")], false);
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE;
            let _ = ShowWindow(window.hwnd(), SW_SHOWNOACTIVATE);
            set_item_state(list(&window), 0, LVIS_SELECTED.0 | LVIS_FOCUSED.0);
        }
        pump(window.hwnd(), 20);
        (window, recorder, id)
    }

    fn press_f2(window: &ViewerWindow) {
        use windows::Win32::UI::WindowsAndMessaging::WM_KEYDOWN;
        unsafe {
            SendMessageW(list(window), WM_KEYDOWN, Some(WPARAM(VK_F2.0 as usize)), Some(LPARAM(0)));
        }
    }

    /// 名前の変更のダイアログの入力欄。
    fn name_edit(hwnd: HWND) -> HWND {
        let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
        unsafe { GetDlgItem(Some(HWND(ctx.dialog.get() as *mut _)), crate::native::settings::IDC_RENAME_NAME).unwrap() }
    }

    /// ピン留めの行の F2 で名前の変更のダイアログが開き（表示中は `modal_is_open`、入力欄には今の名前）、
    /// 「OK」のときだけ入力を伝える。「キャンセル」・`cancel_modal`・隠す要求（閉じる）・終了の後のやり直しの
    /// タイマーでは閉じて伝えない。閉じた後に自分を起こす。履歴の行・アイテムが無いときは開かない。
    #[test]
    fn rename_dialog_reports_name_only_on_ok() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, id) = window_with_pinned_row();
        let hwnd = window.hwnd();
        *recorder.pinned_title.borrow_mut() = Some(Some("旧名".into()));

        let seen = Rc::new(RefCell::new(String::new()));
        let seen_in = Rc::clone(&seen);
        during_dialog(hwnd, move || {
            assert!(modal_is_open(hwnd), "名前の変更の表示中になっていない");
            let edit = name_edit(hwnd);
            let mut buf = [0u16; 64];
            let n = unsafe { GetWindowTextW(edit, &mut buf) } as usize;
            *seen_in.borrow_mut() = String::from_utf16_lossy(&buf[..n]);
            let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
            unsafe {
                let _ = SetWindowTextW(edit, w!("新名"));
                SendMessageW(HWND(ctx.dialog.get() as *mut _), WM_COMMAND, Some(WPARAM(IDOK.0 as usize)), Some(LPARAM(0)));
            }
        });
        press_f2(&window);
        assert!(!finish_menu_test(), "「OK」で閉じなかった");
        assert_eq!(seen.borrow().as_str(), "旧名");
        assert_eq!(recorder.renamed.borrow().as_slice(), [(id, "新名".to_string())]);
        assert!(!modal_is_open(hwnd));
        let before = recorder.wakes.get();
        pump(hwnd, 30);
        assert!(recorder.wakes.get() > before, "閉じた後に起こしていない");

        let cancel_button = move || {
            let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
            unsafe {
                SendMessageW(HWND(ctx.dialog.get() as *mut _), WM_COMMAND, Some(WPARAM(IDCANCEL.0 as usize)), Some(LPARAM(0)));
            }
        };
        during_dialog(hwnd, cancel_button);
        press_f2(&window);
        assert!(!finish_menu_test(), "「キャンセル」で閉じなかった");
        during_dialog(hwnd, move || cancel_modal(hwnd));
        press_f2(&window);
        assert!(!finish_menu_test(), "cancel_modal で閉じなかった");
        CLOSE_RETRIES.with(|n| n.set(0));
        during_dialog(hwnd, move || retry_modal_close(hwnd));
        press_f2(&window);
        assert!(!finish_menu_test(), "やり直しのタイマーで閉じなかった");
        assert!(CLOSE_RETRIES.with(Cell::get) >= 1);
        assert_eq!(recorder.renamed.borrow().len(), 1, "キャンセルなのに伝えた");

        // 入力の上限（`NAME_MAX_LEN`）より長い今の名前は、変えずに OK しても縮めない
        let long = "長".repeat(NAME_MAX_LEN + 44);
        *recorder.pinned_title.borrow_mut() = Some(Some(long.clone()));
        during_dialog(hwnd, move || {
            let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
            unsafe {
                SendMessageW(HWND(ctx.dialog.get() as *mut _), WM_COMMAND, Some(WPARAM(IDOK.0 as usize)), Some(LPARAM(0)));
            }
        });
        press_f2(&window);
        assert!(!finish_menu_test());
        assert_eq!(recorder.renamed.borrow().last(), Some(&(id, long)), "長い名前を切り詰めた");
        recorder.renamed.borrow_mut().pop();

        recorder.hide_on_close.set(true);
        during_dialog(hwnd, move || unsafe {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        });
        press_f2(&window);
        assert!(!finish_menu_test(), "閉じる要求で閉じなかった");
        assert_eq!(recorder.renamed.borrow().len(), 1, "キャンセルなのに伝えた");

        // 履歴の行・アイテムが無くなっていたときは開かない（開いたら予備のタイマーが閉じる）
        let opened = Rc::new(Cell::new(false));
        let (window, recorder, _id) = window_with_pinned_row();
        let hwnd = window.hwnd();
        *recorder.pinned_title.borrow_mut() = None;
        let opened_in = Rc::clone(&opened);
        during_dialog(hwnd, move || {
            opened_in.set(true);
            cancel_modal(hwnd);
        });
        press_f2(&window);
        unsafe {
            set_item_state(list(&window), -1, 0);
            set_item_state(list(&window), 1, LVIS_SELECTED.0 | LVIS_FOCUSED.0);
        }
        *recorder.pinned_title.borrow_mut() = Some(None);
        press_f2(&window);
        finish_menu_test();
        assert!(!opened.get(), "履歴の行・無いアイテムでダイアログを開いた");
        assert!(recorder.renamed.borrow().is_empty());
    }

    /// 確認ダイアログの表示中に終了の要求が来たとき（アプリは `cancel_modal` の後に
    /// `PostQuitMessage`）、確認は閉じて伝えず、WM_QUIT が残る（メインのメッセージループが抜けられる）。
    #[test]
    fn dialog_closed_by_cancel_before_quit_leaves_wm_quit() {
        use windows::Win32::UI::WindowsAndMessaging::{PostQuitMessage, WM_QUIT};
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder) = create_test_window();
        let hwnd = window.hwnd();
        during_dialog(hwnd, move || {
            cancel_modal(hwnd);
            unsafe { PostQuitMessage(0) };
        });
        send_command(hwnd, CMD_CLEAR_HISTORY);
        assert!(!finish_menu_test(), "確認が閉じなかった");
        assert!(recorder.tools.borrow().is_empty());
        let mut quit = false;
        for _ in 0..100 {
            let mut msg = MSG::default();
            if !unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                break;
            }
            if msg.message == WM_QUIT {
                quit = true;
                break;
            }
            unsafe { DispatchMessageW(&msg) };
        }
        assert!(quit, "WM_QUIT が残っていない");
    }

    /// バージョン情報: 表示中は `modal_is_open`、タイトルは「バージョン情報」。「閉じる」・
    /// `cancel_modal`・隠す要求（閉じる）のどれでも閉じ、ハンドラへは何も伝えない。
    #[test]
    fn about_dialog_is_tracked_and_closed_like_confirmation() {
        use windows::Win32::UI::WindowsAndMessaging::IDCLOSE;
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder) = create_test_window();
        let hwnd = window.hwnd();

        let title = Rc::new(RefCell::new(String::new()));
        let seen = Rc::clone(&title);
        during_dialog(hwnd, move || {
            let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
            assert!(modal_is_open(hwnd), "バージョン情報の表示中になっていない");
            let dialog = HWND(ctx.dialog.get() as *mut _);
            let mut buf = [0u16; 64];
            let n = unsafe { GetWindowTextW(dialog, &mut buf) } as usize;
            *seen.borrow_mut() = String::from_utf16_lossy(&buf[..n]);
            unsafe {
                SendMessageW(dialog, TDM_CLICK_BUTTON.0 as u32, Some(WPARAM(IDCLOSE.0 as usize)), Some(LPARAM(0)));
            }
        });
        send_command(hwnd, CMD_ABOUT);
        assert!(!finish_menu_test(), "「閉じる」で閉じなかった");
        assert_eq!(title.borrow().as_str(), "バージョン情報");
        assert!(!modal_is_open(hwnd));

        during_dialog(hwnd, move || cancel_modal(hwnd));
        send_command(hwnd, CMD_ABOUT);
        assert!(!finish_menu_test(), "cancel_modal で閉じなかった");

        recorder.hide_on_close.set(true);
        during_dialog(hwnd, move || unsafe {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        });
        send_command(hwnd, CMD_ABOUT);
        assert!(!finish_menu_test(), "閉じる要求でバージョン情報が閉じなかった");
        assert!(recorder.tools.borrow().is_empty(), "ハンドラへ操作を伝えた");

        let content = about_content();
        for line in [
            "Windows クリップボード履歴マネージャ",
            "Copyright (c) 2026 tsZ",
            "MIT License",
            "Based on CLCL by Ohno Tomoaki (nakkag/CLCL)",
        ] {
            assert!(content.contains(line), "本文に「{line}」がない");
        }
        assert!(content.starts_with(&format!("バージョン {}", env!("CARGO_PKG_VERSION"))));
    }

    /// ビューアの TaskDialog（バージョン情報）の表示中に設定画面を閉じても、閉じる要求はそのモーダルループの中か後で
    /// 1回だけ処理され、窓は壊れない。
    #[test]
    fn settings_closed_during_viewer_dialog_is_destroyed_once() {
        use crate::native::settings::{OnClosed, OnOk, SettingsWindow};
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let hwnd = window.hwnd();
        let slot: Rc<RefCell<Option<SettingsWindow>>> = Rc::default();
        let closed = Rc::new(Cell::new(0));
        let on_ok: OnOk = Rc::new(|_, _| Err(crate::native::app::ApplyError::Busy));
        let on_closed: OnClosed = {
            let (slot, closed) = (Rc::clone(&slot), Rc::clone(&closed));
            Rc::new(move || {
                closed.set(closed.get() + 1);
                let window = slot.borrow_mut().take();
                drop(window);
            })
        };
        let settings =
            SettingsWindow::create(Some(hwnd), crate::config::Config::default(), on_ok, on_closed).unwrap();
        let dialog = settings.hwnd();
        *slot.borrow_mut() = Some(settings);

        during_dialog(hwnd, move || unsafe {
            SendMessageW(dialog, WM_COMMAND, Some(WPARAM(IDCANCEL.0 as usize)), Some(LPARAM(0)));
            SendMessageW(dialog, WM_COMMAND, Some(WPARAM(IDOK.0 as usize)), Some(LPARAM(0)));
            SendMessageW(dialog, WM_COMMAND, Some(WPARAM(IDCANCEL.0 as usize)), Some(LPARAM(0)));
            cancel_modal(hwnd);
        });
        send_command(hwnd, CMD_ABOUT);
        assert!(!finish_menu_test(), "バージョン情報が閉じなかった");
        // 閉じる要求は設定画面宛て（`pump` はビューア宛てだけを取り出すので、スレッドのメッセージを全部配送する）
        let until = std::time::Instant::now() + std::time::Duration::from_millis(100);
        while std::time::Instant::now() < until {
            unsafe {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(closed.get(), 1, "設定画面を1回だけ閉じていない");
        assert!(slot.borrow().is_none() && !unsafe { IsWindow(Some(dialog)) }.as_bool());
    }

    // --- ツリーの右クリックメニュー ---

    /// 「履歴」「ピン留め」（子にフォルダ f）のツリーを作り、`selected` を選んで、窓を（前面にせずに）
    /// 表示する。戻り値のハンドルは 履歴・ピン留め・f の順。
    fn window_with_tree(selected: Source, folder: Uuid) -> (ViewerWindow, Rc<Recorder>, HWND, [HTREEITEM; 3]) {
        use windows::Win32::UI::Controls::{TVGN_CHILD, TVGN_NEXT, TVGN_ROOT};
        let (window, recorder) = create_test_window();
        let hwnd = window.hwnd();
        let nodes = vec![
            TreeNode { label: HISTORY_LABEL.into(), source: Source::History, children: vec![] },
            TreeNode {
                label: "ピン留め".into(),
                source: Source::Pinned(None),
                children: vec![TreeNode { label: "f".into(), source: Source::Pinned(Some(folder)), children: vec![] }],
            },
        ];
        set_tree(hwnd, &nodes, selected);
        let tree = unsafe { GetDlgItem(Some(hwnd), ID_TREE).unwrap() };
        let next = |flag: u32, from: HTREEITEM| unsafe {
            HTREEITEM(SendMessageW(tree, TVM_GETNEXTITEM, Some(WPARAM(flag as usize)), Some(LPARAM(from.0))).0)
        };
        let history = next(TVGN_ROOT, HTREEITEM(0));
        let pinned = next(TVGN_NEXT, history);
        let folder_item = next(TVGN_CHILD, pinned);
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE;
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        pump(hwnd, 20);
        (window, recorder, tree, [history, pinned, folder_item])
    }

    fn tree_item_at(tree: HWND, flag: u32) -> HTREEITEM {
        HTREEITEM(unsafe { SendMessageW(tree, TVM_GETNEXTITEM, Some(WPARAM(flag as usize)), Some(LPARAM(0))).0 })
    }

    /// 項目の文字の部分の中央（`below` なら最後の項目よりずっと下）の画面座標。
    fn tree_point(tree: HWND, item: HTREEITEM, below: bool) -> POINT {
        let rc = unsafe { tree_item_rect(tree, item) }.expect("項目の矩形が取れない");
        let height = rc.bottom - rc.top;
        let mut pt = POINT {
            x: (rc.left + rc.right) / 2,
            y: if below { rc.bottom + height * 3 } else { (rc.top + rc.bottom) / 2 },
        };
        unsafe {
            let _ = ClientToScreen(tree, &mut pt);
        }
        pt
    }

    fn tree_context_menu(hwnd: HWND, tree: HWND, x: i32, y: i32) {
        let lparam = ((y as u16 as u32) << 16 | (x as u16 as u32)) as isize;
        unsafe {
            SendMessageW(hwnd, WM_CONTEXTMENU, Some(WPARAM(tree.0 as usize)), Some(LPARAM(lparam)));
        }
    }

    /// マウスで右クリックした項目が対象になり、選択（表示元）は変わらない。メニューの間だけその項目を
    /// 強調し、閉じたら外す。項目の無い所ではメニューを出さない。キーボードでは選んでいる項目が対象。
    #[test]
    fn tree_menu_targets_clicked_item_without_changing_selection() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let folder = Uuid::new_v4();
        let (window, recorder, tree, [history, _pinned, folder_item]) = window_with_tree(Source::History, folder);
        let hwnd = window.hwnd();
        *recorder.tree_menu.borrow_mut() = Some(TreeMenu::PinnedFolder { id: folder, title: "f".into(), items: 0, folders: 0 });

        let on_folder = tree_point(tree, folder_item, false);
        let hilited = Rc::new(Cell::new(0isize));
        let seen = Rc::clone(&hilited);
        during_menu(move || {
            assert!(unsafe { ctx_ref(hwnd) }.unwrap().menu_open.get(), "メニューの表示中になっていない");
            seen.set(tree_item_at(tree, TVGN_DROPHILITE).0);
            cancel_modal(hwnd);
        });
        tree_context_menu(hwnd, tree, on_folder.x, on_folder.y);
        assert!(!finish_menu_test(), "cancel_modal でメニューが閉じなかった");
        assert_eq!(recorder.tree_menu_requests.borrow().as_slice(), [Source::Pinned(Some(folder))]);
        assert_eq!(hilited.get(), folder_item.0, "メニューの間、右クリックした項目を強調していない");
        assert_eq!(tree_item_at(tree, TVGN_DROPHILITE).0, 0, "閉じた後も強調が残っている");
        assert_eq!(tree_item_at(tree, TVGN_CARET), history, "右クリックで選択が変わった");
        assert!(recorder.selected.borrow().is_empty(), "表示元の切り替えを伝えた");
        assert!(recorder.tree_commands.borrow().is_empty());

        let below = tree_point(tree, folder_item, true);
        assert_no_menu(hwnd, || tree_context_menu(hwnd, tree, below.x, below.y), "項目の無い所でメニューを出した");
        assert_eq!(recorder.tree_menu_requests.borrow().len(), 1, "項目の無い所でメニューを求めた");

        *recorder.tree_menu.borrow_mut() = Some(TreeMenu::History);
        during_menu(move || cancel_modal(hwnd));
        tree_context_menu(hwnd, tree, -1, -1);
        assert!(!finish_menu_test(), "キーボードから開いたメニューが閉じなかった");
        assert_eq!(recorder.tree_menu_requests.borrow().last(), Some(&Source::History));

        // ハンドラが None を返す項目（階層表示のフォルダ・ピン留めの根）ではメニューを出さない
        *recorder.tree_menu.borrow_mut() = None;
        assert_no_menu(hwnd, || tree_context_menu(hwnd, tree, on_folder.x, on_folder.y), "None なのにメニューを出した");
        assert_eq!(recorder.tree_menu_requests.borrow().len(), 3);
        assert!(!modal_is_open(hwnd));
    }

    /// `open` でメニューが出ないこと。出た場合に止まらないよう閉じる仕掛けは置き、自前描画の項目を
    /// 測った回数が増えないこと（メニューを出せば必ず測る）と、仕掛けが使われていないことで確かめる。
    fn assert_no_menu(hwnd: HWND, open: impl FnOnce(), message: &str) {
        let (measured, _) = crate::menu_draw::test_support::counts();
        during_menu(move || cancel_modal(hwnd));
        open();
        finish_menu_test();
        let unused = IN_MENU.with(|slot| slot.borrow_mut().take()).is_some();
        assert!(unused && crate::menu_draw::test_support::counts().0 == measured, "{message}");
    }

    /// メニューの間にツリーが作り直されても（履歴の追加で階層表示が変わったときなど）、右クリックした
    /// 表示対象の新しい項目を強調し直す。閉じたら外す。
    #[test]
    fn tree_menu_hilite_is_restored_after_rebuild_during_menu() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let folder = Uuid::new_v4();
        let (window, recorder, tree, [_history, _pinned, folder_item]) = window_with_tree(Source::History, folder);
        let hwnd = window.hwnd();
        *recorder.tree_menu.borrow_mut() = Some(TreeMenu::PinnedFolder { id: folder, title: "f".into(), items: 0, folders: 0 });
        let on_folder = tree_point(tree, folder_item, false);
        let seen = Rc::new(Cell::new((0isize, 0isize)));
        let seen_in_menu = Rc::clone(&seen);
        during_menu(move || {
            // 同じ構成で作り直す（項目のハンドルは新しくなる）
            let nodes = vec![
                TreeNode { label: HISTORY_LABEL.into(), source: Source::History, children: vec![] },
                TreeNode {
                    label: "ピン留め".into(),
                    source: Source::Pinned(None),
                    children: vec![TreeNode { label: "f".into(), source: Source::Pinned(Some(folder)), children: vec![] }],
                },
            ];
            set_tree(hwnd, &nodes, Source::History);
            let new_folder = {
                use windows::Win32::UI::Controls::{TVGN_CHILD, TVGN_NEXT, TVGN_ROOT};
                let next = |flag: u32, from: isize| unsafe {
                    SendMessageW(tree, TVM_GETNEXTITEM, Some(WPARAM(flag as usize)), Some(LPARAM(from))).0
                };
                next(TVGN_CHILD, next(TVGN_NEXT, next(TVGN_ROOT, 0)))
            };
            seen_in_menu.set((new_folder, tree_item_at(tree, TVGN_DROPHILITE).0));
            cancel_modal(hwnd);
        });
        tree_context_menu(hwnd, tree, on_folder.x, on_folder.y);
        assert!(!finish_menu_test(), "cancel_modal でメニューが閉じなかった");
        let (new_folder, hilited) = seen.get();
        assert_ne!(new_folder, 0);
        assert_eq!(hilited, new_folder, "作り直した後の項目を強調していない");
        assert_eq!(tree_item_at(tree, TVGN_DROPHILITE).0, 0, "閉じた後も強調が残っている");
        assert_eq!(unsafe { ctx_ref(hwnd) }.unwrap().tree_hilite.get(), None);
    }

    /// ツリーの右ボタンはサブクラスが扱う: 押したらマウスを捕まえて項目を
    /// 覚え、押したのと同じ項目の上で離したときだけメニュー（`show_tree_menu`）へ進む。押した項目は
    /// 強調しない（既定の処理へ渡さない）。ほかの場所で離す・押下を覚えていない離す操作・離す前に
    /// マウスを失った場合は何もしない。ツリーで押している間に一覧から来たマウスの右クリックメニューの
    /// 要求は無視する（キーボードからの要求は扱う）。
    #[test]
    fn tree_right_button_shows_menu_only_when_released_on_pressed_item() {
        use windows::Win32::UI::WindowsAndMessaging::{WM_RBUTTONDOWN, WM_RBUTTONUP};
        let _gui = crate::tray::lock_gui_resource_tests();
        let folder = Uuid::new_v4();
        let (window, recorder, tree, [history, _pinned, folder_item]) = window_with_tree(Source::History, folder);
        let hwnd = window.hwnd();
        let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
        let client = |item: HTREEITEM, below: bool| {
            let mut pt = tree_point(tree, item, below);
            unsafe {
                let _ = ScreenToClient(tree, &mut pt);
            }
            LPARAM(((pt.y as u16 as u32) << 16 | pt.x as u16 as u32) as isize)
        };
        let (on_folder, on_history, below) = (client(folder_item, false), client(history, false), client(folder_item, true));
        let down = |at: LPARAM| unsafe {
            SendMessageW(tree, WM_RBUTTONDOWN, Some(WPARAM(0x0002)), Some(at));
        };
        let up = |at: LPARAM| unsafe {
            SendMessageW(tree, WM_RBUTTONUP, Some(WPARAM(0)), Some(at));
        };
        *recorder.tree_menu.borrow_mut() = Some(TreeMenu::PinnedFolder { id: folder, title: "f".into(), items: 0, folders: 0 });

        // 同じ項目で押して離す: メニューが出る。押している間は強調せず、マウスを捕まえている
        let seen = Rc::new(Cell::new(0isize));
        let seen_in_menu = Rc::clone(&seen);
        during_menu(move || {
            seen_in_menu.set(tree_item_at(tree, TVGN_DROPHILITE).0);
            cancel_modal(hwnd);
        });
        down(on_folder);
        assert_eq!(tree_item_at(tree, TVGN_DROPHILITE).0, 0, "押した項目を強調した");
        assert_eq!(unsafe { GetCapture() }, tree, "ツリーの外で離しても届くようにマウスを捕まえていない");
        up(on_folder);
        assert!(!finish_menu_test(), "cancel_modal でメニューが閉じなかった");
        assert_eq!(seen.get(), folder_item.0, "メニューの間、右クリックした項目を強調していない");
        assert_eq!(recorder.tree_menu_requests.borrow().as_slice(), [Source::Pinned(Some(folder))]);
        assert_ne!(unsafe { GetCapture() }, tree, "離した後もマウスを捕まえている");
        assert_eq!(ctx.tree_rpress.get(), None);
        assert_eq!(tree_item_at(tree, TVGN_CARET), history, "選択が変わった");

        // 押している間にツリーが作り直されても（項目のハンドルは新しくなる）、同じ表示元の項目で離せば出る
        // （押した項目は表示元で覚える）
        let nodes = vec![
            TreeNode { label: HISTORY_LABEL.into(), source: Source::History, children: vec![] },
            TreeNode {
                label: "ピン留め".into(),
                source: Source::Pinned(None),
                children: vec![TreeNode { label: "f".into(), source: Source::Pinned(Some(folder)), children: vec![] }],
            },
        ];
        during_menu(move || cancel_modal(hwnd));
        down(on_folder);
        set_tree(hwnd, &nodes, Source::History);
        up(on_folder);
        assert!(!finish_menu_test(), "作り直しの後に同じ項目で離してメニューが閉じなかった（出なかった）");
        assert_eq!(recorder.tree_menu_requests.borrow().len(), 2, "作り直しの後に同じ項目で離してもメニューを求めなかった");

        // 別の項目・項目の無い所で離す、押下を覚えていない離す操作: 何もしない
        assert_no_menu(hwnd, || { down(on_folder); up(on_history); }, "別の項目で離してメニューを出した");
        assert_no_menu(hwnd, || { down(on_folder); up(below); }, "項目の無い所で離してメニューを出した");
        assert_no_menu(hwnd, || up(on_folder), "押下の無い離す操作でメニューを出した");
        assert_eq!(recorder.tree_menu_requests.borrow().len(), 2, "メニューを求めた");

        // 離す前にマウスを失ったら（Alt+Tab など）、押下は終わったものとする
        down(on_folder);
        unsafe {
            let _ = ReleaseCapture();
        }
        assert_eq!(ctx.tree_rpress.get(), None, "マウスを失っても押下が残った");
        assert_no_menu(hwnd, || up(on_folder), "マウスを失った後の離す操作でメニューを出した");

        // ツリーで押している間に一覧から来たマウスの要求は無視し、押下を終わらせる
        *recorder.menu.borrow_mut() = Some(RowMenu::default());
        down(on_folder);
        assert_no_menu(hwnd, || context_menu_at(&window, 10, 10), "ツリーで押している間に一覧のメニューを出した");
        assert_eq!(ctx.tree_rpress.get(), None);
        assert!(recorder.menu_requests.borrow().is_empty());
        unsafe {
            let _ = ReleaseCapture();
        }
    }

    /// メニューのモーダルループの中で `keys`（↓ で先頭、↑ で最後の項目へ）と Enter を送って項目を選ぶ。
    /// `in_dialog` があれば、その後に出る確認ダイアログの中で行う仕掛けを置く（メニューの予備のタイマーは
    /// 片付ける）。
    fn choose_in_menu(hwnd: HWND, keys: &'static [u16], in_dialog: Option<Box<dyn FnOnce()>>) {
        use windows::Win32::UI::Input::KeyboardAndMouse::VK_RETURN;
        use windows::Win32::UI::WindowsAndMessaging::WM_KEYDOWN;
        during_menu(move || {
            finish_menu_test();
            if let Some(f) = in_dialog {
                during_dialog(hwnd, f);
            }
            unsafe {
                for key in keys.iter().copied().chain([VK_RETURN.0]) {
                    let _ = PostMessageW(Some(hwnd), WM_KEYDOWN, WPARAM(key as usize), LPARAM(0));
                }
            }
        });
    }

    const KEY_FIRST: &[u16] = &[windows::Win32::UI::Input::KeyboardAndMouse::VK_DOWN.0];
    const KEY_SECOND: &[u16] = &[
        windows::Win32::UI::Input::KeyboardAndMouse::VK_DOWN.0,
        windows::Win32::UI::Input::KeyboardAndMouse::VK_DOWN.0,
    ];
    const KEY_LAST: &[u16] = &[windows::Win32::UI::Input::KeyboardAndMouse::VK_UP.0];

    /// 最後の項目（「削除...」「履歴のクリア...」）を選び、確認ダイアログの中で `in_dialog` を行う。
    fn choose_last_then_in_dialog(hwnd: HWND, in_dialog: impl FnOnce() + 'static) {
        choose_in_menu(hwnd, KEY_LAST, Some(Box::new(in_dialog)));
    }

    /// 確認ダイアログのボタンを押す。確認の表示中（メニューの表示中ではない）でなければ失敗させる。
    fn click_dialog_button(hwnd: HWND, button: i32) {
        let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
        assert!(ctx.dialog.get() != 0 && !ctx.menu_open.get(), "確認の表示中になっていない");
        unsafe {
            SendMessageW(
                HWND(ctx.dialog.get() as *mut _),
                TDM_CLICK_BUTTON.0 as u32,
                Some(WPARAM(button as usize)),
                Some(LPARAM(0)),
            );
        }
    }

    /// ツリーのメニューの操作は、確認で「削除」を選んだときだけハンドラへ伝える。「履歴」の履歴のクリアは
    /// メニューバーと同じ `on_tool_command`、ピン留めのフォルダの削除は `on_tree_command`。
    #[test]
    fn tree_menu_commands_are_reported_only_after_confirmation() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let folder = Uuid::new_v4();
        let (window, recorder, tree, _items) = window_with_tree(Source::Pinned(Some(folder)), folder);
        let hwnd = window.hwnd();
        *recorder.tree_menu.borrow_mut() = Some(TreeMenu::PinnedFolder { id: folder, title: "f".into(), items: 2, folders: 0 });

        let reached = Rc::new(Cell::new(false));
        let reached_in_dialog = Rc::clone(&reached);
        choose_last_then_in_dialog(hwnd, move || {
            reached_in_dialog.set(true);
            click_dialog_button(hwnd, IDCANCEL.0);
        });
        tree_context_menu(hwnd, tree, -1, -1);
        assert!(!finish_menu_test(), "メニューで選べなかったか、確認が閉じなかった");
        assert!(reached.get(), "確認ダイアログまで進んでいない");
        assert!(recorder.tree_commands.borrow().is_empty(), "キャンセルなのに伝えた");
        // メニューを出すときと、確認の直前（名前・件数の取り直し）の2回求める
        assert_eq!(recorder.tree_menu_requests.borrow().as_slice(), [Source::Pinned(Some(folder)); 2]);

        choose_last_then_in_dialog(hwnd, move || click_dialog_button(hwnd, CONFIRM_DELETE));
        tree_context_menu(hwnd, tree, -1, -1);
        assert!(!finish_menu_test(), "メニューで選べなかったか、確認が閉じなかった");
        assert_eq!(recorder.tree_commands.borrow().as_slice(), [TreeCommand::DeleteFolder(folder)]);
        assert!(recorder.tools.borrow().is_empty());

        *recorder.tree_menu.borrow_mut() = Some(TreeMenu::History);
        choose_last_then_in_dialog(hwnd, move || click_dialog_button(hwnd, CONFIRM_DELETE));
        tree_context_menu(hwnd, tree, -1, -1);
        assert!(!finish_menu_test(), "メニューで選べなかったか、確認が閉じなかった");
        assert_eq!(recorder.tools.borrow().as_slice(), [ToolCommand::ClearHistory]);
        assert_eq!(recorder.tree_commands.borrow().len(), 1);
        assert!(!modal_is_open(hwnd));
    }

    // --- ピン留めの一覧のフォルダの行と並べ替え ---

    /// ピン留めの項目の行は「移動」の後に「上へ」「下へ」（先頭・末尾の方は灰色で、選んでも操作にならない）。
    /// フォルダの行は「開く」「上へ」「下へ」「名前の変更...」「削除...」だけ。並べ替えの無い行には出さない。
    #[test]
    fn row_menu_offers_reorder_and_folder_items() {
        use crate::native::model::Reorder;
        use windows::Win32::UI::WindowsAndMessaging::{GetMenuItemCount, GetMenuItemID, GetMenuState, MF_BYPOSITION};
        let _gui = crate::tray::lock_gui_resource_tests();
        let ids_of = |menu: &PopupMenu| unsafe {
            (0..GetMenuItemCount(Some(menu.handle()))).map(|i| GetMenuItemID(menu.handle(), i) as usize).collect::<Vec<_>>()
        };
        let grayed = |menu: &PopupMenu, pos: u32| unsafe { GetMenuState(menu.handle(), pos, MF_BYPOSITION) & MF_GRAYED.0 != 0 };

        let item = RowMenu { current: Some(None), reorder: Some(Reorder { up: false, down: true }), ..RowMenu::default() };
        let menu = build_row_menu(&item, 96).unwrap();
        assert_eq!(ids_of(&menu), [MENU_SEND, MENU_MOVE_UP, MENU_MOVE_DOWN, MENU_RENAME, MENU_DELETE]);
        assert!(grayed(&menu, 1) && !grayed(&menu, 2), "先頭の「上へ」が灰色でない");
        assert_eq!(row_command(MENU_MOVE_UP, &item), None, "灰色の「上へ」を操作にした");
        assert_eq!(row_command(MENU_MOVE_DOWN, &item), Some(RowCommand::Reorder(Direction::Down)));

        let folder = RowMenu { folder: true, reorder: Some(Reorder { up: true, down: true }), ..RowMenu::default() };
        let menu = build_row_menu(&folder, 96).unwrap();
        assert_eq!(ids_of(&menu), [MENU_SEND, MENU_MOVE_UP, MENU_MOVE_DOWN, MENU_RENAME, MENU_DELETE]);
        assert!(!grayed(&menu, 1) && !grayed(&menu, 2));
        assert_eq!(row_command(MENU_MOVE_UP, &folder), Some(RowCommand::Reorder(Direction::Up)));
        assert_eq!(row_command(MENU_RENAME, &folder), Some(RowCommand::Rename));
        let folder_while_searching = RowMenu { folder: true, ..RowMenu::default() };
        assert_eq!(ids_of(&build_row_menu(&folder_while_searching, 96).unwrap()), [MENU_SEND, MENU_RENAME, MENU_DELETE]);
        assert_eq!(row_command(MENU_MOVE_DOWN, &RowMenu::default()), None);
    }

    /// ツリー（「ピン留め」の下にフォルダ）と、一覧にフォルダの行・ピン留めの項目の行・履歴の行を入れ、
    /// 先頭（フォルダの行）を選んで一覧にフォーカスを置く。戻り値の ID は フォルダ・項目・履歴の順。
    fn window_with_folder_row() -> (ViewerWindow, Rc<Recorder>, [Uuid; 3]) {
        use crate::native::model::FolderCounts;
        let folder = Uuid::new_v4();
        let (window, recorder, _tree, _items) = window_with_tree(Source::Pinned(None), folder);
        let folder_row = Row { id: folder, pinned: true, folder: Some(FolderCounts { items: 0, folders: 0 }), ..row("f") };
        let item_row = Row { pinned: true, ..row("p") };
        let history_row = row("h");
        let ids = [folder, item_row.id, history_row.id];
        set_rows(window.hwnd(), vec![folder_row, item_row, history_row], false);
        unsafe {
            set_item_state(list(&window), 0, LVIS_SELECTED.0 | LVIS_FOCUSED.0);
            let _ = SetFocus(Some(list(&window)));
        }
        pump(window.hwnd(), 20);
        *recorder.tree_menu.borrow_mut() = Some(TreeMenu::PinnedFolder { id: folder, title: "f".into(), items: 0, folders: 0 });
        (window, recorder, ids)
    }

    fn select_row(window: &ViewerWindow, index: i32) {
        unsafe {
            set_item_state(list(window), -1, 0);
            set_item_state(list(window), index, LVIS_SELECTED.0 | LVIS_FOCUSED.0);
        }
    }

    /// フォルダの行の Enter・ダブルクリックは送らず、通知の処理から戻った後にツリーでそのフォルダを選ぶ
    /// （表示元の切り替えとして伝わる）。項目の行はいつもどおり送る。
    #[test]
    fn activating_folder_row_selects_folder_in_tree() {
        use windows::Win32::UI::Input::KeyboardAndMouse::VK_RETURN;
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, [folder, item, _]) = window_with_folder_row();
        let hwnd = window.hwnd();
        press_through_dialog_manager(&window, list(&window), VK_RETURN.0);
        assert!(recorder.selected.borrow().is_empty(), "一覧の通知の処理の中でフォルダを開いた");
        pump(hwnd, 30);
        assert_eq!(recorder.selected.borrow().as_slice(), [Source::Pinned(Some(folder))]);
        let tree = unsafe { GetDlgItem(Some(hwnd), ID_TREE).unwrap() };
        let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
        assert_eq!(unsafe { selected_tree_source(ctx, tree) }, Some(Source::Pinned(Some(folder))));
        assert!(recorder.activated.borrow().is_empty(), "フォルダの行を送った");

        // ダブルクリックも同じ（ツリーの選択は済んでいるので、選び直しでは伝わらない。一度「ピン留め」へ戻す）
        unsafe {
            if let Some(item) = find_tree_item(ctx, tree, Source::Pinned(None)) {
                SendMessageW(tree, TVM_SELECTITEM, Some(WPARAM(TVGN_CARET as usize)), Some(LPARAM(item.0)));
            }
        }
        recorder.selected.borrow_mut().clear();
        let mut nm = NMITEMACTIVATE { hdr: NMHDR { hwndFrom: list(&window), idFrom: ID_LIST as usize, code: NM_DBLCLK }, iItem: 0, ..Default::default() };
        unsafe {
            SendMessageW(hwnd, WM_NOTIFY, Some(WPARAM(ID_LIST as usize)), Some(LPARAM(&mut nm as *mut _ as isize)));
        }
        pump(hwnd, 30);
        assert_eq!(recorder.selected.borrow().as_slice(), [Source::Pinned(Some(folder))]);

        select_row(&window, 1);
        press_through_dialog_manager(&window, list(&window), VK_RETURN.0);
        assert_eq!(recorder.activated.borrow().as_slice(), [item]);
    }

    /// フォルダの行の Delete は確認を出し、「削除」のときだけフォルダの削除（`on_tree_command`）を伝える。
    /// `on_delete` には来ない。
    #[test]
    fn deleting_folder_row_asks_for_confirmation() {
        use windows::Win32::UI::WindowsAndMessaging::WM_KEYDOWN;
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, [folder, ..]) = window_with_folder_row();
        let hwnd = window.hwnd();
        let press_delete = || unsafe {
            SendMessageW(list(&window), WM_KEYDOWN, Some(WPARAM(VK_DELETE.0 as usize)), Some(LPARAM(0)));
        };
        during_dialog(hwnd, move || click_dialog_button(hwnd, IDCANCEL.0));
        press_delete();
        assert!(!finish_menu_test(), "確認がキャンセルで閉じなかった");
        assert!(recorder.tree_commands.borrow().is_empty(), "キャンセルなのに伝えた");
        during_dialog(hwnd, move || click_dialog_button(hwnd, CONFIRM_DELETE));
        press_delete();
        assert!(!finish_menu_test(), "確認が「削除」で閉じなかった");
        assert_eq!(recorder.tree_commands.borrow().as_slice(), [TreeCommand::DeleteFolder(folder)]);
        assert!(recorder.deleted.borrow().is_empty(), "フォルダの行を確認なしの削除で伝えた");
    }

    /// フォルダの行の F2 はフォルダの名前を聞き、前後の空白を除いて空でなければ名前の変更（`on_tree_command`）を
    /// 伝える。空なら伝えない。アイテムの名前の変更（`on_rename_pinned`）には来ない。
    #[test]
    fn renaming_folder_row_reports_trimmed_name() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, [folder, ..]) = window_with_folder_row();
        let hwnd = window.hwnd();
        let answer = move |text: &'static str| {
            move || {
                let edit = name_edit(hwnd);
                let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
                let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
                unsafe {
                    let _ = SetWindowTextW(edit, PCWSTR(wide.as_ptr()));
                    SendMessageW(HWND(ctx.dialog.get() as *mut _), WM_COMMAND, Some(WPARAM(IDOK.0 as usize)), Some(LPARAM(0)));
                }
            }
        };
        let seen = Rc::new(RefCell::new(String::new()));
        let seen_in = Rc::clone(&seen);
        during_dialog(hwnd, move || {
            let mut buf = [0u16; 64];
            let n = unsafe { GetWindowTextW(name_edit(hwnd), &mut buf) } as usize;
            *seen_in.borrow_mut() = String::from_utf16_lossy(&buf[..n]);
            answer(" 新しい ")();
        });
        press_f2(&window);
        assert!(!finish_menu_test(), "「OK」で閉じなかった");
        assert_eq!(seen.borrow().as_str(), "f", "今の名前を入れていない");
        during_dialog(hwnd, answer("  "));
        press_f2(&window);
        assert!(!finish_menu_test());
        assert_eq!(
            recorder.tree_commands.borrow().as_slice(),
            [TreeCommand::RenameFolder { id: folder, title: "新しい".into() }]
        );
        assert!(recorder.renamed.borrow().is_empty());
    }

    /// Alt+↑・Alt+↓ はアクセラレータで並べ替えのコマンドになり、一覧にフォーカスがあるときだけ、選んでいる
    /// ピン留めの行（フォルダの行を含む）の並べ替えを伝える。履歴の行・一覧の外では伝えない。Alt を押して
    /// いなければ変換しない。Alt の状態は、このスレッドのキーの状態を書き換えて作る。
    #[test]
    fn alt_arrows_reorder_selected_pinned_row_only_in_list() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardState, SetKeyboardState, VK_MENU};
        use windows::Win32::UI::WindowsAndMessaging::WM_SYSKEYDOWN;
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, [folder, item, _]) = window_with_folder_row();
        let hwnd = window.hwnd();
        // lParam の 29 ビット目は Alt を押している印（WM_SYSKEYDOWN の説明）
        let key = |vk: u16| MSG { hwnd: list(&window), message: WM_SYSKEYDOWN, wParam: WPARAM(vk as usize), lParam: LPARAM(0x2148_0001), ..Default::default() };
        let mut saved = [0u8; 256];
        unsafe {
            GetKeyboardState(&mut saved).unwrap();
        }
        let with_alt = |down: bool| {
            let mut state = saved;
            state[VK_MENU.0 as usize] = if down { 0x80 } else { 0 };
            unsafe { SetKeyboardState(&state).unwrap() };
        };
        with_alt(false);
        let plain = translate_accelerator(hwnd, &key(VK_DOWN.0));
        with_alt(true);
        let down = translate_accelerator(hwnd, &key(VK_DOWN.0));
        select_row(&window, 1);
        let up = translate_accelerator(hwnd, &key(VK_UP.0));
        select_row(&window, 2);
        translate_accelerator(hwnd, &key(VK_UP.0));
        select_row(&window, 1);
        unsafe {
            let _ = SetFocus(Some(search(&window)));
        }
        translate_accelerator(hwnd, &key(VK_UP.0));
        unsafe {
            SetKeyboardState(&saved).unwrap();
        }
        assert!(!plain, "Alt なしの ↓ を変換した");
        assert!(down && up, "Alt+↑・Alt+↓ を変換しなかった");
        assert_eq!(
            recorder.commands.borrow().as_slice(),
            [(folder, RowCommand::Reorder(Direction::Down)), (item, RowCommand::Reorder(Direction::Up))]
        );
    }

    // --- ピン留めの行のドラッグ ---

    /// 一覧の上の落とす先: フォルダの行は上下の 1/4 が行の間で中央が中、項目の行は上半分・下半分で前後の間、
    /// 最後の行より下は末尾。ドラッグしている行のすぐ上・すぐ下の間（動かない所）と、自分の中は落とせない。
    #[test]
    fn list_drop_mark_splits_rows_into_gaps_and_folder_middle() {
        use crate::native::model::FolderCounts;
        let folder = Row { pinned: true, folder: Some(FolderCounts { items: 0, folders: 0 }), ..row("f") };
        let (a, b) = (Row { pinned: true, ..row("a") }, Row { pinned: true, ..row("b") });
        let rows = vec![a.clone(), folder.clone(), b.clone()];
        let at = |dragged: &Row, i: usize, y: i32| list_drop_mark(&rows, dragged.id, Some((i, y, 40)));
        // a をドラッグ: a の上下と、フォルダの行の上の 1/4（a のすぐ下の間）は動かない所
        assert_eq!(at(&a, 0, 5), None);
        assert_eq!(at(&a, 0, 30), None);
        assert_eq!(at(&a, 1, 5), None);
        assert_eq!(at(&a, 1, 20), Some(DropMark::Into(folder.id)));
        assert_eq!(at(&a, 1, 35), Some(DropMark::Before(Some(b.id))));
        assert_eq!(at(&a, 2, 30), Some(DropMark::Before(None)));
        assert_eq!(list_drop_mark(&rows, a.id, None), Some(DropMark::Before(None)));
        assert_eq!(at(&a, 9, 0), None, "無い行を落とす先にした");
        // フォルダ自身: 自分の行は中にせず、前後の間（動かない）。上の行の間へは動く
        assert_eq!(at(&folder, 1, 20), None);
        assert_eq!(at(&folder, 0, 5), Some(DropMark::Before(Some(a.id))));
        // 最後の行: 最後の行より下は動かない
        assert_eq!(list_drop_mark(&rows, b.id, None), None);
        assert_eq!(at(&b, 0, 30), Some(DropMark::Before(Some(folder.id))));

        // ピン留めの行は移動、履歴の行はツリーのピン留めへのピン留めだけ
        let drag = RowDrag { target: a.target(), parent: Some(Uuid::nil()), list_folder: Some(Some(Uuid::nil())), mark: None };
        assert_eq!(drag.command(DropMark::Before(None)), Some(RowCommand::Place { to: Some(Uuid::nil()), before: None }));
        assert_eq!(drag.command(DropMark::Into(folder.id)), Some(RowCommand::Place { to: Some(folder.id), before: None }));
        assert_eq!(drag.command(DropMark::Tree(None)), Some(RowCommand::Place { to: None, before: None }));
        // 一覧に落とせないドラッグ（検索中のツリーのフォルダなど）は、一覧の上の落とす先を操作にしない
        let tree_only = RowDrag { list_folder: None, ..drag };
        assert_eq!(tree_only.command(DropMark::Before(None)), None);
        assert_eq!(tree_only.command(DropMark::Into(folder.id)), None);
        assert_eq!(tree_only.command(DropMark::Tree(None)), Some(RowCommand::Place { to: None, before: None }));
        let history = RowDrag { target: row("h").target(), parent: None, list_folder: None, mark: None };
        assert_eq!(history.command(DropMark::Tree(Some(folder.id))), Some(RowCommand::Pin(Some(folder.id))));
        assert_eq!(history.command(DropMark::Tree(None)), Some(RowCommand::Pin(None)));
        assert_eq!(history.command(DropMark::Before(None)), None);
        assert_eq!(history.command(DropMark::Into(folder.id)), None);
    }

    /// `LVN_BEGINDRAG` を窓へ送る（一覧の `index` の行のドラッグの始まり）。
    fn begin_drag(window: &ViewerWindow, index: i32) {
        let mut nm = NMLISTVIEW {
            hdr: NMHDR { hwndFrom: list(window), idFrom: ID_LIST as usize, code: LVN_BEGINDRAG },
            iItem: index,
            ..Default::default()
        };
        unsafe {
            SendMessageW(window.hwnd(), WM_NOTIFY, Some(WPARAM(ID_LIST as usize)), Some(LPARAM(&mut nm as *mut _ as isize)));
        }
    }

    /// 窓のクライアント座標で、一覧の `index` の行の、上端から `fraction`（0〜1）の高さの点。
    fn list_row_point(window: &ViewerWindow, index: usize, fraction: f32) -> POINT {
        let list = list(window);
        let mut rc = RECT { left: LVIR_BOUNDS as i32, ..Default::default() };
        unsafe {
            SendMessageW(list, LVM_GETITEMRECT, Some(WPARAM(index)), Some(LPARAM(&mut rc as *mut _ as isize)));
        }
        let mut points = [POINT { x: (rc.left + rc.right) / 2, y: rc.top + ((rc.bottom - rc.top) as f32 * fraction) as i32 }];
        unsafe {
            MapWindowPoints(Some(list), Some(window.hwnd()), &mut points);
        }
        points[0]
    }

    /// 窓のクライアント座標で、ツリーの項目の文字の中央。
    fn tree_item_point(window: &ViewerWindow, tree: HWND, item: HTREEITEM) -> POINT {
        let rc = unsafe { tree_item_rect(tree, item) }.expect("項目の矩形が取れない");
        let mut points = [POINT { x: (rc.left + rc.right) / 2, y: (rc.top + rc.bottom) / 2 }];
        unsafe {
            MapWindowPoints(Some(tree), Some(window.hwnd()), &mut points);
        }
        points[0]
    }

    /// 窓へマウスのメッセージ（`WM_MOUSEMOVE`・`WM_LBUTTONUP`）を送る。
    fn mouse(hwnd: HWND, msg: u32, point: POINT) {
        let lparam = ((point.y as u16 as u32) << 16 | (point.x as u16 as u32)) as isize;
        unsafe {
            SendMessageW(hwnd, msg, Some(WPARAM(0)), Some(LPARAM(lparam)));
        }
    }

    fn drop_mark(hwnd: HWND) -> Option<DropMark> {
        unsafe { ctx_ref(hwnd) }.unwrap().row_drag.get().and_then(|drag| drag.mark)
    }

    /// ドラッグ中の行の絵の小窓（出ていて見えていること）。
    fn shown_drag_image(hwnd: HWND) -> HWND {
        let image = unsafe { ctx_ref(hwnd) }.unwrap().drag_image.get();
        assert_ne!(image, 0, "行の絵を出していない");
        let image = HWND(image as *mut _);
        assert!(unsafe { IsWindowVisible(image) }.as_bool(), "行の絵が見えていない");
        image
    }

    /// 行の絵が、カーソル（窓のクライアント座標 `to`）の右下にある。
    fn assert_drag_image_at(hwnd: HWND, image: HWND, to: POINT) {
        use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
        let mut screen = to;
        let mut rc = RECT::default();
        unsafe {
            let _ = ClientToScreen(hwnd, &mut screen);
            GetWindowRect(image, &mut rc).unwrap();
        }
        let expected = drag_image_origin(hwnd, unsafe { ctx_ref(hwnd) }.unwrap(), screen);
        assert_eq!((rc.left, rc.top), expected, "行の絵がカーソルに付いて動かない");
    }

    /// 行の絵が消えている（小窓を破棄した）。
    fn assert_drag_image_gone(hwnd: HWND, image: HWND) {
        assert!(!unsafe { IsWindow(Some(image)) }.as_bool(), "行の絵が残っている");
        assert_eq!(unsafe { ctx_ref(hwnd) }.unwrap().drag_image.get(), 0);
    }

    /// ピン留めの行のドラッグ: 窓がマウスを捕まえ、動かすと落とす先の目印が変わり、離すとマウスを放して、その位置への
    /// 移動を伝える（フォルダの行の中央 → そのフォルダの末尾、フォルダの行の上端 → 一覧のフォルダのその前、
    /// ツリーのフォルダ → その末尾）。ツリーの強調は離すと消える。フォルダを自分のツリーの項目へは落とせない。
    #[test]
    fn dragging_pinned_row_reports_drop_position() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, [folder, item, _]) = window_with_folder_row();
        let hwnd = window.hwnd();
        let tree = unsafe { GetDlgItem(Some(hwnd), ID_TREE).unwrap() };
        let folder_item = unsafe { find_tree_item(ctx_ref(hwnd).unwrap(), tree, Source::Pinned(Some(folder))) }.unwrap();
        let drag_to = |index: i32, to: POINT| {
            begin_drag(&window, index);
            assert_eq!(unsafe { GetCapture() }, hwnd, "マウスを捕まえていない");
            let image = shown_drag_image(hwnd);
            mouse(hwnd, WM_MOUSEMOVE, to);
            assert_drag_image_at(hwnd, image, to);
            let mark = drop_mark(hwnd);
            let hilite = tree_item_at(tree, TVGN_DROPHILITE);
            mouse(hwnd, WM_LBUTTONUP, to);
            assert_drag_image_gone(hwnd, image);
            assert_ne!(unsafe { GetCapture() }, hwnd, "離した後もマウスを捕まえている");
            assert!(unsafe { ctx_ref(hwnd) }.unwrap().row_drag.get().is_none());
            assert_eq!(tree_item_at(tree, TVGN_DROPHILITE).0, 0, "離した後も強調が残っている");
            (mark, hilite)
        };

        assert_eq!(drag_to(1, list_row_point(&window, 0, 0.5)).0, Some(DropMark::Into(folder)));
        assert_eq!(drag_to(1, list_row_point(&window, 0, 0.1)).0, Some(DropMark::Before(Some(folder))));
        assert_eq!(drag_to(1, tree_item_point(&window, tree, folder_item)), (Some(DropMark::Tree(Some(folder))), folder_item));
        assert_eq!(drag_to(0, tree_item_point(&window, tree, folder_item)), (None, HTREEITEM(0)), "フォルダを自分の中へ落とせる");
        assert_eq!(
            recorder.commands.borrow().as_slice(),
            [
                (item, RowCommand::Place { to: Some(folder), before: None }),
                (item, RowCommand::Place { to: None, before: Some(folder) }),
                (item, RowCommand::Place { to: Some(folder), before: None }),
            ]
        );
    }

    /// ドラッグの取り消し: Esc（検索欄は消さない）・マウスを失う・隠す・終了の要求（`cancel_modal`）で、目印を消して
    /// マウスを放し、移動を伝えない。ハンドラがドラッグできないとした行（検索中のピン留めの行）、名前の編集中は
    /// ドラッグを始めない。
    #[test]
    fn row_drag_is_cancelled_and_not_started_when_not_allowed() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, [folder, ..]) = window_with_folder_row();
        let hwnd = window.hwnd();
        let tree = unsafe { GetDlgItem(Some(hwnd), ID_TREE).unwrap() };
        let folder_item = unsafe { find_tree_item(ctx_ref(hwnd).unwrap(), tree, Source::Pinned(Some(folder))) }.unwrap();
        let over_tree = tree_item_point(&window, tree, folder_item);
        unsafe {
            let _ = SetWindowTextW(search(&window), w!("x"));
        }
        let cancels: [&dyn Fn(); 3] = [
            &|| unsafe {
                SendMessageW(hwnd, WM_COMMAND, Some(WPARAM(IDCANCEL.0 as usize)), Some(LPARAM(0)));
            },
            &|| unsafe {
                let _ = ReleaseCapture();
            },
            &|| cancel_modal(hwnd),
        ];
        for cancel in cancels {
            begin_drag(&window, 1);
            let image = shown_drag_image(hwnd);
            mouse(hwnd, WM_MOUSEMOVE, over_tree);
            assert_eq!(drop_mark(hwnd), Some(DropMark::Tree(Some(folder))));
            cancel();
            assert_drag_image_gone(hwnd, image);
            assert!(unsafe { ctx_ref(hwnd) }.unwrap().row_drag.get().is_none(), "取り消されていない");
            assert_ne!(unsafe { GetCapture() }, hwnd);
            assert_eq!(tree_item_at(tree, TVGN_DROPHILITE).0, 0, "取り消した後も強調が残っている");
            // 取り消した後に離しても落とさない
            mouse(hwnd, WM_LBUTTONUP, over_tree);
        }
        assert_eq!(window_text(search(&window)), "x", "Esc でドラッグと一緒に検索欄を消した");

        let not_started = |index: i32| {
            begin_drag(&window, index);
            let started = unsafe { ctx_ref(hwnd) }.unwrap().row_drag.get().is_some() || unsafe { GetCapture() } == hwnd;
            mouse(hwnd, WM_LBUTTONUP, over_tree);
            !started
        };
        recorder.no_drag.set(true);
        assert!(not_started(1), "動かせない行のドラッグを始めた");
        recorder.no_drag.set(false);
        unsafe { ctx_ref(hwnd) }.unwrap().edit.set(EditState::Editing(EditTarget::Rename { id: folder }));
        assert!(not_started(1), "名前の編集中にドラッグを始めた");
        unsafe { ctx_ref(hwnd) }.unwrap().edit.set(EditState::None);
        assert!(recorder.commands.borrow().is_empty(), "取り消した・始めていないドラッグで移動を伝えた");
    }

    /// 履歴の行のドラッグ: ツリーのピン留めのルート・フォルダの上だけが落とす先で（ルートも、ツリーで選んでいる
    /// 所に関係なく落とせる）、離すとそこへのピン留め（`RowCommand::Pin`）を伝える。一覧の上と、ツリーの「履歴」の
    /// 上には落とせない。
    #[test]
    fn dragging_history_row_pins_into_tree_folder() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let folder = Uuid::new_v4();
        let (window, recorder, tree, [history_item, pinned_item, folder_item]) = window_with_tree(Source::History, folder);
        let hwnd = window.hwnd();
        let rows = vec![row("h1"), row("h2")];
        let id = rows[0].id;
        set_rows(hwnd, rows, false);
        pump(hwnd, 20);
        let drag_to = |to: POINT| {
            begin_drag(&window, 0);
            assert_eq!(unsafe { GetCapture() }, hwnd, "履歴の行のドラッグを始めない");
            mouse(hwnd, WM_MOUSEMOVE, to);
            let mark = drop_mark(hwnd);
            mouse(hwnd, WM_LBUTTONUP, to);
            assert_ne!(unsafe { GetCapture() }, hwnd);
            assert_eq!(tree_item_at(tree, TVGN_DROPHILITE).0, 0, "離した後も強調が残っている");
            mark
        };
        assert_eq!(drag_to(tree_item_point(&window, tree, folder_item)), Some(DropMark::Tree(Some(folder))));
        assert_eq!(drag_to(tree_item_point(&window, tree, pinned_item)), Some(DropMark::Tree(None)));
        assert_eq!(drag_to(tree_item_point(&window, tree, history_item)), None, "履歴へ落とせる");
        assert_eq!(drag_to(list_row_point(&window, 1, 0.5)), None, "履歴の一覧の中へ落とせる");
        assert_eq!(drag_to(list_row_point(&window, 0, 0.1)), None);
        assert_eq!(
            recorder.commands.borrow().as_slice(),
            [(id, RowCommand::Pin(Some(folder))), (id, RowCommand::Pin(None))]
        );
    }

    /// ツリー（「履歴」、「ピン留め」の下に f（その下に g）と h）と、ルートの中身の一覧（f の行・項目 p の行・h の行）を
    /// 作り、「ピン留め」を選んで表示する。戻り値の ID は f・g・h・p、ツリーの項目は 履歴・ピン留め・f・g・h の順。
    fn window_with_folder_tree() -> (ViewerWindow, Rc<Recorder>, HWND, [Uuid; 4], [HTREEITEM; 5]) {
        use crate::native::model::FolderCounts;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE;
        let (f, g, h) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let (window, recorder) = create_test_window();
        let hwnd = window.hwnd();
        let folder_node = |id: Uuid, label: &str, children: Vec<TreeNode>| TreeNode {
            label: label.into(),
            source: Source::Pinned(Some(id)),
            children,
        };
        let nodes = vec![
            TreeNode { label: HISTORY_LABEL.into(), source: Source::History, children: vec![] },
            TreeNode {
                label: "ピン留め".into(),
                source: Source::Pinned(None),
                children: vec![folder_node(f, "f", vec![folder_node(g, "g", vec![])]), folder_node(h, "h", vec![])],
            },
        ];
        set_tree(hwnd, &nodes, Source::Pinned(None));
        let tree = unsafe { GetDlgItem(Some(hwnd), ID_TREE).unwrap() };
        let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
        let item = |source| unsafe { find_tree_item(ctx, tree, source) }.unwrap();
        let items = [
            item(Source::History),
            item(Source::Pinned(None)),
            item(Source::Pinned(Some(f))),
            item(Source::Pinned(Some(g))),
            item(Source::Pinned(Some(h))),
        ];
        unsafe {
            SendMessageW(tree, TVM_EXPAND, Some(WPARAM(TVE_EXPAND.0 as usize)), Some(LPARAM(items[2].0)));
        }
        let folder_row = |id: Uuid, label: &str| Row { id, pinned: true, folder: Some(FolderCounts { items: 0, folders: 0 }), ..row(label) };
        let p = Row { pinned: true, ..row("p") };
        let ids = [f, g, h, p.id];
        set_rows(hwnd, vec![folder_row(f, "f"), p, folder_row(h, "h")], false);
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        pump(hwnd, 20);
        (window, recorder, tree, ids, items)
    }

    /// `TVN_BEGINDRAG` を窓へ送る（ツリーの `item` のドラッグの始まり）。
    fn begin_tree_drag_message(hwnd: HWND, tree: HWND, item: HTREEITEM) {
        let mut nm = NMTREEVIEWW {
            hdr: NMHDR { hwndFrom: tree, idFrom: ID_TREE as usize, code: TVN_BEGINDRAGW },
            itemNew: TVITEMW { hItem: item, ..Default::default() },
            ..Default::default()
        };
        unsafe {
            SendMessageW(hwnd, WM_NOTIFY, Some(WPARAM(ID_TREE as usize)), Some(LPARAM(&mut nm as *mut _ as isize)));
        }
    }

    /// ツリーの `from` をドラッグし、`to`（窓のクライアント座標）へ動かして離す。動かした後の落とす先を返す。離した
    /// 後はマウスを放し、ツリーの強調が消えている。
    fn drag_tree_item(window: &ViewerWindow, tree: HWND, from: HTREEITEM, to: POINT) -> Option<DropMark> {
        let hwnd = window.hwnd();
        begin_tree_drag_message(hwnd, tree, from);
        assert_eq!(unsafe { GetCapture() }, hwnd, "ツリーのフォルダのドラッグを始めない");
        let image = shown_drag_image(hwnd);
        mouse(hwnd, WM_MOUSEMOVE, to);
        assert_drag_image_at(hwnd, image, to);
        let mark = drop_mark(hwnd);
        mouse(hwnd, WM_LBUTTONUP, to);
        assert_drag_image_gone(hwnd, image);
        assert_ne!(unsafe { GetCapture() }, hwnd, "離した後もマウスを捕まえている");
        assert_eq!(tree_item_at(tree, TVGN_DROPHILITE).0, 0, "離した後も強調が残っている");
        mark
    }

    /// ツリーのピン留めのフォルダのドラッグ: ツリーのほかのフォルダ・ルートの上で離すとその末尾へ、一覧の行の間・
    /// フォルダの行の上で離すと一覧に出しているフォルダのその位置・そのフォルダの中へ移す。自分・自分の中・今いる
    /// フォルダ・「履歴」の上には落とせない。
    #[test]
    fn dragging_tree_folder_moves_it_into_tree_or_list() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, tree, [f, g, h, p], [history, root, f_item, g_item, h_item]) = window_with_folder_tree();
        let at = |item| tree_item_point(&window, tree, item);
        let drag = |from, to| drag_tree_item(&window, tree, from, to);

        assert_eq!(drag(f_item, at(h_item)), Some(DropMark::Tree(Some(h))));
        assert_eq!(drag(f_item, at(f_item)), None, "自分へ落とせる");
        assert_eq!(drag(f_item, at(g_item)), None, "自分の中へ落とせる");
        assert_eq!(drag(f_item, at(root)), None, "今いるフォルダへ落とせる");
        assert_eq!(drag(f_item, at(history)), None);
        // 一覧（ルート）: h の行の下端は末尾、真ん中は h の中
        assert_eq!(drag(f_item, list_row_point(&window, 2, 0.9)), Some(DropMark::Before(None)));
        assert_eq!(drag(f_item, list_row_point(&window, 2, 0.5)), Some(DropMark::Into(h)));
        // f の中の g: 今いる f へは落とせず、ルートへは落とせる。一覧（ルート）の p の前へも落とせる
        assert_eq!(drag(g_item, at(f_item)), None);
        assert_eq!(drag(g_item, at(root)), Some(DropMark::Tree(None)));
        assert_eq!(drag(g_item, list_row_point(&window, 1, 0.1)), Some(DropMark::Before(Some(p))));
        assert_eq!(
            recorder.commands.borrow().as_slice(),
            [
                (f, RowCommand::Place { to: Some(h), before: None }),
                (f, RowCommand::Place { to: None, before: None }),
                (f, RowCommand::Place { to: Some(h), before: None }),
                (g, RowCommand::Place { to: None, before: None }),
                (g, RowCommand::Place { to: None, before: Some(p) }),
            ]
        );
        assert!(recorder.selected.borrow().is_empty(), "ドラッグでツリーの選択が変わった");
    }

    /// ツリーのフォルダのドラッグで一覧の上に落とせないのは、一覧にそのフォルダ自身かその中を出しているとき、行を
    /// 動かせないとき（検索中）、ピン留めでないものを出しているとき。そのときもツリーの上へは落とせる。ルートの
    /// 「ピン留め」と「履歴」はドラッグを始めない。
    #[test]
    fn tree_folder_drag_drops_on_list_only_when_list_is_outside_it() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, tree, [f, _g, h, _p], [history, root, f_item, g_item, h_item]) = window_with_folder_tree();
        let hwnd = window.hwnd();
        let select = |item: HTREEITEM| unsafe {
            SendMessageW(tree, TVM_SELECTITEM, Some(WPARAM(TVGN_CARET as usize)), Some(LPARAM(item.0)));
        };
        let on_list = list_row_point(&window, 2, 0.9);
        let to_h = tree_item_point(&window, tree, h_item);
        let drag = |from, to| drag_tree_item(&window, tree, from, to);

        select(f_item);
        assert_eq!(drag(f_item, on_list), None, "自分を出している一覧へ落とせる");
        select(g_item);
        assert_eq!(drag(f_item, on_list), None, "自分の中を出している一覧へ落とせる");
        assert_eq!(drag(f_item, to_h), Some(DropMark::Tree(Some(h))));
        select(h_item);
        assert_eq!(drag(f_item, on_list), Some(DropMark::Before(None)), "ほかのフォルダを出している一覧へ落とせない");
        select(root);
        recorder.no_drag.set(true);
        assert_eq!(drag(f_item, on_list), None, "検索中の一覧へ落とせる");
        recorder.no_drag.set(false);
        select(history);
        assert_eq!(drag(f_item, on_list), None, "履歴の一覧へ落とせる");

        for item in [root, history] {
            begin_tree_drag_message(hwnd, tree, item);
            assert!(unsafe { ctx_ref(hwnd) }.unwrap().row_drag.get().is_none(), "ルート・履歴のドラッグを始めた");
            assert_ne!(unsafe { GetCapture() }, hwnd);
        }
        assert_eq!(
            recorder.commands.borrow().as_slice(),
            [(f, RowCommand::Place { to: Some(h), before: None }), (f, RowCommand::Place { to: Some(h), before: None })]
        );
    }

    /// 右ボタンでの取り消し: 押すとドラッグを取り消して目印を消すが、右ボタンを離すまでマウスを捕まえたままにし、
    /// その間に左ボタンを離しても落とさない。右ボタンを離すとマウスを放す。待っている間に隠す・終了の要求
    /// （`cancel_modal`）が来ても放す。ドラッグしていないときの右ボタンは扱わない。
    #[test]
    fn right_button_cancels_row_drag_and_holds_capture_until_release() {
        use windows::Win32::UI::WindowsAndMessaging::{WM_RBUTTONDOWN, WM_RBUTTONUP};
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, [folder, ..]) = window_with_folder_row();
        let hwnd = window.hwnd();
        let tree = unsafe { GetDlgItem(Some(hwnd), ID_TREE).unwrap() };
        let folder_item = unsafe { find_tree_item(ctx_ref(hwnd).unwrap(), tree, Source::Pinned(Some(folder))) }.unwrap();
        let over_tree = tree_item_point(&window, tree, folder_item);
        let ctx = unsafe { ctx_ref(hwnd) }.unwrap();

        begin_drag(&window, 1);
        let image = shown_drag_image(hwnd);
        mouse(hwnd, WM_MOUSEMOVE, over_tree);
        assert_eq!(drop_mark(hwnd), Some(DropMark::Tree(Some(folder))));
        mouse(hwnd, WM_RBUTTONDOWN, over_tree);
        assert_drag_image_gone(hwnd, image);
        assert!(ctx.row_drag.get().is_none(), "右ボタンで取り消されない");
        assert_eq!(tree_item_at(tree, TVGN_DROPHILITE).0, 0, "取り消した後も強調が残っている");
        assert_eq!(unsafe { GetCapture() }, hwnd, "右ボタンを離す前にマウスを放した");
        mouse(hwnd, WM_LBUTTONUP, over_tree);
        assert_eq!(unsafe { GetCapture() }, hwnd);
        mouse(hwnd, WM_RBUTTONUP, over_tree);
        assert_ne!(unsafe { GetCapture() }, hwnd, "右ボタンを離してもマウスを放さない");
        assert!(!ctx.row_drag_right_up.get());

        begin_drag(&window, 1);
        mouse(hwnd, WM_RBUTTONDOWN, over_tree);
        cancel_modal(hwnd);
        assert_ne!(unsafe { GetCapture() }, hwnd, "右ボタンを待つ間の隠す要求でマウスを放さない");
        assert!(!ctx.row_drag_right_up.get());

        // ドラッグしていなければ扱わない（待つ状態にならない）
        mouse(hwnd, WM_RBUTTONDOWN, over_tree);
        assert!(!ctx.row_drag_right_up.get());
        mouse(hwnd, WM_RBUTTONUP, over_tree);
        assert!(recorder.commands.borrow().is_empty(), "取り消したドラッグで移動を伝えた");
    }

    /// 行のドラッグ中のキー操作（一覧の Enter・Delete・F2、Alt+↑・Alt+↓、ツリーの F2）は何もしない: 何も伝えず、
    /// フォルダを開かず、ダイアログを開かず、名前の編集を始めない。ドラッグは続き、マウスも捕まえたまま。右ボタンでの
    /// 取り消しの後、右ボタンを離すのを待つ間も同じ。選んでいる行がフォルダの行でも項目の行でも同じ。
    #[test]
    fn keys_during_row_drag_do_nothing() {
        use windows::Win32::UI::Input::KeyboardAndMouse::VK_RETURN;
        use windows::Win32::UI::WindowsAndMessaging::{WM_KEYDOWN, WM_RBUTTONDOWN, WM_RBUTTONUP};
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, [folder, ..]) = window_with_folder_row();
        let hwnd = window.hwnd();
        let tree = unsafe { GetDlgItem(Some(hwnd), ID_TREE).unwrap() };
        let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
        // ツリーの F2 が名前の編集を始められるよう、ツリーではフォルダを選んでおく
        unsafe {
            let item = find_tree_item(ctx, tree, Source::Pinned(Some(folder))).unwrap();
            SendMessageW(tree, TVM_SELECTITEM, Some(WPARAM(TVGN_CARET as usize)), Some(LPARAM(item.0)));
        }
        recorder.selected.borrow_mut().clear();
        let press = |target: HWND, vk: u16| unsafe {
            SendMessageW(target, WM_KEYDOWN, Some(WPARAM(vk as usize)), Some(LPARAM(0)));
        };
        let try_keys = |selected: i32| {
            select_row(&window, selected);
            unsafe {
                let _ = SetFocus(Some(list(&window)));
            }
            // ダイアログが開いてしまったら、予備のタイマーが閉じる（`finish_menu_test` が真になる）
            during_dialog(hwnd, || {});
            press_through_dialog_manager(&window, list(&window), VK_RETURN.0);
            press(list(&window), VK_DELETE.0);
            press(list(&window), VK_F2.0);
            send_command(hwnd, CMD_MOVE_UP);
            send_command(hwnd, CMD_MOVE_DOWN);
            press(tree, VK_F2.0);
            pump(hwnd, 30);
            assert!(!finish_menu_test(), "ドラッグ中のキー操作でダイアログを開いた");
            assert_eq!(ctx.edit.get(), EditState::None, "ドラッグ中に名前の編集を始めた");
            assert_eq!(unsafe { GetCapture() }, hwnd, "キー操作でマウスを放した");
        };

        for selected in [0, 1] {
            begin_drag(&window, 1);
            try_keys(selected);
            assert!(ctx.row_drag.get().is_some(), "キー操作でドラッグが終わった");
            unsafe {
                SendMessageW(hwnd, WM_COMMAND, Some(WPARAM(IDCANCEL.0 as usize)), Some(LPARAM(0)));
            }
            assert!(ctx.row_drag.get().is_none());
        }
        // 右ボタンを離すのを待つ間
        begin_drag(&window, 1);
        mouse(hwnd, WM_RBUTTONDOWN, POINT { x: 0, y: 0 });
        assert!(ctx.row_drag_right_up.get());
        try_keys(1);
        mouse(hwnd, WM_RBUTTONUP, POINT { x: 0, y: 0 });
        assert!(!ctx.row_drag_right_up.get());

        assert!(recorder.commands.borrow().is_empty(), "ドラッグ中のキー操作を伝えた");
        assert!(recorder.activated.borrow().is_empty(), "ドラッグ中に行を送った");
        assert!(recorder.deleted.borrow().is_empty(), "ドラッグ中に行を消した");
        assert!(recorder.tree_commands.borrow().is_empty(), "ドラッグ中にフォルダを消した・名前を変えた");
        assert!(recorder.renamed.borrow().is_empty(), "ドラッグ中に名前を変えた");
        assert!(recorder.selected.borrow().is_empty(), "ドラッグ中にフォルダを開いた");
    }

    /// ドラッグの間にツリーの選択が変わる（一覧が別のフォルダを出す）・検索を始める（ハンドラがドラッグを許さなく
    /// なる）と、一覧の上には落とせない（目印を出さず、離しても伝えない）。元に戻れば、また一覧の上に落とせる。
    /// 起床による作り直しでも、同じ表示元のままなら落とせ、表示中のフォルダが消えて「履歴」へ切り替わると落とせない。
    #[test]
    fn row_drag_does_not_drop_on_list_after_list_changes() {
        use crate::native::model::FolderCounts;
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, [folder, item, history]) = window_with_folder_row();
        let hwnd = window.hwnd();
        let tree = unsafe { GetDlgItem(Some(hwnd), ID_TREE).unwrap() };
        let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
        let select_tree = |source: Source| unsafe {
            let item = find_tree_item(ctx, tree, source).unwrap();
            SendMessageW(tree, TVM_SELECTITEM, Some(WPARAM(TVGN_CARET as usize)), Some(LPARAM(item.0)));
        };
        // フォルダの行の中央（落とせればそのフォルダの中）
        let on_folder_row = list_row_point(&window, 0, 0.5);
        let mark_over_list = || {
            mouse(hwnd, WM_MOUSEMOVE, on_folder_row);
            drop_mark(hwnd)
        };

        begin_drag(&window, 1);
        assert_eq!(mark_over_list(), Some(DropMark::Into(folder)));
        select_tree(Source::Pinned(Some(folder)));
        assert_eq!(mark_over_list(), None, "ツリーの選択が変わった後も一覧の上に落とせる");
        select_tree(Source::Pinned(None));
        assert_eq!(mark_over_list(), Some(DropMark::Into(folder)), "元の選択に戻っても一覧の上に落とせない");
        recorder.no_drag.set(true);
        assert_eq!(mark_over_list(), None, "検索を始めた後も一覧の上に落とせる");
        recorder.no_drag.set(false);
        assert_eq!(mark_over_list(), Some(DropMark::Into(folder)));

        // 起床による作り直し（`App::rebuild` の `set_tree`・`set_rows` と同じ呼び方）
        let tree_nodes = |children: Vec<TreeNode>| {
            vec![
                TreeNode { label: HISTORY_LABEL.into(), source: Source::History, children: vec![] },
                TreeNode { label: "ピン留め".into(), source: Source::Pinned(None), children },
            ]
        };
        let folder_node = TreeNode { label: "f".into(), source: Source::Pinned(Some(folder)), children: vec![] };
        set_tree(hwnd, &tree_nodes(vec![folder_node]), Source::Pinned(None));
        let rows = vec![
            Row { id: folder, pinned: true, folder: Some(FolderCounts { items: 0, folders: 0 }), ..row("f") },
            Row { id: item, pinned: true, ..row("p") },
            Row { id: history, ..row("h") },
        ];
        set_rows(hwnd, rows, false);
        assert_eq!(mark_over_list(), Some(DropMark::Into(folder)), "同じ表示元で作り直した後に一覧の上に落とせない");
        set_tree(hwnd, &tree_nodes(vec![]), Source::History);
        set_rows(hwnd, vec![Row { id: history, ..row("h") }], false);
        assert_eq!(mark_over_list(), None, "表示中のフォルダが消えて履歴へ切り替わった後も一覧の上に落とせる");

        mouse(hwnd, WM_LBUTTONUP, on_folder_row);
        assert!(ctx.row_drag.get().is_none());
        assert_ne!(unsafe { GetCapture() }, hwnd);
        assert!(recorder.commands.borrow().is_empty(), "一覧が変わった後に一覧の上へ落とした");
    }

    /// 実際のマウスの入力で、`from` を押して `over` まで動かす（どちらも窓のクライアント座標。`from` の下は子の窓
    /// `press_on` のはず）。押して動かす・離すのは別のスレッドから行い（一覧・ツリーはドラッグを検知するまで押下の
    /// 処理から戻らない）、このスレッドはメッセージを処理し続ける。動かし終えたときに、窓がマウスを捕まえているかと
    /// 落とす先の目印を記録して返す。その後、`right_cancel` なら右ボタンを押して離してから、左ボタンを離す（そうで
    /// なければ左ボタンを離すだけ）。カーソルは最後に元へ戻す。
    fn real_mouse_drag(
        window: &ViewerWindow,
        press_on: HWND,
        from: POINT,
        over: POINT,
        right_cancel: bool,
    ) -> Option<(bool, Option<DropMark>)> {
        use std::sync::mpsc;
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_RIGHTDOWN,
            MOUSEEVENTF_RIGHTUP, MOUSEINPUT, MOUSE_EVENT_FLAGS,
        };
        use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;
        let hwnd = window.hwnd();
        // 位置は決めて置く（CW_USEDEFAULT は作るたびにずらして置くので、ほかのテストの後では行が画面の外に出うる）
        unsafe {
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 100, 100, 0, 0, SWP_NOSIZE);
        }
        pump(hwnd, 50);
        let to_screen = |mut p: POINT| {
            let _ = unsafe { ClientToScreen(hwnd, &mut p) };
            (p.x, p.y)
        };
        let from = to_screen(from);
        let over = to_screen(over);
        let under = unsafe { windows::Win32::UI::WindowsAndMessaging::WindowFromPoint(POINT { x: from.0, y: from.1 }) };
        let (mut rect, ex) = (RECT::default(), unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetWindowLongW(hwnd, windows::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE)
        });
        let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut rect) };
        assert_eq!(
            under,
            press_on,
            "押す位置 {from:?} の下が押すはずの窓ではない（窓 {rect:?}、拡張スタイル {ex:#x}、表示 {}）",
            is_visible(hwnd)
        );
        let mut saved = POINT::default();
        unsafe { GetCursorPos(&mut saved) }.unwrap();
        let (moved_tx, moved) = mpsc::channel();
        let (ack, ack_rx) = mpsc::channel::<()>();
        let input = std::thread::spawn(move || {
            let button = |flags: MOUSE_EVENT_FLAGS| unsafe {
                let event = INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { dwFlags: flags, ..Default::default() } } };
                SendInput(&[event], std::mem::size_of::<INPUT>() as i32);
            };
            let pause = |ms| std::thread::sleep(std::time::Duration::from_millis(ms));
            unsafe { SetCursorPos(from.0, from.1) }.unwrap();
            pause(100);
            button(MOUSEEVENTF_LEFTDOWN);
            pause(100);
            // ドラッグと判定される幅を超えるよう、少しずつ動かす
            for step in 1..=10 {
                unsafe { SetCursorPos(from.0 + (over.0 - from.0) * step / 10, from.1 + (over.1 - from.1) * step / 10) }.unwrap();
                pause(30);
            }
            pause(100);
            moved_tx.send(()).unwrap();
            let _ = ack_rx.recv_timeout(std::time::Duration::from_secs(5));
            if right_cancel {
                button(MOUSEEVENTF_RIGHTDOWN);
                pause(100);
                button(MOUSEEVENTF_RIGHTUP);
                pause(100);
            }
            button(MOUSEEVENTF_LEFTUP);
            pause(100);
            unsafe { SetCursorPos(saved.x, saved.y) }.unwrap();
        });
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut during = None;
        while std::time::Instant::now() < until && !input.is_finished() {
            pump(hwnd, 10);
            if during.is_none() && moved.try_recv().is_ok() {
                during = Some((unsafe { GetCapture() } == hwnd, drop_mark(hwnd)));
                ack.send(()).unwrap();
            }
        }
        input.join().unwrap();
        pump(hwnd, 100);
        during
    }

    /// 実際のマウスの入力で、ピン留めの行を押して動かすと一覧がドラッグを検知し（`LVN_BEGINDRAG`）、窓がマウスを
    /// 捕まえて、フォルダの行の上で離すとそのフォルダへの移動を伝える。
    /// マウスのカーソルを動かす（終わったら戻す）ので、通常の実行では動かさない。
    #[test]
    #[ignore]
    fn real_mouse_drag_moves_row_into_folder() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, [folder, item, _]) = window_with_folder_row();
        let during = real_mouse_drag(&window, list(&window), list_row_point(&window, 1, 0.5), list_row_point(&window, 0, 0.5), false);
        assert_eq!(during, Some((true, Some(DropMark::Into(folder)))), "ドラッグが始まらない・目印が違う");
        assert_ne!(unsafe { GetCapture() }, window.hwnd());
        assert_eq!(recorder.commands.borrow().as_slice(), [(item, RowCommand::Place { to: Some(folder), before: None })]);
    }

    /// 実際のマウスの入力で、ツリーのピン留めのフォルダを押して動かすとツリーがドラッグを検知し（`TVN_BEGINDRAG`）、
    /// 窓がマウスを捕まえる。押してもツリーの選択（一覧に出しているフォルダ）は変わらないので、一覧の上にも落とせる。
    /// ツリーのほかのフォルダの上で離すとその中へ、一覧の最後の行の下端で離すと一覧のフォルダの末尾へ移す。
    /// マウスのカーソルを動かす（終わったら戻す）ので、通常の実行では動かさない。
    #[test]
    #[ignore]
    fn real_mouse_tree_folder_drag_moves_into_tree_and_list() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, tree, [f, _g, h, _p], [_history, _root, f_item, _g_item, h_item]) = window_with_folder_tree();
        let hwnd = window.hwnd();
        let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
        let during = real_mouse_drag(&window, tree, tree_item_point(&window, tree, f_item), tree_item_point(&window, tree, h_item), false);
        assert_eq!(during, Some((true, Some(DropMark::Tree(Some(h))))), "ドラッグが始まらない・目印が違う");
        assert_eq!(unsafe { selected_tree_source(ctx, tree) }, Some(Source::Pinned(None)), "押してツリーの選択が変わった");
        let during = real_mouse_drag(&window, tree, tree_item_point(&window, tree, f_item), list_row_point(&window, 2, 0.9), false);
        assert_eq!(during, Some((true, Some(DropMark::Before(None)))), "一覧の上に落とせない");
        assert_eq!(unsafe { selected_tree_source(ctx, tree) }, Some(Source::Pinned(None)));
        assert_ne!(unsafe { GetCapture() }, hwnd);
        assert!(recorder.selected.borrow().is_empty(), "ドラッグで表示元の切り替えを伝えた");
        assert_eq!(
            recorder.commands.borrow().as_slice(),
            [(f, RowCommand::Place { to: Some(h), before: None }), (f, RowCommand::Place { to: None, before: None })]
        );
    }

    /// 実際のマウスの入力で、ドラッグ中に右クリックすると取り消され、その後に左ボタンを離しても移動を伝えない。
    /// 右ボタンを離す操作が一覧へ渡って、右クリックのメニューを求めることもない。マウスは最後に放している。
    /// マウスのカーソルを動かす（終わったら戻す）ので、通常の実行では動かさない。
    #[test]
    #[ignore]
    fn real_mouse_right_click_cancels_drag_without_menu() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, [folder, ..]) = window_with_folder_row();
        let during = real_mouse_drag(&window, list(&window), list_row_point(&window, 1, 0.5), list_row_point(&window, 0, 0.5), true);
        assert_eq!(during, Some((true, Some(DropMark::Into(folder)))), "ドラッグが始まらない・目印が違う");
        let ctx = unsafe { ctx_ref(window.hwnd()) }.unwrap();
        assert!(ctx.row_drag.get().is_none() && !ctx.row_drag_right_up.get());
        assert_ne!(unsafe { GetCapture() }, window.hwnd(), "マウスを放していない");
        assert!(recorder.commands.borrow().is_empty(), "取り消したのに移動を伝えた");
        assert!(recorder.menu_requests.borrow().is_empty(), "右ボタンを離す操作で右クリックのメニューを求めた");
        assert!(recorder.tree_menu_requests.borrow().is_empty());
    }

    // --- ツリーの名前の編集 ---

    fn edit_control(tree: HWND) -> HWND {
        use windows::Win32::UI::Controls::TVM_GETEDITCONTROL;
        HWND(unsafe { SendMessageW(tree, TVM_GETEDITCONTROL, None, None).0 } as *mut _)
    }

    /// `IsDialogMessageW` が Enter・Esc から作るのと同じ `WM_COMMAND`（code 0・lParam 0）を送り、後始末まで進める。
    fn dialog_key(hwnd: HWND, id: i32) {
        unsafe {
            SendMessageW(hwnd, WM_COMMAND, Some(WPARAM(id as usize)), Some(LPARAM(0)));
        }
        pump(hwnd, 30);
    }

    fn item_label(tree: HWND, item: HTREEITEM) -> String {
        let mut buf = [0u16; 64];
        let mut tv = TVITEMW {
            mask: TVIF_HANDLE | TVIF_TEXT,
            hItem: item,
            pszText: PWSTR(buf.as_mut_ptr()),
            cchTextMax: buf.len() as i32,
            ..Default::default()
        };
        unsafe {
            SendMessageW(tree, TVM_GETITEMW, None, Some(LPARAM(&mut tv as *mut _ as isize)));
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }

    /// 子の項目（`parent` の子を順に）。
    fn children(tree: HWND, parent: HTREEITEM) -> Vec<HTREEITEM> {
        let next = |flag: u32, from: HTREEITEM| unsafe {
            HTREEITEM(SendMessageW(tree, TVM_GETNEXTITEM, Some(WPARAM(flag as usize)), Some(LPARAM(from.0))).0)
        };
        let mut out = Vec::new();
        let mut item = next(TVGN_CHILD, parent);
        while item.0 != 0 {
            out.push(item);
            item = next(TVGN_NEXT, item);
        }
        out
    }

    /// メニューから名前の変更を選ぶと編集が始まる。編集欄の通知（id 1 の EN_CHANGE など lParam が 0 でない
    /// WM_COMMAND）では確定しない。Enter（IDOK）で確定し、前後の空白を除いた名前で頼む。ツリーの文字は
    /// 変えない（保存が済んでから表示する）。選択・表示元は変わらない。
    #[test]
    fn rename_from_menu_commits_on_enter_and_keeps_label_until_saved() {
        use windows::Win32::UI::WindowsAndMessaging::EN_CHANGE;
        let _gui = crate::tray::lock_gui_resource_tests();
        let folder = Uuid::new_v4();
        let (window, recorder, tree, [history, _pinned, folder_item]) = window_with_tree(Source::History, folder);
        let hwnd = window.hwnd();
        let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
        *recorder.tree_menu.borrow_mut() = Some(TreeMenu::PinnedFolder { id: folder, title: "f".into(), items: 0, folders: 0 });

        // 「フォルダの作成」「名前の変更」「（区切り線）」「削除...」の2番目
        choose_in_menu(hwnd, KEY_SECOND, None);
        let at = tree_point(tree, folder_item, false);
        tree_context_menu(hwnd, tree, at.x, at.y);
        assert!(!finish_menu_test(), "メニューで選べなかった");
        assert_eq!(ctx.edit.get(), EditState::Editing(EditTarget::Rename { id: folder }));
        let edit = edit_control(tree);
        assert!(!edit.is_invalid(), "編集欄が無い");
        assert!(notice_blocked(hwnd), "編集中に失敗の通知を止めていない");

        unsafe {
            let _ = SetWindowTextW(edit, w!("  新しい名前 "));
            SendMessageW(hwnd, WM_COMMAND, Some(WPARAM(1 | (EN_CHANGE as usize) << 16)), Some(LPARAM(edit.0 as isize)));
        }
        assert!(matches!(ctx.edit.get(), EditState::Editing(_)), "編集欄の通知で編集が終わった");
        dialog_key(hwnd, IDOK.0);
        assert_eq!(ctx.edit.get(), EditState::None);
        assert_eq!(
            recorder.tree_commands.borrow().as_slice(),
            [TreeCommand::RenameFolder { id: folder, title: "新しい名前".into() }]
        );
        assert_eq!(item_label(tree, folder_item), "f", "保存を待たずに名前を変えた");
        assert_eq!(tree_item_at(tree, TVGN_CARET), history);
        assert!(recorder.selected.borrow().is_empty(), "表示元の切り替えを伝えた");
        assert!(!notice_blocked(hwnd));
    }

    /// ピン留めの根のメニューから作成を選ぶと、根の子の末尾に仮の項目（TEMP_TOKEN）を出して名前を入れる。
    /// Esc（IDCANCEL）・空の名前では何も頼まず、確定なら親と名前で頼む。どの場合も後始末で仮の項目を消す。
    #[test]
    fn create_from_menu_uses_temporary_item_and_removes_it() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let folder = Uuid::new_v4();
        let (window, recorder, tree, [_history, pinned, _folder_item]) = window_with_tree(Source::History, folder);
        let hwnd = window.hwnd();
        let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
        *recorder.tree_menu.borrow_mut() = Some(TreeMenu::PinnedRoot);
        let start = || {
            choose_in_menu(hwnd, KEY_FIRST, None);
            let at = tree_point(tree, pinned, false);
            tree_context_menu(hwnd, tree, at.x, at.y);
            assert!(!finish_menu_test(), "メニューで選べなかった");
        };

        start();
        assert_eq!(ctx.edit.get(), EditState::Editing(EditTarget::Create { parent: None }));
        let kids = children(tree, pinned);
        assert_eq!(kids.len(), 2, "仮の項目が無い");
        assert_eq!(item_label(tree, kids[1]), NEW_FOLDER_LABEL);
        assert_eq!(unsafe { tree_item_source(ctx, tree, kids[1]) }, None, "仮の項目が表示元になっている");
        dialog_key(hwnd, IDCANCEL.0);
        assert_eq!(ctx.edit.get(), EditState::None);
        assert_eq!(children(tree, pinned).len(), 1, "取り消しの後に仮の項目が残った");

        start();
        unsafe {
            let _ = SetWindowTextW(edit_control(tree), w!(" \u{3000} "));
        }
        dialog_key(hwnd, IDOK.0);
        assert!(recorder.tree_commands.borrow().is_empty(), "空の名前で頼んだ");

        start();
        unsafe {
            let _ = SetWindowTextW(edit_control(tree), w!("箱"));
        }
        dialog_key(hwnd, IDOK.0);
        assert_eq!(
            recorder.tree_commands.borrow().as_slice(),
            [TreeCommand::CreateFolder { parent: None, title: "箱".into() }]
        );
        assert_eq!(children(tree, pinned).len(), 1, "確定の後に仮の項目が残った");
        assert!(recorder.selected.borrow().is_empty());
    }

    /// F2 は選んでいるピン留めのフォルダの名前の変更を始める。ほかの項目では始めない。
    #[test]
    fn f2_starts_rename_only_on_pinned_folder() {
        use windows::Win32::UI::WindowsAndMessaging::WM_KEYDOWN;
        let _gui = crate::tray::lock_gui_resource_tests();
        let folder = Uuid::new_v4();
        let (window, _recorder, tree, _items) = window_with_tree(Source::History, folder);
        let hwnd = window.hwnd();
        let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
        let f2 = || unsafe {
            SendMessageW(tree, WM_KEYDOWN, Some(WPARAM(VK_F2.0 as usize)), Some(LPARAM(0)));
        };
        f2();
        assert_eq!(ctx.edit.get(), EditState::None, "履歴で名前の変更を始めた");

        let (window, _recorder, tree, _items) = window_with_tree(Source::Pinned(Some(folder)), folder);
        let hwnd = window.hwnd();
        let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
        unsafe {
            SendMessageW(tree, WM_KEYDOWN, Some(WPARAM(VK_F2.0 as usize)), Some(LPARAM(0)));
        }
        assert_eq!(ctx.edit.get(), EditState::Editing(EditTarget::Rename { id: folder }));
        dialog_key(hwnd, IDCANCEL.0);
        assert_eq!(ctx.edit.get(), EditState::None);
    }

    fn nodes_without_folder() -> Vec<TreeNode> {
        vec![
            TreeNode { label: HISTORY_LABEL.into(), source: Source::History, children: vec![] },
            TreeNode { label: "ピン留め".into(), source: Source::Pinned(None), children: vec![] },
        ]
    }

    /// 編集中のツリーの作り直しは保留し、後始末で最後の構成で作り直す。選ぶ表示元は、編集を始めたときの
    /// 選択が消えていれば保留の `selected`。アプリの表示元と同じなら伝えない。
    #[test]
    fn tree_rebuild_during_edit_is_held_and_applied_after() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let folder = Uuid::new_v4();
        let (window, recorder, tree, [_history, pinned, folder_item]) =
            window_with_tree(Source::Pinned(Some(folder)), folder);
        let hwnd = window.hwnd();
        let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
        unsafe { begin_edit(hwnd, ctx, EditTarget::Rename { id: folder }) };
        assert!(matches!(ctx.edit.get(), EditState::Editing(_)));

        // 編集中に、フォルダが消えた構成が来て、アプリは表示元を履歴へ戻した
        set_tree(hwnd, &nodes_without_folder(), Source::History);
        assert_eq!(children(tree, pinned), [folder_item], "編集中に作り直した");
        assert!(!edit_control(tree).is_invalid(), "編集欄が消えた");
        dialog_key(hwnd, IDCANCEL.0);
        assert_eq!(ctx.edit.get(), EditState::None);
        let pinned_now = tree_item_at(tree, TVGN_CARET);
        assert_eq!(unsafe { tree_item_source(ctx, tree, pinned_now) }, Some(Source::History), "消えた表示元を選んだ");
        let pinned_item = unsafe { find_tree_item(ctx, tree, Source::Pinned(None)) }.unwrap();
        assert!(children(tree, pinned_item).is_empty(), "保留した構成で作り直していない");
        assert!(ctx.tree_pending.borrow().is_none(), "保留を使い切っていない");
        assert!(recorder.selected.borrow().is_empty(), "アプリと同じ表示元なのに伝えた");
    }

    /// 編集の間にユーザーがマウスで選んだ表示元は、後始末で採り、アプリへ1回伝える。
    #[test]
    fn user_choice_during_edit_is_adopted_and_reported_once() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let folder = Uuid::new_v4();
        let (window, recorder, tree, [history, _pinned, _folder_item]) =
            window_with_tree(Source::Pinned(Some(folder)), folder);
        let hwnd = window.hwnd();
        let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
        unsafe { begin_edit(hwnd, ctx, EditTarget::Rename { id: folder }) };

        // ツリーの TVN_SELCHANGED（マウスで「履歴」を選んだ）を届ける
        let mut nm = NMTREEVIEWW::default();
        nm.hdr = NMHDR { hwndFrom: tree, idFrom: ID_TREE as usize, code: TVN_SELCHANGEDW };
        nm.action = TVC_BYMOUSE;
        nm.itemNew.hItem = history;
        nm.itemNew.lParam = LPARAM(0);
        unsafe {
            SendMessageW(hwnd, WM_NOTIFY, Some(WPARAM(ID_TREE as usize)), Some(LPARAM(&nm as *const _ as isize)));
        }
        assert!(recorder.selected.borrow().is_empty(), "編集中に表示元の切り替えを伝えた");
        dialog_key(hwnd, IDCANCEL.0);
        assert_eq!(recorder.selected.borrow().as_slice(), [Source::History]);
        assert_eq!(tree_item_at(tree, TVGN_CARET), history);
    }

    /// 隠す・終了の要求（`cancel_modal`）は編集を取り消す。始めている途中（フォーカスを移した後・
    /// `TVN_BEGINLABELEDIT` の前）に来ても開始を止め、後始末は一度だけで、仮の項目も消える。
    #[test]
    fn cancel_request_stops_edit_at_every_stage() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let folder = Uuid::new_v4();
        let (window, recorder, tree, [_history, pinned, _folder_item]) = window_with_tree(Source::History, folder);
        let hwnd = window.hwnd();
        let ctx = unsafe { ctx_ref(hwnd) }.unwrap();

        unsafe { begin_edit(hwnd, ctx, EditTarget::Rename { id: folder }) };
        assert!(matches!(ctx.edit.get(), EditState::Editing(_)));
        cancel_modal(hwnd);
        assert_eq!(ctx.edit.get(), EditState::Finishing);
        pump(hwnd, 30);
        assert_eq!(ctx.edit.get(), EditState::None);

        for point in ["after_focus", "before_begin"] {
            EDIT_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move |hwnd, at| {
                    if at == point {
                        cancel_modal(hwnd);
                    }
                }))
            });
            unsafe { begin_edit(hwnd, ctx, EditTarget::Create { parent: None }) };
            EDIT_HOOK.with(|hook| *hook.borrow_mut() = None);
            assert!(edit_control(tree).is_invalid(), "{point}: 取り消したのに編集が始まった");
            assert_eq!(ctx.edit.get(), EditState::Finishing, "{point}");
            pump(hwnd, 30);
            assert_eq!(ctx.edit.get(), EditState::None, "{point}");
            assert!(ctx.edit_done.borrow().is_none());
            assert_eq!(children(tree, pinned).len(), 1, "{point}: 仮の項目が残った");
        }
        assert!(recorder.tree_commands.borrow().is_empty());
    }

    /// 編集中（後始末が済むまで）は、ツリー・一覧の右クリックメニュー、新しい編集、右ボタンの押下の追跡を
    /// 止める。
    #[test]
    fn menus_and_new_edits_are_blocked_while_editing() {
        use windows::Win32::UI::WindowsAndMessaging::WM_RBUTTONDOWN;
        let _gui = crate::tray::lock_gui_resource_tests();
        let folder = Uuid::new_v4();
        let (window, recorder, tree, [history, _pinned, _folder_item]) = window_with_tree(Source::History, folder);
        let hwnd = window.hwnd();
        let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
        *recorder.tree_menu.borrow_mut() = Some(TreeMenu::History);
        *recorder.menu.borrow_mut() = Some(RowMenu::default());
        unsafe { begin_edit(hwnd, ctx, EditTarget::Rename { id: folder }) };

        assert_no_menu(hwnd, || tree_context_menu(hwnd, tree, -1, -1), "編集中にツリーのメニューを出した");
        assert_no_menu(hwnd, || context_menu_at(&window, -1, -1), "編集中に一覧のメニューを出した");
        assert!(recorder.tree_menu_requests.borrow().is_empty());
        let at = tree_point(tree, history, false);
        let mut client = at;
        unsafe {
            let _ = ScreenToClient(tree, &mut client);
            SendMessageW(
                tree,
                WM_RBUTTONDOWN,
                Some(WPARAM(0x0002)),
                Some(LPARAM(((client.y as u16 as u32) << 16 | client.x as u16 as u32) as isize)),
            );
        }
        assert_eq!(ctx.tree_rpress.get(), None, "編集中に右ボタンの押下を追った");
        unsafe { begin_edit(hwnd, ctx, EditTarget::Create { parent: None }) };
        assert_eq!(ctx.edit.get(), EditState::Editing(EditTarget::Rename { id: folder }), "編集中に別の編集を始めた");
        dialog_key(hwnd, IDCANCEL.0);
        assert_eq!(ctx.edit.get(), EditState::None);
    }

    /// 名前の編集の後始末を待つ間（`Finishing`）の Enter・Esc は、編集にも一覧・検索欄にも使わない
    /// （検索欄を消さない）。
    #[test]
    fn enter_and_esc_while_finishing_edit_are_swallowed() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let folder = Uuid::new_v4();
        let (window, recorder, tree, _items) = window_with_tree(Source::History, folder);
        let hwnd = window.hwnd();
        let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
        let search = search(&window);
        unsafe {
            let _ = SetWindowTextW(search, w!("abc"));
            begin_edit(hwnd, ctx, EditTarget::Rename { id: folder });
            use windows::Win32::UI::Controls::TVM_ENDEDITLABELNOW as END;
            SendMessageW(tree, END, Some(WPARAM(1)), None);
        }
        assert_eq!(ctx.edit.get(), EditState::Finishing);
        unsafe {
            SendMessageW(hwnd, WM_COMMAND, Some(WPARAM(IDCANCEL.0 as usize)), Some(LPARAM(0)));
            SendMessageW(hwnd, WM_COMMAND, Some(WPARAM(IDOK.0 as usize)), Some(LPARAM(0)));
        }
        assert_eq!(window_text(search), "abc", "後始末を待つ間の Esc で検索欄を消した");
        assert!(recorder.activated.borrow().is_empty());
        pump(hwnd, 30);
        assert_eq!(ctx.edit.get(), EditState::None);
    }

    /// Esc の検索欄の消去は `IsDialogMessageW` からの IDCANCEL（lParam 0）だけ。コントロールからの通知
    /// （lParam がそのコントロール）では消さない。
    #[test]
    fn idcancel_from_control_notification_does_not_clear_search() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let hwnd = window.hwnd();
        let search = search(&window);
        unsafe {
            let _ = SetWindowTextW(search, w!("abc"));
            SendMessageW(hwnd, WM_COMMAND, Some(WPARAM(IDCANCEL.0 as usize)), Some(LPARAM(search.0 as isize)));
        }
        assert_eq!(window_text(search), "abc", "コントロールからの通知で検索欄を消した");
        unsafe {
            SendMessageW(hwnd, WM_COMMAND, Some(WPARAM(IDCANCEL.0 as usize)), Some(LPARAM(0)));
        }
        assert_eq!(window_text(search), "");
    }

    // --- 終了・起動 ---

    thread_local! {
        /// やり直しのタイマーが閉じる要求をやり直した回数（テスト用）
        pub(super) static CLOSE_RETRIES: Cell<usize> = const { Cell::new(0) };
    }

    /// 後片付け中は、送られた `WM_CLOSE`・`WM_ENDSESSION`・`WM_ACTIVATE` や起床でアプリの処理を呼ばず、
    /// `WM_CLOSE` で窓を壊さない。そのまま `ViewerWindow` を破棄しても、窓の破棄の処理は通る。
    #[test]
    fn teardown_ignores_handlers_but_lets_window_be_destroyed() {
        use windows::Win32::UI::WindowsAndMessaging::WA_ACTIVE;
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder) = create_test_window();
        let hwnd = window.hwnd();
        begin_teardown(hwnd);
        unsafe {
            SendMessageW(hwnd, WM_CLOSE, None, None);
            SendMessageW(hwnd, WM_ENDSESSION, Some(WPARAM(1)), None);
            SendMessageW(hwnd, WM_ACTIVATE, Some(WPARAM(WA_ACTIVE as usize)), None);
            let _ = PostMessageW(Some(hwnd), WM_APP_WAKE, WPARAM(0), LPARAM(0));
        }
        pump(hwnd, 30);
        assert_eq!(recorder.closes.get(), 0, "後片付け中に閉じる処理を呼んだ");
        assert_eq!(recorder.end_sessions.get(), 0, "後片付け中にセッションの終了の処理を呼んだ");
        assert_eq!(recorder.wakes.get(), 0, "後片付け中に起床の処理を呼んだ");
        assert!(unsafe { IsWindow(Some(hwnd)) }.as_bool(), "WM_CLOSE で窓が壊れた");
        let handler_refs = Rc::strong_count(&recorder);
        drop(window);
        assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool());
        assert!(Rc::strong_count(&recorder) < handler_refs, "破棄でハンドラを手放していない");
    }

    /// 終了の要求の後もメニューが開いていれば、やり直しのタイマーが閉じる要求を出し直して閉じる
    /// （ここでは、最初の閉じる要求を出さずにタイマーだけを張り、`EndMenu` が一度で閉じなかった場合に
    /// 見立てる。実際に `EndMenu` が失敗する環境で閉じることは確かめられない）。閉じたらタイマーは止まる。
    #[test]
    fn close_retry_timer_closes_menu_and_dialog_then_stops() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, _ids) = window_with_rows(&["a"], 0);
        *recorder.menu.borrow_mut() = Some(RowMenu::default());
        let hwnd = window.hwnd();

        CLOSE_RETRIES.with(|n| n.set(0));
        during_menu(move || retry_modal_close(hwnd));
        context_menu_at(&window, -1, -1);
        assert!(!finish_menu_test(), "やり直しのタイマーでメニューが閉じなかった");
        assert!(CLOSE_RETRIES.with(Cell::get) >= 1, "閉じる要求をやり直していない");
        assert!(recorder.commands.borrow().is_empty());
        pump(hwnd, CLOSE_RETRY_MS as u64 * 2 + 100);
        let after_close = CLOSE_RETRIES.with(Cell::get);
        pump(hwnd, CLOSE_RETRY_MS as u64 * 2 + 100);
        assert_eq!(CLOSE_RETRIES.with(Cell::get), after_close, "閉じた後もタイマーが動いている");

        // 確認ダイアログも同じ（キャンセルになり、履歴のクリアは伝えない）
        CLOSE_RETRIES.with(|n| n.set(0));
        during_dialog(hwnd, move || retry_modal_close(hwnd));
        send_command(hwnd, CMD_CLEAR_HISTORY);
        assert!(!finish_menu_test(), "やり直しのタイマーで確認が閉じなかった");
        assert!(CLOSE_RETRIES.with(Cell::get) >= 1);
        assert!(recorder.tools.borrow().is_empty(), "キャンセルなのに伝えた");
    }

    /// 後片付けに入ると、やり直しのタイマーは止まる（閉じる要求を出さない）。
    #[test]
    fn begin_teardown_stops_close_retry_timer() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder, _ids) = window_with_rows(&["a"], 0);
        *recorder.menu.borrow_mut() = Some(RowMenu::default());
        let hwnd = window.hwnd();
        CLOSE_RETRIES.with(|n| n.set(0));
        during_menu(move || {
            retry_modal_close(hwnd);
            begin_teardown(hwnd);
        });
        context_menu_at(&window, -1, -1);
        // タイマーが止まっているので、予備のタイマーが閉じる
        assert!(finish_menu_test(), "後片付けに入った後もタイマーでメニューを閉じた");
        assert_eq!(CLOSE_RETRIES.with(Cell::get), 0);
    }

    /// 二重起動の表示要求: 窓が遅れて見つかっても届く。投稿に失敗したら続けてやり直す。上限まで
    /// 届けられなければ false。
    #[test]
    fn show_request_waits_for_window_until_limit() {
        use std::time::Duration;
        let fake = HWND(0x1234 as *mut _);
        let tries = Cell::new(0);
        let found_later = || {
            tries.set(tries.get() + 1);
            (tries.get() >= 3).then_some(fake)
        };
        assert!(deliver_show_request(found_later, |h| h == fake, Duration::from_secs(5), Duration::from_millis(1)));
        assert_eq!(tries.get(), 3);

        let posts = Cell::new(0);
        let post_fails_once = |_h: HWND| {
            posts.set(posts.get() + 1);
            posts.get() >= 2
        };
        assert!(deliver_show_request(|| Some(fake), post_fails_once, Duration::from_secs(5), Duration::from_millis(1)));
        assert_eq!(posts.get(), 2);

        let started = std::time::Instant::now();
        assert!(!deliver_show_request(|| None, |_h| true, Duration::from_millis(50), Duration::from_millis(5)));
        assert!(started.elapsed() >= Duration::from_millis(50));
    }

    /// Ctrl+F はアクセラレータで検索欄へのコマンドになる（Ctrl を押していなければ変換しない）。
    /// ほかの窓宛てのキー操作は変換しない。Ctrl の状態は、このスレッドのキーの状態を書き換えて作る。
    #[test]
    fn ctrl_f_is_translated_only_for_viewer() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardState, SetKeyboardState, VK_CONTROL};
        use windows::Win32::UI::WindowsAndMessaging::WM_KEYDOWN;
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let (other, _other_recorder) = create_test_window();
        let hwnd = window.hwnd();
        let key = |target: HWND| MSG { hwnd: target, message: WM_KEYDOWN, wParam: WPARAM(b'F' as usize), lParam: LPARAM(0x0021_0001), ..Default::default() };
        let mut saved = [0u8; 256];
        unsafe {
            GetKeyboardState(&mut saved).unwrap();
        }
        let with_ctrl = |down: bool| {
            let mut state = saved;
            state[VK_CONTROL.0 as usize] = if down { 0x80 } else { 0 };
            unsafe { SetKeyboardState(&state).unwrap() };
        };
        with_ctrl(false);
        let plain = translate_accelerator(hwnd, &key(list(&window)));
        with_ctrl(true);
        let ctrl = translate_accelerator(hwnd, &key(list(&window)));
        let foreign = translate_accelerator(hwnd, &key(other.hwnd()));
        unsafe {
            SetKeyboardState(&saved).unwrap();
        }
        assert!(!plain, "Ctrl なしの F を変換した");
        assert!(ctrl, "Ctrl+F を変換しなかった");
        assert!(!foreign, "ほかの窓宛てのキー操作を変換した");
    }

    // --- 境目 ---

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT { left, top, right, bottom }
    }

    /// ドラッグする前は既定の比率の配置（ツリーは幅の 1/4、一覧の下端は高さの 6 割）に、4px の境目を
    /// 挟む。境目の上の点は境目、子コントロールの上の点は境目ではない。
    #[test]
    fn default_layout_keeps_r1_ratio_with_splitter_gaps() {
        let l = compute_layout(900, 700, 24, 96, None, None);
        assert_eq!(l.tree, rect(0, 0, 225, 700));
        assert_eq!(l.vertical_bar, rect(225, 0, 229, 700));
        assert_eq!(l.search, rect(229, 0, 900, 24));
        assert_eq!(l.list, rect(229, 24, 900, 420));
        assert_eq!(l.horizontal_bar, rect(229, 420, 900, 424));
        assert_eq!(l.preview, rect(229, 424, 900, 700));
        assert_eq!(splitter_at(&l, 226, 10), Some(Splitter::Vertical));
        assert_eq!(splitter_at(&l, 500, 421), Some(Splitter::Horizontal));
        assert_eq!(splitter_at(&l, 224, 10), None);
        assert_eq!(splitter_at(&l, 500, 419), None);
        // 横の境目はツリーの横まで伸びない（縦の境目が優先）
        assert_eq!(splitter_at(&l, 100, 421), None);
    }

    /// ドラッグで決めた大きさは 96 DPI 基準で、窓の DPI で換算する。最小値（ツリーの幅 80・右側 200・
    /// 一覧とプレビューの高さ 60）で制限し、窓が狭すぎるときはツリー・プレビューの最小を優先する。
    #[test]
    fn dragged_sizes_are_scaled_and_clamped() {
        let l = compute_layout(900, 700, 24, 144, Some(150), Some(180));
        assert_eq!(l.tree.right, 225);
        assert_eq!(l.preview.bottom - l.preview.top, 270);

        let l = compute_layout(900, 700, 24, 96, Some(10), Some(5));
        assert_eq!(l.tree.right, 80);
        assert_eq!(l.preview.bottom - l.preview.top, 60);
        let l = compute_layout(900, 700, 24, 96, Some(10_000), Some(10_000));
        assert_eq!(l.search.right - l.search.left, 200, "右側の最小の幅を残していない");
        assert_eq!(l.list.bottom - l.list.top, 60, "一覧の最小の高さを残していない");

        // 窓の最小（クライアント領域 400×300）でも最小値は収まる
        let l = compute_layout(400, 300, 24, 96, None, None);
        assert!(l.tree.right >= 80 && l.search.right - l.search.left >= 200);
        assert!(l.list.bottom - l.list.top >= 60 && l.preview.bottom - l.preview.top >= 60);
        // それより狭いと、ツリーとプレビューの最小を優先する（大きさが負にならない）
        let l = compute_layout(150, 100, 24, 96, None, None);
        assert_eq!(l.tree.right, 80);
        assert_eq!(l.preview.bottom - l.preview.top, 60);
        assert!(l.list.bottom >= l.list.top && l.search.right >= l.search.left);
    }

    /// 境目をマウスで押して動かすと、その境目の大きさを覚えて配置し直す。離すとドラッグを終える。
    /// 動かしていない境目は比率の配置のまま。
    #[test]
    fn dragging_splitters_moves_panes() {
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE;
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, _recorder) = create_test_window();
        let hwnd = window.hwnd();
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        let ctx = unsafe { ctx_ref(hwnd) }.unwrap();
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        let point = |x: i32, y: i32| LPARAM(((y as u16 as u32) << 16 | (x as u16 as u32)) as isize);
        let child_height = |id: i32| {
            let mut rc = RECT::default();
            unsafe {
                let child = GetDlgItem(Some(hwnd), id).unwrap();
                let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(child, &mut rc);
            }
            rc.bottom - rc.top
        };
        let drag = |from: (i32, i32), to: (i32, i32)| unsafe {
            SendMessageW(hwnd, WM_LBUTTONDOWN, Some(WPARAM(1)), Some(point(from.0, from.1)));
            SendMessageW(hwnd, WM_MOUSEMOVE, Some(WPARAM(1)), Some(point(to.0, to.1)));
            SendMessageW(hwnd, WM_LBUTTONUP, Some(WPARAM(0)), Some(point(to.0, to.1)));
        };

        // 覚える値は 96 DPI 基準の整数なので、100% 以外では境目の位置がその丸めの分だけずれる
        let quantized = |px: i32| crate::menu_tooltip::scale_for_dpi(unscale(px, dpi) as i32, dpi);
        let before = current_layout(hwnd).unwrap();
        let bar = before.vertical_bar;
        drag((bar.left + 1, 50), (bar.left + 1 + 40, 50));
        let after = current_layout(hwnd).unwrap();
        assert_eq!(after.tree.right, quantized(before.tree.right + 40));
        assert_eq!(ctx.tree_width.get(), Some(unscale(before.tree.right + 40, dpi)));
        assert_eq!(ctx.preview_height.get(), None, "動かしていない境目を覚えた");
        assert!(ctx.drag.get().is_none(), "離してもドラッグが終わっていない");

        let bar = after.horizontal_bar;
        drag((bar.left + 10, bar.top + 1), (bar.left + 10, bar.top + 1 - 30));
        let moved = current_layout(hwnd).unwrap();
        assert_eq!(moved.preview.bottom - moved.preview.top, quantized(after.preview.bottom - after.preview.top + 30));
        // 子コントロールも配置し直されている
        assert_eq!(child_height(ID_PREVIEW), moved.preview.bottom - moved.preview.top);

        // DPI が変わったらドラッグを終える（押した位置のずれが古い DPI の px のため）
        let bar = moved.vertical_bar;
        let mut suggested = RECT::default();
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut suggested);
            SendMessageW(hwnd, WM_LBUTTONDOWN, Some(WPARAM(1)), Some(point(bar.left + 1, 50)));
            assert!(ctx.drag.get().is_some());
            let dpi_param = WPARAM((dpi as usize) << 16 | dpi as usize);
            SendMessageW(hwnd, WM_DPICHANGED, Some(dpi_param), Some(LPARAM(&suggested as *const _ as isize)));
        }
        assert!(ctx.drag.get().is_none(), "DPI の変更でドラッグが終わっていない");
    }

    /// クリップボードのクリア・最前面の切り替えはそのまま伝え、Ctrl+F（CMD_FIND）は伝えない。
    #[test]
    fn tool_commands_are_reported() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (window, recorder) = create_test_window();
        send_command(window.hwnd(), CMD_CLEAR_CLIPBOARD);
        send_command(window.hwnd(), CMD_TOPMOST);
        send_command(window.hwnd(), CMD_FIND);
        send_command(window.hwnd(), CMD_CHECK_DATA);
        assert_eq!(
            recorder.tools.borrow().as_slice(),
            [ToolCommand::ClearClipboard, ToolCommand::ToggleTopmost, ToolCommand::CheckData]
        );
        assert!(!unsafe { ctx_ref(window.hwnd()) }.unwrap().accel.is_invalid());
    }
}
