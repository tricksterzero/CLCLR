//! クリップボード監視。
//!
//! 隠しメッセージ専用ウィンドウで `WM_CLIPBOARDUPDATE` を受信し、
//! チャネル経由で別スレッド（ワーカー）に通知する。ワーカー側でデバウンス・
//! ウィンドウフィルタ判定・クリップボードのキャプチャを行う。
//! ウィンドウプロシージャは Win32 API 呼び出しを最小限にとどめ、
//! 業務ロジック（デバウンス・フィルタ・キャプチャ）は通常の安全な Rust コードに閉じ込める。

use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::core::{w, Error as WinError, PCWSTR, Result as WinResult};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, CountClipboardFormats, EmptyClipboard,
    EnumClipboardFormats, GetClipboardData, GetClipboardFormatNameW, GetClipboardOwner,
    GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW,
    RemoveClipboardFormatListener, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GWLP_USERDATA, GetClassNameW, GetForegroundWindow, GetMessageW,
    GetWindowLongPtrW, GetWindowTextW, HWND_MESSAGE, MSG, PostQuitMessage,
    RegisterClassW, SetWindowLongPtrW, TranslateMessage, WM_CLIPBOARDUPDATE, WM_DESTROY,
    WM_NCCREATE, WM_USER, WNDCLASSW, WS_OVERLAPPED,
};

use crate::config::{Config, FilterAction};
use crate::data::{Entry, Format};

// --- Error ---

#[derive(Debug)]
pub enum ClipboardError {
    Win32(WinError),
    /// `set_clipboard`に非空の`formats`を渡したのに1形式も書き込めなかった
    /// （`formats`が最初から空の場合はこれに含めない。意図的な空クリップボード化と区別するため）
    AllFormatsFailed,
    /// 空にした後の `SetClipboardData` がすべて失敗した（元の中身は消えている。写しを取って戻すことは
    /// しない）
    AllFormatsFailedAfterEmpty,
}

impl std::fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Win32(e) => write!(f, "クリップボードを使えません（{e}）"),
            Self::AllFormatsFailed => write!(f, "どの形式もクリップボードへ書けませんでした"),
            Self::AllFormatsFailedAfterEmpty => {
                write!(f, "どの形式もクリップボードへ書けませんでした（クリップボードは空になっています）")
            }
        }
    }
}

impl std::error::Error for ClipboardError {}

impl From<WinError> for ClipboardError {
    fn from(e: WinError) -> Self {
        Self::Win32(e)
    }
}

type Result<T> = std::result::Result<T, ClipboardError>;

// --- Format name resolution ---

/// HGLOBAL ではなく GDI オブジェクトハンドルを介するため、汎用キャプチャの対象外。
/// GDI 変換は将来これらの形式を有効化する時に別途実装する。
/// CF_DSP* はプライベート形式に付随する表示用の別表現で、ハンドル種別は対応する
/// 標準形式（CF_BITMAP 等）と同じ。CF_OWNERDISPLAY はデータが NULL のため
/// `GetClipboardData` の Err で自然にスキップされ、ここに含める必要はない。
const GDI_OBJECT_FORMATS: [u32; 7] = [
    2,      // CF_BITMAP
    3,      // CF_METAFILEPICT
    9,      // CF_PALETTE
    14,     // CF_ENHMETAFILE
    0x0082, // CF_DSPBITMAP
    0x0083, // CF_DSPMETAFILEPICT
    0x008E, // CF_DSPENHMETAFILE
];

fn standard_format_name(id: u32) -> Option<&'static str> {
    Some(match id {
        1 => "CF_TEXT",
        2 => "CF_BITMAP",
        3 => "CF_METAFILEPICT",
        4 => "CF_SYLK",
        5 => "CF_DIF",
        6 => "CF_TIFF",
        7 => "CF_OEMTEXT",
        8 => "CF_DIB",
        9 => "CF_PALETTE",
        10 => "CF_PENDATA",
        11 => "CF_RIFF",
        12 => "CF_WAVE",
        13 => "CF_UNICODETEXT",
        14 => "CF_ENHMETAFILE",
        15 => "CF_HDROP",
        16 => "CF_LOCALE",
        17 => "CF_DIBV5",
        _ => return None,
    })
}

fn format_name(id: u32) -> String {
    if let Some(name) = standard_format_name(id) {
        return name.to_string();
    }
    let mut buf = [0u16; 256];
    let len = unsafe { GetClipboardFormatNameW(id, &mut buf) };
    if len > 0 {
        String::from_utf16_lossy(&buf[..len as usize])
    } else {
        format!("0x{id:04X}")
    }
}

// --- RAII guards for Win32 resources ---

/// プロセスの中でクリップボードを開く区間を1つずつに並べるロック（ビューアの送出・ホットキーの
/// 送出・起動時の同期・監視の取り込みが重なっても、形式が混ざらないようにする）。どれも同じ
/// 監視の窓を渡して開くので、プロセス内の並びは OS の排他ではなくこのロックで保証する（同じ窓を
/// 渡した別スレッドの `OpenClipboard` が失敗するかは確かめていない）。書く側は、閉じて変更番号を
/// 記録するまでこのロックを持つ（`ClipboardGuard::close`）。このロックを持ったまま `Core` の
/// 操作を呼ばない。
static CLIPBOARD_LOCK: Mutex<()> = Mutex::new(());

/// 開いているクリップボード。開く前に `CLIPBOARD_LOCK` を取り、`close` か破棄で
/// `CloseClipboard` を呼んでからロックを放す（`Drop::drop` の後にフィールドが破棄される）。
pub(crate) struct ClipboardGuard {
    /// まだ閉じていない（`close` を呼んだら false。破棄で閉じ直さない）
    open: bool,
    _lock: MutexGuard<'static, ()>,
}

/// 他プロセス（クリップボードを監視・同期する常駐アプリ等）は変更の直後に
/// 一瞬（実測0.4〜1.2ms）クリップボードを開くため、その間の`OpenClipboard`は
/// ACCESS_DENIEDで即座に失敗する。短い間隔で再試行する（実測では衝突は
/// 全て1回目の再試行で解消）。
const OPEN_ATTEMPTS: u32 = 10;
const OPEN_RETRY_INTERVAL: Duration = Duration::from_millis(10);

impl ClipboardGuard {
    /// `owner` を渡して開く。窓を渡さない `OpenClipboard(None)` では、開いている間もほかの
    /// プロセスが開いて書けてしまう（実測）。
    fn open(owner: HWND) -> Result<Self> {
        let lock = CLIPBOARD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut attempt = 1;
        loop {
            match unsafe { OpenClipboard(Some(owner)) } {
                Ok(()) => return Ok(Self { open: true, _lock: lock }),
                Err(e) if attempt >= OPEN_ATTEMPTS => return Err(e.into()),
                Err(_) => {
                    attempt += 1;
                    thread::sleep(OPEN_RETRY_INTERVAL);
                }
            }
        }
    }

    /// 閉じて、閉じた後の変更番号を返す。ロックはガードを破棄するまで持つ（呼び出し側は番号を
    /// 記録してからガードを破棄する。`self` を消費すると、記録の前にロックが外れ、監視の照合が
    /// 間に入る）。変更番号は閉じるときにも増えるので、開いたまま読んだ番号は、監視が次に開いて
    /// 読む番号と一致しない（実測）。閉じられなければログに書いて `None`（書いた内容は
    /// 取り消せないので、呼び出し側は結果を変えない）。
    fn close(&mut self) -> Option<u32> {
        self.open = false;
        match unsafe { CloseClipboard() } {
            Ok(()) => Some(unsafe { GetClipboardSequenceNumber() }),
            Err(e) => {
                eprintln!("クリップボードを閉じられません: {e}");
                None
            }
        }
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        if self.open {
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }
}

/// クリップボードを開くときに渡す窓（監視の窓）と、抑止する変更番号。監視が作り
/// （`ClipboardWatcher::port`）、書く側（ホットキー・ビューアの操作・起動時の同期）と監視の
/// 取り込みが共有する。クリップボードを開く処理は、すべてこれを通す。
///
/// 抑止: 自分の書き込みを履歴に入れない設定（`delete_on_send`）のとき、書く側は閉じた後の
/// 変更番号を記録し、監視は開いてから読んだ番号が記録と同じなら取り込まない。記録と照合は
/// どちらも `CLIPBOARD_LOCK` の中で行うので、書いてから記録するまでの間に監視の照合は入らない。
/// 番号は変更ごとに変わるので、後から書かれたほかの内容を飲み込まない。記録は消さない
/// （同じ番号なら同じ中身）。残る危険: 閉じてから番号を読むまでの間に、ほかのプロセスが
/// 書き込みを丸ごと済ませると、その番号を記録し、その内容は取り込まれない。
pub struct ClipboardPort {
    owner: HWND,
    /// 抑止する変更番号。0 は記録なし（`GetClipboardSequenceNumber` は権限が無いと 0 を返す）
    suppressed: AtomicU32,
}

// HWND はハンドル（不透明な識別子）で、参照外しはしない。`OpenClipboard` に渡すだけ
unsafe impl Send for ClipboardPort {}
unsafe impl Sync for ClipboardPort {}

impl ClipboardPort {
    fn new(owner: HWND) -> Self {
        Self { owner, suppressed: AtomicU32::new(0) }
    }

    /// テスト用: 窓を持たない port（クリップボードを開かないテストへ渡す。開くとデバッグビルドで
    /// 止まる）。
    #[cfg(test)]
    pub(crate) fn unopenable() -> Self {
        Self::new(HWND::default())
    }

    pub(crate) fn open(&self) -> Result<ClipboardGuard> {
        debug_assert!(!self.owner.is_invalid(), "窓の無い port でクリップボードを開こうとした");
        ClipboardGuard::open(self.owner)
    }

    /// 閉じた後の変更番号を、抑止する番号として記録する（0・閉じられなかったときは記録しない。
    /// その内容は取り込まれうる。二重に入る側に倒す）。
    fn record(&self, seq: Option<u32>) {
        if let Some(seq) = seq.filter(|&seq| seq != 0) {
            self.suppressed.store(seq, Ordering::SeqCst);
        }
    }

