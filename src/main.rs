// releaseはGUIサブシステム（コンソールを開かない）。debugはコンソールを残し、
// eprintlnによる開発時診断を見えるようにする
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod clipboard;
mod config;
mod data;
mod datacheck;
mod dib;
mod folder_security;
mod hdrop;
mod hotkey;
mod icons;
mod menu_draw;
mod menu_tooltip;
mod native;
mod ops;
mod paste;
mod service;
mod storage;
mod store;
mod tools;
mod tray;
mod ui_thread;

use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Arc, RwLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, IsDialogMessageW, TranslateMessage, MSG,
};

use clipboard::{ClipboardPort, ClipboardWatcher};
use config::Config;
use native::app::{App, Parts};
use native::viewer::{self, ViewerHandler, ViewerWindow};
use ops::{Core, OpError};

/// ウィンドウのタイトル・トレイのツールチップ・エラーダイアログで使うアプリの表示名。
pub const APP_DISPLAY_NAME: &str = "CLCLR";

const SINGLE_INSTANCE_MUTEX_NAME: PCWSTR = w!("CLCLR_SingleInstanceMutex");

/// 多重起動防止。既に起動中なら既存のビューア窓へ表示要求を送ってNoneを返す
/// （呼び出し元はプロセスを終了する）。起動してよい場合はMutexハンドルを返すので、
/// `main`のローカル変数として保持し、プロセス終了までスコープを維持すること
/// （ドロップするとハンドルが閉じられ二重起動防止の意味がなくなる）。
/// 表示要求は、先に起動した側が窓を作るまで待ってから送る（起動の途中の二重起動）。届けられな
/// ければ、その旨を知らせる。
fn acquire_single_instance() -> Option<HANDLE> {
    use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;

    unsafe {
        // 作れなければ、二重起動と区別して知らせる（何も言わずに終わらない）
        let mutex = match CreateMutexW(None, false, SINGLE_INSTANCE_MUTEX_NAME) {
            Ok(mutex) => mutex,
            Err(e) => {
                report_error(&format!("CLCLR を起動できません（二重起動の確認に失敗しました）。\n\n{e}"));
                return None;
            }
        };
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // 待つ間に Mutex を持ち続けない（先に起動した側が終われば、次の起動が Mutex を取れる）
            let _ = CloseHandle(mutex);
            if !viewer::request_show_existing() {
                report_error(
                    "CLCLR は起動中ですが、窓を表示できませんでした。しばらく待ってからもう一度起動してください。",
                );
            }
            None
        } else {
            Some(mutex)
        }
    }
}

/// パニック情報から1行分のログテキストを組み立てる（末尾に改行を含む）。
/// フォーマットのみを担当する純粋関数（`PanicHookInfo`はテストで構築できないため、
/// `install_panic_hook`から分離してここだけを単体テストする）。
fn format_panic_log_line(timestamp_unix: u64, location: &str, message: &str) -> String {
    format!("[{timestamp_unix}] panic at {location}: {message}\n")
}

fn panic_location_string(info: &std::panic::PanicHookInfo<'_>) -> String {
    info.location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "unknown".to_string())
}

/// panic!マクロの典型的なペイロード（&str・String）から本文を取り出す。
/// それ以外の型でpanicした場合（`panic_any`等）は固定文言にフォールバックする。
fn panic_message_string(info: &std::panic::PanicHookInfo<'_>) -> String {
    if let Some(s) = info.payload().downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "(no message)".to_string()
    }
}

/// パニック発生時、標準の出力（デバッグビルドはコンソールへ表示される）に加えて
/// `storage::base_dir()`配下の`panic.log`へも追記する。releaseビルドはGUI
/// サブシステムでコンソールが無く、eprintln!もパニックの標準メッセージも
/// どこにも表示されず消えるため、原因調査の手がかりをファイルに残す。
/// ログ処理自体で二次パニックしないよう、書き込み失敗は無視する。
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let line = format_panic_log_line(
            timestamp,
            &panic_location_string(info),
            &panic_message_string(info),
        );
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(storage::base_dir().join("panic.log"))
        {
            let _ = f.write_all(line.as_bytes());
        }
    }));
}

/// 致命的・重要なエラーの報告。releaseはGUIサブシステムでstderrが届かないため、
/// eprintlnに加えてメッセージボックスでも報告する。
fn report_error(message: &str) {
    eprintln!("{message}");
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let text = HSTRING::from(message);
    let caption = HSTRING::from(APP_DISPLAY_NAME);
    unsafe {
        MessageBoxW(None, PCWSTR(text.as_ptr()), PCWSTR(caption.as_ptr()), MB_OK | MB_ICONERROR);
    }
}

