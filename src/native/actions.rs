//! ビューアの操作（送る・テキスト変換・関連付けで開く・ピン留め・削除）を行う専用スレッド。
//!
//! メインスレッドは依頼（`Action`）を送るだけで、完了を待たない。
//! このスレッドは依頼を届いた順に1件ずつ処理し、失敗は閉じられる通知先（`FailureSink`）へ
//! 送ってビューアを起こす。
//!
//! - 受け付け: 1件の操作は、`Core` の受け付けの登録（`Ticket`）1つで、読み込みから書き込み
//!   （開くは Shell スレッドへの引き渡し）までを覆う。登録できなければ（終了処理が始まった）何も
//!   しない。キューに入れただけの依頼は、まだ受け付けていない
//! - 関連付けで開く・書き出してフォルダで表示:`ShellExecuteExW`・`SHOpenFolderAndSelectItems` は、依頼ごとに
//!   作る短命の Shell スレッドで呼ぶ（ダイアログ
//!   などで戻らないことがあり、その間に後の依頼を待たせないため）。Shell の起動そのものは受け付けの
//!   外で、Shell スレッドは join しない。同時に起動中は `MAX_SHELL_LAUNCHES` 件まで（空きを待たずに
//!   失敗にする）
//! - 起動時の一時フォルダの掃除は、このスレッドの最初の仕事（メインスレッドの起動処理に I/O を
//!   足さない。同じスレッドなので、開く前に必ず終わる）
//! - このスレッドも join しない（通常の終了で `Core::shutdown(None)` が成功したときは、受け付けの
//!   中の処理はそれが待ち終えている。残るのは掃除と、受け付けに登録できずに抜ける依頼と、閉じた
//!   通知先へのログと、引き渡し済みの Shell の起動だけ。セッション終了の時間切れでは、処理中の
//!   操作が残りうる）

use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};

use uuid::Uuid;
use windows::core::PCWSTR;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows::Win32::UI::Shell::{
    ILCreateFromPathW, ILFree, SHOpenFolderAndSelectItems, ShellExecuteExW, SEE_MASK_FLAG_LOG_USAGE,
    SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::clipboard::ClipboardPort;
use crate::config::Config;
use crate::data::{utf16_bytes, utf16_text, Format};
use crate::datacheck::{CleanResult, DataReport};
use crate::ops::{Core, OpError};
use crate::service::ImageData;
use crate::store::Direction;
use crate::tools::text::{convert_entry_date, TextTransform};

/// 同時に起動中にできる「関連付けで開く」の数（ダイアログを閉じずに開き続けても、Shell
/// スレッドが増え続けないようにする）。
const MAX_SHELL_LAUNCHES: usize = 4;

/// ビューアからの操作の依頼。`pinned` は表示元がピン留めか（`Source::Pinned`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// クリップボードへ送る（貼り付けはしない）
    Send { id: Uuid, pinned: bool },
    /// CF_UNICODETEXT に変換を当てた結果を、新しいテキストとしてクリップボードへ書く
    Transform { id: Uuid, pinned: bool, transform: TextTransform },
    /// 画像を一時フォルダへ書き出し（WebP で保存してあればそのまま、ほかは BMP）、関連付けで開く
    OpenImage { id: Uuid, pinned: bool },
    /// 画像を `OpenImage` と同じく書き出し、そのファイルを選んだ状態でフォルダを開く
    OpenImageLocation { id: Uuid, pinned: bool },
    /// 履歴の項目をピン留めに複製し、`to`（None はルート）のフォルダへ入れる（`Core::pin`）
    Pin { id: Uuid, to: Option<Uuid> },
    /// ピン留めの項目を `to`（None はルート）のフォルダへ移す（`Core::move_pinned`）
    MovePinned { id: Uuid, to: Option<Uuid> },
    /// ピン留めの項目・フォルダを、同じ親の中で隣と入れ替える（`Core::reorder_pinned`）
    ReorderPinned { id: Uuid, direction: Direction },
    /// `parent`（None はルート）の中にフォルダを作る（`Core::create_folder`）
    CreateFolder { parent: Option<Uuid>, title: String },
    /// フォルダの名前を変える（`Core::rename_folder`）
    RenameFolder { id: Uuid, title: String },
    /// ピン留めのアイテムの名前を変える（空なら自動の名前に戻す。`Core::rename_pinned_item`）
    RenamePinned { id: Uuid, title: String },
    /// 項目を消す（履歴は `Core::delete_history`、ピン留めは `Core::delete_pinned`）
    Delete { id: Uuid, pinned: bool },
    /// 履歴を全部消す（`Core::clear_history`。確認はビューアが依頼の前に済ませる）
    ClearHistory,
    /// クリップボードを空にする（C版 CLCL の tool_utl「クリップボードのクリア」）
    ClearClipboard,
    /// 設定の保持件数へ切り詰める（設定の反映から。`Core::trim`）
    Trim,
    /// データのチェック（`Core::check_data`。結果は `Notice::CheckReport` で返す）
    CheckData,
    /// チェックの結果のうち、今も見つかるものを消す（`Core::clean_data`。結果は `Notice::CleanResult`）
    CleanData(DataReport),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionKind {
    Send,
    Transform,
    OpenImage,
    OpenImageLocation,
    Pin,
    MovePinned,
    ReorderPinned,
    CreateFolder,
    RenameFolder,
    RenamePinned,
    Delete,
    ClearHistory,
    ClearClipboard,
    /// 設定の反映（保存した設定の一部をその場で反映できなかった）。操作の失敗の設定
    /// （`notify_action_errors`）に関係なく知らせる
    ApplySettings,
    /// 項目は消したが、履歴のファイル（history.toml）を書き直せなかった（削除・クリア・切り詰めの
    /// `OpError::IndexNotSaved`）。異常終了すると次の起動で項目が戻るので、操作の失敗の設定に関係なく知らせる
    SaveHistory,
    /// データのチェック・その削除（利用者が自分で頼んだ操作なので、失敗は設定に関係なく知らせる）
    CheckData,
    CleanData,
}

impl ActionKind {
    /// 操作の失敗の設定（`notify_action_errors`）に関係なく知らせるか。
    pub fn always_notified(self) -> bool {
        matches!(self, Self::ApplySettings | Self::SaveHistory | Self::CheckData | Self::CleanData)
    }

    /// 失敗の知らせの見出し（「{見出し}: {理由}」の前半。「{操作の名前}に失敗しました」の形にしないのは、
    /// 操作の名前をつなぐと「クリップボードへ送るに失敗しました」のような文になるため）。
    fn failure_headline(self) -> &'static str {
        match self {
            Self::Send => "クリップボードへ送れませんでした",
            Self::Transform => "テキストを変換できませんでした",
            Self::OpenImage => "画像を開けませんでした",
            Self::OpenImageLocation => "画像を書き出してフォルダで表示できませんでした",
            Self::Pin => "ピン留めに追加できませんでした",
            Self::MovePinned => "移動できませんでした",
            Self::ReorderPinned => "並べ替えられませんでした",
            Self::CreateFolder => "フォルダを作成できませんでした",
            Self::RenameFolder | Self::RenamePinned => "名前を変更できませんでした",
            Self::Delete => "削除できませんでした",
            Self::ClearHistory => "履歴をクリアできませんでした",
            Self::ClearClipboard => "クリップボードをクリアできませんでした",
            Self::ApplySettings => "設定の一部を反映できませんでした",
            Self::SaveHistory => "履歴のファイルを保存できませんでした",
            Self::CheckData => "データをチェックできませんでした",
            Self::CleanData => "データを削除できませんでした",
        }
    }
}

impl Action {
    fn kind(&self) -> ActionKind {
        match self {
            Self::Send { .. } => ActionKind::Send,
            Self::Transform { .. } => ActionKind::Transform,
            Self::OpenImage { .. } => ActionKind::OpenImage,
            Self::OpenImageLocation { .. } => ActionKind::OpenImageLocation,
            Self::Pin { .. } => ActionKind::Pin,
            Self::MovePinned { .. } => ActionKind::MovePinned,
            Self::ReorderPinned { .. } => ActionKind::ReorderPinned,
            Self::CreateFolder { .. } => ActionKind::CreateFolder,
            Self::RenameFolder { .. } => ActionKind::RenameFolder,
            Self::RenamePinned { .. } => ActionKind::RenamePinned,
            Self::Delete { .. } => ActionKind::Delete,
            Self::ClearHistory => ActionKind::ClearHistory,
            Self::ClearClipboard => ActionKind::ClearClipboard,
            Self::Trim => ActionKind::ApplySettings,
            Self::CheckData => ActionKind::CheckData,
            Self::CleanData(_) => ActionKind::CleanData,
        }
    }
}