    fn is_suppressed(&self, seq: u32) -> bool {
        seq != 0 && self.suppressed.load(Ordering::SeqCst) == seq
    }

    /// テスト用: 記録している番号。
    #[cfg(test)]
    pub(crate) fn suppressed_seq(&self) -> u32 {
        self.suppressed.load(Ordering::SeqCst)
    }
}

struct GlobalLockGuard {
    handle: HGLOBAL,
    ptr: *mut core::ffi::c_void,
}

impl GlobalLockGuard {
    fn lock(handle: HGLOBAL) -> Option<Self> {
        let ptr = unsafe { GlobalLock(handle) };
        if ptr.is_null() {
            None
        } else {
            Some(Self { handle, ptr })
        }
    }

    fn as_slice(&self, len: usize) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.cast::<u8>(), len) }
    }
}

impl Drop for GlobalLockGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = GlobalUnlock(self.handle);
        }
    }
}

// --- Capture ---

/// クリップボード形式のデータサイズが採用条件を満たすか判定する（Win32呼び出しを
/// 伴わない純粋関数、テスト容易化のため`capture_clipboard`から切り出す）。
/// サイズ0は意味のあるデータを持たないためスキップする（空データのFormatを
/// 履歴に積まない）。
fn should_capture_size(size: usize, limit: u64) -> bool {
    if size == 0 {
        return false;
    }
    limit == 0 || size as u64 <= limit
}

/// 取り込みの1回の結果。
enum Capture {
    /// 変更番号（開いた直後に読んだもの）が抑止する番号と同じ。中身は読んでいない
    Suppressed(u32),
    /// 読んだ（形式が無ければ `None`）
    Read(u32, Option<Entry>),
    /// 取り込む形式の大きさの合計が上限を超えた。中身は写していない
    TooLarge(u32, TooLarge),
}

/// 取り込む形式の大きさの合計（`total`）が、`Config::capture_total_limit`（`limit`）を超えた。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooLarge {
    pub total: u64,
    pub limit: u64,
}

/// 監視のワーカーから知らせること（`ClipboardWatcher::spawn` の `on_problem`）。
#[derive(Debug)]
pub enum WatchProblem {
    /// 読み取りか `on_entry` がパニックし、監視を止めた（止めた結果。失敗したら監視は登録されたまま）
    Panicked(WinResult<()>),
    /// 取り込む形式の大きさの合計が上限を超えたので、何も取り込まなかった
    TooLarge(TooLarge),
}

/// 監視の取り込み: 開いてから変更番号を読み、抑止する番号と同じなら中身を読まない。開いている
/// 間はほかのプロセスが書けない（監視の窓を渡して開くため）ので、読んだ番号と中身は同じ状態の
/// もの。
fn capture_clipboard(config: &Config, port: &ClipboardPort) -> Result<Capture> {
    let guard = port.open()?;
    let seq = unsafe { GetClipboardSequenceNumber() };
    if port.is_suppressed(seq) {
        return Ok(Capture::Suppressed(seq));
    }
    Ok(match read_entry(&guard, config) {
        Ok(entry) => Capture::Read(seq, entry),
        Err(too_large) => Capture::TooLarge(seq, too_large),
    })
}

/// 合計の上限（0は無制限）を超えるか。
fn exceeds_total(total: u64, limit: u64) -> bool {
    limit != 0 && total > limit
}

/// 開いているクリップボード（`guard`）の中身を読む（`capture_clipboard` の本体）。先に取り込む形式と大きさを
/// 集め、合計が上限を超えたら中身を写さずに `Err` を返す（大きな確保そのものをしない）。ハンドルは
/// クリップボードを開いている間は有効で、ほかのプロセスは書き換えられない。
fn read_entry(_guard: &ClipboardGuard, config: &Config) -> std::result::Result<Option<Entry>, TooLarge> {
    let mut picked = Vec::new();
    let mut total = 0u64;
    let mut id = 0u32;
    loop {
        id = unsafe { EnumClipboardFormats(id) };
        if id == 0 {
            break;
        }
        if GDI_OBJECT_FORMATS.contains(&id) {
            continue;
        }

        let name = format_name(id);
        if config.should_capture(&name) == FilterAction::Ignore {
            continue;
        }

        let Ok(handle) = (unsafe { GetClipboardData(id) }) else {
            continue;
        };
        let hglobal = HGLOBAL(handle.0);
        let size = unsafe { GlobalSize(hglobal) };
        if !should_capture_size(size, config.size_limit(&name)) {
            continue;
        }
        total = total.saturating_add(size as u64);
        picked.push((id, name, hglobal, size));
    }

    let limit = config.capture_total_limit;
    if exceeds_total(total, limit) {
        return Err(TooLarge { total, limit });
    }
    let formats: Vec<Format> = picked
        .into_iter()
        .filter_map(|(id, name, hglobal, size)| {
            let guard = GlobalLockGuard::lock(hglobal)?;
            Some(Format { format_name: name, format_id: id, data: guard.as_slice(size).to_vec() })
        })
        .collect();
    Ok((!formats.is_empty()).then(|| Entry::new(formats)))
}

// --- Write to clipboard ---

/// クリップボードを空にする（C版 CLCL の tool_utl「クリップボードのクリア」の統合）。
/// 空への変更通知は形式ゼロになるため、`capture_clipboard`が履歴に積むことはない。
pub fn clear_clipboard(port: &ClipboardPort) -> Result<()> {
    write_marked(port, false, |_| unsafe { EmptyClipboard() }.map_err(Into::into))
}

/// クリップボードが空か（形式が1つも無いか）。起動時同期の分岐に使う。
/// `CountClipboardFormats`はクリップボードを開かずに呼べる。
pub fn is_clipboard_empty() -> bool {
    unsafe { CountClipboardFormats() == 0 }
}

/// エントリの全形式をクリップボードへ書き込む（テキスト変換の結果など、履歴に取り込ませる書き込み）。
/// 自分の書き込みを履歴に入れない（`delete_on_send`設定と対）場合は、これを直接呼ばず
/// `set_clipboard_suppressed`を使う。
///
/// `formats`が非空なのに1形式も書き込めなかった場合は`ClipboardError::AllFormatsFailed`を
/// 返す（クリップボードが空になったのに呼び出し側へ失敗が伝わらない問題の修正）。
/// `formats`が最初から空の場合は意図的な空クリップボード化として`Ok`のまま扱う。
pub fn set_clipboard(port: &ClipboardPort, formats: &[Format]) -> Result<()> {
    write_marked(port, false, |guard| write_formats(guard, formats))
}

/// 開いているクリップボード（`guard`）を空にして、全形式を書き込む（`set_clipboard` の本体）。
/// 形式 ID の解決とメモリの確保を、空にする前に済ませる。1つも用意できなければ空にせずに
/// `AllFormatsFailed` を返す（失敗した送出で、利用者のクリップボードの中身を消さない）。
/// 空にした後の `SetClipboardData` がすべて失敗したときは中身が消えているので、別の
/// `AllFormatsFailedAfterEmpty` で知らせる（用意できたメモリを渡すだけの段階で、失敗は想定しにくい）。
fn write_formats(_guard: &ClipboardGuard, formats: &[Format]) -> Result<()> {
    let prepared: Vec<(u32, HGLOBAL)> = formats.iter().filter_map(|fmt| Some((resolve_format_id(fmt)?, global_copy(&fmt.data)?))).collect();
    if !formats.is_empty() && prepared.is_empty() {
        return Err(ClipboardError::AllFormatsFailed);
    }
    if let Err(e) = unsafe { EmptyClipboard() } {
        for (_, hglobal) in prepared {
            unsafe {
                let _ = GlobalFree(Some(hglobal));
            }
        }
        return Err(e.into());
    }

    let mut succeeded = 0usize;
    for (id, hglobal) in prepared {
        unsafe {
            // 成功後はOSが所有権を持つため解放しない。失敗時のみ自分で解放する
            if SetClipboardData(id, Some(HANDLE(hglobal.0))).is_err() {
                let _ = GlobalFree(Some(hglobal));
            } else {
                succeeded += 1;
            }
        }
    }
    if !formats.is_empty() && succeeded == 0 {
        return Err(ClipboardError::AllFormatsFailedAfterEmpty);
    }
    Ok(())
}

/// `data` を写した `GMEM_MOVEABLE` のメモリ（`SetClipboardData` に渡すメモリの API の要件）。確保・ロックが
/// できなければ None（確保したものは解放する）。
fn global_copy(data: &[u8]) -> Option<HGLOBAL> {
    unsafe {
        let hglobal = GlobalAlloc(GMEM_MOVEABLE, data.len().max(1)).ok()?;
        let ptr = GlobalLock(hglobal);
        if ptr.is_null() {
            let _ = GlobalFree(Some(hglobal));
            return None;
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr.cast::<u8>(), data.len());
        let _ = GlobalUnlock(hglobal);
        Some(hglobal)
    }
}

/// `set_clipboard`に、自分の書き込みを履歴に入れない抑止を付けたもの（ペースト送出。
/// `should_suppress` は `delete_on_send` 設定）。抑止するときは、閉じた後の変更番号を記録する
/// （`ClipboardPort` の説明）。書き込みに失敗したら記録を変えない（ほかの送出の記録を消さない）。
/// 監視を切っているときも記録してよい（記録した番号と別の変更は一致しないので、監視を戻した後の
/// コピーを飲み込まない）。
pub fn set_clipboard_suppressed(port: &ClipboardPort, formats: &[Format], should_suppress: bool) -> Result<()> {
    write_marked(port, should_suppress, |guard| write_formats(guard, formats))
}

/// `port` で開いたクリップボードへ `write` で書き、閉じる。`suppress` なら閉じた後の変更番号を
/// 抑止する番号として記録する。`write` が失敗したら記録せずに返す（破棄で閉じる）。閉じる処理の
/// 失敗は書き込みの結果に含めない（書いた内容は取り消せない。記録しないので取り込まれうる）。
fn write_marked(
    port: &ClipboardPort,
    suppress: bool,
    write: impl FnOnce(&ClipboardGuard) -> Result<()>,
) -> Result<()> {
    let mut guard = port.open()?;
    write(&guard)?;
    let seq = guard.close();
    if suppress {
        port.record(seq);
    }
    // 記録してからロックを放す（監視の照合は、記録の後にしか入らない）
    drop(guard);
    Ok(())
}

