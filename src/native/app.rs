//! トレイ・ホットキーからのイベントを受けて、ビューア窓とコアを操作する結線。
//! トレイ・ホットキーのイベントの処理、監視の切り替え、
//! 一覧の作り直し、検索、プレビューを受け持つ。
//!
//! メインスレッドだけで使う（`ViewerHandler` としてビューア窓の wndproc から呼ばれる）。
//! 起動時にスレッド群を作り終えるまでは `parts` が空で、終了時に `take_parts` で取り出して
//! 順に止める。`RefCell` の借用はイベント1件の処理の間だけで、ビューア窓へメッセージを
//! 送る（`viewer::set_rows` など、再入を起こす）前に手放す。
//!
//! サービスのロックは `Core::read` で、読み込み元（メタデータと resident の `Arc` の写し）を
//! 取り出す間だけ持ち、先頭の切り出し・blob の読み込み（プレビュー）はロックの外で行う。検索は
//! 検索スレッド（`search.rs`）に頼み、結果は起床（`on_wake`）のときに受け取る。
//!
//! 一覧は、サービスの変更番号（`HistoryService::revision`）が前に作ったときと変わったときだけ
//! 作り直す（トレイ・ホットキー・検索の結果による起床では作り直さない）。

use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use uuid::Uuid;
use windows::core::HSTRING;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, PostMessageW, PostQuitMessage, MB_ICONWARNING, MB_OK};

use crate::clipboard::ClipboardWatcher;
use crate::config::{merge_edit, Config, ConfigIssue};
use crate::data::{utf16_text, Format};
use crate::hotkey::{HotkeyEvent, Hotkeys};
use crate::datacheck::{CleanResult, DataReport};
use crate::folder_security::FolderReport;
use crate::native::actions::{Action, ActionFailure, ActionKind, FailureSink, Notice};
use crate::native::settings::{self, SettingsWindow};
use crate::native::images::ImageSource;
use crate::native::model::{
    self, Reorder, Row, RowCommand, RowMenu, RowTarget, Source, ToolCommand, TreeCommand, TreeMenu, TreeNode,
};
use crate::native::search::{self, SearchResult, TextSource};
use crate::native::viewer::{self, ViewerHandler};
use crate::ops::Core;
use crate::service::HistoryService;
use crate::storage::{EntryMeta, Storage};
use crate::store;
use crate::tray::{Tray, TrayEvent};

/// プレビューで読むテキストの先頭のバイト数（UTF-16 で `PREVIEW_MAX_UNITS` 単位分）。
const PREVIEW_TEXT_BYTES: usize = model::PREVIEW_MAX_UNITS * 2;
/// プレビューで読むファイル一覧（CF_HDROP）の先頭のバイト数。
const PREVIEW_HDROP_BYTES: usize = 64 * 1024;
/// セッション終了（サインアウト・再起動）のとき、受け付け済みの操作が終わるのを待つ上限
/// （Windows はセッション終了の応答を5秒前後で打ち切ることがある）。
const END_SESSION_WAIT: Duration = Duration::from_secs(4);
/// ホットキーの登録し直し（設定の反映）が失敗したときの知らせに添える文。設定は戻さないので、
/// 有効のまま残っていることと次の起動で試すことを伝える。
const REREGISTER_RETRY_NOTE: &str = "設定は保存しました。次の起動でもう一度試します。";

/// 起動時に作るスレッド群。終了時はホットキー → トレイ → 監視の順に止める。
pub struct Parts {
    pub watcher: ClipboardWatcher,
    pub tray: Option<Tray>,
    pub hotkeys: Option<Hotkeys>,
}

pub struct App {
    config: Arc<RwLock<Config>>,
    /// 設定ファイルの置き場所（テストでは一時フォルダへ差し替える）
    config_path: PathBuf,
    core: Core,
    tray_events: Receiver<TrayEvent>,
    hotkey_events: Receiver<HotkeyEvent>,
    parts: RefCell<Option<Parts>>,
    /// 一覧に表示している対象
    source: Cell<Source>,
    /// 非表示の間に履歴が変わった（表示したときに作り直す）
    dirty: Cell<bool>,
    /// 前回ツリーに入れた構成（変わったときだけ作り直す）
    last_tree: RefCell<Option<Vec<TreeNode>>>,
    /// 検索文字列（小文字化済み。空なら絞り込まない）
    needle: RefCell<String>,
    /// 検索スレッドとのつなぎ（`attach_search`。無ければ検索しない）
    search: RefCell<Option<SearchLink>>,
    /// 最後に頼んだ検索の世代
    search_generation: Cell<u64>,
    /// 結果を待っている検索（届いたらこの写しから行を作る）
    pending_search: RefCell<Option<PendingSearch>>,
    /// 前に一覧を作ったときのサービスの変更番号
    last_revision: Cell<Option<u64>>,
    /// プレビューに出している項目
    preview_id: Cell<Option<Uuid>>,
    /// 操作スレッドとのつなぎ（`attach_actions`。無ければ操作しない）
    actions: RefCell<Option<ActionLink>>,
    /// 知らせる前の操作の失敗と、表示中か
    notifier: RefCell<Notifier>,
    /// 失敗の知らせ方（メッセージボックス。テストでは差し替える）
    show_failures: RefCell<Rc<dyn Fn(HWND, &str)>>,
    /// データのチェックの結果の出し方（確認ダイアログ。「削除」なら true。テストでは差し替える）
    show_check: RefCell<Rc<dyn Fn(HWND, &DataReport) -> bool>>,
    /// データの削除の結果の出し方（テストでは差し替える）
    show_clean: RefCell<Rc<dyn Fn(HWND, &CleanResult)>>,
    /// 保存先のフォルダの権限の警告の出し方（テストでは差し替える）
    show_folder: RefCell<Rc<dyn Fn(HWND, &FolderReport) -> viewer::FolderChoice>>,
    /// 警告を出すために隠したビューアを出す方法（`viewer::show`。テストでは差し替える）
    reveal_viewer: RefCell<Rc<dyn Fn(HWND)>>,
    /// ビューアを隠している（`on_hidden` から `on_shown` まで）。この間は知らせを出さずに残し、表示したときに
    /// 出す（隠す操作で閉じた結果の後に、残りの結果が隠れた窓の上に続けて出ないように）
    viewer_hidden: Cell<bool>,
    /// 終了の要求を受けた（トレイの「終了」・トレイなしで閉じる・セッション終了）。以後は操作を
    /// 依頼せず、失敗を知らせない
    exiting: Cell<bool>,
    /// セッション終了（`WM_ENDSESSION`）で終了処理をした
    session_ended: Cell<bool>,
    /// 実行中にトレイを作るための、トレイの操作の送り手（`tray_events` と同じ通り道）とビューアの起こし方
    /// （`attach_tray_source`）
    tray_source: RefCell<Option<(Sender<TrayEvent>, viewer::Waker)>>,
    /// 設定の反映の途中（`apply_settings`。再入した反映は断る）
    applying: Cell<bool>,
    /// 最後に頼んだホットキーの登録し直しの番号（`HotkeyEvent::Reregistered` の照合。0 はまだ無い）
    settings_generation: Cell<u64>,
    /// 開いている設定画面（閉じると `on_settings_closed` が空にする。終了のときは `close_settings`）
    settings: RefCell<Option<SettingsWindow>>,
    /// 自分への弱い参照（設定画面の OK・閉じたときの処理が `App` を呼ぶため。`bind_self` で入れる）
    me: RefCell<Weak<App>>,
    /// テストだけで差し込む仕掛け（設定の反映の途中の決まった位置で呼ぶ。再入の代わりに終了の要求を起こす）
    #[cfg(test)]
    settings_hook: RefCell<Option<Box<dyn Fn(&'static str)>>>,
}

/// 設定の反映を断った・できなかった理由（`App::apply_settings`）。
#[derive(Debug)]
pub enum ApplyError {
    /// 入力に誤りがある（何もしていない）
    Invalid(Vec<ConfigIssue>),
    /// 保存できなかった（何も反映していない）
    Save(String),
    /// 今は反映できない（反映の途中・終了の要求の後・メニューやダイアログ・失敗の通知の表示中。何もしていない）
    Busy,
}

/// スコープを抜けると `Cell<bool>` を下ろす（早期の return・パニックでも）。
struct FlagReset<'a>(&'a Cell<bool>);

impl Drop for FlagReset<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// 操作スレッドとのつなぎ。依頼の送り手を手放すと操作スレッドは終わる。
struct ActionLink {
    requests: Sender<Action>,
    failures: FailureSink,
}

/// 知らせる前の失敗とデータのチェックの結果（届いた順）。`notifying` は表示中（メッセージボックス・ダイアログの
/// モーダルループの中で再入した起床は、積むだけで表示しない。外側の表示のループが拾う）。
#[derive(Default)]
struct Notifier {
    pending: VecDeque<Notice>,
    notifying: bool,
}

/// 検索スレッドとのつなぎ。依頼の送り手を手放すと検索スレッドは終わる。
struct SearchLink {
    commands: Sender<search::Command>,
    results: Receiver<SearchResult>,
}

/// 一覧の1行を作るための写し（ロック中に写す）。
struct RowSource {
    meta: EntryMeta,
    /// ディスクに書かない形式の名前（一覧の2行目用）
    extra_formats: Vec<String>,
    /// ピン留めの項目か（行の操作の対象。`Row::pinned`）
    pinned: bool,
}

/// 結果を待っている検索。
struct PendingSearch {
    generation: u64,
    /// 結果を出すときに先頭の行を選ぶか（表示対象や検索文字列が変わった）
    source_changed: bool,
    /// 候補の行の写し（候補の順）
    rows: Vec<RowSource>,
}

/// 一覧の中身（ロック中に作る）。
enum Listing {
    Rows(Vec<Row>),
    /// 検索の候補（検索スレッドへの依頼と、結果から行を作るための写し）
    Search(Vec<search::Candidate>, Vec<RowSource>),
}

/// プレビューの読み込み元（ロック中に写す）。ディスクに書かない形式（resident）は `Arc` の
/// 写しだけを持ち、先頭の切り出し・上限の判定はロックの外（`build_preview`）で行う。
struct PreviewSource {
    meta: EntryMeta,
    resident: Arc<Vec<Format>>,
    storage: Storage,
}

/// ディスクに書かない形式の先頭の写し。
struct ResidentHead {
    data: Vec<u8>,
    /// 写す前の大きさ（バイト）
    total: usize,
}

/// プレビューに出すもの。
enum PreviewContent {
    Text(String),
    /// 画像は読み込みスレッドで読む（UI スレッドでは復号しない）
    Image(ImageSource),
    /// プレビューできる形式が無い・読めない（「（プレビューなし）」を出す）
    Nothing,
}

impl App {
    pub fn new(
        config: Arc<RwLock<Config>>,
        core: Core,
        tray_events: Receiver<TrayEvent>,
        hotkey_events: Receiver<HotkeyEvent>,
    ) -> Self {
        Self {
            config,
            config_path: Config::default_path(),
            core,
            tray_events,
            hotkey_events,
            parts: RefCell::new(None),
            source: Cell::new(Source::History),
            dirty: Cell::new(true),
            last_tree: RefCell::new(None),
            needle: RefCell::new(String::new()),
            search: RefCell::new(None),
            search_generation: Cell::new(0),
            pending_search: RefCell::new(None),
            last_revision: Cell::new(None),
            preview_id: Cell::new(None),
            actions: RefCell::new(None),
            notifier: RefCell::new(Notifier::default()),
            show_failures: RefCell::new(Rc::new(show_failure_box)),
            show_check: RefCell::new(Rc::new(viewer::show_data_report)),
            show_clean: RefCell::new(Rc::new(viewer::show_clean_result)),
            show_folder: RefCell::new(Rc::new(viewer::show_folder_report)),
            reveal_viewer: RefCell::new(Rc::new(viewer::show)),
            viewer_hidden: Cell::new(false),
            exiting: Cell::new(false),
            session_ended: Cell::new(false),
            tray_source: RefCell::new(None),
            applying: Cell::new(false),
            settings_generation: Cell::new(0),
            settings: RefCell::new(None),
            me: RefCell::new(Weak::new()),
            #[cfg(test)]
            settings_hook: RefCell::new(None),
        }
    }

    /// 自分への弱い参照を入れる（`Rc` に入れた直後に呼ぶ。入れていなければ設定画面を開かない）。
    pub fn bind_self(this: &Rc<App>) {
        *this.me.borrow_mut() = Rc::downgrade(this);
    }

    /// 設定画面を開く（すでに開いていれば前へ出す）。ビューアのメニュー・ダイアログの表示中はビューアが呼ばない。
    /// 終了の要求の後は開かない。
    fn open_settings(&self, hwnd: HWND) {
        if self.exiting.get() {
            return;
        }
        if let Some(window) = self.settings.borrow().as_ref() {
            window.bring_to_front();
            return;
        }
        let weak = self.me.borrow().clone();
        if weak.upgrade().is_none() {
            eprintln!("設定画面を開けません（App が結び付いていません）");
            return;
        }
        let base = self.config.read().unwrap().clone();
        let ok_app = weak.clone();
        let on_ok: settings::OnOk = Rc::new(move |base, draft| match ok_app.upgrade() {
            Some(app) => app.apply_settings(hwnd, base, draft).map(|_| ()),
            None => Err(ApplyError::Busy),
        });
        let on_closed: settings::OnClosed = Rc::new(move || {
            if let Some(app) = weak.upgrade() {
                app.on_settings_closed();
            }
        });
        match SettingsWindow::create(Some(hwnd), base, on_ok, on_closed) {
            Ok(window) => *self.settings.borrow_mut() = Some(window),
            Err(e) => self.push_settings_failures(hwnd, vec![format!("設定画面を開けません: {e}")]),
        }
    }

    /// 設定画面が閉じた（設定画面の閉じる要求の処理から呼ばれる）。欄を先に空にしてから捨てる（窓を破棄する）。
    fn on_settings_closed(&self) {
        let window = self.settings.borrow_mut().take();
        drop(window);
    }

    /// 終了のとき（メインのループを抜けた後、ビューアの後片付けの前）に設定画面を破棄する。欄を先に空にする。
    pub fn close_settings(&self) {
        let window = self.settings.borrow_mut().take();
        drop(window);
    }

    /// 設定の反映の途中の決まった位置（テストの仕掛けを呼ぶだけ。テスト以外では何もしない）。
    fn settings_test_point(&self, _stage: &'static str) {
        #[cfg(test)]
        if let Some(hook) = self.settings_hook.borrow().as_ref() {
            hook(_stage);
        }
    }

    /// 実行中にトレイを作るための送り手とビューアの起こし方を受け取る（`main` がトレイを作る前に）。
    pub fn attach_tray_source(&self, events: Sender<TrayEvent>, waker: viewer::Waker) {
        *self.tray_source.borrow_mut() = Some((events, waker));
    }

    /// 操作スレッドとつなぐ。失敗は `failures` に積まれ、ビューアが起こされる（`on_wake` で受け取る）。
    pub fn attach_actions(&self, requests: Sender<Action>, failures: FailureSink) {
        *self.actions.borrow_mut() = Some(ActionLink { requests, failures });
    }

    /// 操作を依頼する（完了を待たない）。終了の要求の後は依頼しない。
    pub fn request(&self, action: Action) {
        let _ = self.try_request(action);
    }

    /// 操作を依頼し、依頼できたかを返す（操作スレッドが無い・終了の要求の後・送り手の失敗は false）。
    fn try_request(&self, action: Action) -> bool {
        if self.exiting.get() {
            return false;
        }
        self.actions.borrow().as_ref().is_some_and(|link| link.requests.send(action).is_ok())
    }

    /// 終了の要求を受けた。以後は操作を依頼せず、失敗を知らせない。失敗の通知先を閉じ、まだ
    /// 知らせていない失敗はログに書く（閉じた後に届く失敗は、操作スレッドの側でログに書かれる）。
    /// ビューア窓を破棄する前に呼ぶ（破棄した窓へ起床を投稿させない）。行の右クリックメニューを
    /// 表示していれば閉じる（閉じたメニューの操作は依頼しない）。一度で閉じなければ、ビューアが
    /// タイマーで閉じる要求をやり直す（`viewer::retry_modal_close`）。
    fn begin_exit(&self, hwnd: Option<HWND>) {
        self.exiting.set(true);
        // 設定画面は隠すだけ（反映の中の再入で呼ばれうるので、ここでは破棄しない。`main` がループの後に破棄する）
        if let Some(window) = self.settings.borrow().as_ref() {
            window.end();
        }
        if let Some(hwnd) = hwnd {
            viewer::cancel_modal(hwnd);
            viewer::retry_modal_close(hwnd);
            self.save_viewer_size(hwnd);
        }
        let unshown: Vec<Notice> = {
            let mut notifier = self.notifier.borrow_mut();
            notifier.pending.drain(..).collect()
        };
        let closed = self.actions.borrow().as_ref().map(|link| link.failures.close()).unwrap_or_default();
        for notice in unshown.iter().chain(&closed) {
            eprintln!("{notice}");
        }
    }