/// 操作スレッドからビューアへの知らせ（失敗と、データのチェックの結果）。届いた順に1つのキューに並べ、その順に
/// 出す。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    Failure(ActionFailure),
    CheckReport(DataReport),
    CleanResult(CleanResult),
}

impl fmt::Display for Notice {
    /// ログに書く文（出せなかったとき）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failure(failure) => write!(f, "{failure}"),
            Self::CheckReport(r) => write!(
                f,
                "データのチェックの結果: 参照されないファイル {} 件、データが欠けた項目 {} 件、一時ファイル {} 件、対象外 {} 件",
                r.orphans.len(),
                r.missing.len(),
                r.temps.len(),
                r.ignored.len()
            ),
            Self::CleanResult(r) => write!(
                f,
                "データの削除の結果: ファイル {} 件（{} バイト）、項目 {} 件、消せなかったもの {} 件{}",
                r.files_removed,
                r.bytes_removed,
                r.items_removed,
                r.failures.len(),
                if r.interrupted { "、途中でやめた" } else { "" }
            ),
        }
    }
}

/// 操作の失敗（メインスレッドが設定に従って知らせる）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionFailure {
    pub kind: ActionKind,
    pub message: String,
}

impl fmt::Display for ActionFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.failure_headline(), self.message)
    }
}

/// 失敗の通知先。操作スレッド・Shell スレッドが積み、メインスレッドが取り出す。閉じた後
/// （終了の要求の後）に届いた失敗はログに書くだけで、ビューアを起こさない。
///
/// 積むことと起床の投稿（`notify`）を、閉じることと同じロックの中で行う。閉じるのはビューア窓を
/// 破棄する前なので、破棄した窓へ起床を投稿しない。ロックの区間は短く、Shell の呼び出しや
/// 通知の表示の間は持たない。
#[derive(Clone)]
pub struct FailureSink {
    state: Arc<Mutex<SinkState>>,
    notify: Arc<dyn Fn() + Send + Sync>,
}

struct SinkState {
    open: bool,
    queue: Vec<Notice>,
}

impl FailureSink {
    pub fn new(notify: impl Fn() + Send + Sync + 'static) -> Self {
        Self { state: Arc::new(Mutex::new(SinkState { open: true, queue: Vec::new() })), notify: Arc::new(notify) }
    }

    pub fn report(&self, failure: ActionFailure) {
        self.post(Notice::Failure(failure));
    }

    /// 知らせを積んでビューアを起こす（閉じた後はログに書くだけ）。
    pub fn post(&self, notice: Notice) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.open {
            state.queue.push(notice);
            (self.notify)();
        } else {
            eprintln!("{notice}");
        }
    }

    /// 積まれた知らせを届いた順に取り出す（メインスレッドの起床で呼ぶ）。
    pub fn drain(&self) -> Vec<Notice> {
        std::mem::take(&mut self.state.lock().unwrap_or_else(|p| p.into_inner()).queue)
    }

    /// ビューアを起こすだけ（ピン留め・削除が成功したとき、一覧を作り直させる）。閉じた後は
    /// 何もしない（破棄した窓へ起床を投稿しない。`report` と同じロックの中で判定する）。
    pub fn wake(&self) {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.open {
            (self.notify)();
        }
    }

    /// 閉じる。まだ取り出されていない知らせを返す（呼び出し側がログに書く）。
    pub fn close(&self) -> Vec<Notice> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.open = false;
        std::mem::take(&mut state.queue)
    }
}

/// 同時に起動中の「関連付けで開く」の数。
#[derive(Default)]
struct ShellSlots {
    in_use: Mutex<usize>,
}

/// 起動の枠1つ。破棄で返す（Shell スレッドの終わり・生成の失敗・パニックでも）。
pub struct ShellSlot {
    slots: Arc<ShellSlots>,
}

impl ShellSlots {
    /// 空きがあれば枠を取る。空きを待たない。
    fn try_acquire(self: &Arc<Self>) -> Option<ShellSlot> {
        let mut in_use = self.in_use.lock().unwrap_or_else(|p| p.into_inner());
        if *in_use >= MAX_SHELL_LAUNCHES {
            return None;
        }
        *in_use += 1;
        Some(ShellSlot { slots: Arc::clone(self) })
    }
}

impl Drop for ShellSlot {
    fn drop(&mut self) {
        *self.slots.in_use.lock().unwrap_or_else(|p| p.into_inner()) -= 1;
    }
}

/// 操作スレッドの副作用（テストでは差し替える）。
pub trait Effects: Send + 'static {
    /// 送る: `should_suppress`（`delete_on_send`）なら、書いた変更の番号を抑止する番号として記録する
    fn write_send(&self, port: &ClipboardPort, formats: &[Format], should_suppress: bool) -> Result<(), String>;
    /// 変換: 抑止せずに書く（新しい履歴として取り込まれる）
    fn write_text(&self, port: &ClipboardPort, formats: &[Format]) -> Result<(), String>;
    /// クリップボードを空にする
    fn clear_clipboard(&self, port: &ClipboardPort) -> Result<(), String>;
    /// 起動時の一時フォルダの掃除
    fn clean_temp(&self, dir: &Path);
    /// Shell スレッドを作って `path` を `action` のとおりに開く（生成が終われば戻る。起動の結果は待たない）
    fn launch(&self, path: PathBuf, action: ShellAction, slot: ShellSlot, sink: FailureSink) -> Result<(), String>;
}

/// 書き出したファイルの開き方。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellAction {
    /// 関連付けで開く（`ShellExecuteExW`）
    Open,
    /// ファイルを選んだ状態でフォルダを開く（`SHOpenFolderAndSelectItems`）
    Reveal,
}

impl ShellAction {
    /// Shell スレッドの中で失敗したときに知らせる種類。
    fn failure_kind(self) -> ActionKind {
        match self {
            Self::Open => ActionKind::OpenImage,
            Self::Reveal => ActionKind::OpenImageLocation,
        }
    }
}

/// 本物の副作用。
pub struct SystemEffects;

impl Effects for SystemEffects {
    fn write_send(&self, port: &ClipboardPort, formats: &[Format], should_suppress: bool) -> Result<(), String> {
        // エラーの文言だけで理由が分かる（見出しと重ねない）
        crate::clipboard::set_clipboard_suppressed(port, formats, should_suppress).map_err(|e| e.to_string())
    }

    fn write_text(&self, port: &ClipboardPort, formats: &[Format]) -> Result<(), String> {
        crate::clipboard::set_clipboard(port, formats).map_err(|e| e.to_string())
    }

    fn clear_clipboard(&self, port: &ClipboardPort) -> Result<(), String> {
        crate::clipboard::clear_clipboard(port).map_err(|e| e.to_string())
    }

    fn clean_temp(&self, dir: &Path) {
        if let Err(e) = std::fs::remove_dir_all(dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            // 外部のアプリが開いたままのファイルは消せない。次の起動でもう一度消す
            eprintln!("一時フォルダ {} を消しきれませんでした: {e}", dir.display());
        }
    }

    fn launch(&self, path: PathBuf, action: ShellAction, slot: ShellSlot, sink: FailureSink) -> Result<(), String> {
        let open = match action {
            ShellAction::Open => open_with_association as fn(&Path) -> Result<(), String>,
            ShellAction::Reveal => reveal_in_folder,
        };
        thread::Builder::new()
            .name("clclr-shell".to_string())
            .spawn(move || shell_thread_body(&path, action.failure_kind(), slot, &sink, open))
            .map(drop)
            .map_err(|e| format!("起動用のスレッドを作れません（{e}）"))
    }
}

/// Shell スレッドの中身（`open` を差し替えてテストする）。失敗は `kind` で知らせる。枠はこの関数を抜けると返る。
fn shell_thread_body(
    path: &Path,
    kind: ActionKind,
    slot: ShellSlot,
    sink: &FailureSink,
    open: impl FnOnce(&Path) -> Result<(), String>,
) {
    let _slot = slot;
    if let Err(message) = open(path) {
        sink.report(ActionFailure { kind, message });
    }
}

