//! グローバルホットキー（RegisterHotKey）とポップアップメニュー。
//!
//! tray.rs / clipboard.rs と同じ隠しウィンドウ + メッセージループスレッド。
//! WM_HOTKEY を受けた瞬間（UIが動く前）にフォーカス状態を記録し、その場で
//! ネイティブ TrackPopupMenu を表示する。選択後の送出（フォーカス復帰・
//! クリップボード書き込み・自動ペースト）もこのスレッド上で完結させる
//! （send_pick。C版と同じ構造）。UI へは ShowViewer 要求だけを channel で渡す。
//!
//! メニューは C 版と同じくネイティブの TrackPopupMenu（自前描画は `menu_draw.rs`）。
//!
//! ペースト送出（Ctrl+V）の間はホットキーを一時解除する（貼り付け先アプリの
//! ショートカットと衝突しないため。C版と同じ手順）。

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use uuid::Uuid;
use windows::core::{w, Error as WinError, BOOL, PCWSTR, Result as WinResult};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetDoubleClickTime, RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT,
    MOD_CONTROL, MOD_SHIFT, MOD_WIN, VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_MENU,
    VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_SHIFT,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW,
    DestroyWindow, DispatchMessageW, EndMenu, GetMessageW, GetWindowLongPtrW,
    KillTimer, PeekMessageW, PostMessageW, PostQuitMessage, PostThreadMessageW, RegisterClassW,
    SetTimer, SetWindowLongPtrW, SetWindowsHookExW,
    TrackPopupMenu, TranslateMessage, UnhookWindowsHookEx, CREATESTRUCTW, CW_USEDEFAULT,
    GWLP_USERDATA, HMENU, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MENU_ITEM_FLAGS, MF_GRAYED, MSG, PM_NOREMOVE, WM_QUIT,
    TPM_LEFTALIGN, TPM_RETURNCMD, TPM_TOPALIGN, WH_KEYBOARD_LL, WM_CLOSE, WM_DESTROY, WM_DRAWITEM, WM_HOTKEY,
    WM_KEYUP, WM_MEASUREITEM, WM_MENUCHAR,
    WM_MENUSELECT, WM_NCCREATE, WM_SYSKEYUP, WM_TIMER, WM_USER, WNDCLASSW, WS_OVERLAPPED,
};

use crate::clipboard::ClipboardPort;
use crate::config::{Config, DoublePressAction, Hotkey};
use crate::data::EntryKind;
use crate::icons;
use crate::menu_draw::{self, escape_menu_text, MenuImage, PopupMenu};
use crate::menu_tooltip;
use crate::ops::Core;
use crate::paste::{capture_focus, cursor_pos, restore_focus, FocusInfo};
use crate::storage::EntryMeta;
use crate::store::{self, PinnedNode};

// --- Public events ---

pub enum HotkeyEvent {
    /// ビューアを表示する（二度押しアクション）
    ShowViewer,
    /// 設定の反映による登録し直し（`Hotkeys::reregister`）を行った。`generation` は依頼の番号、`problems` は
    /// できなかったことの説明（空なら全部できた）。メニューの表示中に重なった依頼は最後の1回だけ行うので、
    /// 番号は行った依頼のもの
    Reregistered { generation: u64, problems: Vec<String> },
}

// --- Messages / ids ---

const HOTKEY_ID_POPUP: i32 = 0x0300; // C版 HKEY_ID と同値（意味はないが由来として）
const WM_APP_REREGISTER: u32 = WM_USER + 3;
const WM_APP_SHUTDOWN: u32 = WM_USER + 4;
/// LLフックからの転送（wparam=正規化済みVK | フックの世代 << 32、lparam=キーが起きた時刻（ミリ秒）<< 1 | key up）。
/// C版のDLL→WM_KEY_HOOK転送と同じ構造をプロセス内で行う
const WM_APP_KEY_EVENT: u32 = WM_USER + 5;
/// トレイアイコンの左クリックからのポップアップメニュー表示依頼（tray.rsから）
const WM_APP_SHOW_POPUP_AT_MOUSE: u32 = WM_USER + 6;
const DOUBLE_PRESS_TIMER_ID: usize = 1;

const CLASS_NAME: PCWSTR = w!("CLCLR_Hotkey");

pub type Result<T> = std::result::Result<T, WinError>;

// --- Hotkey parsing ---

/// ホットキーのキー欄として妥当か（英数字1文字のみ）。`to_win32`の変換と
/// 設定の保存前の確かめ（`Config::validate`。不正な値だと知らせずに登録されない問題の修正）で共有する。
pub fn is_valid_hotkey_key(key: &str) -> bool {
    let mut chars = key.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return false;
    };
    c.is_ascii_alphanumeric()
}

/// 設定のHotkeyをRegisterHotKey引数へ変換する。キーは英数字1文字のみ対応
/// （それ以外・空文字はNone=登録しない）。
fn to_win32(hotkey: &Hotkey) -> Option<(HOT_KEY_MODIFIERS, u32)> {
    if !hotkey.enabled {
        return None;
    }
    let mut modifiers = HOT_KEY_MODIFIERS(0);
    for m in &hotkey.modifiers {
        modifiers |= match m.to_ascii_lowercase().as_str() {
            "alt" => MOD_ALT,
            "ctrl" => MOD_CONTROL,
            "shift" => MOD_SHIFT,
            "win" => MOD_WIN,
            _ => return None,
        };
    }
    if !is_valid_hotkey_key(&hotkey.key) {
        return None;
    }
    let c = hotkey.key.chars().next()?.to_ascii_uppercase();
    Some((modifiers, c as u32))
}

// --- Hotkey thread ---

/// ホットキー窓のコンテキスト。`GWLP_USERDATA`経由で`&HotkeyContext`（共有参照）として
/// 取り出す。`TrackPopupMenu`のモーダルループ中は`wndproc`が再入するため、可変な状態は
/// `Cell`に持ち、`&mut`の別名を作らない。加えて、ポップアップ処理中（`busy`）に届いた
/// 終了・再登録の依頼は保留し、処理が終わった最外周で実行する。窓の破棄でコンテキストが
/// 解放されるので、`show_popup_menu`が戻る前に解放しないための設計
/// （解放後の参照を防ぐ）。
struct HotkeyContext {
    events: Sender<HotkeyEvent>,
    config: Arc<RwLock<Config>>,
    core: Core,
    /// クリップボードを開く窓と、抑止する変更番号（`ClipboardWatcher::port`。抑止は
    /// `delete_on_send`設定と対）
    clipboard: Arc<ClipboardPort>,
    repaint: Box<dyn Fn() + Send>,
    /// ポップアップ処理中（メニュー表示から送出の完了まで）。多重表示の防止
    /// （表示中の再ホットキーを無視）と、終了・再登録の保留判定に使う
    menu_open: Cell<bool>,
    /// LLキーフックを張った専用のスレッド（二度押しアクション有効時のみ）。借用は張る・外す間だけ
    hook: RefCell<Option<HookThread>>,
    /// 今のフックの世代（張るたびに進める。古い世代の転送は捨てる）
    hook_generation: Cell<u32>,
    /// 二度押し判定: 監視中の修飾キー（0=無効状態）
    dp_vk: Cell<u32>,
    /// 二度押し判定: 対象修飾キーのkey up回数
    dp_count: Cell<u32>,
    /// 二度押し判定: 前に数えた key up が起きた時刻（ミリ秒、`KBDLLHOOKSTRUCT::time`）
    dp_last_up: Cell<u32>,
    /// ポップアップ処理中に届いた設定の再登録依頼の番号（0 は依頼なし。重なった依頼は最後の番号だけを
    /// 残し、処理後に1回だけ実行する）
    reregister_pending: Cell<u64>,
    /// ポップアップ処理中に届いた終了依頼（処理後に`DestroyWindow`）
    shutdown_pending: Cell<bool>,
}

/// `GWLP_USERDATA`のコンテキストを共有参照で取り出す。窓の破棄後（null）は`None`。
/// 返す参照は`WM_DESTROY`まで有効（ポップアップ処理中は解放されない）。
unsafe fn ctx_ref<'a>(hwnd: HWND) -> Option<&'a HotkeyContext> {
    unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const HotkeyContext).as_ref() }
}

/// `menu_open`を、スコープを抜ける（早期returnを含む）ときに必ず下ろす。
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

// --- Low-level keyboard hook (double press) ---

/// LLフックの転送先ウィンドウ（ホットキー窓。フックを外している間は 0）。フックプロシージャにはコンテキストを
/// 渡せないため static 経由で持つ（フックはこのプロセスで1つしか張らない）。
static HOOK_TARGET: AtomicIsize = AtomicIsize::new(0);
/// 今のフックの世代（転送に含め、ホットキー窓は今の世代と違う転送を捨てる。張り直しの前に投稿済みの古い転送を
/// 数えない）
static HOOK_GENERATION: AtomicU32 = AtomicU32::new(0);

/// 二度押しのフックを張った専用のスレッド（Microsoft Learn の LowLevelKeyboardProc: フックは
/// 張ったスレッドへのメッセージで呼ばれ、時間切れになると黙って外される。ホットキーのスレッドは送出などで止まるので、
/// 転送するだけのスレッドに張る）。破棄で止めて待つ。
struct HookThread {
    thread_id: u32,
    handle: Option<JoinHandle<()>>,
}

impl HookThread {
    /// スレッドを起こしてフックを張る。張れなければその説明（スレッドは終わっている）。
    fn spawn() -> std::result::Result<Self, String> {
        let (tx, rx) = std::sync::mpsc::channel::<std::result::Result<u32, String>>();
        let handle = thread::Builder::new()
            .name("clclr-kbhook".to_string())
            .spawn(move || unsafe {
                // 止める依頼（`PostThreadMessageW`）が失敗しないよう、フックを張る前にメッセージキューを作る
                let mut msg = MSG::default();
                let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
                let hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_hook_proc), None, 0) {
                    Ok(hook) => hook,
                    Err(e) => {
                        let _ = tx.send(Err(format!("修飾キーの二度押しを検出するキーフックを設定できません: {e}")));
                        return;
                    }
                };
                let _ = tx.send(Ok(GetCurrentThreadId()));
                while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                    DispatchMessageW(&msg);
                }
                let _ = UnhookWindowsHookEx(hook);
            })
            .map_err(|e| format!("キーフックのスレッドを作れません: {e}"))?;
        match rx.recv() {
            Ok(Ok(thread_id)) => Ok(Self { thread_id, handle: Some(handle) }),
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                let _ = handle.join();
                Err("キーフックのスレッドが途中で終わりました".to_string())
            }
        }
    }
}

impl Drop for HookThread {
    /// 止める依頼を投げ（失敗したら、スレッドが終わっていなければ投げ直す）、フックを外して抜けるのを待つ。フックの
    /// スレッドはホットキーのスレッドへ投稿しかしないので、ここで待っても互いに待たない。
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        while !handle.is_finished() {
            if unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) }.is_ok() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = handle.join();
    }
}

