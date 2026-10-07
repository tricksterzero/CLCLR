//! フォーカスの記録・復帰とペースト送出。
//!
//! C版 MainProc.c の focus_info（get_focus_info / set_focus_info）と
//! SendKey.c（sendkey_paste / key_wait）の移植。ポップアップメニューは
//! 表示時にフォーカスを奪うため、表示前に元のアクティブウィンドウ・フォーカス・
//! キャレット位置を記録し、選択後に復帰させてから SendInput で Ctrl+V を送る。

use std::time::Duration;

use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetFocus, SendInput, SetFocus, INPUT, INPUT_0, INPUT_KEYBOARD,
    KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_CONTROL, VK_MENU, VK_SHIFT, VK_V,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCaretPos, GetForegroundWindow, GetGUIThreadInfo, GetSystemMetrics,
    GetWindowThreadProcessId, GUITHREADINFO, SendMessageTimeoutW, SetForegroundWindow,
    SystemParametersInfoW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, SMTO_ABORTIFHUNG, SPI_GETFOREGROUNDLOCKTIMEOUT,
    SPI_SETFOREGROUNDLOCKTIMEOUT, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_NCACTIVATE,
};

/// メニュー表示前のフォーカス状態のスナップショット。
/// HWNDはスレッド間を渡るためisizeの生値で保持する（ハンドルは参照外ししないので安全）。
#[derive(Clone, Copy, Debug, Default)]
pub struct FocusInfo {
    active_wnd: isize,
    focus_wnd: isize,
    /// キャレットのスクリーン座標（取得できた場合のみ）
    pub caret_pos: Option<(i32, i32)>,
}

impl FocusInfo {
    pub fn has_target(&self) -> bool {
        self.active_wnd != 0
    }
}

/// 座標が仮想スクリーン（全モニタを包含する矩形）内にあるかを判定する。
/// マルチモニタ環境の座標ズレや、対象アプリ側の内部状態が不整合な場合の
/// ゴミ座標を弾くための保険（キャレット座標の主判定は`caret_client_pos_is_plausible`）。
fn is_point_on_virtual_screen(pt: POINT) -> bool {
    unsafe {
        let x_min = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y_min = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        pt.x >= x_min && pt.x < x_min + width && pt.y >= y_min && pt.y < y_min + height
    }
}

/// クライアント座標としてキャレット位置がもっともらしいか（原点(0,0)＝実際には
/// キャレットが存在しないことを示す典型値ではないか）を判定する。C版 MainProc.c
/// `get_focus_info`の`cpos.x > 0 || cpos.y > 0`と同じ基準（変換前のクライアント座標で
/// 判定する点も含めて踏襲）。
fn caret_client_pos_is_plausible(pt: POINT) -> bool {
    pt.x > 0 || pt.y > 0
}

/// キャレットのスクリーン座標を求める。2段構え:
/// 1. `GetGUIThreadInfo`の`hwndCaret`（システムキャレットを持つウィンドウ）が
///    有効なら、その`rcCaret`を使う。フラグの`GUI_CARETBLINKING`は点滅の点灯/消灯を
///    示すだけで常時は立たないため判定に使わない（`hwndCaret`の有無で存在を判定する）。
///    この経路はAttachThreadInput不要（対象スレッドを所有していなくても呼べるとMS Learnに
///    明記）。
/// 2. 上記が使えない場合は`GetCaretPos`（要`AttachThreadInput`、C版と同じ）にフォールバックし、
///    `caret_client_pos_is_plausible`で(0,0)相当の疑わしい値を除外する。
///
/// なお、Electron/Chromium・WPF・WinUI3等の自前描画キャレットを使うアプリ（新しいWin11
/// メモ帳を含む）はWin32のシステムキャレットを一切公開しないため、この2段構えでも
/// キャレット位置は取得できずマウス位置にフォールバックする。これはC版CLCLでも同様
/// （`get_focus_info`は同じWin32 APIしか使っていない）で、Win32レベルでの解消は
/// UI Automationへの移行が必要な別範囲の話になる。
fn caret_screen_pos(target_thread: u32, focus_wnd: HWND) -> Option<(i32, i32)> {
    unsafe {
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if GetGUIThreadInfo(target_thread, &mut info).is_ok() && !info.hwndCaret.is_invalid() {
            let mut pt = POINT {
                x: info.rcCaret.left,
                y: info.rcCaret.top,
            };
            if ClientToScreen(info.hwndCaret, &mut pt).as_bool() && is_point_on_virtual_screen(pt)
            {
                return Some((pt.x, pt.y));
            }
        }

        let mut pt = POINT::default();
        if GetCaretPos(&mut pt).is_ok() && caret_client_pos_is_plausible(pt) {
            if ClientToScreen(focus_wnd, &mut pt).as_bool() && is_point_on_virtual_screen(pt) {
                return Some((pt.x, pt.y));
            }
        }

        None
    }
}

