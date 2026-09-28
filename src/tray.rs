//! システムトレイ。
//!
//! clipboard.rsと同じ「隠しウィンドウ + メッセージループスレッド」パターン。
//! トレイ操作はchannel経由でUIスレッドへ通知し、UI側の状態変更（アイコン切替・
//! 破棄）はトレイウィンドウへのPostMessageで依頼する。
//!
//! アイコンはMaterial Symbols Sharpの埋め込みRGBAデータ（res/icons/）から生成する。
//! 左クリック=LeftClickイベント（native/app.rsが受けてhotkey.rsのポップアップメニューを
//! マウス位置固定で表示する。ホットキースレッド起動失敗時のみビューア表示に
//! フォールバック）、ダブルクリック=ShowViewerイベント、右クリック=
//! ネイティブ管理メニューとする。
//!
//! 単クリックとダブルクリックの区別はC版と同じタイマー方式。1回目の左ボタン
//! アップで`GetDoubleClickTime`のタイマーを張り、時間内に2回目のアップが来れば
//! ダブルクリック、来なければタイマー満了でLeftClickとする（単クリックの反応が
//! 判定時間ぶん遅れる）。`WM_LBUTTONDBLCLK`は使わず、アップの回数で自前判定する
//! （通常のウィンドウと違い、トレイのコールバックでの到達がドキュメントで
//! 確認できないため）。

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use windows::core::{w, Error as WinError, BOOL, PCWSTR, Result as WinResult};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, CreateIconIndirect, DefWindowProcW,
    DestroyIcon, DestroyWindow, DispatchMessageW, EndMenu, GetCursorPos, GetMessageW,
    KillTimer, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow,
    SetTimer, SetWindowLongPtrW, TrackPopupMenu, TranslateMessage, CW_USEDEFAULT, GWLP_USERDATA,
    GetWindowLongPtrW, HICON, ICONINFO, MF_CHECKED,
    MF_UNCHECKED, MSG, TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_CLOSE, WM_DESTROY,
    WM_DRAWITEM, WM_LBUTTONUP, WM_MEASUREITEM, WM_MENUCHAR, WM_NCCREATE, WM_RBUTTONUP, WM_TIMER, WM_USER,
    WNDCLASSW, WS_OVERLAPPED,
};

use crate::menu_draw::{self, PopupMenu};

// --- Public events ---

/// トレイ操作のUIへの通知。
pub enum TrayEvent {
    /// 左クリック（native/app.rsがhotkey.rsのポップアップメニューをマウス位置固定で表示）。
    /// ダブルクリックでないと確定してから（判定時間経過後に）送られる
    LeftClick,
    /// ダブルクリック、またはメニュー「ビューアを表示」
    ShowViewer,
    /// メニュー「クリップボードを監視」トグル
    ToggleWatch,
    /// メニュー「終了」
    Exit,
}

// --- Window messages / menu ids ---

const WM_TRAY_NOTIFY: u32 = WM_USER + 1;
/// アイコン再設定依頼（wparam: 0=監視OFF, 1=監視ON）
const WM_APP_SET_ICON: u32 = WM_USER + 2;
const WM_APP_SHUTDOWN: u32 = WM_USER + 3;

const MENU_ID_VIEWER: usize = 1;
const MENU_ID_WATCH: usize = 2;
const MENU_ID_EXIT: usize = 3;

const TRAY_ID: u32 = 1;
const CLASS_NAME: PCWSTR = w!("CLCLR_Tray");
/// 単クリック/ダブルクリック判別タイマー
const CLICK_TIMER_ID: usize = 1;

/// タスクバーを作ったときにブロードキャストされる登録メッセージ（`TaskbarCreated`）の番号。登録できなければ 0
/// （0 のメッセージは届かない扱い）。受けたらアイコンを付け直す（エクスプローラーの再起動で消えたアイコンを戻す。
/// Microsoft Learn「The Taskbar」の Taskbar Creation Notification）
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

fn is_taskbar_created(msg: u32) -> bool {
    msg != 0 && msg == TASKBAR_CREATED.load(Ordering::SeqCst)
}

// --- Click discrimination ---

/// 左ボタンアップ1回分の判定結果。
#[derive(Debug, PartialEq, Eq)]
enum ClickAction {
    /// 1回目のアップ: 判定時間のタイマーを張って2回目を待つ
    StartTimer,
    /// 判定時間内の2回目のアップ: タイマーを止めてダブルクリックとして扱う
    DoubleClick,
}

/// 単クリックとダブルクリックを区別する状態（Win32非依存。テスト用に切り出し）。
#[derive(Default)]
struct ClickTracker {
    /// 1回目のアップを受け、タイマー満了か2回目のアップを待っている
    pending: bool,
}

impl ClickTracker {
    fn on_left_up(&mut self) -> ClickAction {
        if self.pending {
            self.pending = false;
            ClickAction::DoubleClick
        } else {
            self.pending = true;
            ClickAction::StartTimer
        }
    }

    /// タイマー満了。待機中だった場合のみtrue（=単クリック確定）。
    fn on_timer(&mut self) -> bool {
        std::mem::take(&mut self.pending)
    }
}

// --- Error ---

pub type Result<T> = std::result::Result<T, WinError>;

// --- Icon ---

const TRAY_ICON_SIZE: i32 = 32;
/// アプリアイコン（監視中）。`res/icons/app.ico`の32x32の画像をRGBAへ展開したもの
/// （Material Symbols Sharp "assignment"）。app.icoを差し替えたら、同じ手順で
/// `app_32.rgba`も作り直す（ICOはPNG圧縮のためテストでの自動突き合わせはできない）。
const TRAY_WATCH_ON_RGBA: &[u8] = include_bytes!("../res/icons/app_32.rgba");
/// Material Symbols Sharp "content_paste_off"（監視停止）。
const TRAY_WATCH_OFF_RGBA: &[u8] = include_bytes!("../res/icons/tray_watch_off_32.rgba");

