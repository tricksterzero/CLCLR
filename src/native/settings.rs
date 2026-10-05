//! 設定画面。設定を反映するときに「何をその場で反映するか」の判定（`effects`）と、設定画面の
//! 窓（DIALOGEX のリソース `res/settings.rc` とタブ、モードレス）。反映そのものは `App::apply_settings`。
//!
//! 窓の寿命: `App` が
//! `SettingsWindow` を持つ。閉じる（OK の成功・キャンセル・×・Esc）ときはその場で破棄せず、自分へ
//! `WM_APP_SETTINGS_CLOSE` を投稿し、その処理で `on_closed`（`App` が `SettingsWindow` を捨てる）を呼ぶ。OK の
//! 処理（中で `apply_settings` がメッセージを処理しながら待ち、ハンドラが再入しうる）の間は閉じる要求を記録する
//! だけにし、戻ってから1回だけ閉じる。終了の要求の後（`end`）は隠すだけで、`main` がメッセージループを抜けた後に
//! 破棄する。破棄の入口はこの2つ（とビューアの破棄に伴う持たれた窓の破棄）だけで、どれも OK の処理の途中では
//! 起きない。設定画面自身はモーダル（メッセージボックスなど）を出さない（誤りは窓の中の文字）。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::MapWindowPoints;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_LISTVIEW_CLASSES, ICC_TAB_CLASSES, ICC_UPDOWN_CLASS, INITCOMMONCONTROLSEX,
    LIST_VIEW_ITEM_STATE_FLAGS, LVCFMT_LEFT, LVCF_FMT, LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVIF_STATE, LVIF_TEXT,
    LVIS_FOCUSED, LVIS_SELECTED, LVITEMW, LVM_DELETEITEM, LVM_ENSUREVISIBLE, LVM_GETNEXTITEM, LVM_INSERTCOLUMNW,
    LVM_INSERTITEMW, LVM_SETCOLUMNWIDTH, LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMSTATE, LVM_SETITEMTEXTW,
    LVNI_SELECTED, LVN_ITEMCHANGED, LVS_EX_FULLROWSELECT, NMHDR, NMLISTVIEW, TCIF_TEXT, TCITEMW, TCM_ADJUSTRECT,
    TCM_GETCURSEL, TCM_INSERTITEMW, TCM_SETCURSEL, TCN_SELCHANGE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, GetKeyState, SetFocus, VK_CONTROL, VK_SHIFT, VK_TAB};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateDialogParamW, DestroyWindow, GetClientRect, GetDlgItem, GetParent, GetWindowLongPtrW, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, IsChild, IsDialogMessageW, IsWindow, PostMessageW, SendMessageW,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow, BM_GETCHECK, BM_SETCHECK,
    BN_CLICKED, CBN_SELCHANGE, CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL, EN_CHANGE, MSG, SWP_NOACTIVATE, SW_HIDE,
    SW_SHOW, WINDOW_LONG_PTR_INDEX, WM_APP, WM_CLOSE, WM_COMMAND, WM_DPICHANGED, WM_INITDIALOG, WM_KEYDOWN,
    WM_NCDESTROY, WM_NOTIFY,
};

use crate::config::{
    Config, ConfigIssue, DoublePressAction, FilterAction, FormatFilter, OverlapCheck, WindowFilter,
};
use crate::native::app::ApplyError;

include!(concat!(env!("OUT_DIR"), "/resource_ids.rs"));

/// 設定を反映するときに、その場で行うこと（`effects`）。ここに無い設定（ツールチップ・メニューの件数・
/// 音・フィルタ・追加の間隔・操作の失敗の知らせ方など）は、使う時点で共有の設定を読むので、反映の処理は
/// 要らない。`start_hidden`・`check_folder_permissions` は起動時だけ、保存の有無（完全メモリモードとの切り替え）は
/// 次の起動から効く。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Effects {
    /// ホットキーの登録・キーフックを設定に合わせ直す（`popup_menu`・`double_press_*` が変わった）
    pub hotkeys: bool,
    /// 保持件数へ切り詰める（`history.effective_max()` が変わった。減ったときだけ実際に消える）
    pub trim: bool,
    /// ビューアのツリー・一覧を作り直す（階層表示の設定が変わった）
    pub rebuild: bool,
}

/// 設定が `before` から `after` に変わったときに、その場で行うこと。監視（`clipboard_watch`）とトレイ
/// （`show_trayicon`）は、設定の値の差ではなく実際の状態との差で決めるので、ここでは扱わない
/// （`App::apply_settings`）。
pub fn effects(before: &Config, after: &Config) -> Effects {
    let (b, a) = (&before.hotkey, &after.hotkey);
    Effects {
        hotkeys: b.popup_menu != a.popup_menu
            || b.double_press_ctrl != a.double_press_ctrl
            || b.double_press_shift != a.double_press_shift
            || b.double_press_alt != a.double_press_alt,
        trim: before.history.effective_max() != after.history.effective_max(),
        rebuild: before.history.grouping != after.history.grouping,
    }
}

// --- 設定画面の窓 ---

/// OK で呼ぶ反映（開いたときの設定・画面で編集した設定。`App::apply_settings` を包む）。
pub type OnOk = Rc<dyn Fn(&Config, &Config) -> Result<(), ApplyError>>;
/// 閉じたとき（`App` が `SettingsWindow` を捨てる）。
pub type OnClosed = Rc<dyn Fn()>;

/// 自分へ投稿する閉じる要求（その処理で `on_closed` を呼ぶ）。
const WM_APP_SETTINGS_CLOSE: u32 = WM_APP + 40;
/// DPI が変わった後（既定の処理がダイアログを伸縮し終えてから）ページを置き直す。
const WM_APP_PLACE_PAGES: u32 = WM_APP + 41;
/// `UDM_SETRANGE32`（上下のボタンの範囲）。
const UDM_SETRANGE32: u32 = 0x0400 + 111;

/// ダイアログの `DWLP_USER`（`DWLP_MSGRESULT`・`DWLP_DLGPROC` の後。どちらもポインタの大きさ）。
const DWLP_USER: WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX((2 * std::mem::size_of::<isize>()) as i32);

/// タブ（ページ）の並び。
const PAGES: [(i32, &str); 6] = [
    (IDD_PAGE_GENERAL, "全般"),
    (IDD_PAGE_HISTORY, "履歴"),
    (IDD_PAGE_HOTKEY, "ホットキー"),
    (IDD_PAGE_TEXT, "テキスト変換"),
    (IDD_PAGE_FORMAT, "形式フィルタ"),
    (IDD_PAGE_WINDOW, "ウィンドウフィルタ"),
];
const PAGE_GENERAL: usize = 0;
const PAGE_HISTORY: usize = 1;
const PAGE_HOTKEY: usize = 2;
const PAGE_TEXT: usize = 3;
const PAGE_FORMAT: usize = 4;
const PAGE_WINDOW: usize = 5;

/// 数値の入力欄（範囲は `Config::validate` と同じ値）。
struct NumField {
    field: &'static str,
    label: &'static str,
    page: usize,
    ctl: i32,
    spin: i32,
    min: u32,
    max: u32,
}

const NUMS: [NumField; 10] = [
    NumField { field: "history.max", label: "最大件数", page: PAGE_HISTORY, ctl: IDC_HIS_MAX, spin: IDC_HIS_MAX_SPIN, min: 0, max: 10_000 },
    NumField { field: "history.grouping.visible_items", label: "直下に表示する件数", page: PAGE_HISTORY, ctl: IDC_HIS_VISIBLE, spin: IDC_HIS_VISIBLE_SPIN, min: 1, max: 1_000 },
    NumField { field: "history.grouping.folders", label: "フォルダ数", page: PAGE_HISTORY, ctl: IDC_HIS_FOLDERS, spin: IDC_HIS_FOLDERS_SPIN, min: 1, max: 100 },
    NumField { field: "history.grouping.items_per_folder", label: "フォルダ内の件数", page: PAGE_HISTORY, ctl: IDC_HIS_PER_FOLDER, spin: IDC_HIS_PER_FOLDER_SPIN, min: 1, max: 1_000 },
    NumField { field: "history.add_interval_ms", label: "追加までの遅延", page: PAGE_HISTORY, ctl: IDC_HIS_INTERVAL, spin: IDC_HIS_INTERVAL_SPIN, min: 1, max: 10_000 },
    NumField { field: "hotkey.menu_max_items", label: "メニューに出す履歴件数", page: PAGE_HOTKEY, ctl: IDC_HK_MENU_ITEMS, spin: IDC_HK_MENU_ITEMS_SPIN, min: 1, max: 100 },
    NumField { field: "hotkey.tooltip.delay_ms", label: "表示までの待ち時間", page: PAGE_HOTKEY, ctl: IDC_HK_TIP_DELAY, spin: IDC_HK_TIP_DELAY_SPIN, min: 0, max: 5_000 },
    NumField { field: "hotkey.tooltip.max_chars", label: "最大文字数", page: PAGE_HOTKEY, ctl: IDC_HK_TIP_CHARS, spin: IDC_HK_TIP_CHARS_SPIN, min: 1, max: 100_000 },
    NumField { field: "hotkey.tooltip.max_lines", label: "最大行数", page: PAGE_HOTKEY, ctl: IDC_HK_TIP_LINES, spin: IDC_HK_TIP_LINES_SPIN, min: 1, max: 200 },
    NumField { field: "tools.text.word_break_width", label: "折り返し幅", page: PAGE_TEXT, ctl: IDC_TX_WRAP, spin: IDC_TX_WRAP_SPIN, min: 1, max: 1_000 },
];

/// 誤りの項目（`ConfigIssue::field`）→ ページ・入力欄・その欄を有効にするチェックボックス（無効な間は、そちらへ
/// フォーカスを移す）。`Config::validate` と数値の読み取りの誤りの `field` はすべてここにある（テストで確かめる）。
struct FieldPlace {
    field: &'static str,
    page: usize,
    ctl: i32,
    enabler: Option<(i32, &'static str)>,
}

const PLACES: [FieldPlace; 16] = [
    // フィルタの行の誤り（`ConfigIssue::row` の行を選んでから、その欄へ）
    FieldPlace { field: "format_filters.format_name", page: PAGE_FORMAT, ctl: IDC_FMT_NAME, enabler: None },
    FieldPlace { field: "format_filters.limit_size", page: PAGE_FORMAT, ctl: IDC_FMT_LIMIT, enabler: None },
    FieldPlace { field: "capture_total_limit", page: PAGE_FORMAT, ctl: IDC_FMT_TOTAL, enabler: None },
    FieldPlace { field: "window_filters.title", page: PAGE_WINDOW, ctl: IDC_WIN_TITLE, enabler: None },
    FieldPlace { field: "history.max", page: PAGE_HISTORY, ctl: IDC_HIS_MAX, enabler: None },
    FieldPlace { field: "history.grouping.visible_items", page: PAGE_HISTORY, ctl: IDC_HIS_VISIBLE, enabler: Some((IDC_HIS_GROUP, "古い履歴をフォルダにまとめる")) },
    FieldPlace { field: "history.grouping.folders", page: PAGE_HISTORY, ctl: IDC_HIS_FOLDERS, enabler: Some((IDC_HIS_GROUP, "古い履歴をフォルダにまとめる")) },
    FieldPlace { field: "history.grouping.items_per_folder", page: PAGE_HISTORY, ctl: IDC_HIS_PER_FOLDER, enabler: Some((IDC_HIS_GROUP, "古い履歴をフォルダにまとめる")) },
    FieldPlace { field: "history.add_interval_ms", page: PAGE_HISTORY, ctl: IDC_HIS_INTERVAL, enabler: None },
    FieldPlace { field: "history.sound_file", page: PAGE_HISTORY, ctl: IDC_HIS_SOUND_FILE, enabler: Some((IDC_HIS_SOUND, "履歴に追加された時に音を鳴らす")) },
    FieldPlace { field: "hotkey.menu_max_items", page: PAGE_HOTKEY, ctl: IDC_HK_MENU_ITEMS, enabler: None },
    FieldPlace { field: "hotkey.tooltip.delay_ms", page: PAGE_HOTKEY, ctl: IDC_HK_TIP_DELAY, enabler: Some((IDC_HK_TOOLTIP, "メニュー項目にツールチップを表示する")) },
    FieldPlace { field: "hotkey.tooltip.max_chars", page: PAGE_HOTKEY, ctl: IDC_HK_TIP_CHARS, enabler: Some((IDC_HK_TOOLTIP, "メニュー項目にツールチップを表示する")) },
    FieldPlace { field: "hotkey.tooltip.max_lines", page: PAGE_HOTKEY, ctl: IDC_HK_TIP_LINES, enabler: Some((IDC_HK_TOOLTIP, "メニュー項目にツールチップを表示する")) },
    FieldPlace { field: "tools.text.word_break_width", page: PAGE_TEXT, ctl: IDC_TX_WRAP, enabler: None },
    FieldPlace { field: "hotkey.popup_menu.key", page: PAGE_HOTKEY, ctl: IDC_HK_KEY, enabler: Some((IDC_HK_POPUP, "ポップアップメニューのホットキー")) },
];

/// チェックボックスで入力できる・できないを切り替える欄（機能が無効な間は隠さずに灰色）。
const ENABLERS: [(usize, i32, &[i32]); 5] = [
    (PAGE_HISTORY, IDC_HIS_GROUP, &[
        IDC_HIS_VISIBLE, IDC_HIS_VISIBLE_SPIN, IDC_HIS_FOLDERS, IDC_HIS_FOLDERS_SPIN, IDC_HIS_PER_FOLDER,
        IDC_HIS_PER_FOLDER_SPIN, IDC_HIS_FORMAT,
    ]),
    (PAGE_HISTORY, IDC_HIS_SOUND, &[IDC_HIS_SOUND_FILE]),
    (PAGE_HOTKEY, IDC_HK_POPUP, &[IDC_HK_CTRL, IDC_HK_SHIFT, IDC_HK_ALT, IDC_HK_WIN, IDC_HK_KEY]),
    (PAGE_HOTKEY, IDC_HK_TOOLTIP, &[
        IDC_HK_TIP_DELAY, IDC_HK_TIP_DELAY_SPIN, IDC_HK_TIP_CHARS, IDC_HK_TIP_CHARS_SPIN, IDC_HK_TIP_LINES,
        IDC_HK_TIP_LINES_SPIN,
    ]),
    (PAGE_TEXT, IDC_TX_DATE, &[IDC_TX_DATE_FMT, IDC_TX_TIME_FMT]),
];

const OVERLAPS: [(OverlapCheck, &str); 3] =
    [(OverlapCheck::None, "しない"), (OverlapCheck::Last, "直近1件と比較"), (OverlapCheck::All, "全履歴と比較")];
const DOUBLE_PRESSES: [(DoublePressAction, &str); 5] = [
    (DoublePressAction::None, "なし"),
    (DoublePressAction::Menu, "メニュー表示"),
    (DoublePressAction::MenuPinned, "メニュー表示（ピン留めのみ）"),
    (DoublePressAction::MenuHistory, "メニュー表示（履歴のみ）"),
    (DoublePressAction::Viewer, "ビューア表示"),
];
/// ホットキーの修飾キー（設定の文字と、チェックボックス）。読み戻すときはこの順。
const MODIFIERS: [(&str, i32); 4] = [("ctrl", IDC_HK_CTRL), ("shift", IDC_HK_SHIFT), ("alt", IDC_HK_ALT), ("win", IDC_HK_WIN)];
const ACTIONS: [(FilterAction, &str); 2] = [(FilterAction::Add, "取り込む"), (FilterAction::Ignore, "無視")];

// --- フィルタの行 ---
//
// 行の値の正は `Ctx` の Vec で、一覧（ListView）は表示だけ。開いたときに `base` から作り、利用者が触ったときだけ
// 書き換える（一覧の文字から値を戻すと、触っていない行が「変えた」に見え、`merge_edit` で今の値を上書きしうる）。
// Vec の借用は、同期の知らせ（`WM_NOTIFY`・`WM_COMMAND`）を起こしうる UI の呼び出しの前に必ず手放す。

/// 形式フィルタの1行。上限は入力のままの文字で持つ（選び替えても読めない入力を失わず、OK のときに行ごとに読む）。
#[derive(Clone, Debug, PartialEq)]
struct FormatRow {
    name: String,
    action: FilterAction,
    save: bool,
    limit_text: String,
}

impl FormatRow {
    fn from_filter(f: &FormatFilter) -> Self {
        Self { name: f.format_name.clone(), action: f.action, save: f.save, limit_text: f.limit_size.to_string() }
    }

    /// 追加する行の既定（空の名前・取り込む・保存・上限 0）。
    fn new() -> Self {
        Self { name: String::new(), action: FilterAction::Add, save: true, limit_text: "0".to_string() }
    }