/// COM の初期化（STA）。成功（S_OK・S_FALSE）したときだけ作り、破棄で `CoUninitialize` と対にする。
struct ComApartment;

impl ComApartment {
    fn init() -> Result<Self, String> {
        // Microsoft Learn の ShellExecuteExW の説明: Shell 拡張が COM を使うため、STA・OLE1 の
        // DDE なしで初期化してから呼ぶ
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        if hr.is_ok() {
            Ok(Self)
        } else {
            Err(format!("COM を初期化できません（{}）", windows::core::Error::from_hresult(hr)))
        }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

/// `ShellExecuteExW` に渡す内容。窓は渡さない（join しない Shell スレッドが、終了で破棄された
/// ビューア窓を使わないため）。メッセージループのない、すぐ終わるスレッドから呼ぶので
/// `SEE_MASK_NOASYNC` が必須（Microsoft Learn の SHELLEXECUTEINFOW の説明）。Shell 自身の
/// エラー画面は出さず（`SEE_MASK_FLAG_NO_UI`。セキュリティの確認は対象外）、失敗はアプリの
/// 通知に集める。ユーザーの操作による起動なので `SEE_MASK_FLAG_LOG_USAGE`。
fn shell_execute_info(file: &[u16]) -> SHELLEXECUTEINFOW {
    SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_LOG_USAGE | SEE_MASK_FLAG_NO_UI,
        lpFile: PCWSTR(file.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    }
}

fn open_with_association(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    let _com = ComApartment::init()?;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut info = shell_execute_info(&wide);
    // OS のメッセージだけで理由が分かる（「関連付けられたアプリがありません」など。見出しと重ねない）
    unsafe { ShellExecuteExW(&mut info) }.map_err(|e| e.to_string())
}

/// `path` を選んだ状態で、そのフォルダをエクスプローラーで開く（Microsoft Learn の
/// SHOpenFolderAndSelectItems: 呼ぶ前に COM の初期化が要る。`cidl` が 0 なら、`pidlFolder` の項目の親の
/// フォルダを開いてその項目を選ぶ）。
fn reveal_in_folder(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    let _com = ComApartment::init()?;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let pidl = unsafe { ILCreateFromPathW(PCWSTR(wide.as_ptr())) };
    if pidl.is_null() {
        return Err("ファイルの場所を取得できません".to_string());
    }
    let result = unsafe { SHOpenFolderAndSelectItems(pidl, None, 0) }.map_err(|e| e.to_string());
    unsafe { ILFree(Some(pidl)) };
    result
}

/// 「関連付けで開く」の一時フォルダ（`%TEMP%\CLCLR`）。
pub fn external_open_dir() -> PathBuf {
    std::env::temp_dir().join("CLCLR")
}

/// `dir` に `{id}-{uuid}.{extension}` を新しく作って書く。既存のファイルは変えない（`create_new`）。
fn write_new_file(dir: &Path, id: Uuid, extension: &str, data: &[u8]) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("一時フォルダを作れません（{e}）"))?;
    let path = dir.join(format!("{id}-{}.{extension}", Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| format!("一時ファイルを作れません（{e}）"))?;
    file.write_all(data).map_err(|e| format!("一時ファイルへ書けません（{e}）"))?;
    Ok(path)
}

/// 操作スレッドを起こす。依頼の送り手がすべて無くなると終わる。join はしない（モジュールの説明）。
pub fn spawn(
    core: Core,
    config: Arc<RwLock<Config>>,
    port: Arc<ClipboardPort>,
    sink: FailureSink,
    requests: Receiver<Action>,
) -> std::io::Result<JoinHandle<()>> {
    let worker = Worker::new(core, config, port, sink, SystemEffects, external_open_dir());
    thread::Builder::new().name("clclr-actions".to_string()).spawn(move || worker.run(requests))
}

/// 操作を止めた理由。
enum Stop {
    /// 受け付けが閉じている（終了処理が始まった）。知らせない
    Closing,
    Failed(String),
    /// 項目は消したが履歴のファイルを書けなかった（`ActionKind::SaveHistory` として知らせる）
    IndexNotSaved(String),
}

impl From<OpError> for Stop {
    fn from(e: OpError) -> Self {
        match e {
            OpError::Closing => Self::Closing,
            OpError::IndexNotSaved(e) => Self::IndexNotSaved(format!("項目は消しました。あとで書き直します（{e}）")),
            e => Self::Failed(e.to_string()),
        }
    }
}

struct Worker<E: Effects> {
    core: Core,
    config: Arc<RwLock<Config>>,
    /// クリップボードを開く窓と、抑止する変更番号（`ClipboardWatcher::port`）
    port: Arc<ClipboardPort>,
    sink: FailureSink,
    effects: E,
    temp_dir: PathBuf,
    slots: Arc<ShellSlots>,
}

impl<E: Effects> Worker<E> {
    fn new(
        core: Core,
        config: Arc<RwLock<Config>>,
        port: Arc<ClipboardPort>,
        sink: FailureSink,
        effects: E,
        temp_dir: PathBuf,
    ) -> Self {
        Self { core, config, port, sink, effects, temp_dir, slots: Arc::default() }
    }

    fn run(self, requests: Receiver<Action>) {
        self.effects.clean_temp(&self.temp_dir);
        for action in requests.iter() {
            self.handle(action);
        }
    }

    fn handle(&self, action: Action) {
        let kind = action.kind();
        let result = match action {
            Action::Send { id, pinned } => self.send(id, pinned),
            Action::Transform { id, pinned, transform } => self.transform(id, pinned, transform),
            Action::OpenImage { id, pinned } => self.export_image(id, pinned, ShellAction::Open),
            Action::OpenImageLocation { id, pinned } => self.export_image(id, pinned, ShellAction::Reveal),
            Action::Pin { id, to } => self.pin(id, to),
            Action::MovePinned { id, to } => self.move_pinned(id, to),
            Action::ReorderPinned { id, direction } => self.reorder_pinned(id, direction),
            Action::CreateFolder { parent, title } => self.create_folder(parent, &title),
            Action::RenameFolder { id, title } => self.rename_folder(id, &title),
            Action::RenamePinned { id, title } => self.rename_pinned(id, &title),
            Action::Delete { id, pinned } => self.delete(id, pinned),
            Action::ClearHistory => self.clear_history(),
            Action::ClearClipboard => self.clear_clipboard(),
            Action::Trim => self.trim(),
            Action::CheckData => self.check_data(),
            Action::CleanData(plan) => self.clean_data(&plan),
        };
        match result {
            Err(Stop::Failed(message)) => self.sink.report(ActionFailure { kind, message }),
            Err(Stop::IndexNotSaved(message)) => self.sink.report(ActionFailure { kind: ActionKind::SaveHistory, message }),
            Ok(()) | Err(Stop::Closing) => {}
        }
    }

    /// 送る: ピン留めなら日付の変換を通し、`delete_on_send` なら自分の
    /// 書き込みを履歴に積まない。
    fn send(&self, id: Uuid, pinned: bool) -> Result<(), Stop> {
        let ticket = self.core.admit()?;
        let entry = self.core.load_for_send_admitted(&ticket, id, pinned)?;
        let (delete_on_send, text_cfg) = {
            let cfg = self.config.read().unwrap_or_else(|p| p.into_inner());
            (cfg.history.delete_on_send, cfg.tools.text.clone())
        };
        let entry = if pinned { convert_entry_date(entry, &text_cfg) } else { entry };
        self.effects.write_send(&self.port, &entry.formats, delete_on_send).map_err(Stop::Failed)?;
        drop(ticket);
        Ok(())
    }

    /// テキスト変換して送る: 元の項目は変えず、抑止しない。テキストが無ければ何もしない。
    fn transform(&self, id: Uuid, pinned: bool, transform: TextTransform) -> Result<(), Stop> {
        let ticket = self.core.admit()?;
        let entry = self.core.load_for_send_admitted(&ticket, id, pinned)?;
        let Some(text) = entry.formats.iter().find(|f| f.format_name == "CF_UNICODETEXT") else {
            return Ok(());
        };
        let cfg = self.config.read().unwrap_or_else(|p| p.into_inner()).tools.text.clone();
        let converted = transform.apply(&utf16_text(&text.data), &cfg);
        let formats =
            [Format { format_name: "CF_UNICODETEXT".to_string(), format_id: 13, data: utf16_bytes(&converted) }];
        self.effects.write_text(&self.port, &formats).map_err(Stop::Failed)?;
        drop(ticket);
        Ok(())
    }