/// RGBA（左上原点、R,G,B,A順）を、Win32の`CreateBitmap`が期待する32bpp ARGB
/// （リトルエンディアンでメモリ配置するとBGRA）のピクセル配列に詰め替える。
/// menu_tooltip.rsのツールチップのサムネイルでも共用する。
pub(crate) fn rgba_to_argb_pixels(rgba: &[u8]) -> Vec<u32> {
    rgba.chunks_exact(4)
        .map(|c| (u32::from(c[3]) << 24) | (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]))
        .collect()
}

/// 埋め込みRGBA（左上原点、R,G,B,A順）から32bpp ARGBビットマップ経由でHICONを作る。
/// ビューアの種別アイコン（native/viewer.rs）でも共用する。
pub(crate) fn icon_from_rgba(rgba: &[u8], size: i32) -> WinResult<HICON> {
    let pixels = rgba_to_argb_pixels(rgba);

    unsafe {
        let color = CreateBitmap(size, size, 1, 32, Some(pixels.as_ptr().cast()));
        // マスクは全ピクセル不透明（ARGBのアルファで制御するため全0）
        let mask_bits = vec![0u8; (size * size / 8) as usize];
        let mask = CreateBitmap(size, size, 1, 1, Some(mask_bits.as_ptr().cast()));
        let info = ICONINFO {
            fIcon: true.into(),
            hbmMask: mask,
            hbmColor: color,
            ..Default::default()
        };
        let icon = CreateIconIndirect(&info);
        // CreateIconIndirectはビットマップをコピーするだけで所有権を取らない仕様のため、
        // 成否に関わらずここで解放する（呼び出しのたび=起動時・監視ON/OFF切替のたびに
        // GDIハンドルが2個ずつリークしていた問題の修正）
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

/// 監視ON/OFFに応じたトレイアイコンを取得する。
fn tray_icon(watch_enabled: bool) -> WinResult<HICON> {
    let rgba = if watch_enabled {
        TRAY_WATCH_ON_RGBA
    } else {
        TRAY_WATCH_OFF_RGBA
    };
    icon_from_rgba(rgba, TRAY_ICON_SIZE)
}

// --- Tray thread ---

/// トレイウィンドウのコンテキスト。`GWLP_USERDATA`経由で`&TrayContext`（共有参照）として
/// 取り出す。`TrackPopupMenu`のモーダルループ中は`wndproc`が再入するため、可変な状態は
/// `Cell`/`RefCell`に持ち、`&mut`の別名を作らない。加えて、メニュー表示中に終了依頼が
/// 届いてもコンテキストを解放せず、メニューが閉じた後の最外周で処理する
/// （`shutdown_pending`。解放後の参照を防ぐ）。
struct TrayContext {
    events: Sender<TrayEvent>,
    /// メニューのチェック状態表示用。監視の実際の状態（`ClipboardWatcher::watch_state`）で、設定の値は
    /// 読まない（登録に失敗すると食い違うため）
    watch_state: Arc<AtomicBool>,
    /// 現在表示中のアイコン（差し替え時に破棄する）
    icon: Cell<HICON>,
    /// 表示中のアイコンが監視中のもの（true）か停止中のもの（false）か（トレイのスレッドが書き、テストが
    /// 別のスレッドから読むので `AtomicBool`）
    icon_watch: AtomicBool,
    /// 通知領域にアイコンを付けられているか（最後の登録・付け直しの結果。`Tray::icon_shown` が読む）
    icon_shown: Arc<AtomicBool>,
    /// 通知用（イベント送信後にUIを起こす）
    repaint: Box<dyn Fn() + Send>,
    /// 単クリック/ダブルクリックの判別状態（借用は各呼び出しの間だけ。モーダル呼び出しを
    /// またいで保持しない）
    clicks: RefCell<ClickTracker>,
    /// `TrackPopupMenu`のモーダルループ中か（再入時の多重表示・解放の防止）
    menu_open: Cell<bool>,
    /// メニュー表示中に届いた終了依頼。メニューが閉じてから`DestroyWindow`する
    shutdown_pending: Cell<bool>,
}

/// `GWLP_USERDATA`のコンテキストを共有参照で取り出す。窓の破棄後（null）は`None`。
/// 返す参照の有効期間は呼び出し側の責任: `WM_DESTROY`まで（=メニュー表示中は解放されない）。
unsafe fn ctx_ref<'a>(hwnd: HWND) -> Option<&'a TrayContext> {
    unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const TrayContext).as_ref() }
}

fn notify_icon_data(hwnd: HWND, icon: HICON) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY_NOTIFY,
        hIcon: icon,
        ..Default::default()
    };
    let tip: Vec<u16> = crate::APP_DISPLAY_NAME.encode_utf16().collect();
    data.szTip[..tip.len()].copy_from_slice(&tip);
    data
}

/// トレイアイコンを登録する。C版準拠でNIM_MODIFY→失敗ならNIM_ADD。付けられたか（どちらかが成功したか）を返す
/// （タスクバーができる前の登録などで失敗する）。
fn add_or_update_icon(hwnd: HWND, icon: HICON) -> bool {
    let data = notify_icon_data(hwnd, icon);
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &data).as_bool() || Shell_NotifyIconW(NIM_ADD, &data).as_bool() }
}