/// フックが転送するキー入力か。自分の `SendInput`（自動貼り付け）の入力は、注入の印があって目印
/// （`paste::PASTE_INPUT_MARK`）が一致するので数えない。
fn should_forward(flags_injected: bool, extra_info: usize) -> bool {
    !(flags_injected && extra_info == crate::paste::PASTE_INPUT_MARK)
}

/// 左右バリアントを汎用の修飾キーVKへ正規化する。修飾キー以外はそのまま返す。
fn normalize_modifier(vk: u32) -> u32 {
    match vk {
        v if v == VK_LCONTROL.0 as u32 || v == VK_RCONTROL.0 as u32 => VK_CONTROL.0 as u32,
        v if v == VK_LSHIFT.0 as u32 || v == VK_RSHIFT.0 as u32 => VK_SHIFT.0 as u32,
        v if v == VK_LMENU.0 as u32 || v == VK_RMENU.0 as u32 => VK_MENU.0 as u32,
        v => v,
    }
}

/// WH_KEYBOARD_LL フックプロシージャ（フック専用のスレッドで呼ばれる）。キーイベントをホットキー窓へ転送する
/// だけで、すぐ次のフックへ渡す（判定は wndproc 側=WM_APP_KEY_EVENTで行う。C版のDLLが転送のみだったのと同じ分担）。
/// 転送には、キーが起きた時刻とフックの世代を含める。
unsafe extern "system" fn ll_hook_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if ncode >= 0 {
            let target = HOOK_TARGET.load(Ordering::SeqCst);
            let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let injected = info.flags.0 & LLKHF_INJECTED.0 != 0;
            if target != 0 && should_forward(injected, info.dwExtraInfo) {
                let msg = wparam.0 as u32;
                let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
                let generation = HOOK_GENERATION.load(Ordering::SeqCst);
                let (wparam, lparam) = pack_key_event(normalize_modifier(info.vkCode), generation, info.time, up);
                let _ = PostMessageW(Some(HWND(target as *mut _)), WM_APP_KEY_EVENT, wparam, lparam);
            }
        }
        CallNextHookEx(None, ncode, wparam, lparam)
    }
}

/// `WM_APP_KEY_EVENT` の引数に詰める（wParam = VK | 世代 << 32、lParam = 時刻 << 1 | key up）。
/// 64ビットでだけ組むので、どちらも欠けない。
const _: () = assert!(usize::BITS == 64, "WM_APP_KEY_EVENT の詰め方は 64 ビットの WPARAM・LPARAM が前提");
fn pack_key_event(vk: u32, generation: u32, time: u32, up: bool) -> (WPARAM, LPARAM) {
    (
        WPARAM(vk as usize | (generation as usize) << 32),
        LPARAM(((time as isize) << 1) | isize::from(up)),
    )
}

/// `pack_key_event` の逆（VK・世代・時刻・key up）。
fn unpack_key_event(wparam: WPARAM, lparam: LPARAM) -> (u32, u32, u32, bool) {
    (
        wparam.0 as u32,
        (wparam.0 >> 32) as u32,
        (lparam.0 >> 1) as u32,
        lparam.0 & 1 != 0,
    )
}

/// 二度押しの判定の状態とタイマーを捨てる（フックを張る・外すとき。前のフックの間に数えたものと合わせない）。
fn reset_double_press(hwnd: HWND, ctx: &HotkeyContext) {
    ctx.dp_vk.set(0);
    ctx.dp_count.set(0);
    ctx.dp_last_up.set(0);
    unsafe {
        let _ = KillTimer(Some(hwnd), DOUBLE_PRESS_TIMER_ID);
    }
}

/// 二度押しアクション設定に応じてLLフックを張る/外す（C版のロード方針と同じ:
/// 有効なアクションが無ければフック自体を張らない）。フックは専用のスレッドに張る（`HookThread`）。
/// フックを張れなかったときは、その説明を返す。
fn sync_keyboard_hook(hwnd: HWND, ctx: &HotkeyContext) -> Option<String> {
    let want = ctx.config.read().unwrap().hotkey.any_double_press();
    let has = ctx.hook.borrow().is_some();
    if want && !has {
        reset_double_press(hwnd, ctx);
        let generation = ctx.hook_generation.get().wrapping_add(1);
        ctx.hook_generation.set(generation);
        // 転送先と世代をフックより先に入れる（張った直後の転送も今の世代になる）
        HOOK_GENERATION.store(generation, Ordering::SeqCst);
        HOOK_TARGET.store(hwnd.0 as isize, Ordering::SeqCst);
        match HookThread::spawn() {
            Ok(thread) => *ctx.hook.borrow_mut() = Some(thread),
            Err(e) => {
                HOOK_TARGET.store(0, Ordering::SeqCst);
                return Some(e);
            }
        }
    } else if !want && has {
        stop_keyboard_hook(hwnd, ctx);
    }
    None
}

/// フックを外す（スレッドを止めて待つ）。判定の状態も捨てる。
fn stop_keyboard_hook(hwnd: HWND, ctx: &HotkeyContext) {
    HOOK_TARGET.store(0, Ordering::SeqCst);
    let thread = ctx.hook.borrow_mut().take();
    drop(thread);
    reset_double_press(hwnd, ctx);
}

/// 二度押し判定の状態遷移（C版 MainProc.c WM_KEY_HOOKハンドラの移植、
/// Win32呼び出しを含まない部分だけを切り出したもの）。修飾キーのkey upを数え、
/// 他のキーが挟まったら無効化する。同じ修飾キーの key up でも、前の key up から `interval` ミリ秒を超えて
/// 起きたものは数え直す（時刻はキーが起きた時刻。ホットキーのスレッドが止まっている間にたまった転送を続けて受け取っても、
/// 間を空けた2回を二度押しと数えない）。戻り値はタイマーを(再)設定すべきか。
fn advance_double_press(
    dp_vk: &mut u32,
    dp_count: &mut u32,
    last_up: &mut u32,
    vk: u32,
    up: bool,
    time: u32,
    interval: u32,
) -> bool {
    let is_modifier = vk == VK_CONTROL.0 as u32 || vk == VK_SHIFT.0 as u32 || vk == VK_MENU.0 as u32;
    if !is_modifier {
        // 修飾キー以外が挟まったら判定を無効化（Ctrl+C等を二度押しと誤認しない）
        *dp_vk = 0;
        *dp_count = 0;
        return false;
    }
    if !up {
        return false;
    }
    if *dp_vk == vk && *dp_count > 0 && time.wrapping_sub(*last_up) <= interval {
        *dp_count += 1;
    } else {
        *dp_vk = vk;
        *dp_count = 1;
    }
    *last_up = time;
    true
}

/// 二度押し判定。タイマー満了時にカウントが2以上なら二度押しとして発火する。
fn on_key_event(hwnd: HWND, ctx: &HotkeyContext, vk: u32, up: bool, time: u32) {
    let (mut dp_vk, mut dp_count, mut last_up) = (ctx.dp_vk.get(), ctx.dp_count.get(), ctx.dp_last_up.get());
    let interval = unsafe { GetDoubleClickTime() };
    let arm_timer = advance_double_press(&mut dp_vk, &mut dp_count, &mut last_up, vk, up, time, interval);
    ctx.dp_vk.set(dp_vk);
    ctx.dp_count.set(dp_count);
    ctx.dp_last_up.set(last_up);
    if !arm_timer {
        return;
    }
    // 1回目は二度押し猶予時間の満了を待ち、2回目以降は即座にタイマーを発火させる
    let wait_ms = if dp_count == 1 { interval } else { 1 };
    unsafe {
        SetTimer(Some(hwnd), DOUBLE_PRESS_TIMER_ID, wait_ms, None);
    }
}

/// 二度押しタイマー満了。カウント2以上なら設定されたアクションを実行する。
fn on_double_press_timer(hwnd: HWND, ctx: &HotkeyContext) {
    unsafe {
        let _ = KillTimer(Some(hwnd), DOUBLE_PRESS_TIMER_ID);
    }
    let fired = ctx.dp_count.get() > 1;
    let vk = ctx.dp_vk.get();
    ctx.dp_vk.set(0);
    ctx.dp_count.set(0);
    if !fired {
        return;
    }
    let hk = ctx.config.read().unwrap().hotkey.clone();
    let action = match vk {
        v if v == VK_CONTROL.0 as u32 => hk.double_press_ctrl,
        v if v == VK_SHIFT.0 as u32 => hk.double_press_shift,
        v if v == VK_MENU.0 as u32 => hk.double_press_alt,
        _ => DoublePressAction::None,
    };
    let content = match action {
        DoublePressAction::Menu => MenuContent::All,
        DoublePressAction::MenuPinned => MenuContent::PinnedOnly,
        DoublePressAction::MenuHistory => MenuContent::HistoryOnly,
        DoublePressAction::None | DoublePressAction::Viewer => MenuContent::All,
    };
    match action {
        DoublePressAction::None => {}
        DoublePressAction::Menu | DoublePressAction::MenuPinned | DoublePressAction::MenuHistory => {
            if !ctx.menu_open.get() {
                show_popup_menu(hwnd, ctx, true, content);
            }
        }
        DoublePressAction::Viewer => {
            let _ = ctx.events.send(HotkeyEvent::ShowViewer);
            (ctx.repaint)();
        }
    }
}

// --- Popup menu (native) ---

/// メニュー1行分（idはUuid、TrackPopupMenuへはインデックス+1で渡す）。
struct MenuRow {
    id: Uuid,
    pinned: bool,
    label: String,
    /// 種別（サムネイルの無い行の種別アイコン。ビューアの一覧と同じ）
    kind: EntryKind,
    /// サムネイルのファイル名（CF_DIBに`thumb`があるエントリのみ）。サービスのロックの中では
    /// 名前だけを写し、ファイルはロックの外で読む（`load_menu_thumbnails`）
    thumb_name: Option<String>,
    /// サムネイルWebP。表示時にHBITMAP化する。一覧は無ければ種別アイコンへフォールバックするが、
    /// メニューは軽量さを優先しフォールバックせずテキストのみの項目にする（意図的な割り切り）
    thumbnail: Option<Vec<u8>>,
}

/// メニュー項目のニーモニック接頭辞。1〜9番目は"&1 "〜"&9 "、10番目のみ"&0 "、
/// 11番目以降は付けない（Windows標準の数字ニーモニックの範囲に収める。
/// C版`menu_text_format`のようなカスタム書式は持たせず固定にする）。
fn accel_prefix(ordinal: usize) -> String {
    match ordinal {
        1..=9 => format!("&{ordinal} "),
        10 => "&0 ".to_string(),
        _ => String::new(),
    }
}