/// 現在のフォアグラウンドウィンドウ・フォーカス・キャレット位置を記録する。
/// フォーカスとキャレット（`GetCaretPos`側の経路）は対象スレッドに
/// `AttachThreadInput`しないと取れない（C版 get_focus_info と同じ手順）。
pub fn capture_focus() -> FocusInfo {
    unsafe {
        let active = GetForegroundWindow();
        if active.is_invalid() {
            return FocusInfo::default();
        }
        let target_thread = GetWindowThreadProcessId(active, None);
        let current_thread = GetCurrentThreadId();

        let mut focus = active;
        let mut caret_pos = None;
        if target_thread != current_thread
            && AttachThreadInput(current_thread, target_thread, true).as_bool()
        {
            let f = GetFocus();
            if !f.is_invalid() {
                focus = f;
            }
            caret_pos = caret_screen_pos(target_thread, focus);
            let _ = AttachThreadInput(current_thread, target_thread, false);
        }
        FocusInfo {
            active_wnd: active.0 as isize,
            focus_wnd: focus.0 as isize,
            caret_pos,
        }
    }
}

/// `SetForegroundWindow`のフォアグラウンド権限制限を回避してウィンドウをアクティブ化する
/// （C版 main.c `_SetForegroundWindow` の移植）。
///
/// RegisterHotKey 経由（Alt+C）はホットキー入力の受信で権限が付与されるが、
/// キーフック＋タイマー経由（修飾キー二度押し）は入力を受信しないため権限がなく、
/// 素の`SetForegroundWindow`はOSに拒否される。対象スレッドを現フォアグラウンド
/// スレッドに`AttachThreadInput`し、ForegroundLockTimeout を一時的に0にして呼ぶ。
pub fn force_set_foreground(hwnd: HWND) -> bool {
    unsafe {
        let fg_thread = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let target_thread = GetWindowThreadProcessId(hwnd, None);
        let attached = fg_thread != 0
            && fg_thread != target_thread
            && AttachThreadInput(target_thread, fg_thread, true).as_bool();

        let flags = SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0);
        let mut timeout: usize = 0;
        let _ = SystemParametersInfoW(
            SPI_GETFOREGROUNDLOCKTIMEOUT,
            0,
            Some(&mut timeout as *mut usize as *mut _),
            flags,
        );
        // SPI_SETFOREGROUNDLOCKTIMEOUT はポインタではなく値そのものを pvParam で渡す仕様
        let _ = SystemParametersInfoW(SPI_SETFOREGROUNDLOCKTIMEOUT, 0, Some(std::ptr::null_mut()), flags);

        let ret = SetForegroundWindow(hwnd).as_bool();

        let _ = SystemParametersInfoW(SPI_SETFOREGROUNDLOCKTIMEOUT, 0, Some(timeout as *mut _), flags);
        if attached {
            let _ = AttachThreadInput(target_thread, fg_thread, false);
        }
        ret
    }
}