    /// 履歴の項目をピン留めに複製し、ビューアを起こして一覧に反映させる。対象・入れる先が無い
    /// ときも知らせる（したかったことができていない。知らせないのは削除だけ）。
    fn pin(&self, id: Uuid, to: Option<Uuid>) -> Result<(), Stop> {
        self.core.pin(id, to)?;
        self.sink.wake();
        Ok(())
    }

    /// ピン留めの項目を移し、ビューアを起こして一覧に反映させる。対象・入れる先が無いときも知らせる。
    fn move_pinned(&self, id: Uuid, to: Option<Uuid>) -> Result<(), Stop> {
        self.core.move_pinned(id, to)?;
        self.sink.wake();
        Ok(())
    }

    /// ピン留めの項目・フォルダを並べ替え、ビューアを起こして一覧・ツリーに反映させる。対象が無いとき
    /// （削除と入れ違った）・保存できないときは知らせる。
    fn reorder_pinned(&self, id: Uuid, direction: Direction) -> Result<(), Stop> {
        self.core.reorder_pinned(id, direction)?;
        self.sink.wake();
        Ok(())
    }

    /// フォルダを作り、ビューアを起こしてツリーに反映させる（保存が済んでからツリーに出る）。
    /// 親が無い・同じ名前がある・名前が空のときは知らせる。
    fn create_folder(&self, parent: Option<Uuid>, title: &str) -> Result<(), Stop> {
        self.core.create_folder(parent, title)?;
        self.sink.wake();
        Ok(())
    }

    /// フォルダの名前を変え、ビューアを起こしてツリーに反映させる（保存が済んでから新しい名前が出る）。
    /// フォルダが無い・同じ名前がある・名前が空のときは知らせる。
    fn rename_folder(&self, id: Uuid, title: &str) -> Result<(), Stop> {
        self.core.rename_folder(id, title)?;
        self.sink.wake();
        Ok(())
    }

    /// ピン留めのアイテムの名前を変え、ビューアを起こして一覧に反映させる（保存が済んでから新しい名前が出る）。
    /// アイテムが無い（削除と入れ違った）・保存できないときは知らせる。
    fn rename_pinned(&self, id: Uuid, title: &str) -> Result<(), Stop> {
        self.core.rename_pinned_item(id, title)?;
        self.sink.wake();
        Ok(())
    }

    /// 項目を消し、ビューアを起こして一覧に反映させる。すでに無い（押し出し・別の削除と
    /// 入れ違った）ときは、消したかった状態になっているので知らせない。
    fn delete(&self, id: Uuid, pinned: bool) -> Result<(), Stop> {
        let result = if pinned { self.core.delete_pinned(id) } else { self.core.delete_history(id) };
        // 項目は消えたが履歴のファイルへ書けなかった（`IndexNotSaved`）ときも、保存の失敗（`ActionKind::SaveHistory`）
        // として知らせる。知らせる処理がビューアを起こし、起きたビューアが一覧を作り直す
        match result {
            Ok(()) | Err(OpError::NotFound) => {}
            Err(e) => return Err(e.into()),
        }
        self.sink.wake();
        Ok(())
    }

    /// 履歴を全部消し、ビューアを起こして一覧に反映させる。
    fn clear_history(&self) -> Result<(), Stop> {
        self.core.clear_history()?;
        self.sink.wake();
        Ok(())
    }

    /// 設定の保持件数へ切り詰め、ビューアを起こして一覧に反映させる。失敗は「設定の反映」の
    /// 失敗として知らせる（`Action::kind`）。
    fn trim(&self) -> Result<(), Stop> {
        self.core.trim().map_err(|e| match Stop::from(e) {
            Stop::Failed(message) => Stop::Failed(format!("保持件数の切り詰め: {message}")),
            stop => stop,
        })?;
        self.sink.wake();
        Ok(())
    }

    /// データのチェック。結果をビューアへ知らせる（見つけたものを消すかは、ビューアが利用者に聞く）。
    fn check_data(&self) -> Result<(), Stop> {
        let report = self.core.check_data()?;
        self.sink.post(Notice::CheckReport(report));
        Ok(())
    }

    /// チェックの結果のうち、今も見つかるものを消し、結果をビューアへ知らせる（一覧も作り直させる）。
    fn clean_data(&self, plan: &DataReport) -> Result<(), Stop> {
        let result = self.core.clean_data(plan)?;
        self.sink.post(Notice::CleanResult(result));
        Ok(())
    }

    /// クリップボードを空にする。送ると同じく、受け付けの登録の下で行う（終了処理が受け付けを
    /// 閉じた後はクリップボードに触らない）。空になった変更は形式がないので、履歴には積まれない。
    fn clear_clipboard(&self) -> Result<(), Stop> {
        let ticket = self.core.admit()?;
        self.effects.clear_clipboard(&self.port).map_err(Stop::Failed)?;
        drop(ticket);
        Ok(())
    }