/// メニュー行の表示ラベル（1行・50文字まで）。
fn menu_label(meta: &EntryMeta) -> String {
    let base = meta
        .title
        .clone()
        .or_else(|| meta.preview.clone())
        .unwrap_or_else(|| {
            let has = |name: &str| meta.formats.iter().any(|f| f.format_name == name);
            if has("CF_DIB") {
                "（画像）".to_string()
            } else if has("CF_HDROP") {
                "（ファイル）".to_string()
            } else {
                "（データ）".to_string()
            }
        });
    base.lines().next().unwrap_or("").chars().take(50).collect()
}

/// エントリのCF_DIBサムネイルのファイル名（一覧と同じ`thumb`参照）。サムネイルが無い
/// （小画像で生成されなかった）場合は`None`で、その項目はテキストのみのまま表示する。
fn thumb_name(meta: &EntryMeta) -> Option<String> {
    meta.formats.iter().find_map(|f| f.thumb.clone())
}

/// メニューの各行のサムネイルのファイルを読む（サービスのロックの外で呼ぶ）。読むときに
/// ファイルが消えていたら（削除・押し出しと入れ違い）、その行はサムネイルなしで出す。
fn load_menu_thumbnails(layout: &mut MenuLayout, storage: &crate::storage::Storage) {
    for row in &mut layout.rows {
        if let Some(name) = &row.thumb_name {
            row.thumbnail = storage.load_thumbnail(name);
        }
    }
}

/// メニューに出すもの（修飾キーの二度押しの設定で選ぶ。ホットキー・トレイは `All`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuContent {
    /// 履歴と「ピン留め(&P)」の子メニュー
    All,
    /// ピン留めだけ（ルートの中身をメニューに直に並べる）
    PinnedOnly,
    /// 履歴だけ
    HistoryOnly,
}

/// ピン留めだけのメニューで、ピン留めが1件も無いときに出す選べない行。
const NO_PINNED_LABEL: &str = "（ピン留めなし）";

/// メニューの構成。`rows`が選択可能な全行（コマンドID=インデックス+1）で、
/// `visible`/`groups`/`pinned`が`rows`内の区画を示す。
struct MenuLayout {
    content: MenuContent,
    rows: Vec<MenuRow>,
    /// 先頭からこの件数が直下に出す履歴行
    visible: usize,
    /// 履歴の階層表示の導出フォルダ（サブメニュー化する行範囲）
    groups: Vec<store::HistoryGroup>,
    /// ピン留め（「ピン留め(&P)」の子メニューの中身。ピン留めの木と同じ形。空なら子メニューを出さない）
    pinned: Vec<PinnedMenuNode>,
    /// 「ピン留め(&P)」の子メニューを履歴より上に出す（`hotkey.menu_pinned_first`。`All` だけに効く）
    pinned_first: bool,
}

/// ピン留めの子メニューの項目（フォルダは入れ子の子メニューにする。C版の「登録アイテム」と同じ）。
#[derive(Debug, PartialEq)]
enum PinnedMenuNode {
    /// `MenuLayout::rows` の添字
    Item(usize),
    Folder { title: String, children: Vec<PinnedMenuNode> },
}

/// ピン留めの子メニューの入口の名前（C版の「登録アイテム(&R)」）。
const PINNED_MENU_LABEL: &str = "ピン留め(&P)";
/// 中身の無いフォルダの子メニューに出す、選べない行。
const EMPTY_FOLDER_LABEL: &str = "（空）";

/// 履歴・ピン留めのスナップショットからメニュー構成を作る（サービスのロックの中で呼ぶ。
/// I/O はせず、サムネイルはファイル名だけを写す）。
/// 階層表示が有効な間は履歴全件を区画つきで出し、`menu_max_items`は使わない
/// （メニューの構造をビューアの階層表示と一致させるため）。ピン留めは全部（フォルダの中も）出す。
/// `content` が出さないと決めた側（ピン留めのみの履歴、履歴のみのピン留め）は写さない。
fn build_menu_layout(history: &store::History, pinned: &[PinnedNode], config: &Config, content: MenuContent) -> MenuLayout {
    let grouping = &config.history.grouping;
    let history_take = if content == MenuContent::PinnedOnly {
        0
    } else if grouping.enabled {
        usize::MAX
    } else {
        config.hotkey.menu_max_items as usize
    };
    let mut rows: Vec<MenuRow> = history
        .iter()
        .take(history_take)
        .map(|item| MenuRow {
            id: item.meta.id,
            pinned: false,
            label: menu_label(&item.meta),
            // ディスクに書かない形式（resident）も含めて決める（ビューアの一覧と同じ）
            kind: EntryKind::from_format_names(
                item.meta.formats.iter().map(|f| f.format_name.as_str()).chain(item.resident.iter().map(|f| f.format_name.as_str())),
            ),
            thumb_name: thumb_name(&item.meta),
            thumbnail: None,
        })
        .collect();
    let (visible, groups) = if grouping.enabled {
        store::group_history(rows.len(), grouping)
    } else {
        (rows.len(), Vec::new())
    };
    let pinned = if content == MenuContent::HistoryOnly { Vec::new() } else { pinned_menu_nodes(pinned, &mut rows) };
    MenuLayout {
        content,
        rows,
        visible,
        groups,
        pinned,
        pinned_first: config.hotkey.menu_pinned_first,
    }
}

/// ピン留めの木を、`rows` にアイテムの行を足しながら子メニューの形へ写す。
fn pinned_menu_nodes(nodes: &[PinnedNode], rows: &mut Vec<MenuRow>) -> Vec<PinnedMenuNode> {
    nodes
        .iter()
        .map(|node| match node {
            PinnedNode::Item(meta) => {
                rows.push(MenuRow {
                    id: meta.id,
                    pinned: true,
                    label: menu_label(meta),
                    kind: EntryKind::from_format_names(meta.formats.iter().map(|f| f.format_name.as_str())),
                    thumb_name: thumb_name(meta),
                    thumbnail: None,
                });
                PinnedMenuNode::Item(rows.len() - 1)
            }
            PinnedNode::Folder(folder) => PinnedMenuNode::Folder {
                title: folder.title.clone(),
                children: pinned_menu_nodes(&folder.children, rows),
            },
        })
        .collect()
}

/// メニューに項目を並べる（履歴 → 区切り線 → 「ピン留め(&P)」の子メニュー。`pinned_first` なら
/// 「ピン留め(&P)」→ 区切り線 → 履歴）。ニーモニックはメニュー・子メニューごとに1から振る（履歴の番号は
/// 並びによらず履歴の先頭から）。ピン留めのみなら、ピン留めのルートの中身をメニューに直に並べる。
fn fill_popup_menu(popup: &mut PopupMenu, layout: &MenuLayout, thumb_size: u32) {
    let menu = popup.handle();
    if layout.content == MenuContent::PinnedOnly {
        if layout.pinned.is_empty() {
            popup.append_command(menu, 0, NO_PINNED_LABEL, MF_GRAYED, None);
        } else {
            append_pinned_nodes(popup, menu, &layout.pinned, layout, thumb_size);
        }
        return;
    }
    // ピン留めは履歴と区切り線で分ける（ピン留めが無ければ区切り線も子メニューも出さない）
    let has_pinned = !layout.pinned.is_empty();
    if layout.pinned_first && has_pinned {
        append_pinned_submenu(popup, layout, thumb_size);
        popup.append_separator(menu);
    }
    append_history_rows(popup, layout, thumb_size);
    if !layout.pinned_first && has_pinned {
        popup.append_separator(menu);
        append_pinned_submenu(popup, layout, thumb_size);
    }
}

/// メニューに履歴を並べる（直下の行 → 階層表示の導出フォルダ。無ければ「（履歴なし）」）。
fn append_history_rows(popup: &mut PopupMenu, layout: &MenuLayout, thumb_size: u32) {
    let menu = popup.handle();
    if layout.visible == 0 && layout.groups.is_empty() {
        // 「（空）」「（ピン留めなし）」と同じく選べない行
        popup.append_command(menu, 0, "（履歴なし）", MF_GRAYED, None);
    }
    // 直下の履歴
    for i in 0..layout.visible {
        append_menu_row(popup, menu, i, i + 1, &layout.rows[i], thumb_size);
    }
    // 階層表示の導出フォルダ（サブメニュー。親の破棄で一緒に破棄される）
    for group in &layout.groups {
        let Some(sub) = popup.add_submenu(&escape_menu_text(&group.title), Some(icons::FOLDER)) else {
            continue;
        };
        for (ordinal, i) in group.range.clone().enumerate() {
            append_menu_row(popup, sub, i, ordinal + 1, &layout.rows[i], thumb_size);
        }
    }
}

/// メニューに「ピン留め(&P)」の子メニューを足す。
fn append_pinned_submenu(popup: &mut PopupMenu, layout: &MenuLayout, thumb_size: u32) {
    if let Some(sub) = popup.add_submenu(PINNED_MENU_LABEL, Some(icons::PINNED)) {
        append_pinned_nodes(popup, sub, &layout.pinned, layout, thumb_size);
    }
}

/// `target` の子メニューにピン留めの項目を並べる（フォルダは入れ子の子メニュー）。
fn append_pinned_nodes(popup: &mut PopupMenu, target: HMENU, nodes: &[PinnedMenuNode], layout: &MenuLayout, thumb_size: u32) {
    if nodes.is_empty() {
        popup.append_command(target, 0, EMPTY_FOLDER_LABEL, MF_GRAYED, None);
        return;
    }
    for (n, node) in nodes.iter().enumerate() {
        let ordinal = n + 1;
        match node {
            PinnedMenuNode::Item(i) => append_menu_row(popup, target, *i, ordinal, &layout.rows[*i], thumb_size),
            PinnedMenuNode::Folder { title, children } => {
                let text = format!("{}{}", accel_prefix(ordinal), escape_menu_text(title));
                if let Some(sub) = popup.add_submenu_to(target, &text, Some(icons::FOLDER)) {
                    append_pinned_nodes(popup, sub, children, layout, thumb_size);
                }
            }
        }
    }
}

/// メニュー行サムネイルの長辺px（96 DPI 基準。C版の既定`menu_bmp_width/height`に合わせる）。
/// 実際の大きさは、メニューを出すモニターの DPI に合わせる（`show_popup_menu`）。
const MENU_THUMB_SIZE: u32 = 32;

/// メニューに1行追加する（自前描画。左の欄に、サムネイルがあればサムネイル、無ければ種別アイコンを
/// 描く）。画像の画素は `PopupMenu` が持ち、メニューの破棄の後で解放する。`thumb_size`はサムネイルの
/// 長辺px（DPI に合わせたもの）。
fn append_menu_row(menu: &mut PopupMenu, target: HMENU, i: usize, ordinal: usize, row: &MenuRow, thumb_size: u32) {
    let label = format!("{}{}", accel_prefix(ordinal), escape_menu_text(&row.label));
    let thumbnail = row.thumbnail.as_ref().and_then(|webp| crate::dib::webp_to_rgba_scaled(webp, thumb_size).ok());
    let image = match &thumbnail {
        Some((width, height, rgba)) => MenuImage::Thumbnail { width: *width, height: *height, rgba },
        None => MenuImage::Icon(icons::for_kind(row.kind)),
    };
    menu.append_command(target, i + 1, &label, MENU_ITEM_FLAGS(0), Some(image));
}