    fn cells(&self) -> Vec<String> {
        let action = ACTIONS.iter().find(|(a, _)| *a == self.action).map_or("", |(_, l)| l);
        vec![self.name.clone(), action.to_string(), yes_no(self.save), self.limit_text.clone()]
    }
}

fn window_cells(w: &WindowFilter) -> Vec<String> {
    vec![w.title.clone(), w.class_name.clone(), yes_no(w.ignore)]
}

fn yes_no(on: bool) -> String {
    if on { "する" } else { "しない" }.to_string()
}

/// 上限の欄の文字を読む（空白は除く。空・数字以外・桁あふれは None）。
fn parse_limit(text: &str) -> Option<u64> {
    let t: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    t.parse().ok()
}

/// 上限の欄の横に出す読みやすい大きさ。読めない入力は空。
fn readable_size(text: &str) -> String {
    match parse_limit(text) {
        None => String::new(),
        Some(0) => "（無制限）".to_string(),
        Some(n) => format!("= {}", readable_bytes(n)),
    }
}

/// バイト数の読みやすい表し方（1024 未満は「n バイト」、ほかは 1024 単位で小数1桁の KB・MB・GB・TB）。
pub(crate) fn readable_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} バイト");
    }
    let mut v = n as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

/// 行を編集する一覧（形式フィルタ・ウィンドウフィルタのページ）。
struct RowList {
    page: usize,
    list: i32,
    add: i32,
    delete: i32,
    /// 選んだ行の入力欄（行を選んでいない間は灰色）。先頭は追加の後にフォーカスする欄
    fields: &'static [i32],
    /// 列の見出しと幅の比（合計 100。幅は一覧のクライアント幅から決める）
    columns: &'static [(&'static str, i32)],
}

const ROW_LISTS: [RowList; 2] = [
    RowList {
        page: PAGE_FORMAT,
        list: IDC_FMT_LIST,
        add: IDC_FMT_ADD,
        delete: IDC_FMT_DELETE,
        fields: &[IDC_FMT_NAME, IDC_FMT_ACTION, IDC_FMT_SAVE, IDC_FMT_LIMIT],
        columns: &[("形式名", 40), ("動作", 20), ("保存", 15), ("上限（バイト）", 25)],
    },
    RowList {
        page: PAGE_WINDOW,
        list: IDC_WIN_LIST,
        add: IDC_WIN_ADD,
        delete: IDC_WIN_DELETE,
        fields: &[IDC_WIN_TITLE, IDC_WIN_CLASS, IDC_WIN_IGNORE],
        columns: &[("タイトル（部分一致）", 45), ("クラス名（完全一致）", 40), ("除外", 15)],
    },
];

fn row_list(page: usize) -> Option<&'static RowList> {
    ROW_LISTS.iter().find(|r| r.page == page)
}

thread_local! {
    /// 今の設定画面の窓（メッセージループが `IsDialogMessageW` に通す。無ければ 0）。`WM_NCDESTROY` で消す
    static ACTIVE: Cell<isize> = const { Cell::new(0) };
    /// `WM_INITDIALOG` で窓に結び付けたコンテキスト（作成に失敗したときに、解放の担い手を見分ける）
    static ATTACHED: Cell<isize> = const { Cell::new(0) };
    /// テストだけ: 閉じる要求の投稿を失敗させる
    #[cfg(test)]
    static FAIL_CLOSE_POST: Cell<bool> = const { Cell::new(false) };
}

/// 設定画面の窓のコンテキスト（`DWLP_USER`。`WM_NCDESTROY` で解放）。
struct Ctx {
    base: Config,
    pages: Vec<HWND>,
    on_ok: OnOk,
    on_closed: OnClosed,
    /// OK の処理中（`on_ok` の中でメッセージを処理しながら待つことがある）
    ok_running: Cell<bool>,
    /// OK の処理中に届いた閉じる要求（OK が戻ってから閉じる）
    close_pending: Cell<bool>,
    /// 閉じる要求を投稿した（二重に投稿しない）
    closing: Cell<bool>,
    /// 終了の要求を受けた（`SettingsWindow::end`）。以後、表示・入力・閉じる要求の投稿をしない
    ended: Cell<bool>,
    /// 形式フィルタ・ウィンドウフィルタの行（画面の中の正）
    formats: RefCell<Vec<FormatRow>>,
    windows: RefCell<Vec<WindowFilter>>,
    /// 選んだ行の値を入力欄へ入れている間（入力欄の変化を行へ書き戻さない。無いと前の行へ書き込まれる）
    syncing: Cell<bool>,
    /// 行の追加・削除・誤りの行の選択の間（選択の知らせを処理しない。終わった後に入力欄を1回だけ埋める）
    suppress: Cell<bool>,
}

impl Ctx {
    fn row_count(&self, page: usize) -> usize {
        match page {
            PAGE_FORMAT => self.formats.borrow().len(),
            PAGE_WINDOW => self.windows.borrow().len(),
            _ => 0,
        }
    }

    fn row_cells(&self, page: usize, index: usize) -> Option<Vec<String>> {
        match page {
            PAGE_FORMAT => self.formats.borrow().get(index).map(FormatRow::cells),
            PAGE_WINDOW => self.windows.borrow().get(index).map(window_cells),
            _ => None,
        }
    }
}

/// 窓の `Ctx` を共有参照で取り出す（まだ・もう無ければ None）。
unsafe fn ctx_ref<'a>(hwnd: HWND) -> Option<&'a Ctx> {
    unsafe { (GetWindowLongPtrW(hwnd, DWLP_USER) as *const Ctx).as_ref() }
}

/// 設定画面の窓。`Drop` で窓を破棄する（まだあれば）。
pub struct SettingsWindow {
    hwnd: HWND,
}

impl SettingsWindow {
    /// 設定画面を作って表示する。`owner` はビューアの窓（持ち主。ビューアより前に出る）。`base` は開いたときの設定で、
    /// 画面へ入れ、OK で `on_ok(&base, &draft)` を呼ぶ。
    pub fn create(owner: Option<HWND>, base: Config, on_ok: OnOk, on_closed: OnClosed) -> windows::core::Result<Self> {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_TAB_CLASSES | ICC_UPDOWN_CLASS | ICC_LISTVIEW_CLASSES,
        };
        unsafe {
            let _ = InitCommonControlsEx(&icc);
        }
        let formats = base.format_filters.iter().map(FormatRow::from_filter).collect();
        let windows = base.window_filters.clone();
        let ctx = Box::new(Ctx {
            base,
            pages: Vec::new(),
            on_ok,
            on_closed,
            ok_running: Cell::new(false),
            close_pending: Cell::new(false),
            closing: Cell::new(false),
            ended: Cell::new(false),
            formats: RefCell::new(formats),
            windows: RefCell::new(windows),
            syncing: Cell::new(false),
            suppress: Cell::new(false),
        });
        let raw = Box::into_raw(ctx);
        ATTACHED.with(|a| a.set(0));
        let created = unsafe {
            CreateDialogParamW(
                Some(GetModuleHandleW(None)?.into()),
                PCWSTR(IDD_SETTINGS as usize as *const u16),
                owner,
                Some(settings_proc),
                LPARAM(raw as isize),
            )
        };
        let hwnd = match created {
            Ok(hwnd) => hwnd,
            Err(e) => {
                // WM_INITDIALOG に届く前に作れなかったなら、コンテキストは窓に結び付いていないのでここで解放する
                // （届いていれば、窓の破棄の WM_NCDESTROY が解放する）
                if ATTACHED.with(Cell::get) != raw as isize {
                    drop(unsafe { Box::from_raw(raw) });
                }
                return Err(e);
            }
        };
        let window = Self { hwnd };
        if unsafe { ctx_ref(hwnd) }.is_none_or(|ctx| ctx.pages.len() != PAGES.len()) {
            // ページを作れなかった（窓は `window` の破棄で壊れ、コンテキストは WM_NCDESTROY で解放される）
            return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL));
        }
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        Ok(window)
    }

    #[cfg(test)]
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// テスト用: ページの窓（`PAGES` の順）。
    #[cfg(test)]
    pub fn pages(&self) -> Vec<HWND> {
        unsafe { ctx_ref(self.hwnd) }.map(|ctx| ctx.pages.clone()).unwrap_or_default()
    }

    /// 前へ出す（すでに開いているのに、もう一度開こうとしたとき）。
    pub fn bring_to_front(&self) {
        if unsafe { ctx_ref(self.hwnd) }.is_some_and(|ctx| ctx.ended.get()) {
            return;
        }
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOW);
            let _ = SetForegroundWindow(self.hwnd);
        }
    }

    /// ビューアを隠したとき（編集中の内容はそのまま）。
    pub fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    /// ビューアを表示したとき（終了の要求の後は出さない）。
    pub fn show(&self) {
        if unsafe { ctx_ref(self.hwnd) }.is_some_and(|ctx| !ctx.ended.get()) {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_SHOW);
            }
        }
    }

    /// 終了の要求を受けた: 隠し、以後は表示・入力・閉じる要求の投稿をしない（破棄は `main` がループの後に行う）。
    pub fn end(&self) {
        if let Some(ctx) = unsafe { ctx_ref(self.hwnd) } {
            ctx.ended.set(true);
        }
        self.hide();
    }
}