    /// メッセージループを抜けた後の後片付け: 通知先を閉じ（`begin_exit`）、操作スレッドへの依頼の
    /// 送り手を手放す（操作スレッドはこの後、手元の依頼を終えて抜ける。join はしない）。
    pub fn finish_actions(&self) {
        self.begin_exit(None);
        *self.actions.borrow_mut() = None;
    }

    /// セッション終了（`on_end_session`）で終了処理をしたか。したなら `main` は最後の保存を
    /// やり直さない（時間切れの後に、無期限の待ちへ戻らない）。
    pub fn session_ended(&self) -> bool {
        self.session_ended.get()
    }

    /// 操作スレッドから届いた失敗を受け取る。設定で知らせないとき・終了の要求の後はログだけ。ただし
    /// 設定の反映の失敗（`ActionKind::ApplySettings`）と、履歴のファイルを書けなかったこと
    /// （`ActionKind::SaveHistory`）は、設定に関係なく知らせる（`ActionKind::always_notified`）。
    /// データのチェックの結果は、終了の要求の後でなければいつも出す。
    fn collect_failures(&self) {
        let notices = self.actions.borrow().as_ref().map(|link| link.failures.drain()).unwrap_or_default();
        if notices.is_empty() {
            return;
        }
        let notify_actions = self.config.read().unwrap().general.notify_action_errors;
        for notice in notices {
            let notify = !self.exiting.get()
                && match &notice {
                    Notice::Failure(failure) => notify_actions || failure.kind.always_notified(),
                    Notice::CheckReport(_) | Notice::CleanResult(_) | Notice::FolderReport(_) => true,
                };
            if notify {
                self.notifier.borrow_mut().pending.push_back(notice);
            } else {
                eprintln!("{notice}");
            }
        }
    }

    /// 設定の反映の失敗を積む（次の起床で知らせる。終了の要求の後はログだけ）。
    fn push_settings_failures(&self, hwnd: HWND, messages: Vec<String>) {
        if messages.is_empty() {
            return;
        }
        if self.exiting.get() {
            for message in &messages {
                eprintln!("設定の一部を反映できませんでした: {message}");
            }
            return;
        }
        let failure = ActionFailure { kind: ActionKind::ApplySettings, message: messages.join("\n") };
        self.notifier.borrow_mut().pending.push_back(Notice::Failure(failure));
        // 反映は設定画面の OK の中で呼ばれる。メッセージボックスはそこでは出さず、次の起床で出す
        unsafe {
            let _ = PostMessageW(Some(hwnd), viewer::WM_APP_WAKE, WPARAM(0), LPARAM(0));
        }
    }

    /// 積まれた知らせを届いた順に出す（続いた失敗は1つのメッセージボックスにまとめ、データのチェックの結果は
    /// 1つずつダイアログで出す）。表示中（再入）なら何もしない（外側のループが拾う）。表示から戻るたびに終了の
    /// 要求とビューアを隠したか（隠していれば残りは表示したときに出す）を確かめ、表示中に積まれた分は、外からの
    /// 起床を待たずに続けて出す。表示の前に `RefCell` の借用・
    /// 設定・サービスのガードを持たない。
    fn show_pending_failures(&self, hwnd: HWND) {
        // 表示中（再入）と、メニュー・確認ダイアログの表示中・ツリーの名前の編集中は出さない（閉じる・
        // 終わると窓が自分を起こすので、そのときに出る。編集中に出すとフォーカスが外れて編集が確定するため）
        if self.notifier.borrow().notifying || viewer::notice_blocked(hwnd) {
            return;
        }
        self.notifier.borrow_mut().notifying = true;
        let _reset = NotifyingReset(&self.notifier);
        loop {
            if self.exiting.get() {
                let rest: Vec<Notice> = self.notifier.borrow_mut().pending.drain(..).collect();
                for notice in &rest {
                    eprintln!("{notice}");
                }
                break;
            }
            // 隠している間は出さずに残す（表示したときに `on_shown` が起こす）
            if self.viewer_hidden.get() {
                break;
            }
            // 先頭から続く失敗はまとめて取り出す（知らせの順は保つ）
            let (failures, next) = {
                let mut notifier = self.notifier.borrow_mut();
                let mut failures = Vec::new();
                while matches!(notifier.pending.front(), Some(Notice::Failure(_))) {
                    if let Some(Notice::Failure(f)) = notifier.pending.pop_front() {
                        failures.push(f);
                    }
                }
                let next = if failures.is_empty() { notifier.pending.pop_front() } else { None };
                (failures, next)
            };
            // 借用を放してから出す（表示中の再入で差し替え・借用が起きても壊れない）
            if !failures.is_empty() {
                let show = Rc::clone(&self.show_failures.borrow());
                show(hwnd, &failure_text(&failures));
                continue;
            }
            match next {
                None | Some(Notice::Failure(_)) => break,
                Some(Notice::CheckReport(report)) => {
                    let show = Rc::clone(&self.show_check.borrow());
                    // 「削除」で閉じても、閉じている間に終了の要求が来ていたら依頼しない
                    if show(hwnd, &report) && !self.exiting.get() {
                        self.request(Action::CleanData(report));
                    }
                }
                Some(Notice::CleanResult(result)) => {
                    let show = Rc::clone(&self.show_clean.borrow());
                    show(hwnd, &result);
                }
                Some(Notice::FolderReport(report)) => {
                    let show = Rc::clone(&self.show_folder.borrow());
                    match show(hwnd, &report) {
                        // 閉じている間に終了の要求が来ていたら保存しない
                        viewer::FolderChoice::StopChecking if !self.exiting.get() => self.stop_folder_check(),
                        viewer::FolderChoice::StopChecking | viewer::FolderChoice::Continue => {}
                        // ダイアログを作れなかった: 本文だけをメッセージボックスで知らせる（警告を見ないまま終わらない）
                        viewer::FolderChoice::Failed => {
                            let show = Rc::clone(&self.show_failures.borrow());
                            show(hwnd, &format!("保存先のフォルダの権限を見直してください\n\n{}", viewer::folder_report_text(&report).0));
                        }
                        // 出せなかった（ほかの表示中）: 先頭へ戻し、次の起床で出し直す
                        viewer::FolderChoice::NotShown => {
                            self.notifier.borrow_mut().pending.push_front(Notice::FolderReport(report));
                            break;
                        }
                    }
                }
            }
        }
    }

    /// 保存先のフォルダの権限の警告がまだ出ていないのにビューアを隠していれば（非表示で起動したなど）、ビューアを出す
    /// （警告は表示したときにしか出ないため。安全と確かめられなかったときだけで、終了の要求の後は出さない）。
    fn reveal_for_folder_report(&self, hwnd: HWND) {
        let waiting = self.notifier.borrow().pending.iter().any(|n| matches!(n, Notice::FolderReport(_)));
        if waiting && self.viewer_hidden.get() && !self.exiting.get() {
            // 借用を放してから出す（表示で `on_shown` が呼ばれる）
            let reveal = Rc::clone(&self.reveal_viewer.borrow());
            reveal(hwnd);
        }
    }

    /// 「今後は確かめない」: 設定ファイルに書けてから共有の設定に反映する。書けなければ知らせる（次の起動でまた確かめる）。
    fn stop_folder_check(&self) {
        let mut next = self.config.read().unwrap().clone();
        next.general.check_folder_permissions = false;
        match next.save(&self.config_path) {
            Ok(()) => self.config.write().unwrap().general.check_folder_permissions = false,
            Err(e) => {
                let message = format!("「今後は確かめない」を設定ファイルに保存できませんでした（次の起動でまた確かめます）: {e}");
                self.notifier
                    .borrow_mut()
                    .pending
                    .push_back(Notice::Failure(ActionFailure { kind: ActionKind::ApplySettings, message }));
            }
        }
    }

    /// ビューアを表示せずに起動した（`start_hidden`）ことを伝える。表示するまで知らせを残す（`on_hidden` の後と同じ。
    /// 表示していない窓には `WM_SHOWWINDOW` が来ないため、起動の処理から呼ぶ）。
    pub fn mark_viewer_hidden(&self) {
        self.viewer_hidden.set(true);
    }

    /// テスト用: 設定ファイルの置き場所を差し替える。
    #[cfg(test)]
    fn set_config_path(&mut self, path: PathBuf) {
        self.config_path = path;
    }

    /// テスト用: 失敗の知らせ方を差し替える。
    #[cfg(test)]
    fn set_failure_display(&self, show: impl Fn(HWND, &str) + 'static) {
        *self.show_failures.borrow_mut() = Rc::new(show);
    }

    /// テスト用: データのチェック・削除の結果の出し方を差し替える。
    #[cfg(test)]
    fn set_check_display(&self, check: impl Fn(HWND, &DataReport) -> bool + 'static, clean: impl Fn(HWND, &CleanResult) + 'static) {
        *self.show_check.borrow_mut() = Rc::new(check);
        *self.show_clean.borrow_mut() = Rc::new(clean);
    }

    /// テスト用: 保存先のフォルダの権限の警告の出し方と、警告のためにビューアを出す方法を差し替える。
    #[cfg(test)]
    fn set_folder_display(
        &self,
        show: impl Fn(HWND, &FolderReport) -> viewer::FolderChoice + 'static,
        reveal: impl Fn(HWND) + 'static,
    ) {
        *self.show_folder.borrow_mut() = Rc::new(show);
        *self.reveal_viewer.borrow_mut() = Rc::new(reveal);
    }

    /// 検索スレッドとつなぐ。結果が届いたら検索スレッドがビューアを起こす（`on_wake` で受け取る）。
    pub fn attach_search(&self, commands: Sender<search::Command>, results: Receiver<SearchResult>) {
        *self.search.borrow_mut() = Some(SearchLink { commands, results });
    }

    /// 検索スレッドとのつなぎを外す（依頼の送り手を手放すので、検索スレッドは終わる）。
    pub fn detach_search(&self) {
        *self.search.borrow_mut() = None;
    }

    pub fn set_parts(&self, parts: Parts) {
        *self.parts.borrow_mut() = Some(parts);
    }

    pub fn take_parts(&self) -> Option<Parts> {
        self.parts.borrow_mut().take()
    }

    /// 監視 ON/OFF の切り替え（トレイメニューから）。実際の状態の反対へ切り替え、できたら設定ファイルにも
    /// 反映する。できなければ設定は変えず、アイコンは実際の状態のまま（ログへ書く）。
    fn toggle_watch(&self, parts: &Parts) {
        let enabled = !parts.watcher.listening();
        if let Err(e) = parts.watcher.set_watch(enabled) {
            eprintln!("クリップボードの監視を{}できません: {e}", if enabled { "開始" } else { "停止" });
        } else {
            self.config.write().unwrap().general.clipboard_watch = enabled;
            self.save_config();
        }
        if let Some(tray) = &parts.tray {
            tray.set_watch_icon(parts.watcher.listening());
        }
    }

    /// 設定ファイルへ書く（小さな TOML の書き込み。メインスレッドで行う）。
    fn save_config(&self) {
        if let Err(e) = self.config.read().unwrap().save(&self.config_path) {
            eprintln!("設定の保存に失敗: {e}");
        }
    }

    /// 最前面に表示の切り替え（「ツール」から）。設定ファイルにも反映し、次の起動でも使う。
    fn toggle_topmost(&self, hwnd: HWND) {
        let on = {
            let mut cfg = self.config.write().unwrap();
            cfg.general.viewer_always_on_top = !cfg.general.viewer_always_on_top;
            cfg.general.viewer_always_on_top
        };
        viewer::set_topmost(hwnd, on);
        self.save_config();
    }

    /// ビューアの大きさ（最小化・最大化していないときのクライアント領域、96 DPI 基準）を、
    /// 変わっていれば設定ファイルへ書く（隠すとき・終了時）。下限より小さい値は下限にする。
    /// 共有の設定へは書けてから反映する（書けなかったときは、次に隠す・終了するときに書き直す）。
    fn save_viewer_size(&self, hwnd: HWND) {
        let Some((w, h)) = viewer::normal_size(hwnd) else {
            return;
        };
        let size = (w.max(crate::config::VIEWER_MIN_WIDTH), h.max(crate::config::VIEWER_MIN_HEIGHT));
        let mut next = self.config.read().unwrap().clone();
        if (next.general.viewer_width, next.general.viewer_height) == size {
            return;
        }
        next.general.viewer_width = size.0;
        next.general.viewer_height = size.1;
        match next.save(&self.config_path) {
            Ok(()) => {
                let mut cfg = self.config.write().unwrap();
                cfg.general.viewer_width = size.0;
                cfg.general.viewer_height = size.1;
            }
            Err(e) => eprintln!("ビューアの大きさの保存に失敗: {e}"),
        }
    }

    /// 設定画面の OK: 入力を確かめ、開いたとき（`base`）・画面で編集した値（`draft`）・今の値を合わせて保存し、
    /// その場で反映する。メインスレッドで呼ぶ。
    ///
    /// 断る（`Busy`）: 反映の途中・終了の要求の後・メニューや TaskDialog の表示中・失敗の通知の表示中（設定画面は
    /// モードレスなので、ほかのモーダルの中でも OK を押しうる）。入力の誤りは `Invalid`、保存の失敗は `Save`
    /// で、どれも何もしない。保存できた後にその場で反映できなかったものは、保存を戻さずに**この関数が知らせる**
    /// （次の起床で。呼び出し側は知らせない）。戻り値の説明は、その内容の写し。
    ///
    /// 順序: 今の値を読む → 合わせる → 保存 → 共有の設定を置き換え（この区間はメッセージを処理する呼び出しを
    /// しないので、ほかの設定の保存（すべてメインスレッド）が割り込まない）→ 監視 → ホットキー（投稿で依頼して
    /// 待たない）→ 切り詰めの依頼 → トレイを作る（起動時と同じく作り終えるのを待つ。待つ間はメッセージを処理
    /// しない）→ 一覧の作り直し（子コントロールへ送るので再入しうる）→ トレイを消す（借用の外で。送られた
    /// メッセージを処理しながら待つ）→ トレイが無くビューアが隠れていれば出す。途中の各段の後に終了の要求・
    /// セッションの終了を確かめ、来ていればそこで止める（残りを反映せず、反映できなかったものはログへ）。
    pub fn apply_settings(&self, hwnd: HWND, base: &Config, draft: &Config) -> Result<Vec<String>, ApplyError> {
        if self.applying.get() || self.exiting.get() || self.notifier.borrow().notifying || viewer::modal_is_open(hwnd) {
            return Err(ApplyError::Busy);
        }
        let issues = draft.validate();
        if !issues.is_empty() {
            return Err(ApplyError::Invalid(issues));
        }
        self.applying.set(true);
        let _applying = FlagReset(&self.applying);

        // 再入しない区間
        let current = self.config.read().unwrap().clone();
        let merged = merge_edit(base, draft, &current).map_err(|e| ApplyError::Save(e.to_string()))?;
        merged.save(&self.config_path).map_err(|e| ApplyError::Save(e.to_string()))?;
        *self.config.write().unwrap() = merged.clone();

        let effects = settings::effects(&current, &merged);
        let mut failures = Vec::new();
        // 終了の要求・セッションの終了が来ていれば、残りを反映せずに止める
        let stopped = || self.exiting.get() || self.session_ended.get();
        {
            // この借用の中では、別のスレッドの窓へ同期で送る呼び出しをしない（監視の登録は Win32 の API、アイコンと
            // ホットキーは投稿）。トレイを作る・一覧を作り直す・トレイを消す前に手放す
            let parts = self.parts.borrow();
            if let Some(parts) = parts.as_ref() {
                let watch = merged.general.clipboard_watch;
                if parts.watcher.listening() != watch {
                    if let Err(e) = parts.watcher.set_watch(watch) {
                        failures.push(format!("クリップボードの監視を{}できません: {e}", if watch { "開始" } else { "停止" }));
                    }
                    if let Some(tray) = &parts.tray {
                        tray.set_watch_icon(parts.watcher.listening());
                    }
                }
                if effects.hotkeys {
                    match &parts.hotkeys {
                        Some(hotkeys) => {
                            let generation = self.settings_generation.get() + 1;
                            self.settings_generation.set(generation);
                            if let Err(e) = hotkeys.reregister(generation) {
                                failures.push(format!("ホットキーの設定を反映できません: {e}"));
                            }
                        }
                        None => failures.push(
                            "ホットキーの設定を反映できません（ホットキーの機能を起動できていません。次の起動でもう一度試します）"
                                .to_string(),
                        ),
                    }
                }
            }
        }
        if effects.trim && !self.try_request(Action::Trim) {
            failures.push("保持件数の切り詰めを依頼できません（次の起動で切り詰めます）".to_string());
        }
        self.settings_test_point("before_tray_create");
        if stopped() {
            return Ok(self.log_settings_failures(failures));
        }
        if merged.general.show_trayicon {
            let missing = self.parts.borrow().as_ref().is_some_and(|p| p.tray.is_none());
            if missing {
                match self.spawn_tray() {
                    Ok(tray) => {
                        if let Some(parts) = self.parts.borrow_mut().as_mut() {
                            parts.tray = Some(tray);
                        }
                    }
                    Err(e) => failures.push(format!("トレイアイコンを作れません: {e}")),
                }
            }
        }
        if stopped() {
            return Ok(self.log_settings_failures(failures));
        }
        if effects.rebuild {
            self.refresh(hwnd, false);
        }
        self.settings_test_point("after_refresh");
        if stopped() {
            return Ok(self.log_settings_failures(failures));
        }
        if !merged.general.show_trayicon {
            let tray = self.parts.borrow_mut().as_mut().and_then(|p| p.tray.take());
            // 借用を手放してから破棄する（破棄の待ちの間に、送られたメッセージでハンドラが再入しうる）
            self.settings_test_point("tray_taken");
            drop(tray);
            if stopped() {
                return Ok(self.log_settings_failures(failures));
            }
        }
        if !self.tray_icon_shown() && !viewer::is_visible(hwnd) {
            viewer::show(hwnd);
        }
        self.push_settings_failures(hwnd, failures.clone());
        Ok(failures)
    }