/// 設定ファイルを読めないときに起動をやめる知らせの文。誤りの位置は書式の誤りのときだけ `e` に入る
/// （読み取りの誤りでは入らないので、位置を前提にしない）。
fn config_load_failure_message(path: &std::path::Path, e: &config::ConfigError) -> String {
    format!(
        "設定ファイルを読み込めないため、起動をやめます。履歴は変えていません。\n\n{}\n{e}\n\n\
         設定ファイルを直すか、名前を変えてから（既定の設定で起動します）、もう一度起動してください。",
        path.display()
    )
}

/// 起動時にホットキー・二度押しでできなかったこと（`Hotkeys::startup_problems`）を知らせる文。
/// 1行目に何が起きたか・この後どうなるか、空行の後に1件1行の詳細（起動時のダイアログの形）。問題が無ければ `None`。
fn hotkey_problems_message(problems: &[String]) -> Option<String> {
    (!problems.is_empty()).then(|| {
        format!("一部のホットキーを使えません。ほかの機能はそのまま使えます。\n\n{}", problems.join("\n"))
    })
}

/// 起動時のクリップボード同期:空なら履歴の最新を復元し、データがあれば履歴へ取り込む
/// （取り込みは監視有効時のみ。監視パイプライン経由なので、フィルタ・重複チェック等は
/// 通常のキャプチャと同じ扱いになる）。復元はメインスレッドを止めないよう、短命なスレッドで
/// 行う（`restore_latest_entry`）。
fn sync_clipboard_on_start(config: &RwLock<Config>, core: &Core, watcher: &ClipboardWatcher) {
    let (sync_on_start, watch_enabled) = {
        let c = config.read().unwrap();
        (c.general.startup_clipboard_sync, c.general.clipboard_watch)
    };
    if !sync_on_start {
        return;
    }
    if clipboard::is_clipboard_empty() {
        let core = core.clone();
        let port = watcher.port();
        let _ = std::thread::Builder::new()
            .name("clclr-restore".to_string())
            .spawn(move || restore_latest_entry(&core, &port));
    } else if watch_enabled {
        watcher.capture_now();
    }
}

/// 空のクリップボードへ履歴の最新を戻す（起動時の同期、専用のスレッドで呼ぶ）。
/// 読み込みは `Core::load_for_send`（厳密な読み込み）。書き込みの前にもう一度受け付けに登録し、
/// 書き終わるまで持つ（締め切り後なら書かない。書いている間に終了処理が最後の保存へ進まない）。
/// 空の確認と書き込みは、クリップボードを開いたまま続けて行う（間に入った新しいコピーを消さない）。
/// 戻した内容は取り込まない（書いた変更の番号を抑止する番号として記録する）。書けたら true。
fn restore_latest_entry(core: &Core, port: &ClipboardPort) -> bool {
    let Some(Some(id)) = core.read(|s| s.history.front().map(|item| item.meta.id)) else {
        return false;
    };
    let entry = match core.load_for_send(id, false) {
        Ok(entry) => entry,
        Err(OpError::Closing) => return false,
        Err(e) => {
            eprintln!("起動時のクリップボード復元の読み込みに失敗: {e}");
            return false;
        }
    };
    let Ok(_ticket) = core.admit() else {
        return false;
    };
    match clipboard::set_clipboard_if_empty_suppressed(port, &entry.formats) {
        Ok(written) => written,
        Err(e) => {
            eprintln!("起動時のクリップボード復元に失敗: {e}");
            false
        }
    }
}