impl Drop for SettingsWindow {
    fn drop(&mut self) {
        unsafe {
            if IsWindow(Some(self.hwnd)).as_bool() {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

/// 今の設定画面の窓（無ければ None）。
pub fn active_dialog() -> Option<HWND> {
    let raw = ACTIVE.with(Cell::get);
    (raw != 0).then(|| HWND(raw as *mut _))
}

/// `msg` が設定画面かその子孫宛てなら、その設定画面。
fn target_dialog(msg: &MSG) -> Option<HWND> {
    let dialog = active_dialog()?;
    (msg.hwnd == dialog || unsafe { IsChild(dialog, msg.hwnd) }.as_bool()).then_some(dialog)
}

/// メッセージループの前処理: 設定画面の中の Ctrl+Tab・Ctrl+Shift+Tab でタブを切り替える（処理したら true）。
pub fn pre_translate(msg: &MSG) -> bool {
    if msg.message != WM_KEYDOWN || msg.wParam.0 != VK_TAB.0 as usize {
        return false;
    }
    let Some(dialog) = target_dialog(msg) else {
        return false;
    };
    if unsafe { GetKeyState(VK_CONTROL.0 as i32) } >= 0 {
        return false;
    }
    let backward = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
    let Ok(tab) = (unsafe { GetDlgItem(Some(dialog), IDC_SET_TAB) }) else {
        return false;
    };
    let count = PAGES.len() as isize;
    let current = unsafe { SendMessageW(tab, TCM_GETCURSEL, None, None) }.0;
    let next = (current + if backward { count - 1 } else { 1 }).rem_euclid(count) as usize;
    select_page(dialog, next);
    unsafe {
        let _ = SetFocus(Some(tab));
    }
    true
}

/// メッセージループ: 設定画面かその子孫宛てのメッセージを `IsDialogMessageW` に通す（Tab・アクセスキー・Enter・Esc）。
/// 処理したら true（呼び出し側は `TranslateMessage` に通さない）。
pub fn is_dialog_message(msg: &MSG) -> bool {
    target_dialog(msg).is_some_and(|dialog| unsafe { IsDialogMessageW(dialog, msg) }.as_bool())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn item(page: HWND, id: i32) -> Option<HWND> {
    unsafe { GetDlgItem(Some(page), id) }.ok()
}

fn set_text(page: HWND, id: i32, text: &str) {
    if let Some(ctl) = item(page, id) {
        let w = wide(text);
        unsafe {
            let _ = SetWindowTextW(ctl, PCWSTR(w.as_ptr()));
        }
    }
}

fn get_text(page: HWND, id: i32) -> String {
    let Some(ctl) = item(page, id) else {
        return String::new();
    };
    let len = unsafe { GetWindowTextLengthW(ctl) } as usize;
    let mut buf = vec![0u16; len + 1];
    let n = unsafe { GetWindowTextW(ctl, &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..n])
}

/// 入力欄を入力できる・できないにする。すぐ前の見出し（アクセスキーを持つ静的文字。`settings.rc` では見出しを
/// 入力欄のすぐ前に置く）も合わせる: 灰色の欄を指すアクセスキーを押すと、ダイアログはその先の入力できる項目を探し、
/// 親の OK まで進んで押してしまう（ユーザーの実機の報告とテストで確認）。灰色の見出しはアクセスキーで選ばれない。
fn enable_field(page: HWND, id: i32, on: bool) {
    use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetWindow, GW_HWNDPREV};
    let Some(ctl) = item(page, id) else {
        return;
    };
    unsafe {
        let _ = EnableWindow(ctl, on);
    }
    let Ok(prev) = (unsafe { GetWindow(ctl, GW_HWNDPREV) }) else {
        return;
    };
    let mut class = [0u16; 16];
    let n = unsafe { GetClassNameW(prev, &mut class) } as usize;
    let len = unsafe { GetWindowTextLengthW(prev) } as usize;
    let mut buf = vec![0u16; len + 1];
    let m = unsafe { GetWindowTextW(prev, &mut buf) } as usize;
    if String::from_utf16_lossy(&class[..n]).eq_ignore_ascii_case("Static") && String::from_utf16_lossy(&buf[..m]).contains("(&") {
        unsafe {
            let _ = EnableWindow(prev, on);
        }
    }
}

fn set_check(page: HWND, id: i32, on: bool) {
    if let Some(ctl) = item(page, id) {
        unsafe {
            SendMessageW(ctl, BM_SETCHECK, Some(WPARAM(usize::from(on))), None);
        }
    }
}

fn get_check(page: HWND, id: i32) -> bool {
    item(page, id).is_some_and(|ctl| unsafe { SendMessageW(ctl, BM_GETCHECK, None, None) }.0 == 1)
}

fn fill_combo(page: HWND, id: i32, labels: &[&str], selected: usize) {
    if let Some(ctl) = item(page, id) {
        for label in labels {
            let w = wide(label);
            unsafe {
                SendMessageW(ctl, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
            }
        }
        unsafe {
            SendMessageW(ctl, CB_SETCURSEL, Some(WPARAM(selected)), None);
        }
    }
}

fn combo_index(page: HWND, id: i32) -> usize {
    item(page, id).map_or(0, |ctl| unsafe { SendMessageW(ctl, CB_GETCURSEL, None, None) }.0.max(0) as usize)
}

/// 設定を画面へ入れる（開いたとき）。上下のボタンの範囲も入れる。フィルタの一覧はコンテキストの行から作り、
/// どの行も選ばない。
fn load(ctx: &Ctx) {
    let (pages, c) = (&ctx.pages[..], &ctx.base);
    let [general, history, hotkey, text] = [pages[PAGE_GENERAL], pages[PAGE_HISTORY], pages[PAGE_HOTKEY], pages[PAGE_TEXT]];
    set_check(general, IDC_GEN_WATCH, c.general.clipboard_watch);
    set_check(general, IDC_GEN_TRAY, c.general.show_trayicon);
    set_check(general, IDC_GEN_START_HIDDEN, c.general.start_hidden);
    set_check(general, IDC_GEN_SYNC, c.general.startup_clipboard_sync);
    set_check(general, IDC_GEN_NOTIFY, c.general.notify_action_errors);
    set_check(general, IDC_GEN_FOLDER_CHECK, c.general.check_folder_permissions);

    let h = &c.history;
    let values: [u64; 10] = [
        u64::from(h.max),
        u64::from(h.grouping.visible_items),
        u64::from(h.grouping.folders),
        u64::from(h.grouping.items_per_folder),
        h.add_interval_ms,
        u64::from(c.hotkey.menu_max_items),
        u64::from(c.hotkey.tooltip.delay_ms),
        u64::from(c.hotkey.tooltip.max_chars),
        u64::from(c.hotkey.tooltip.max_lines),
        u64::from(c.tools.text.word_break_width),
    ];
    for (num, value) in NUMS.iter().zip(values) {
        if let Some(spin) = item(pages[num.page], num.spin) {
            unsafe {
                SendMessageW(spin, UDM_SETRANGE32, Some(WPARAM(num.min as usize)), Some(LPARAM(num.max as isize)));
            }
        }
        // 範囲外の値（手で書き換えた設定ファイル）もそのまま出す（OK で誤りとして知らせる）
        set_text(pages[num.page], num.ctl, &value.to_string());
    }
    set_check(history, IDC_HIS_GROUP, h.grouping.enabled);
    set_text(history, IDC_HIS_FORMAT, &h.grouping.folder_name_format);
    let overlap = OVERLAPS.iter().position(|(v, _)| *v == h.overlap_check).unwrap_or(0);
    fill_combo(history, IDC_HIS_OVERLAP, &OVERLAPS.map(|(_, l)| l), overlap);
    set_check(history, IDC_HIS_SAVE_EXIT, h.save_on_exit);
    set_check(history, IDC_HIS_SAVE_CHANGE, h.save_on_change);
    set_check(history, IDC_HIS_DELETE_ON_SEND, h.delete_on_send);
    set_check(history, IDC_HIS_SOUND, h.sound_on_add);
    set_text(history, IDC_HIS_SOUND_FILE, &h.sound_file);

    let k = &c.hotkey;
    set_check(hotkey, IDC_HK_POPUP, k.popup_menu.enabled);
    for (name, id) in MODIFIERS {
        set_check(hotkey, id, k.popup_menu.modifiers.iter().any(|m| m.eq_ignore_ascii_case(name)));
    }
    set_text(hotkey, IDC_HK_KEY, &k.popup_menu.key);
    set_check(hotkey, IDC_HK_AUTOPASTE, k.auto_paste);
    set_check(hotkey, IDC_HK_PINNED_FIRST, k.menu_pinned_first);
    set_check(hotkey, IDC_HK_TOOLTIP, k.tooltip.enabled);
    for (id, value) in [(IDC_HK_DP_CTRL, k.double_press_ctrl), (IDC_HK_DP_SHIFT, k.double_press_shift), (IDC_HK_DP_ALT, k.double_press_alt)] {
        let index = DOUBLE_PRESSES.iter().position(|(v, _)| *v == value).unwrap_or(0);
        fill_combo(hotkey, id, &DOUBLE_PRESSES.map(|(_, l)| l), index);
    }

    let t = &c.tools.text;
    set_text(text, IDC_TX_QUOTE, &t.quote_char);
    set_text(text, IDC_TX_OPEN, &t.put_text_open);
    set_text(text, IDC_TX_CLOSE, &t.put_text_close);
    set_check(text, IDC_TX_TRIM, t.delete_crlf_trim_leading);
    set_check(text, IDC_TX_DATE, t.convert_date_on_send);
    set_text(text, IDC_TX_DATE_FMT, &t.date_format);
    set_text(text, IDC_TX_TIME_FMT, &t.time_format);

    let default = ACTIONS.iter().position(|(a, _)| *a == c.format_filter_default).unwrap_or(0);
    fill_combo(pages[PAGE_FORMAT], IDC_FMT_DEFAULT, &ACTIONS.map(|(_, l)| l), default);
    fill_combo(pages[PAGE_FORMAT], IDC_FMT_ACTION, &ACTIONS.map(|(_, l)| l), 0);
    // 読みやすい大きさは、下の `sync_page` が出す
    set_text(pages[PAGE_FORMAT], IDC_FMT_TOTAL, &c.capture_total_limit.to_string());
    for rl in &ROW_LISTS {
        let Some(list) = item(pages[rl.page], rl.list) else {
            continue;
        };
        init_list(list, rl);
        for i in 0..ctx.row_count(rl.page) {
            if let Some(cells) = ctx.row_cells(rl.page, i) {
                insert_row(list, i, &cells);
            }
        }
        fill_editor(ctx, rl);
    }

    for (index, &page) in pages.iter().enumerate() {
        sync_page(page, index);
    }
}

// --- フィルタの一覧の操作 ---

/// 一覧の列を作る（開いたとき）。行全体を選べるようにする。
fn init_list(list: HWND, rl: &RowList) {
    unsafe {
        SendMessageW(
            list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            Some(WPARAM(LVS_EX_FULLROWSELECT as usize)),
            Some(LPARAM(LVS_EX_FULLROWSELECT as isize)),
        );
    }
    for (i, (title, _)) in rl.columns.iter().enumerate() {
        let mut text = wide(title);
        let column = LVCOLUMNW {
            mask: LVCF_TEXT | LVCF_WIDTH | LVCF_FMT,
            fmt: LVCFMT_LEFT,
            cx: 50,
            pszText: PWSTR(text.as_mut_ptr()),
            ..Default::default()
        };
        unsafe {
            SendMessageW(list, LVM_INSERTCOLUMNW, Some(WPARAM(i)), Some(LPARAM(&column as *const LVCOLUMNW as isize)));
        }
    }
    fit_columns(list, rl);
}

/// 列幅を一覧のクライアント幅の比で入れ直す（開いたときと、DPI が変わった後。ダイアログの伸縮は列幅の px を
/// 変えないため）。最後の列は残りの幅。
fn fit_columns(list: HWND, rl: &RowList) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(list, &mut rc);
    }
    let width = rc.right - rc.left;
    let mut used = 0;
    for (i, (_, ratio)) in rl.columns.iter().enumerate() {
        let w = if i + 1 == rl.columns.len() { width - used } else { width * ratio / 100 };
        used += w;
        unsafe {
            SendMessageW(list, LVM_SETCOLUMNWIDTH, Some(WPARAM(i)), Some(LPARAM(w.max(0) as isize)));
        }
    }
}

fn insert_row(list: HWND, index: usize, cells: &[String]) {
    let mut first = wide(cells.first().map_or("", String::as_str));
    let lvitem = LVITEMW { mask: LVIF_TEXT, iItem: index as i32, pszText: PWSTR(first.as_mut_ptr()), ..Default::default() };
    unsafe {
        SendMessageW(list, LVM_INSERTITEMW, None, Some(LPARAM(&lvitem as *const LVITEMW as isize)));
    }
    set_cells(list, index, cells);
}

fn set_cells(list: HWND, index: usize, cells: &[String]) {
    for (sub, cell) in cells.iter().enumerate() {
        let mut text = wide(cell);
        let lvitem = LVITEMW { iSubItem: sub as i32, pszText: PWSTR(text.as_mut_ptr()), ..Default::default() };
        unsafe {
            SendMessageW(list, LVM_SETITEMTEXTW, Some(WPARAM(index)), Some(LPARAM(&lvitem as *const LVITEMW as isize)));
        }
    }
}

fn selected_row(list: HWND) -> Option<usize> {
    let i = unsafe { SendMessageW(list, LVM_GETNEXTITEM, Some(WPARAM(usize::MAX)), Some(LPARAM(LVNI_SELECTED as isize))) }.0;
    usize::try_from(i).ok()
}

/// `index` の行を選んで見える位置へ（None は選択を外す）。
fn select_row(list: HWND, index: Option<usize>) {
    let on = LVIS_SELECTED.0 | LVIS_FOCUSED.0;
    let set = |i: isize, state: u32| {
        let lvitem = LVITEMW {
            mask: LVIF_STATE,
            stateMask: LIST_VIEW_ITEM_STATE_FLAGS(on),
            state: LIST_VIEW_ITEM_STATE_FLAGS(state),
            ..Default::default()
        };
        unsafe {
            SendMessageW(list, LVM_SETITEMSTATE, Some(WPARAM(i as usize)), Some(LPARAM(&lvitem as *const LVITEMW as isize)));
        }
    };
    match index {
        Some(i) => {
            set(i as isize, on);
            unsafe {
                SendMessageW(list, LVM_ENSUREVISIBLE, Some(WPARAM(i)), Some(LPARAM(0)));
            }
        }
        None => set(-1, 0),
    }
}

/// 選んだ行の値を入力欄へ入れる（`syncing` の間。入力欄の変化を行へ書き戻さない）。行を選んでいなければ、
/// 入力欄を空にして灰色にし、削除のボタンも灰色にする。
fn fill_editor(ctx: &Ctx, rl: &RowList) {
    let page = ctx.pages[rl.page];
    let Some(list) = item(page, rl.list) else {
        return;
    };
    let selected = selected_row(list).filter(|&i| i < ctx.row_count(rl.page));
    // 値を写してから UI を呼ぶ（入力欄への書き込みは同期の知らせを起こす）
    let format = (rl.page == PAGE_FORMAT).then(|| selected.and_then(|i| ctx.formats.borrow().get(i).cloned()));
    let window = (rl.page == PAGE_WINDOW).then(|| selected.and_then(|i| ctx.windows.borrow().get(i).cloned()));
    ctx.syncing.set(true);
    let _syncing = FlagReset(&ctx.syncing);
    match (format, window) {
        (Some(row), _) => {
            let row = row.unwrap_or_else(|| FormatRow { limit_text: String::new(), ..FormatRow::new() });
            set_text(page, IDC_FMT_NAME, &row.name);
            if let Some(combo) = item(page, IDC_FMT_ACTION) {
                let index = ACTIONS.iter().position(|(a, _)| *a == row.action).unwrap_or(0);
                unsafe {
                    SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(index)), None);
                }
            }
            set_check(page, IDC_FMT_SAVE, selected.is_some() && row.save);
            set_text(page, IDC_FMT_LIMIT, &row.limit_text);
            set_text(page, IDC_FMT_LIMIT_SIZE, &readable_size(&row.limit_text));
        }
        (_, Some(row)) => {
            let row = row.unwrap_or(WindowFilter { title: String::new(), class_name: String::new(), ignore: false });
            set_text(page, IDC_WIN_TITLE, &row.title);
            set_text(page, IDC_WIN_CLASS, &row.class_name);
            set_check(page, IDC_WIN_IGNORE, row.ignore);
        }
        _ => {}
    }
    for &id in rl.fields.iter().chain([&rl.delete]) {
        enable_field(page, id, selected.is_some());
    }
}

/// 入力欄の変化を、選んだ行へ書き戻し、一覧のその行の文字を入れ直す。
fn store_field(ctx: &Ctx, rl: &RowList, id: i32) {
    if ctx.syncing.get() || ctx.suppress.get() {
        return;
    }
    let page = ctx.pages[rl.page];
    let Some(list) = item(page, rl.list) else {
        return;
    };
    let Some(index) = selected_row(list) else {
        return;
    };
    // 入力欄を読んでから借用する（読むのは同期の知らせを起こさないが、借用の間は UI を呼ばない）
    let cells = match rl.page {
        PAGE_FORMAT => {
            let (name, action) = (get_text(page, IDC_FMT_NAME), ACTIONS[combo_index(page, IDC_FMT_ACTION).min(ACTIONS.len() - 1)].0);
            let (save, limit_text) = (get_check(page, IDC_FMT_SAVE), get_text(page, IDC_FMT_LIMIT));
            let mut rows = ctx.formats.borrow_mut();
            let Some(row) = rows.get_mut(index) else {
                return;
            };
            match id {
                IDC_FMT_NAME => row.name = name,
                IDC_FMT_ACTION => row.action = action,
                IDC_FMT_SAVE => row.save = save,
                IDC_FMT_LIMIT => row.limit_text = limit_text,
                _ => return,
            }
            row.cells()
        }
        PAGE_WINDOW => {
            let (title, class_name, ignore) =
                (get_text(page, IDC_WIN_TITLE), get_text(page, IDC_WIN_CLASS), get_check(page, IDC_WIN_IGNORE));
            let mut rows = ctx.windows.borrow_mut();
            let Some(row) = rows.get_mut(index) else {
                return;
            };
            match id {
                IDC_WIN_TITLE => row.title = title,
                IDC_WIN_CLASS => row.class_name = class_name,
                IDC_WIN_IGNORE => row.ignore = ignore,
                _ => return,
            }
            window_cells(row)
        }
        _ => return,
    };
    set_cells(list, index, &cells);
    if id == IDC_FMT_LIMIT {
        set_text(page, IDC_FMT_LIMIT_SIZE, &readable_size(&get_text(page, IDC_FMT_LIMIT)));
    }
}

/// 行を選び直す操作（追加・削除・誤りの行）の間は選択の知らせを処理せず、終わってから入力欄を1回だけ埋める
/// （途中の知らせで、確定する前の行や「選択なし」を入力欄に出さない）。
fn with_rows_changing(ctx: &Ctx, rl: &RowList, change: impl FnOnce()) {
    {
        ctx.suppress.set(true);
        let _suppress = FlagReset(&ctx.suppress);
        change();
    }
    fill_editor(ctx, rl);
}

/// 追加: 既定の行を最後に足して選び、先頭の入力欄へフォーカスを移す。
fn add_row(ctx: &Ctx, rl: &RowList) {
    let page = ctx.pages[rl.page];
    let Some(list) = item(page, rl.list) else {
        return;
    };
    with_rows_changing(ctx, rl, || {
        let (index, cells) = match rl.page {
            PAGE_FORMAT => {
                let row = FormatRow::new();
                let cells = row.cells();
                let mut rows = ctx.formats.borrow_mut();
                rows.push(row);
                (rows.len() - 1, cells)
            }
            _ => {
                let row = WindowFilter { title: String::new(), class_name: String::new(), ignore: true };
                let cells = window_cells(&row);
                let mut rows = ctx.windows.borrow_mut();
                rows.push(row);
                (rows.len() - 1, cells)
            }
        };
        insert_row(list, index, &cells);
        select_row(list, Some(index));
    });
    if let Some(ctl) = item(page, rl.fields[0]) {
        unsafe {
            let _ = SetFocus(Some(ctl));
        }
    }
}

/// 削除: 選んだ行を消し、同じ位置（最後の行を消したら新しい最後）の行を選ぶ。フォーカスは一覧へ。
fn delete_row(ctx: &Ctx, rl: &RowList) {
    let page = ctx.pages[rl.page];
    let Some(list) = item(page, rl.list) else {
        return;
    };
    let Some(index) = selected_row(list).filter(|&i| i < ctx.row_count(rl.page)) else {
        return;
    };
    with_rows_changing(ctx, rl, || {
        let left = match rl.page {
            PAGE_FORMAT => {
                let mut rows = ctx.formats.borrow_mut();
                rows.remove(index);
                rows.len()
            }
            _ => {
                let mut rows = ctx.windows.borrow_mut();
                rows.remove(index);
                rows.len()
            }
        };
        unsafe {
            SendMessageW(list, LVM_DELETEITEM, Some(WPARAM(index)), None);
        }
        select_row(list, None);
        select_row(list, (left > 0).then(|| index.min(left - 1)));
    });
    unsafe {
        let _ = SetFocus(Some(list));
    }
}

/// 誤りの行を選ぶ（入力欄が埋まる）。
fn select_issue_row(ctx: &Ctx, rl: &RowList, row: usize) {
    let Some(list) = item(ctx.pages[rl.page], rl.list) else {
        return;
    };
    with_rows_changing(ctx, rl, || select_row(list, Some(row)));
}

/// 数値の入力欄を読む（空欄・数字以外・桁あふれは、その項目の誤り）。範囲は `Config::validate` が確かめる。
fn read_num(pages: &[HWND], num: &NumField, issues: &mut Vec<ConfigIssue>) -> u64 {
    // 上下のボタンは桁区切りを入れない（UDS_NOTHOUSANDS）が、貼り付けに備えて空白は除く
    let text: String = get_text(pages[num.page], num.ctl).chars().filter(|c| !c.is_whitespace()).collect();
    match text.parse::<u64>() {
        Ok(v) if u32::try_from(v).is_ok() || num.field == "history.add_interval_ms" => v,
        _ => {
            issues.push(ConfigIssue { field: num.field, row: None, message: format!("「{}」に数値を入力してください", num.label) });
            0
        }
    }
}

/// 画面からドラフトを作る（`base` の写しから、設定画面にある項目だけを書き換える）。数値として読めない入力は
/// その項目の誤りとして返す（範囲は返さない。`Config::validate` が確かめる）。フィルタは `formats`・`windows`
/// （コンテキストの行の写し）から作る。触っていなければ `base` と同じになる（上限は `to_string` から読み戻す）。
fn read_draft(pages: &[HWND], base: &Config, formats: &[FormatRow], windows: &[WindowFilter]) -> Result<Config, Vec<ConfigIssue>> {
    let [general, history, hotkey, text] = [pages[PAGE_GENERAL], pages[PAGE_HISTORY], pages[PAGE_HOTKEY], pages[PAGE_TEXT]];
    let mut c = base.clone();
    let mut issues = Vec::new();
    c.general.clipboard_watch = get_check(general, IDC_GEN_WATCH);
    c.general.show_trayicon = get_check(general, IDC_GEN_TRAY);
    c.general.start_hidden = get_check(general, IDC_GEN_START_HIDDEN);
    c.general.startup_clipboard_sync = get_check(general, IDC_GEN_SYNC);
    c.general.notify_action_errors = get_check(general, IDC_GEN_NOTIFY);
    c.general.check_folder_permissions = get_check(general, IDC_GEN_FOLDER_CHECK);

    let n: Vec<u64> = NUMS.iter().map(|num| read_num(pages, num, &mut issues)).collect();
    let as_u32 = |v: u64| u32::try_from(v).unwrap_or(u32::MAX);
    c.history.max = as_u32(n[0]);
    c.history.grouping.visible_items = as_u32(n[1]);
    c.history.grouping.folders = as_u32(n[2]);
    c.history.grouping.items_per_folder = as_u32(n[3]);
    c.history.add_interval_ms = n[4];
    c.hotkey.menu_max_items = as_u32(n[5]);
    c.hotkey.tooltip.delay_ms = as_u32(n[6]);
    c.hotkey.tooltip.max_chars = as_u32(n[7]);
    c.hotkey.tooltip.max_lines = as_u32(n[8]);
    c.tools.text.word_break_width = as_u32(n[9]);

    c.history.grouping.enabled = get_check(history, IDC_HIS_GROUP);
    c.history.grouping.folder_name_format = get_text(history, IDC_HIS_FORMAT);
    c.history.overlap_check = OVERLAPS[combo_index(history, IDC_HIS_OVERLAP).min(OVERLAPS.len() - 1)].0;
    c.history.save_on_exit = get_check(history, IDC_HIS_SAVE_EXIT);
    c.history.save_on_change = get_check(history, IDC_HIS_SAVE_CHANGE);
    c.history.delete_on_send = get_check(history, IDC_HIS_DELETE_ON_SEND);
    c.history.sound_on_add = get_check(history, IDC_HIS_SOUND);
    c.history.sound_file = get_text(history, IDC_HIS_SOUND_FILE);

    c.hotkey.popup_menu.enabled = get_check(hotkey, IDC_HK_POPUP);
    // 修飾キーは、チェックの集合が開いたときと同じなら開いたときの並びのまま（並びの違いで「変えた」と
    // 見なされ、合わせ方で今の値を上書きしないため）。違えば ctrl・shift・alt・win の順
    let chosen: Vec<String> = MODIFIERS.iter().filter(|(_, id)| get_check(hotkey, *id)).map(|(n, _)| n.to_string()).collect();
    let same = MODIFIERS.iter().all(|(name, _)| {
        chosen.iter().any(|m| m == name) == base.hotkey.popup_menu.modifiers.iter().any(|m| m.eq_ignore_ascii_case(name))
    });
    if !same {
        c.hotkey.popup_menu.modifiers = chosen;
    }
    c.hotkey.popup_menu.key = get_text(hotkey, IDC_HK_KEY);
    c.hotkey.auto_paste = get_check(hotkey, IDC_HK_AUTOPASTE);
    c.hotkey.menu_pinned_first = get_check(hotkey, IDC_HK_PINNED_FIRST);
    c.hotkey.tooltip.enabled = get_check(hotkey, IDC_HK_TOOLTIP);
    let dp = |id| DOUBLE_PRESSES[combo_index(hotkey, id).min(DOUBLE_PRESSES.len() - 1)].0;
    c.hotkey.double_press_ctrl = dp(IDC_HK_DP_CTRL);
    c.hotkey.double_press_shift = dp(IDC_HK_DP_SHIFT);
    c.hotkey.double_press_alt = dp(IDC_HK_DP_ALT);

    c.tools.text.quote_char = get_text(text, IDC_TX_QUOTE);
    c.tools.text.put_text_open = get_text(text, IDC_TX_OPEN);
    c.tools.text.put_text_close = get_text(text, IDC_TX_CLOSE);
    c.tools.text.delete_crlf_trim_leading = get_check(text, IDC_TX_TRIM);
    c.tools.text.convert_date_on_send = get_check(text, IDC_TX_DATE);
    c.tools.text.date_format = get_text(text, IDC_TX_DATE_FMT);
    c.tools.text.time_format = get_text(text, IDC_TX_TIME_FMT);

    c.format_filter_default = ACTIONS[combo_index(pages[PAGE_FORMAT], IDC_FMT_DEFAULT).min(ACTIONS.len() - 1)].0;
    c.capture_total_limit = parse_limit(&get_text(pages[PAGE_FORMAT], IDC_FMT_TOTAL)).unwrap_or_else(|| {
        issues.push(ConfigIssue {
            field: "capture_total_limit",
            row: None,
            message: "「コピーの合計の上限」に数値を入力してください".to_string(),
        });
        0
    });
    c.format_filters = formats
        .iter()
        .enumerate()
        .map(|(i, r)| FormatFilter {
            format_name: r.name.clone(),
            action: r.action,
            save: r.save,
            limit_size: parse_limit(&r.limit_text).unwrap_or_else(|| {
                issues.push(ConfigIssue {
                    field: "format_filters.limit_size",
                    row: Some(i),
                    message: format!("形式フィルタの {} 行目: 「上限」に数値を入力してください", i + 1),
                });
                0
            }),
        })
        .collect();
    c.window_filters = windows.to_vec();

    if issues.is_empty() { Ok(c) } else { Err(issues) }
}

/// ページの「入力できる・できない」と、階層表示の保持総数・コピーの合計の上限の読みやすい大きさの表示を合わせる。
fn sync_page(page: HWND, index: usize) {
    if index == PAGE_FORMAT {
        set_text(page, IDC_FMT_TOTAL_SIZE, &readable_size(&get_text(page, IDC_FMT_TOTAL)));
    }
    for (p, enabler, targets) in ENABLERS {
        if p != index {
            continue;
        }
        let on = get_check(page, enabler);
        for &id in targets {
            enable_field(page, id, on);
        }
    }
    if index == PAGE_HISTORY {
        let read = |id| get_text(page, id).trim().parse::<u64>().ok();
        let text = match (read(IDC_HIS_VISIBLE), read(IDC_HIS_FOLDERS), read(IDC_HIS_PER_FOLDER)) {
            (Some(v), Some(f), Some(p)) => format!(
                "有効な間の保持総数: {}件（「最大件数」より優先）",
                v.saturating_add(f.saturating_mul(p))
            ),
            _ => "有効な間の保持総数: —".to_string(),
        };
        set_text(page, IDC_HIS_TOTAL, &text);
    }
}

/// タブの表示領域（ダイアログのクライアント座標）へページを置く。
fn place_pages(dialog: HWND, pages: &[HWND]) {
    let Some(tab) = item(dialog, IDC_SET_TAB) else {
        return;
    };
    unsafe {
        let mut rc = RECT::default();
        let _ = GetWindowRect(tab, &mut rc);
        let mut pts = [POINT { x: rc.left, y: rc.top }, POINT { x: rc.right, y: rc.bottom }];
        MapWindowPoints(None, Some(dialog), &mut pts);
        let mut r = RECT { left: pts[0].x, top: pts[0].y, right: pts[1].x, bottom: pts[1].y };
        SendMessageW(tab, TCM_ADJUSTRECT, Some(WPARAM(0)), Some(LPARAM(&mut r as *mut RECT as isize)));
        for &page in pages {
            let _ = SetWindowPos(page, Some(tab), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOACTIVATE);
        }
    }
}

/// `index` のタブを選び、そのページだけを出す。
fn select_page(dialog: HWND, index: usize) {
    let Some(ctx) = (unsafe { ctx_ref(dialog) }) else {
        return;
    };
    if let Some(tab) = item(dialog, IDC_SET_TAB) {
        unsafe {
            SendMessageW(tab, TCM_SETCURSEL, Some(WPARAM(index)), None);
        }
    }
    for (i, &page) in ctx.pages.iter().enumerate() {
        unsafe {
            let _ = ShowWindow(page, if i == index { SW_SHOW } else { SW_HIDE });
        }
    }
}

/// 誤りの欄に出す文: 最初の誤りの説明（無効な欄なら直し方を書き添える）と、残りの件数（誤りの欄は固定の高さなので、
/// 全部は並べない。直して OK を押すと次の誤りを示す）。
fn issue_text(pages: &[HWND], issues: &[ConfigIssue]) -> String {
    let Some(first) = issues.first() else {
        return String::new();
    };
    let hint = PLACES
        .iter()
        .find(|p| p.field == first.field)
        .and_then(|p| p.enabler.filter(|(id, _)| !get_check(pages[p.page], *id)))
        .map(|(_, label)| format!("（「{label}」をオンにすると直せます）"))
        .unwrap_or_default();
    let rest = match issues.len() - 1 {
        0 => String::new(),
        n => format!("\nほかに {n} 件の誤りがあります（直して OK を押すと次を示します）"),
    };
    format!("{}{hint}{rest}", first.message)
}

/// 誤りを出す: 最初の誤りのタブを出し、その入力欄（無効な間は有効にするチェックボックス）へフォーカスを移し、
/// 誤りの欄に最初の誤りの説明と残りの件数を出す。フィルタの行の誤りは、先にその行を選ぶ。
fn show_issues(dialog: HWND, pages: &[HWND], issues: &[ConfigIssue]) {
    set_text(dialog, IDC_SET_ERROR, &issue_text(pages, issues));
    let Some(first) = issues.first() else {
        return;
    };
    let Some(place) = PLACES.iter().find(|p| p.field == first.field) else {
        return;
    };
    select_page(dialog, place.page);
    if let (Some(row), Some(rl), Some(ctx)) = (first.row, row_list(place.page), unsafe { ctx_ref(dialog) }) {
        if row < ctx.row_count(rl.page) {
            select_issue_row(ctx, rl, row);
        }
    }
    let page = pages[place.page];
    let target = match place.enabler {
        Some((enabler, _)) if !get_check(page, enabler) => enabler,
        _ => place.ctl,
    };
    if let Some(ctl) = item(page, target) {
        unsafe {
            let _ = SetFocus(Some(ctl));
        }
    }
}

/// 閉じる要求を1回だけ投稿する（投稿できなければ印を戻し、もう一度閉じられるようにする）。
fn post_close(dialog: HWND, ctx: &Ctx) {
    if ctx.closing.get() || ctx.ended.get() {
        return;
    }
    ctx.closing.set(true);
    #[cfg(test)]
    let forced = FAIL_CLOSE_POST.with(Cell::get);
    #[cfg(not(test))]
    let forced = false;
    let posted = !forced && unsafe { PostMessageW(Some(dialog), WM_APP_SETTINGS_CLOSE, WPARAM(0), LPARAM(0)) }.is_ok();
    if !posted {
        ctx.closing.set(false);
        eprintln!("設定画面を閉じる要求を投稿できませんでした（もう一度閉じてください）");
    }
}

/// キャンセル・×・Esc。OK の処理中は記録だけ（OK が戻ってから閉じる）。
fn request_close(dialog: HWND) {
    let Some(ctx) = (unsafe { ctx_ref(dialog) }) else {
        return;
    };
    if ctx.ended.get() || ctx.closing.get() {
        return;
    }
    if ctx.ok_running.get() {
        ctx.close_pending.set(true);
        return;
    }
    post_close(dialog, ctx);
}

/// スコープを抜けると `Cell<bool>` を下ろす（早期の return でも）。
struct FlagReset<'a>(&'a Cell<bool>);