    /// 終了の要求の後の、反映できなかったもの（知らせずにログへ）。
    fn log_settings_failures(&self, failures: Vec<String>) -> Vec<String> {
        for failure in &failures {
            eprintln!("設定の一部を反映できませんでした: {failure}");
        }
        failures
    }

    /// トレイがあり、通知領域にアイコンを付けられているか（`Tray::icon_shown`）。無ければ、ビューアを隠すと
    /// トレイから戻れない。
    fn tray_icon_shown(&self) -> bool {
        self.parts.borrow().as_ref().is_some_and(|p| p.tray.as_ref().is_some_and(Tray::icon_shown))
    }

    /// 実行中にトレイを作る（起動時と同じ引数。作ったトレイの操作も `tray_events` に届く）。
    fn spawn_tray(&self) -> Result<Tray, String> {
        let source = self.tray_source.borrow().as_ref().map(|(tx, waker)| (tx.clone(), *waker));
        let Some((events, waker)) = source else {
            return Err("トレイを作る準備ができていません".to_string());
        };
        let watch_state = self
            .parts
            .borrow()
            .as_ref()
            .map(|p| p.watcher.watch_state())
            .ok_or_else(|| "クリップボードの監視がありません".to_string())?;
        Tray::spawn(watch_state, events, move || waker.wake()).map_err(|e| e.to_string())
    }

    /// ツリーと一覧を作り直す。非表示の間は印だけ付けて、表示したときに行う
    /// （見えない一覧のために行を作らない）。`source_changed` なら先頭の行を選ぶ。
    fn refresh(&self, hwnd: HWND, source_changed: bool) {
        if !viewer::is_visible(hwnd) {
            self.dirty.set(true);
            return;
        }
        self.rebuild(hwnd, source_changed);
    }

    /// 可視判定をせずに作り直す（`WM_SHOWWINDOW` の時点では `IsWindowVisible` がまだ
    /// false のことがあるため、`on_shown` からはこちらを呼ぶ）。
    fn rebuild(&self, hwnd: HWND, source_changed: bool) {
        self.dirty.set(false);
        let grouping = self.config.read().unwrap().history.grouping.clone();
        let needle = self.needle.borrow().clone();
        let current = self.source.get();
        let read = self.core.read(|service| {
            let tree = model::build_tree(service.history.len(), &grouping, &service.pinned);
            // 表示中の対象が消えた（ピン留めフォルダの削除、階層フォルダの減少）ときは履歴へ戻す
            let source = if tree_contains(&tree, current) { current } else { Source::History };
            let listing = if needle.is_empty() {
                Listing::Rows(model::rows_for(service, source, &grouping))
            } else {
                let (candidates, rows) = search_candidates(service, source);
                Listing::Search(candidates, rows)
            };
            (tree, source, listing, service.revision(), service.history.len())
        });
        // サービスのロックが壊れている（別のスレッドのパニック）ときは作り直さない
        let Some((tree, source, listing, revision, history_count)) = read else {
            return;
        };
        self.last_revision.set(Some(revision));
        // 件数はツリーの構成と別に書き換える（取り込みのたびにツリーを作り直さない）
        viewer::set_history_count(hwnd, history_count);
        let source_changed = source_changed || source != self.source.get();
        self.source.set(source);
        let tree_changed = self.last_tree.borrow().as_ref() != Some(&tree);
        if tree_changed {
            viewer::set_tree(hwnd, &tree, source);
            *self.last_tree.borrow_mut() = Some(tree);
        }
        match listing {
            Listing::Rows(rows) => {
                // 待っていた検索の結果は、届いても使わない
                *self.pending_search.borrow_mut() = None;
                let selected = viewer::set_rows(hwnd, rows, source_changed);
                self.update_preview(hwnd, selected);
            }
            // 一覧は結果が届くまで今のまま（`receive_search_results` で差し替える）
            Listing::Search(candidates, rows) => self.start_search(needle, candidates, rows, source_changed),
        }
    }

    /// 検索スレッドへ検索を頼む。前に待っていた検索の結果は使わない。
    fn start_search(&self, needle: String, candidates: Vec<search::Candidate>, rows: Vec<RowSource>, source_changed: bool) {
        let generation = self.search_generation.get().wrapping_add(1);
        self.search_generation.set(generation);
        // 前の検索の結果をまだ出していなければ、先頭を選ぶかどうかも引き継ぐ
        let source_changed = source_changed
            || self.pending_search.borrow().as_ref().is_some_and(|p| p.source_changed);
        *self.pending_search.borrow_mut() = Some(PendingSearch { generation, source_changed, rows });
        if let Some(link) = self.search.borrow().as_ref() {
            let _ = link.commands.send(search::Command::Search(search::SearchRequest { generation, needle, candidates }));
        }
    }

    /// 届いた検索の結果を受け取り、待っていた世代のものなら一覧を差し替える。隠している間に
    /// 届いたものは使わず、表示したときに作り直す。
    fn receive_search_results(&self, hwnd: HWND) {
        let results: Vec<SearchResult> =
            self.search.borrow().as_ref().map(|link| link.results.try_iter().collect()).unwrap_or_default();
        let waiting = self.pending_search.borrow().as_ref().map(|p| p.generation);
        let Some(result) = results.into_iter().find(|r| Some(r.generation) == waiting) else {
            return;
        };
        let Some(pending) = self.pending_search.borrow_mut().take() else {
            return;
        };
        if !viewer::is_visible(hwnd) {
            self.dirty.set(true);
            return;
        }
        let rows = rows_from_matches(pending.rows, &result.matched);
        let selected = viewer::set_rows(hwnd, rows, pending.source_changed);
        self.update_preview(hwnd, selected);
    }

    /// 選択中の項目をプレビューに出す（同じ項目なら何もしない）。選んでいなければ、プレビューに出す
    /// ものが無いことを出す（作った直後の空の欄にも出すため、毎回出し直す）。
    fn update_preview(&self, hwnd: HWND, id: Option<Uuid>) {
        if id.is_some() && self.preview_id.get() == id {
            return;
        }
        self.preview_id.set(id);
        let Some(id) = id else {
            viewer::set_preview_none(hwnd);
            return;
        };
        let source = self.core.read(|service| preview_source(service, id)).flatten();
        match source.map(build_preview) {
            Some(PreviewContent::Image(image)) => viewer::show_preview_image(hwnd, id, image),
            Some(PreviewContent::Text(text)) => viewer::set_preview_text(hwnd, &text),
            Some(PreviewContent::Nothing) | None => viewer::set_preview_none(hwnd),
        }
    }
}

fn tree_contains(nodes: &[TreeNode], source: Source) -> bool {
    nodes.iter().any(|n| n.source == source || tree_contains(&n.children, source))
}

fn resident_head(formats: &[Format], name: &str, cap: usize) -> Option<ResidentHead> {
    formats
        .iter()
        .find(|f| f.format_name == name)
        .map(|f| ResidentHead { data: f.data[..f.data.len().min(cap)].to_vec(), total: f.data.len() })
}

/// 検索の候補を写す（ロック中に呼ぶ。I/O はせず、全文も写さない。全文の在りかだけを渡し、
/// 読み込みは検索スレッドが行う）。履歴系の表示中は履歴の全件（階層表示の区画は無視して横断）、
/// ピン留めの表示中はフォルダをまたいだピン留めの全件。
fn search_candidates(service: &HistoryService, source: Source) -> (Vec<search::Candidate>, Vec<RowSource>) {
    let rows: Vec<RowSource> = match source {
        Source::History | Source::HistoryGroup(_) => service
            .history
            .iter()
            .map(|item| RowSource {
                meta: item.meta.clone(),
                extra_formats: item.resident.iter().map(|f| f.format_name.clone()).collect(),
                pinned: false,
            })
            .collect(),
        Source::Pinned(_) => {
            let mut metas = Vec::new();
            for node in &service.pinned {
                store::collect_item_metas(node, &mut metas);
            }
            metas.into_iter().map(|meta| RowSource { meta, extra_formats: Vec::new(), pinned: true }).collect()
        }
    };
    let candidates = rows
        .iter()
        .map(|r| {
            let text = match r.meta.formats.iter().find(|f| f.format_name == "CF_UNICODETEXT") {
                Some(f) => TextSource::Blob(f.blob.clone()),
                None if r.extra_formats.iter().any(|name| name == "CF_UNICODETEXT") => TextSource::Resident,
                None => TextSource::None,
            };
            search::Candidate { id: r.meta.id, title: r.meta.title.clone(), text }
        })
        .collect();
    (candidates, rows)
}

/// 検索の結果（一致した ID）に入っている候補だけを、候補の順で一覧の行にする。
fn rows_from_matches(rows: Vec<RowSource>, matched: &[Uuid]) -> Vec<Row> {
    let matched: HashSet<Uuid> = matched.iter().copied().collect();
    rows.into_iter()
        .filter(|r| matched.contains(&r.meta.id))
        .map(|r| {
            let extra: Vec<&str> = r.extra_formats.iter().map(String::as_str).collect();
            Row { pinned: r.pinned, ..model::row_from_meta(&r.meta, &extra) }
        })
        .collect()
}

/// プレビューの読み込み元を写す（ロック中に呼ぶ。I/O はせず、resident は `Arc` の写しだけ）。
fn preview_source(service: &HistoryService, id: Uuid) -> Option<PreviewSource> {
    let (meta, resident) = match service.history.get_by_id(id) {
        Some(item) => (&item.meta, Arc::clone(&item.resident)),
        None => (store::find_item(&service.pinned, id)?, Arc::default()),
    };
    Some(PreviewSource { meta: meta.clone(), resident, storage: service.storage_handle() })
}

/// プレビューに出すものを決める（ロックの外で呼ぶ）。優先順はテキスト＞画像＞
/// ファイル一覧。テキストとファイル一覧は先頭だけを読み、省略したことを書き添える。画像は
/// 読み込み元だけを返す（読み込みスレッドが読む）。
fn build_preview(s: PreviewSource) -> PreviewContent {
    let resident_text = resident_head(&s.resident, "CF_UNICODETEXT", PREVIEW_TEXT_BYTES);
    let resident_hdrop = resident_head(&s.resident, "CF_HDROP", PREVIEW_HDROP_BYTES);
    // メモリだけに持つ画像は写さず、resident の `Arc` のまま読み込みスレッドへ渡す（大きさの判定と
    // 縮小は読み込みスレッドが行う）
    let resident_dib = s.resident.iter().any(|f| f.format_name == "CF_DIB");
    let format = |name: &str| s.meta.formats.iter().find(|f| f.format_name == name);
    // 先頭の写しと、全体の大きさ（ディスクに書かない形式なら写す前の大きさ、ディスクにある形式なら
    // メタデータの大きさ）
    let head = |resident: Option<ResidentHead>, name: &str, cap: usize| -> Option<(Vec<u8>, u64)> {
        match resident {
            Some(r) => Some((r.data, r.total as u64)),
            None => {
                let f = format(name)?;
                let data = s.storage.load_blob_prefix(&f.blob, cap).ok()?;
                Some((data, f.size))
            }
        }
    };
    if let Some((data, total_bytes)) = head(resident_text, "CF_UNICODETEXT", PREVIEW_TEXT_BYTES) {
        // 全体の文字数は末尾の NUL を除く
        let total_units = (total_bytes as usize / 2).saturating_sub(1);
        let truncated = total_bytes as usize > PREVIEW_TEXT_BYTES;
        return PreviewContent::Text(model::preview_text(&utf16_text(&data), truncated, total_units));
    }
    if resident_dib {
        return PreviewContent::Image(ImageSource::Resident(Arc::clone(&s.resident)));
    }
    if let Some(f) = format("CF_DIB") {
        return PreviewContent::Image(ImageSource::Blob(f.blob.clone()));
    }
    if let Some((data, total_bytes)) = head(resident_hdrop, "CF_HDROP", PREVIEW_HDROP_BYTES) {
        let truncated = total_bytes as usize > PREVIEW_HDROP_BYTES;
        let mut paths = crate::hdrop::parse_hdrop(&data);
        if truncated {
            // 先頭だけを読んだので、最後のパスは途中で切れているかもしれない
            paths.pop();
        }
        return PreviewContent::Text(model::preview_paths(&paths, truncated));
    }
    PreviewContent::Nothing
}

/// 表示のループを抜けたら（早期の return・パニックでも）`notifying` を下ろす。`RefMut` は持たず、
/// 下ろすときだけ短く借りる。
struct NotifyingReset<'a>(&'a RefCell<Notifier>);

impl Drop for NotifyingReset<'_> {
    fn drop(&mut self) {
        self.0.borrow_mut().notifying = false;
    }
}

/// 失敗の知らせの本文（1件1行）。
fn failure_text(failures: &[ActionFailure]) -> String {
    failures.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")
}

/// 失敗をメッセージボックスで知らせる（持ち主はビューア窓。メインスレッドで呼ぶ）。
fn show_failure_box(hwnd: HWND, text: &str) {
    show_message_box(Some(hwnd), crate::APP_DISPLAY_NAME, text);
}

fn show_message_box(owner: Option<HWND>, caption: &str, text: &str) {
    unsafe {
        MessageBoxW(owner, &HSTRING::from(text), &HSTRING::from(caption), MB_OK | MB_ICONWARNING);
    }
}

impl ViewerHandler for App {
    fn on_wake(&self, hwnd: HWND) {
        let hotkey_events: Vec<HotkeyEvent> = self.hotkey_events.try_iter().collect();
        // 終了の要求の後に届いたトレイ・ホットキーのイベントは捨てる（窓を出す・監視と設定ファイルを変えるなどを
        // しない）。登録し直しの結果はログへ書く（下）
        for event in hotkey_events {
            match event {
                HotkeyEvent::ShowViewer if self.exiting.get() => {}
                HotkeyEvent::ShowViewer => viewer::show(hwnd),
                // 最後に頼んだ登録し直しの結果だけを知らせる（古い番号の結果は、その後の反映で上書きされている）
                HotkeyEvent::Reregistered { generation, problems } => {
                    if generation == self.settings_generation.get() && !problems.is_empty() {
                        if self.exiting.get() {
                            self.log_settings_failures(problems);
                        } else {
                            // 設定は OK のときに保存済み（`apply_settings`）。有効のまま残っている理由を添える
                            let message = format!("{}\n{REREGISTER_RETRY_NOTE}", problems.join("\n"));
                            self.notifier
                                .borrow_mut()
                                .pending
                                .push_back(Notice::Failure(ActionFailure { kind: ActionKind::ApplySettings, message }));
                        }
                    }
                }
            }
        }

        let tray_events: Vec<TrayEvent> = self.tray_events.try_iter().collect();
        for event in tray_events {
            // 同じ起床の中で「終了」の後に届いたものも捨てる
            if self.exiting.get() {
                continue;
            }
            match event {
                // ホットキースレッドが無い場合（起動失敗）はポップアップを出せないため
                // ビューアの表示にフォールバックする
                TrayEvent::LeftClick => {
                    let parts = self.parts.borrow();
                    match parts.as_ref().and_then(|p| p.hotkeys.as_ref()) {
                        Some(hotkeys) => hotkeys.show_popup_menu_at_mouse(),
                        None => viewer::show(hwnd),
                    }
                }
                TrayEvent::ShowViewer => viewer::show(hwnd),
                TrayEvent::ToggleWatch => {
                    if let Some(parts) = self.parts.borrow().as_ref() {
                        self.toggle_watch(parts);
                    }
                }
                TrayEvent::Exit => {
                    self.begin_exit(Some(hwnd));
                    unsafe { PostQuitMessage(0) };
                }
            }
        }

        self.receive_search_results(hwnd);

        // 起床の理由には履歴の追加（監視・ホットキーからの送出）も含まれる。サービスの変更番号が
        // 前に作ったときから変わったときだけ作り直す
        if let Some(revision) = self.core.read(|s| s.revision()) {
            if self.last_revision.get() != Some(revision) {
                self.refresh(hwnd, false);
            }
        }

        // 操作の失敗は最後に知らせる（メッセージボックスのモーダルループの中で再入が起きても、
        // この起床の処理は終わっている）
        self.collect_failures();
        self.reveal_for_folder_report(hwnd);
        self.show_pending_failures(hwnd);
    }