/// テスト用: `port` で実際のクリップボードを開き、書き込みの本体だけを `write` に差し替えて
/// 書く（排他・閉じる・記録は本物）。
#[cfg(test)]
pub(crate) fn with_open_port(
    port: &ClipboardPort,
    suppress: bool,
    write: impl FnOnce(&ClipboardGuard) -> Result<()>,
) -> Result<()> {
    write_marked(port, suppress, write)
}

/// クリップボードが空のときだけ書き込む（起動時の同期で、空のクリップボードへ最新の履歴を戻す。
/// 常に抑止する）。空の確認と書き込みを、クリップボードを開いたまま続けて行う（監視の窓を渡して
/// 開くので、開いている間はほかのアプリが中身を変えられない。確認と書き込みの間に入った新しい
/// コピーを消さないため）。空でなければ書かずに `Ok(false)` を返し、記録も変えない。
pub fn set_clipboard_if_empty_suppressed(port: &ClipboardPort, formats: &[Format]) -> Result<bool> {
    let mut guard = port.open()?;
    if unsafe { CountClipboardFormats() } != 0 {
        return Ok(false);
    }
    write_formats(&guard, formats)?;
    let seq = guard.close();
    port.record(seq);
    // 記録してからロックを放す（`write_marked` と同じ）
    drop(guard);
    Ok(true)
}

/// 書き込み時のクリップボード形式IDを解決する。標準形式は固定ID。
/// カスタム形式のIDはセッションごとに変わるため、保存されたIDではなく
/// 形式名から`RegisterClipboardFormat`で引き直す（同名は常に同一IDが返る）。
/// 名前が取れなかった形式（"0x...."表記）はIDを再現できないためスキップする。
fn resolve_format_id(fmt: &Format) -> Option<u32> {
    for id in 1..=17 {
        if standard_format_name(id) == Some(fmt.format_name.as_str()) {
            return Some(id);
        }
    }
    if fmt.format_name.starts_with("0x") {
        return None;
    }
    let wide: Vec<u16> = fmt.format_name.encode_utf16().chain([0]).collect();
    let id = unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) };
    (id != 0).then_some(id)
}

/// 指定ウィンドウのタイトル・クラス名がフィルタに一致するか判定する。
/// 無効なハンドル（取得失敗）は「一致しない」として扱う（安全側＝除外せず取り込む）。
fn is_window_handle_ignored(hwnd: HWND, config: &Config) -> bool {
    if hwnd.is_invalid() {
        return false;
    }

    let mut title_buf = [0u16; 512];
    let title_len = unsafe { GetWindowTextW(hwnd, &mut title_buf) }.max(0) as usize;
    let title = String::from_utf16_lossy(&title_buf[..title_len]);

    let mut class_buf = [0u16; 256];
    let class_len = unsafe { GetClassNameW(hwnd, &mut class_buf) }.max(0) as usize;
    let class_name = String::from_utf16_lossy(&class_buf[..class_len]);

    config.is_window_ignored(&title, &class_name)
}

/// フォアグラウンドウィンドウ、またはクリップボード所有者ウィンドウのいずれかが
/// フィルタに一致すれば除外する（OR条件）。
///
/// デバウンス完了後の`GetForegroundWindow`だけを見ると、コピー元と別のウィンドウを
/// 判定しうる。
/// `GetClipboardOwner`（コピー元によりデータを設定したウィンドウ）への単純な置き換えは
/// 採らない: Chromium系アプリ（Chrome/Edge/Electron等）はクリップボード書き込み時に
/// 専用の非表示メッセージ窓（タイトルなし、クラス名`Chrome_MessageWindow`）を使うため、
/// オーナー単独判定だと既存のブラウザ向けフィルタ設定（タイトル/クラス名指定）が
/// 一致しなくなり、除外漏れが起きる（Chromium
/// `clipboard_win.cc`/`message_window.cc`で確認）。両者のOR判定なら、既存の
/// 可視ウィンドウベースの検出力を維持しつつ、コピー元がフォーカス外へ移った
/// 場合の検出力を補える。
fn is_clipboard_source_ignored(config: &Config) -> bool {
    let foreground = unsafe { GetForegroundWindow() };
    // GetClipboardOwnerは所有者なしをErrで表す（GetLastErrorの一般的な流儀）。
    // 「一致しない」として扱うため無効なハンドルにフォールバックする
    let owner = unsafe { GetClipboardOwner() }.unwrap_or_default();
    is_window_handle_ignored(foreground, config) || is_window_handle_ignored(owner, config)
}

// --- Worker thread: debounce + capture ---

/// 取り込みの1回ごとの判定（テストが `ClipboardWatcher::spawn_observed` で観測する。`seq` は
/// クリップボードを開いた直後に読んだ変更番号）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum Outcome {
    /// 前面の窓・持ち主の窓がフィルタに一致した（開いていない）
    Ignored,
    /// 抑止する番号と同じだった（中身は読んでいない）
    Suppressed { seq: u32 },
    /// 取り込んだ（`on_entry` を呼んだ後）
    Captured { seq: u32 },
    /// 取り込める形式が無かった
    Empty { seq: u32 },
    /// クリップボードを開けなかった
    Failed,
    /// まとめ待ちの間に監視が切られた（開いていない）
    WatchOff,
    /// 読み取りか取り込み（`on_entry`）がパニックした（監視を止め、`on_problem` を呼んだ後）
    Panicked,
    /// 取り込む形式の大きさの合計が上限を超えた（中身は写さず、`on_problem` を呼んだ後）
    TooLarge { seq: u32 },
}

/// 監視の登録（`AddClipboardFormatListener`）の切り替え。ビューア（`set_watch`）と、取り込みがパニックした
/// ワーカー（監視を止める）の両方から切り替えるので、登録・解除と状態の更新を1つのロックの中で行う。
struct WatchSwitch {
    hwnd: HWND,
    lock: Mutex<()>,
    /// 今クリップボードの変更の通知に登録しているか（実際の状態。登録・解除が成功したときだけ変える）
    listening: Arc<AtomicBool>,
}

// HWND はハンドル（不透明な識別子）で、参照外しはしない。登録・解除に渡すだけ
unsafe impl Send for WatchSwitch {}
unsafe impl Sync for WatchSwitch {}

impl WatchSwitch {
    /// 今の実際の状態と同じなら何もせず `Ok`。違えば登録・解除し、成功したときだけ状態を変える。
    fn set(&self, enabled: bool) -> WinResult<()> {
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        if self.listening.load(Ordering::SeqCst) == enabled {
            return Ok(());
        }
        unsafe {
            if enabled {
                AddClipboardFormatListener(self.hwnd)?;
            } else {
                RemoveClipboardFormatListener(self.hwnd)?;
            }
        }
        self.listening.store(enabled, Ordering::SeqCst);
        Ok(())
    }

    fn listening(&self) -> bool {
        self.listening.load(Ordering::SeqCst)
    }
}

/// 監視の窓からワーカーへの知らせ（クリップボードが変わった。`capture_now` の頼みも同じ形で送る）。
#[derive(Clone, Copy, Debug)]
struct Changed {
    /// 変更の通知を受けた瞬間の前面の窓（ハンドルの値。ほとんどはコピー元）。0 は無い（`capture_now`）
    foreground: isize,
}

impl Changed {
    /// 通知を受けた瞬間の前面の窓がウィンドウフィルタに当たるか（ワーカーが受け取ってすぐに読む）。
    fn source_ignored(self, config: &Config) -> bool {
        !config.window_filters.is_empty()
            && is_window_handle_ignored(HWND(self.foreground as *mut core::ffi::c_void), config)
    }
}