/// ポップアップメニューを表示し、選択されたらUIへイベントを送る。
/// このスレッド上でモーダルに動く（TrackPopupMenuが自前でメッセージを回す）。
/// `use_caret`: ホットキー経由（テキスト入力中に開くことが多い）は`true`でキャレット
/// 位置を優先。トレイアイコンのクリック経由は`false`でマウス位置に固定する
/// （たまたま裏で編集中のウィンドウがあっても、クリックしたトレイの近くに出すのが自然）。
fn show_popup_menu(hwnd: HWND, ctx: &HotkeyContext, use_caret: bool, content: MenuContent) {
    // 処理中フラグ（メニュー表示から送出の完了まで）。この間に届いた終了・再登録の依頼は
    // 保留され、呼び出し元（wndproc）が`run_deferred`で戻った後に処理する。
    // `ctx`はこの関数が戻るまで解放されない
    let _busy = BusyGuard::enter(&ctx.menu_open);
    // フォーカス記録はメニュー表示（＝フォーカスを奪う）前に行うのが肝
    let focus = capture_focus();

    // ロック順序: configのガードを手放してからserviceを取る（他スレッドと同順）
    let config = ctx.config.read().unwrap().clone();
    // サービスのロックの中では行の写しだけを取り、サムネイルのファイルはロックの外で読む
    let Some(mut layout) =
        ctx.core.read(|service| build_menu_layout(&service.history, &service.pinned, &config, content))
    else {
        return;
    };
    load_menu_thumbnails(&mut layout, &ctx.core.storage());

    // キャレット位置（取れなければ、またはuse_caret=falseならマウス位置）に表示する。
    // 行のサムネイルの大きさを、その位置のモニターの DPI に合わせるため、先に決める
    let (x, y) = if use_caret {
        focus.caret_pos.unwrap_or_else(cursor_pos)
    } else {
        cursor_pos()
    };
    // メニュー全体（子メニューを含む）を、表示する位置のモニターの DPI で描く
    let dpi = menu_tooltip::dpi_at_point(x, y);
    let thumb_size = menu_tooltip::scale_for_dpi(MENU_THUMB_SIZE as i32, dpi) as u32;
    let has_thumbnails = layout.rows.iter().any(|r| r.thumbnail.is_some());

    unsafe {
        let Ok(mut popup) = PopupMenu::new(dpi, has_thumbnails.then_some(thumb_size as i32)) else {
            return;
        };
        let menu = popup.handle();
        fill_popup_menu(&mut popup, &layout, thumb_size);

        // TrackPopupMenuを閉じられるようにするための前面化（トレイメニューと同じ定石）。
        // 二度押し（キーフック）経由ではフォアグラウンド権限がないため強制版を使う
        let _ = crate::paste::force_set_foreground(hwnd);
        // メニュー表示中のWM_MENUSELECT/WM_TIMERで項目ツールチップを出す。
        // セッションは自己完結（wndprocが再入して&mut ctxと衝突しないため）
        menu_tooltip::begin_session(
            hwnd,
            ctx.core.clone(),
            config.hotkey.tooltip.clone(),
            layout
                .rows
                .iter()
                .map(|r| menu_tooltip::TooltipTarget { id: r.id, pinned: r.pinned })
                .collect(),
        );
        // 表示前に終了が依頼されていた場合（`EndMenu`は閉じる対象がなく空振りする）は、
        // メニューを開かない。開くと閉じる契機がなくなり、`Drop`のjoinが戻らなくなる
        let cmd = if ctx.shutdown_pending.get() {
            BOOL(0)
        } else {
            TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN,
                x,
                y,
                Some(0),
                hwnd,
                None,
            )
        };
        menu_tooltip::end_session();
        let _ = PostMessageW(Some(hwnd), 0, WPARAM(0), LPARAM(0)); // WM_NULL
        // メニュー（子メニューごと）を破棄してから、項目の描画の材料（サムネイル・フォント）を解放する
        drop(popup);

        // 終了依頼でメニューが閉じられた場合は、送出（フォーカス復帰・書き込み・貼り付け）を
        // しない。窓の破棄は戻った先のwndprocが行う
        if ctx.shutdown_pending.get() {
            return;
        }
        let picked = (cmd.0 as usize)
            .checked_sub(1)
            .and_then(|i| layout.rows.get(i));
        match picked {
            Some(row) => send_pick(hwnd, ctx, row.id, row.pinned, &focus),
            // キャンセル時は元ウィンドウへフォーカスを返すだけ（C版準拠）。戻り値（復帰成否）は
            // 後続処理が無いため使わない
            None => {
                restore_focus(&focus);
            }
        }
    }
}

/// メニューで選ばれたエントリを送出する（フォーカス復帰 → クリップボード → 自動ペースト）。
///
/// C版と同じく、メニューを表示したこのスレッド上で完結させる。
/// フォーカス復帰の`SetForegroundWindow`は、フォアグラウンド（メニューのオーナー窓）を所有する
/// このスレッドから呼ばないと効かない（別のスレッドから呼ぶと成功を返しつつ実際には
/// 切り替わらず、Ctrl+Vが自分の隠し窓に消えることを実機で確認）。
fn send_pick(hwnd: HWND, ctx: &HotkeyContext, id: Uuid, pinned: bool, focus: &FocusInfo) {
    // 読み込みは受け付け・操作用のロックの下で、サービスのロックの外で行う（取り込みの変換中は
    // 待つことがあるが、このスレッドなのでビューアのメインスレッドは止まらない）。受け付けで
    // 覆うのは読み込みと、後のクリップボードへの書き込み（フォーカスの復帰・貼り付けまで延ばすと、
    // 終了時に相互待ちの恐れがある）
    let entry = match ctx.core.load_for_send(id, pinned) {
        Ok(entry) => entry,
        Err(e) => {
            eprintln!("メニュー選択エントリの読み込みに失敗: {e}");
            // キャンセルと同じく、元のウィンドウへフォーカスを返す（前面がメニューのオーナーの隠し窓のまま
            // 残らないように）
            restore_focus(focus);
            return;
        }
    };
    let (delete_on_send, auto_paste, text_cfg) = {
        let cfg = ctx.config.read().unwrap();
        (cfg.history.delete_on_send, cfg.hotkey.auto_paste, cfg.tools.text.clone())
    };
    let entry = if pinned {
        crate::tools::text::convert_entry_date(entry, &text_cfg)
    } else {
        entry
    };

    // フォーカス復帰の成否はauto_paste実行の可否判定に使う（クリップボードへの書き込み
    // 自体は復帰の成否と無関係に行うため、ここでは早期returnしない）
    let focus_restored = restore_focus(focus);

    // クリップボードへの書き込みの間だけ、もう一度受け付けに登録する（終了処理が受け付けを
    // 閉じた後は書かない。書いている間に最後の保存へ進まない）。前面化と自動貼り付けは
    // 登録の外（前面化で起きうるスレッド間の相互待ちを、受け付けの待ちへ持ち込まない）
    {
        let Ok(_ticket) = ctx.core.admit() else {
            return;
        };
        // 自分の書き込みを履歴に再登録しない設定なら、書いた変更の番号を抑止する番号として記録する
        if let Err(e) = crate::clipboard::set_clipboard_suppressed(&ctx.clipboard, &entry.formats, delete_on_send) {
            eprintln!("クリップボードへの書き込みに失敗: {e}");
            return;
        }
    }
    // C版準拠: Shift押下中は自動ペーストを抑制。送出中はホットキーを
    // 一時解除して貼り付け先のCtrl+Vと衝突しないようにする（同一スレッド上
    // なのでsuspend/resumeのメッセージ往復は不要、直接呼ぶ）。フォーカス復帰に
    // 失敗した場合はauto_pasteをスキップする（対象ウィンドウが消滅していた場合等に
    // 誤ったウィンドウへCtrl+Vを送出しないため）
    if auto_paste && focus_restored && !crate::paste::shift_held() {
        unregister_all(hwnd);
        // send_pasteのSendInputはWH_KEYBOARD_LLにも乗るが、目印（`paste::PASTE_INPUT_MARK`）を付けてあり、
        // フックが二度押し判定へ転送しない（`should_forward`。Ctrl down→V down/up(リセット)→Ctrl up(新規1回
        // 押下として記録)という経路になり、直後の実操作と合算して誤発火しうるため）
        crate::paste::send_paste();
        // 外している一瞬に他のアプリが同じキーを取った場合だけ失敗する。画面には出さない
        // （メニューから送出した直後のため。起動時の失敗は `Hotkeys::startup_problems` で伝える）
        if let Some(problem) = register_from_config(hwnd, &ctx.config) {
            eprintln!("{problem}");
        }
    }
    (ctx.repaint)();
}

/// 登録できなかったときは、その説明を返す。
fn register_from_config(hwnd: HWND, config: &Arc<RwLock<Config>>) -> Option<String> {
    let hotkey = config.read().unwrap().hotkey.popup_menu.clone();
    let (modifiers, vk) = to_win32(&hotkey)?;
    unsafe { RegisterHotKey(Some(hwnd), HOTKEY_ID_POPUP, modifiers, vk) }.err().map(|e| {
        format!(
            "ホットキー {} を登録できません（他のアプリが使っている可能性があります）: {e}",
            hotkey_label(&hotkey)
        )
    })
}

/// 表示用のホットキー名（例: "Alt+C"）。
fn hotkey_label(hotkey: &Hotkey) -> String {
    let mut parts: Vec<String> = hotkey
        .modifiers
        .iter()
        .map(|m| {
            let mut chars = m.chars();
            chars.next().map_or(String::new(), |c| c.to_uppercase().chain(chars).collect())
        })
        .collect();
    parts.push(hotkey.key.to_uppercase());
    parts.join("+")
}

fn unregister_all(hwnd: HWND) {
    unsafe {
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID_POPUP);
    }
}

/// ホットキーの再登録とキーフックの同期（設定適用時・起動時）。できなかったことの説明を返す。
fn apply_reregister(hwnd: HWND, ctx: &HotkeyContext) -> Vec<String> {
    unregister_all(hwnd);
    let mut problems = Vec::new();
    problems.extend(register_from_config(hwnd, &ctx.config));
    problems.extend(sync_keyboard_hook(hwnd, ctx));
    problems
}