/// 記録したウィンドウへフォアグラウンドとフォーカスを戻す（C版 set_focus_info）。
/// 戻り値は`force_set_foreground`の成否。呼び出し側はこれを見て、復帰に失敗した場合は
/// 誤ったウィンドウへの送出（Ctrl+V等）を避けるべき（対象ウィンドウが消滅していた場合等）。
pub fn restore_focus(fi: &FocusInfo) -> bool {
    if !fi.has_target() {
        return false;
    }
    unsafe {
        let active = HWND(fi.active_wnd as *mut _);
        let restored = force_set_foreground(active);
        notify_nc_activate(active);

        let target_thread = GetWindowThreadProcessId(active, None);
        let current_thread = GetCurrentThreadId();
        if target_thread != current_thread
            && AttachThreadInput(current_thread, target_thread, true).as_bool()
        {
            let _ = SetFocus(Some(HWND(fi.focus_wnd as *mut _)));
            let _ = AttachThreadInput(current_thread, target_thread, false);
        }
        restored
    }
}

/// 非アクティブ描画のまま残るのを防ぐ（C版のWM_NCACTIVATE送信を踏襲）。
/// 対象ウィンドウのスレッドが応答しない場合に無期限でブロックしないよう、素の
/// `SendMessageW`ではなくタイムアウト付きを使う（`SMTO_ABORTIFHUNG`で既にハング中と
/// 判定されているウィンドウには即座にタイムアウトさせる）。
///
/// 注意: `force_set_foreground`（`SetForegroundWindow`）自体も内部でウィンドウへ
/// 同期メッセージ（WM_NCACTIVATE等）を送出するため、対象が完全にハング状態
/// （5秒以上`GetMessage`を呼んでいない）の場合はこの関数の前段でブロックしうる。
/// `SetForegroundWindow`にタイムアウト付きの代替APIは無く、この関数単体の
/// タイムアウト化では`restore_focus`全体を非ブロッキングにはできない
/// （実機で確かめた）。
fn notify_nc_activate(hwnd: HWND) {
    let mut result: usize = 0;
    unsafe {
        let _ = SendMessageTimeoutW(
            hwnd,
            WM_NCACTIVATE,
            WPARAM(1),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            200,
            Some(&mut result),
        );
    }
}

/// 貼り付けを送らなかった・送りきれなかった理由（`send_paste`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteSkipped {
    /// 修飾キーが離されないまま待ちが時間切れになった（送ると Ctrl+Alt+V などになる）
    ModifiersHeld,
    /// 待っている間に前面の窓が、戻した窓から変わった（別の窓へ貼り付けない）
    ForegroundChanged,
    /// `SendInput` が送れたのは入力の一部だけ（送れた数）。送り直さない（二重に貼り付けうる）。押したままのキーを離す
    /// 入力は送った（それも入らなかったときはログに書く）。貼り付けが起きたかは分からない
    Partial(u32),
}

/// Ctrl+V をアクティブウィンドウへ送出する（C版 sendkey_paste 相当）。
///
/// 先に修飾キーの解放を待つ（C版 key_wait）: Alt+C 直後は Alt が押されたまま
/// なので、待たずに送ると Ctrl+Alt+V になってしまう。待ちが時間切れなら送らない。
/// 待った後、送る直前に前面の窓が `fi` で戻した窓のままかを確かめ、変わっていれば送らない（待っている間に利用者や
/// ほかのアプリが前面を変えると、別の窓へ貼り付けてしまうため）。確かめるのは前面の窓までで、同じ窓の中で入力欄が
/// 変わったことは見分けない。確かめてから送るまでの間に変わることも防げない。
pub fn send_paste(fi: &FocusInfo) -> Result<(), PasteSkipped> {
    let inputs = [
        key_input(VK_CONTROL, false),
        key_input(VK_V, false),
        key_input(VK_V, true),
        key_input(VK_CONTROL, true),
    ];
    if !wait_modifiers_released() {
        return Err(PasteSkipped::ModifiersHeld);
    }
    if unsafe { GetForegroundWindow() }.0 as isize != fi.active_wnd {
        return Err(PasteSkipped::ForegroundChanged);
    }
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        // 押す入力だけが入ったキーを押したままにしない（離す入力を送る。それも入らなければ、押したままになりうる）
        let release: Vec<INPUT> = keys_left_down(sent).iter().map(|&vk| key_input(vk, true)).collect();
        if !release.is_empty() {
            let released = unsafe { SendInput(&release, std::mem::size_of::<INPUT>() as i32) };
            if released as usize != release.len() {
                eprintln!("自動の貼り付けで押したキーを離す入力を送りきれませんでした（{released}/{}）", release.len());
            }
        }
        return Err(PasteSkipped::Partial(sent));
    }
    Ok(())
}