/// メインスレッドのメッセージループ。`PostQuitMessage`（トレイの「終了」など）で抜ける。
/// ビューア窓のキー操作（Tab での移動、Esc）は `IsDialogMessageW` に任せる。これが処理した
/// メッセージは `TranslateMessage` に通さない（二重に変換すると IME の入力が壊れるため）。
fn run_message_loop(viewer: windows::Win32::Foundation::HWND) {
    let mut msg = MSG::default();
    unsafe {
        loop {
            // 0 = WM_QUIT、-1 = エラー。どちらもループを抜けて終了処理へ進む
            let r = GetMessageW(&mut msg, None, 0, 0).0;
            if r == 0 || r == -1 {
                break;
            }
            // 設定画面（モードレス）宛てのキー操作: Ctrl+Tab でタブの切り替え、Tab・アクセスキー・Enter・Esc は
            // IsDialogMessageW（設定画面とその子宛てのメッセージだけ。処理したものは TranslateMessage に通さない）
            if native::settings::pre_translate(&msg) || native::settings::is_dialog_message(&msg) {
                continue;
            }
            // Ctrl+F などのキー操作は、Tab・Esc の処理（IsDialogMessageW）より先に変換する
            if native::viewer::translate_accelerator(viewer, &msg) {
                continue;
            }
            if IsDialogMessageW(viewer, &msg).as_bool() {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn main() {
    // 予期しないpanicの手がかりを残す（可能な限り早い段階で設定する）
    install_panic_hook();

    // 多重起動防止。既存インスタンスがあれば表示を依頼してこのプロセスは終了する。
    // _instance_mutexはプロセス終了までスコープを維持する必要がある（コメント参照）
    let Some(_instance_mutex) = acquire_single_instance() else {
        return;
    };

    // 設定ファイル不在はload側でデフォルト扱いになるため、ここでErrになるのは
    // 「ファイルはあるが読めない/パースできない」場合のみ。そのときは `history.toml` と同じく起動をやめる
    // （既定の設定で続けると、既定の保持件数で履歴を切り詰め、読めなかった設定ファイルも後の保存で上書きする）。
    // 履歴にもクリップボードにも触る前（`Core::open` の前）に止める
    let config_path = Config::default_path();
    let config = match Config::load(&config_path) {
        Ok(config) => config,
        Err(e) => {
            report_error(&config_load_failure_message(&config_path, &e));
            std::process::exit(1);
        }
    };
    // 設定の実行時変更を全所有者（watcher/service/UI）へ波及させるため共有する
    let config = Arc::new(RwLock::new(config));

    let core = match Core::open(Arc::clone(&config)) {
        Ok(c) => c,
        Err(e) => {
            report_error(&format!("履歴・ピン留めのデータを開けないため、終了します。\n\n{e}"));
            std::process::exit(1);
        }
    };

    // ビューア窓を先に（非表示で）作り、他スレッドからの起床先を用意してから各スレッドを起動する
    let (tray_tx, tray_rx) = mpsc::channel();
    let (hotkey_tx, hotkey_rx) = mpsc::channel();
    let app = Rc::new(App::new(Arc::clone(&config), core.clone(), tray_rx, hotkey_rx));
    App::bind_self(&app);
    let viewer_size = {
        let [w, h] = config.read().unwrap().general.viewer_size();
        (w as u32, h as u32)
    };
    let handler: Rc<dyn ViewerHandler> = app.clone();
    let viewer = match ViewerWindow::create(APP_DISPLAY_NAME, viewer_size, handler) {
        Ok(v) => v,
        Err(e) => {
            report_error(&format!("ビューアの窓を作れないため、終了します。\n\n{e}"));
            std::process::exit(1);
        }
    };
    let waker = viewer.waker();
    viewer::set_topmost(viewer.hwnd(), config.read().unwrap().general.viewer_always_on_top);

    // 画像（一覧のサムネイル・プレビュー）の読み込みスレッド。依頼の送り手はビューア窓が持ち、
    // 窓の破棄で手放すので、スレッドはその後に終わる
    let image_thread = {
        let (request_tx, request_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let storage = core.storage();
        let wanted = native::images::WantedThumbs::default();
        let wanted_preview = native::images::WantedPreview::default();
        let handle = native::images::spawn(
            storage,
            request_rx,
            result_tx,
            Arc::clone(&wanted),
            Arc::clone(&wanted_preview),
            viewer.image_notifier(),
        );
        viewer::attach_image_loader(viewer.hwnd(), request_tx, result_rx, wanted, wanted_preview);
        handle
    };

    // 検索スレッド。結果が出たらビューアを起こし、`App` が起床のときに受け取る。依頼の送り手は
    // `App` が持ち、終了時に `detach_search` で手放すので、スレッドはその後に終わる
    let search_thread = {
        let (command_tx, command_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let handle = native::search::spawn(core.clone(), command_rx, result_tx, move || waker.wake());
        app.attach_search(command_tx, result_rx);
        handle
    };

    let watcher = {
        let core = core.clone();
        match ClipboardWatcher::spawn(Arc::clone(&config), move |entry| {
            match core.capture(entry) {
                // 終了処理が始まった後の取り込みは保存しない（締め切り）
                Ok(()) | Err(OpError::Closing) => {}
                Err(e) => eprintln!("履歴への取り込みに失敗: {e}"),
            }
            waker.wake();
        }) {
            Ok(w) => w,
            Err(e) => {
                report_error(&format!("クリップボードの監視を始められないため、終了します。\n\n{e}"));
                std::process::exit(1);
            }
        }
    };

    // ビューアの操作（送る・テキスト変換・関連付けで開く）のスレッド。失敗は通知先に積まれ、
    // ビューアが起こされる。join はしない（actions.rs の説明。通常の終了で最後の
    // `core.shutdown(None)` が成功したときは、受け付けの中の処理はそれが待ち終えている）
    {
        let (request_tx, request_rx) = mpsc::channel();
        let failures = native::actions::FailureSink::new(move || waker.wake());
        match native::actions::spawn(
            core.clone(),
            Arc::clone(&config),
            watcher.port(),
            failures.clone(),
            request_rx,
        ) {
            Ok(_detached) => {
                // 保存先のフォルダの権限を確かめる（作業スレッド。起動は待たない）。結果は失敗の通知先に届き、
                // ビューアが警告する（非表示で起動していても、警告があればビューアを出す）
                if config.read().unwrap().general.check_folder_permissions {
                    native::actions::spawn_folder_check(failures.clone(), std::env::current_exe());
                }
                app.attach_actions(request_tx, failures)
            }
            Err(e) => report_error(&format!(
                "ビューアの操作（送る・変換・ピン留めなど）のスレッドを作れないため、これらの操作は使えません。\n\n{e}"
            )),
        }
    }

    sync_clipboard_on_start(&config, &core, &watcher);

    // トレイ（設定で無効なら作らない。その場合は窓を閉じると終了する）。設定の反映で実行中に作るときも同じ
    // 送り手と起こし方を使う。アイコンは監視の実際の状態で決める
    app.attach_tray_source(tray_tx.clone(), waker);
    let tray = if config.read().unwrap().general.show_trayicon {
        match tray::Tray::spawn(watcher.watch_state(), tray_tx, move || waker.wake()) {
            Ok(t) => Some(t),
            Err(e) => {
                report_error(&format!("トレイアイコンを作れないため、トレイなしで続けます。\n\n{e}"));
                None
            }
        }
    } else {
        None
    };

    // グローバルホットキー（設定で無効なら登録されないがスレッドは常駐）
    let hotkeys = match hotkey::Hotkeys::spawn(
        Arc::clone(&config),
        core.clone(),
        watcher.port(),
        hotkey_tx,
        move || waker.wake(),
    ) {
        Ok(h) => Some(h),
        Err(e) => {
            report_error(&format!("ホットキーを使えないため、ホットキーなしで続けます。\n\n{e}"));
            None
        }
    };
    let hotkey_problems = hotkeys.as_ref().and_then(|h| hotkey_problems_message(h.startup_problems()));

    // 非表示起動はトレイのアイコンを付けられたときだけ（アイコンが無いと窓へ戻る手段が見えないため。
    // タスクバーができる前の登録で付けられなかった場合も表示して起動する）
    let start_hidden =
        config.read().unwrap().general.start_hidden && tray.as_ref().is_some_and(tray::Tray::icon_shown);
    app.set_parts(Parts { watcher, tray, hotkeys });
    // 登録できなかった（他のアプリが同じキーを使っている等）ときも、スレッドは動いているので
    // 続行する。ダイアログの間もトレイの操作が届くよう、スレッド群を渡し終えてから知らせる
    if let Some(message) = hotkey_problems {
        report_error(&message);
    }
    if start_hidden {
        // 表示するまで知らせを残す（隠したときと同じ。`on_hidden` は表示していない窓には来ない）
        app.mark_viewer_hidden();
    } else {
        viewer::show(viewer.hwnd());
    }

    run_message_loop(viewer.hwnd());

    // 操作の失敗の通知先を閉じ（この後に届いた失敗はログだけ。破棄する窓へ起床を投稿しない）、
    // 操作スレッドへの依頼の送り手を手放す
    app.finish_actions();
    // 終了: まず受け付けを閉じ、受け付け済みの操作が終わるのを待って最後の保存をする（スレッドを
    // 止めるところで止まっても保存は済んでいるように、先に行う）。受け付けが閉じた後の取り込み・
    // 送出は何もせずに戻るので、その後に止める監視スレッドの join とも相互待ちにならない。
    // セッション終了で終了処理をした後は、やり直さない（時間切れの後に無期限の待ちへ戻らない）
    if !app.session_ended() {
        if let Err(e) = core.shutdown(None) {
            report_error(&format!("終了時に履歴を保存できませんでした。\n\n{e}"));
        }
    }
    // 設定画面を破棄する（終了の要求では隠しただけ。ここならスタックに設定画面の処理は無い。ビューアを破棄すると
    // 持たれた窓も破棄されるので、その前に明示的に破棄する）
    app.close_settings();
    // ビューア窓を後片付け中にする（この後、ホットキー・トレイのスレッドの終わりを待つ間に送られた
    // メッセージへ応じるが、アプリの処理は呼ばない）
    viewer::begin_teardown(viewer.hwnd());
    // ホットキー → トレイ → 監視の順にスレッドを止め、窓を破棄する（ホットキー・トレイは、送られた
    // メッセージに応じながら終わるまで待つ。`ui_thread::stop_ui_thread`）
    if let Some(Parts { watcher, tray, hotkeys }) = app.take_parts() {
        drop(hotkeys);
        drop(tray);
        drop(watcher);
    }
    app.detach_search();
    let _ = search_thread.join();
    drop(viewer);
    // 窓の破棄で依頼の送り手が無くなり、読み込み中の1件を終えたところで抜ける
    let _ = image_thread.join();
}

#[cfg(test)]
mod tests {
    use super::format_panic_log_line;
    use windows::core::w;
    use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;

    /// `acquire_single_instance`が依拠するWin32の前提を検証する回帰テスト:
    /// 同名のMutexを2回作成すると、2回目は既存ハンドルを返しつつ
    /// `GetLastError() == ERROR_ALREADY_EXISTS`になる（本番の
    /// `CLCLR_SingleInstanceMutex`とは別名を使い、実プロセスとの競合を避ける）。
    #[test]
    fn duplicate_named_mutex_reports_already_exists() {
        let name = w!("CLCLR_SingleInstanceMutex_Test_duplicate_named_mutex_reports_already_exists");
        unsafe {
            let first = CreateMutexW(None, false, name).unwrap();
            let second = CreateMutexW(None, false, name).unwrap();
            let err = GetLastError();
            let _ = CloseHandle(second);
            let _ = CloseHandle(first);
            assert_eq!(err, ERROR_ALREADY_EXISTS);
        }
    }

    /// 起動時の同期の復元は、履歴が空なら何もせず、終了処理が始まった（受け付けが閉じた）後も
    /// 何もしない（クリップボードには触れない）。
    #[test]
    fn restore_does_nothing_for_empty_history_or_after_shutdown() {
        use crate::ops::tests::{temp_core, text_entry};
        // クリップボードを開けない port（開こうとするとデバッグビルドで止まる = 触れないことも見る）
        let port = crate::clipboard::ClipboardPort::unopenable();
        let (dir, core) = temp_core(crate::config::Config::default());
        assert!(!super::restore_latest_entry(&core, &port), "空の履歴で書いた");
        core.capture(text_entry("new")).unwrap();
        core.shutdown(None).unwrap();
        assert!(!super::restore_latest_entry(&core, &port), "終了処理の後に書いた");
        assert_eq!(port.suppressed_seq(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 起動時のホットキーの問題は、1行目に何が起きたか・この後どうなるか、空行の後に詳細。
    #[test]
    fn hotkey_problems_message_leads_with_summary() {
        assert_eq!(super::hotkey_problems_message(&[]), None);
        let problems = ["ホットキー Alt+C を登録できません".to_string(), "キーフックを設定できません".to_string()];
        assert_eq!(
            super::hotkey_problems_message(&problems).unwrap(),
            "一部のホットキーを使えません。ほかの機能はそのまま使えます。\n\n\
             ホットキー Alt+C を登録できません\nキーフックを設定できません"
        );
    }

    /// 設定ファイルを読めないときの知らせ: 起動をやめること・履歴は変えていないこと・ファイルの場所・誤りの説明
    /// （書式の誤りなら位置も）・直し方を出す。
    #[test]
    fn config_load_failure_message_explains_stop_and_fix() {
        let dir = std::env::temp_dir().join(format!("clclr-main-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[general\n").unwrap();
        let e = crate::config::Config::load(&path).unwrap_err();
        let message = super::config_load_failure_message(&path, &e);
        assert!(message.starts_with("設定ファイルを読み込めないため、起動をやめます。履歴は変えていません。"), "{message}");
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains("1 行目"), "書式の誤りの位置が無い: {message}");
        assert!(message.contains("名前を変えてから（既定の設定で起動します）"), "{message}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn format_panic_log_line_includes_timestamp_location_and_message() {
        let line = format_panic_log_line(1700000000, "src/foo.rs:10:5", "boom");
        assert_eq!(line, "[1700000000] panic at src/foo.rs:10:5: boom\n");
    }

    #[test]
    fn format_panic_log_line_ends_with_newline() {
        let line = format_panic_log_line(0, "unknown", "(no message)");
        assert!(line.ends_with('\n'));
    }
}