/// 設定の反映による登録し直しを行い、結果を番号付きでビューアへ返す。ビューアへは通り道と
/// 投稿の起床で返し、同期では送らない。
fn reregister_and_report(hwnd: HWND, ctx: &HotkeyContext, generation: u64) {
    let problems = apply_reregister(hwnd, ctx);
    for problem in &problems {
        eprintln!("{problem}");
    }
    let _ = ctx.events.send(HotkeyEvent::Reregistered { generation, problems });
    (ctx.repaint)();
}

/// ポップアップ処理中に保留された終了・再登録の依頼を、処理が終わっている場合に限って
/// 実行する。モーダルループ中に再入したwndprocから呼ばれても`menu_open`で弾かれ、
/// 外側の`show_popup_menu`が戻った後の呼び出しで実行される。終了が保留されていれば
/// 再登録は無駄なので行わない。呼び出し側の`ctx`参照はこの時点で終わっていること
/// （`DestroyWindow`がコンテキストを解放するため）。
unsafe fn run_deferred(hwnd: HWND) {
    let shutdown = {
        let Some(ctx) = (unsafe { ctx_ref(hwnd) }) else {
            return;
        };
        if ctx.menu_open.get() {
            return;
        }
        if !ctx.shutdown_pending.get() {
            let generation = ctx.reregister_pending.replace(0);
            if generation != 0 {
                reregister_and_report(hwnd, ctx, generation);
            }
            return;
        }
        true
    };
    // `ctx`の参照はここで終わっている（DestroyWindowがコンテキストを解放する）
    if shutdown {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_NCCREATE => {
                let cs = lparam.0 as *const CREATESTRUCTW;
                if let Some(cs) = cs.as_ref() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_HOTKEY => {
                if let Some(ctx) = ctx_ref(hwnd) {
                    if wparam.0 as i32 == HOTKEY_ID_POPUP && !ctx.menu_open.get() {
                        show_popup_menu(hwnd, ctx, true, MenuContent::All);
                    }
                }
                run_deferred(hwnd);
                LRESULT(0)
            }
            WM_APP_SHOW_POPUP_AT_MOUSE => {
                if let Some(ctx) = ctx_ref(hwnd) {
                    if !ctx.menu_open.get() {
                        show_popup_menu(hwnd, ctx, false, MenuContent::All);
                    }
                }
                run_deferred(hwnd);
                LRESULT(0)
            }
            WM_APP_KEY_EVENT => {
                if let Some(ctx) = ctx_ref(hwnd) {
                    // 今の世代のフックからの転送だけを数える（`ll_hook_proc` の詰め方）
                    let (vk, generation, time, up) = unpack_key_event(wparam, lparam);
                    if ctx.hook.borrow().is_some() && generation == ctx.hook_generation.get() {
                        on_key_event(hwnd, ctx, vk, up, time);
                    }
                }
                LRESULT(0)
            }
            WM_MENUSELECT => {
                menu_tooltip::on_menu_select(wparam, lparam);
                LRESULT(0)
            }
            // ポップアップメニューの自前描画（`show_popup_menu` の `PopupMenu` の項目。モーダル中に
            // 再入して届く。項目のデータを共有参照で読むだけで、`ctx` の状態には触らない）
            WM_MEASUREITEM if menu_draw::on_measure_item(hwnd, lparam) => LRESULT(1),
            WM_DRAWITEM if menu_draw::on_draw_item(lparam) => LRESULT(1),
            WM_MENUCHAR => match menu_draw::on_menu_char(wparam, lparam) {
                Some(result) => result,
                None => DefWindowProcW(hwnd, msg, wparam, lparam),
            },
            WM_TIMER if wparam.0 == menu_tooltip::TIMER_ID => {
                menu_tooltip::on_timer();
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == DOUBLE_PRESS_TIMER_ID => {
                if let Some(ctx) = ctx_ref(hwnd) {
                    on_double_press_timer(hwnd, ctx);
                }
                run_deferred(hwnd);
                LRESULT(0)
            }
            WM_APP_REREGISTER => {
                if let Some(ctx) = ctx_ref(hwnd) {
                    // wParam は依頼の番号（`Hotkeys::reregister`）
                    let generation = wparam.0 as u64;
                    if ctx.menu_open.get() {
                        // ポップアップ処理中は登録・フックを触らず、終わってから反映する
                        // （依頼は投稿なので呼び出し元は待たない）。重なった依頼は最後の番号だけ残す
                        ctx.reregister_pending.set(generation);
                    } else {
                        reregister_and_report(hwnd, ctx, generation);
                    }
                }
                LRESULT(0)
            }
            // WM_CLOSEは既定処理（DefWindowProcW）がDestroyWindowを呼ぶため、外部から
            // （タスクの終了要求・SC_CLOSE等）届いてもモーダル中に窓が破棄されないよう、
            // 独自の終了依頼と同じ保留経路に通す
            WM_CLOSE | WM_APP_SHUTDOWN => {
                match ctx_ref(hwnd) {
                    // ポップアップ処理中: ここで窓を破棄するとshow_popup_menuの`ctx`が
                    // 解放済みになる。メニューだけ閉じ、破棄は戻った後に行う
                    Some(ctx) if ctx.menu_open.get() => {
                        ctx.shutdown_pending.set(true);
                        let _ = EndMenu();
                    }
                    _ => {
                        let _ = DestroyWindow(hwnd);
                    }
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                unregister_all(hwnd);
                let ctx = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut HotkeyContext;
                if !ctx.is_null() {
                    // ポップアップ処理中に解放しない（保留で守る設計の不変条件）
                    debug_assert!(!(*ctx).menu_open.get(), "HotkeyContext freed while popup is active");
                    let ctx = Box::from_raw(ctx);
                    if ctx.hook.borrow().is_some() {
                        stop_keyboard_hook(hwnd, &ctx);
                    }
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

// --- Public handle ---

struct SendHwnd(HWND);
unsafe impl Send for SendHwnd {}

pub struct Hotkeys {
    hwnd: HWND,
    thread: Option<JoinHandle<()>>,
    startup_problems: Vec<String>,
}

impl Hotkeys {
    pub fn spawn(
        config: Arc<RwLock<Config>>,
        core: Core,
        clipboard: Arc<ClipboardPort>,
        events: Sender<HotkeyEvent>,
        repaint: impl Fn() + Send + 'static,
    ) -> Result<Self> {
        let (hwnd_tx, hwnd_rx) = std::sync::mpsc::channel::<WinResult<(SendHwnd, Vec<String>)>>();

        let thread = thread::spawn(move || {
            let create = || -> WinResult<(HWND, Vec<String>)> {
                unsafe {
                    let hinstance = GetModuleHandleW(None)?.into();
                    let class = WNDCLASSW {
                        lpfnWndProc: Some(wndproc),
                        hInstance: hinstance,
                        lpszClassName: CLASS_NAME,
                        ..Default::default()
                    };
                    RegisterClassW(&class);

                    let ctx_config = Arc::clone(&config);
                    let ctx = Box::into_raw(Box::new(HotkeyContext {
                        events,
                        config: ctx_config,
                        core,
                        clipboard,
                        repaint: Box::new(repaint),
                        menu_open: Cell::new(false),
                        hook: RefCell::new(None),
                        hook_generation: Cell::new(0),
                        dp_vk: Cell::new(0),
                        dp_count: Cell::new(0),
                        dp_last_up: Cell::new(0),
                        reregister_pending: Cell::new(0),
                        shutdown_pending: Cell::new(false),
                    }));
                    // メッセージ専用窓（HWND_MESSAGE）ではなく通常の非表示トップレベル窓に
                    // する。TrackPopupMenuのオーナーはフォアグラウンド化できる実ウィンドウで
                    // ある必要があり、メッセージ専用窓を強制フォアグラウンドにすると
                    // TrackPopupMenuがメニューを表示できないままモーダルループに入って
                    // フックスレッドごと固まる（実機確認）。表示しない限り画面には出ない
                    let hwnd = CreateWindowExW(
                        Default::default(),
                        CLASS_NAME,
                        CLASS_NAME,
                        WS_OVERLAPPED,
                        CW_USEDEFAULT,
                        CW_USEDEFAULT,
                        0,
                        0,
                        None,
                        None,
                        Some(hinstance),
                        Some(ctx.cast()),
                    )?;
                    // ホットキー登録とキーフック同期をまとめて実行し、できなかったことを
                    // 呼び出し元へ返す（このスレッドの窓なので、ここで直接呼んでも再入しない）
                    let problems = match ctx_ref(hwnd) {
                        Some(ctx) => apply_reregister(hwnd, ctx),
                        None => Vec::new(),
                    };
                    Ok((hwnd, problems))
                }
            };
            match create() {
                Ok((hwnd, problems)) => {
                    let _ = hwnd_tx.send(Ok((SendHwnd(hwnd), problems)));
                    let mut msg = MSG::default();
                    loop {
                        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
                        if ret.0 <= 0 {
                            break;
                        }
                        unsafe {
                            let _ = TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                    }
                }
                Err(e) => {
                    let _ = hwnd_tx.send(Err(e));
                }
            }
        });

        let (hwnd, startup_problems) = hwnd_rx
            .recv()
            .map_err(|_| WinError::from_hresult(windows::Win32::Foundation::E_FAIL))??;

        Ok(Self {
            hwnd: hwnd.0,
            thread: Some(thread),
            startup_problems,
        })
    }

    /// 起動時にできなかったこと（ホットキーの登録・キーフックの設定の失敗）の説明。
    /// スレッドと窓は動いているので、呼び出し元はユーザーへ知らせて続行できる。
    pub fn startup_problems(&self) -> &[String] {
        &self.startup_problems
    }

    /// 設定変更後の再登録（設定の反映から呼ぶ）。`generation` は依頼の番号（1 以上）で、結果は
    /// `HotkeyEvent::Reregistered` で返る。投稿するだけで待たない（別のスレッドの窓へ `SendMessageW` で送ると、
    /// 返事を待つ間に呼び出し元がほかのスレッドから送られたメッセージを処理し、ビューアのハンドラへ再入しうる
    /// ため）。ポップアップメニューの処理中なら、反映はメニューが閉じて送出が終わった
    /// 後になる（重なった依頼は最後の番号の1回だけ行う）。投稿できなければ Err。
    pub fn reregister(&self, generation: u64) -> WinResult<()> {
        debug_assert!(generation != 0, "0 は「依頼なし」に使う");
        unsafe { PostMessageW(Some(self.hwnd), WM_APP_REREGISTER, WPARAM(generation as usize), LPARAM(0)) }
    }

    /// トレイアイコンの左クリックからポップアップメニューを表示する
    /// （マウス位置固定、tray.rsから呼ぶ）。ホットキースレッド上でモーダルに
    /// 動く（TrackPopupMenuがそのスレッドを表示中占有する）ため、呼び出し元の
    /// トレイスレッドを巻き込んでブロックしないよう`PostMessageW`で依頼する
    /// （完了を待つ必要はない、fire-and-forget）。
    pub fn show_popup_menu_at_mouse(&self) {
        unsafe {
            let _ = PostMessageW(Some(self.hwnd), WM_APP_SHOW_POPUP_AT_MOUSE, WPARAM(0), LPARAM(0));
        }
    }
}

impl Drop for Hotkeys {
    /// 終了を依頼し、スレッドが終わるまで、送られたメッセージに応じながら待つ（`ui_thread::stop_ui_thread`。
    /// ホットキースレッドの前面化がこのスレッドの窓へメッセージを送っても、互いに待たない）。
    fn drop(&mut self) {
        if let Some(t) = self.thread.take() {
            crate::ui_thread::stop_ui_thread(self.hwnd, WM_APP_SHUTDOWN, t, crate::ui_thread::REPOST_INTERVAL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_modifier_maps_left_right_variants_to_generic_vk() {
        assert_eq!(normalize_modifier(VK_LCONTROL.0 as u32), VK_CONTROL.0 as u32);
        assert_eq!(normalize_modifier(VK_RCONTROL.0 as u32), VK_CONTROL.0 as u32);
        assert_eq!(normalize_modifier(VK_LSHIFT.0 as u32), VK_SHIFT.0 as u32);
        assert_eq!(normalize_modifier(VK_RSHIFT.0 as u32), VK_SHIFT.0 as u32);
        assert_eq!(normalize_modifier(VK_LMENU.0 as u32), VK_MENU.0 as u32);
        assert_eq!(normalize_modifier(VK_RMENU.0 as u32), VK_MENU.0 as u32);
    }

    #[test]
    fn normalize_modifier_leaves_non_modifier_keys_unchanged() {
        let vk_a = 0x41;
        assert_eq!(normalize_modifier(vk_a), vk_a);
        // 既に汎用VKの値もそのまま
        assert_eq!(normalize_modifier(VK_CONTROL.0 as u32), VK_CONTROL.0 as u32);
    }

    /// ポップアップ処理を実際に走らせるためのホットキースレッド。ホットキー登録は無効
    /// （常用中のアプリのAlt+Cを奪わない）、二度押しは`double_press_ctrl`だけで切り替える。
    fn spawn_test_hotkeys(
        config: Arc<RwLock<Config>>,
    ) -> (std::path::PathBuf, Hotkeys, std::sync::mpsc::Receiver<HotkeyEvent>) {
        let (dir, core) = crate::ops::tests::temp_core(Config::default());
        let (tx, rx) = std::sync::mpsc::channel();
        let hotkeys = Hotkeys::spawn(
            config,
            core,
            // これらのテストは送出しない（クリップボードを開かない）
            Arc::new(crate::clipboard::ClipboardPort::unopenable()),
            tx,
            || {},
        )
        .unwrap();
        (dir, hotkeys, rx)
    }

    fn test_config() -> Arc<RwLock<Config>> {
        let mut config = Config::default();
        config.hotkey.popup_menu.enabled = false;
        config.hotkey.double_press_ctrl = DoublePressAction::None;
        config.hotkey.double_press_shift = DoublePressAction::None;
        config.hotkey.double_press_alt = DoublePressAction::None;
        Arc::new(RwLock::new(config))
    }

    #[test]
    fn hotkey_label_capitalizes_modifiers_and_key() {
        let hotkey = Hotkey { enabled: true, modifiers: vec!["ctrl".into(), "alt".into()], key: "c".into() };
        assert_eq!(hotkey_label(&hotkey), "Ctrl+Alt+C");
    }

    /// 他が先に同じキーを登録していると、起動時の問題として返す（スレッドは動き続ける）。
    /// 競合しなければ問題はない。普段使われない Ctrl+Alt+Shift+Q を使う。
    #[test]
    fn startup_problems_report_hotkey_already_taken() {
        let _guard = crate::tray::lock_gui_resource_tests();
        let config = test_config();
        config.write().unwrap().hotkey.popup_menu = Hotkey {
            enabled: true,
            modifiers: vec!["ctrl".into(), "alt".into(), "shift".into()],
            key: "Q".into(),
        };
        let (modifiers, vk) = to_win32(&config.read().unwrap().hotkey.popup_menu).unwrap();

        let (dir, hotkeys, _rx) = spawn_test_hotkeys(Arc::clone(&config));
        assert!(hotkeys.startup_problems().is_empty(), "競合がないのに問題がある: {:?}", hotkeys.startup_problems());
        drop(hotkeys);
        let _ = std::fs::remove_dir_all(dir);

        const TEST_ID: i32 = 0x0301;
        unsafe { RegisterHotKey(None, TEST_ID, modifiers, vk) }.expect("前提: テスト用のキーを先に登録できない");
        let (dir, hotkeys, _rx) = spawn_test_hotkeys(config);
        let problems = hotkeys.startup_problems().to_vec();
        drop(hotkeys);
        unsafe {
            let _ = UnregisterHotKey(None, TEST_ID);
        }
        let _ = std::fs::remove_dir_all(dir);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("Ctrl+Alt+Shift+Q"), "{problems:?}");
    }

    /// メニュー表示中の終了の回帰（解放後参照の修正）。実際に`TrackPopupMenu`の
    /// モーダルループへ入れ、その最中に`Drop`（=`WM_APP_SHUTDOWN`）を届ける。
    /// 修正前はここで`HotkeyContext`が解放され、戻った先の`ctx`が解放済みになる。
    /// 修正後は`WM_DESTROY`の`debug_assert`が守り、Dropはメニューを閉じてから
    /// 窓を破棄してスレッドをjoinできる。
    #[test]
    fn shutdown_while_menu_is_open_is_deferred_until_menu_closes() {
        shutdown_while_menu_is_open(false);
    }

    /// 外部からの`WM_CLOSE`（タスクの終了要求等）も、既定処理の`DestroyWindow`に流さず
    /// 独自の終了依頼と同じく保留する（既定処理を通ると破棄へ進む迂回経路を塞ぐ）。
    #[test]
    fn wm_close_while_menu_is_open_is_deferred_until_menu_closes() {
        shutdown_while_menu_is_open(true);
    }

    fn shutdown_while_menu_is_open(via_wm_close: bool) {
        use std::time::Duration;
        use windows::Win32::UI::WindowsAndMessaging::WM_CLOSE;

        struct SendHotkeys(Hotkeys);
        unsafe impl Send for SendHotkeys {}
        impl SendHotkeys {
            fn into_inner(self) -> Hotkeys {
                self.0
            }
        }

        let _guard = crate::tray::lock_gui_resource_tests();
        let (dir, hotkeys, _rx) = spawn_test_hotkeys(test_config());
        let (measured, drawn) = crate::menu_draw::test_support::counts();
        hotkeys.show_popup_menu_at_mouse();
        assert!(
            crate::tray::wait_for_menu_window(Duration::from_secs(3)),
            "ポップアップメニューが表示されなかった（デスクトップが使えない環境では検証できない）"
        );
        // メニュー（空の履歴なので「（履歴なし）」の1行）は自前描画の経路を通る
        std::thread::sleep(Duration::from_millis(100));
        let (measured_after, drawn_after) = crate::menu_draw::test_support::counts();
        assert!(measured_after > measured && drawn_after > drawn, "自前描画の経路を通っていない");
        if via_wm_close {
            unsafe {
                PostMessageW(Some(hotkeys.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)).unwrap();
            }
            // 閉じられる（=保留された終了が実行される）まで待つ。Dropはその後のjoinのみ
            assert!(
                crate::tray::wait_for_menu_window_gone(Duration::from_secs(3)),
                "WM_CLOSEでメニューが閉じられなかった"
            );
        }

        let wrapped = SendHotkeys(hotkeys);
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let dropper = std::thread::spawn(move || {
            drop(wrapped.into_inner());
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "メニュー表示中の終了でDropが完了しなかった"
        );
        dropper.join().unwrap();
        assert!(
            crate::tray::wait_for_menu_window_gone(Duration::from_secs(3)),
            "終了後もメニューが残っている"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// メニュー表示中の設定適用（`reregister`）は、呼び出しがすぐ戻り、キーフックの張り替えを
    /// メニューが閉じるまで保留し、閉じた後に反映する。重なった依頼（番号 1・2）は最後の番号の1回だけ
    /// 行い、結果は番号 2 の1件だけ届く。
    #[test]
    fn reregister_while_menu_is_open_is_deferred_and_applied_after_close() {
        use std::time::{Duration, Instant};
        use windows::Win32::UI::WindowsAndMessaging::WM_CANCELMODE;

        let _guard = crate::tray::lock_gui_resource_tests();
        let config = test_config();
        let (dir, hotkeys, rx) = spawn_test_hotkeys(Arc::clone(&config));
        // 起動時の同期ではキーフックは張られない（二度押しアクションが全て無効）
        assert_eq!(HOOK_TARGET.load(Ordering::Relaxed), 0);

        hotkeys.show_popup_menu_at_mouse();
        assert!(
            crate::tray::wait_for_menu_window(Duration::from_secs(3)),
            "ポップアップメニューが表示されなかった（デスクトップが使えない環境では検証できない）"
        );

        // メニュー表示中に二度押しアクションを有効化して再登録を依頼する
        config.write().unwrap().hotkey.double_press_ctrl = DoublePressAction::Viewer;
        let started = Instant::now();
        hotkeys.reregister(1).unwrap();
        hotkeys.reregister(2).unwrap();
        assert!(started.elapsed() < Duration::from_secs(2), "reregisterがメニュー表示中にブロックした");
        // 保留中: まだキーフックは張られていない
        assert_eq!(HOOK_TARGET.load(Ordering::Relaxed), 0);

        // メニューを閉じる（オーナー窓へのWM_CANCELMODEでモーダルループが終わる）
        unsafe {
            PostMessageW(Some(hotkeys.hwnd), WM_CANCELMODE, WPARAM(0), LPARAM(0)).unwrap();
        }
        assert!(
            crate::tray::wait_for_menu_window_gone(Duration::from_secs(3)),
            "メニューを閉じられなかった"
        );
        // 閉じた後に保留の再登録が反映され、キーフックが張られる
        let deadline = Instant::now() + Duration::from_secs(3);
        while HOOK_TARGET.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_ne!(HOOK_TARGET.load(Ordering::Relaxed), 0, "保留した再登録が反映されなかった");
        // 結果はキーフックを張った後に送られるので、少し待って受け取る
        let mut reports = Vec::new();
        while let Ok(event) = rx.recv_timeout(Duration::from_millis(500)) {
            if let HotkeyEvent::Reregistered { generation, .. } = event {
                reports.push(generation);
            }
        }
        assert_eq!(reports, [2], "重なった依頼の結果が最後の番号の1件になっていない");

        drop(hotkeys);
        assert_eq!(HOOK_TARGET.load(Ordering::Relaxed), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn meta_with(title: Option<&str>, preview: Option<&str>, formats: &[&str]) -> EntryMeta {
        EntryMeta {
            id: Uuid::new_v4(),
            title: title.map(str::to_string),
            modified: 0.0,
            hash: 0,
            preview: preview.map(str::to_string),
            formats: formats
                .iter()
                .map(|name| crate::storage::FormatMeta {
                    format_name: name.to_string(),
                    format_id: 0,
                    blob: String::new(),
                    size: 0,
                    thumb: None,
                })
                .collect(),
        }
    }

    fn history_of(metas: Vec<EntryMeta>) -> store::History {
        store::History::from_items(
            metas.into_iter().map(|meta| store::HistoryItem { meta, resident: Arc::default() }).collect(),
        )
    }

    fn folder(title: &str, children: Vec<PinnedNode>) -> PinnedNode {
        PinnedNode::Folder(store::PinnedFolder { id: Uuid::new_v4(), title: title.to_string(), children })
    }

    /// ピン留めは、フォルダの中も含めて全部を木の形のまま写す（ルートが空でフォルダの中だけでも出る）。
    /// 行の文字に「[ピン]」は付けない。
    #[test]
    fn menu_layout_keeps_whole_pinned_tree() {
        let history = history_of(vec![meta_with(Some("h1"), None, &[]), meta_with(Some("h2"), None, &[])]);
        let (a, b, c) = (meta_with(Some("a"), None, &[]), meta_with(Some("b"), None, &["CF_DIB"]), meta_with(Some("c"), None, &[]));
        let ids = [a.id, b.id, c.id];
        let pinned = vec![
            PinnedNode::Item(a),
            folder("外", vec![PinnedNode::Item(b), folder("空", vec![])]),
            PinnedNode::Item(c),
        ];
        let layout = build_menu_layout(&history, &pinned, &Config::default(), MenuContent::All);
        assert_eq!(layout.visible, 2);
        assert_eq!(
            layout.pinned,
            [
                PinnedMenuNode::Item(2),
                PinnedMenuNode::Folder {
                    title: "外".into(),
                    children: vec![PinnedMenuNode::Item(3), PinnedMenuNode::Folder { title: "空".into(), children: vec![] }],
                },
                PinnedMenuNode::Item(4),
            ]
        );
        let pinned_rows: Vec<_> = layout.rows[2..].iter().map(|r| (r.id, r.pinned, r.label.as_str())).collect();
        assert_eq!(pinned_rows, [(ids[0], true, "a"), (ids[1], true, "b"), (ids[2], true, "c")]);
        assert!(layout.rows[..2].iter().all(|r| !r.pinned));

        // ルートにアイテムが無く、フォルダの中だけでも出る
        let only_folder = vec![folder("F", vec![PinnedNode::Item(meta_with(Some("x"), None, &[]))])];
        let layout = build_menu_layout(&history, &only_folder, &Config::default(), MenuContent::All);
        assert!(matches!(&layout.pinned[..], [PinnedMenuNode::Folder { children, .. }] if children == &[PinnedMenuNode::Item(2)]));
    }

    /// メニューの並び: 履歴 → 区切り線 → 「ピン留め(&P)」の子メニュー。フォルダは入れ子の子メニュー、
    /// 空のフォルダには選べない行が1つ。コマンド ID は `rows` の添字 + 1。
    #[test]
    fn popup_menu_puts_pinned_tree_in_nested_submenus() {
        use windows::Win32::UI::WindowsAndMessaging::{GetMenuItemCount, GetMenuItemID, GetMenuState, GetSubMenu, MF_BYPOSITION, MF_SEPARATOR};
        let _gui = crate::tray::lock_gui_resource_tests();
        let history = history_of(vec![meta_with(Some("h1"), None, &[]), meta_with(Some("h2"), None, &[])]);
        let pinned = vec![
            PinnedNode::Item(meta_with(Some("a"), None, &[])),
            folder("外", vec![PinnedNode::Item(meta_with(Some("b"), None, &[])), folder("空", vec![])]),
            PinnedNode::Item(meta_with(Some("c"), None, &[])),
        ];
        let layout = build_menu_layout(&history, &pinned, &Config::default(), MenuContent::All);
        let mut popup = PopupMenu::new(96, None).unwrap();
        fill_popup_menu(&mut popup, &layout, 32);
        let root = popup.handle();
        unsafe {
            let ids = |menu: HMENU| (0..GetMenuItemCount(Some(menu))).map(|i| GetMenuItemID(menu, i) as i32).collect::<Vec<_>>();
            // 子メニューの入口の ID は -1
            assert_eq!(ids(root), [1, 2, 0, -1]);
            assert_ne!(GetMenuState(root, 2, MF_BYPOSITION) & MF_SEPARATOR.0, 0, "区切り線が無い");
            let pins = GetSubMenu(root, 3);
            assert_eq!(ids(pins), [3, -1, 5]);
            let outer = GetSubMenu(pins, 1);
            assert_eq!(ids(outer), [4, -1]);
            let empty = GetSubMenu(outer, 1);
            assert_eq!(ids(empty), [0]);
            assert_ne!(GetMenuState(empty, 0, MF_BYPOSITION) & MF_GRAYED.0, 0, "空のフォルダの行が選べる");
        }

        // ピン留めが無ければ、区切り線も子メニューも出さない
        let layout = build_menu_layout(&history, &[], &Config::default(), MenuContent::All);
        let mut popup = PopupMenu::new(96, None).unwrap();
        fill_popup_menu(&mut popup, &layout, 32);
        assert_eq!(unsafe { GetMenuItemCount(Some(popup.handle())) }, 2);

        // 履歴が無くピン留めだけなら「（履歴なし）」の後に区切り線と子メニュー
        let layout = build_menu_layout(&history_of(vec![]), &pinned, &Config::default(), MenuContent::All);
        let mut popup = PopupMenu::new(96, None).unwrap();
        fill_popup_menu(&mut popup, &layout, 32);
        let root = popup.handle();
        unsafe {
            assert_eq!(GetMenuItemCount(Some(root)), 3);
            assert_eq!(GetMenuItemID(root, 0), 0);
            // 「（空）」と同じく選べない
            assert_ne!(GetMenuState(root, 0, MF_BYPOSITION) & MF_GRAYED.0, 0, "（履歴なし）が選べる");
            assert!(!GetSubMenu(root, 2).is_invalid());
        }
    }

    /// ピン留めを上に出す設定（`hotkey.menu_pinned_first`）: 「ピン留め(&P)」→ 区切り線 → 履歴。子メニューの
    /// 中身とコマンド ID は並びによらない。ピン留めが無ければ区切り線も子メニューも出さず、履歴が無ければ最後が
    /// 「（履歴なし）」。ピン留めのみ・履歴のみのメニューには効かない。
    #[test]
    fn pinned_first_puts_pinned_submenu_above_history() {
        use windows::Win32::UI::WindowsAndMessaging::{GetMenuItemCount, GetMenuItemID, GetMenuState, GetSubMenu, MF_BYPOSITION, MF_SEPARATOR};
        let _gui = crate::tray::lock_gui_resource_tests();
        let history = history_of(vec![meta_with(Some("h1"), None, &[]), meta_with(Some("h2"), None, &[])]);
        let pinned = vec![
            PinnedNode::Item(meta_with(Some("a"), None, &[])),
            folder("外", vec![PinnedNode::Item(meta_with(Some("b"), None, &[]))]),
        ];
        let mut config = Config::default();
        config.hotkey.menu_pinned_first = true;
        let menu_of = |history: &store::History, pinned: &[PinnedNode], content| {
            let layout = build_menu_layout(history, pinned, &config, content);
            let mut popup = PopupMenu::new(96, None).unwrap();
            fill_popup_menu(&mut popup, &layout, 32);
            popup
        };
        let ids = |menu: HMENU| unsafe { (0..GetMenuItemCount(Some(menu))).map(|i| GetMenuItemID(menu, i) as i32).collect::<Vec<_>>() };

        let popup = menu_of(&history, &pinned, MenuContent::All);
        let root = popup.handle();
        assert_eq!(ids(root), [-1, 0, 1, 2]);
        assert_ne!(unsafe { GetMenuState(root, 1, MF_BYPOSITION) } & MF_SEPARATOR.0, 0, "区切り線が無い");
        let pins = unsafe { GetSubMenu(root, 0) };
        assert_eq!(ids(pins), [3, -1]);
        assert_eq!(ids(unsafe { GetSubMenu(pins, 1) }), [4]);

        let popup = menu_of(&history, &[], MenuContent::All);
        assert_eq!(ids(popup.handle()), [1, 2]);

        let popup = menu_of(&history_of(vec![]), &pinned, MenuContent::All);
        let root = popup.handle();
        assert_eq!(ids(root), [-1, 0, 0]);
        assert_ne!(unsafe { GetMenuState(root, 1, MF_BYPOSITION) } & MF_SEPARATOR.0, 0, "区切り線が無い");
        assert_eq!(unsafe { GetMenuState(root, 2, MF_BYPOSITION) } & MF_SEPARATOR.0, 0, "最後が（履歴なし）でない");
        assert_ne!(unsafe { GetMenuState(root, 2, MF_BYPOSITION) } & MF_GRAYED.0, 0, "（履歴なし）が選べる");

        assert_eq!(ids(menu_of(&history, &pinned, MenuContent::PinnedOnly).handle()), [1, -1]);
        assert_eq!(ids(menu_of(&history, &pinned, MenuContent::HistoryOnly).handle()), [1, 2]);
    }

    /// 二度押しのメニューの種類: ピン留めのみはルートの中身を直に並べ（履歴は出さない。無ければ
    /// 選べない「（ピン留めなし）」）、履歴のみは「ピン留め(&P)」を付けない。
    #[test]
    fn menu_content_selects_pinned_or_history_only() {
        use windows::Win32::UI::WindowsAndMessaging::{GetMenuItemCount, GetMenuItemID, GetMenuState, GetSubMenu, MF_BYPOSITION};
        let _gui = crate::tray::lock_gui_resource_tests();
        let history = history_of(vec![meta_with(Some("h1"), None, &[]), meta_with(Some("h2"), None, &[])]);
        let pinned = vec![
            PinnedNode::Item(meta_with(Some("a"), None, &[])),
            folder("外", vec![PinnedNode::Item(meta_with(Some("b"), None, &[]))]),
        ];
        let menu_of = |history: &store::History, pinned: &[PinnedNode], content| {
            let layout = build_menu_layout(history, pinned, &Config::default(), content);
            let mut popup = PopupMenu::new(96, None).unwrap();
            fill_popup_menu(&mut popup, &layout, 32);
            (layout, popup)
        };
        let ids = |menu: HMENU| unsafe { (0..GetMenuItemCount(Some(menu))).map(|i| GetMenuItemID(menu, i) as i32).collect::<Vec<_>>() };

        let (layout, popup) = menu_of(&history, &pinned, MenuContent::PinnedOnly);
        assert!(layout.rows.iter().all(|r| r.pinned), "ピン留めのみに履歴の行がある");
        assert_eq!(ids(popup.handle()), [1, -1]);
        assert_eq!(ids(unsafe { GetSubMenu(popup.handle(), 1) }), [2]);

        let (_layout, popup) = menu_of(&history, &[], MenuContent::PinnedOnly);
        assert_eq!(ids(popup.handle()), [0]);
        assert_ne!(unsafe { GetMenuState(popup.handle(), 0, MF_BYPOSITION) } & MF_GRAYED.0, 0, "（ピン留めなし）が選べる");

        let (layout, popup) = menu_of(&history, &pinned, MenuContent::HistoryOnly);
        assert!(layout.rows.iter().all(|r| !r.pinned), "履歴のみにピン留めの行がある");
        assert_eq!(ids(popup.handle()), [1, 2]);
    }

    #[test]
    fn menu_label_prefers_title_over_preview_and_fallback() {
        let meta = meta_with(Some("タイトル"), Some("プレビュー"), &[]);
        assert_eq!(menu_label(&meta), "タイトル");
    }

    #[test]
    fn menu_label_falls_back_to_preview_when_title_is_none() {
        let meta = meta_with(None, Some("プレビュー"), &[]);
        assert_eq!(menu_label(&meta), "プレビュー");
    }

    #[test]
    fn menu_label_falls_back_to_format_based_placeholder() {
        assert_eq!(menu_label(&meta_with(None, None, &["CF_DIB"])), "（画像）");
        assert_eq!(menu_label(&meta_with(None, None, &["CF_HDROP"])), "（ファイル）");
        assert_eq!(menu_label(&meta_with(None, None, &["CF_UNICODETEXT"])), "（データ）");
    }

    #[test]
    fn menu_label_takes_only_first_line() {
        let meta = meta_with(Some("1行目\n2行目"), None, &[]);
        assert_eq!(menu_label(&meta), "1行目");
    }

    #[test]
    fn menu_label_truncates_at_fifty_chars() {
        let long = "あ".repeat(60);
        let meta = meta_with(Some(&long), None, &[]);
        let label = menu_label(&meta);
        assert_eq!(label.chars().count(), 50);
        assert_eq!(label, "あ".repeat(50));
    }

    #[test]
    fn menu_label_exactly_fifty_chars_is_not_truncated_further() {
        let exact = "あ".repeat(50);
        let meta = meta_with(Some(&exact), None, &[]);
        assert_eq!(menu_label(&meta).chars().count(), 50);
    }

    #[test]
    fn escape_menu_text_doubles_ampersand() {
        // "&"はWin32メニューのニーモニック指定のため、リテラル表示には"&&"が要る
        // （実機確認: 未エスケープだと"Q&Amp;A"が"QAmp;A"になり"A"に意図しない下線）
        assert_eq!(escape_menu_text("Q&Amp;A"), "Q&&Amp;A");
        assert_eq!(escape_menu_text("A&B&C"), "A&&B&&C");
        assert_eq!(escape_menu_text("no ampersand"), "no ampersand");
    }

    #[test]
    fn accel_prefix_covers_1_to_9_then_0_then_none() {
        assert_eq!(accel_prefix(1), "&1 ");
        assert_eq!(accel_prefix(9), "&9 ");
        assert_eq!(accel_prefix(10), "&0 ");
        assert_eq!(accel_prefix(11), "");
        assert_eq!(accel_prefix(0), ""); // 呼び出し側は1始まりのみ渡すが念のため
    }

    #[test]
    fn thumb_name_is_none_without_dib_thumb() {
        // thumbを持たないCF_DIBメタ（小画像相当）はNone
        let meta = meta_with(None, None, &["CF_DIB"]);
        assert!(thumb_name(&meta).is_none());
    }

    #[test]
    fn hotkey_parsing() {
        let hk = |enabled, mods: &[&str], key: &str| Hotkey {
            enabled,
            modifiers: mods.iter().map(|s| s.to_string()).collect(),
            key: key.to_string(),
        };
        assert_eq!(
            to_win32(&hk(true, &["alt"], "C")),
            Some((MOD_ALT, u32::from('C')))
        );
        assert_eq!(
            to_win32(&hk(true, &["ctrl", "shift"], "v")),
            Some((MOD_CONTROL | MOD_SHIFT, u32::from('V')))
        );
        assert_eq!(to_win32(&hk(false, &["alt"], "C")), None); // 無効
        assert_eq!(to_win32(&hk(true, &["alt"], "")), None); // キーなし
        assert_eq!(to_win32(&hk(true, &["alt"], "CX")), None); // 2文字
        assert_eq!(to_win32(&hk(true, &["hyper"], "C")), None); // 未知の修飾
    }

    #[test]
    fn is_valid_hotkey_key_rejects_non_ascii_alphanumeric() {
        assert!(is_valid_hotkey_key("C"));
        assert!(is_valid_hotkey_key("7"));
        assert!(!is_valid_hotkey_key("")); // 空
        assert!(!is_valid_hotkey_key("CX")); // 2文字
        assert!(!is_valid_hotkey_key("@")); // 記号
        assert!(!is_valid_hotkey_key("あ")); // 日本語（1文字だがASCII英数字でない）
        assert!(!is_valid_hotkey_key("１")); // 全角数字
    }

    /// 二度押しの判定の状態（VK・回数・前の key up の時刻）
    struct Press {
        vk: u32,
        count: u32,
        last_up: u32,
    }

    impl Press {
        fn new(vk: u32, count: u32, last_up: u32) -> Self {
            Self { vk, count, last_up }
        }

        /// 猶予 500ms で1件進める
        fn step(&mut self, vk: u32, up: bool, time: u32) -> bool {
            advance_double_press(&mut self.vk, &mut self.count, &mut self.last_up, vk, up, time, 500)
        }
    }

    #[test]
    fn advance_double_press_ignores_non_modifier_key_down() {
        let mut p = Press::new(0, 0, 0);
        let vk_a = 0x41; // 'A'、修飾キーではない
        assert!(!p.step(vk_a, false, 100));
        assert_eq!((p.vk, p.count), (0, 0));
    }

    #[test]
    fn advance_double_press_resets_state_when_non_modifier_key_is_pressed() {
        // Ctrlを1回押した状態で、その後Cを押すと二度押し判定はリセットされる
        // （Ctrl+C等を誤って二度押しと認識しない）
        let mut p = Press::new(VK_CONTROL.0 as u32, 1, 100);
        let vk_c = 0x43;
        assert!(!p.step(vk_c, true, 150));
        assert_eq!((p.vk, p.count), (0, 0));
    }

    #[test]
    fn advance_double_press_ignores_modifier_key_down() {
        // key downでは何もしない（key upのみカウントする）
        let mut p = Press::new(0, 0, 0);
        let modifier = VK_CONTROL.0 as u32;
        assert!(!p.step(modifier, false, 100));
        assert_eq!((p.vk, p.count), (0, 0));
    }

    #[test]
    fn advance_double_press_first_key_up_sets_count_to_one() {
        let mut p = Press::new(0, 0, 0);
        let modifier = VK_SHIFT.0 as u32;
        assert!(p.step(modifier, true, 100));
        assert_eq!((p.vk, p.count, p.last_up), (modifier, 1, 100));
    }

    #[test]
    fn advance_double_press_same_key_second_up_increments_count() {
        let modifier = VK_MENU.0 as u32;
        let mut p = Press::new(modifier, 1, 100);
        assert!(p.step(modifier, true, 400));
        assert_eq!((p.vk, p.count, p.last_up), (modifier, 2, 400));
    }

    #[test]
    fn advance_double_press_different_modifier_restarts_count() {
        // Ctrlを1回押した直後にShiftを押すと、Shiftの1回目としてカウントし直す
        let mut p = Press::new(VK_CONTROL.0 as u32, 1, 100);
        let shift = VK_SHIFT.0 as u32;
        assert!(p.step(shift, true, 200));
        assert_eq!((p.vk, p.count), (shift, 1));
    }

    /// 同じ修飾キーでも、前の key up から猶予を超えて起きた key up は数え直す（ホットキーのスレッドが止まって
    /// いた間の転送を続けて受け取っても、キーが起きた時刻で判定する）。
    /// 時刻が一周した直後も、差で判定する。
    #[test]
    fn advance_double_press_restarts_when_gap_exceeds_interval() {
        let ctrl = VK_CONTROL.0 as u32;
        let mut p = Press::new(ctrl, 1, 1_000);
        assert!(p.step(ctrl, true, 1_501));
        assert_eq!((p.count, p.last_up), (1, 1_501), "猶予を超えた2回を二度押しと数えた");
        assert!(p.step(ctrl, true, 2_001));
        assert_eq!(p.count, 2, "猶予ちょうどの2回を数えなかった");

        let mut p = Press::new(ctrl, 1, u32::MAX - 100);
        assert!(p.step(ctrl, true, 200));
        assert_eq!(p.count, 2, "時刻が一周した直後の2回を数えなかった");
    }

    /// 転送の詰め方は、VK・世代・時刻（最大値を含む）・key up をそのまま戻す。
    #[test]
    fn key_event_packing_round_trips() {
        for (vk, generation, time, up) in [
            (VK_CONTROL.0 as u32, 1, 0, false),
            (VK_SHIFT.0 as u32, u32::MAX, u32::MAX, true),
            (VK_MENU.0 as u32, 7, 0x8000_0000, true),
        ] {
            let (wparam, lparam) = pack_key_event(vk, generation, time, up);
            assert_eq!(unpack_key_event(wparam, lparam), (vk, generation, time, up));
        }
    }

    /// 自分の自動貼り付けの入力（注入の印と目印の両方がある）だけを転送しない。注入の印がない入力や、
    /// 目印のない注入（ほかのアプリの `SendInput` など）は転送する。
    #[test]
    fn own_paste_input_is_not_forwarded() {
        let mark = crate::paste::PASTE_INPUT_MARK;
        assert!(!should_forward(true, mark));
        assert!(should_forward(true, 0));
        assert!(should_forward(false, mark));
        assert!(should_forward(false, 0));
    }

    /// フックのスレッドは張って起き、破棄で止まって戻る（止める依頼が届かずに待ち続けない）。
    #[test]
    fn hook_thread_starts_and_stops() {
        use std::time::{Duration, Instant};

        // 実際にフックを張る（転送先の `HOOK_TARGET` を読む）ため、ほかのフックのテストと直列化する
        let _guard = crate::tray::lock_gui_resource_tests();
        for _ in 0..3 {
            let thread = HookThread::spawn().expect("キーフックを張れなかった");
            assert_ne!(thread.thread_id, 0);
            let started = Instant::now();
            drop(thread);
            assert!(started.elapsed() < Duration::from_secs(2), "フックのスレッドがすぐに止まらなかった");
        }
    }
}