fn worker_loop(
    rx: Receiver<Changed>,
    config: Arc<RwLock<Config>>,
    port: Arc<ClipboardPort>,
    switch: Arc<WatchSwitch>,
    on_entry: impl Fn(Entry),
    on_problem: impl Fn(WatchProblem),
    on_outcome: impl Fn(Outcome),
) {
    while let Ok(first) = rx.recv() {
        // 設定は変更イベントごとにスナップショットを取る（設定画面からの
        // 実行時変更を次のキャプチャから反映しつつ、Win32呼び出し中に
        // ロックを保持しないため）
        let config = config.read().unwrap_or_else(|p| p.into_inner()).clone();
        let interval = Duration::from_millis(config.history.add_interval_ms.max(1));

        // ウィンドウフィルタは、変更の通知を受けた瞬間の前面の窓（ほとんどはコピー元。通知を受ける前に切り替えられると
        // 別の窓になり、その場合は漏れうる）でも判定する。まとめ待ちの後の前面の窓だけだと、コピーしてすぐ別の窓へ
        // 切り替えたときに当たらない。知らせを受け取ったらすぐに読む（ワーカーは
        // 知らせを待って止まっているので、通知の直後）。まとめた知らせのどれかで当たれば、まとめた1回を取り込まない
        let mut ignored_at_change = first.source_ignored(&config);

        // デバウンス: interval 内に届いた後続イベントは1回にまとめる
        loop {
            match rx.recv_timeout(interval) {
                Ok(changed) => {
                    ignored_at_change |= changed.source_ignored(&config);
                    continue;
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }

        // まとめ待ちの間に監視が切られていたら取り込まない（切った後に、待っていた変更を取り込まない）。
        // 起動時の同期（`capture_now`）は監視がオンのときだけ頼まれる
        if !switch.listening() {
            on_outcome(Outcome::WatchOff);
            continue;
        }
        if ignored_at_change || is_clipboard_source_ignored(&config) {
            on_outcome(Outcome::Ignored);
            continue;
        }

        // 抑止は、まとめた後の最後の状態の変更番号で見分ける（`capture_clipboard`）。自分の
        // 抑止つきの書き込みの後に、変換・ほかのコピーが書かれていれば番号が違うので取り込む。
        // 読み取りと取り込みのパニックは、この1回の中で捕まえる（捕まえないとワーカーのスレッドが終わり、
        // 以後のコピーを黙って取り込まなくなる）。開いたクリップボードは巻き戻しで閉じ、`CLIPBOARD_LOCK`・
        // `Core` のロックは poison しても取れる（`Core` は以後の変更と保存を断る）ので、続けても壊れた状態は
        // 使わない。捕まえたら監視を止め（同じ不具合で繰り返さない。戻すのは利用者）、知らせる
        let captured = panic::catch_unwind(AssertUnwindSafe(|| match capture_clipboard(&config, &port) {
            Ok(Capture::Suppressed(seq)) => Outcome::Suppressed { seq },
            Ok(Capture::Read(seq, Some(entry))) => {
                on_entry(entry);
                Outcome::Captured { seq }
            }
            Ok(Capture::Read(seq, None)) => Outcome::Empty { seq },
            Ok(Capture::TooLarge(seq, too_large)) => {
                on_problem(WatchProblem::TooLarge(too_large));
                Outcome::TooLarge { seq }
            }
            Err(_) => Outcome::Failed,
        }));
        let outcome = captured.unwrap_or_else(|_| {
            on_problem(WatchProblem::Panicked(switch.set(false)));
            Outcome::Panicked
        });
        on_outcome(outcome);
    }
}

// --- Window procedure ---

struct WindowContext {
    changed_tx: Sender<Changed>,
}

const WM_APP_SHUTDOWN: u32 = WM_USER + 1;
const CLASS_NAME: PCWSTR = w!("CLCLR_ClipboardWatcher");

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
            WM_CLIPBOARDUPDATE => {
                let ctx = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowContext;
                if let Some(ctx) = ctx.as_ref() {
                    // 前面の窓のハンドルを取るだけ（タイトル・クラス名はワーカーが読む）
                    let foreground = GetForegroundWindow().0 as isize;
                    let _ = ctx.changed_tx.send(Changed { foreground });
                }
                LRESULT(0)
            }
            WM_APP_SHUTDOWN => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                let ctx = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowContext;
                if !ctx.is_null() {
                    drop(Box::from_raw(ctx));
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

fn create_message_window(changed_tx: Sender<Changed>) -> WinResult<HWND> {
    unsafe {
        let hinstance = GetModuleHandleW(None)?.into();

        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        // 既に登録済み(2回目以降のspawn等)の場合は失敗するが、CreateWindowExW側で
        // 実際にクラスが使えるかどうかが最終的に判定されるためここでは無視してよい。
        RegisterClassW(&class);

        let ctx = Box::into_raw(Box::new(WindowContext { changed_tx }));

        CreateWindowExW(
            Default::default(),
            CLASS_NAME,
            CLASS_NAME,
            WS_OVERLAPPED,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinstance),
            Some(ctx.cast()),
        )
    }
}

fn run_message_loop() {
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

/// HWNDはハンドル（不透明な識別子）であり生ポインタとしての参照外しは行わないため、
/// スレッド間でやり取りしても安全。windows-rsのHWNDは`*mut c_void`を包むだけで
/// 自動ではSendにならないため、起動時の受け渡し用に明示的に許可する。
struct SendHwnd(HWND);
unsafe impl Send for SendHwnd {}

/// メッセージウィンドウへシャットダウン要求を送り、そのスレッドの終了を待つ（`spawn`の登録失敗時と`Drop`の
/// 後片付け）。投稿に失敗しても（投稿の列が満杯など）止まらないよう、投げ直しながら待つ（ホットキー・トレイと
/// 同じ `ui_thread::stop_ui_thread`）。
fn shutdown_window_thread(hwnd: HWND, thread: JoinHandle<()>) {
    crate::ui_thread::stop_ui_thread(hwnd, WM_APP_SHUTDOWN, thread, crate::ui_thread::REPOST_INTERVAL);
}

// --- Public API ---

/// クリップボード監視の起動ハンドル。
/// Drop時にウィンドウ・スレッドを後片付けする。
pub struct ClipboardWatcher {
    hwnd: HWND,
    /// クリップボードを開くときに渡す窓（この監視の窓）と、抑止する変更番号
    port: Arc<ClipboardPort>,
    /// 変更通知チャネルの送信側の複製（`capture_now`用。主経路は監視ウィンドウ）。
    /// `Option`にしているのは`Drop::drop`内で`worker_thread.join()`より前に
    /// 明示的に破棄するため（下記Drop実装のコメント参照）。
    changed_tx: Option<mpsc::Sender<Changed>>,
    window_thread: Option<JoinHandle<()>>,
    worker_thread: Option<JoinHandle<()>>,
    /// 監視の窓を今クリップボードの変更の通知に登録しているか（実際の状態）と、その切り替え。
    /// トレイのアイコン・メニューのチェックもこれで決める（設定の値と食い違いうるため）
    switch: Arc<WatchSwitch>,
}

impl ClipboardWatcher {
    /// 監視を開始する。`on_entry` はキャプチャされた `Entry` ごとに呼ばれる
    /// （ワーカースレッド上で実行されるため、呼び出し先はスレッドセーフである必要がある）。
    /// `on_problem` は、取り込めなかったことを知らせる（`WatchProblem`。パニックしたときは監視を止めた後で、
    /// ワーカーは続くので、監視を戻せば取り込みも戻る）。
    /// `config` は共有参照で、フィルタ・デバウンス等の変更は次のキャプチャから反映される。
    pub fn spawn(
        config: Arc<RwLock<Config>>,
        on_entry: impl Fn(Entry) + Send + 'static,
        on_problem: impl Fn(WatchProblem) + Send + 'static,
    ) -> Result<Self> {
        Self::spawn_inner(config, on_entry, on_problem, |_| {})
    }

    /// テスト用: `spawn` に、取り込みの1回ごとの判定の観測（`on_outcome`）を足したもの。
    #[cfg(test)]
    pub(crate) fn spawn_observed(
        config: Arc<RwLock<Config>>,
        on_entry: impl Fn(Entry) + Send + 'static,
        on_problem: impl Fn(WatchProblem) + Send + 'static,
        on_outcome: impl Fn(Outcome) + Send + 'static,
    ) -> Result<Self> {
        Self::spawn_inner(config, on_entry, on_problem, on_outcome)
    }

    fn spawn_inner(
        config: Arc<RwLock<Config>>,
        on_entry: impl Fn(Entry) + Send + 'static,
        on_problem: impl Fn(WatchProblem) + Send + 'static,
        on_outcome: impl Fn(Outcome) + Send + 'static,
    ) -> Result<Self> {
        let (changed_tx, changed_rx) = mpsc::channel::<Changed>();
        let (hwnd_tx, hwnd_rx) = mpsc::channel::<WinResult<SendHwnd>>();

        let window_changed_tx = changed_tx.clone();
        let window_thread = thread::spawn(move || match create_message_window(window_changed_tx) {
            Ok(hwnd) => {
                let _ = hwnd_tx.send(Ok(SendHwnd(hwnd)));
                run_message_loop();
            }
            Err(e) => {
                let _ = hwnd_tx.send(Err(e));
            }
        });

        let hwnd = hwnd_rx
            .recv()
            .map_err(|_| ClipboardError::Win32(WinError::from_hresult(windows::Win32::Foundation::E_FAIL)))??
            .0;

        let watch = config.read().unwrap_or_else(|p| p.into_inner()).general.clipboard_watch;
        if watch {
            if let Err(e) = unsafe { AddClipboardFormatListener(hwnd) } {
                // 失敗時はここまでに作った窓・スレッドを片付けてから返す（残すとリークする）
                shutdown_window_thread(hwnd, window_thread);
                return Err(e.into());
            }
        }

        let port = Arc::new(ClipboardPort::new(hwnd));
        let worker_port = Arc::clone(&port);
        let switch = Arc::new(WatchSwitch { hwnd, lock: Mutex::new(()), listening: Arc::new(AtomicBool::new(watch)) });
        let worker_switch = Arc::clone(&switch);
        let worker_thread = thread::spawn(move || {
            worker_loop(changed_rx, config, worker_port, worker_switch, on_entry, on_problem, on_outcome)
        });

        Ok(Self {
            hwnd,
            port,
            changed_tx: Some(changed_tx),
            window_thread: Some(window_thread),
            worker_thread: Some(worker_thread),
            switch,
        })
    }

    /// 現在のクリップボード内容を（変更を待たず）取り込む。起動時同期用。
    /// 変更通知と同じワーカー経路に乗せるため、フィルタ・重複チェック等の
    /// ポリシーは通常のキャプチャと同一に適用される（変更の瞬間の前面の窓は無いので、ウィンドウフィルタは
    /// 判定の時点の前面の窓と持ち主の窓だけで見る）。
    pub fn capture_now(&self) {
        if let Some(tx) = &self.changed_tx {
            let _ = tx.send(Changed { foreground: 0 });
        }
    }

    /// クリップボードを開くときに渡す窓（この監視の窓）と、抑止する変更番号（`ClipboardPort`）。
    /// クリップボードへ書く側（ホットキー・ビューアの操作・起動時の同期）へ渡す。窓はこの監視を
    /// 破棄するまで生きている（`main` は、受け付けを閉じて書く側が書かなくなった後に破棄する）。
    pub fn port(&self) -> Arc<ClipboardPort> {
        Arc::clone(&self.port)
    }

    /// クリップボード監視のON/OFFを切り替える（設定の反映・トレイの切り替えの適用点）。今の実際の状態と
    /// 同じなら何もせず `Ok`。違えば登録・解除し、成功したときだけ状態を変える（失敗は `Err` で、状態は
    /// そのまま）。
    pub fn set_watch(&self, enabled: bool) -> WinResult<()> {
        self.switch.set(enabled)
    }

    /// 今クリップボードの変更の通知に登録しているか（実際の状態。取り込みのパニックでワーカーが止めることもある）。
    pub fn listening(&self) -> bool {
        self.switch.listening()
    }

    /// 実際の状態の共有（トレイがアイコン・メニューのチェックに使う）。
    pub fn watch_state(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.switch.listening)
    }
}

impl Drop for ClipboardWatcher {
    fn drop(&mut self) {
        // 解除はワーカー（取り込みのパニック）と同じ切り替えを通す（同時に解除しても、登録と状態が食い違わない）
        let _ = self.switch.set(false);
        if let Some(t) = self.window_thread.take() {
            shutdown_window_thread(self.hwnd, t);
        }
        // worker_loopの`rx.recv()`を終了させるため、自分が保持するSenderをここで
        // 明示的に破棄する。フィールドは`drop`メソッド本体の実行後まで生存するため、
        // 何もしなければ`self.changed_tx`が生き続け、下のjoinが永久に返らない
        // （実機確認済みのデッドロック。window_thread側のSender複製は直前のjoinで
        // 既にWM_DESTROY処理によりdrop済みなので、残る送信側はこれだけ）
        self.changed_tx = None;
        if let Some(t) = self.worker_thread.take() {
            // ワーカーは前面の窓・持ち主の窓のタイトルを読む。それがこのスレッドの窓（ビューア）だと `WM_GETTEXT` が
            // 送られてくるので、応じながら待つ（ただ join すると互いに待って止まる）
            crate::ui_thread::join_answering_sent_messages(t, crate::ui_thread::REPOST_INTERVAL, || {});
        }
    }
}

/// 実クリップボードを使うテストの直列化（ほかのモジュールのテストも同じものを使う）。
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Arc, Mutex, MutexGuard, RwLock};

    use super::ClipboardWatcher;
    use crate::config::Config;

    /// 実クリップボードを読み書きするテスト同士が並列に動くと、互いの書き込みや
    /// `EmptyClipboard`が混ざるため、このMutexで直列化する（別々のテストが別々の監視の窓で
    /// 開くので、実行時の `CLIPBOARD_LOCK` だけでは書き込みから検証までをまとめて守れない）。
    /// 実行時の `CLIPBOARD_LOCK` とは別のロックで、取る順はいつもこちら → `CLIPBOARD_LOCK`
    /// （テストがこれを持ったまま本体の関数を呼ぶ）。
    static CLIPBOARD_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// テスト用の監視（監視は切ってある。クリップボードを開く窓 `port()` を得るためのもの）。
    /// 窓とスレッドを作るので、呼ぶ前にクリップボード用 → GUI 用（`tray::lock_gui_resource_tests`）
    /// の順にロックを取り、破棄（窓・スレッドの終わりの待ち）が済むまで持つ。
    pub(crate) fn test_watcher() -> ClipboardWatcher {
        let mut config = Config::default();
        config.general.clipboard_watch = false;
        ClipboardWatcher::spawn(Arc::new(RwLock::new(config)), |_| {}, |_| {}).unwrap()
    }

    /// `CLIPBOARD_TEST_LOCK`を取る。直前のテストがロックを持ったままパニックしても
    /// （poison）続行する。そうしないと1件の失敗が後続のテストへ連鎖し、失敗件数が
    /// 実際より多く見える。
    pub(crate) fn lock_clipboard_tests() -> MutexGuard<'static, ()> {
        CLIPBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::lock_clipboard_tests;
    use super::*;
    use std::time::Duration;

    #[test]
    fn standard_format_name_maps_known_ids() {
        assert_eq!(standard_format_name(1), Some("CF_TEXT"));
        assert_eq!(standard_format_name(8), Some("CF_DIB"));
        assert_eq!(standard_format_name(13), Some("CF_UNICODETEXT"));
        assert_eq!(standard_format_name(15), Some("CF_HDROP"));
        assert_eq!(standard_format_name(17), Some("CF_DIBV5"));
    }

    #[test]
    fn standard_format_name_returns_none_for_unknown_id() {
        assert_eq!(standard_format_name(0), None);
        assert_eq!(standard_format_name(9999), None);
    }

    #[test]
    fn format_name_uses_standard_name_when_available() {
        // 標準形式IDはGetClipboardFormatNameWを呼ばずにそのまま返る
        assert_eq!(format_name(13), "CF_UNICODETEXT");
        assert_eq!(format_name(15), "CF_HDROP");
    }

    /// テスト専用: dropを別スレッドへ持ち込むためのSendラッパー（HWNDはハンドルであり
    /// 参照外ししないため安全。SendHwndと同趣旨）。フィールドはdrop時に消費されるだけで
    /// 読み出さないため`dead_code`警告を抑止する。
    #[allow(dead_code)]
    struct SendWatcher(ClipboardWatcher);
    unsafe impl Send for SendWatcher {}

    /// `ClipboardWatcher::drop`が`worker_thread.join()`でハングしないことの回帰テスト
    /// （`changed_tx`がフィールドとして生存し続けるために起きていたデッドロック）。
    /// dropを別スレッドで実行し、タイムアウト付きで完了を待つことで検証する。
    #[test]
    fn watcher_drop_does_not_deadlock() {
        // 監視の窓とスレッドを作り、監視はオン（ほかのテストの書き込みを取り込みに行きうる）ので、
        // クリップボード用 → GUI 用の順にロックを取り、破棄の完了（下の待ち）まで持つ
        let _clipboard = lock_clipboard_tests();
        let _gui = crate::tray::lock_gui_resource_tests();
        let config = Arc::new(RwLock::new(Config::default()));
        let watcher = ClipboardWatcher::spawn(config, |_| {}, |_| {}).unwrap();
        let watcher = SendWatcher(watcher);

        let (done_tx, done_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            drop(watcher);
            let _ = done_tx.send(());
        });

        assert!(
            done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "ClipboardWatcher::drop がタイムアウトした（デッドロックの疑い）"
        );
        let _ = handle.join();
    }

    /// 監視の切り替え: 実際の状態は設定の値で始まり、切り替えが成功したときだけ変わる。
    /// 今と同じ状態への切り替えは何もせず `Ok`。共有の状態（トレイが読む）も同じ値になる。
    #[test]
    fn set_watch_tracks_actual_state_and_skips_same_state() {
        let _clipboard = lock_clipboard_tests();
        let _gui = crate::tray::lock_gui_resource_tests();
        let mut config = Config::default();
        config.general.clipboard_watch = false;
        let watcher = ClipboardWatcher::spawn(Arc::new(RwLock::new(config)), |_| {}, |_| {}).unwrap();
        let shared = watcher.watch_state();
        assert!(!watcher.listening());
        watcher.set_watch(false).expect("同じ状態への切り替えが失敗した");
        assert!(!watcher.listening());
        watcher.set_watch(true).expect("登録できなかった");
        assert!(watcher.listening() && shared.load(Ordering::SeqCst));
        watcher.set_watch(true).expect("同じ状態への切り替えが失敗した（二重の登録を呼んだ疑い）");
        watcher.set_watch(false).expect("解除できなかった");
        assert!(!watcher.listening() && !shared.load(Ordering::SeqCst));
        drop(watcher);
    }

    /// `shutdown_window_thread`（`spawn`が`AddClipboardFormatListener`登録失敗時に使う
    /// 後片付け）が実際のウィンドウ・スレッドに対してハングせず完了する。
    #[test]
    fn shutdown_window_thread_does_not_hang() {
        // 監視の窓とスレッドを作るので、クリップボード用 → GUI 用の順にロックを取る
        let _clipboard = lock_clipboard_tests();
        let _gui = crate::tray::lock_gui_resource_tests();
        let (changed_tx, _changed_rx) = mpsc::channel::<Changed>();
        let (hwnd_tx, hwnd_rx) = mpsc::channel::<WinResult<SendHwnd>>();
        let window_thread = thread::spawn(move || match create_message_window(changed_tx) {
            Ok(hwnd) => {
                let _ = hwnd_tx.send(Ok(SendHwnd(hwnd)));
                run_message_loop();
            }
            Err(e) => {
                let _ = hwnd_tx.send(Err(e));
            }
        });
        let hwnd = SendHwnd(hwnd_rx.recv().unwrap().unwrap().0);

        let (done_tx, done_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let hwnd = hwnd;
            shutdown_window_thread(hwnd.0, window_thread);
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "shutdown_window_threadがタイムアウトした（ウィンドウ・スレッドの後片付けがハングする可能性）"
        );
        let _ = handle.join();
    }

    /// 無効なハンドル（`GetClipboardOwner`が失敗した場合の`unwrap_or_default`等）は
    /// 「一致しない」＝除外しない側に倒す（安全側デフォルト）。
    #[test]
    fn is_window_handle_ignored_returns_false_for_invalid_handle() {
        let config = Config::default();
        assert!(!is_window_handle_ignored(HWND::default(), &config));
    }

    /// メッセージを処理し続ける別のスレッドに作った、タイトル付きの窓（ほかのアプリの窓の代わり。ワーカーがタイトルを
    /// 読むと、そのスレッドへ `WM_GETTEXT` が送られるので、処理し続ける）。破棄で窓を閉じ、スレッドを終える。
    struct TitledWindow {
        hwnd: isize,
        thread_id: u32,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl TitledWindow {
        fn new(title: &'static str) -> Self {
            use windows::Win32::System::Threading::GetCurrentThreadId;
            let (tx, rx) = mpsc::channel();
            let thread = thread::spawn(move || unsafe {
                let hwnd = CreateWindowExW(
                    Default::default(),
                    w!("STATIC"),
                    &windows::core::HSTRING::from(title),
                    WS_OVERLAPPED,
                    0,
                    0,
                    10,
                    10,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap();
                tx.send((hwnd.0 as isize, GetCurrentThreadId())).unwrap();
                run_message_loop();
                let _ = DestroyWindow(hwnd);
            });
            let (hwnd, thread_id) = rx.recv().unwrap();
            Self { hwnd, thread_id, thread: Some(thread) }
        }
    }

    impl Drop for TitledWindow {
        fn drop(&mut self) {
            use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_QUIT};
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    fn title_filter(title: &str) -> crate::config::WindowFilter {
        crate::config::WindowFilter { title: title.to_string(), class_name: String::new(), ignore: true }
    }

    /// 変更の知らせに付けた前面の窓（通知を受けた瞬間の窓）で、ウィンドウフィルタを判定する。前面の窓が無い知らせ
    /// （`capture_now`）とフィルタが無いときは当たらない。
    #[test]
    fn change_notice_judges_window_filter_by_foreground_at_change() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let window = TitledWindow::new("CLCLR 除外テスト窓");
        let changed = Changed { foreground: window.hwnd };
        let mut config = Config::default();
        assert!(!changed.source_ignored(&config), "フィルタが無いのに当たった");
        config.window_filters.push(title_filter("除外テスト"));
        assert!(changed.source_ignored(&config));
        assert!(!Changed { foreground: 0 }.source_ignored(&config));
        config.window_filters[0].title = "当たらない".to_string();
        assert!(!changed.source_ignored(&config));
    }

    /// ワーカーは、まとめ待ちの後の前面の窓が当たらなくても、変更の知らせに付いた前面の窓（コピーした瞬間の窓）が
    /// 当たれば取り込まない（コピーしてすぐ別の窓へ切り替えた場合）。当たらない知らせだけなら取り込む。
    #[test]
    fn worker_ignores_copy_when_window_at_change_matches_filter() {
        let _clipboard = lock_clipboard_tests();
        let _gui = crate::tray::lock_gui_resource_tests();
        settle_clipboard();
        let watcher = test_support::test_watcher();
        let port = watcher.port();
        let window = TitledWindow::new("CLCLR 除外テスト窓");
        let mut config = Config::default();
        config.history.add_interval_ms = 50;
        config.window_filters.push(title_filter("除外テスト"));
        let switch = Arc::new(WatchSwitch { hwnd: HWND::default(), lock: Mutex::new(()), listening: Arc::new(AtomicBool::new(true)) });
        let (changed_tx, changed_rx) = mpsc::channel();
        let (entry_tx, entries) = mpsc::channel();
        let (outcome_tx, outcomes) = mpsc::channel();
        let worker = {
            let (config, port) = (Arc::new(RwLock::new(config)), Arc::clone(&port));
            thread::spawn(move || {
                worker_loop(
                    changed_rx,
                    config,
                    port,
                    switch,
                    move |entry| {
                        let _ = entry_tx.send(entry);
                    },
                    |_| {},
                    move |outcome| {
                        let _ = outcome_tx.send(outcome);
                    },
                )
            })
        };
        set_clipboard(&port, &[text("除外したい窓のコピー")]).unwrap();
        changed_tx.send(Changed { foreground: window.hwnd }).unwrap();
        assert_eq!(outcomes.recv_timeout(Duration::from_secs(5)).unwrap(), Outcome::Ignored);
        assert!(entries.try_recv().is_err(), "除外した窓のコピーを取り込んだ");

        // まとめた知らせのどれかが当たれば、まとめた1回を取り込まない
        changed_tx.send(Changed { foreground: 0 }).unwrap();
        changed_tx.send(Changed { foreground: window.hwnd }).unwrap();
        assert_eq!(outcomes.recv_timeout(Duration::from_secs(5)).unwrap(), Outcome::Ignored);

        changed_tx.send(Changed { foreground: 0 }).unwrap();
        let copied = seq();
        assert_eq!(outcomes.recv_timeout(Duration::from_secs(5)).unwrap(), Outcome::Captured { seq: copied });
        assert_eq!(entry_text(&entries.try_recv().unwrap()), "除外したい窓のコピー");
        drop(changed_tx);
        worker.join().unwrap();
    }

    /// 監視のワーカーの終わりを待つ側（`ClipboardWatcher` の `Drop`）は、ワーカーが待つ側のスレッドの窓（ビューアの
    /// 代わり）のタイトルを読む間も、送られてくる `WM_GETTEXT` に応じながら待つ（ただ join すると互いに待って止まる）。
    /// 止まったときは、見張りのスレッドが知らせる（テストは戻らない）。
    #[test]
    fn waiting_for_worker_answers_its_window_text_request() {
        use std::sync::atomic::AtomicUsize;
        use windows::Win32::UI::WindowsAndMessaging::{DefWindowProcW, RegisterClassW, WM_GETTEXT, WNDCLASSW};
        static GETTEXT: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "system" fn count_gettext(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
            if msg == WM_GETTEXT {
                GETTEXT.fetch_add(1, Ordering::SeqCst);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }

        let _gui = crate::tray::lock_gui_resource_tests();
        GETTEXT.store(0, Ordering::SeqCst);
        let hwnd = unsafe {
            let class_name = w!("CLCLR_TestWaiterWindow");
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(count_gettext),
                hInstance: GetModuleHandleW(None).unwrap().into(),
                lpszClassName: class_name,
                ..Default::default()
            });
            CreateWindowExW(Default::default(), class_name, w!("CLCLR 除外テスト窓（待つ側）"), WS_OVERLAPPED, 0, 0, 10, 10, None, None, None, None)
                .unwrap()
        };
        let mut config = Config::default();
        config.window_filters.push(title_filter("除外テスト"));
        // 監視は切ってある扱い（クリップボードは開かない）
        let switch = Arc::new(WatchSwitch { hwnd: HWND::default(), lock: Mutex::new(()), listening: Arc::new(AtomicBool::new(false)) });
        let (changed_tx, changed_rx) = mpsc::channel();
        let worker = {
            let (config, port) = (Arc::new(RwLock::new(config)), Arc::new(ClipboardPort::unopenable()));
            thread::spawn(move || worker_loop(changed_rx, config, port, switch, |_| {}, |_| {}, |_| {}))
        };
        changed_tx.send(Changed { foreground: hwnd.0 as isize }).unwrap();
        drop(changed_tx);

        let (done_tx, done_rx) = mpsc::channel::<()>();
        let watchdog = thread::spawn(move || done_rx.recv_timeout(Duration::from_secs(10)).is_ok());
        crate::ui_thread::join_answering_sent_messages(worker, Duration::from_millis(100), || {});
        let _ = done_tx.send(());
        assert!(watchdog.join().unwrap(), "ワーカーの終わりを待つ側が WM_GETTEXT に応じず、互いに待った");
        assert!(GETTEXT.load(Ordering::SeqCst) >= 1, "前提: ワーカーが待つ側の窓のタイトルを読んでいない");
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }

    /// 実際のウィンドウでタイトル/クラス名フィルタが機能すること
    /// （`is_clipboard_source_ignored`のOR判定が依拠するWin32連携部分の回帰）。
    #[test]
    fn is_window_handle_ignored_matches_real_window_by_title() {
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, RegisterClassW, CW_USEDEFAULT, WNDCLASSW,
            WS_OVERLAPPED,
        };

        unsafe extern "system" fn test_wndproc(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }

        // 窓を作って壊すので GUI 用のロックを取る（クリップボードには触れない）
        let _gui = crate::tray::lock_gui_resource_tests();
        unsafe {
            let hinstance = GetModuleHandleW(None).unwrap().into();
            let class_name = w!("CLCLR_TestFilterWindow");
            let class = WNDCLASSW {
                lpfnWndProc: Some(test_wndproc),
                hInstance: hinstance,
                lpszClassName: class_name,
                ..Default::default()
            };
            RegisterClassW(&class);
            let hwnd = CreateWindowExW(
                Default::default(),
                class_name,
                w!("CLCLR Test Filter Window"),
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

            let mut config = Config::default();
            config.window_filters.push(crate::config::WindowFilter {
                title: "Test Filter".to_string(),
                class_name: String::new(),
                ignore: true,
            });
            assert!(is_window_handle_ignored(hwnd, &config));

            config.window_filters[0].title = "Does Not Match".to_string();
            assert!(!is_window_handle_ignored(hwnd, &config));

            let _ = DestroyWindow(hwnd);
        }
    }

    /// クリップボードへの書き込み直後は、他プロセス（クリップボードを監視・同期する
    /// 常駐アプリ等）が変更を検知して一瞬`OpenClipboard`する。その多くは書き込みから
    /// 1〜2ms以内なので、`CLIPBOARD_TEST_LOCK`取得後、クリップボード操作系テストの
    /// 冒頭でこれを呼んで待つ。遅れて読みに来るプロセス（実測で約100〜200ms後）との
    /// 衝突はこの待ちでは避けられず、`ClipboardGuard::open`の再試行で吸収する。
    fn settle_clipboard() {
        std::thread::sleep(Duration::from_millis(50));
    }

    /// クリップボードへ書くテストの準備: クリップボード用 → GUI 用の順にロックを取り、テスト用の
    /// 監視（窓・スレッド）を作る。フィールドは宣言の順に破棄されるので、監視（窓・スレッドの
    /// 終わりの待ち）→ GUI 用 → クリップボード用の順に放す。
    struct Fixture {
        watcher: ClipboardWatcher,
        _gui: MutexGuard<'static, ()>,
        _clipboard: MutexGuard<'static, ()>,
    }

    fn fixture() -> Fixture {
        let clipboard = lock_clipboard_tests();
        let gui = crate::tray::lock_gui_resource_tests();
        settle_clipboard();
        Fixture { watcher: super::test_support::test_watcher(), _gui: gui, _clipboard: clipboard }
    }

    fn text(s: &str) -> Format {
        Format { format_name: "CF_UNICODETEXT".to_string(), format_id: 13, data: crate::data::utf16_bytes(s) }
    }

    fn unresolvable() -> Format {
        Format { format_name: "0xDEAD".to_string(), format_id: 0, data: vec![1, 2, 3] }
    }

    fn seq() -> u32 {
        unsafe { GetClipboardSequenceNumber() }
    }

    fn entry_text(entry: &Entry) -> String {
        let f = entry.formats.iter().find(|f| f.format_name == "CF_UNICODETEXT").unwrap();
        crate::data::utf16_text(&f.data)
    }

    /// 今のクリップボードのテキスト（読むだけなので変更番号は変わらない）。
    fn current_text(port: &ClipboardPort) -> String {
        let guard = port.open().unwrap();
        entry_text(&read_entry(&guard, &Config::default()).unwrap().unwrap())
    }

    /// 別のスレッドで自分の窓（ほかのアプリの代わり）を作り、`f` に渡す。窓は `f` の後に壊す。
    fn with_other_window<T: Send + 'static>(f: impl FnOnce(HWND) -> T + Send + 'static) -> T {
        thread::spawn(move || unsafe {
            let hwnd = CreateWindowExW(
                Default::default(),
                w!("STATIC"),
                w!("CLCLR other clipboard window"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
            .unwrap();
            let result = f(hwnd);
            let _ = DestroyWindow(hwnd);
            result
        })
        .join()
        .unwrap()
    }

    /// 抑止つきの書き込みは、閉じた後の変更番号を記録する。抑止しない書き込み・変換・空にする、は
    /// 記録を変えない。
    #[test]
    fn suppressed_write_records_sequence_after_close() {
        let f = fixture();
        let port = f.watcher.port();
        assert_eq!(port.suppressed_seq(), 0);
        set_clipboard_suppressed(&port, &[text("抑止")], true).unwrap();
        let recorded = port.suppressed_seq();
        assert_ne!(recorded, 0);
        assert_eq!(recorded, seq(), "閉じた後の番号ではない");

        set_clipboard(&port, &[text("変換")]).unwrap();
        set_clipboard_suppressed(&port, &[text("抑止しない")], false).unwrap();
        clear_clipboard(&port).unwrap();
        assert_eq!(port.suppressed_seq(), recorded);
        assert_ne!(seq(), recorded);
    }

    /// すべての形式が失敗した抑止つきの書き込みは、記録を変えない（ほかの送出の記録を消さない）。
    #[test]
    fn failed_suppressed_write_keeps_record() {
        let f = fixture();
        let port = f.watcher.port();
        set_clipboard_suppressed(&port, &[text("先の送出")], true).unwrap();
        let recorded = port.suppressed_seq();
        let result = set_clipboard_suppressed(&port, &[unresolvable()], true);
        assert!(matches!(result, Err(ClipboardError::AllFormatsFailed)), "{result:?}");
        assert_eq!(port.suppressed_seq(), recorded);
    }

    /// 失敗する書き込みがロックの中で止まっている間に始めた別の抑止つきの書き込みは待ち、終わった
    /// 後の記録は後の書き込みの番号（「失敗した送出の取り消しが、ほかの送出の抑止を消す」
    /// の回帰。書き込みの本体だけ差し替え、排他・閉じる・記録は本物）。
    #[test]
    fn failed_write_in_lock_does_not_clear_record_of_next_write() {
        let f = fixture();
        let port = f.watcher.port();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let first = {
            let port = Arc::clone(&port);
            thread::spawn(move || {
                with_open_port(&port, true, |_| {
                    entered_tx.send(()).unwrap();
                    let _ = release_rx.recv_timeout(Duration::from_secs(10));
                    Err(ClipboardError::AllFormatsFailed)
                })
            })
        };
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let second = {
            let port = Arc::clone(&port);
            thread::spawn(move || with_open_port(&port, true, |_| Ok(())))
        };
        thread::sleep(Duration::from_millis(50));
        assert!(!second.is_finished(), "開いている間に別の書き込みが入った");
        release_tx.send(()).unwrap();
        assert!(first.join().unwrap().is_err());
        assert!(second.join().unwrap().is_ok());
        assert_ne!(port.suppressed_seq(), 0, "後の書き込みの番号を記録していない");
        assert_eq!(port.suppressed_seq(), seq());
    }

    /// 起動時の同期の書き込み: クリップボードが空でなければ書かず（中身も記録も変えない）、
    /// 空なら書いて記録する。
    #[test]
    fn set_clipboard_if_empty_writes_only_when_empty() {
        let f = fixture();
        let port = f.watcher.port();
        set_clipboard(&port, &[text("既存のコピー")]).unwrap();
        assert!(!set_clipboard_if_empty_suppressed(&port, &[text("復元")]).unwrap());
        assert_eq!(port.suppressed_seq(), 0, "書かなかったのに記録した");
        assert_eq!(current_text(&port), "既存のコピー");

        clear_clipboard(&port).unwrap();
        assert!(set_clipboard_if_empty_suppressed(&port, &[text("復元")]).unwrap());
        assert_eq!(port.suppressed_seq(), seq());
        assert_eq!(current_text(&port), "復元");
    }

    /// 回帰: 全形式が失敗した場合（`resolve_format_id`が`None`を返す形式のみ渡す）、
    /// `set_clipboard`はもはや`Ok`を返さず`Err(AllFormatsFailed)`を返すこと。
    /// `format_name`が"0x"で始まる形式は`resolve_format_id`が常に`None`を返す仕様
    /// （元の名前を再現できないためスキップする設計）を利用して、実際にWin32呼び出しを
    /// 経由せず確実に「全形式失敗」を再現する。
    #[test]
    fn set_clipboard_fails_when_all_formats_unresolvable() {
        let f = fixture();
        let port = f.watcher.port();
        set_clipboard(&port, &[text("元の中身")]).unwrap();
        let before = seq();
        let result = set_clipboard(&port, &[unresolvable()]);
        assert!(matches!(result, Err(ClipboardError::AllFormatsFailed)));
        // 失敗した送出で、利用者のクリップボードの中身を消さない
        assert_eq!(current_text(&port), "元の中身", "全形式が失敗したのにクリップボードを空にした");
        assert_eq!(seq(), before, "全形式が失敗したのにクリップボードを変えた");
    }

    /// `formats`が最初から空の場合は「意図的な空クリップボード化」として`Ok`のまま
    /// （`AllFormatsFailed`の対象外）。
    #[test]
    fn set_clipboard_succeeds_with_empty_formats() {
        let f = fixture();
        assert!(set_clipboard(&f.watcher.port(), &[]).is_ok());
    }

    /// 少なくとも1形式が成功すれば、他が失敗していても`Ok`を返す
    /// （部分的な失敗は許容し、全滅した場合のみエラーにする設計）。
    #[test]
    fn set_clipboard_succeeds_when_at_least_one_format_resolves() {
        let f = fixture();
        assert!(set_clipboard(&f.watcher.port(), &[unresolvable(), text("ok")]).is_ok());
    }

    /// 回帰: 他の窓がクリップボードを開いている間の`OpenClipboard`は即座に失敗するため、
    /// `ClipboardGuard::open`が再試行して書き込みを成功させる。他プロセスの代わりに、
    /// 別スレッドの窓で30ms開いたままにする。
    #[test]
    fn set_clipboard_retries_while_clipboard_is_held_elsewhere() {
        let f = fixture();
        let (opened_tx, opened_rx) = mpsc::channel();
        let holder = thread::spawn(move || {
            with_other_window(move |hwnd| unsafe {
                OpenClipboard(Some(hwnd)).unwrap();
                opened_tx.send(()).unwrap();
                thread::sleep(Duration::from_millis(30));
                let _ = CloseClipboard();
            })
        });
        opened_rx.recv().unwrap();
        let result = set_clipboard(&f.watcher.port(), &[text("retry")]);
        holder.join().unwrap();
        assert!(result.is_ok(), "{result:?}");
    }

    /// 監視の窓を渡して開いている間は、ほかのプロセス（clip.exe）が書けない（窓を渡さない
    /// `OpenClipboard(None)` では書けた）。同じプロセスの別の窓は、窓を渡さずに開いて
    /// いても開けなかった（変異の確認）ので、ほかのプロセスで確かめる。閉じた後に同じコマンドで
    /// 書けることも確かめる（コマンドが動かない環境で、失敗を排他と取り違えて合格しないため）。
    #[test]
    fn open_port_excludes_other_processes() {
        let f = fixture();
        let port = f.watcher.port();
        let clip = || {
            std::process::Command::new("cmd")
                .args(["/c", "echo CLCLR open_port_excludes_other_processes| clip"])
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
        };
        let guard = port.open().unwrap();
        let before = seq();
        assert!(!clip().success(), "開いている間にほかのプロセスが書けた");
        assert_eq!(seq(), before, "開いている間にクリップボードが変わった");
        drop(guard);
        assert!(clip().success(), "閉じた後もほかのプロセスが書けない（コマンドが動かない環境の可能性）");
        assert_ne!(seq(), before, "閉じた後のほかのプロセスの書き込みで番号が変わらない");
    }

    /// 書いた内容は、持ち主（監視の窓）が壊れた後もクリップボードに残る（遅延描画ではないため。
    /// 終了で監視を破棄した後に貼り付けられるか）。
    #[test]
    fn written_content_survives_watcher_drop() {
        let Fixture { watcher, _gui, _clipboard } = fixture();
        set_clipboard_suppressed(&watcher.port(), &[text("終了後も残る")], true).unwrap();
        assert_eq!(unsafe { GetClipboardOwner() }.ok(), Some(watcher.hwnd), "持ち主が監視の窓ではない");
        drop(watcher);
        let reader = super::test_support::test_watcher();
        assert_eq!(current_text(&reader.port()), "終了後も残る");
    }

    /// 画面に出る文言は日本語（頭に英語の「Win32:」を付けない）。
    #[test]
    fn clipboard_error_messages_are_japanese() {
        assert_eq!(ClipboardError::AllFormatsFailed.to_string(), "どの形式もクリップボードへ書けませんでした");
        assert_eq!(
            ClipboardError::AllFormatsFailedAfterEmpty.to_string(),
            "どの形式もクリップボードへ書けませんでした（クリップボードは空になっています）"
        );
        let win32 = ClipboardError::Win32(WinError::from_hresult(windows::Win32::Foundation::E_ACCESSDENIED)).to_string();
        assert!(win32.starts_with("クリップボードを使えません（") && !win32.contains("Win32"), "{win32}");
    }

    /// 変更番号 0（権限が無いとき）は記録せず、一致ともみなさない。閉じられなかったとき（`None`）も
    /// 記録を変えない。
    #[test]
    fn zero_or_missing_sequence_is_never_recorded() {
        let port = ClipboardPort::unopenable();
        port.record(Some(0));
        assert_eq!(port.suppressed_seq(), 0);
        assert!(!port.is_suppressed(0));
        port.record(Some(5));
        port.record(None);
        port.record(Some(0));
        assert!(port.is_suppressed(5));
        assert!(!port.is_suppressed(6));
    }

    /// 本物の監視（判定を観測する）と、その取り込み・判定の受け口。
    struct Observed {
        watcher: ClipboardWatcher,
        entries: mpsc::Receiver<Entry>,
        outcomes: mpsc::Receiver<Outcome>,
        /// `on_problem` に渡された知らせ
        problems: mpsc::Receiver<WatchProblem>,
        _gui: MutexGuard<'static, ()>,
        _clipboard: MutexGuard<'static, ()>,
    }

    fn observed(interval_ms: u64, watch: bool) -> Observed {
        observed_with(observed_config(interval_ms, watch), |_| false)
    }

    fn observed_config(interval_ms: u64, watch: bool) -> Config {
        let mut config = Config::default();
        config.general.clipboard_watch = watch;
        config.history.add_interval_ms = interval_ms;
        config
    }

    /// `panic_on` が真を返す項目では、取り込み（`on_entry`）がパニックする。
    fn observed_with(config: Config, panic_on: impl Fn(&Entry) -> bool + Send + 'static) -> Observed {
        let clipboard = lock_clipboard_tests();
        let gui = crate::tray::lock_gui_resource_tests();
        settle_clipboard();
        let (entry_tx, entries) = mpsc::channel();
        let (outcome_tx, outcomes) = mpsc::channel();
        let (problem_tx, problems) = mpsc::channel();
        let watcher = ClipboardWatcher::spawn_observed(
            Arc::new(RwLock::new(config)),
            move |entry| {
                if panic_on(&entry) {
                    panic!("テスト用の取り込みのパニック");
                }
                let _ = entry_tx.send(entry);
            },
            move |problem| {
                let _ = problem_tx.send(problem);
            },
            move |outcome| {
                let _ = outcome_tx.send(outcome);
            },
        )
        .unwrap();
        Observed { watcher, entries, outcomes, problems, _gui: gui, _clipboard: clipboard }
    }

    impl Observed {
        fn next_outcome(&self) -> Outcome {
            self.outcomes.recv_timeout(Duration::from_secs(5)).expect("監視の判定を観測できない")
        }
    }

    /// 抑止つきの送出だけなら、監視はその番号で飛ばし、取り込まない。
    #[test]
    fn watcher_skips_suppressed_write() {
        let o = observed(50, true);
        let port = o.watcher.port();
        set_clipboard_suppressed(&port, &[text("送った")], true).unwrap();
        assert_eq!(o.next_outcome(), Outcome::Suppressed { seq: port.suppressed_seq() });
        assert!(o.entries.try_recv().is_err(), "抑止した送出を取り込んだ");
    }

    /// 抑止つきの送出の直後（取り込みの間隔のうち）に変換の結果を書くと、まとめた1回で変換の結果を
    /// 取り込む（目印で一緒に飛ばす「漏れ」の回帰）。
    #[test]
    fn watcher_captures_conversion_right_after_suppressed_send() {
        let o = observed(500, true);
        let port = o.watcher.port();
        set_clipboard_suppressed(&port, &[text("送った")], true).unwrap();
        set_clipboard(&port, &[text("変換の結果")]).unwrap();
        let converted = seq();
        // 最初の判定が変換の後の番号なら、2つの書き込みはまとめられている（送出の番号の判定は無い）
        assert_eq!(o.next_outcome(), Outcome::Captured { seq: converted });
        assert_eq!(entry_text(&o.entries.try_recv().unwrap()), "変換の結果");
    }

    /// 抑止つきの送出の直後に、ほかの窓（ほかのアプリの代わり）が書いた内容も取り込む。
    #[test]
    fn watcher_captures_other_write_right_after_suppressed_send() {
        let o = observed(500, true);
        let port = o.watcher.port();
        set_clipboard_suppressed(&port, &[text("送った")], true).unwrap();
        with_other_window(|hwnd| set_clipboard(&ClipboardPort::new(hwnd), &[text("ほかのコピー")]).unwrap());
        let copied = seq();
        assert_eq!(o.next_outcome(), Outcome::Captured { seq: copied });
        assert_eq!(entry_text(&o.entries.try_recv().unwrap()), "ほかのコピー");
    }

    /// 書く側がロックの中にいる間は、監視は判定しない（待つ）。書き終えて記録した後に判定するので、
    /// 交差しても自分の書き込みを取り込まない（開く前に目印を見ると起きる「二重」の回帰）。
    #[test]
    fn watcher_waits_for_writer_and_skips_its_write() {
        let o = observed(50, true);
        let port = o.watcher.port();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let writer = {
            let port = Arc::clone(&port);
            thread::spawn(move || {
                with_open_port(&port, true, |_| {
                    entered_tx.send(()).unwrap();
                    let _ = release_rx.recv_timeout(Duration::from_secs(10));
                    Ok(())
                })
            })
        };
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        o.watcher.capture_now();
        // 取り込みの間隔（50ms）の数倍待っても判定が無い = 監視はロックで待っている
        assert!(
            o.outcomes.recv_timeout(Duration::from_millis(300)).is_err(),
            "書く側がロックを持っている間に監視が判定した"
        );
        release_tx.send(()).unwrap();
        writer.join().unwrap().unwrap();
        assert_eq!(o.next_outcome(), Outcome::Suppressed { seq: port.suppressed_seq() });
        assert!(o.entries.try_recv().is_err(), "書き込みと交差して取り込んだ");
    }

    /// 監視を切っている間の抑止つきの送出の記録は、監視を入れた後のコピーを飲み込まない（今までは
    /// 監視を切っている間に目印を立てないことで防いでいた）。
    #[test]
    fn record_while_watch_is_off_does_not_swallow_later_copy() {
        let o = observed(50, false);
        let port = o.watcher.port();
        set_clipboard_suppressed(&port, &[text("切っている間の送出")], true).unwrap();
        assert_ne!(port.suppressed_seq(), 0);
        o.watcher.set_watch(true).unwrap();
        set_clipboard(&port, &[text("後のコピー")]).unwrap();
        let copied = seq();
        assert_eq!(o.next_outcome(), Outcome::Captured { seq: copied });
        assert_eq!(entry_text(&o.entries.try_recv().unwrap()), "後のコピー");
    }

    /// まとめ待ちの間に監視を切ったら、待っていた変更を取り込まない。
    #[test]
    fn change_pending_when_watch_is_turned_off_is_not_captured() {
        let o = observed(500, true);
        let port = o.watcher.port();
        set_clipboard(&port, &[text("切る前のコピー")]).unwrap();
        o.watcher.set_watch(false).unwrap();
        assert_eq!(o.next_outcome(), Outcome::WatchOff);
        assert!(o.entries.try_recv().is_err(), "監視を切った後に取り込んだ");
    }

    /// 取り込みがパニックしても、ワーカーは続く。監視を止めて（トレイと共有する状態も）知らせ、止めている間の
    /// コピーは取り込まない。監視を戻せば、次のコピーを取り込む。
    #[test]
    fn panic_in_capture_stops_watch_and_worker_keeps_running() {
        let o = observed_with(observed_config(50, true), |entry| entry_text(entry) == "パニックする");
        let port = o.watcher.port();
        let shared = o.watcher.watch_state();

        set_clipboard(&port, &[text("パニックする")]).unwrap();
        assert_eq!(o.next_outcome(), Outcome::Panicked);
        let problem = o.problems.try_recv().expect("パニックを知らせていない");
        assert!(matches!(problem, WatchProblem::Panicked(Ok(()))), "監視を止められなかった: {problem:?}");
        assert!(o.problems.try_recv().is_err(), "1回のパニックで2回知らせた");
        assert!(!o.watcher.listening() && !shared.load(Ordering::SeqCst), "監視が止まっていない");
        assert!(o.entries.try_recv().is_err());

        // 止めている間のコピーは取り込まない（通知の登録も外れているので、判定も来ない）
        set_clipboard(&port, &[text("止めている間")]).unwrap();
        assert!(o.outcomes.recv_timeout(Duration::from_millis(300)).is_err(), "止めた後に判定した");

        o.watcher.set_watch(true).unwrap();
        set_clipboard(&port, &[text("戻した後")]).unwrap();
        let copied = seq();
        assert_eq!(o.next_outcome(), Outcome::Captured { seq: copied });
        assert_eq!(entry_text(&o.entries.try_recv().unwrap()), "戻した後");
        assert!(o.problems.try_recv().is_err());
    }

    /// 取り込む形式の大きさの合計が上限を超えたコピーは、何も取り込まずに知らせる。上限以下なら取り込み、
    /// 0 は無制限。合計は、形式フィルタで取り込まない形式を数えない。
    #[test]
    fn copy_over_total_limit_is_not_captured_and_is_reported() {
        let mut config = observed_config(50, true);
        // 「テキスト」は UTF-16 と終端で 10 バイト（`GlobalSize` は確保の単位で丸められうるので、余裕を見る）
        config.capture_total_limit = 64;
        let o = observed_with(config.clone(), |_| false);
        let port = o.watcher.port();

        let big = "あ".repeat(100);
        set_clipboard(&port, &[text(&big)]).unwrap();
        let copied = seq();
        assert_eq!(o.next_outcome(), Outcome::TooLarge { seq: copied });
        let problem = o.problems.try_recv().expect("上限を超えたことを知らせていない");
        let WatchProblem::TooLarge(TooLarge { total, limit }) = problem else { panic!("{problem:?}") };
        assert!(total > 64 && limit == 64, "{total} {limit}");
        assert!(o.entries.try_recv().is_err(), "上限を超えたコピーを取り込んだ");

        // 取り込まない形式（CF_TEXT など、既定で無視する形式）は合計に数えない
        set_clipboard(&port, &[text("テキスト"), Format { format_name: "CF_TEXT".to_string(), format_id: 1, data: vec![b'x'; 200] }])
            .unwrap();
        let copied = seq();
        assert_eq!(o.next_outcome(), Outcome::Captured { seq: copied });
        assert_eq!(entry_text(&o.entries.try_recv().unwrap()), "テキスト");
        assert!(o.problems.try_recv().is_err());
    }

    #[test]
    fn total_limit_zero_is_unlimited() {
        assert!(!exceeds_total(u64::MAX, 0));
        assert!(!exceeds_total(64, 64));
        assert!(exceeds_total(65, 64));
    }

    /// 回帰: サイズ0は上限設定に関わらず常に不採用（空データのFormatを履歴に積まない）。
    #[test]
    fn should_capture_size_rejects_zero_regardless_of_limit() {
        assert!(!should_capture_size(0, 0));
        assert!(!should_capture_size(0, 1000));
    }

    /// 上限0は「無制限」を意味し、非0サイズは常に採用。上限指定時は上限以下のみ採用。
    #[test]
    fn should_capture_size_respects_limit() {
        assert!(should_capture_size(1, 0));
        assert!(should_capture_size(1_000_000, 0));
        assert!(should_capture_size(100, 100));
        assert!(!should_capture_size(101, 100));
    }
}