/// アイコンを付け（付け直し）、結果を `icon_shown` に入れる。付けられなければログへ書く。
fn show_icon(hwnd: HWND, ctx: &TrayContext) {
    let shown = add_or_update_icon(hwnd, ctx.icon.get());
    ctx.icon_shown.store(shown, Ordering::SeqCst);
    if !shown {
        eprintln!("トレイアイコンを通知領域に付けられませんでした（タスクバーができたら付け直します）");
    }
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

/// トレイのメニューを作る。「クリップボードを監視」のチェックは `watch`（監視の実際の状態。設定の値は
/// 読まない）。作れなければ None。
fn build_tray_menu(dpi: u32, watch: bool) -> Option<PopupMenu> {
    let mut popup = PopupMenu::new(dpi, None).ok()?;
    let menu = popup.handle();
    let watch_flag = if watch { MF_CHECKED } else { MF_UNCHECKED };
    popup.append_command(menu, MENU_ID_VIEWER, "ビューアを表示", MF_UNCHECKED, None);
    popup.append_command(menu, MENU_ID_WATCH, "クリップボードを監視", watch_flag, None);
    popup.append_separator(menu);
    popup.append_command(menu, MENU_ID_EXIT, "終了", MF_UNCHECKED, None);
    Some(popup)
}

fn show_tray_menu(hwnd: HWND, ctx: &TrayContext) {
    // モーダルループ中の再入（メニュー表示中のトレイ操作）では二重に開かない
    if ctx.menu_open.get() {
        return;
    }
    // メニュー表示からUIへの通知（`events.send`・`repaint`コールバック）の完了まで処理中とする。
    // この間に届いた終了依頼は保留され、`ctx`は関数が戻るまで解放されない
    let _busy = BusyGuard::enter(&ctx.menu_open);
    unsafe {
        let mut pos = POINT::default();
        let _ = GetCursorPos(&mut pos);
        // メニューを出す位置（カーソル）のモニターの DPI で描く
        let dpi = crate::menu_tooltip::dpi_at_point(pos.x, pos.y);
        let Some(popup) = build_tray_menu(dpi, ctx.watch_state.load(Ordering::SeqCst)) else {
            return;
        };
        let menu = popup.handle();

        // トレイメニューの定石: 前面化しないとメニューが閉じなくなる
        let _ = SetForegroundWindow(hwnd);
        // TrackPopupMenuの間はwndprocが再入する。`ctx`は共有参照で、この間に解放されない
        // （終了依頼は`shutdown_pending`に保留される）ため、戻った後も安全に使える。
        // 表示前に終了が依頼されていた場合（`EndMenu`は閉じる対象がなく空振りする）は、
        // メニューを開かない（開くと閉じる契機がなくなり、`Drop`のjoinが戻らなくなる）
        let cmd = if ctx.shutdown_pending.get() {
            BOOL(0)
        } else {
            TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
                pos.x,
                pos.y,
                Some(0),
                hwnd,
                None,
            )
        };
        let _ = PostMessageW(Some(hwnd), 0, WPARAM(0), LPARAM(0)); // WM_NULL（メニュー後始末の定石）
        // メニューを破棄してから、項目の描画の材料（フォント）を解放する
        drop(popup);

        // 終了依頼でメニューが閉じられた場合は、選択結果があってもUIへ送らない
        if ctx.shutdown_pending.get() {
            return;
        }
        let event = match cmd.0 as usize {
            MENU_ID_VIEWER => Some(TrayEvent::ShowViewer),
            MENU_ID_WATCH => Some(TrayEvent::ToggleWatch),
            MENU_ID_EXIT => Some(TrayEvent::Exit),
            _ => None,
        };
        if let Some(event) = event {
            let _ = ctx.events.send(event);
            (ctx.repaint)();
        }
    }
}