impl Drop for FlagReset<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// OK: ドラフトを作り、`on_ok` を呼ぶ。成功（または処理中の閉じる要求）で閉じ、誤りは窓の中に出して開いたまま。
fn press_ok(dialog: HWND) {
    let Some(ctx) = (unsafe { ctx_ref(dialog) }) else {
        return;
    };
    if ctx.ok_running.get() || ctx.closing.get() || ctx.ended.get() {
        return;
    }
    let pages = ctx.pages.clone();
    let (formats, windows) = (ctx.formats.borrow().clone(), ctx.windows.borrow().clone());
    let draft = match read_draft(&pages, &ctx.base, &formats, &windows) {
        Ok(draft) => draft,
        Err(issues) => {
            show_issues(dialog, &pages, &issues);
            return;
        }
    };
    // 反映の間に使うものを手元に持つ（反映の中でメッセージを処理しても、コンテキストの中を参照し続けない）
    let base = ctx.base.clone();
    let on_ok = Rc::clone(&ctx.on_ok);
    ctx.ok_running.set(true);
    let result = {
        let _running = FlagReset(&ctx.ok_running);
        on_ok(&base, &draft)
    };
    // OK の処理中は破棄の入口が働かないので、コンテキストはまだある
    let Some(ctx) = (unsafe { ctx_ref(dialog) }) else {
        return;
    };
    if ctx.ended.get() {
        return;
    }
    match result {
        Ok(()) => post_close(dialog, ctx),
        Err(_) if ctx.close_pending.get() => post_close(dialog, ctx),
        Err(ApplyError::Invalid(issues)) => show_issues(dialog, &pages, &issues),
        Err(ApplyError::Save(e)) => set_text(dialog, IDC_SET_ERROR, &format!("設定を保存できませんでした: {e}")),
        Err(ApplyError::Busy) => set_text(
            dialog,
            IDC_SET_ERROR,
            "今は反映できません。ほかのダイアログを閉じてから、もう一度 OK を押してください",
        ),
    }
}

/// 閉じる要求の処理: `on_closed` を呼ぶ（`App` が `SettingsWindow` を捨て、この窓は破棄される）。その後は
/// コンテキストに触らない。
///
/// OK の処理中は破棄しない: `press_ok` はコンテキストの `ok_running` を借りたまま `on_ok` を呼び、`on_ok` の中では
/// 送られたメッセージが処理されうる（トレイの停止を待つ間など）。ほかのプロセスがこのメッセージを送ってくると、ここで
/// 窓を破棄すると、戻った `press_ok` が解放したコンテキストに書く。OK の処理中に届いたら閉じる要求を記録だけし
/// （`request_close` と同じ）、投稿済みの印を下ろす（OK が戻った後の `post_close` が投稿し直せるように）。
fn handle_close(dialog: HWND) {
    let on_closed = {
        let Some(ctx) = (unsafe { ctx_ref(dialog) }) else {
            return;
        };
        if ctx.ended.get() {
            return;
        }
        if ctx.ok_running.get() {
            ctx.close_pending.set(true);
            ctx.closing.set(false);
            return;
        }
        Rc::clone(&ctx.on_closed)
    };
    on_closed();
}