    /// 画像を一時フォルダへ書き出し、関連付けで開く・ファイルを選んでフォルダを開く。WebP で保存してある
    /// 画像はその WebP をそのまま（`.webp`）、ほか（元の DIB のままの blob、メモリだけの画像）は BMP にして
    /// 書く。編集結果は取り込まない。画像が
    /// 無ければ何もしない。受け付けの登録は Shell スレッドの生成が戻るまで持つ（終了処理が受け付けを閉じた後に、
    /// 新しく起動を始めない）。Shell の起動そのものは受け付けの外。
    fn export_image(&self, id: Uuid, pinned: bool, action: ShellAction) -> Result<(), Stop> {
        let ticket = self.core.admit()?;
        let Some(image) = self.core.load_image_admitted(&ticket, id, pinned)? else {
            return Ok(());
        };
        // 枠を取ってから書く（空きが無いのに一時ファイルを書いて残さない）
        let slot = self.slots.try_acquire().ok_or_else(|| {
            Stop::Failed(format!("ほかの画像を開いている途中です（同時に開けるのは {MAX_SHELL_LAUNCHES} 件まで）"))
        })?;
        let path = match image {
            ImageData::Webp(webp) => write_new_file(&self.temp_dir, id, "webp", &webp),
            ImageData::Dib(dib) => {
                let bmp = crate::dib::dib_to_bmp_file(&dib).map_err(|e| Stop::Failed(e.to_string()))?;
                write_new_file(&self.temp_dir, id, "bmp", &bmp)
            }
        }
        .map_err(Stop::Failed)?;
        self.effects.launch(path, action, slot, self.sink.clone()).map_err(Stop::Failed)?;
        drop(ticket);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Entry;
    use crate::ops::tests::{blob_path, front_id, pinned_item, temp_core, text_entry};
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::time::Duration;

    /// 止める地点（`entered` に届き、`release` へ送ると進む）。
    struct Gate {
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    fn gate() -> (Arc<Gate>, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        (Arc::new(Gate { entered: entered_tx, release: Mutex::new(release_rx) }), entered_rx, release_tx)
    }

    impl Gate {
        fn pass(&self) {
            let _ = self.entered.send(());
            let _ = self.release.lock().unwrap().recv_timeout(Duration::from_secs(10));
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Send { formats: Vec<(String, Vec<u8>)>, should_suppress: bool },
        Text(Vec<(String, Vec<u8>)>),
        Clean,
        Launch(PathBuf),
        /// ファイルを選んでフォルダを開く
        Reveal(PathBuf),
        ClearClipboard,
    }

    /// 呼ばれた副作用を記録する偽物。`write_gate`・`launch_gate` があればそこで止まる。
    #[derive(Clone, Default)]
    struct Fake {
        calls: Arc<Mutex<Vec<Call>>>,
        write_gate: Option<Arc<Gate>>,
        launch_gate: Option<Arc<Gate>>,
        launch_fails: bool,
        /// 書き込みを本物の排他・抑止の制御に通す（実際のクリップボードを開くが、書かない）
        real_lock: bool,
    }

    impl Fake {
        fn calls(&self) -> std::sync::MutexGuard<'_, Vec<Call>> {
            self.calls.lock().unwrap()
        }
    }

    impl Effects for Fake {
        fn write_send(&self, port: &ClipboardPort, formats: &[Format], should_suppress: bool) -> Result<(), String> {
            let record = || {
                if let Some(gate) = &self.write_gate {
                    gate.pass();
                }
                self.calls().push(Call::Send { formats: plain(formats), should_suppress });
            };
            if self.real_lock {
                crate::clipboard::with_open_port(port, should_suppress, |_| {
                    record();
                    Ok(())
                })
                .map_err(|e| e.to_string())
            } else {
                record();
                Ok(())
            }
        }

        fn write_text(&self, _port: &ClipboardPort, formats: &[Format]) -> Result<(), String> {
            self.calls().push(Call::Text(plain(formats)));
            Ok(())
        }

        fn clear_clipboard(&self, _port: &ClipboardPort) -> Result<(), String> {
            self.calls().push(Call::ClearClipboard);
            Ok(())
        }

        fn clean_temp(&self, _dir: &Path) {
            self.calls().push(Call::Clean);
        }

        fn launch(&self, path: PathBuf, action: ShellAction, slot: ShellSlot, _sink: FailureSink) -> Result<(), String> {
            if let Some(gate) = &self.launch_gate {
                gate.pass();
            }
            if self.launch_fails {
                drop(slot);
                return Err("生成できない（テスト）".to_string());
            }
            self.calls().push(match action {
                ShellAction::Open => Call::Launch(path),
                ShellAction::Reveal => Call::Reveal(path),
            });
            // 起動は終わったものとして枠を返す
            drop(slot);
            Ok(())
        }
    }

    struct Harness {
        dir: PathBuf,
        core: Core,
        fake: Fake,
        sink: FailureSink,
        /// 通知先がビューアを起こした回数
        wakes: Arc<std::sync::atomic::AtomicUsize>,
        /// クリップボードを開く窓（既定は開けない port。本物の排他を通すテストは `with_port` で渡す）
        port: Arc<ClipboardPort>,
        temp: PathBuf,
    }

    impl Harness {
        fn new(fake: Fake) -> Self {
            let (dir, core) = temp_core(Config::default());
            let temp = dir.join("open-temp");
            let wakes: Arc<std::sync::atomic::AtomicUsize> = Arc::default();
            let sink = {
                let wakes = Arc::clone(&wakes);
                FailureSink::new(move || {
                    wakes.fetch_add(1, Ordering::SeqCst);
                })
            };
            Self { dir, core, fake, sink, wakes, port: Arc::new(ClipboardPort::unopenable()), temp }
        }

        fn wakes(&self) -> usize {
            self.wakes.load(Ordering::SeqCst)
        }

        fn worker(&self) -> Worker<Fake> {
            self.worker_with(Config::default())
        }

        fn worker_with(&self, config: Config) -> Worker<Fake> {
            let config = Arc::new(RwLock::new(config));
            Worker::new(
                self.core.clone(),
                config,
                Arc::clone(&self.port),
                self.sink.clone(),
                self.fake.clone(),
                self.temp.clone(),
            )
        }

        /// 1件だけ処理する（掃除を含めない）。
        fn handle(&self, action: Action) {
            self.worker().handle(action);
        }

        /// 積まれた失敗（失敗以外の知らせは捨てる。見るテストは `notices`）。
        fn failures(&self) -> Vec<ActionFailure> {
            failures_of(self.sink.drain())
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn failures_of(notices: Vec<Notice>) -> Vec<ActionFailure> {
        notices
            .into_iter()
            .filter_map(|n| match n {
                Notice::Failure(f) => Some(f),
                _ => None,
            })
            .collect()
    }

    fn plain(formats: &[Format]) -> Vec<(String, Vec<u8>)> {
        formats.iter().map(|f| (f.format_name.clone(), f.data.clone())).collect()
    }

    fn text_of(formats: &[(String, Vec<u8>)]) -> String {
        utf16_text(&formats.iter().find(|(name, _)| name == "CF_UNICODETEXT").unwrap().1)
    }

    /// 2x2 の 32bpp DIB（BITMAPINFOHEADER + 画素）。
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
        v.extend([0x10u8; 16]);
        Entry::new(vec![Format { format_name: "CF_DIB".to_string(), format_id: 8, data: v }])
    }

    #[test]
    fn send_writes_history_entry_with_suppress_settings() {
        let h = Harness::new(Fake::default());
        h.core.capture(text_entry("送る %d")).unwrap();
        h.handle(Action::Send { id: front_id(&h.core), pinned: false });
        let calls = h.fake.calls();
        let [Call::Send { formats, should_suppress }] = calls.as_slice() else {
            panic!("{calls:?}");
        };
        // 履歴から送るときは日付を変換しない（既定の設定: delete_on_send が有効）
        assert_eq!(text_of(formats), "送る %d");
        assert!(*should_suppress);
        assert!(h.failures().is_empty());
    }

    /// ピン留めから送るときは、設定（`convert_date_on_send`。既定は false）が有効なら日付を変換する。
    #[test]
    fn send_from_pinned_converts_date_only_when_enabled() {
        let h = Harness::new(Fake::default());
        h.core.capture(text_entry("日付 %d")).unwrap();
        h.core.pin(front_id(&h.core), None).unwrap();
        let id = pinned_item(&h.core, 0).id;
        h.handle(Action::Send { id, pinned: true });
        let mut enabled = Config::default();
        enabled.tools.text.convert_date_on_send = true;
        h.worker_with(enabled).handle(Action::Send { id, pinned: true });
        let calls = h.fake.calls();
        let [Call::Send { formats: off, .. }, Call::Send { formats: on, .. }] = calls.as_slice() else {
            panic!("{calls:?}")
        };
        assert_eq!(text_of(off), "日付 %d");
        let text = text_of(on);
        assert!(text.starts_with("日付 ") && !text.contains("%d"), "{text}");
    }

    #[test]
    fn send_with_missing_blob_reports_failure_and_writes_nothing() {
        let h = Harness::new(Fake::default());
        h.core.capture(text_entry("消える")).unwrap();
        let meta = h.core.read(|s| s.history.front().unwrap().meta.clone()).unwrap();
        std::fs::remove_file(blob_path(&h.dir, &meta)).unwrap();
        h.handle(Action::Send { id: meta.id, pinned: false });
        assert!(h.fake.calls().is_empty());
        let failures = h.failures();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].kind, ActionKind::Send);
        // 知らせる文は「{見出し}: {理由}」
        assert_eq!(
            failures[0].to_string(),
            "クリップボードへ送れませんでした: 項目のデータ（CF_UNICODETEXT）が見つからないか、壊れています"
        );
    }

    #[test]
    fn transform_writes_converted_text_without_changing_entry() {
        let h = Harness::new(Fake::default());
        h.core.capture(text_entry("abc")).unwrap();
        let id = front_id(&h.core);
        h.handle(Action::Transform { id, pinned: false, transform: TextTransform::ToUpper });
        let calls = h.fake.calls();
        let [Call::Text(formats)] = calls.as_slice() else { panic!("{calls:?}") };
        assert_eq!(text_of(formats), "ABC");
        assert_eq!(h.core.load_for_send(id, false).map(|e| text_of(&plain(&e.formats))).unwrap(), "abc");
    }

    #[test]
    fn transform_or_open_without_matching_format_does_nothing() {
        let h = Harness::new(Fake::default());
        h.core.capture(dib_entry()).unwrap();
        let id = front_id(&h.core);
        h.handle(Action::Transform { id, pinned: false, transform: TextTransform::ToUpper });
        h.core.capture(text_entry("文字だけ")).unwrap();
        h.handle(Action::OpenImage { id: front_id(&h.core), pinned: false });
        assert!(h.fake.calls().is_empty());
        assert!(h.failures().is_empty());
    }

    /// 書き出したファイルの一覧（開き方、パス）。
    fn exported(h: &Harness) -> Vec<(ShellAction, PathBuf)> {
        h.fake
            .calls()
            .iter()
            .map(|c| match c {
                Call::Launch(p) => (ShellAction::Open, p.clone()),
                Call::Reveal(p) => (ShellAction::Reveal, p.clone()),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    /// 保存してある画像（WebP の blob）は、関連付けで開く・書き出してフォルダで表示のどちらでも、その WebP を
    /// 変換せずに `.webp` へ書き出す。ピン留めの画像も同じ。毎回新しいファイルを作る。
    #[test]
    fn open_and_reveal_export_saved_webp_as_is() {
        let h = Harness::new(Fake::default());
        h.core.capture(dib_entry()).unwrap();
        let id = front_id(&h.core);
        h.core.pin(id, None).unwrap();
        let pinned_id = pinned_item(&h.core, 0).id;
        let blob = |meta: crate::storage::EntryMeta| std::fs::read(blob_path(&h.dir, &meta)).unwrap();
        let saved = blob(h.core.read(|s| s.history.front().unwrap().meta.clone()).unwrap());
        let pinned_saved = blob(pinned_item(&h.core, 0));
        h.handle(Action::OpenImage { id, pinned: false });
        h.handle(Action::OpenImage { id, pinned: false });
        h.handle(Action::OpenImageLocation { id, pinned: false });
        h.handle(Action::OpenImageLocation { id: pinned_id, pinned: true });
        let files = exported(&h);
        let actions: Vec<ShellAction> = files.iter().map(|(a, _)| *a).collect();
        assert_eq!(actions, [ShellAction::Open, ShellAction::Open, ShellAction::Reveal, ShellAction::Reveal]);
        assert_ne!(files[0].1, files[1].1, "同じファイルに書いた");
        for (i, (_, p)) in files.iter().enumerate() {
            assert_eq!(p.extension().unwrap(), "webp");
            let expected = if i == 3 { &pinned_saved } else { &saved };
            assert_eq!(&std::fs::read(p).unwrap(), expected, "保存してある WebP と違う");
            let owner = if i == 3 { pinned_id } else { id };
            assert!(p.file_name().unwrap().to_string_lossy().starts_with(&owner.to_string()));
        }
        assert!(h.failures().is_empty());
    }

    /// メモリだけに持つ画像（完全メモリモード）は BMP にして書き出す。WebP の blob が壊れていたら書き出さずに
    /// 知らせる。
    #[test]
    fn memory_image_is_exported_as_bmp_and_broken_webp_is_reported() {
        let h = Harness::new(Fake::default());
        let worker = h.worker_with(crate::ops::tests::memory_only_config());
        let (dir, memory_core) = temp_core(crate::ops::tests::memory_only_config());
        memory_core.capture(dib_entry()).unwrap();
        let memory_worker = Worker::new(
            memory_core.clone(),
            Arc::new(RwLock::new(crate::ops::tests::memory_only_config())),
            Arc::clone(&h.port),
            h.sink.clone(),
            h.fake.clone(),
            h.temp.clone(),
        );
        memory_worker.handle(Action::OpenImageLocation { id: front_id(&memory_core), pinned: false });
        let files = exported(&h);
        let [(ShellAction::Reveal, p)] = files.as_slice() else { panic!("{files:?}") };
        assert_eq!(p.extension().unwrap(), "bmp");
        assert_eq!(&std::fs::read(p).unwrap()[..2], b"BM");
        let _ = std::fs::remove_dir_all(dir);

        h.core.capture(dib_entry()).unwrap();
        let meta = h.core.read(|s| s.history.front().unwrap().meta.clone()).unwrap();
        std::fs::write(blob_path(&h.dir, &meta), b"not a webp").unwrap();
        worker.handle(Action::OpenImage { id: meta.id, pinned: false });
        assert_eq!(exported(&h).len(), 1, "壊れた WebP を書き出した");
        let failures = h.failures();
        assert_eq!(failures.iter().map(|f| f.kind).collect::<Vec<_>>(), [ActionKind::OpenImage]);
    }

    #[test]
    fn write_new_file_never_overwrites_existing_file() {
        let dir = std::env::temp_dir().join(format!("clclr-bmp-test-{}", Uuid::new_v4()));
        let id = Uuid::new_v4();
        let first = write_new_file(&dir, id, "bmp", b"first").unwrap();
        // 同じ名前を作ろうとしても既存のファイルは変わらない（create_new）
        let err = std::fs::OpenOptions::new().write(true).create_new(true).open(&first);
        assert!(err.is_err());
        let second = write_new_file(&dir, id, "bmp", b"second").unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read(&first).unwrap(), b"first");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn requests_after_shutdown_do_nothing() {
        let h = Harness::new(Fake::default());
        h.core.capture(text_entry("終了後")).unwrap();
        let id = front_id(&h.core);
        h.core.shutdown(None).unwrap();
        h.handle(Action::Send { id, pinned: false });
        h.handle(Action::Transform { id, pinned: false, transform: TextTransform::ToUpper });
        h.handle(Action::OpenImage { id, pinned: false });
        assert!(h.fake.calls().is_empty());
        assert!(h.failures().is_empty(), "終了中は知らせない");
    }

    /// 書き込みの途中（本物の排他と抑止の制御の中）で止めて通常の終了を始めると、終了処理は
    /// 書き込みが終わるまで最後の保存へ進まない。
    #[test]
    fn shutdown_waits_for_send_in_progress() {
        // 監視の窓（クリップボードを開く窓）を作るので、クリップボード用 → GUI 用の順にロックを取る。
        // ローカル変数は逆の順に破棄されるので、監視（窓・スレッドの終わりの待ち）はロックより先
        let _clipboard = crate::clipboard::test_support::lock_clipboard_tests();
        let _gui = crate::tray::lock_gui_resource_tests();
        let watcher = crate::clipboard::test_support::test_watcher();
        let (write_gate, entered, release) = gate();
        let mut h = Harness::new(Fake { write_gate: Some(write_gate), real_lock: true, ..Fake::default() });
        h.port = watcher.port();
        h.core.capture(text_entry("書いている途中")).unwrap();
        let id = front_id(&h.core);
        let worker = h.worker();
        let sender = thread::spawn(move || worker.handle(Action::Send { id, pinned: false }));
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let core = h.core.clone();
        let shutdown = thread::spawn(move || core.shutdown(None));
        thread::sleep(Duration::from_millis(100));
        assert!(!shutdown.is_finished(), "書き込みの途中で終了処理が進んだ");
        release.send(()).unwrap();
        sender.join().unwrap();
        shutdown.join().unwrap().unwrap();
        assert_eq!(h.fake.calls().len(), 1);
        // 書き込みの本体は差し替えて何も書かないので、閉じた後の番号は今の番号
        let now = unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() };
        assert_ne!(h.port.suppressed_seq(), 0, "抑止する番号を記録していない");
        assert_eq!(h.port.suppressed_seq(), now);
    }

    /// Shell スレッドの生成の直前で止めて終了を始めると、終了処理は生成が終わるまで待つ。
    /// 終了処理の後に届いた「開く」は生成しない。
    #[test]
    fn open_holds_admission_until_launch_is_handed_off() {
        let (launch_gate, entered, release) = gate();
        let h = Harness::new(Fake { launch_gate: Some(launch_gate), ..Fake::default() });
        h.core.capture(dib_entry()).unwrap();
        let id = front_id(&h.core);
        let worker = h.worker();
        let opener = thread::spawn(move || worker.handle(Action::OpenImage { id, pinned: false }));
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let core = h.core.clone();
        let shutdown = thread::spawn(move || core.shutdown(None));
        thread::sleep(Duration::from_millis(100));
        assert!(!shutdown.is_finished(), "引き渡しの前に終了処理が進んだ");
        release.send(()).unwrap();
        opener.join().unwrap();
        shutdown.join().unwrap().unwrap();
        assert_eq!(h.fake.calls().len(), 1);
        // 終了の後の依頼（止める地点は通らない）
        h.handle(Action::OpenImage { id, pinned: false });
        assert_eq!(h.fake.calls().len(), 1);
    }

    #[test]
    fn full_shell_slots_fail_immediately_and_slots_come_back() {
        let h = Harness::new(Fake::default());
        h.core.capture(dib_entry()).unwrap();
        let id = front_id(&h.core);
        let worker = h.worker();
        let held: Vec<ShellSlot> = (0..MAX_SHELL_LAUNCHES).map(|_| worker.slots.try_acquire().unwrap()).collect();
        worker.handle(Action::OpenImage { id, pinned: false });
        assert!(h.fake.calls().is_empty());
        assert_eq!(h.failures().len(), 1);
        // 空きが無いときは一時ファイルを書かない
        let written = std::fs::read_dir(&h.temp).map(|d| d.count()).unwrap_or(0);
        assert_eq!(written, 0, "空きが無いのに一時ファイルを書いた");
        drop(held);
        assert_eq!(*worker.slots.in_use.lock().unwrap(), 0);
        worker.handle(Action::OpenImage { id, pinned: false });
        assert_eq!(h.fake.calls().len(), 1);
        assert_eq!(*worker.slots.in_use.lock().unwrap(), 0);
    }

    #[test]
    fn launch_failure_is_reported_and_returns_slot() {
        let h = Harness::new(Fake { launch_fails: true, ..Fake::default() });
        h.core.capture(dib_entry()).unwrap();
        let worker = h.worker();
        worker.handle(Action::OpenImage { id: front_id(&h.core), pinned: false });
        assert_eq!(h.failures().len(), 1);
        assert_eq!(*worker.slots.in_use.lock().unwrap(), 0);
    }

    /// Shell スレッドの中の失敗（COM の初期化・起動）は通知され、枠は返る。
    #[test]
    fn shell_thread_failure_is_reported_and_returns_slot() {
        let sink = FailureSink::new(|| {});
        let slots: Arc<ShellSlots> = Arc::default();
        let slot = slots.try_acquire().unwrap();
        shell_thread_body(Path::new("x.bmp"), ActionKind::OpenImage, slot, &sink, |_| Err("COM を初期化できません（テスト）".to_string()));
        assert_eq!(sink.drain().len(), 1);
        assert_eq!(*slots.in_use.lock().unwrap(), 0);
        // 書き出してフォルダで表示の失敗は、その種類で知らせる
        let slot = slots.try_acquire().unwrap();
        shell_thread_body(Path::new("x.webp"), ShellAction::Reveal.failure_kind(), slot, &sink, |_| Err("x".to_string()));
        assert_eq!(failures_of(sink.drain()).iter().map(|f| f.kind).collect::<Vec<_>>(), [ActionKind::OpenImageLocation]);
        assert_eq!(*slots.in_use.lock().unwrap(), 0);
    }

    #[test]
    fn shell_execute_info_passes_no_window_and_waits_for_launch() {
        let file: Vec<u16> = "x.bmp\0".encode_utf16().collect();
        let info = shell_execute_info(&file);
        assert!(info.hwnd.is_invalid(), "窓を渡している");
        assert_eq!(info.fMask, SEE_MASK_NOASYNC | SEE_MASK_FLAG_LOG_USAGE | SEE_MASK_FLAG_NO_UI);
        assert!(info.lpVerb.is_null());
    }

    #[test]
    fn worker_cleans_temp_before_first_request() {
        let h = Harness::new(Fake::default());
        h.core.capture(text_entry("掃除の後")).unwrap();
        let (tx, rx) = mpsc::channel();
        tx.send(Action::Send { id: front_id(&h.core), pinned: false }).unwrap();
        drop(tx);
        h.worker().run(rx);
        let calls = h.fake.calls();
        assert!(matches!(calls.as_slice(), [Call::Clean, Call::Send { .. }]), "{calls:?}");
    }

    /// ピン留め・削除は Core の操作を行い、成功したらビューアを起こす（一覧を作り直させる）。
    /// すでに無い項目の削除は知らせない。見つからない項目のピン留めは知らせる。
    #[test]
    fn pin_and_delete_change_items_and_wake_viewer() {
        let h = Harness::new(Fake::default());
        h.core.capture(text_entry("ピン留めと削除")).unwrap();
        let id = front_id(&h.core);
        h.handle(Action::Pin { id, to: None });
        assert_eq!(h.core.read(|s| s.pinned.len()), Some(1));
        assert_eq!(h.wakes(), 1);
        let pinned_id = pinned_item(&h.core, 0).id;
        h.handle(Action::Delete { id, pinned: false });
        assert!(h.core.read(|s| s.history.is_empty()).unwrap());
        assert_eq!(h.wakes(), 2);
        h.handle(Action::Delete { id: pinned_id, pinned: true });
        assert!(h.core.read(|s| s.pinned.is_empty()).unwrap());
        assert_eq!(h.wakes(), 3);
        assert!(h.failures().is_empty());

        h.handle(Action::Delete { id, pinned: false });
        assert!(h.failures().is_empty(), "すでに無い項目の削除を知らせた");
        h.handle(Action::Pin { id, to: None });
        let failures = h.failures();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].kind, ActionKind::Pin);
        assert!(h.fake.calls().is_empty(), "クリップボード・Shell を使った");
    }

    /// 削除・履歴のクリアで、項目は消えたが履歴のファイルへ書けなかったときは、書けなかったことを知らせる
    /// （知らせる処理がビューアを起こし、一覧が作り直される）。
    #[test]
    fn removal_with_unsaved_index_wakes_and_reports() {
        let h = Harness::new(Fake::default());
        h.core.capture(text_entry("一件目")).unwrap();
        h.core.capture(text_entry("二件目")).unwrap();
        let id = front_id(&h.core);
        // `write_atomic` の一時ファイルと同じ名前のフォルダで、history.toml への書き込みを失敗させる
        std::fs::create_dir_all(h.dir.join("history.toml.tmp")).unwrap();
        h.handle(Action::Delete { id, pinned: false });
        assert_eq!(h.core.read(|s| s.history.len()), Some(1));
        assert_eq!(h.wakes(), 1, "知らせでビューアを起こしていない");
        let failures = h.failures();
        assert_eq!(failures.len(), 1, "{failures:?}");
        // 見出しは「削除できませんでした」ではなく保存の失敗（項目は消えているため）
        assert_eq!(failures[0].kind, ActionKind::SaveHistory);
        assert!(failures[0].kind.always_notified(), "操作の失敗の設定で隠れる");
        let text = failures[0].to_string();
        assert!(text.starts_with("履歴のファイルを保存できませんでした: 項目は消しました。"), "{text}");
        h.handle(Action::ClearHistory);
        assert!(h.core.read(|s| s.history.is_empty()).unwrap());
        assert_eq!(h.wakes(), 2);
        assert_eq!(h.failures().len(), 1);
    }

    /// 入れる先付きのピン留めと移動: 成功したらビューアを起こす。入れる先が無い・移す項目が
    /// 無いときは知らせる（知らせないのは削除だけ）。
    #[test]
    fn pin_into_folder_and_move_wake_viewer_and_report_missing_targets() {
        let h = Harness::new(Fake::default());
        h.core.capture(text_entry("入れる")).unwrap();
        let id = front_id(&h.core);
        h.core.create_folder(None, "箱").unwrap();
        let folder = crate::ops::tests::folder_id(&h.core, "箱");
        h.handle(Action::Pin { id, to: Some(folder) });
        assert_eq!(h.wakes(), 1);
        let pinned_id = h.core.read(|s| crate::store::find_folder(&s.pinned, folder).unwrap().children[0].id()).unwrap();
        h.handle(Action::MovePinned { id: pinned_id, to: None });
        assert_eq!(h.wakes(), 2);
        assert_eq!(h.core.read(|s| crate::store::parent_of(&s.pinned, pinned_id)), Some(Some(None)));
        assert!(h.failures().is_empty());

        h.handle(Action::Pin { id, to: Some(Uuid::new_v4()) });
        h.handle(Action::MovePinned { id: pinned_id, to: Some(Uuid::new_v4()) });
        h.handle(Action::MovePinned { id: Uuid::new_v4(), to: None });
        let kinds: Vec<ActionKind> = h.failures().iter().map(|f| f.kind).collect();
        assert_eq!(kinds, [ActionKind::Pin, ActionKind::MovePinned, ActionKind::MovePinned]);
        // 失敗の知らせも起こす（`FailureSink::report`）ので、成功の 2 回と失敗の 3 回
        assert_eq!(h.wakes(), 5);
    }

    /// 並べ替え: 成功（端で動かないときも）はビューアを起こす。項目が無いときは知らせる。
    #[test]
    fn reorder_pinned_wakes_viewer_and_reports_missing_item() {
        let h = Harness::new(Fake::default());
        h.core.capture(text_entry("並べる")).unwrap();
        h.core.pin(front_id(&h.core), None).unwrap();
        h.core.create_folder(None, "箱").unwrap();
        let item = pinned_item(&h.core, 0).id;
        h.handle(Action::ReorderPinned { id: item, direction: Direction::Down });
        assert_eq!(h.wakes(), 1);
        assert_eq!(h.core.read(|s| s.pinned[1].id()), Some(item));
        h.handle(Action::ReorderPinned { id: item, direction: Direction::Down });
        assert_eq!(h.wakes(), 2);
        assert!(h.failures().is_empty());

        h.handle(Action::ReorderPinned { id: Uuid::new_v4(), direction: Direction::Up });
        let kinds: Vec<ActionKind> = h.failures().iter().map(|f| f.kind).collect();
        assert_eq!(kinds, [ActionKind::ReorderPinned]);
    }

    /// フォルダの作成・名前の変更: 成功したらビューアを起こす。同じ名前・無い親・無いフォルダは
    /// 知らせる。
    #[test]
    fn create_and_rename_folder_wake_viewer_and_report_failures() {
        let h = Harness::new(Fake::default());
        h.handle(Action::CreateFolder { parent: None, title: " 箱 ".into() });
        assert_eq!(h.wakes(), 1);
        let folder = crate::ops::tests::folder_id(&h.core, "箱");
        h.handle(Action::RenameFolder { id: folder, title: "棚".into() });
        assert_eq!(h.wakes(), 2);
        assert_eq!(crate::ops::tests::folder_id(&h.core, "棚"), folder);
        assert!(h.failures().is_empty());

        h.handle(Action::CreateFolder { parent: None, title: "棚".into() });
        h.handle(Action::CreateFolder { parent: Some(Uuid::new_v4()), title: "新".into() });
        h.handle(Action::RenameFolder { id: Uuid::new_v4(), title: "新".into() });
        let kinds: Vec<ActionKind> = h.failures().iter().map(|f| f.kind).collect();
        assert_eq!(kinds, [ActionKind::CreateFolder, ActionKind::CreateFolder, ActionKind::RenameFolder]);
    }

    /// データのチェックと削除: 結果は知らせのキューに届いた順に積まれ、ビューアを起こす。失敗（何も
    /// 消さなかった）も知らせる。終了の後は何もしない。
    #[test]
    fn check_and_clean_post_results_in_order() {
        let h = Harness::new(Fake::default());
        let orphan = format!("{}_0.bin", Uuid::new_v4());
        std::fs::write(h.dir.join("blobs").join(&orphan), b"abc").unwrap();
        h.handle(Action::CheckData);
        let notices = h.sink.drain();
        let [Notice::CheckReport(report)] = notices.as_slice() else { panic!("{notices:?}") };
        assert_eq!(report.orphans.len(), 1);
        h.handle(Action::CleanData(report.clone()));
        h.handle(Action::CheckData);
        let notices = h.sink.drain();
        let [Notice::CleanResult(result), Notice::CheckReport(after)] = notices.as_slice() else { panic!("{notices:?}") };
        assert_eq!((result.files_removed, result.bytes_removed), (1, 3));
        assert!(after.is_clean());
        assert_eq!(h.wakes(), 3);

        // ディスクのインデックスにメモリに無い項目があると、削除は失敗として知らせる
        let outside = crate::storage::EntryMeta { id: Uuid::new_v4(), title: None, modified: 0.0, hash: 0, preview: None, formats: vec![] };
        h.core.storage().save_history_index(&[outside]).unwrap();
        h.handle(Action::CleanData(report.clone()));
        let kinds: Vec<ActionKind> = h.failures().iter().map(|f| f.kind).collect();
        assert_eq!(kinds, [ActionKind::CleanData]);
        assert!(ActionKind::CleanData.always_notified() && ActionKind::CheckData.always_notified());

        h.core.shutdown(None).unwrap();
        h.handle(Action::CheckData);
        assert!(h.sink.drain().is_empty(), "終了の後に知らせた");
    }

    /// ピン留めのアイテムの名前の変更: 成功したらビューアを起こす。無いアイテムは知らせる。
    #[test]
    fn rename_pinned_wakes_viewer_and_reports_missing_item() {
        let h = Harness::new(Fake::default());
        h.core.capture(crate::ops::tests::text_entry("本文")).unwrap();
        h.core.pin(crate::ops::tests::front_id(&h.core), None).unwrap();
        let id = crate::ops::tests::pinned_item(&h.core, 0).id;
        h.handle(Action::RenamePinned { id, title: "名前".into() });
        assert_eq!(h.wakes(), 1);
        assert_eq!(crate::ops::tests::pinned_item(&h.core, 0).title.as_deref(), Some("名前"));
        assert!(h.failures().is_empty());

        h.handle(Action::RenamePinned { id: Uuid::new_v4(), title: "x".into() });
        let kinds: Vec<ActionKind> = h.failures().iter().map(|f| f.kind).collect();
        assert_eq!(kinds, [ActionKind::RenamePinned]);
    }

    /// 履歴のクリアは履歴だけを全部消し（ピン留めは残す）、ビューアを起こす。クリップボードの
    /// クリアはクリップボードを空にするだけ。終了処理の後はどちらも何もしない。
    #[test]
    fn clear_history_and_clipboard() {
        let h = Harness::new(Fake::default());
        for text in ["一", "二"] {
            h.core.capture(text_entry(text)).unwrap();
        }
        h.core.pin(front_id(&h.core), None).unwrap();
        h.handle(Action::ClearHistory);
        assert!(h.core.read(|s| s.history.is_empty()).unwrap());
        assert_eq!(h.core.read(|s| s.pinned.len()), Some(1));
        assert_eq!(h.wakes(), 1);
        h.handle(Action::ClearClipboard);
        assert_eq!(h.fake.calls().as_slice(), [Call::ClearClipboard]);
        assert!(h.failures().is_empty());

        h.core.capture(text_entry("終了の前")).unwrap();
        h.core.shutdown(None).unwrap();
        h.handle(Action::ClearHistory);
        h.handle(Action::ClearClipboard);
        assert_eq!(h.core.read(|s| s.history.len()), Some(1), "終了処理の後に消した");
        assert_eq!(h.fake.calls().len(), 1, "終了処理の後にクリップボードに触った");
        assert!(h.failures().is_empty(), "終了中は知らせない");
    }

    /// 閉じた通知先は、成功の起床もしない（破棄した窓へ投稿しない）。
    #[test]
    fn closed_sink_does_not_wake_after_success() {
        let h = Harness::new(Fake::default());
        h.core.capture(text_entry("閉じた後")).unwrap();
        let id = front_id(&h.core);
        h.sink.close();
        h.handle(Action::Pin { id, to: None });
        assert_eq!(h.core.read(|s| s.pinned.len()), Some(1));
        assert_eq!(h.wakes(), 0);
    }

    #[test]
    fn sink_after_close_logs_instead_of_waking() {
        let wakes = Arc::new(Mutex::new(0));
        let sink = {
            let wakes = Arc::clone(&wakes);
            FailureSink::new(move || *wakes.lock().unwrap() += 1)
        };
        let failure = ActionFailure { kind: ActionKind::Send, message: "x".to_string() };
        sink.report(failure.clone());
        assert_eq!(*wakes.lock().unwrap(), 1);
        assert_eq!(sink.close(), vec![Notice::Failure(failure.clone())], "取り出されていない失敗を返していない");
        sink.report(failure);
        assert_eq!(*wakes.lock().unwrap(), 1, "閉じた後に起こした");
        assert!(sink.drain().is_empty());
    }

    /// 起床の投稿の途中で閉じようとしても、閉じるのは投稿が終わるまで待ち、積まれた失敗は
    /// 閉じるときに返る（取りこぼさない）。閉じた後は投稿しない。
    #[test]
    fn sink_close_waits_for_wake_in_progress() {
        let (wake_gate, entered, release) = gate();
        let wakes = Arc::new(Mutex::new(0));
        let sink = {
            let wakes = Arc::clone(&wakes);
            FailureSink::new(move || {
                wake_gate.pass();
                *wakes.lock().unwrap() += 1;
            })
        };
        let reporter = {
            let sink = sink.clone();
            thread::spawn(move || sink.report(ActionFailure { kind: ActionKind::OpenImage, message: "y".to_string() }))
        };
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let closer = {
            let sink = sink.clone();
            thread::spawn(move || sink.close())
        };
        thread::sleep(Duration::from_millis(50));
        assert!(!closer.is_finished(), "投稿の途中で閉じた");
        release.send(()).unwrap();
        reporter.join().unwrap();
        assert_eq!(closer.join().unwrap().len(), 1);
        assert_eq!(*wakes.lock().unwrap(), 1);
    }
}