/// メニュー表示中に保留された終了依頼を、メニューが閉じている場合に限って実行する。
/// メニューのモーダルループ中に再入したwndprocから呼ばれても`menu_open`で弾かれ、
/// 外側の`show_tray_menu`が戻った後の呼び出しで実行される。
unsafe fn run_deferred_shutdown(hwnd: HWND) {
    let due = unsafe { ctx_ref(hwnd) }
        .is_some_and(|ctx| ctx.shutdown_pending.get() && !ctx.menu_open.get());
    if due {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            // GWLP_USERDATAはlpCreateParams経由ではなく、CreateWindowExW成功後に
            // `Tray::spawn`側で明示的に設定する（下記コメント参照）。WM_NCCREATEは
            // 素通しでよい
            WM_NCCREATE => DefWindowProcW(hwnd, msg, wparam, lparam),
            WM_TRAY_NOTIFY => {
                if let Some(ctx) = ctx_ref(hwnd) {
                    match lparam.0 as u32 {
                        WM_LBUTTONUP => {
                            let action = ctx.clicks.borrow_mut().on_left_up();
                            match action {
                                ClickAction::StartTimer => {
                                    SetTimer(Some(hwnd), CLICK_TIMER_ID, GetDoubleClickTime(), None);
                                }
                                ClickAction::DoubleClick => {
                                    let _ = KillTimer(Some(hwnd), CLICK_TIMER_ID);
                                    let _ = ctx.events.send(TrayEvent::ShowViewer);
                                    (ctx.repaint)();
                                }
                            }
                        }
                        WM_RBUTTONUP => show_tray_menu(hwnd, ctx),
                        _ => {}
                    }
                }
                // `ctx`の参照はここで終わっている。メニュー表示中に終了依頼が届いていれば、
                // メニューが閉じた今、コンテキストを解放してよい
                run_deferred_shutdown(hwnd);
                LRESULT(0)
            }
            // メニューの自前描画（`show_tray_menu` の `PopupMenu` の項目。モーダル中に再入して届く。
            // 項目のデータを共有参照で読むだけで、`ctx` の状態には触らない）
            WM_MEASUREITEM if menu_draw::on_measure_item(hwnd, lparam) => LRESULT(1),
            WM_DRAWITEM if menu_draw::on_draw_item(lparam) => LRESULT(1),
            WM_MENUCHAR => match menu_draw::on_menu_char(wparam, lparam) {
                Some(result) => result,
                None => DefWindowProcW(hwnd, msg, wparam, lparam),
            },
            WM_TIMER if wparam.0 == CLICK_TIMER_ID => {
                let _ = KillTimer(Some(hwnd), CLICK_TIMER_ID);
                if let Some(ctx) = ctx_ref(hwnd) {
                    let confirmed = ctx.clicks.borrow_mut().on_timer();
                    if confirmed {
                        let _ = ctx.events.send(TrayEvent::LeftClick);
                        (ctx.repaint)();
                    }
                }
                LRESULT(0)
            }
            WM_APP_SET_ICON => {
                if let Some(ctx) = ctx_ref(hwnd) {
                    if let Ok(icon) = tray_icon(wparam.0 != 0) {
                        let old = ctx.icon.replace(icon);
                        show_icon(hwnd, ctx);
                        let _ = DestroyIcon(old);
                        ctx.icon_watch.store(wparam.0 != 0, Ordering::SeqCst);
                    }
                }
                LRESULT(0)
            }
            // タスクバーを作った（エクスプローラーの再起動など）: 付けていたアイコンは消えたものとして付け直す
            // （主ディスプレイの DPI の変更でも届き、そのときはアイコンが残っているので、NIM_MODIFY から試す）
            m if is_taskbar_created(m) => {
                if let Some(ctx) = ctx_ref(hwnd) {
                    show_icon(hwnd, ctx);
                }
                LRESULT(0)
            }
            // WM_CLOSEは既定処理（DefWindowProcW）がDestroyWindowを呼ぶため、外部から届いても
            // メニュー表示中に窓が破棄されないよう、独自の終了依頼と同じ保留経路に通す
            WM_CLOSE | WM_APP_SHUTDOWN => {
                match ctx_ref(hwnd) {
                    // メニュー表示中: ここで窓を破棄するとTrackPopupMenuから戻った先の
                    // `ctx`が解放済みになる。メニューだけ閉じ、破棄は戻った後に行う
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
                let ctx = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayContext;
                if !ctx.is_null() {
                    // メニューのモーダルループ中に解放しない（`shutdown_pending`で保留する設計の不変条件）
                    debug_assert!(!(*ctx).menu_open.get(), "TrayContext freed while menu is open");
                    let ctx = Box::from_raw(ctx);
                    let icon = ctx.icon.get();
                    let data = notify_icon_data(hwnd, icon);
                    let _ = Shell_NotifyIconW(NIM_DELETE, &data);
                    // 外から `WM_CLOSE` が届いて窓が壊れた場合も、アイコンが無いことを `Tray::icon_shown` に出す
                    ctx.icon_shown.store(false, Ordering::SeqCst);
                    let _ = DestroyIcon(icon);
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// `CreateWindowExW`の結果を受けて、成功なら`ctx`をウィンドウへ結び付け、
/// 失敗なら`ctx`・`icon`を手動解放する。`ctx`はこの時点でまだどのウィンドウにも
/// 結び付いていないことが呼び出し元（`Tray::spawn`）側で保証されているため、
/// 失敗時の解放はWM_DESTROYとの二重解放を起こさない。ロジックをテスト可能に
/// するため`CreateWindowExW`の呼び出し自体から切り出した
/// （2026-09-16修正、起動時`CreateWindowExW`失敗でTrayContextとアイコンが
/// リークしていた問題）。
fn finish_window_creation(
    create_result: WinResult<HWND>,
    ctx: *mut TrayContext,
    icon: HICON,
) -> WinResult<HWND> {
    let hwnd = match create_result {
        Ok(hwnd) => hwnd,
        Err(e) => {
            unsafe {
                let _ = DestroyIcon(icon);
                drop(Box::from_raw(ctx));
            }
            return Err(e);
        }
    };
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, ctx as isize);
        show_icon(hwnd, &*ctx);
    }
    Ok(hwnd)
}

// --- Public handle ---

/// HWNDをスレッド間で受け渡すためのラッパー（clipboard.rsのSendHwndと同趣旨）。
struct SendHwnd(HWND);
unsafe impl Send for SendHwnd {}

/// トレイの起動ハンドル。Drop時にアイコン・ウィンドウ・スレッドを片付ける。
pub struct Tray {
    hwnd: HWND,
    thread: Option<JoinHandle<()>>,
    icon_shown: Arc<AtomicBool>,
}

impl Tray {
    /// トレイを開始する。`events`へ操作を送信し、送信後に`repaint`を呼んでUIを起こす。最初のアイコンと
    /// メニューのチェックは、監視の実際の状態（`watch_state`）で決める。
    pub fn spawn(
        watch_state: Arc<AtomicBool>,
        events: Sender<TrayEvent>,
        repaint: impl Fn() + Send + 'static,
    ) -> Result<Self> {
        let watch = watch_state.load(Ordering::SeqCst);
        let (hwnd_tx, hwnd_rx) = std::sync::mpsc::channel::<WinResult<SendHwnd>>();
        let icon_shown = Arc::new(AtomicBool::new(false));
        let ctx_icon_shown = Arc::clone(&icon_shown);
        if TASKBAR_CREATED.load(Ordering::SeqCst) == 0 {
            TASKBAR_CREATED.store(unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }, Ordering::SeqCst);
        }

        let thread = thread::spawn(move || {
            let create = || -> WinResult<HWND> {
                unsafe {
                    let hinstance = GetModuleHandleW(None)?.into();
                    let class = WNDCLASSW {
                        lpfnWndProc: Some(wndproc),
                        hInstance: hinstance,
                        lpszClassName: CLASS_NAME,
                        ..Default::default()
                    };
                    RegisterClassW(&class);

                    let icon = tray_icon(watch)?;
                    let ctx = Box::into_raw(Box::new(TrayContext {
                        events,
                        watch_state,
                        icon: Cell::new(icon),
                        icon_watch: AtomicBool::new(watch),
                        icon_shown: ctx_icon_shown,
                        repaint: Box::new(repaint),
                        clicks: RefCell::new(ClickTracker::default()),
                        menu_open: Cell::new(false),
                        shutdown_pending: Cell::new(false),
                    }));
                    // ctxはlpCreateParamsで渡さない（＝CreateWindowExW失敗時にWM_DESTROYが
                    // 一度も走らずTrayContext・iconがリークしていた問題の修正）。成功時のみ
                    // 明示的にGWLP_USERDATAへ結び付けることで、失敗時は「まだどの
                    // ウィンドウにも結び付いていない」ことが保証され、finish_window_creation
                    // 側で迷いなく手動解放できる（WM_DESTROYとの二重解放の心配がない）。
                    // 窓はメッセージ専用窓（HWND_MESSAGE）ではなく、表示しない通常のトップレベル窓にする
                    // （メッセージ専用窓はブロードキャストを受け取らず、`TaskbarCreated` が届かない。
                    // Microsoft Learn「Window Features」）
                    let create_result = CreateWindowExW(
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
                        None,
                    );
                    finish_window_creation(create_result, ctx, icon)
                }
            };
            match create() {
                Ok(hwnd) => {
                    let _ = hwnd_tx.send(Ok(SendHwnd(hwnd)));
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

        let hwnd = hwnd_rx
            .recv()
            .map_err(|_| WinError::from_hresult(windows::Win32::Foundation::E_FAIL))??
            .0;

        Ok(Self {
            hwnd,
            thread: Some(thread),
            icon_shown,
        })
    }

    /// 通知領域にアイコンを付けられているか（最後の登録・付け直しの結果）。付けられていない間は、ビューアを
    /// 隠して起動しない（窓へ戻る手段が見えないため）。エクスプローラーが落ちてアイコンが消えたことまでは
    /// 分からない（付け直しの `TaskbarCreated` が届くまで真のまま）。
    pub fn icon_shown(&self) -> bool {
        self.icon_shown.load(Ordering::SeqCst)
    }

    /// テスト用: 通知領域にアイコンを付けられなかった状態にする（`icon_shown` だけを偽にする）。
    #[cfg(test)]
    pub(crate) fn mark_icon_not_shown(&self) {
        self.icon_shown.store(false, Ordering::SeqCst);
    }

    /// 監視ON/OFFに合わせてアイコンを差し替える。
    pub fn set_watch_icon(&self, watch_enabled: bool) {
        unsafe {
            let _ = PostMessageW(
                Some(self.hwnd),
                WM_APP_SET_ICON,
                WPARAM(usize::from(watch_enabled)),
                LPARAM(0),
            );
        }
    }
}

impl Drop for Tray {
    /// 終了を依頼し、スレッドが終わるまで、送られたメッセージに応じながら待つ（`ui_thread::stop_ui_thread`。
    /// メニューの表示中は、止める依頼の投げ直しのたびに `EndMenu` をやり直す）。
    fn drop(&mut self) {
        if let Some(t) = self.thread.take() {
            crate::ui_thread::stop_ui_thread(self.hwnd, WM_APP_SHUTDOWN, t, crate::ui_thread::REPOST_INTERVAL);
        }
    }
}

/// `GetGuiResources`はプロセス全体のGDI/Userオブジェクト数を返すため、
/// 複数のテストが並列に実測すると互いの増減が混ざり見かけ上の
/// リークとして誤検出する（実機確認済み。clipboard.rsの
/// `CLIPBOARD_TEST_LOCK`と同じ理由）。GUIリソース数を計測するテストと、
/// 実際に窓・アイコン・ビットマップ等を作るテスト（menu_tooltip.rsを含む）は
/// このロックで直列化する。
#[cfg(test)]
pub(crate) static GUI_RESOURCE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `GUI_RESOURCE_TEST_LOCK`を取る。直前のテストがロックを持ったままパニックしても
/// （poison）続行する。そうしないと1件の失敗が後続のテストへ連鎖し、失敗件数が
/// 実際より多く見える。
#[cfg(test)]
pub(crate) fn lock_gui_resource_tests() -> std::sync::MutexGuard<'static, ()> {
    GUI_RESOURCE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// ポップアップメニューの窓（クラス`#32768`）が現れるまで待つ（モーダル中の再入テスト用）。
/// 他のアプリのメニューを誤検知しないよう、このプロセスが所有する窓だけを数える。
#[cfg(test)]
pub(crate) fn wait_for_menu_window(timeout: std::time::Duration) -> bool {
    poll_menu_window(timeout, true)
}

/// ポップアップメニューの窓が消えるまで待つ。
#[cfg(test)]
pub(crate) fn wait_for_menu_window_gone(timeout: std::time::Duration) -> bool {
    poll_menu_window(timeout, false)
}

#[cfg(test)]
fn poll_menu_window(timeout: std::time::Duration, want_present: bool) -> bool {
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, GetWindowThreadProcessId};

    /// このプロセスが所有するメニュー窓があるか
    fn own_menu_window_exists() -> bool {
        let own_pid = unsafe { GetCurrentProcessId() };
        let mut prev = HWND::default();
        while let Ok(menu) = unsafe { FindWindowExW(None, Some(prev), w!("#32768"), None) } {
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(menu, Some(&mut pid)) };
            if pid == own_pid {
                return true;
            }
            prev = menu;
        }
        false
    }

    let deadline = std::time::Instant::now() + timeout;
    loop {
        let present = own_menu_window_exists();
        if present == want_present {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetGuiResources, GR_GDIOBJECTS, GR_USEROBJECTS,
    };

    #[test]
    fn click_tracker_first_up_starts_timer() {
        let mut t = ClickTracker::default();
        assert_eq!(t.on_left_up(), ClickAction::StartTimer);
    }

    #[test]
    fn click_tracker_second_up_before_timer_is_double_click() {
        let mut t = ClickTracker::default();
        t.on_left_up();
        assert_eq!(t.on_left_up(), ClickAction::DoubleClick);
        // ダブルクリックで待機は解消済み。後から届いたタイマー満了は単クリックにしない
        assert!(!t.on_timer());
    }

    #[test]
    fn click_tracker_timer_expiry_confirms_single_click_once() {
        let mut t = ClickTracker::default();
        t.on_left_up();
        assert!(t.on_timer());
        assert!(!t.on_timer());
    }

    #[test]
    fn click_tracker_starts_over_after_single_click_confirmed() {
        let mut t = ClickTracker::default();
        t.on_left_up();
        t.on_timer();
        assert_eq!(t.on_left_up(), ClickAction::StartTimer);
    }

    #[test]
    fn click_tracker_timer_without_click_is_ignored() {
        let mut t = ClickTracker::default();
        assert!(!t.on_timer());
    }

    #[test]
    fn tray_icon_assets_have_expected_size_and_are_distinguishable() {
        // icon_from_rgbaは長さを検査せずポインタを渡すため、素材の寸法違いは
        // 範囲外読み取りになる。ここで固定する
        let bytes = (TRAY_ICON_SIZE * TRAY_ICON_SIZE * 4) as usize;
        assert_eq!(TRAY_WATCH_ON_RGBA.len(), bytes);
        assert_eq!(TRAY_WATCH_OFF_RGBA.len(), bytes);
        // 監視ON/OFFのアイコンが見分けられること
        assert_ne!(TRAY_WATCH_ON_RGBA, TRAY_WATCH_OFF_RGBA);
        // 全面透明（空の素材・変換失敗）でないこと
        assert!(TRAY_WATCH_ON_RGBA.chunks_exact(4).any(|p| p[3] > 0));
    }

    /// トレイの最初のアイコンとメニューのチェックは、渡した監視の実際の状態（共有の値）で決まる（設定の値は
    /// 読まない。登録に失敗して設定と食い違っても、実際の状態を示す）。実際の状態がオフなら停止中の
    /// アイコンでチェックなし、状態が変わればメニューのチェックも変わる。
    #[test]
    fn tray_icon_and_menu_follow_shared_watch_state() {
        let _guard = lock_gui_resource_tests();
        let (tx, _rx) = std::sync::mpsc::channel::<TrayEvent>();
        let state = Arc::new(AtomicBool::new(false));
        let tray = Tray::spawn(Arc::clone(&state), tx, || {}).unwrap();
        // トレイのスレッドと共有する値（`Arc<AtomicBool>` と `AtomicBool`）だけを読む
        let ctx = unsafe { &*(GetWindowLongPtrW(tray.hwnd, GWLP_USERDATA) as *const TrayContext) };
        assert!(Arc::ptr_eq(&ctx.watch_state, &state), "渡した状態を共有していない");
        assert!(!ctx.icon_watch.load(Ordering::SeqCst), "実際の状態がオフなのに監視中のアイコンで始めた");
        tray.set_watch_icon(true);
        // 投稿した差し替えがトレイのスレッドで処理されるのを待つ
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !ctx.icon_watch.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(ctx.icon_watch.load(Ordering::SeqCst), "アイコンを差し替えていない");
        drop(tray);

        // メニューの「クリップボードを監視」のチェックは、作ったメニューの項目で確かめる
        use windows::Win32::UI::WindowsAndMessaging::{GetMenuState, MF_BYCOMMAND};
        let checked = |watch: bool| {
            let popup = build_tray_menu(96, watch).expect("メニューを作れない");
            let state = unsafe { GetMenuState(popup.handle(), MENU_ID_WATCH as u32, MF_BYCOMMAND) };
            state & MF_CHECKED.0 != 0
        };
        assert!(!checked(false), "実際の状態がオフなのにチェックが付いた");
        assert!(checked(true), "実際の状態がオンなのにチェックが無い");
    }

    /// エクスプローラーの再起動でアイコンが消えても、`TaskbarCreated` を受けたトレイの窓が付け直す。
    /// 消えたことを `NIM_DELETE` で、再起動の知らせを登録メッセージの投稿で作る（実際のブロードキャストは、
    /// トップレベル窓であることで受け取れる。メッセージ専用窓は受け取らない）。付けられたかは
    /// `Shell_NotifyIconGetRect` で確かめる（付いていなければ失敗する）。タスクバーのない環境では検証できない。
    #[test]
    fn taskbar_created_re_adds_removed_icon() {
        use std::time::{Duration, Instant};
        use windows::Win32::UI::Shell::{Shell_NotifyIconGetRect, NOTIFYICONIDENTIFIER};
        use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GetDesktopWindow, GA_PARENT};

        let _guard = lock_gui_resource_tests();
        let (tx, _rx) = std::sync::mpsc::channel::<TrayEvent>();
        let tray = Tray::spawn(Arc::new(AtomicBool::new(true)), tx, || {}).unwrap();
        // 親はデスクトップ（トップレベル窓。メッセージ専用窓なら親はメッセージ専用のルート）
        assert_eq!(unsafe { GetAncestor(tray.hwnd, GA_PARENT) }, unsafe { GetDesktopWindow() });
        assert!(tray.icon_shown(), "アイコンを付けられなかった（タスクバーのない環境では検証できない）");
        let id = NOTIFYICONIDENTIFIER {
            cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
            hWnd: tray.hwnd,
            uID: TRAY_ID,
            ..Default::default()
        };
        let is_added = || unsafe { Shell_NotifyIconGetRect(&id) }.is_ok();
        assert!(is_added(), "付けたはずのアイコンが見つからない");

        // エクスプローラーの再起動でアイコンが消えた状態を作る
        let data = notify_icon_data(tray.hwnd, HICON::default());
        assert!(unsafe { Shell_NotifyIconW(NIM_DELETE, &data) }.as_bool());
        assert!(!is_added(), "消したアイコンが残っている");

        let msg = TASKBAR_CREATED.load(Ordering::SeqCst);
        assert_ne!(msg, 0, "TaskbarCreated を登録できていない");
        unsafe { PostMessageW(Some(tray.hwnd), msg, WPARAM(0), LPARAM(0)).unwrap() };
        let deadline = Instant::now() + Duration::from_secs(3);
        while !is_added() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(is_added(), "TaskbarCreated でアイコンを付け直さなかった");
        assert!(tray.icon_shown());

        // 外からの `WM_CLOSE` で窓が壊れたら、アイコンは外れ、`icon_shown` も偽になる
        unsafe { PostMessageW(Some(tray.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)).unwrap() };
        let deadline = Instant::now() + Duration::from_secs(3);
        while tray.icon_shown() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!tray.icon_shown(), "窓が壊れたのにアイコンがあることになっている");
        assert!(!is_added(), "窓が壊れたのにアイコンが残っている");
        drop(tray);
    }

    /// 実際のトレイウィンドウ（表示しないトップレベル窓）へ通知メッセージを直接投げ、
    /// タイマーが発火すること、単クリック→LeftClick・ダブルクリック→ShowViewerが届くことを確認する。
    /// 実シェルからの`WM_LBUTTONDBLCLK`等は関与しない（この実装はアップの回数だけで判定する）。
    #[test]
    fn tray_window_discriminates_single_and_double_click() {
        use std::sync::mpsc::RecvTimeoutError;
        use std::time::Duration;

        // 実際のトレイ窓・アイコン（USERオブジェクト）を作るため、プロセス全体の
        // GUIリソース数を実測する他のテストと直列化する
        let _guard = lock_gui_resource_tests();
        let (tx, rx) = std::sync::mpsc::channel::<TrayEvent>();
        let tray = Tray::spawn(Arc::new(AtomicBool::new(true)), tx, || {}).unwrap();
        let post_up = || unsafe {
            PostMessageW(Some(tray.hwnd), WM_TRAY_NOTIFY, WPARAM(0), LPARAM(WM_LBUTTONUP as isize)).unwrap();
        };
        // 判定時間（既定500ms）より十分長く待つ
        let wait = Duration::from_millis(u64::from(unsafe { GetDoubleClickTime() }) + 1500);

        // 単クリック: 判定時間経過後にLeftClickが1回だけ届く
        post_up();
        assert!(matches!(rx.recv_timeout(wait), Ok(TrayEvent::LeftClick)));
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(300)),
            Err(RecvTimeoutError::Timeout)
        ));

        // ダブルクリック: ShowViewerだけが届き、LeftClickは後から来ない
        post_up();
        post_up();
        assert!(matches!(rx.recv_timeout(wait), Ok(TrayEvent::ShowViewer)));
        assert!(matches!(
            rx.recv_timeout(wait),
            Err(RecvTimeoutError::Timeout)
        ));
    }

    /// メニュー表示中の終了・アイコン差し替えの回帰（解放後参照の修正）。右クリック通知で
    /// 実際に`TrackPopupMenu`のモーダルループへ入れ、その最中に`set_watch_icon`と`Drop`
    /// （=`WM_APP_SHUTDOWN`）を届ける。修正前はここで`TrayContext`が解放され、戻った先の
    /// `ctx`が解放済みになる。修正後は`WM_DESTROY`の`debug_assert`（メニュー表示中に
    /// 解放しない）が守り、Dropはメニューを閉じてから窓を破棄してスレッドをjoinできる。
    #[test]
    fn shutdown_while_menu_is_open_is_deferred_until_menu_closes() {
        shutdown_while_menu_is_open(false);
    }

    /// 外部からの`WM_CLOSE`も、既定処理の`DestroyWindow`に流さず保留する
    /// （既定処理を通ると破棄へ進む迂回経路を塞ぐ）。
    #[test]
    fn wm_close_while_menu_is_open_is_deferred_until_menu_closes() {
        shutdown_while_menu_is_open(true);
    }

    fn shutdown_while_menu_is_open(via_wm_close: bool) {
        use std::sync::mpsc::RecvTimeoutError;
        use std::time::Duration;
        use windows::Win32::UI::WindowsAndMessaging::WM_CLOSE;

        struct SendTray(Tray);
        unsafe impl Send for SendTray {}
        impl SendTray {
            fn into_inner(self) -> Tray {
                self.0
            }
        }

        let _guard = lock_gui_resource_tests();
        let (tx, rx) = std::sync::mpsc::channel::<TrayEvent>();
        let tray = Tray::spawn(Arc::new(AtomicBool::new(true)), tx, || {}).unwrap();
        let (measured, drawn) = crate::menu_draw::test_support::counts();
        unsafe {
            PostMessageW(Some(tray.hwnd), WM_TRAY_NOTIFY, WPARAM(0), LPARAM(WM_RBUTTONUP as isize)).unwrap();
        }
        assert!(
            wait_for_menu_window(Duration::from_secs(3)),
            "トレイのメニューが表示されなかった（デスクトップが使えない環境では検証できない）"
        );
        // メニューは自前描画の経路を通る（4項目: 3つのコマンドと区切り線）
        std::thread::sleep(Duration::from_millis(100));
        let (measured_after, drawn_after) = crate::menu_draw::test_support::counts();
        assert!(measured_after >= measured + 4, "項目を測っていない: {measured} → {measured_after}");
        assert!(drawn_after >= drawn + 4, "項目を描いていない: {drawn} → {drawn_after}");

        // メニュー表示中のアイコン差し替え（再入しても安全に処理される）
        tray.set_watch_icon(false);
        std::thread::sleep(Duration::from_millis(100));
        if via_wm_close {
            unsafe {
                PostMessageW(Some(tray.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)).unwrap();
            }
            assert!(
                wait_for_menu_window_gone(Duration::from_secs(3)),
                "WM_CLOSEでメニューが閉じられなかった"
            );
        }

        // メニュー表示中の終了。Dropはスレッドのjoinまで待つので、別スレッドで実行して
        // 固まった場合はタイムアウトで検出する
        let wrapped = SendTray(tray);
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
            wait_for_menu_window_gone(Duration::from_secs(3)),
            "終了後もメニューが残っている"
        );
        // 終了依頼で閉じたメニューの結果はUIへ送られない（コンテキスト解放でSenderは切断される）
        assert!(matches!(rx.recv_timeout(Duration::from_millis(300)), Err(RecvTimeoutError::Disconnected)));
    }

    #[test]
    fn rgba_to_argb_pixels_packs_channels_in_argb_order() {
        // R=0x11, G=0x22, B=0x33, A=0xFF
        let rgba = [0x11, 0x22, 0x33, 0xFF];
        assert_eq!(rgba_to_argb_pixels(&rgba), vec![0xFF11_2233]);
    }

    #[test]
    fn rgba_to_argb_pixels_handles_multiple_pixels_and_alpha() {
        let rgba = [0xFF, 0x00, 0x00, 0x80, 0x00, 0xFF, 0x00, 0x40];
        assert_eq!(
            rgba_to_argb_pixels(&rgba),
            vec![0x80FF_0000, 0x4000_FF00]
        );
    }

    /// `tray_icon`のGDIリーク回帰: `CreateIconIndirect`に渡した
    /// color/maskビットマップを解放し忘れると、呼ぶたびにプロセスのGDIオブジェクト数が
    /// 2個ずつ増え続ける。`GetGuiResources(GR_GDIOBJECTS)`でプロセスのGDIオブジェクト数を
    /// 実測し、繰り返し呼んでも増加し続けないことを確認する。
    #[test]
    fn tray_icon_does_not_leak_gdi_objects() {
        let _guard = lock_gui_resource_tests();
        // ウォームアップ（初回呼び出し特有の遅延初期化コストを計測対象から外す）
        let icon = tray_icon(true).unwrap();
        unsafe {
            let _ = DestroyIcon(icon);
        }

        let before = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
        for i in 0..50 {
            let icon = tray_icon(i % 2 == 0).unwrap();
            unsafe {
                let _ = DestroyIcon(icon);
            }
        }
        let after = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };

        // HICON自体の生成/破棄コストによる多少の変動は許容する。修正前はcolor/mask
        // ビットマップが解放されないため50回で最低100個リークしていた
        assert!(
            after <= before + 10,
            "GDIオブジェクトがリークしている可能性: before={before}, after={after}"
        );
    }

    /// `CreateWindowExW`失敗時にTrayContext・iconがリークしていた問題の回帰。
    /// `finish_window_creation`へErrを渡し、Errが伝播すること・iconのGDIハンドルが
    /// 孤児として残らないことを確認する。ctxは失敗時に手動解放されるため、
    /// `Box::from_raw`による二重解放が起きればここでpanic/クラッシュするはず
    /// （実際には一度しか解放されないため問題ない）。
    #[test]
    fn finish_window_creation_frees_ctx_and_icon_on_create_failure() {
        let _guard = lock_gui_resource_tests();
        // HICON自体は`GR_GDIOBJECTS`ではなく`GR_USEROBJECTS`（アイコン/カーソル/
        // ウィンドウ等）でカウントされるため、こちらで観測する。iconを作る
        // *前*の値を基準にしないと、作った分がbeforeに混ざって差が見えなくなる
        let before = unsafe { GetGuiResources(GetCurrentProcess(), GR_USEROBJECTS) };

        let icon = tray_icon(true).unwrap();
        let (tx, _rx) = std::sync::mpsc::channel::<TrayEvent>();
        let ctx = Box::into_raw(Box::new(TrayContext {
            events: tx,
            watch_state: Arc::new(AtomicBool::new(true)),
            icon: Cell::new(icon),
            icon_watch: AtomicBool::new(true),
            icon_shown: Arc::new(AtomicBool::new(false)),
            repaint: Box::new(|| {}),
            clicks: RefCell::new(ClickTracker::default()),
            menu_open: Cell::new(false),
            shutdown_pending: Cell::new(false),
        }));

        let result = finish_window_creation(
            Err(WinError::from_hresult(windows::Win32::Foundation::E_FAIL)),
            ctx,
            icon,
        );
        let after = unsafe { GetGuiResources(GetCurrentProcess(), GR_USEROBJECTS) };

        assert!(result.is_err());
        assert!(
            after <= before,
            "CreateWindowExW失敗時にiconが孤児として残っている: before={before}, after={after}"
        );
    }
}