unsafe extern "system" fn settings_proc(dialog: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> isize {
    unsafe {
        match msg {
            WM_INITDIALOG => {
                let raw = lparam.0 as *mut Ctx;
                SetWindowLongPtrW(dialog, DWLP_USER, raw as isize);
                ATTACHED.with(|a| a.set(raw as isize));
                if let Some(tab) = item(dialog, IDC_SET_TAB) {
                    for (i, (_, name)) in PAGES.iter().enumerate() {
                        let mut text = wide(name);
                        let tcitem = TCITEMW { mask: TCIF_TEXT, pszText: PWSTR(text.as_mut_ptr()), ..Default::default() };
                        SendMessageW(tab, TCM_INSERTITEMW, Some(WPARAM(i)), Some(LPARAM(&tcitem as *const TCITEMW as isize)));
                    }
                }
                let hinst = GetModuleHandleW(None).ok().map(Into::into);
                let mut pages = Vec::new();
                for (i, (id, _)) in PAGES.iter().enumerate() {
                    match CreateDialogParamW(hinst, PCWSTR(*id as usize as *const u16), Some(dialog), Some(page_proc), LPARAM(i as isize)) {
                        Ok(page) => pages.push(page),
                        Err(e) => {
                            // 足りないページのまま返す（`SettingsWindow::create` が数を確かめて窓を破棄する）
                            eprintln!("設定画面のページを作れません: {e}");
                            break;
                        }
                    }
                }
                // 書き換えはこの1回だけ（この後は共有参照 `ctx_ref` で読む）
                (*raw).pages = pages.clone();
                if pages.len() != PAGES.len() {
                    return 1;
                }
                place_pages(dialog, &pages);
                if let Some(ctx) = ctx_ref(dialog) {
                    load(ctx);
                }
                ACTIVE.with(|a| a.set(dialog.0 as isize));
                select_page(dialog, 0);
                1
            }
            WM_NOTIFY => {
                let hdr = &*(lparam.0 as *const NMHDR);
                if hdr.idFrom as i32 == IDC_SET_TAB && hdr.code == TCN_SELCHANGE {
                    if let Some(tab) = item(dialog, IDC_SET_TAB) {
                        let index = SendMessageW(tab, TCM_GETCURSEL, None, None).0.max(0) as usize;
                        select_page(dialog, index);
                    }
                }
                0
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                let code = ((wparam.0 >> 16) & 0xFFFF) as u32;
                if code == BN_CLICKED || code == 0 {
                    if id == windows::Win32::UI::WindowsAndMessaging::IDOK.0 {
                        press_ok(dialog);
                        return 1;
                    }
                    if id == windows::Win32::UI::WindowsAndMessaging::IDCANCEL.0 {
                        request_close(dialog);
                        return 1;
                    }
                }
                0
            }
            WM_CLOSE => {
                request_close(dialog);
                1
            }
            WM_DPICHANGED => {
                // 既定の処理（ダイアログの伸縮）の後にページを置き直す
                let _ = PostMessageW(Some(dialog), WM_APP_PLACE_PAGES, WPARAM(0), LPARAM(0));
                0
            }
            WM_APP_PLACE_PAGES => {
                if let Some(ctx) = ctx_ref(dialog) {
                    place_pages(dialog, &ctx.pages);
                    for rl in &ROW_LISTS {
                        if let Some(list) = item(ctx.pages[rl.page], rl.list) {
                            fit_columns(list, rl);
                        }
                    }
                }
                1
            }
            WM_APP_SETTINGS_CLOSE => {
                handle_close(dialog);
                1
            }
            WM_NCDESTROY => {
                let raw = GetWindowLongPtrW(dialog, DWLP_USER) as *mut Ctx;
                if !raw.is_null() {
                    SetWindowLongPtrW(dialog, DWLP_USER, 0);
                    drop(Box::from_raw(raw));
                }
                if ACTIVE.with(Cell::get) == dialog.0 as isize {
                    ACTIVE.with(|a| a.set(0));
                }
                0
            }
            _ => 0,
        }
    }
}

/// ページ（子のダイアログ）。`DWLP_USER` はページの番号。チェックボックス・数値の変化で、入力できる・できないと
/// 保持総数の表示を合わせる。フィルタのページでは、追加・削除・入力欄の変化・一覧の選択の変化を扱う（コンテキストは
/// 親の設定画面の `DWLP_USER`）。
unsafe extern "system" fn page_proc(page: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> isize {
    unsafe {
        let index = || GetWindowLongPtrW(page, DWLP_USER) as usize;
        // ページをすべて作り終えるまでは扱わない（`Ctx::pages` がまだ無い）
        let ctx = || GetParent(page).ok().and_then(|dialog| ctx_ref(dialog)).filter(|c| c.pages.len() == PAGES.len());
        match msg {
            WM_INITDIALOG => {
                SetWindowLongPtrW(page, DWLP_USER, lparam.0);
                0
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                let code = ((wparam.0 >> 16) & 0xFFFF) as u32;
                if let (Some(rl), Some(ctx)) = (row_list(index()), ctx()) {
                    if code == BN_CLICKED && id == rl.add {
                        add_row(ctx, rl);
                        return 1;
                    }
                    if code == BN_CLICKED && id == rl.delete {
                        delete_row(ctx, rl);
                        return 1;
                    }
                    let changed = match id {
                        IDC_FMT_ACTION => code == CBN_SELCHANGE,
                        IDC_FMT_SAVE | IDC_WIN_IGNORE => code == BN_CLICKED,
                        _ => code == EN_CHANGE,
                    };
                    if changed && rl.fields.contains(&id) {
                        store_field(ctx, rl, id);
                        return 1;
                    }
                }
                if code == BN_CLICKED || code == EN_CHANGE {
                    sync_page(page, index());
                }
                0
            }
            WM_NOTIFY => {
                let hdr = &*(lparam.0 as *const NMHDR);
                if let (Some(rl), Some(ctx)) = (row_list(index()), ctx()) {
                    if hdr.idFrom as i32 == rl.list && hdr.code == LVN_ITEMCHANGED {
                        let nm = &*(lparam.0 as *const NMLISTVIEW);
                        let selection_changed = (nm.uChanged.0 & LVIF_STATE.0) != 0
                            && ((nm.uNewState ^ nm.uOldState) & LVIS_SELECTED.0) != 0;
                        // 行番号は知らせのものを使わず、確定した選択を読む（fill_editor）
                        if selection_changed && !ctx.suppress.get() {
                            fill_editor(ctx, rl);
                        }
                    }
                }
                0
            }
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DoublePressAction;

    #[test]
    fn effects_follow_changed_settings() {
        let base = Config::default();
        assert_eq!(effects(&base, &base), Effects::default(), "変えていないのに反映する");

        let mut c = base.clone();
        c.hotkey.popup_menu.key = "V".to_string();
        assert_eq!(effects(&base, &c), Effects { hotkeys: true, ..Effects::default() });
        let mut c = base.clone();
        c.hotkey.double_press_shift = DoublePressAction::Menu;
        assert!(effects(&base, &c).hotkeys);

        // 階層表示の切り替え（保持件数が同じになるようにそろえる）は作り直しだけ
        let mut flat = base.clone();
        flat.history.grouping.enabled = false;
        flat.history.max = flat.history.grouping.total();
        let mut grouped = flat.clone();
        grouped.history.grouping.enabled = true;
        assert_eq!(effects(&flat, &grouped), Effects { rebuild: true, ..Effects::default() }, "保持件数は同じ");
        // 階層表示が有効な間のフォルダ数は、保持件数も変わる
        let mut more = grouped.clone();
        more.history.grouping.folders += 1;
        assert_eq!(effects(&grouped, &more), Effects { trim: true, rebuild: true, ..Effects::default() });
        // 無効な間の最大件数は、保持件数だけ
        let mut smaller = flat.clone();
        smaller.history.max -= 1;
        assert_eq!(effects(&flat, &smaller), Effects { trim: true, ..Effects::default() });
        let mut c = base.clone();
        c.history.grouping.folder_name_format = "x%1".to_string();
        assert_eq!(effects(&base, &c), Effects { rebuild: true, ..Effects::default() });

        // 使う時点で読む設定は、その場の反映が要らない
        let mut c = base.clone();
        c.hotkey.tooltip.max_lines = 3;
        c.hotkey.menu_max_items = 5;
        c.history.sound_on_add = !base.history.sound_on_add;
        c.general.notify_action_errors = !base.general.notify_action_errors;
        c.general.check_folder_permissions = !base.general.check_folder_permissions;
        c.general.clipboard_watch = !base.general.clipboard_watch;
        c.general.show_trayicon = !base.general.show_trayicon;
        assert_eq!(effects(&base, &c), Effects::default());
    }

    // --- 設定画面の窓 ---

    use std::cell::RefCell;
    use std::time::{Duration, Instant};
    use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetClassNameW, GetClientRect, GetWindow, GetWindowLongW, IsWindowVisible, PeekMessageW, TranslateMessage, GWL_STYLE, GW_CHILD, GW_HWNDNEXT, IDCANCEL, IDOK,
        PM_REMOVE, WS_TABSTOP, WS_VISIBLE,
    };

    /// 設定画面の試験台。閉じたとき（`on_closed`）は、`slot` の窓を捨てて数える（`App` と同じ）。
    struct Harness {
        slot: Rc<RefCell<Option<SettingsWindow>>>,
        closed: Rc<Cell<usize>>,
        calls: Rc<RefCell<Vec<(String, String)>>>,
        dialog: HWND,
    }

    impl Harness {
        fn open(base: Config, result: impl Fn() -> Result<(), ApplyError> + 'static) -> Self {
            let slot: Rc<RefCell<Option<SettingsWindow>>> = Rc::default();
            let closed = Rc::new(Cell::new(0));
            let calls: Rc<RefCell<Vec<(String, String)>>> = Rc::default();
            let on_ok: OnOk = {
                let calls = Rc::clone(&calls);
                Rc::new(move |base, draft| {
                    calls.borrow_mut().push((toml::to_string(base).unwrap(), toml::to_string(draft).unwrap()));
                    result()
                })
            };
            let on_closed: OnClosed = {
                let (slot, closed) = (Rc::clone(&slot), Rc::clone(&closed));
                Rc::new(move || {
                    closed.set(closed.get() + 1);
                    let window = slot.borrow_mut().take();
                    drop(window);
                })
            };
            let window = SettingsWindow::create(None, base, on_ok, on_closed).expect("設定画面を作れない");
            let dialog = window.hwnd();
            *slot.borrow_mut() = Some(window);
            Self { slot, closed, calls, dialog }
        }

        fn pages(&self) -> Vec<HWND> {
            unsafe { ctx_ref(self.dialog) }.expect("設定画面が無い").pages.clone()
        }

        fn ctx(&self) -> &Ctx {
            unsafe { ctx_ref(self.dialog) }.expect("設定画面が無い")
        }

        /// 今の画面からドラフトを読む（OK と同じ読み方）。
        fn read(&self, base: &Config) -> Result<Config, Vec<ConfigIssue>> {
            let ctx = self.ctx();
            let (formats, windows) = (ctx.formats.borrow().clone(), ctx.windows.borrow().clone());
            read_draft(&self.pages(), base, &formats, &windows)
        }

        /// ページのボタンを押す（ページへの `BN_CLICKED`）。
        fn click(&self, page: usize, id: i32) {
            let page = self.pages()[page];
            unsafe {
                SendMessageW(
                    page,
                    WM_COMMAND,
                    Some(WPARAM(((BN_CLICKED as usize) << 16) | id as usize)),
                    Some(LPARAM(item(page, id).unwrap().0 as isize)),
                );
            }
        }

        fn list(&self, page: usize) -> HWND {
            item(self.pages()[page], row_list(page).unwrap().list).unwrap()
        }
    }

    /// 一覧のセルの文字。
    fn cell(list: HWND, row: usize, sub: usize) -> String {
        use windows::Win32::UI::Controls::LVM_GETITEMTEXTW;
        let mut buf = [0u16; 256];
        let lvitem = LVITEMW { iSubItem: sub as i32, pszText: PWSTR(buf.as_mut_ptr()), cchTextMax: buf.len() as i32, ..Default::default() };
        let len = unsafe { SendMessageW(list, LVM_GETITEMTEXTW, Some(WPARAM(row)), Some(LPARAM(&lvitem as *const _ as isize))) }.0;
        String::from_utf16_lossy(&buf[..len as usize])
    }

    fn row_total(list: HWND) -> usize {
        use windows::Win32::UI::Controls::LVM_GETITEMCOUNT;
        unsafe { SendMessageW(list, LVM_GETITEMCOUNT, None, None) }.0 as usize
    }

    fn format(name: &str, action: FilterAction, save: bool, limit_size: u64) -> FormatFilter {
        FormatFilter { format_name: name.to_string(), action, save, limit_size }
    }

    fn window(title: &str, class_name: &str, ignore: bool) -> WindowFilter {
        WindowFilter { title: title.to_string(), class_name: class_name.to_string(), ignore }
    }

    impl Harness {

        fn command(&self, id: i32) {
            unsafe {
                SendMessageW(self.dialog, WM_COMMAND, Some(WPARAM(id as usize)), Some(LPARAM(0)));
            }
        }

        fn is_open(&self) -> bool {
            self.slot.borrow().is_some() && unsafe { IsWindow(Some(self.dialog)) }.as_bool()
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let window = self.slot.borrow_mut().take();
            drop(window);
        }
    }

    fn pump(ms: u64) {
        let until = Instant::now() + Duration::from_millis(ms);
        let mut msg = MSG::default();
        while Instant::now() < until {
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if is_dialog_message(&msg) {
                        continue;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn class_of(h: HWND) -> String {
        let mut buf = [0u16; 64];
        let n = unsafe { GetClassNameW(h, &mut buf) } as usize;
        String::from_utf16_lossy(&buf[..n])
    }

    fn children(parent: HWND) -> Vec<HWND> {
        let mut out = Vec::new();
        let mut child = unsafe { GetWindow(parent, GW_CHILD) }.ok();
        while let Some(c) = child {
            out.push(c);
            child = unsafe { GetWindow(c, GW_HWNDNEXT) }.ok();
        }
        out
    }

    /// 既定でない値をあちこちに入れた設定（設定画面にある項目）。
    fn unusual() -> Config {
        let mut c = Config::default();
        c.general.clipboard_watch = false;
        c.general.start_hidden = true;
        c.general.notify_action_errors = false;
        c.general.check_folder_permissions = false;
        c.history.max = 77;
        c.history.grouping.enabled = true;
        c.history.grouping.visible_items = 12;
        c.history.grouping.folders = 3;
        c.history.grouping.items_per_folder = 40;
        c.history.grouping.folder_name_format = "[%1〜%2]".to_string();
        c.history.overlap_check = OverlapCheck::All;
        c.history.add_interval_ms = 2500;
        c.history.save_on_change = true;
        c.history.sound_on_add = true;
        c.history.sound_file = r"C:\音\a.wav".to_string();
        c.hotkey.popup_menu.modifiers = vec!["alt".to_string(), "ctrl".to_string()];
        c.hotkey.popup_menu.key = "V".to_string();
        c.hotkey.auto_paste = false;
        c.hotkey.menu_max_items = 9;
        c.hotkey.menu_pinned_first = true;
        c.hotkey.tooltip.delay_ms = 0;
        c.hotkey.tooltip.max_chars = 99_999;
        c.hotkey.tooltip.max_lines = 3;
        c.hotkey.double_press_ctrl = DoublePressAction::Viewer;
        c.hotkey.double_press_alt = DoublePressAction::Menu;
        c.tools.text.quote_char = "》".to_string();
        c.tools.text.word_break_width = 40;
        c.tools.text.put_text_open = "〔".to_string();
        c.tools.text.put_text_close = "〕".to_string();
        c.tools.text.delete_crlf_trim_leading = false;
        c.tools.text.convert_date_on_send = true;
        c.tools.text.date_format = "yyyy年M月d日".to_string();
        c.tools.text.time_format = "H時m分".to_string();
        c.format_filter_default = FilterAction::Add;
        c.format_filters = vec![
            format("CF_UNICODETEXT", FilterAction::Add, true, 0),
            format("独自の形式", FilterAction::Ignore, false, 1_048_576),
            format("CF_DIB", FilterAction::Add, false, i64::MAX as u64), // TOML で表せる最大
        ];
        c.window_filters = vec![window("メモ帳", "", true), window("", "Notepad", false), window("秘密", "Chrome_WidgetWin_1", true)];
        c
    }

    /// exe に、アイコン（ID 1）・マニフェスト（ID 1）・バージョン情報（ID 1）・設定画面と名前の変更のダイアログが入っている
    /// （app.rc の文字コードの指定とヘッダーの読み込みがあっても）。
    #[test]
    fn resources_include_icon_manifest_and_settings_dialogs() {
        use windows::Win32::System::LibraryLoader::FindResourceW;
        let module = unsafe { GetModuleHandleW(None) }.unwrap();
        let find = |name: usize, kind: usize| unsafe {
            !FindResourceW(Some(module), PCWSTR(name as *const u16), PCWSTR(kind as *const u16)).is_invalid()
        };
        const RT_DIALOG: usize = 5;
        const RT_GROUP_ICON: usize = 14;
        const RT_MANIFEST: usize = 24;
        const RT_VERSION: usize = 16;
        assert!(find(1, RT_GROUP_ICON), "アイコンが無い");
        assert!(find(1, RT_MANIFEST), "マニフェストが無い");
        assert!(find(1, RT_VERSION), "バージョン情報が無い");
        for id in [IDD_SETTINGS, IDD_PAGE_GENERAL, IDD_PAGE_HISTORY, IDD_PAGE_HOTKEY, IDD_PAGE_TEXT, IDD_PAGE_FORMAT, IDD_PAGE_WINDOW, IDD_RENAME] {
            assert!(find(id as usize, RT_DIALOG), "ダイアログ {id} が無い");
        }
    }

    /// マニフェストの中身が実際に効いている: プロセスは PerMonitorV2 で、読み込まれる Common Controls は v6。
    /// テストの exe にも同じマニフェスト（ID 1）が入り、起動時に適用される。スレッドの DPI の設定を
    /// 変える処理は無いので、プロセスの値を見る。Common Controls の依存を外すと、このテストより前に exe の起動が
    /// `STATUS_ENTRYPOINT_NOT_FOUND` で失敗した（v6 が要る関数 `TaskDialogIndirect` を読み込むためと見ているが、
    /// 見つからなかった関数の名前は確かめていない）。v6 の確認は、起動で落ちなくなったときのため。
    #[test]
    fn manifest_enables_per_monitor_v2_and_common_controls_v6() {
        use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
        use windows::Win32::System::Threading::GetCurrentProcess;
        use windows::Win32::UI::HiDpi::{AreDpiAwarenessContextsEqual, GetDpiAwarenessContextForProcess, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
        use windows::core::{s, w};
        let context = unsafe { GetDpiAwarenessContextForProcess(GetCurrentProcess()) };
        assert!(unsafe { AreDpiAwarenessContextsEqual(context, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }.as_bool(), "PerMonitorV2 になっていない");

        // DLLVERSIONINFO（shlwapi.h）
        #[repr(C)]
        struct DllVersionInfo {
            cb_size: u32,
            major: u32,
            minor: u32,
            build: u32,
            platform_id: u32,
        }
        let comctl = unsafe { LoadLibraryW(w!("comctl32.dll")) }.expect("comctl32.dll を読めない");
        let proc = unsafe { GetProcAddress(comctl, s!("DllGetVersion")) }.expect("DllGetVersion が無い");
        let dll_get_version: unsafe extern "system" fn(*mut DllVersionInfo) -> windows::core::HRESULT = unsafe { std::mem::transmute(proc) };
        let mut info = DllVersionInfo { cb_size: std::mem::size_of::<DllVersionInfo>() as u32, major: 0, minor: 0, build: 0, platform_id: 0 };
        unsafe { dll_get_version(&mut info) }.unwrap();
        assert_eq!(info.major, 6, "Common Controls が v6 でない（{}.{}）", info.major, info.minor);
    }

    /// 画面へ入れて読み戻すと、設定画面にある項目は変わらない（既定と既定でない値。修飾キーの並びも開いたときの
    /// まま）。修飾キーを変えると ctrl・shift・alt・win の順になる。
    #[test]
    fn load_then_read_round_trips() {
        let _gui = crate::tray::lock_gui_resource_tests();
        // フィルタが空・リストにない形式の両方の値も
        let mut empty = Config::default();
        empty.format_filters.clear();
        empty.window_filters.clear();
        let mut empty_add = empty.clone();
        empty_add.format_filter_default = FilterAction::Add;
        for base in [Config::default(), unusual(), empty, empty_add] {
            let h = Harness::open(base.clone(), || Ok(()));
            let draft = h.read(&base).expect("読み戻せない");
            assert_eq!(toml::to_string(&draft).unwrap(), toml::to_string(&base).unwrap());
        }
        let base = unusual();
        let h = Harness::open(base.clone(), || Ok(()));
        let pages = h.pages();
        set_check(pages[PAGE_HOTKEY], IDC_HK_WIN, true);
        let draft = h.read(&base).unwrap();
        assert_eq!(draft.hotkey.popup_menu.modifiers, ["ctrl", "alt", "win"]);
    }

    /// フィルタの行を編集して元へ戻すと、一覧は開いたときと同じになり、合わせ方（`merge_edit`）で今の値が残る
    /// （開いている間にほかの経路で変わった値を上書きしない）。触らずに OK しても同じ。
    #[test]
    fn edited_then_restored_filter_rows_keep_current_value() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let base = unusual();
        let h = Harness::open(base.clone(), || Ok(()));
        let page = h.pages()[PAGE_FORMAT];
        let list = h.list(PAGE_FORMAT);
        select_row(list, Some(1));
        set_text(page, IDC_FMT_LIMIT, "5");
        assert_eq!(h.ctx().formats.borrow()[1].limit_text, "5");
        set_text(page, IDC_FMT_LIMIT, "1048576");
        let wpage = h.pages()[PAGE_WINDOW];
        select_row(h.list(PAGE_WINDOW), Some(0));
        set_check(wpage, IDC_WIN_IGNORE, false);
        h.click(PAGE_WINDOW, IDC_WIN_IGNORE);
        set_check(wpage, IDC_WIN_IGNORE, true);
        h.click(PAGE_WINDOW, IDC_WIN_IGNORE);
        let draft = h.read(&base).unwrap();
        assert_eq!(draft.format_filters.len(), 3);
        assert_eq!(toml::to_string(&draft).unwrap(), toml::to_string(&base).unwrap(), "元へ戻したのに違う");

        // 開いている間にほかの経路で配列が変わった（今は無い経路だが、合わせ方の性質として確かめる）
        let mut current = base.clone();
        current.format_filters.push(format("CF_HDROP", FilterAction::Add, true, 0));
        current.window_filters.clear();
        let merged = crate::config::merge_edit(&base, &draft, &current).unwrap();
        assert_eq!(merged.format_filters.len(), 4, "触っていない配列で今の値を上書きした");
        assert!(merged.window_filters.is_empty());
    }

    /// 数値として読めない入力（空欄・桁あふれ）は、その項目の誤りとして返す。`Config::validate` と数値の欄の誤りの
    /// 項目は、すべて誤りの場所の表にある。
    #[test]
    fn unreadable_numbers_and_every_issue_field_have_a_place() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let base = Config::default();
        let h = Harness::open(base.clone(), || Ok(()));
        let pages = h.pages();
        set_text(pages[PAGE_HISTORY], IDC_HIS_MAX, "");
        set_text(pages[PAGE_HOTKEY], IDC_HK_TIP_CHARS, "99999999999");
        let issues = h.read(&base).unwrap_err();
        let fields: Vec<&str> = issues.iter().map(|i| i.field).collect();
        assert_eq!(fields, ["history.max", "hotkey.tooltip.max_chars"]);
        assert!(issues[0].message.contains("「最大件数」に数値を入力してください"), "{:?}", issues[0]);

        let mut bad = Config::default();
        bad.history.max = u32::MAX;
        bad.history.grouping.visible_items = 0;
        bad.history.grouping.folders = 0;
        bad.history.grouping.items_per_folder = 0;
        bad.history.add_interval_ms = 0;
        bad.hotkey.menu_max_items = 0;
        bad.hotkey.tooltip.delay_ms = u32::MAX;
        bad.hotkey.tooltip.max_chars = 0;
        bad.hotkey.tooltip.max_lines = 0;
        bad.tools.text.word_break_width = 0;
        bad.hotkey.popup_menu.enabled = true;
        bad.hotkey.popup_menu.key = String::new();
        bad.format_filters = vec![format("", FilterAction::Add, true, 0)];
        bad.window_filters = vec![window("", "", true)];
        bad.history.sound_on_add = true;
        bad.history.sound_file = r"\\server\share\a.wav".to_string();
        let issues = bad.validate();
        assert_eq!(issues.len(), 14, "前提: すべての項目が誤り");
        let limit = ["format_filters.limit_size"]; // 上限は画面の読み取りの誤り（read_draft）
        for field in issues.iter().map(|i| i.field).chain(NUMS.iter().map(|n| n.field)).chain(limit) {
            assert!(PLACES.iter().any(|p| p.field == field), "{field} の場所が無い");
        }
        // 行の誤りの場所は、行を編集するページの入力欄
        for issue in issues.iter().filter(|i| i.row.is_some()) {
            let place = PLACES.iter().find(|p| p.field == issue.field).unwrap();
            let rl = row_list(place.page).expect("行の誤りが一覧の無いページを指す");
            assert!(rl.fields.contains(&place.ctl), "{} の欄が行の入力欄でない", issue.field);
        }
    }

    /// OK: `on_ok` に開いたときの設定と画面の設定が渡り、成功なら（投稿の後に）1回だけ閉じる。
    #[test]
    fn ok_passes_base_and_draft_and_closes_once_on_success() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let base = Config::default();
        let h = Harness::open(base.clone(), || Ok(()));
        let pages = h.pages();
        set_check(pages[PAGE_GENERAL], IDC_GEN_NOTIFY, !base.general.notify_action_errors);
        h.command(IDOK.0);
        assert_eq!(h.calls.borrow().len(), 1);
        let (b, d) = h.calls.borrow()[0].clone();
        assert_eq!(b, toml::to_string(&base).unwrap());
        let draft: Config = toml::from_str(&d).unwrap();
        assert_eq!(draft.general.notify_action_errors, !base.general.notify_action_errors);
        assert!(h.is_open(), "投稿の前に閉じた");
        pump(100);
        assert_eq!(h.closed.get(), 1);
        assert!(!h.is_open());
        assert!(active_dialog().is_none(), "閉じた後も今の設定画面として残っている");
    }

    /// 反映の誤り: 保存できない・今は反映できない・入力の誤りは、窓の中に出して開いたまま。
    #[test]
    fn ok_errors_are_shown_and_window_stays_open() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let cases: [(fn() -> Result<(), ApplyError>, &str); 3] = [
            (|| Err(ApplyError::Save("ふさがっている".into())), "設定を保存できませんでした: ふさがっている"),
            (|| Err(ApplyError::Busy), "今は反映できません"),
            (
                || Err(ApplyError::Invalid(vec![ConfigIssue { field: "history.max", row: None, message: "範囲の外".into() }])),
                "範囲の外",
            ),
        ];
        for (result, expected) in cases {
            let h = Harness::open(Config::default(), result);
            h.command(IDOK.0);
            pump(50);
            assert!(h.is_open(), "{expected}: 閉じた");
            assert!(get_text(h.dialog, IDC_SET_ERROR).contains(expected), "{expected}: {}", get_text(h.dialog, IDC_SET_ERROR));
        }
    }

    /// 無効（灰色）の欄の誤りは、そのタブを出して、有効にするチェックボックスへフォーカスを移し、直し方を書き添える。
    /// `on_ok` は呼ばない。
    #[test]
    fn issue_on_disabled_field_focuses_its_enabler() {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetFocus;
        let _gui = crate::tray::lock_gui_resource_tests();
        let mut base = Config::default();
        base.history.grouping.enabled = false;
        let h = Harness::open(base, || Ok(()));
        unsafe {
            let _ = SetForegroundWindow(h.dialog);
        }
        let pages = h.pages();
        set_text(pages[PAGE_HISTORY], IDC_HIS_VISIBLE, "");
        h.command(IDOK.0);
        assert!(h.calls.borrow().is_empty(), "読めない入力なのに反映を呼んだ");
        let tab = item(h.dialog, IDC_SET_TAB).unwrap();
        assert_eq!(unsafe { SendMessageW(tab, TCM_GETCURSEL, None, None) }.0, PAGE_HISTORY as isize);
        assert!(unsafe { IsWindowVisible(pages[PAGE_HISTORY]) }.as_bool());
        let focus = unsafe { GetFocus() };
        assert_eq!(focus, item(pages[PAGE_HISTORY], IDC_HIS_GROUP).unwrap(), "有効にするチェックボックスへ移っていない");
        assert!(get_text(h.dialog, IDC_SET_ERROR).contains("をオンにすると直せます"));
    }

    /// 誤りが複数あるときは、最初の誤りの説明と残りの件数を出す。いちばん長くなりうる説明（無効な欄の範囲外の値に
    /// 直し方を添え、残りの件数も付く）でも、誤りの欄（固定の高さ）に収まる。
    #[test]
    fn several_issues_show_first_and_count_and_fit_the_error_area() {
        use windows::Win32::Graphics::Gdi::{DrawTextW, GetDC, ReleaseDC, SelectObject, DT_CALCRECT, DT_WORDBREAK, HFONT};
        use windows::Win32::UI::WindowsAndMessaging::WM_GETFONT;
        let _gui = crate::tray::lock_gui_resource_tests();
        let mut base = Config::default();
        base.hotkey.tooltip.enabled = false;
        let h = Harness::open(base, || Ok(()));
        let pages = h.pages();
        set_text(pages[PAGE_HISTORY], IDC_HIS_MAX, "");
        set_text(pages[PAGE_HISTORY], IDC_HIS_INTERVAL, "");
        h.command(IDOK.0);
        let shown = get_text(h.dialog, IDC_SET_ERROR);
        assert!(shown.contains("「最大件数」に数値を入力してください"), "{shown}");
        assert!(shown.contains("ほかに 1 件"), "{shown}");
        assert!(!shown.contains("追加までの遅延"), "2件目まで並べた: {shown}");

        let longest = vec![
            ConfigIssue {
                field: "hotkey.tooltip.delay_ms",
                row: None,
                message: format!("「表示までの待ち時間」は 0 から 5000 の範囲で入力してください（今の値は {}）", u32::MAX),
            };
            11
        ];
        let text = issue_text(&pages, &longest);
        assert!(text.contains("をオンにすると直せます") && text.contains("ほかに 10 件"), "{text}");
        let area = item(h.dialog, IDC_SET_ERROR).unwrap();
        let mut client = RECT::default();
        let mut need = RECT::default();
        unsafe {
            let _ = GetClientRect(area, &mut client);
            need.right = client.right;
            let font = HFONT(SendMessageW(area, WM_GETFONT, None, None).0 as *mut _);
            let dc = GetDC(Some(area));
            let old = SelectObject(dc, font.into());
            let mut w: Vec<u16> = text.encode_utf16().collect();
            DrawTextW(dc, &mut w, &mut need, DT_CALCRECT | DT_WORDBREAK);
            SelectObject(dc, old);
            ReleaseDC(Some(area), dc);
        }
        assert!(need.bottom <= client.bottom, "誤りの欄に収まらない: 要る高さ {} > 欄 {}", need.bottom, client.bottom);
    }

    /// OK の処理中（反映の中の再入）に届いた閉じる要求は記録だけで、OK が戻ってから1回だけ閉じる。二重の閉じる要求も
    /// 1回だけ。
    #[test]
    fn close_during_ok_is_deferred_and_closes_once() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let dialog_cell: Rc<Cell<isize>> = Rc::default();
        let inner = Rc::clone(&dialog_cell);
        let h = Harness::open(Config::default(), move || {
            let dialog = HWND(inner.get() as *mut _);
            unsafe {
                // 反映の中の再入に当たる: キャンセルを2回、×を1回
                SendMessageW(dialog, WM_COMMAND, Some(WPARAM(IDCANCEL.0 as usize)), Some(LPARAM(0)));
                SendMessageW(dialog, WM_COMMAND, Some(WPARAM(IDCANCEL.0 as usize)), Some(LPARAM(0)));
                SendMessageW(dialog, WM_CLOSE, None, None);
                assert!(ctx_ref(dialog).unwrap().close_pending.get(), "処理中の閉じる要求を記録していない");
                assert!(!ctx_ref(dialog).unwrap().closing.get(), "処理中に閉じる要求を投稿した");
            }
            Err(ApplyError::Busy)
        });
        dialog_cell.set(h.dialog.0 as isize);
        h.command(IDOK.0);
        assert!(!unsafe { ctx_ref(h.dialog) }.unwrap().ok_running.get(), "OK の処理中の印が残っている");
        pump(100);
        assert_eq!(h.closed.get(), 1);
        assert!(!h.is_open());
    }

    /// OK の処理中に、閉じる要求のメッセージ（`WM_APP_SETTINGS_CLOSE`）を直接送られても（ほかのプロセスからの送信）、
    /// 窓とコンテキストを破棄しない。OK が戻ってから（成功でも失敗でも）1回だけ閉じる。
    #[test]
    fn close_message_sent_during_ok_does_not_destroy_window() {
        let _gui = crate::tray::lock_gui_resource_tests();
        for succeed in [true, false] {
            let dialog_cell: Rc<Cell<isize>> = Rc::default();
            let inner = Rc::clone(&dialog_cell);
            let h = Harness::open(Config::default(), move || {
                let dialog = HWND(inner.get() as *mut _);
                unsafe {
                    SendMessageW(dialog, WM_APP_SETTINGS_CLOSE, None, None);
                    SendMessageW(dialog, WM_APP_SETTINGS_CLOSE, None, None);
                    assert!(IsWindow(Some(dialog)).as_bool(), "OK の処理中に窓を破棄した");
                    let ctx = ctx_ref(dialog).expect("OK の処理中にコンテキストを解放した");
                    assert!(ctx.close_pending.get() && !ctx.closing.get());
                }
                if succeed { Ok(()) } else { Err(ApplyError::Busy) }
            });
            dialog_cell.set(h.dialog.0 as isize);
            h.command(IDOK.0);
            assert!(h.is_open(), "OK が戻った時点で閉じた（閉じるのは投稿の後）");
            pump(100);
            assert_eq!(h.closed.get(), 1, "succeed={succeed}");
            assert!(!h.is_open());
        }
    }

    /// 閉じる要求の投稿に失敗したら印を戻し、もう一度閉じられる。
    #[test]
    fn failed_close_post_can_be_retried() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let h = Harness::open(Config::default(), || Ok(()));
        FAIL_CLOSE_POST.with(|f| f.set(true));
        h.command(IDCANCEL.0);
        FAIL_CLOSE_POST.with(|f| f.set(false));
        assert!(!unsafe { ctx_ref(h.dialog) }.unwrap().closing.get(), "失敗したのに投稿済みのまま");
        pump(50);
        assert_eq!(h.closed.get(), 0);
        h.command(IDCANCEL.0);
        pump(100);
        assert_eq!(h.closed.get(), 1);
    }

    /// 終了の要求の後（`end`）: 隠れ、表示の要求・OK・閉じる要求では何もしない。破棄は持ち主が捨てたとき。
    #[test]
    fn ended_window_stays_hidden_and_ignores_requests() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let h = Harness::open(Config::default(), || Ok(()));
        h.slot.borrow().as_ref().unwrap().end();
        assert!(!unsafe { IsWindowVisible(h.dialog) }.as_bool());
        h.slot.borrow().as_ref().unwrap().show();
        assert!(!unsafe { IsWindowVisible(h.dialog) }.as_bool(), "終了の後に出した");
        h.command(IDOK.0);
        h.command(IDCANCEL.0);
        pump(100);
        assert!(h.calls.borrow().is_empty() && h.closed.get() == 0);
        assert!(h.is_open(), "持ち主が捨てる前に破棄した");
        let window = h.slot.borrow_mut().take();
        drop(window);
        assert!(!unsafe { IsWindow(Some(h.dialog)) }.as_bool());
    }

    /// 機能が無効な間の欄は灰色。チェックを入れると入力できる。保持総数は数値の変化で書き換わる。
    #[test]
    fn enablers_toggle_fields_and_total_follows_numbers() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let mut base = Config::default();
        base.history.grouping.enabled = false;
        base.hotkey.tooltip.enabled = false;
        base.tools.text.convert_date_on_send = false;
        let h = Harness::open(base, || Ok(()));
        let pages = h.pages();
        let enabled = |page: usize, id: i32| unsafe { IsWindowEnabled(item(pages[page], id).unwrap()) }.as_bool();
        let click = |page: usize, id: i32, on: bool| {
            set_check(pages[page], id, on);
            unsafe {
                SendMessageW(
                    pages[page],
                    WM_COMMAND,
                    Some(WPARAM(((BN_CLICKED as usize) << 16) | id as usize)),
                    Some(LPARAM(item(pages[page], id).unwrap().0 as isize)),
                );
            }
        };
        assert!(!enabled(PAGE_HISTORY, IDC_HIS_VISIBLE) && !enabled(PAGE_HOTKEY, IDC_HK_TIP_LINES) && !enabled(PAGE_TEXT, IDC_TX_DATE_FMT));
        click(PAGE_HISTORY, IDC_HIS_GROUP, true);
        click(PAGE_HOTKEY, IDC_HK_TOOLTIP, true);
        click(PAGE_TEXT, IDC_TX_DATE, true);
        assert!(enabled(PAGE_HISTORY, IDC_HIS_FORMAT) && enabled(PAGE_HOTKEY, IDC_HK_TIP_LINES) && enabled(PAGE_TEXT, IDC_TX_TIME_FMT));
        click(PAGE_HOTKEY, IDC_HK_POPUP, false);
        assert!(!enabled(PAGE_HOTKEY, IDC_HK_KEY) && !enabled(PAGE_HOTKEY, IDC_HK_WIN));

        set_text(pages[PAGE_HISTORY], IDC_HIS_VISIBLE, "5");
        set_text(pages[PAGE_HISTORY], IDC_HIS_FOLDERS, "2");
        set_text(pages[PAGE_HISTORY], IDC_HIS_PER_FOLDER, "10");
        assert!(get_text(pages[PAGE_HISTORY], IDC_HIS_TOTAL).contains("25件"), "{}", get_text(pages[PAGE_HISTORY], IDC_HIS_TOTAL));
        set_text(pages[PAGE_HISTORY], IDC_HIS_FOLDERS, "");
        assert!(get_text(pages[PAGE_HISTORY], IDC_HIS_TOTAL).contains("—"));
    }

    /// はみ出し: 全ページと親のダイアログで、文字の幅（`&` を除く。チェックボックスは箱の分を引く）が枠の幅を超えず、
    /// 各コントロールの枠が親のクライアント領域の中にある。ページはタブの表示領域の中にある（テストで作る窓は
    /// そのときのモニターの DPI。ほかの倍率は実機で確かめる）。
    #[test]
    fn every_control_fits_without_overflow() {
        use windows::Win32::Foundation::SIZE;
        use windows::Win32::Graphics::Gdi::{GetDC, GetTextExtentPoint32W, ReleaseDC, SelectObject, HFONT};
        use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
        use windows::Win32::UI::WindowsAndMessaging::{BS_AUTOCHECKBOX, SM_CXMENUCHECK, WM_GETFONT};
        let _gui = crate::tray::lock_gui_resource_tests();
        let h = Harness::open(Config::default(), || Ok(()));
        let mut problems = Vec::new();
        let mut parents = vec![h.dialog];
        parents.extend(h.pages());
        for parent in parents {
            let mut client = RECT::default();
            unsafe {
                let _ = GetClientRect(parent, &mut client);
            }
            for c in children(parent) {
                let class = class_of(c);
                if class == "#32770" {
                    continue; // ページ（下で確かめる）
                }
                // 枠が親のクライアント領域の中にある
                let mut rc = RECT::default();
                unsafe {
                    let _ = GetWindowRect(c, &mut rc);
                }
                let mut pts = [POINT { x: rc.left, y: rc.top }, POINT { x: rc.right, y: rc.bottom }];
                unsafe { MapWindowPoints(None, Some(parent), &mut pts) };
                if pts[0].x < 0 || pts[0].y < 0 || pts[1].x > client.right || pts[1].y > client.bottom {
                    problems.push(format!("{} id={}: 枠が親の外 {:?} / {:?}", class, unsafe { windows::Win32::UI::WindowsAndMessaging::GetDlgCtrlID(c) }, pts, client));
                }
                // ドロップダウンの一覧の項目は、閉じたときの欄（矢印のボタンを除いた幅）に収まる
                if class == "ComboBox" {
                    use windows::Win32::UI::WindowsAndMessaging::{CB_GETCOUNT, CB_GETLBTEXT, CB_GETLBTEXTLEN, SM_CXVSCROLL};
                    let mut cc = RECT::default();
                    unsafe {
                        let _ = GetClientRect(c, &mut cc);
                    }
                    let dpi = unsafe { GetDpiForWindow(c) };
                    let room = cc.right - unsafe { GetSystemMetricsForDpi(SM_CXVSCROLL, dpi) } - 4 * dpi as i32 / 96;
                    let count = unsafe { SendMessageW(c, CB_GETCOUNT, None, None) }.0;
                    for i in 0..count.max(0) as usize {
                        let len = unsafe { SendMessageW(c, CB_GETLBTEXTLEN, Some(WPARAM(i)), None) }.0.max(0) as usize;
                        let mut buf = vec![0u16; len + 1];
                        unsafe { SendMessageW(c, CB_GETLBTEXT, Some(WPARAM(i)), Some(LPARAM(buf.as_mut_ptr() as isize))) };
                        let mut size = SIZE::default();
                        unsafe {
                            let font = HFONT(SendMessageW(c, WM_GETFONT, None, None).0 as *mut _);
                            let dc = GetDC(Some(c));
                            let old = SelectObject(dc, font.into());
                            let _ = GetTextExtentPoint32W(dc, &buf[..len], &mut size);
                            SelectObject(dc, old);
                            ReleaseDC(Some(c), dc);
                        }
                        if size.cx > room {
                            problems.push(format!("{:?}: 文字 {} > 欄 {}", String::from_utf16_lossy(&buf[..len]), size.cx, room));
                        }
                    }
                    continue;
                }
                if class != "Static" && class != "Button" {
                    continue;
                }
                let text = {
                    let len = unsafe { GetWindowTextLengthW(c) } as usize;
                    let mut buf = vec![0u16; len + 1];
                    let n = unsafe { GetWindowTextW(c, &mut buf) } as usize;
                    String::from_utf16_lossy(&buf[..n]).replace('&', "")
                };
                if text.is_empty() {
                    continue;
                }
                let mut cc = RECT::default();
                let mut size = SIZE::default();
                unsafe {
                    let _ = GetClientRect(c, &mut cc);
                    let font = HFONT(SendMessageW(c, WM_GETFONT, None, None).0 as *mut _);
                    let dc = GetDC(Some(c));
                    let old = SelectObject(dc, font.into());
                    let w: Vec<u16> = text.encode_utf16().collect();
                    let _ = GetTextExtentPoint32W(dc, &w, &mut size);
                    SelectObject(dc, old);
                    ReleaseDC(Some(c), dc);
                }
                let style = unsafe { GetWindowLongW(c, GWL_STYLE) } as u32;
                let is_check = class == "Button" && (style & 0xF) == BS_AUTOCHECKBOX as u32;
                let dpi = unsafe { GetDpiForWindow(c) };
                let box_w = if is_check { (unsafe { GetSystemMetricsForDpi(SM_CXMENUCHECK, dpi) }) + 4 * dpi as i32 / 96 } else { 0 };
                if size.cx > cc.right - box_w {
                    problems.push(format!("{text:?}: 文字 {} > 枠 {}", size.cx, cc.right - box_w));
                }
            }
        }
        // ページはタブの表示領域（`TCM_ADJUSTRECT`）の中
        let tab = item(h.dialog, IDC_SET_TAB).unwrap();
        let mut trc = RECT::default();
        unsafe {
            let _ = GetWindowRect(tab, &mut trc);
            SendMessageW(tab, TCM_ADJUSTRECT, Some(WPARAM(0)), Some(LPARAM(&mut trc as *mut RECT as isize)));
        }
        for page in h.pages() {
            let mut prc = RECT::default();
            unsafe {
                let _ = GetWindowRect(page, &mut prc);
            }
            if prc.left < trc.left || prc.top < trc.top || prc.right > trc.right || prc.bottom > trc.bottom {
                problems.push(format!("ページがタブの表示領域の外 {prc:?} / {trc:?}"));
            }
        }
        // タブの見出しは1列で、最後の見出しがタブの幅の中（収まらないと横に矢印が出る）
        {
            use windows::Win32::UI::Controls::{TCM_GETITEMRECT, TCM_GETROWCOUNT};
            let rows = unsafe { SendMessageW(tab, TCM_GETROWCOUNT, None, None) }.0;
            let mut last = RECT::default();
            let mut client = RECT::default();
            unsafe {
                SendMessageW(tab, TCM_GETITEMRECT, Some(WPARAM(PAGES.len() - 1)), Some(LPARAM(&mut last as *mut RECT as isize)));
                let _ = GetClientRect(tab, &mut client);
            }
            if rows != 1 || last.right > client.right {
                problems.push(format!("タブの見出しが収まらない: 列 {rows}、最後の見出しの右 {} > {}", last.right, client.right));
            }
        }
        // 一覧の列見出しの文字（見出しから読んだ実際の文字）が期待どおりで、列の幅に収まる（列の中身は利用者の
        // データなので対象外）
        let expected_titles: [&[&str]; 2] = [
            &["形式名", "動作", "保存", "上限（バイト）"],
            &["タイトル（部分一致）", "クラス名（完全一致）", "除外"],
        ];
        for (rl, expected) in ROW_LISTS.iter().zip(expected_titles) {
            use windows::Win32::UI::Controls::{HDITEMW, HDI_TEXT, HDM_GETITEMCOUNT, HDM_GETITEMRECT, HDM_GETITEMW, LVM_GETHEADER};
            let list = item(h.pages()[rl.page], rl.list).unwrap();
            let header = HWND(unsafe { SendMessageW(list, LVM_GETHEADER, None, None) }.0 as *mut _);
            let dpi = unsafe { GetDpiForWindow(list) } as i32;
            let count = unsafe { SendMessageW(header, HDM_GETITEMCOUNT, None, None) }.0 as usize;
            let titles: Vec<String> = (0..count)
                .map(|i| {
                    let mut buf = [0u16; 64];
                    let mut hd = HDITEMW { mask: HDI_TEXT, pszText: PWSTR(buf.as_mut_ptr()), cchTextMax: buf.len() as i32, ..Default::default() };
                    unsafe {
                        SendMessageW(header, HDM_GETITEMW, Some(WPARAM(i)), Some(LPARAM(&mut hd as *mut HDITEMW as isize)));
                    }
                    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                    String::from_utf16_lossy(&buf[..len])
                })
                .collect();
            if titles != expected {
                problems.push(format!("列見出しが違う: {titles:?} / {expected:?}"));
            }
            for (i, title) in titles.iter().enumerate() {
                let mut rc = RECT::default();
                let mut size = SIZE::default();
                unsafe {
                    SendMessageW(header, HDM_GETITEMRECT, Some(WPARAM(i)), Some(LPARAM(&mut rc as *mut RECT as isize)));
                    let font = HFONT(SendMessageW(header, WM_GETFONT, None, None).0 as *mut _);
                    let dc = GetDC(Some(header));
                    let old = SelectObject(dc, font.into());
                    let w: Vec<u16> = title.encode_utf16().collect();
                    let _ = GetTextExtentPoint32W(dc, &w, &mut size);
                    SelectObject(dc, old);
                    ReleaseDC(Some(header), dc);
                }
                // 見出しの左右の余白（おおよそ 6px ずつ）
                if size.cx + 12 * dpi / 96 > rc.right - rc.left {
                    problems.push(format!("列見出し {title:?}: 文字 {} + 余白 > 列 {}", size.cx, rc.right - rc.left));
                }
            }
        }
        assert!(problems.is_empty(), "はみ出し:\n{}", problems.join("\n"));
    }

    /// 追加・入力欄の変更・削除を実際のメッセージで行い、行の値（画面の中の正）・一覧・選択・入力欄・フォーカスを
    /// 確かめる。
    #[test]
    fn add_edit_delete_rows_through_messages() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, IsWindowEnabled};
        let _gui = crate::tray::lock_gui_resource_tests();
        let base = unusual();
        let h = Harness::open(base.clone(), || Ok(()));
        unsafe {
            let _ = SetForegroundWindow(h.dialog);
        }
        select_page(h.dialog, PAGE_FORMAT);
        let page = h.pages()[PAGE_FORMAT];
        let list = h.list(PAGE_FORMAT);
        let enabled = |id: i32| unsafe { IsWindowEnabled(item(page, id).unwrap()) }.as_bool();
        assert_eq!(selected_row(list), None);
        assert!(!enabled(IDC_FMT_NAME) && !enabled(IDC_FMT_DELETE), "行を選んでいないのに入力できる");

        // 追加: 既定の行が最後に入って選ばれ、形式名の欄へ
        h.click(PAGE_FORMAT, IDC_FMT_ADD);
        assert_eq!(h.ctx().formats.borrow().len(), 4);
        assert_eq!(h.ctx().formats.borrow()[3], FormatRow::new());
        assert_eq!((row_total(list), selected_row(list)), (4, Some(3)));
        assert!(enabled(IDC_FMT_NAME) && enabled(IDC_FMT_DELETE));
        assert_eq!(unsafe { GetFocus() }, item(page, IDC_FMT_NAME).unwrap());
        assert_eq!((get_text(page, IDC_FMT_NAME), get_text(page, IDC_FMT_LIMIT)), (String::new(), "0".to_string()));
        assert!(get_check(page, IDC_FMT_SAVE));

        // 入力欄の変更が行と一覧のセルへ入る
        set_text(page, IDC_FMT_NAME, "HTML Format");
        set_text(page, IDC_FMT_LIMIT, "2048");
        if let Some(combo) = item(page, IDC_FMT_ACTION) {
            unsafe {
                SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(1)), None);
                SendMessageW(page, WM_COMMAND, Some(WPARAM(((CBN_SELCHANGE as usize) << 16) | IDC_FMT_ACTION as usize)), Some(LPARAM(combo.0 as isize)));
            }
        }
        set_check(page, IDC_FMT_SAVE, false);
        h.click(PAGE_FORMAT, IDC_FMT_SAVE);
        assert_eq!(
            h.ctx().formats.borrow()[3],
            FormatRow { name: "HTML Format".into(), action: FilterAction::Ignore, save: false, limit_text: "2048".into() }
        );
        assert_eq!((0..4).map(|s| cell(list, 3, s)).collect::<Vec<_>>(), ["HTML Format", "無視", "しない", "2048"]);
        assert_eq!(get_text(page, IDC_FMT_LIMIT_SIZE), "= 2.0 KB");

        // 真ん中の行を消すと、同じ位置（元の3行目）が選ばれ、フォーカスは一覧へ
        select_row(list, Some(1));
        h.click(PAGE_FORMAT, IDC_FMT_DELETE);
        let names: Vec<String> = h.ctx().formats.borrow().iter().map(|r| r.name.clone()).collect();
        assert_eq!(names, ["CF_UNICODETEXT", "CF_DIB", "HTML Format"]);
        assert_eq!((row_total(list), selected_row(list)), (3, Some(1)));
        assert_eq!(get_text(page, IDC_FMT_NAME), "CF_DIB", "入力欄が選んだ行と合わない");
        assert_eq!(unsafe { GetFocus() }, list);
        // 最後の行を消すと、新しい最後が選ばれる
        select_row(list, Some(2));
        h.click(PAGE_FORMAT, IDC_FMT_DELETE);
        assert_eq!((row_total(list), selected_row(list)), (2, Some(1)));
        assert_eq!(get_text(page, IDC_FMT_NAME), "CF_DIB");
        // 全部消すと、入力欄と削除は灰色で空
        h.click(PAGE_FORMAT, IDC_FMT_DELETE);
        h.click(PAGE_FORMAT, IDC_FMT_DELETE);
        assert!(h.ctx().formats.borrow().is_empty());
        assert_eq!((row_total(list), selected_row(list)), (0, None));
        assert!(!enabled(IDC_FMT_NAME) && !enabled(IDC_FMT_DELETE));
        assert_eq!(get_text(page, IDC_FMT_NAME), "");
        let draft = h.read(&base).unwrap();
        assert!(draft.format_filters.is_empty());

        // ウィンドウフィルタ: 追加の既定は 空・空・除外
        let wpage = h.pages()[PAGE_WINDOW];
        let wlist = h.list(PAGE_WINDOW);
        h.click(PAGE_WINDOW, IDC_WIN_ADD);
        assert_eq!(h.ctx().windows.borrow().last().cloned().map(|w| (w.title, w.class_name, w.ignore)), Some((String::new(), String::new(), true)));
        assert_eq!(selected_row(wlist), Some(3));
        set_text(wpage, IDC_WIN_CLASS, "CabinetWClass");
        assert_eq!(h.ctx().windows.borrow()[3].class_name, "CabinetWClass");
        assert_eq!(cell(wlist, 3, 1), "CabinetWClass");
        assert_eq!(cell(wlist, 3, 2), "する");
    }

    /// 行を選び替えると、入力欄に新しい行の値が入り、前の行も新しい行も書き換わらない（選択の知らせの中で入力欄へ
    /// 書く間の変化を行へ戻さない）。
    #[test]
    fn switching_rows_does_not_write_into_other_rows() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let base = unusual();
        let h = Harness::open(base.clone(), || Ok(()));
        let page = h.pages()[PAGE_FORMAT];
        let list = h.list(PAGE_FORMAT);
        select_row(list, Some(0));
        set_text(page, IDC_FMT_NAME, "A2");
        select_row(list, Some(1));
        assert_eq!(get_text(page, IDC_FMT_NAME), "独自の形式");
        assert_eq!(get_text(page, IDC_FMT_LIMIT), "1048576");
        assert_eq!(get_text(page, IDC_FMT_LIMIT_SIZE), "= 1.0 MB");
        assert!(!get_check(page, IDC_FMT_SAVE));
        select_row(list, Some(2));
        let rows = h.ctx().formats.borrow().clone();
        assert_eq!(rows[0].name, "A2");
        assert_eq!(rows[1], FormatRow::from_filter(&base.format_filters[1]));
        assert_eq!(rows[2], FormatRow::from_filter(&base.format_filters[2]));
        assert_eq!(get_text(page, IDC_FMT_LIMIT), i64::MAX.to_string());

        let wpage = h.pages()[PAGE_WINDOW];
        let wlist = h.list(PAGE_WINDOW);
        select_row(wlist, Some(0));
        set_text(wpage, IDC_WIN_TITLE, "メモ帳2");
        select_row(wlist, Some(1));
        assert_eq!((get_text(wpage, IDC_WIN_TITLE), get_text(wpage, IDC_WIN_CLASS)), (String::new(), "Notepad".to_string()));
        assert!(!get_check(wpage, IDC_WIN_IGNORE));
        let rows = h.ctx().windows.borrow().clone();
        assert_eq!(rows[0].title, "メモ帳2");
        assert_eq!((rows[1].title.as_str(), rows[1].class_name.as_str(), rows[1].ignore), ("", "Notepad", false));
        // 選択を外すと、入力欄は空で灰色
        select_row(wlist, None);
        assert_eq!(get_text(wpage, IDC_WIN_CLASS), "");
    }

    /// 行の誤り（`Config::validate` の空の形式名・重複・両方空のウィンドウ行、画面の読めない上限）は、そのタブを出し、
    /// その行を選んで、その欄へフォーカスする。
    #[test]
    fn row_issues_select_the_row_and_focus_the_field() {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetFocus;
        let _gui = crate::tray::lock_gui_resource_tests();
        let mut base = Config::default();
        base.format_filters.push(format("", FilterAction::Add, true, 0));
        base.window_filters = vec![window("a", "", true), window("", "", true)];
        let h = Harness::open(base.clone(), || Ok(()));
        unsafe {
            let _ = SetForegroundWindow(h.dialog);
        }
        let pages = h.pages();
        let tab = item(h.dialog, IDC_SET_TAB).unwrap();
        let current = || unsafe { SendMessageW(tab, TCM_GETCURSEL, None, None) }.0 as usize;

        let issues = base.validate();
        assert_eq!(issues.len(), 2, "{issues:?}");
        show_issues(h.dialog, &pages, &issues);
        assert_eq!(current(), PAGE_FORMAT);
        assert_eq!(selected_row(h.list(PAGE_FORMAT)), Some(3));
        assert_eq!(unsafe { GetFocus() }, item(pages[PAGE_FORMAT], IDC_FMT_NAME).unwrap());
        assert!(get_text(h.dialog, IDC_SET_ERROR).contains("形式フィルタの 4 行目: 形式名を入力してください"));
        assert!(get_text(h.dialog, IDC_SET_ERROR).contains("ほかに 1 件"));

        show_issues(h.dialog, &pages, &issues[1..]);
        assert_eq!(current(), PAGE_WINDOW);
        assert_eq!(selected_row(h.list(PAGE_WINDOW)), Some(1));
        assert_eq!(unsafe { GetFocus() }, item(pages[PAGE_WINDOW], IDC_WIN_TITLE).unwrap());

        // 読めない上限は OK で（画面の読み取りの誤り。反映は呼ばない）
        select_row(h.list(PAGE_FORMAT), Some(1));
        set_text(pages[PAGE_FORMAT], IDC_FMT_LIMIT, "");
        assert_eq!(get_text(pages[PAGE_FORMAT], IDC_FMT_LIMIT_SIZE), "");
        select_row(h.list(PAGE_FORMAT), Some(0));
        select_page(h.dialog, PAGE_GENERAL);
        h.command(IDOK.0);
        assert!(h.calls.borrow().is_empty(), "読めない上限なのに反映を呼んだ");
        assert_eq!(current(), PAGE_FORMAT);
        assert_eq!(selected_row(h.list(PAGE_FORMAT)), Some(1));
        assert_eq!(unsafe { GetFocus() }, item(pages[PAGE_FORMAT], IDC_FMT_LIMIT).unwrap());
        assert!(get_text(h.dialog, IDC_SET_ERROR).contains("形式フィルタの 2 行目: 「上限」に数値を入力してください"));
    }

    /// コピーの合計の上限: 開いたときの値と読みやすい大きさを出し、入力に合わせて表示を直す。読めない入力は OK で
    /// その欄の誤りにし（反映は呼ばない）、直せば編集した値で反映する。
    #[test]
    fn capture_total_limit_field_shows_size_and_is_read_on_ok() {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetFocus;
        let _gui = crate::tray::lock_gui_resource_tests();
        let mut base = Config::default();
        base.capture_total_limit = 1024 * 1024;
        let h = Harness::open(base, || Ok(()));
        unsafe {
            let _ = SetForegroundWindow(h.dialog);
        }
        let page = h.pages()[PAGE_FORMAT];
        assert_eq!(get_text(page, IDC_FMT_TOTAL), "1048576");
        assert_eq!(get_text(page, IDC_FMT_TOTAL_SIZE), "= 1.0 MB");
        set_text(page, IDC_FMT_TOTAL, "0");
        assert_eq!(get_text(page, IDC_FMT_TOTAL_SIZE), "（無制限）");

        set_text(page, IDC_FMT_TOTAL, "");
        assert_eq!(get_text(page, IDC_FMT_TOTAL_SIZE), "");
        select_page(h.dialog, PAGE_GENERAL);
        h.command(IDOK.0);
        assert!(h.calls.borrow().is_empty(), "読めない上限なのに反映を呼んだ");
        assert_eq!(unsafe { GetFocus() }, item(page, IDC_FMT_TOTAL).unwrap());
        assert!(get_text(h.dialog, IDC_SET_ERROR).contains("「コピーの合計の上限」に数値を入力してください"));

        set_text(page, IDC_FMT_TOTAL, "2048");
        h.command(IDOK.0);
        let calls = h.calls.borrow();
        assert_eq!(calls.len(), 1);
        let draft: Config = toml::from_str(&calls[0].1).unwrap();
        assert_eq!(draft.capture_total_limit, 2048);
    }

    /// 行の入力欄にフォーカスがあるときの Enter は OK。アクセスキーで一覧・入力欄へ移る（メッセージループと同じ
    /// `is_dialog_message` 経由）。
    #[test]
    fn enter_in_row_field_presses_ok_and_access_keys_reach_list_and_fields() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, VK_RETURN};
        use windows::Win32::UI::WindowsAndMessaging::{WM_CHAR, WM_SYSCHAR};
        let _gui = crate::tray::lock_gui_resource_tests();
        let base = unusual();
        let h = Harness::open(base.clone(), || Ok(()));
        unsafe {
            let _ = SetForegroundWindow(h.dialog);
        }
        select_page(h.dialog, PAGE_FORMAT);
        let page = h.pages()[PAGE_FORMAT];
        select_row(h.list(PAGE_FORMAT), Some(0));
        let alt = |ch: char| {
            let focus = unsafe { GetFocus() };
            let msg = MSG { hwnd: focus, message: WM_SYSCHAR, wParam: WPARAM(ch as usize), lParam: LPARAM(1 << 29), ..Default::default() };
            assert!(is_dialog_message(&msg), "Alt+{ch} を設定画面が処理しない");
        };
        unsafe {
            let _ = SetFocus(Some(item(h.dialog, IDC_SET_TAB).unwrap()));
        }
        alt('l');
        assert_eq!(unsafe { GetFocus() }, h.list(PAGE_FORMAT), "Alt+L で一覧へ移らない");
        alt('u');
        assert_eq!(unsafe { GetFocus() }, item(page, IDC_FMT_LIMIT).unwrap(), "Alt+U で上限へ移らない");
        alt('n');
        let name = item(page, IDC_FMT_NAME).unwrap();
        assert_eq!(unsafe { GetFocus() }, name, "Alt+N で形式名へ移らない");

        for message in [WM_KEYDOWN, WM_CHAR] {
            let msg = MSG { hwnd: name, message, wParam: WPARAM(if message == WM_KEYDOWN { VK_RETURN.0 as usize } else { 13 }), ..Default::default() };
            let _ = is_dialog_message(&msg);
        }
        assert_eq!(h.calls.borrow().len(), 1, "形式名の欄の Enter で OK にならない");
        pump(100);
        assert_eq!(h.closed.get(), 1);
    }

    /// 灰色の欄を指す見出しのアクセスキーを押しても、OK にならず（窓が閉じない）、チェックも変わらない（ユーザーの
    /// 実機の報告: 行を選んでいないときの Alt+N で設定画面が閉じた）。全ページで、次の欄が灰色の見出しをすべて試す。
    #[test]
    fn access_key_of_label_before_disabled_field_does_nothing() {
        use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
        use windows::Win32::UI::WindowsAndMessaging::{GetDlgCtrlID, WM_SYSCHAR};
        let _gui = crate::tray::lock_gui_resource_tests();
        let mut base = unusual();
        base.history.grouping.enabled = false;
        base.history.sound_on_add = false;
        base.hotkey.popup_menu.enabled = false;
        base.hotkey.tooltip.enabled = false;
        base.tools.text.convert_date_on_send = false;
        let h = Harness::open(base.clone(), || Ok(()));
        unsafe {
            let _ = SetForegroundWindow(h.dialog);
        }
        let pages = h.pages();
        let mut tried = Vec::new();
        for (index, &page) in pages.iter().enumerate() {
            select_page(h.dialog, index);
            let kids = children(page);
            for (i, &label) in kids.iter().enumerate() {
                let text = {
                    let len = unsafe { GetWindowTextLengthW(label) } as usize;
                    let mut buf = vec![0u16; len + 1];
                    let n = unsafe { GetWindowTextW(label, &mut buf) } as usize;
                    String::from_utf16_lossy(&buf[..n])
                };
                let Some(key) = text.split("(&").nth(1).and_then(|s| s.chars().next()) else {
                    continue;
                };
                let Some(&next) = kids.get(i + 1) else {
                    continue;
                };
                if class_of(label) != "Static" || unsafe { IsWindowEnabled(next) }.as_bool() {
                    continue;
                }
                let before = h.read(&base).map(|c| toml::to_string(&c).unwrap());
                unsafe {
                    let _ = SetFocus(Some(item(h.dialog, IDC_SET_TAB).unwrap()));
                }
                let msg = MSG {
                    hwnd: item(h.dialog, IDC_SET_TAB).unwrap(),
                    message: WM_SYSCHAR,
                    wParam: WPARAM(key.to_ascii_lowercase() as usize),
                    lParam: LPARAM(1 << 29),
                    ..Default::default()
                };
                let _ = is_dialog_message(&msg);
                let after = h.read(&base).map(|c| toml::to_string(&c).unwrap());
                tried.push(format!("ページ {index} Alt+{key}（{text}、次 id={}）", unsafe { GetDlgCtrlID(next) }));
                assert!(h.calls.borrow().is_empty(), "{}: OK になった", tried.last().unwrap());
                assert_eq!(before, after, "{}: 設定が変わった", tried.last().unwrap());
            }
        }
        pump(50);
        assert!(h.is_open() && h.closed.get() == 0, "閉じた: {tried:?}");
        assert!(tried.len() >= 10, "試した見出しが少ない: {tried:?}");
    }

    /// 列幅は、DPI が変わった後の置き直し（`WM_APP_PLACE_PAGES`）で、一覧の幅の比に入れ直す。
    #[test]
    fn columns_are_refit_after_placing_pages() {
        use windows::Win32::UI::Controls::LVM_GETCOLUMNWIDTH;
        let _gui = crate::tray::lock_gui_resource_tests();
        let h = Harness::open(unusual(), || Ok(()));
        for rl in &ROW_LISTS {
            let list = h.list(rl.page);
            let widths = || (0..rl.columns.len()).map(|i| unsafe { SendMessageW(list, LVM_GETCOLUMNWIDTH, Some(WPARAM(i)), None) }.0).collect::<Vec<_>>();
            let fitted = widths();
            let mut rc = RECT::default();
            unsafe {
                let _ = GetClientRect(list, &mut rc);
            }
            assert_eq!(fitted.iter().sum::<isize>(), (rc.right - rc.left) as isize, "列幅の合計が一覧の幅と違う");
            unsafe {
                SendMessageW(list, LVM_SETCOLUMNWIDTH, Some(WPARAM(0)), Some(LPARAM(10)));
            }
            assert_ne!(widths(), fitted);
            unsafe {
                SendMessageW(h.dialog, WM_APP_PLACE_PAGES, None, None);
            }
            assert_eq!(widths(), fitted, "置き直しで列幅を入れ直さない");
        }
    }

    #[test]
    fn readable_size_shows_units() {
        assert_eq!(readable_size("0"), "（無制限）");
        assert_eq!(readable_size("1023"), "= 1023 バイト");
        assert_eq!(readable_size("1024"), "= 1.0 KB");
        assert_eq!(readable_size("10485760"), "= 10.0 MB");
        assert_eq!(readable_size(" 3 221 225 472 "), "= 3.0 GB");
        assert_eq!(readable_size(&u64::MAX.to_string()), "= 16777216.0 TB");
        assert_eq!(readable_size(""), "");
        assert_eq!(readable_size("18446744073709551616"), "", "桁あふれ");
    }

    /// Tab で、各ページの入力できる項目を全部回れる（`WS_TABSTOP` の漏れが無い）。Ctrl+Tab・Ctrl+Shift+Tab でタブが替わる。
    #[test]
    fn tab_visits_every_field_and_ctrl_tab_switches_pages() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, GetKeyboardState, SetKeyboardState};
        let _gui = crate::tray::lock_gui_resource_tests();
        let mut base = unusual(); // 入力できる・できないのチェックを全部オン
        base.hotkey.popup_menu.enabled = true;
        base.hotkey.tooltip.enabled = true;
        let h = Harness::open(base, || Ok(()));
        unsafe {
            let _ = SetForegroundWindow(h.dialog);
        }
        let pages = h.pages();
        for (index, &page) in pages.iter().enumerate() {
            select_page(h.dialog, index);
            if let Some(rl) = row_list(index) {
                // 行を選ぶと、行の入力欄と削除のボタンも入力できる
                select_row(item(page, rl.list).unwrap(), Some(0));
            }
            let expected: Vec<HWND> = children(page)
                .into_iter()
                .filter(|&c| {
                    let style = unsafe { GetWindowLongW(c, GWL_STYLE) } as u32;
                    style & WS_TABSTOP.0 != 0 && style & WS_VISIBLE.0 != 0 && unsafe { IsWindowEnabled(c) }.as_bool()
                })
                .collect();
            unsafe {
                let _ = SetFocus(Some(expected[0]));
            }
            let mut seen = std::collections::HashSet::new();
            for _ in 0..(expected.len() + 6) * 2 {
                let focus = unsafe { GetFocus() };
                seen.insert(focus.0 as isize);
                let msg = MSG { hwnd: focus, message: WM_KEYDOWN, wParam: WPARAM(VK_TAB.0 as usize), ..Default::default() };
                assert!(is_dialog_message(&msg), "Tab を設定画面が処理しない");
            }
            for c in &expected {
                // コンボボックスはフォーカスが中の編集欄に移ることがあるので、子も含めて見る
                let hit = seen.contains(&(c.0 as isize)) || children(*c).iter().any(|k| seen.contains(&(k.0 as isize)));
                assert!(hit, "ページ {index}: id={} に Tab で届かない", unsafe { windows::Win32::UI::WindowsAndMessaging::GetDlgCtrlID(*c) });
            }
        }

        // Ctrl+Tab・Ctrl+Shift+Tab（キーの状態は、このスレッドのキーボードの状態で与え、後で戻す）
        select_page(h.dialog, 0);
        let tab = item(h.dialog, IDC_SET_TAB).unwrap();
        let mut saved = [0u8; 256];
        unsafe {
            let _ = GetKeyboardState(&mut saved);
        }
        let press = |shift: bool| {
            let mut state = saved;
            state[VK_CONTROL.0 as usize] = 0x80;
            state[VK_SHIFT.0 as usize] = if shift { 0x80 } else { 0 };
            unsafe {
                let _ = SetKeyboardState(&state);
            }
            let msg = MSG { hwnd: pages[0], message: WM_KEYDOWN, wParam: WPARAM(VK_TAB.0 as usize), ..Default::default() };
            let handled = pre_translate(&msg);
            unsafe {
                let _ = SetKeyboardState(&saved);
            }
            handled
        };
        let current = || unsafe { SendMessageW(tab, TCM_GETCURSEL, None, None) }.0;
        assert!(press(false));
        assert_eq!(current(), 1);
        assert!(press(true));
        assert!(press(true));
        assert_eq!(current(), PAGES.len() as isize - 1, "Ctrl+Shift+Tab で先頭から最後へ回らない");
        // Ctrl を押していなければ、ふつうの Tab（前処理しない）
        let msg = MSG { hwnd: pages[0], message: WM_KEYDOWN, wParam: WPARAM(VK_TAB.0 as usize), ..Default::default() };
        assert!(!pre_translate(&msg));
    }
}