/// `send_paste` の4つの入力（Ctrl を押す・V を押す・V を離す・Ctrl を離す）のうち、先頭の `sent` 個だけが入ったときに
/// 押したままになっているキー（離す順）。
fn keys_left_down(sent: u32) -> &'static [VIRTUAL_KEY] {
    match sent {
        1 | 3 => &[VK_CONTROL],
        2 => &[VK_V, VK_CONTROL],
        _ => &[],
    }
}

/// 現在のマウスカーソル位置（スクリーン座標）。キャレット位置が取れない時の
/// ポップアップ表示位置フォールバック。
pub fn cursor_pos() -> (i32, i32) {
    let mut pt = POINT::default();
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt);
    }
    (pt.x, pt.y)
}

/// Shiftキーが押されているか（C版準拠: メニュー選択時にShift押下なら自動ペーストを抑制）。
pub fn shift_held() -> bool {
    unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000 != 0 }
}

/// Ctrl/Shift/Alt が全て離されるまで待つ（最大2秒でタイムアウト）。離されたら true、時間切れなら false。
fn wait_modifiers_released() -> bool {
    for _ in 0..100 {
        let held = [VK_MENU, VK_CONTROL, VK_SHIFT]
            .iter()
            .any(|vk| unsafe { GetAsyncKeyState(vk.0 as i32) } as u16 & 0x8000 != 0);
        if !held {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// 自分が `SendInput` で送るキー入力の目印（`KEYBDINPUT::dwExtraInfo`）。二度押しのキーフックは、注入の印があって
/// この目印の入力を数えない（自動貼り付けの Ctrl を二度押しの1回目に数えない）。ほかのアプリが
/// 同じ値を使うと、その入力も数えない（残る制限）。
pub const PASTE_INPUT_MARK: usize = 0x434C_434C; // "CLCL"

fn key_input(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() },
                dwExtraInfo: PASTE_INPUT_MARK,
                ..Default::default()
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 実際に押下中の修飾キーの検知精度までは検証できない（`GetAsyncKeyState`は
    /// テスト実行環境の実キー状態に依存し、モックする手段もない）が、テスト実行中に
    /// 修飾キーが押されていない通常の状況では即座（最初のポーリングで）に返ることを
    /// 検証できる。無限ループ化やタイムアウトの大幅な延び（20ms×100回=最大2秒）を
    /// 起こす回帰があれば、この余裕を持った閾値でも検知できる。
    #[test]
    fn wait_modifiers_released_returns_promptly_when_no_modifier_is_held() {
        let start = std::time::Instant::now();
        assert!(wait_modifiers_released(), "修飾キーが押されていないのに時間切れになった");
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    /// 入力の一部だけが入ったとき、押したままのキーを離す（Ctrl・V を押したまま残さない）。全部入った・何も入らない
    /// ときは何もしない。
    #[test]
    fn keys_left_down_after_partial_send() {
        assert_eq!(keys_left_down(0), &[] as &[VIRTUAL_KEY]);
        assert_eq!(keys_left_down(1), &[VK_CONTROL]);
        assert_eq!(keys_left_down(2), &[VK_V, VK_CONTROL]);
        assert_eq!(keys_left_down(3), &[VK_CONTROL]);
        assert_eq!(keys_left_down(4), &[] as &[VIRTUAL_KEY]);
    }

    /// 前面の窓が、戻した窓と違えば Ctrl+V を送らない（送れば別の窓へ貼り付けてしまう）。戻した窓に、前面に
    /// なりえない値（存在しないハンドル）を使う。
    #[test]
    fn send_paste_skips_when_foreground_is_not_the_restored_window() {
        let fi = FocusInfo { active_wnd: 0x7FFF_0000, focus_wnd: 0x7FFF_0000, caret_pos: None };
        assert_eq!(send_paste(&fi), Err(PasteSkipped::ForegroundChanged));
    }

    /// キャレットを持たないスレッドに対しては`None`を返すこと。テスト実行スレッド
    /// 自身はGUIメッセージループもキャレットも持たないため、実在しないケースの
    /// 代用として使える（`hwndCaret`が無効→`GetCaretPos`側にフォールック→
    /// クライアント座標が原点相当→`caret_client_pos_is_plausible`で除外、という
    /// 経路をまとめて確認する）。
    #[test]
    fn caret_screen_pos_returns_none_when_no_caret() {
        let thread_id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
        assert_eq!(caret_screen_pos(thread_id, HWND::default()), None);
    }

    /// クライアント座標としてキャレット位置がもっともらしいかの判定
    /// （C版 MainProc.c `get_focus_info`の`cpos.x > 0 || cpos.y > 0`と同じ基準）。
    /// 原点(0,0)＝キャレット不在の典型値のみ除外し、それ以外は片方の軸が0でも
    /// 有効とみなす（テキスト行の左端(x=0)や最上端(y=0)は正当にありうるため）。
    #[test]
    fn caret_client_pos_is_plausible_rejects_only_origin() {
        assert!(!caret_client_pos_is_plausible(POINT { x: 0, y: 0 }));
        assert!(caret_client_pos_is_plausible(POINT { x: 0, y: 5 }));
        assert!(caret_client_pos_is_plausible(POINT { x: 5, y: 0 }));
        assert!(caret_client_pos_is_plausible(POINT { x: 5, y: 5 }));
    }

    /// 対象ウィンドウが記録されていない場合（`capture_focus`失敗時のデフォルト等）、
    /// `force_set_foreground`を呼ぶまでもなくfalseを返すこと。呼び出し側（hotkey.rs::send_pick）
    /// はこの戻り値でauto_pasteの可否を判定する。
    #[test]
    fn restore_focus_returns_false_when_no_target() {
        let fi = FocusInfo::default();
        assert!(!fi.has_target());
        assert!(!restore_focus(&fi));
    }

    /// 既に破棄され存在しないウィンドウハンドルへの復帰は失敗し、falseを返すこと
    /// （対象ウィンドウが消滅していた場合に誤ったウィンドウへCtrl+Vを送出しない回帰）。
    #[test]
    fn restore_focus_returns_false_for_destroyed_window() {
        // 明らかに無効なウィンドウハンドル（現在のプロセスに存在しない値）
        let fi = FocusInfo {
            active_wnd: 0x7FFF_0000,
            focus_wnd: 0x7FFF_0000,
            caret_pos: None,
        };
        assert!(fi.has_target());
        assert!(!restore_focus(&fi));
    }

    /// 対象ウィンドウのメッセージ処理がハングしていても、`notify_nc_activate`が
    /// 無期限にブロックされないこと（`SendMessageTimeoutW`によるタイムアウトの回帰）。
    /// `restore_focus`全体ではなくこの関数単体を対象にするのは、`force_set_foreground`
    /// （`SetForegroundWindow`）自体も内部で同期メッセージを送出するため、`restore_focus`
    /// 全体を計測すると`SetForegroundWindow`側のブロッキングと区別できないため
    /// （実機で確かめた）。
    /// WM_NCACTIVATE受信時に意図的に長時間スリープするテスト用ウィンドウを立て、
    /// `notify_nc_activate`の所要時間がタイムアウト設定値程度に収まることを確認する。
    #[test]
    fn notify_nc_activate_does_not_block_on_hung_window() {
        use std::sync::mpsc;
        use std::time::Instant;
        use windows::core::w;
        use windows::Win32::Foundation::LRESULT;
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostQuitMessage,
            RegisterClassW, TranslateMessage, CW_USEDEFAULT, MSG, WM_DESTROY, WNDCLASSW,
            WS_OVERLAPPED,
        };

        unsafe extern "system" fn hung_wndproc(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            if msg == WM_NCACTIVATE {
                // ハング状態を再現するため、応答をわざと長時間止める
                // （restore_focus側のタイムアウト200msより十分長い3秒）
                std::thread::sleep(Duration::from_secs(3));
            }
            if msg == WM_DESTROY {
                unsafe { PostQuitMessage(0) };
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }

        // 窓を作って壊すので GUI 用のロックを取り、窓のスレッドの終わり（最後の join）まで持つ
        let _gui = crate::tray::lock_gui_resource_tests();
        let (hwnd_tx, hwnd_rx) = mpsc::channel::<isize>();
        let handle = std::thread::spawn(move || unsafe {
            let hinstance = GetModuleHandleW(None).unwrap().into();
            let class_name = w!("CLCLR_TestHungWindow");
            let class = WNDCLASSW {
                lpfnWndProc: Some(hung_wndproc),
                hInstance: hinstance,
                lpszClassName: class_name,
                ..Default::default()
            };
            RegisterClassW(&class);
            let hwnd = CreateWindowExW(
                Default::default(),
                class_name,
                class_name,
                WS_OVERLAPPED,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                0,
                0,
                None,
                None,
                Some(hinstance),
                None,
            )
            .unwrap();
            let _ = hwnd_tx.send(hwnd.0 as isize);
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        });

        let hwnd_val = hwnd_rx.recv().expect("テスト用ウィンドウの作成に失敗");

        let start = Instant::now();
        notify_nc_activate(HWND(hwnd_val as *mut _));
        let elapsed = start.elapsed();

        // タイムアウト設定値(200ms)にAPI呼び出しオーバーヘッドを加味した猶予を持たせる
        assert!(
            elapsed < Duration::from_secs(1),
            "notify_nc_activateが{elapsed:?}かかった（タイムアウトが効いていない可能性）"
        );

        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(HWND(hwnd_val as *mut _)),
                WM_DESTROY,
                WPARAM(0),
                LPARAM(0),
            );
        }
        let _ = handle.join();
    }

    /// 明らかに画面外（仮想スクリーンの矩形から大きく外れた値）の座標は無効と判定すること。
    /// Win11メモ帳で`GetCaretPos`が不正な座標を返し、ポップアップメニューが画面上端に
    /// 表示された不具合の回帰（`capture_focus`はこの判定を通った座標のみ採用する）。
    #[test]
    fn is_point_on_virtual_screen_rejects_far_out_of_range_coordinates() {
        assert!(!is_point_on_virtual_screen(POINT {
            x: -1_000_000,
            y: -1_000_000
        }));
        assert!(!is_point_on_virtual_screen(POINT {
            x: 1_000_000,
            y: 1_000_000
        }));
    }

    /// プライマリモニタ中央付近の座標は有効と判定すること（誤検知でキャレット座標を
    /// 弾きすぎない）。CIのようなヘッドレス環境でも`GetSystemMetrics`は0以上の値を
    /// 返すため、少なくとも(0,0)は仮想スクリーン内に収まる。
    #[test]
    fn is_point_on_virtual_screen_accepts_origin() {
        assert!(is_point_on_virtual_screen(POINT { x: 0, y: 0 }));
    }
}