    /// 閉じる = トレイへ隠す。トレイのアイコンが無い（設定で無効・作成失敗・通知領域に付けられない）ときは
    /// 窓へ戻る手段がなくなるため終了する。
    fn on_close(&self, hwnd: HWND) {
        if self.tray_icon_shown() {
            viewer::hide(hwnd);
        } else {
            self.begin_exit(Some(hwnd));
            unsafe { PostQuitMessage(0) };
        }
    }

    fn on_shown(&self, hwnd: HWND) {
        self.viewer_hidden.set(false);
        if self.dirty.get() {
            self.rebuild(hwnd, true);
        }
        // 隠している間に残した知らせを出す（ここは `WM_SHOWWINDOW` の中なので、モーダルは起きた後に出す）
        if !self.notifier.borrow().pending.is_empty() {
            unsafe {
                let _ = PostMessageW(Some(hwnd), viewer::WM_APP_WAKE, WPARAM(0), LPARAM(0));
            }
        }
        // ビューアと一緒に隠した設定画面を戻す（編集中の内容のまま。終了の要求の後は出さない）
        if let Some(window) = self.settings.borrow().as_ref() {
            window.show();
        }
    }

    fn on_open_settings(&self, hwnd: HWND) {
        self.open_settings(hwnd);
    }

    fn on_source_selected(&self, hwnd: HWND, source: Source) {
        self.source.set(source);
        self.refresh(hwnd, true);
    }

    /// 隠したら、プレビューの中身と検索用の全文を捨てる（非表示の常駐メモリを増やさないため）。
    /// 一覧の写しと検索文字列は残す（表示したときに作り直す）。
    fn on_hidden(&self, hwnd: HWND) {
        self.viewer_hidden.set(true);
        // 設定画面もビューアと一緒に隠す（編集中の内容はそのまま）
        if let Some(window) = self.settings.borrow().as_ref() {
            window.hide();
        }
        self.save_viewer_size(hwnd);
        self.preview_id.set(None);
        viewer::set_preview_none(hwnd);
        if let Some(link) = self.search.borrow().as_ref() {
            let _ = link.commands.send(search::Command::ClearCache);
        }
        self.dirty.set(true);
    }

    fn on_selection_changed(&self, hwnd: HWND, id: Option<Uuid>) {
        self.update_preview(hwnd, id);
    }

    /// DPI が変わるとプレビューの画像は捨てられるので、選択中の項目のプレビューを出し直す。
    fn on_metrics_changed(&self, hwnd: HWND) {
        self.preview_id.set(None);
        self.update_preview(hwnd, viewer::selected_id(hwnd));
    }

    /// サインアウト・再起動・シャットダウン。この後は `main` の終了処理（保存を含む）まで
    /// 進む保証がないため、ここで受け付けを閉じて最後の保存をする。受け付け済みの操作が
    /// 終わるのを待つのは最大 `END_SESSION_WAIT` で、終わらなければ保存しない。
    /// 待つのは処理中の操作の終わりまでで、時間切れでも処理中の操作は止まらない。失敗の通知は
    /// 閉じ、表示中のメッセージボックスは閉じない（この後プロセスは終わらされる）。
    fn on_end_session(&self, hwnd: HWND) {
        self.begin_exit(Some(hwnd));
        self.session_ended.set(true);
        if let Err(e) = self.core.shutdown(Some(END_SESSION_WAIT)) {
            eprintln!("セッション終了時の終了処理が完了しませんでした: {e}");
        }
    }

    fn on_search_changed(&self, hwnd: HWND, text: String) {
        let needle = model::search_needle(&text);
        if *self.needle.borrow() == needle {
            return;
        }
        *self.needle.borrow_mut() = needle;
        self.refresh(hwnd, true);
    }

    fn on_activate(&self, hwnd: HWND, target: RowTarget) {
        self.on_row_command(hwnd, target, RowCommand::Send);
    }

    fn on_delete(&self, hwnd: HWND, target: RowTarget) {
        self.on_row_command(hwnd, target, RowCommand::Delete);
    }

    /// 行がピン留めの項目ならピン留めから、そうでなければ履歴から探し、その形式から決める。
    /// ロックの中では読み込み元（メタデータと resident の `Arc`）を写すだけ。
    /// ピン留めの入れる先の並び（ルートとフォルダ）と、ピン留めの行なら今いる所も、メタデータから
    /// 組み立てる。フォルダの行は、そのフォルダがあれば、開く・移動・名前の変更・削除と並べ替えだけ（移動の
    /// 入れる先からは、自分とその中を除く）。
    /// 並べ替えは、ピン留めの行を検索せずに表示しているときだけ（検索の結果はフォルダをまたぐため）。
    fn row_menu(&self, target: RowTarget) -> Option<RowMenu> {
        let RowTarget { id, pinned, folder } = target;
        let reorderable = pinned && self.needle.borrow().is_empty();
        let reorder_of = |s: &HistoryService| {
            reorderable.then(|| store::shift_bounds(&s.pinned, id)).flatten().map(|(up, down)| Reorder { up, down })
        };
        if folder {
            return self
                .core
                .read(|s| {
                    store::find_folder(&s.pinned, id).map(|_| RowMenu {
                        folder: true,
                        reorder: reorder_of(s),
                        pin_targets: model::pin_targets_except(&s.pinned, Some(id)),
                        current: store::parent_of(&s.pinned, id),
                        ..RowMenu::default()
                    })
                })
                .flatten();
        }
        let (source, targets, current, reorder) = self
            .core
            .read(|s| {
                let source = if pinned { s.pinned_source(id) } else { s.history_source(id) };
                let current = if pinned { store::parent_of(&s.pinned, id) } else { None };
                (source, model::pin_targets(&s.pinned), current, reorder_of(s))
            })?;
        let source = source?;
        let names = source.meta.formats.iter().map(|f| f.format_name.as_str());
        let resident = source.resident.iter().map(|f| f.format_name.as_str());
        let mut menu = RowMenu::from_formats(names.chain(resident), !pinned);
        menu.pin_targets = targets;
        menu.current = current;
        menu.reorder = reorder;
        Some(menu)
    }

    /// 操作スレッドへ依頼する（完了を待たない）。対象の保管先は行が決める（今の表示元からは
    /// 決めない）。ピン留めは履歴の行だけ、移動はピン留めの行だけ、並べ替えとドラッグでの移動はピン留めの行を
    /// 検索せずに表示しているときだけ。フォルダの行は移動・並べ替えだけ（開く・名前の変更・確認の後の削除は
    /// ビューアが扱う）。
    fn on_row_command(&self, _hwnd: HWND, target: RowTarget, command: RowCommand) {
        let RowTarget { id, pinned, folder } = target;
        let unfiltered = || self.needle.borrow().is_empty();
        let action = match command {
            RowCommand::Reorder(direction) if pinned && unfiltered() => Action::ReorderPinned { id, direction },
            RowCommand::Place { to, before } if pinned && unfiltered() => Action::MovePinned { id, to, before },
            RowCommand::Move(to) if pinned => Action::MovePinned { id, to, before: None },
            RowCommand::Reorder(_) | RowCommand::Place { .. } | RowCommand::Move(_) => return,
            _ if folder => return,
            RowCommand::Send => Action::Send { id, pinned },
            RowCommand::Pin(_) if pinned => return,
            RowCommand::Pin(to) => Action::Pin { id, to },
            RowCommand::OpenImage => Action::OpenImage { id, pinned },
            RowCommand::OpenImageLocation => Action::OpenImageLocation { id, pinned },
            RowCommand::Transform(transform) => Action::Transform { id, pinned, transform },
            RowCommand::Delete => Action::Delete { id, pinned },
            // 名前はビューアが聞いてから `on_rename_pinned` で来る
            RowCommand::Rename => return,
        };
        self.request(action);
    }

    /// ピン留めの行（移動）は、検索せずに表示しているときだけ（並べ替えと同じ。検索の結果はフォルダをまたぐため）。
    /// 履歴の行（ピン留め）は、検索中もできる（右クリックの「ピン留めに追加」と同じ）。
    fn can_drag_row(&self, target: RowTarget) -> bool {
        !target.pinned || self.needle.borrow().is_empty()
    }

    fn pinned_title(&self, id: Uuid) -> Option<Option<String>> {
        self.core.read(|s| store::find_item(&s.pinned, id).map(|meta| meta.title.clone())).flatten()
    }

    /// 操作スレッドへ依頼する（完了を待たない。保存が済んだ後の起床で一覧に新しい名前が出る）。
    fn on_rename_pinned(&self, _hwnd: HWND, id: Uuid, title: String) {
        self.request(Action::RenamePinned { id, title });
    }

    /// 履歴のクリア（確認はビューアが済ませてある）・クリップボードのクリアは操作スレッドへ依頼する
    /// （完了を待たない。ファイルの削除・クリップボードの排他の待ちをメインスレッドでしない）。
    fn on_tool_command(&self, hwnd: HWND, command: ToolCommand) {
        match command {
            ToolCommand::ClearHistory => self.request(Action::ClearHistory),
            ToolCommand::ClearClipboard => self.request(Action::ClearClipboard),
            // 走査は操作スレッドで行い、結果は知らせ（`Notice::CheckReport`）で届く
            ToolCommand::CheckData => self.request(Action::CheckData),
            ToolCommand::ToggleTopmost => self.toggle_topmost(hwnd),
        }
    }

    /// 「履歴」には履歴のクリア、ピン留めの根にはフォルダの作成、ピン留めのフォルダには作成・名前の変更・
    /// 削除。履歴の階層表示のフォルダには出さない。ロックの中ではフォルダを
    /// 探して数えるだけ。削除の確認の直前にも呼ばれる（名前・件数を取り直す）。
    fn tree_menu(&self, source: Source) -> Option<TreeMenu> {
        match source {
            Source::History => Some(TreeMenu::History),
            Source::HistoryGroup(_) => None,
            Source::Pinned(None) => Some(TreeMenu::PinnedRoot),
            Source::Pinned(Some(id)) => self.core.read(|s| model::pinned_folder_menu(&s.pinned, id)).flatten(),
        }
    }

    /// 操作スレッドへ依頼する（完了を待たない。作成・名前の変更は、保存が済んだ後の起床でツリーに出る）。
    /// フォルダは `Core::delete_pinned` が中身ごと消す。
    fn on_tree_command(&self, _hwnd: HWND, command: TreeCommand) {
        let action = match command {
            TreeCommand::DeleteFolder(id) => Action::Delete { id, pinned: true },
            TreeCommand::CreateFolder { parent, title } => Action::CreateFolder { parent, title },
            TreeCommand::RenameFolder { id, title } => Action::RenameFolder { id, title },
        };
        self.request(action);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Entry;
    use crate::ops::tests::{memory_only_config, temp_core, text_entry};

    /// 一時フォルダの `Core`（呼び出し側が最後にフォルダを消す）。
    fn temp_service(config: Config) -> (std::path::PathBuf, Core) {
        temp_core(config)
    }

    fn test_app(core: &Core) -> App {
        let (_tray_tx, tray_rx) = std::sync::mpsc::channel();
        let (_hotkey_tx, hotkey_rx) = std::sync::mpsc::channel();
        App::new(Arc::new(RwLock::new(memory_only_config())), core.clone(), tray_rx, hotkey_rx)
    }

    /// 既定の設定（終了時だけ保存）では、取り込んだだけでは履歴のインデックスを書かない。
    /// セッションの終了の通知で書く（サインアウトでは main の終了処理まで進む保証がない）。
    #[test]
    fn end_session_saves_history_index_with_default_config() {
        let (dir, service) = temp_service(Config::default());
        service.capture(text_entry("サインアウト前")).unwrap();
        assert!(!dir.join("history.toml").exists(), "前提: 取り込みの時点でインデックスを書いている");

        test_app(&service).on_end_session(HWND::default());
        assert!(dir.join("history.toml").exists());
        let reopened = Core::open_at(dir.clone(), Arc::new(RwLock::new(Config::default()))).unwrap();
        assert_eq!(reopened.read(|s| s.history.len()), Some(1));
        drop(reopened);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 最前面の切り替えは設定ファイルに書く。ビューアの大きさ（最小化・最大化していないときの
    /// クライアント領域、96 DPI 基準）は、隠すときに変わっていれば書き、変わっていなければ書かない。
    #[test]
    fn topmost_and_viewer_size_are_saved_to_config_file() {
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::UI::HiDpi::GetDpiForWindow;
        use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, SIZE_RESTORED, WM_SIZE};
        let _gui = crate::tray::lock_gui_resource_tests();
        let (dir, service) = temp_service(Config::default());
        let path = dir.join("config.toml");
        let mut app = test_app(&service);
        app.set_config_path(path.clone());
        let app = Rc::new(app);
        let handler: Rc<dyn ViewerHandler> = app.clone();
        let window = viewer::ViewerWindow::create("CLCLR app size test", (640, 480), handler).unwrap();
        let hwnd = window.hwnd();

        app.on_tool_command(hwnd, ToolCommand::ToggleTopmost);
        assert!(Config::load(&path).unwrap().general.viewer_always_on_top);

        let dpi = unsafe { GetDpiForWindow(hwnd) };
        let px = |v: i32| crate::menu_tooltip::scale_for_dpi(v, dpi) as isize;
        unsafe {
            SendMessageW(hwnd, WM_SIZE, Some(WPARAM(SIZE_RESTORED as usize)), Some(LPARAM(px(600) << 16 | px(800))));
        }
        app.on_hidden(hwnd);
        let saved = Config::load(&path).unwrap().general;
        assert_eq!((saved.viewer_width, saved.viewer_height), (800, 600));

        std::fs::remove_file(&path).unwrap();
        app.on_hidden(hwnd);
        assert!(!path.exists(), "大きさが変わっていないのに書いた");

        // 書けなかった（置き場所が同じ名前のフォルダでふさがっている）ときは、次に隠すときに書き直す
        std::fs::create_dir(&path).unwrap();
        unsafe {
            SendMessageW(hwnd, WM_SIZE, Some(WPARAM(SIZE_RESTORED as usize)), Some(LPARAM(px(650) << 16 | px(900))));
        }
        app.on_hidden(hwnd);
        std::fs::remove_dir(&path).unwrap();
        app.on_hidden(hwnd);
        let saved = Config::load(&path).unwrap().general;
        assert_eq!((saved.viewer_width, saved.viewer_height), (900, 650), "書けなかった大きさを書き直していない");
        drop(window);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 履歴のクリア・クリップボードのクリアは操作スレッドへ依頼する（メインスレッドでは行わない）。
    #[test]
    fn clear_commands_are_requested_to_action_thread() {
        let (dir, service) = temp_service(Config::default());
        service.capture(text_entry("消さない")).unwrap();
        let app = test_app(&service);
        let (tx, rx) = std::sync::mpsc::channel();
        app.attach_actions(tx, crate::native::actions::FailureSink::new(|| {}));
        app.on_tool_command(HWND::default(), ToolCommand::ClearHistory);
        app.on_tool_command(HWND::default(), ToolCommand::ClearClipboard);
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), [Action::ClearHistory, Action::ClearClipboard]);
        assert_eq!(service.read(|s| s.history.len()), Some(1), "メインスレッドで消した");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 検索の候補は全文を写さず、在りか（ディスクの blob か、メモリだけに持つ resident か）だけを
    /// 渡す。結果の ID からは、候補の順で行を作る。
    #[test]
    fn search_candidates_carry_text_location_and_rows_follow_matches() {
        for memory_only in [false, true] {
            let config = if memory_only { memory_only_config() } else { Config::default() };
            let (dir, service) = temp_service(config);
            for text in ["one", "two", "three"] {
                service.capture(text_entry(text)).unwrap();
            }
            let (candidates, rows) = service.read(|s| search_candidates(s, Source::History)).unwrap();
            assert_eq!(candidates.len(), 3);
            for c in &candidates {
                if memory_only {
                    assert_eq!(c.text, TextSource::Resident);
                } else {
                    assert!(matches!(c.text, TextSource::Blob(_)), "{:?}", c.text);
                }
            }
            let ids: Vec<Uuid> = candidates.iter().map(|c| c.id).collect();
            let picked = rows_from_matches(rows, &[ids[2], ids[0]]);
            assert_eq!(picked.iter().map(|r| r.id).collect::<Vec<_>>(), vec![ids[0], ids[2]]);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// 検索はメインスレッドで行わない: 頼んだ直後は一覧がそのままで、検索スレッドの結果が届いて
    /// （ビューアが起こされて）から差し替わる。サービスが変わらない起床では作り直さない。
    #[test]
    fn search_result_replaces_rows_after_it_arrives_from_search_thread() {
        use std::rc::Rc;
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, PeekMessageW, ShowWindow, MSG, PM_REMOVE, SW_SHOWNOACTIVATE,
        };
        let _gui = crate::tray::lock_gui_resource_tests();
        let (dir, service) = temp_service(Config::default());
        for text in ["apple", "banana", "cherry"] {
            service.capture(text_entry(text)).unwrap();
        }
        let app = Rc::new(test_app(&service));
        let handler: Rc<dyn ViewerHandler> = app.clone();
        let window = viewer::ViewerWindow::create("CLCLR app test", (400, 300), handler).unwrap();
        let hwnd = window.hwnd();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (res_tx, res_rx) = std::sync::mpsc::channel();
        let waker = window.waker();
        let thread = search::spawn(service.clone(), cmd_rx, res_tx, move || waker.wake());
        app.attach_search(cmd_tx, res_rx);

        let pump_until = |done: &dyn Fn() -> bool| {
            let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !done() && std::time::Instant::now() < until {
                unsafe {
                    let mut msg = MSG::default();
                    while PeekMessageW(&mut msg, Some(hwnd), 0, 0, PM_REMOVE).as_bool() {
                        DispatchMessageW(&msg);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            done()
        };

        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        assert_eq!(viewer::row_ids(hwnd).len(), 3, "表示したときに一覧を作っていない");

        app.on_search_changed(hwnd, "Banana".to_string());
        assert_eq!(viewer::row_ids(hwnd).len(), 3, "検索をメインスレッドで済ませている");
        assert!(pump_until(&|| viewer::row_ids(hwnd).len() == 1), "検索の結果が一覧に出ない");

        // サービスが変わらない起床（トレイなど）では作り直さない
        let before = app.last_revision.get();
        app.on_wake(hwnd);
        assert_eq!(app.last_revision.get(), before);
        assert_eq!(viewer::row_ids(hwnd).len(), 1);

        // 取り込み（変更番号が進む）の後の起床では作り直し、検索もやり直す
        service.capture(text_entry("banana split")).unwrap();
        app.on_wake(hwnd);
        assert!(pump_until(&|| viewer::row_ids(hwnd).len() == 2), "取り込んだ項目が検索の結果に出ない");

        app.detach_search();
        thread.join().unwrap();
        drop(window);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// CF_HDROP（DROPFILES、wide）を作る。
    fn hdrop_entry(paths: &[String]) -> Entry {
        let mut data = Vec::new();
        data.extend(20u32.to_le_bytes()); // pFiles
        data.extend([0u8; 8]); // pt
        data.extend(0u32.to_le_bytes()); // fNC
        data.extend(1u32.to_le_bytes()); // fWide
        for p in paths {
            data.extend(p.encode_utf16().flat_map(|u| u.to_le_bytes()));
            data.extend([0, 0]);
        }
        data.extend([0, 0]);
        Entry::new(vec![Format { format_name: "CF_HDROP".to_string(), format_id: 15, data }])
    }

    fn text_preview_of(core: &Core) -> PreviewContent {
        let source = core
            .read(|service| {
                let id = service.history.front().unwrap().meta.id;
                preview_source(service, id)
            })
            .flatten()
            .unwrap();
        build_preview(source)
    }

    /// 先頭の 64KB を超えるファイル一覧は、途中で切れたパスを出さず、先頭の件数だけであることを
    /// 書き添える（ディスクに書く場合も、メモリだけに持つ場合も）。小さい一覧はそのまま全部出す。
    #[test]
    fn long_file_list_preview_drops_cut_path_and_says_it_is_truncated() {
        for memory_only in [false, true] {
            let config = if memory_only { memory_only_config() } else { Config::default() };
            let (dir, service) = temp_service(config);
            let paths: Vec<String> = (0..2000).map(|i| format!("C:\\Users\\test\\Documents\\file_{i:05}.txt")).collect();
            service.capture(hdrop_entry(&paths)).unwrap();
            let PreviewContent::Text(text) = text_preview_of(&service) else {
                panic!("テキストのプレビューになっていない");
            };
            let (list, note) = text.split_once("\r\n\r\n").expect("省略の注記がない");
            let shown: Vec<&str> = list.split("\r\n").collect();
            assert!(shown.len() < paths.len() && !shown.is_empty(), "件数 {}", shown.len());
            assert!(shown.iter().zip(&paths).all(|(s, p)| s == p), "途中で切れたパス、または順序の違うパスがある");
            assert_eq!(note, format!("（一覧が大きいため、先頭の {} 件だけを表示しています）", shown.len()));
            let _ = std::fs::remove_dir_all(dir);

            let (dir, service) = temp_service(if memory_only { memory_only_config() } else { Config::default() });
            service.capture(hdrop_entry(&paths[..3])).unwrap();
            let PreviewContent::Text(text) = text_preview_of(&service) else {
                panic!("テキストのプレビューになっていない");
            };
            assert_eq!(text, paths[..3].join("\r\n"));
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// メモリだけに持つ画像は写さず、履歴の resident を共有したまま読み込みスレッドへ渡す（大きさの
    /// 判定は読み込みスレッドが行う）。
    #[test]
    fn resident_image_preview_is_shared_without_copy() {
        let (dir, service) = temp_service(memory_only_config());
        let mut dib = Vec::new();
        dib.extend(40u32.to_le_bytes());
        dib.extend(64i32.to_le_bytes());
        dib.extend(64i32.to_le_bytes());
        dib.extend(1u16.to_le_bytes());
        dib.extend(32u16.to_le_bytes());
        dib.extend([0u8; 24]);
        dib.extend(std::iter::repeat_n(0x80u8, 64 * 64 * 4));
        service
            .capture(Entry::new(vec![Format { format_name: "CF_DIB".to_string(), format_id: 8, data: dib }]))
            .unwrap();
        let held = service.read(|s| Arc::clone(&s.history.front().unwrap().resident)).unwrap();
        match text_preview_of(&service) {
            PreviewContent::Image(ImageSource::Resident(shared)) => {
                assert!(Arc::ptr_eq(&shared, &held), "resident を写している");
            }
            _ => panic!("メモリだけの画像を画像として渡していない"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// ディスクに書かない（メモリだけに持つ）長いテキストでも、先頭だけを出していることを書き添える。
    #[test]
    fn long_resident_text_preview_says_it_is_truncated() {
        let (dir, service) = temp_service(memory_only_config());
        let units = model::PREVIEW_MAX_UNITS + 10;
        service.capture(text_entry(&"あ".repeat(units))).unwrap();
        let source = service
            .read(|service| {
                let id = service.history.front().unwrap().meta.id;
                preview_source(service, id)
            })
            .flatten()
            .unwrap();
        let PreviewContent::Text(preview) = build_preview(source) else {
            panic!("テキストのプレビューになっていない");
        };
        assert!(
            preview.ends_with(&format!("全体は約 {units} 文字）")),
            "省略の注記がない（末尾: {:?}）",
            preview.chars().rev().take(40).collect::<String>().chars().rev().collect::<String>()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// プレビューできる形式（テキスト・画像・ファイル一覧）が無い項目は、出すものが無いことにする
    /// （「（プレビューなし）」）。
    #[test]
    fn item_without_previewable_format_has_nothing_to_preview() {
        for memory_only in [false, true] {
            let mut config = if memory_only { memory_only_config() } else { Config::default() };
            config.format_filter_default = crate::config::FilterAction::Add;
            let (dir, service) = temp_service(config);
            service
                .capture(Entry::new(vec![Format { format_name: "CLCLR_Test".to_string(), format_id: 0xC123, data: vec![1, 2, 3] }]))
                .unwrap();
            assert!(matches!(text_preview_of(&service), PreviewContent::Nothing), "memory_only={memory_only}");
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    // --- 一覧の操作 ---

    /// 2x2 の 32bpp DIB の項目。
    fn dib_entry() -> Entry {
        let mut v = Vec::new();
        v.extend(40u32.to_le_bytes());
        v.extend(2i32.to_le_bytes());
        v.extend(2i32.to_le_bytes());
        v.extend(1u16.to_le_bytes());
        v.extend(32u16.to_le_bytes());
        v.extend(0u32.to_le_bytes());
        v.extend(16u32.to_le_bytes());
        v.extend([0u8; 16]);
        v.extend([0x20u8; 16]);
        Entry::new(vec![Format { format_name: "CF_DIB".to_string(), format_id: 8, data: v }])
    }

    /// Enter・ダブルクリック・Delete・右クリックメニューの操作は、行（`RowTarget`）の保管先に
    /// 合わせた依頼になる。今の表示元（`App::source`）には左右されない（検索の結果を待つ間に
    /// 表示元を切り替えても、画面に残っている行のとおりに操作する）。
    /// ピン留めの行の「ピン留めに追加」、履歴の行の「移動」は依頼しない。
    #[test]
    fn row_commands_follow_the_row_not_the_current_source() {
        use crate::tools::text::TextTransform;
        let (dir, core) = temp_service(memory_only_config());
        core.capture(text_entry("行")).unwrap();
        let id = crate::ops::tests::front_id(&core);
        let app = test_app(&core);
        let (requests, received) = mpsc::channel();
        app.attach_actions(requests, FailureSink::new(|| {}));
        let hwnd = HWND::default();
        let history_row = RowTarget { id, pinned: false, folder: false };
        let pinned_row = RowTarget { id, pinned: true, folder: false };
        // 表示元はピン留めに切り替わっているが、画面には履歴の行が残っている
        app.source.set(Source::Pinned(None));
        app.on_activate(hwnd, history_row);
        app.on_delete(hwnd, history_row);
        let folder = Uuid::new_v4();
        app.on_row_command(hwnd, history_row, RowCommand::Pin(None));
        app.on_row_command(hwnd, history_row, RowCommand::Pin(Some(folder)));
        app.on_row_command(hwnd, history_row, RowCommand::Move(Some(folder)));
        app.on_row_command(hwnd, history_row, RowCommand::OpenImage);
        app.on_row_command(hwnd, history_row, RowCommand::Transform(TextTransform::ToUpper));
        app.source.set(Source::History);
        app.on_row_command(hwnd, pinned_row, RowCommand::Pin(None));
        app.on_row_command(hwnd, pinned_row, RowCommand::Move(Some(folder)));
        app.on_row_command(hwnd, pinned_row, RowCommand::Move(None));
        app.on_row_command(hwnd, pinned_row, RowCommand::OpenImageLocation);
        // 名前の変更はビューアが名前を聞いてから `on_rename_pinned` で来る
        app.on_row_command(hwnd, pinned_row, RowCommand::Rename);
        app.on_rename_pinned(hwnd, id, "名前".into());
        app.on_activate(hwnd, pinned_row);
        let got: Vec<Action> = received.try_iter().collect();
        assert_eq!(
            got,
            vec![
                Action::Send { id, pinned: false },
                Action::Delete { id, pinned: false },
                Action::Pin { id, to: None },
                Action::Pin { id, to: Some(folder) },
                Action::OpenImage { id, pinned: false },
                Action::Transform { id, pinned: false, transform: TextTransform::ToUpper },
                Action::MovePinned { id, to: Some(folder), before: None },
                Action::MovePinned { id, to: None, before: None },
                Action::OpenImageLocation { id, pinned: true },
                Action::RenamePinned { id, title: "名前".into() },
                Action::Send { id, pinned: true },
            ]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// ツリーの右クリックメニューは「履歴」・ピン留めの根・ピン留めのフォルダにだけ出す（階層表示のフォルダ・
    /// 無いフォルダには出さない）。フォルダの削除はピン留めの削除として、作成・名前の変更はそのまま操作
    /// スレッドへ依頼する。フォルダは、コアを開く前に pinned.toml へ書いて作る。
    #[test]
    fn tree_menu_is_offered_for_history_and_pinned_nodes() {
        use crate::store::{PinnedFolder, PinnedNode};
        let dir = std::env::temp_dir().join(format!("clclr-test-{}", Uuid::new_v4()));
        let folder = Uuid::new_v4();
        crate::storage::Storage::open(dir.clone())
            .unwrap()
            .save_pinned(&[PinnedNode::Folder(PinnedFolder { id: folder, title: "仕事".into(), children: vec![] })])
            .unwrap();
        let core = Core::open_at(dir.clone(), Arc::new(RwLock::new(Config::default()))).unwrap();
        let app = test_app(&core);
        assert_eq!(app.tree_menu(Source::History), Some(TreeMenu::History));
        assert_eq!(app.tree_menu(Source::HistoryGroup(0)), None);
        assert_eq!(app.tree_menu(Source::Pinned(None)), Some(TreeMenu::PinnedRoot));
        assert_eq!(
            app.tree_menu(Source::Pinned(Some(folder))),
            Some(TreeMenu::PinnedFolder { id: folder, title: "仕事".into(), items: 0, folders: 0 })
        );
        assert_eq!(app.tree_menu(Source::Pinned(Some(Uuid::new_v4()))), None);

        let (requests, received) = mpsc::channel();
        app.attach_actions(requests, FailureSink::new(|| {}));
        app.on_tree_command(HWND::default(), TreeCommand::DeleteFolder(folder));
        app.on_tree_command(HWND::default(), TreeCommand::CreateFolder { parent: Some(folder), title: "中".into() });
        app.on_tree_command(HWND::default(), TreeCommand::RenameFolder { id: folder, title: "遊び".into() });
        assert_eq!(
            received.try_iter().collect::<Vec<_>>(),
            [
                Action::Delete { id: folder, pinned: true },
                Action::CreateFolder { parent: Some(folder), title: "中".into() },
                Action::RenameFolder { id: folder, title: "遊び".into() },
            ]
        );
        drop(app);
        drop(core);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 右クリックメニューの項目は、表示元の項目の形式（ディスクの形式とメモリだけの形式の両方）
    /// から決まる。ピン留めを表示しているときは「ピン留めに追加」を出さず、履歴からは探さない。
    #[test]
    fn row_menu_reflects_formats_and_source() {
        let (dir, core) = temp_service(Config::default());
        core.capture(text_entry("文字")).unwrap();
        let text_id = crate::ops::tests::front_id(&core);
        core.capture(dib_entry()).unwrap();
        let image_id = crate::ops::tests::front_id(&core);
        let app = test_app(&core);
        let history = |id| RowTarget { id, pinned: false, folder: false };
        let pinned = |id| RowTarget { id, pinned: true, folder: false };
        let root_only = model::pin_targets(&[]);
        let expect = |can_pin, has_image, has_text, current, reorder| RowMenu {
            can_pin,
            has_image,
            has_text,
            pin_targets: root_only.clone(),
            current,
            reorder,
            folder: false,
        };
        assert_eq!(app.row_menu(history(text_id)), Some(expect(true, false, true, None, None)));
        assert_eq!(app.row_menu(history(image_id)), Some(expect(true, true, false, None, None)));
        core.pin(text_id, None).unwrap();
        let pinned_id = crate::ops::tests::pinned_item(&core, 0).id;
        let alone = Some(Reorder { up: false, down: false });
        assert_eq!(app.row_menu(pinned(pinned_id)), Some(expect(false, false, true, Some(None), alone)));
        assert_eq!(app.row_menu(pinned(text_id)), None, "ピン留めの行を履歴から探した");
        let _ = std::fs::remove_dir_all(dir);

        // 完全メモリモード: 形式はメモリだけにある
        let (dir, core) = temp_service(memory_only_config());
        core.capture(text_entry("メモリだけ")).unwrap();
        let id = crate::ops::tests::front_id(&core);
        let app = test_app(&core);
        assert_eq!(app.row_menu(history(id)), Some(expect(true, false, true, None, None)));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// ピン留めの並べ替え: メニューの「上へ」「下へ」は同じ親の中の位置で決まり、項目とフォルダを区別しない。
    /// フォルダの行のメニューは、フォルダがあるときだけ出す。検索中は並べ替えを出さず、項目・フォルダの行の並べ替え・
    /// ドラッグでの移動を依頼せず、ピン留めの行のドラッグもさせない（履歴の行のドラッグはピン留めなので、検索中も
    /// させる）。検索中も、右クリックの「移動」と履歴の行のピン留めは依頼する。
    /// フォルダの行は移動・並べ替えだけを依頼し、送る・削除（確認の前）などは依頼しない。
    /// 履歴の行は、移動・並べ替えを依頼しない。
    #[test]
    fn reorder_menu_and_commands_follow_position_and_search() {
        use crate::store::Direction::{Down, Up};
        let (dir, core) = temp_service(Config::default());
        core.capture(text_entry("項目")).unwrap();
        core.pin(crate::ops::tests::front_id(&core), None).unwrap();
        let item = crate::ops::tests::pinned_item(&core, 0).id;
        core.create_folder(None, "箱").unwrap();
        let folder = crate::ops::tests::folder_id(&core, "箱");
        let app = test_app(&core);
        let item_row = RowTarget { id: item, pinned: true, folder: false };
        let folder_row = RowTarget { id: folder, pinned: true, folder: true };

        let menu = app.row_menu(item_row).unwrap();
        assert_eq!(menu.reorder, Some(Reorder { up: false, down: true }));
        assert!(!menu.folder);
        assert_eq!(
            app.row_menu(folder_row),
            Some(RowMenu {
                folder: true,
                reorder: Some(Reorder { up: true, down: false }),
                pin_targets: model::pin_targets(&[]),
                current: Some(None),
                ..RowMenu::default()
            })
        );
        assert_eq!(app.row_menu(RowTarget { id: Uuid::new_v4(), ..folder_row }), None, "無いフォルダのメニューを出した");

        let (requests, received) = mpsc::channel();
        app.attach_actions(requests, FailureSink::new(|| {}));
        let hwnd = HWND::default();
        let history_row = RowTarget { id: item, pinned: false, folder: false };
        let place = RowCommand::Place { to: Some(folder), before: None };
        assert!(app.can_drag_row(item_row) && app.can_drag_row(folder_row) && app.can_drag_row(history_row));
        app.on_row_command(hwnd, item_row, RowCommand::Reorder(Down));
        app.on_row_command(hwnd, folder_row, RowCommand::Reorder(Up));
        app.on_row_command(hwnd, item_row, place);
        app.on_row_command(hwnd, folder_row, RowCommand::Place { to: None, before: Some(item) });
        for command in [RowCommand::Reorder(Up), place, RowCommand::Move(None)] {
            app.on_row_command(hwnd, history_row, command);
        }
        for command in [RowCommand::Send, RowCommand::Delete, RowCommand::Rename, RowCommand::Move(None)] {
            app.on_row_command(hwnd, folder_row, command);
        }
        app.on_activate(hwnd, folder_row);
        app.on_delete(hwnd, folder_row);
        *app.needle.borrow_mut() = "項".into();
        app.on_row_command(hwnd, item_row, RowCommand::Reorder(Down));
        app.on_row_command(hwnd, item_row, place);
        app.on_row_command(hwnd, folder_row, RowCommand::Reorder(Up));
        app.on_row_command(hwnd, folder_row, RowCommand::Place { to: None, before: Some(item) });
        // 検索中も、右クリックの「移動」（末尾へ）と履歴の行のピン留めはできる
        app.on_row_command(hwnd, item_row, RowCommand::Move(Some(folder)));
        app.on_row_command(hwnd, folder_row, RowCommand::Move(None));
        app.on_row_command(hwnd, history_row, RowCommand::Pin(Some(folder)));
        assert_eq!(app.row_menu(item_row).unwrap().reorder, None, "検索中に並べ替えを出した");
        assert!(!app.can_drag_row(item_row) && !app.can_drag_row(folder_row), "検索中にピン留めの行をドラッグできる");
        assert!(app.can_drag_row(history_row), "検索中に履歴の行をドラッグできない（ピン留めはできる）");
        assert_eq!(app.row_menu(folder_row).unwrap().reorder, None);
        assert_eq!(
            received.try_iter().collect::<Vec<_>>(),
            [
                Action::ReorderPinned { id: item, direction: Down },
                Action::ReorderPinned { id: folder, direction: Up },
                Action::MovePinned { id: item, to: Some(folder), before: None },
                Action::MovePinned { id: folder, to: None, before: Some(item) },
                Action::MovePinned { id: folder, to: None, before: None },
                Action::MovePinned { id: item, to: Some(folder), before: None },
                Action::MovePinned { id: folder, to: None, before: None },
                Action::Pin { id: item, to: Some(folder) },
            ]
        );
        drop(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 行のメニューの入れる先はフォルダを深さ優先で並べ、ピン留めの行には今いるフォルダを添える。
    /// フォルダはコアの操作で作る。
    #[test]
    fn row_menu_lists_pin_targets_and_current_folder() {
        let (dir, core) = temp_service(Config::default());
        core.capture(text_entry("文字")).unwrap();
        let text_id = crate::ops::tests::front_id(&core);
        core.create_folder(None, "仕事").unwrap();
        let work = crate::ops::tests::folder_id(&core, "仕事");
        core.create_folder(Some(work), "中").unwrap();
        let nested = crate::ops::tests::folder_id(&core, "中");
        core.pin(text_id, Some(nested)).unwrap();
        let pinned_id = core.read(|s| store::find_folder(&s.pinned, nested).unwrap().children[0].id()).unwrap();
        let app = test_app(&core);

        let menu = app.row_menu(RowTarget { id: pinned_id, pinned: true, folder: false }).unwrap();
        let listed: Vec<(Option<Uuid>, &str, usize)> =
            menu.pin_targets.iter().map(|t| (t.folder, t.title.as_str(), t.depth)).collect();
        assert_eq!(listed, [(None, model::PIN_ROOT_LABEL, 0), (Some(work), "仕事", 1), (Some(nested), "中", 2)]);
        assert_eq!(menu.current, Some(Some(nested)));
        let menu = app.row_menu(RowTarget { id: text_id, pinned: false, folder: false }).unwrap();
        assert_eq!(menu.pin_targets.len(), 3);
        assert_eq!(menu.current, None);
        // フォルダの行: 入れる先から自分とその中を除き、今いる所を添える
        let menu = app.row_menu(RowTarget { id: work, pinned: true, folder: true }).unwrap();
        assert_eq!(menu.pin_targets.iter().map(|t| t.folder).collect::<Vec<_>>(), [None]);
        assert_eq!(menu.current, Some(None));
        let menu = app.row_menu(RowTarget { id: nested, pinned: true, folder: true }).unwrap();
        assert_eq!(menu.pin_targets.iter().map(|t| t.folder).collect::<Vec<_>>(), [None, Some(work)]);
        assert_eq!(menu.current, Some(Some(work)));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 行は作ったときの保管先を持つ: ピン留めの表示・ピン留めの検索の候補から作った行は pinned、
    /// 履歴から作った行は pinned でない。
    #[test]
    fn rows_carry_where_they_came_from() {
        let (dir, core) = temp_service(Config::default());
        core.capture(text_entry("ピン留めへ")).unwrap();
        core.pin(crate::ops::tests::front_id(&core), None).unwrap();
        let grouping = crate::config::HistoryGroupingConfig::default();
        let (history_rows, pinned_rows) = core
            .read(|s| (model::rows_for(s, Source::History, &grouping), model::rows_for(s, Source::Pinned(None), &grouping)))
            .unwrap();
        assert!(history_rows.iter().all(|r| !r.pinned) && !history_rows.is_empty());
        assert!(pinned_rows.iter().all(|r| r.pinned) && !pinned_rows.is_empty());
        for source in [Source::History, Source::Pinned(None)] {
            let (candidates, rows) = core.read(|s| search_candidates(s, source)).unwrap();
            let matched: Vec<Uuid> = candidates.iter().map(|c| c.id).collect();
            let rows = rows_from_matches(rows, &matched);
            assert!(!rows.is_empty());
            assert!(rows.iter().all(|r| r.pinned == matches!(source, Source::Pinned(_))), "{source:?}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// ピン留めの一覧は、項目とフォルダを保存した順に混ぜて並べる（ポップアップメニューと同じ順）。フォルダの
    /// 行の操作の対象はそのフォルダで、2行目は直下の数。検索の候補にはフォルダを入れない（項目だけ）。
    #[test]
    fn pinned_rows_interleave_folders_in_saved_order() {
        use crate::ops::tests::{folder_id, front_id};
        let (dir, core) = temp_service(Config::default());
        core.capture(text_entry("前")).unwrap();
        core.pin(front_id(&core), None).unwrap();
        core.create_folder(None, "箱").unwrap();
        let folder = folder_id(&core, "箱");
        core.capture(text_entry("中")).unwrap();
        core.pin(front_id(&core), Some(folder)).unwrap();
        core.create_folder(Some(folder), "小").unwrap();
        core.capture(text_entry("後")).unwrap();
        core.pin(front_id(&core), None).unwrap();
        let grouping = crate::config::HistoryGroupingConfig::default();
        let rows = core.read(|s| model::rows_for(s, Source::Pinned(None), &grouping)).unwrap();
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, ["前", "箱", "後"]);
        assert_eq!(rows[1].target(), RowTarget { id: folder, pinned: true, folder: true });
        assert_eq!(rows[1].detail(0.0), "フォルダ — 項目 1 件、フォルダ 1 個");
        assert!(rows[0].folder.is_none() && rows[2].folder.is_none());
        let inside = core.read(|s| model::rows_for(s, Source::Pinned(Some(folder)), &grouping)).unwrap();
        assert_eq!(inside.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(), ["中", "小"]);

        let (candidates, _rows) = core.read(|s| search_candidates(s, Source::Pinned(None))).unwrap();
        let titles: Vec<Option<&str>> = candidates.iter().map(|c| c.title.as_deref()).collect();
        assert_eq!(titles, [Some("前"), Some("中"), Some("後")]);
        let _ = std::fs::remove_dir_all(dir);
    }

    // --- 操作の失敗の通知 ---

    use crate::native::actions::ActionKind;
    use std::sync::mpsc;

    fn failure(message: &str) -> ActionFailure {
        ActionFailure { kind: ActionKind::Send, message: message.to_string() }
    }

    /// 通知の試験台: 通知先につないだ App。表示は記録し、最初の表示の間に `during` を1回だけ
    /// 呼ぶ（メッセージボックスのモーダルループの中での再入の代わり）。
    struct NotifyHarness {
        dir: std::path::PathBuf,
        app: Rc<App>,
        tray: mpsc::Sender<TrayEvent>,
        hotkey: mpsc::Sender<HotkeyEvent>,
        sink: FailureSink,
        shown: Rc<RefCell<Vec<String>>>,
        during: Rc<RefCell<Option<Box<dyn FnOnce(&App)>>>>,
    }

    impl NotifyHarness {
        fn new(notify: bool) -> Self {
            let mut config = memory_only_config();
            config.general.notify_action_errors = notify;
            let (dir, core) = temp_service(config.clone());
            let (tray, tray_rx) = mpsc::channel();
            let (hotkey, hotkey_rx) = mpsc::channel();
            let app = Rc::new(App::new(Arc::new(RwLock::new(config)), core, tray_rx, hotkey_rx));
            let sink = FailureSink::new(|| {});
            let (requests, _) = mpsc::channel();
            app.attach_actions(requests, sink.clone());
            let shown: Rc<RefCell<Vec<String>>> = Rc::default();
            let during: Rc<RefCell<Option<Box<dyn FnOnce(&App)>>>> = Rc::default();
            {
                let (shown, during, weak) = (Rc::clone(&shown), Rc::clone(&during), Rc::downgrade(&app));
                app.set_failure_display(move |_hwnd, text| {
                    shown.borrow_mut().push(text.to_string());
                    let f = during.borrow_mut().take();
                    if let (Some(f), Some(app)) = (f, weak.upgrade()) {
                        f(&app);
                    }
                });
            }
            Self { dir, app, tray, hotkey, sink, shown, during }
        }

        fn during_first_display(&self, f: impl FnOnce(&App) + 'static) {
            *self.during.borrow_mut() = Some(Box::new(f));
        }

        fn wake(&self) {
            self.app.on_wake(HWND::default());
        }

        fn shown(&self) -> Vec<String> {
            self.shown.borrow().clone()
        }
    }

    impl Drop for NotifyHarness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// データのチェックの結果: 失敗とチェック・削除の結果は届いた順に出す。チェック・削除の失敗と結果は、
    /// 操作の失敗を知らせない設定でも出す。チェックの結果で「削除」を選んだときだけ削除を依頼し、選んで閉じる間に
    /// 終了の要求が来ていたら依頼しない。
    #[test]
    fn check_results_are_shown_in_order_and_delete_is_requested_only_when_chosen() {
        use crate::datacheck::FileEntry;
        let mut config = memory_only_config();
        config.general.notify_action_errors = false;
        let (dir, core) = temp_service(config.clone());
        let (_tray, tray_rx) = mpsc::channel();
        let (_hotkey, hotkey_rx) = mpsc::channel();
        let app = Rc::new(App::new(Arc::new(RwLock::new(config)), core, tray_rx, hotkey_rx));
        let sink = FailureSink::new(|| {});
        let (requests, received) = mpsc::channel();
        app.attach_actions(requests, sink.clone());
        let log: Rc<RefCell<Vec<String>>> = Rc::default();
        // 1つ目のチェックは「キャンセル」、2つ目は「削除」、3つ目は「削除」を選ぶ間に終了の要求が来る
        let answers = Rc::new(RefCell::new(VecDeque::from([false, true, true])));
        {
            let (log_f, log_c, log_r, weak) = (Rc::clone(&log), Rc::clone(&log), Rc::clone(&log), Rc::downgrade(&app));
            app.set_failure_display(move |_hwnd, text| log_f.borrow_mut().push(format!("F:{text}")));
            app.set_check_display(
                move |_hwnd, report| {
                    log_c.borrow_mut().push(format!("C:{}", report.orphans.len()));
                    let answer = answers.borrow_mut().pop_front().unwrap();
                    if answers.borrow().is_empty() {
                        // parts が無い = トレイなし。閉じる操作は終了の要求になる
                        weak.upgrade().unwrap().on_close(HWND::default());
                    }
                    answer
                },
                move |_hwnd, result| log_r.borrow_mut().push(format!("R:{}", result.files_removed)),
            );
        }
        let report = |n: usize| DataReport {
            orphans: (0..n).map(|i| FileEntry { name: format!("{i}"), size: 1 }).collect(),
            ..DataReport::default()
        };
        sink.report(ActionFailure { kind: ActionKind::CheckData, message: "a".into() });
        sink.report(ActionFailure { kind: ActionKind::Send, message: "設定で知らせない".into() });
        sink.post(Notice::CheckReport(report(1)));
        sink.report(ActionFailure { kind: ActionKind::CleanData, message: "c".into() });
        sink.post(Notice::CheckReport(report(2)));
        sink.post(Notice::CleanResult(CleanResult { files_removed: 2, ..CleanResult::default() }));
        sink.post(Notice::CheckReport(report(3)));
        app.on_wake(HWND::default());
        assert_eq!(
            log.borrow().as_slice(),
            [
                "F:データをチェックできませんでした: a",
                "C:1",
                "F:データを削除できませんでした: c",
                "C:2",
                "R:2",
                "C:3",
            ]
        );
        let got: Vec<Action> = received.try_iter().collect();
        assert_eq!(got, [Action::CleanData(report(2))]);
        assert!(app.exiting.get());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 保存先のフォルダの権限の警告: 非表示で起動していれば、ビューアを出してから出す。出せなかった（ほかの表示中）
    /// ときは残して次の起床で出す。「今後は確かめない」は設定ファイルに書けてから共有の設定に反映する。終了の要求の
    /// 後に届いたものは出さず、ビューアも出さない。
    #[test]
    fn folder_report_reveals_hidden_viewer_and_saves_stop_checking() {
        use crate::folder_security::Unverified;
        let config = memory_only_config();
        assert!(config.general.check_folder_permissions, "前提: 既定は確かめる");
        let (dir, core) = temp_service(config.clone());
        let (_tray, tray_rx) = mpsc::channel();
        let (_hotkey, hotkey_rx) = mpsc::channel();
        let mut app = App::new(Arc::new(RwLock::new(config)), core, tray_rx, hotkey_rx);
        app.set_config_path(dir.join("config.toml"));
        let app = Rc::new(app);
        let sink = FailureSink::new(|| {});
        let (requests, _received) = mpsc::channel();
        app.attach_actions(requests, sink.clone());
        let log: Rc<RefCell<Vec<String>>> = Rc::default();
        // 1回目は出せない（ほかの表示中）、2回目は「今後は確かめない」
        let answers = Rc::new(RefCell::new(VecDeque::from([viewer::FolderChoice::NotShown, viewer::FolderChoice::StopChecking])));
        {
            let (log_f, log_s, log_r, weak) = (Rc::clone(&log), Rc::clone(&log), Rc::clone(&log), Rc::downgrade(&app));
            app.set_failure_display(move |_hwnd, text| log_f.borrow_mut().push(format!("F:{text}")));
            app.set_folder_display(
                move |_hwnd, report| {
                    log_s.borrow_mut().push(format!("S:{}", report.dir.display()));
                    answers.borrow_mut().pop_front().unwrap()
                },
                move |hwnd| {
                    log_r.borrow_mut().push("reveal".to_string());
                    // `viewer::show` は窓を表示し、`WM_SHOWWINDOW` で `on_shown` が呼ばれる
                    weak.upgrade().unwrap().on_shown(hwnd);
                },
            );
        }
        let report = FolderReport { dir: PathBuf::from("X"), unverified: vec![Unverified::Remote], ..FolderReport::default() };
        app.mark_viewer_hidden();
        sink.post(Notice::FolderReport(report.clone()));
        app.on_wake(HWND::default());
        assert_eq!(log.borrow().as_slice(), ["reveal", "S:X"], "隠したまま出した・出さなかった");
        assert!(app.config.read().unwrap().general.check_folder_permissions);
        // 出せなかった分を次の起床で出す（もう表示しているので出し直さない）
        app.on_wake(HWND::default());
        assert_eq!(log.borrow().as_slice(), ["reveal", "S:X", "S:X"]);
        assert!(!app.config.read().unwrap().general.check_folder_permissions, "共有の設定に反映していない");
        assert!(!Config::load(&dir.join("config.toml")).unwrap().general.check_folder_permissions, "設定ファイルに書いていない");

        // 終了の要求の後（トレイなしで閉じる）は、届いても出さず、ビューアも出さない
        app.mark_viewer_hidden();
        app.on_close(HWND::default());
        sink.post(Notice::FolderReport(report));
        app.on_wake(HWND::default());
        assert_eq!(log.borrow().len(), 3, "終了の要求の後に出した");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 警告のダイアログを作れなかったときは、本文をメッセージボックスで知らせる（警告を見ないまま捨てない）。
    #[test]
    fn folder_report_falls_back_to_message_box_when_dialog_fails() {
        use crate::folder_security::Unverified;
        let config = memory_only_config();
        let (dir, core) = temp_service(config.clone());
        let (_tray, tray_rx) = mpsc::channel();
        let (_hotkey, hotkey_rx) = mpsc::channel();
        let app = Rc::new(App::new(Arc::new(RwLock::new(config)), core, tray_rx, hotkey_rx));
        let sink = FailureSink::new(|| {});
        let (requests, _received) = mpsc::channel();
        app.attach_actions(requests, sink.clone());
        let log: Rc<RefCell<Vec<String>>> = Rc::default();
        {
            let log_f = Rc::clone(&log);
            app.set_failure_display(move |_hwnd, text| log_f.borrow_mut().push(text.to_string()));
            app.set_folder_display(|_hwnd, _report| viewer::FolderChoice::Failed, |_hwnd| {});
        }
        sink.post(Notice::FolderReport(FolderReport {
            dir: PathBuf::from(r"C:\Tools\CLCLR"),
            unverified: vec![Unverified::NoAcl],
            ..FolderReport::default()
        }));
        app.on_wake(HWND::default());
        app.on_wake(HWND::default());
        let log = log.borrow();
        assert_eq!(log.len(), 1, "{log:?}");
        assert!(log[0].starts_with("保存先のフォルダの権限を見直してください") && log[0].contains(r"C:\Tools\CLCLR"), "{log:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 「今後は確かめない」を設定ファイルに書けなければ、共有の設定を変えずに知らせる。
    #[test]
    fn folder_report_stop_checking_reports_save_failure() {
        use crate::folder_security::Unverified;
        let config = memory_only_config();
        let (dir, core) = temp_service(config.clone());
        let (_tray, tray_rx) = mpsc::channel();
        let (_hotkey, hotkey_rx) = mpsc::channel();
        let mut app = App::new(Arc::new(RwLock::new(config)), core, tray_rx, hotkey_rx);
        // 設定ファイルの場所にフォルダを置いて、書けないようにする
        std::fs::create_dir_all(dir.join("config.toml")).unwrap();
        app.set_config_path(dir.join("config.toml"));
        let app = Rc::new(app);
        let sink = FailureSink::new(|| {});
        let (requests, _received) = mpsc::channel();
        app.attach_actions(requests, sink.clone());
        let log: Rc<RefCell<Vec<String>>> = Rc::default();
        {
            let (log_f, log_s) = (Rc::clone(&log), Rc::clone(&log));
            app.set_failure_display(move |_hwnd, text| log_f.borrow_mut().push(format!("F:{text}")));
            app.set_folder_display(
                move |_hwnd, _report| {
                    log_s.borrow_mut().push("S".to_string());
                    viewer::FolderChoice::StopChecking
                },
                |_hwnd| {},
            );
        }
        sink.post(Notice::FolderReport(FolderReport { unverified: vec![Unverified::NoAcl], ..FolderReport::default() }));
        app.on_wake(HWND::default());
        let log = log.borrow();
        assert_eq!(log.len(), 2, "{log:?}");
        assert!(log[1].starts_with("F:設定の一部を反映できませんでした: 「今後は確かめない」を設定ファイルに保存できませんでした"), "{log:?}");
        assert!(app.config.read().unwrap().general.check_folder_permissions, "書けないのに共有の設定を変えた");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// ビューアを隠している間は知らせを出さずに残し、表示したときに出す。結果の表示中に隠すと、その結果は閉じ、
    /// 残りの結果・失敗は隠れた窓の上に続けて出ない。
    #[test]
    fn notices_are_held_while_viewer_is_hidden() {
        use crate::datacheck::FileEntry;
        let config = memory_only_config();
        let (dir, core) = temp_service(config.clone());
        let (_tray, tray_rx) = mpsc::channel();
        let (_hotkey, hotkey_rx) = mpsc::channel();
        let app = Rc::new(App::new(Arc::new(RwLock::new(config)), core, tray_rx, hotkey_rx));
        let sink = FailureSink::new(|| {});
        let (requests, _received) = mpsc::channel();
        app.attach_actions(requests, sink.clone());
        let log: Rc<RefCell<Vec<String>>> = Rc::default();
        {
            let (log_f, log_c, weak) = (Rc::clone(&log), Rc::clone(&log), Rc::downgrade(&app));
            app.set_failure_display(move |_hwnd, text| log_f.borrow_mut().push(format!("F:{text}")));
            app.set_check_display(
                move |_hwnd, report| {
                    log_c.borrow_mut().push(format!("C:{}", report.orphans.len()));
                    // 1つ目の表示中に隠す（`viewer::hide` は表示中のダイアログを閉じてから窓を隠す）
                    if report.orphans.len() == 1 {
                        weak.upgrade().unwrap().on_hidden(HWND::default());
                    }
                    false
                },
                |_hwnd, _result| {},
            );
        }
        let report = |n: usize| DataReport {
            orphans: (0..n).map(|i| FileEntry { name: format!("{i}"), size: 1 }).collect(),
            ..DataReport::default()
        };
        // 表示せずに起動した（`mark_viewer_hidden`）ときも、表示するまで残す
        app.mark_viewer_hidden();
        sink.post(Notice::CheckReport(report(1)));
        app.on_wake(HWND::default());
        assert!(log.borrow().is_empty(), "表示せずに起動したのに出した");
        app.on_shown(HWND::default());
        sink.post(Notice::CheckReport(report(2)));
        sink.report(ActionFailure { kind: ActionKind::CheckData, message: "a".into() });
        app.on_wake(HWND::default());
        assert_eq!(log.borrow().as_slice(), ["C:1"], "隠した後に残りを出した");
        app.on_wake(HWND::default());
        assert_eq!(log.borrow().len(), 1, "隠している間に出した");
        app.on_shown(HWND::default());
        app.on_wake(HWND::default());
        assert_eq!(log.borrow().as_slice(), ["C:1", "C:2", "F:データをチェックできませんでした: a"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 表示中に届いた失敗は、外からの起床を待たずに、閉じた後に続けて出る。
    #[test]
    fn failure_arriving_during_display_is_shown_next_without_new_wake() {
        let h = NotifyHarness::new(true);
        h.sink.report(failure("A"));
        let sink = h.sink.clone();
        h.during_first_display(move |app| {
            sink.report(failure("B"));
            // 表示中の起床（再入）は積むだけ
            app.on_wake(HWND::default());
        });
        h.wake();
        let shown = h.shown();
        assert_eq!(shown.len(), 2, "{shown:?}");
        assert!(shown[0].ends_with("A") && shown[1].ends_with("B"), "{shown:?}");
        assert!(!h.app.notifier.borrow().notifying);
    }

    /// 終了の要求の後に届いたトレイのイベント（監視の切り替え）は捨てる。同じ起床の中で「終了」の後に並んでいた
    /// ものも捨てる。
    #[test]
    fn tray_events_after_exit_request_are_ignored() {
        let _clipboard = crate::clipboard::test_support::lock_clipboard_tests();
        let _gui = crate::tray::lock_gui_resource_tests();
        let h = NotifyHarness::new(true);
        h.app.set_parts(Parts { watcher: crate::clipboard::test_support::test_watcher(), tray: None, hotkeys: None });
        let listening = || h.app.parts.borrow().as_ref().unwrap().watcher.listening();
        assert!(!listening(), "前提: 監視は切ってある");
        h.tray.send(TrayEvent::Exit).unwrap();
        h.tray.send(TrayEvent::ToggleWatch).unwrap();
        h.wake();
        assert!(h.app.exiting.get());
        assert!(!listening(), "終了の要求の後の監視の切り替えを行った");
        h.tray.send(TrayEvent::ToggleWatch).unwrap();
        h.wake();
        assert!(!listening(), "終了の要求の後の監視の切り替えを行った");
        drop(h.app.take_parts());
    }

    #[test]
    fn tray_exit_during_display_stops_further_notifications() {
        let h = NotifyHarness::new(true);
        h.sink.report(failure("A"));
        let (sink, tray) = (h.sink.clone(), h.tray.clone());
        h.during_first_display(move |app| {
            sink.report(failure("B"));
            tray.send(TrayEvent::Exit).unwrap();
            app.on_wake(HWND::default());
        });
        h.wake();
        assert_eq!(h.shown().len(), 1);
        assert!(h.app.exiting.get());
        // 通知先は閉じている（この後の失敗はログだけで、積まれない）
        h.sink.report(failure("C"));
        assert!(h.sink.drain().is_empty());
        h.wake();
        assert_eq!(h.shown().len(), 1);
    }

    #[test]
    fn close_without_tray_during_display_stops_further_notifications() {
        let h = NotifyHarness::new(true);
        h.sink.report(failure("A"));
        let sink = h.sink.clone();
        h.during_first_display(move |app| {
            sink.report(failure("B"));
            // parts が無い = トレイなし。閉じる操作は終了になる
            app.on_close(HWND::default());
            app.on_wake(HWND::default());
        });
        h.wake();
        assert_eq!(h.shown().len(), 1);
        assert!(h.app.exiting.get());
    }

    /// 閉じる操作は、トレイのアイコンが通知領域にあれば隠すだけ、トレイはあってもアイコンを付けられていなければ
    /// 終了になる（隠すとトレイから戻れないため）。
    /// アイコンを付けられるかはタスクバーのある環境が前提。
    #[test]
    fn close_hides_only_when_tray_icon_is_shown() {
        let _clipboard = crate::clipboard::test_support::lock_clipboard_tests();
        let _gui = crate::tray::lock_gui_resource_tests();
        let h = NotifyHarness::new(true);
        let tray = Tray::spawn(Arc::new(std::sync::atomic::AtomicBool::new(true)), h.tray.clone(), || {}).unwrap();
        assert!(tray.icon_shown(), "前提: アイコンを付けられなかった（タスクバーのない環境では検証できない）");
        h.app.set_parts(Parts {
            watcher: crate::clipboard::test_support::test_watcher(),
            tray: Some(tray),
            hotkeys: None,
        });
        h.app.on_close(HWND::default());
        assert!(!h.app.exiting.get(), "アイコンがあるのに閉じる操作で終了した");

        h.app.parts.borrow().as_ref().unwrap().tray.as_ref().unwrap().mark_icon_not_shown();
        h.app.on_close(HWND::default());
        assert!(h.app.exiting.get(), "アイコンが無いのに閉じる操作で隠した");
        drop(h.app.take_parts());
    }

    /// 表示中のセッション終了: 最後の保存をして、以後は知らせない（表示中のボックスは閉じない。
    /// この後プロセスは終わらされる）。`main` は最後の保存をやり直さない。
    #[test]
    fn end_session_during_display_saves_and_stops_notifications() {
        let h = NotifyHarness::new(true);
        h.sink.report(failure("A"));
        let sink = h.sink.clone();
        h.during_first_display(move |app| {
            sink.report(failure("B"));
            app.on_end_session(HWND::default());
        });
        h.wake();
        assert_eq!(h.shown().len(), 1);
        assert!(h.app.session_ended());
        assert!(h.app.core.admit().is_err(), "受け付けを閉じていない");
    }

    #[test]
    fn failures_are_not_shown_when_setting_is_off() {
        let h = NotifyHarness::new(false);
        h.sink.report(failure("A"));
        h.wake();
        assert!(h.shown().is_empty());
        assert!(h.app.notifier.borrow().pending.is_empty());

        // 履歴のファイルを書けなかったことは、設定がオフでも知らせる
        h.sink.report(ActionFailure { kind: ActionKind::SaveHistory, message: "B".to_string() });
        h.wake();
        let shown = h.shown();
        assert_eq!(shown.len(), 1, "{shown:?}");
        assert!(shown[0].starts_with("履歴のファイルを保存できませんでした") && shown[0].ends_with("B"), "{shown:?}");
    }

    /// 実物のメッセージボックスの表示中に、モーダルループの中で `PostQuitMessage` が呼ばれると
    /// （トレイの「終了」・トレイなしで閉じる）、ボックスは閉じて WM_QUIT を投げ直す（メインの
    /// メッセージループが抜けられる）。
    #[test]
    fn message_box_closes_on_quit_posted_inside_its_loop_and_reposts_it() {
        use windows::Win32::UI::WindowsAndMessaging::{
            FindWindowW, KillTimer, PeekMessageW, PostMessageW, SetTimer, MSG, PM_REMOVE, WM_CLOSE, WM_QUIT,
        };
        use windows::Win32::Foundation::{LPARAM, WPARAM};

        unsafe extern "system" fn quit_from_timer(_hwnd: HWND, _msg: u32, id: usize, _time: u32) {
            unsafe {
                let _ = KillTimer(None, id);
                PostQuitMessage(0);
            }
        }

        let _gui = crate::tray::lock_gui_resource_tests();
        let caption = format!("CLCLR テスト {}", Uuid::new_v4());
        let (done_tx, done_rx) = mpsc::channel();
        let thread = {
            let caption = caption.clone();
            std::thread::spawn(move || unsafe {
                SetTimer(None, 0, 200, Some(quit_from_timer));
                show_message_box(None, &caption, "WM_QUIT で閉じるかの確認（自動で閉じます）");
                let mut msg = MSG::default();
                let reposted = PeekMessageW(&mut msg, None, WM_QUIT, WM_QUIT, PM_REMOVE).as_bool();
                let _ = done_tx.send(reposted);
            })
        };
        match done_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(reposted) => {
                thread.join().unwrap();
                assert!(reposted, "WM_QUIT を投げ直していない");
            }
            Err(_) => {
                // 閉じなかった: 後続のテストへ残さないよう閉じてから失敗にする
                let caption = HSTRING::from(caption.as_str());
                if let Ok(hwnd) = unsafe { FindWindowW(windows::core::w!("#32770"), &caption) } {
                    let _ = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
                }
                let _ = done_rx.recv_timeout(Duration::from_secs(5));
                let _ = thread.join();
                panic!("モーダルループの中の PostQuitMessage でメッセージボックスが閉じない");
            }
        }
    }

    // --- 設定の反映 ---

    /// 設定の反映の試験台: 設定ファイルを一時フォルダに置き、操作スレッドの代わりの受け口につないだ App
    /// （parts なし）。
    fn settings_app(config: Config) -> (std::path::PathBuf, Rc<App>, mpsc::Receiver<Action>, FailureSink) {
        let (dir, core) = temp_service(config.clone());
        let (_tray_tx, tray_rx) = mpsc::channel();
        let (_hotkey_tx, hotkey_rx) = mpsc::channel();
        let mut app = App::new(Arc::new(RwLock::new(config)), core, tray_rx, hotkey_rx);
        app.set_config_path(dir.join("config.toml"));
        let (tx, rx) = mpsc::channel();
        let sink = FailureSink::new(|| {});
        app.attach_actions(tx, sink.clone());
        (dir, Rc::new(app), rx, sink)
    }

    /// 入力の誤り・保存の失敗・今は反映できないとき（反映の途中・終了の要求の後・失敗の通知の表示中）は、
    /// ファイルも共有の設定も変えない。
    #[test]
    fn apply_settings_refuses_invalid_unsaved_and_busy_without_changes() {
        let base = memory_only_config();
        let (dir, app, rx, _sink) = settings_app(base.clone());
        let path = dir.join("config.toml");
        let mut draft = base.clone();
        draft.history.max = 10_001;
        match app.apply_settings(HWND::default(), &base, &draft) {
            Err(ApplyError::Invalid(issues)) => assert_eq!(issues[0].field, "history.max"),
            other => panic!("入力の誤りになっていない: {other:?}"),
        }
        assert!(!path.exists());

        draft.history.max = 5;
        std::fs::create_dir_all(&path).unwrap(); // 保存先を同じ名前のフォルダでふさぐ
        assert!(matches!(app.apply_settings(HWND::default(), &base, &draft), Err(ApplyError::Save(_))));
        assert_eq!(app.config.read().unwrap().history.max, base.history.max, "保存できないのに共有の設定を変えた");
        std::fs::remove_dir(&path).unwrap();

        for flag in [&app.applying, &app.exiting] {
            flag.set(true);
            assert!(matches!(app.apply_settings(HWND::default(), &base, &draft), Err(ApplyError::Busy)));
            flag.set(false);
        }
        app.notifier.borrow_mut().notifying = true;
        assert!(matches!(app.apply_settings(HWND::default(), &base, &draft), Err(ApplyError::Busy)));
        app.notifier.borrow_mut().notifying = false;
        assert!(!path.exists(), "断ったのに保存した");
        assert_eq!(app.config.read().unwrap().history.max, base.history.max);
        assert!(rx.try_iter().next().is_none(), "断ったのに依頼した");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 反映できたら、合わせた設定を保存して共有の設定にし（開いている間のトレイの監視の切り替えは残る）、
    /// 保持件数が変われば操作スレッドへ切り詰めを依頼する。
    #[test]
    fn apply_settings_saves_merged_config_and_requests_trim() {
        let base = memory_only_config();
        let (dir, app, rx, _sink) = settings_app(base.clone());
        let mut draft = base.clone();
        draft.history.grouping.enabled = false;
        draft.history.max = 5;
        // 開いている間にトレイで監視を切り替えた
        app.config.write().unwrap().general.clipboard_watch = !base.general.clipboard_watch;
        let failures = app.apply_settings(HWND::default(), &base, &draft).unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        let saved = Config::load(&dir.join("config.toml")).unwrap();
        assert_eq!((saved.history.max, saved.history.grouping.enabled), (5, false));
        assert_eq!(saved.general.clipboard_watch, !base.general.clipboard_watch, "トレイの切り替えを上書きした");
        assert_eq!(toml::to_string(&*app.config.read().unwrap()).unwrap(), toml::to_string(&saved).unwrap());
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), [Action::Trim]);
        assert!(!app.applying.get(), "反映の後に印を下ろしていない");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// フィルタは、OK の後に共有の設定を写す処理（取り込みのスレッドは変更のたびに写す）から効く: 反映すると
    /// 共有の設定の判定が変わり、その場の反映（依頼）は無い。実際の取り込みの時点は実機で見る。
    #[test]
    fn apply_settings_puts_filters_into_shared_config() {
        let mut base = memory_only_config();
        base.format_filter_default = crate::config::FilterAction::Add;
        let (dir, app, rx, _sink) = settings_app(base.clone());
        assert_eq!(app.config.read().unwrap().should_capture("独自の形式"), crate::config::FilterAction::Add);
        let mut draft = base.clone();
        draft.format_filters.push(crate::config::FormatFilter {
            format_name: "独自の形式".into(),
            action: crate::config::FilterAction::Ignore,
            save: false,
            limit_size: 4096,
        });
        draft.window_filters.push(crate::config::WindowFilter { title: "秘密".into(), class_name: String::new(), ignore: true });
        let failures = app.apply_settings(HWND::default(), &base, &draft).unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        let shared = app.config.read().unwrap().clone();
        assert_eq!(shared.should_capture("独自の形式"), crate::config::FilterAction::Ignore);
        assert_eq!(shared.size_limit("独自の形式"), 4096);
        assert!(shared.is_window_ignored("秘密のメモ", "Notepad"));
        assert!(rx.try_iter().next().is_none(), "フィルタだけの変更で依頼した");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 切り詰めを依頼できないとき（操作スレッドが無い）は、反映できなかったものとして返し、知らせる。設定の
    /// 反映の失敗は、操作の失敗を知らせない設定でも知らせる（操作スレッドから届いたものも同じ）。
    #[test]
    fn settings_failures_are_notified_even_when_action_errors_are_off() {
        let mut base = memory_only_config();
        base.general.notify_action_errors = false;
        let (dir, app, _rx, sink) = settings_app(base.clone());
        let actions = app.actions.borrow_mut().take(); // 操作スレッドが無い
        let mut draft = base.clone();
        assert!(!base.history.grouping.enabled, "前提: 階層表示は無効（最大件数が保持件数になる）");
        draft.history.max = base.history.max + 1;
        let failures = app.apply_settings(HWND::default(), &base, &draft).unwrap();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("切り詰め"), "{failures:?}");
        let pending: Vec<ActionKind> = app.notifier.borrow_mut().pending.drain(..).map(|n| match n { Notice::Failure(f) => f.kind, other => panic!("{other:?}") }).collect();
        assert_eq!(pending, [ActionKind::ApplySettings]);

        *app.actions.borrow_mut() = actions;
        sink.report(ActionFailure { kind: ActionKind::ApplySettings, message: "切り詰め".into() });
        sink.report(ActionFailure { kind: ActionKind::Send, message: "送る".into() });
        app.collect_failures();
        let pending: Vec<ActionKind> = app.notifier.borrow_mut().pending.drain(..).map(|n| match n { Notice::Failure(f) => f.kind, other => panic!("{other:?}") }).collect();
        assert_eq!(pending, [ActionKind::ApplySettings], "設定の反映の失敗だけを知らせる");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// ホットキーの登録し直しの結果は、最後に頼んだ番号のものだけを知らせる（操作の失敗を知らせない設定でも）。
    #[test]
    fn only_latest_reregister_result_is_notified() {
        let h = NotifyHarness::new(false);
        h.app.settings_generation.set(2);
        for (generation, problems) in [(1, vec!["古い".to_string()]), (2, vec!["新しい".to_string()]), (2, vec![])] {
            h.hotkey.send(HotkeyEvent::Reregistered { generation, problems }).unwrap();
        }
        h.wake();
        let shown = h.shown();
        assert_eq!(shown.len(), 1, "{shown:?}");
        assert!(shown[0].contains("新しい") && !shown[0].contains("古い"), "{shown:?}");
        assert!(shown[0].contains(REREGISTER_RETRY_NOTE), "保存したことと次の起動で試すことを添える: {shown:?}");
    }

    /// 実際の監視・ビューアで: 監視は実際の状態と違えば切り替える。ホットキーの機能が無い・トレイを作れない
    /// ときは反映できなかったものとして返す。トレイが無く、ビューアが隠れていればビューアを出す。
    #[test]
    fn apply_settings_with_parts_reports_what_could_not_be_applied_and_shows_viewer() {
        let _clipboard = crate::clipboard::test_support::lock_clipboard_tests();
        let _gui = crate::tray::lock_gui_resource_tests();
        let mut base = memory_only_config();
        base.general.clipboard_watch = false;
        base.general.show_trayicon = false;
        let (dir, app, _rx, _sink) = settings_app(base.clone());
        let handler: Rc<dyn ViewerHandler> = app.clone();
        let window = viewer::ViewerWindow::create("CLCLR apply test", (400, 300), handler).unwrap();
        let hwnd = window.hwnd();
        let watcher = ClipboardWatcher::spawn(Arc::clone(&app.config), |_| {}).unwrap();
        app.set_parts(Parts { watcher, tray: None, hotkeys: None });

        let mut draft = base.clone();
        draft.general.clipboard_watch = true;
        draft.general.show_trayicon = true; // トレイを作る準備（`attach_tray_source`）が無いので作れない
        draft.hotkey.popup_menu.key = "V".to_string();
        assert!(!viewer::is_visible(hwnd));
        let failures = app.apply_settings(hwnd, &base, &draft).unwrap();
        assert_eq!(failures.len(), 2, "{failures:?}");
        assert!(failures.iter().any(|f| f.contains("ホットキー")) && failures.iter().any(|f| f.contains("トレイ")));
        assert!(app.parts.borrow().as_ref().unwrap().watcher.listening(), "監視を開始していない");
        assert!(viewer::is_visible(hwnd), "トレイが無く隠れていたのにビューアを出していない");

        drop(app.take_parts());
        drop(window);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 投稿されたメッセージを配送しながら WM_QUIT を探す（WM_QUIT はほかの投稿が残っている間は返らない）。
    fn quit_was_posted() -> bool {
        use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, PeekMessageW, MSG, PM_REMOVE, WM_QUIT};
        for _ in 0..100 {
            let mut msg = MSG::default();
            if !unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                return false;
            }
            if msg.message == WM_QUIT {
                return true;
            }
            unsafe { DispatchMessageW(&msg) };
        }
        false
    }

    /// 反映の途中の再入（送られた閉じる要求・セッションの終了）の代わりに、決まった位置で終了の要求を起こし、
    /// その後の反映が止まることを確かめる。位置ごとに:
    /// - トレイを作る前にセッションの終了: トレイを作らず、ビューアを出さない
    /// - 一覧の作り直しの後にセッションの終了: トレイを消さず、ビューアを出さない
    /// - トレイを取り出した後（破棄の待ちの中の再入に当たる）に閉じる要求: トレイが無いので終了の要求になり、
    ///   ビューアを出さない。WM_QUIT が残る
    #[test]
    fn exit_or_end_session_during_apply_stops_the_rest() {
        let _clipboard = crate::clipboard::test_support::lock_clipboard_tests();
        let _gui = crate::tray::lock_gui_resource_tests();
        for stage in ["before_tray_create", "after_refresh", "tray_taken"] {
            let mut base = memory_only_config();
            base.general.clipboard_watch = false;
            // トレイを作る段を試すときは、無い状態から作らせる。ほかは有る状態から消させる
            base.general.show_trayicon = stage != "before_tray_create";
            let (dir, app, _rx, _sink) = settings_app(base.clone());
            let handler: Rc<dyn ViewerHandler> = app.clone();
            let window = viewer::ViewerWindow::create("CLCLR apply exit test", (400, 300), handler).unwrap();
            let hwnd = window.hwnd();
            let watcher = ClipboardWatcher::spawn(Arc::clone(&app.config), |_| {}).unwrap();
            let (tray_tx, _tray_rx) = mpsc::channel();
            app.attach_tray_source(tray_tx.clone(), window.waker());
            let tray = base.general.show_trayicon.then(|| Tray::spawn(watcher.watch_state(), tray_tx, || {}).unwrap());
            app.set_parts(Parts { watcher, tray, hotkeys: None });

            let weak = Rc::downgrade(&app);
            *app.settings_hook.borrow_mut() = Some(Box::new(move |at| {
                if at != stage {
                    return;
                }
                let app = weak.upgrade().unwrap();
                if stage == "tray_taken" {
                    app.on_close(hwnd);
                } else {
                    app.on_end_session(hwnd);
                }
            }));
            let mut draft = base.clone();
            draft.general.show_trayicon = !base.general.show_trayicon;
            draft.history.grouping.folder_name_format = "x%1".to_string(); // 一覧の作り直しを起こす
            app.apply_settings(hwnd, &base, &draft).unwrap();
            *app.settings_hook.borrow_mut() = None;

            let has_tray = app.parts.borrow().as_ref().unwrap().tray.is_some();
            match stage {
                "before_tray_create" => {
                    assert!(app.session_ended(), "{stage}: 前提");
                    assert!(!has_tray, "{stage}: セッションの終了の後にトレイを作った");
                }
                "after_refresh" => {
                    assert!(app.session_ended(), "{stage}: 前提");
                    assert!(has_tray, "{stage}: セッションの終了の後にトレイを消した");
                }
                _ => {
                    assert!(app.exiting.get(), "{stage}: トレイの無い閉じる要求が終了の要求になっていない（前提）");
                    assert!(quit_was_posted(), "{stage}: WM_QUIT が残っていない");
                }
            }
            assert!(!viewer::is_visible(hwnd), "{stage}: 終了の後にビューアを出した");

            drop(app.take_parts());
            drop(window);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    // --- 設定画面 ---

    /// 投稿されたメッセージを `ms` の間、配送する（設定画面の閉じる要求などを処理させる）。
    fn pump_for(ms: u64) {
        use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE};
        let until = std::time::Instant::now() + Duration::from_millis(ms);
        let mut msg = MSG::default();
        while std::time::Instant::now() < until {
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == windows::Win32::UI::WindowsAndMessaging::WM_QUIT {
                        // 後で確かめるために投げ直す
                        windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0);
                        return;
                    }
                    if settings::is_dialog_message(&msg) {
                        continue;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn settings_hwnd(app: &App) -> Option<HWND> {
        app.settings.borrow().as_ref().map(SettingsWindow::hwnd)
    }

    /// 設定画面は1つだけ開く。ビューアを隠すと隠れ、表示すると（編集中のまま）戻る。終了の要求では隠すだけで、
    /// 表示しても戻らない。`close_settings` で破棄する。
    #[test]
    fn settings_window_follows_viewer_and_is_destroyed_after_exit() {
        use windows::Win32::UI::WindowsAndMessaging::{IsWindow, IsWindowVisible};
        let _gui = crate::tray::lock_gui_resource_tests();
        let (dir, app, _rx, _sink) = settings_app(memory_only_config());
        App::bind_self(&app);
        let handler: Rc<dyn ViewerHandler> = app.clone();
        let window = viewer::ViewerWindow::create("CLCLR settings test", (400, 300), handler).unwrap();
        let hwnd = window.hwnd();
        app.on_open_settings(hwnd);
        let dialog = settings_hwnd(&app).expect("設定画面を開いていない");
        app.on_open_settings(hwnd);
        assert_eq!(settings_hwnd(&app), Some(dialog), "二重に開いた");
        let visible = || unsafe { IsWindowVisible(dialog) }.as_bool();
        assert!(visible());

        app.on_hidden(hwnd);
        assert!(!visible(), "ビューアを隠しても隠れない");
        app.on_shown(hwnd);
        assert!(visible(), "ビューアを表示しても戻らない");

        app.begin_exit(Some(hwnd));
        assert!(!visible() && settings_hwnd(&app).is_some(), "終了の要求で隠すだけになっていない");
        app.on_shown(hwnd);
        assert!(!visible(), "終了の後に出した");
        app.close_settings();
        assert!(settings_hwnd(&app).is_none() && !unsafe { IsWindow(Some(dialog)) }.as_bool());
        pump_for(50);
        drop(window);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 設定画面の OK（トレイを消す設定）で反映している途中、トレイの破棄の待ちに当たる位置で再入が起きても、窓は
    /// 壊れない:
    /// - 設定画面の閉じる要求（2回）: OK の後に1回だけ閉じる
    /// - ビューアの閉じる要求（トレイ無し → 終了）・セッションの終了: 設定画面は隠れて残り、閉じる要求は投稿されず、
    ///   `close_settings` で破棄する
    #[test]
    fn reentry_during_settings_ok_keeps_settings_window_safe() {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetDlgItem, IsWindow, IsWindowVisible, SendMessageW, BM_SETCHECK, IDCANCEL, IDOK, WM_COMMAND,
        };
        let _clipboard = crate::clipboard::test_support::lock_clipboard_tests();
        let _gui = crate::tray::lock_gui_resource_tests();
        for case in ["settings_cancel", "viewer_close", "end_session"] {
            let mut base = memory_only_config();
            base.general.clipboard_watch = false;
            base.general.show_trayicon = true;
            let (dir, app, _rx, _sink) = settings_app(base.clone());
            App::bind_self(&app);
            let handler: Rc<dyn ViewerHandler> = app.clone();
            let window = viewer::ViewerWindow::create("CLCLR settings reentry test", (400, 300), handler).unwrap();
            let hwnd = window.hwnd();
            let watcher = ClipboardWatcher::spawn(Arc::clone(&app.config), |_| {}).unwrap();
            let (tray_tx, _tray_rx) = mpsc::channel();
            let tray = Tray::spawn(watcher.watch_state(), tray_tx, || {}).unwrap();
            app.set_parts(Parts { watcher, tray: Some(tray), hotkeys: None });
            viewer::show(hwnd);
            app.on_open_settings(hwnd);
            let dialog = settings_hwnd(&app).unwrap();
            let general = app.settings.borrow().as_ref().unwrap().pages()[0];
            unsafe {
                SendMessageW(GetDlgItem(Some(general), settings::IDC_GEN_TRAY).unwrap(), BM_SETCHECK, Some(WPARAM(0)), None);
            }

            let weak = Rc::downgrade(&app);
            *app.settings_hook.borrow_mut() = Some(Box::new(move |at| {
                if at != "tray_taken" {
                    return;
                }
                let app = weak.upgrade().unwrap();
                match case {
                    "settings_cancel" => unsafe {
                        SendMessageW(dialog, WM_COMMAND, Some(WPARAM(IDCANCEL.0 as usize)), Some(LPARAM(0)));
                        SendMessageW(dialog, WM_COMMAND, Some(WPARAM(IDCANCEL.0 as usize)), Some(LPARAM(0)));
                    },
                    "viewer_close" => app.on_close(hwnd),
                    _ => app.on_end_session(hwnd),
                }
            }));
            unsafe {
                SendMessageW(dialog, WM_COMMAND, Some(WPARAM(IDOK.0 as usize)), Some(LPARAM(0)));
            }
            *app.settings_hook.borrow_mut() = None;
            assert!(app.parts.borrow().as_ref().unwrap().tray.is_none(), "{case}: 前提: トレイを消していない");
            pump_for(150);
            match case {
                "settings_cancel" => {
                    assert!(settings_hwnd(&app).is_none(), "{case}: OK の後に閉じていない");
                    assert!(!unsafe { IsWindow(Some(dialog)) }.as_bool());
                }
                _ => {
                    assert!(app.exiting.get(), "{case}: 前提: 終了の要求になっていない");
                    assert_eq!(settings_hwnd(&app), Some(dialog), "{case}: 終了の要求の後に閉じる要求で破棄した");
                    assert!(!unsafe { IsWindowVisible(dialog) }.as_bool(), "{case}: 終了の後に設定画面が出ている");
                    app.close_settings();
                    assert!(!unsafe { IsWindow(Some(dialog)) }.as_bool());
                    let _ = quit_was_posted();
                }
            }
            drop(app.take_parts());
            drop(window);
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
