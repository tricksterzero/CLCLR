//! UI Automation でキャレットの位置を取る（Win32 のシステムキャレットを公開しないアプリ向け。
//! Chromium・Electron・WPF・WinUI など）。
//!
//! UI Automation のクライアントは、窓を持たない MTA のスレッドから呼ぶ（Microsoft Learn の
//! 「Understanding Threading Issues」）。呼び出しは専用のスレッドで行い、メニューを出す側は
//! 短い時間だけ応答を待って、間に合わなければマウスの位置に出す。専用のスレッドは初めて
//! 問い合わせるときに作り、`UiaCaret` を捨てるまで残す。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::core::{Interface, BOOL};
use windows::Win32::Foundation::RECT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, SAFEARRAY,
};
use windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayDestroy, SafeArrayGetLBound, SafeArrayGetUBound, SafeArrayUnaccessData,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationElement, IUIAutomationTextPattern,
    IUIAutomationTextPattern2, IUIAutomationTextRange, TextPatternRangeEndpoint_Start, TextUnit_Character,
    UIA_TextPattern2Id, UIA_TextPatternId,
};

/// UI Automation の1回の呼び出しで、要素の取得・情報の取得それぞれを待つ時間の上限（ミリ秒。既定は2秒・
/// 20秒）。キャレットの位置を1つ求めるのに何回も呼ぶので、応答しないアプリでは、問い合わせ全体はこれより長くかかりうる
const UIA_TIMEOUT_MS: u32 = 300;

/// メニューを出す側が応答を待つ時間。問い合わせ全体の上限ではないので、間に合わなければマウスの位置に出し、
/// その問い合わせが終わるまでの次の問い合わせは待たずに諦める（`UiaCaret::query`）
pub const QUERY_WAIT: Duration = Duration::from_millis(400);

/// UI Automation のオブジェクトを作り、待ち時間の上限を短くする（`IUIAutomation2` は Windows 8 以降）。
fn create_automation() -> windows::core::Result<IUIAutomation> {
    unsafe {
        let uia: IUIAutomation = CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)?;
        if let Ok(uia2) = uia.cast::<IUIAutomation2>() {
            let _ = uia2.SetConnectionTimeout(UIA_TIMEOUT_MS);
            let _ = uia2.SetTransactionTimeout(UIA_TIMEOUT_MS);
        }
        Ok(uia)
    }
}

/// 範囲の最初の矩形（left, top, width, height。スクリーン座標）。
fn first_rect(range: &IUIAutomationTextRange) -> Option<[f64; 4]> {
    unsafe {
        let psa: *mut SAFEARRAY = range.GetBoundingRectangles().ok()?;
        if psa.is_null() {
            return None;
        }
        let mut out = None;
        if let (Ok(lb), Ok(ub)) = (SafeArrayGetLBound(psa, 1), SafeArrayGetUBound(psa, 1)) {
            let mut data = std::ptr::null_mut();
            if ub - lb + 1 >= 4 && SafeArrayAccessData(psa, &mut data).is_ok() {
                let v = std::slice::from_raw_parts(data as *const f64, 4);
                out = Some([v[0], v[1], v[2], v[3]]);
                let _ = SafeArrayUnaccessData(psa);
            }
        }
        let _ = SafeArrayDestroy(psa);
        out
    }
}

/// 幅のない範囲（キャレット）の左上。矩形が無ければ右の1文字の左端、それも無ければ（行末・文末）
/// 左の1文字の右端を使う。
fn caret_point_of(range: &IUIAutomationTextRange) -> Option<(i32, i32)> {
    unsafe {
        if let Some(r) = first_rect(range) {
            return Some((r[0] as i32, r[1] as i32));
        }
        let next = range.Clone().ok()?;
        if next.ExpandToEnclosingUnit(TextUnit_Character).is_ok() {
            if let Some(r) = first_rect(&next) {
                return Some((r[0] as i32, r[1] as i32));
            }
        }
        let prev = range.Clone().ok()?;
        if prev.MoveEndpointByUnit(TextPatternRangeEndpoint_Start, TextUnit_Character, -1).is_ok() {
            if let Some(r) = first_rect(&prev) {
                return Some(((r[0] + r[2]) as i32, r[1] as i32));
            }
        }
        None
    }
}

/// キャレットの位置が、フォーカスのある要素の矩形（両端を含む）の中にあるか。矩形が空なら外とする。
/// 要素の矩形が空で、キャレットの x が行の先頭になるアプリがある（VS Code の既定の入力方式）ため、
/// 中に無い位置は使わない。
fn caret_in_bounds(point: (i32, i32), bounds: RECT) -> bool {
    bounds.right > bounds.left
        && bounds.bottom > bounds.top
        && (bounds.left..=bounds.right).contains(&point.0)
        && (bounds.top..=bounds.bottom).contains(&point.1)
}

/// フォーカスのある要素のキャレットの位置（スクリーン座標）。`TextPattern2::GetCaretRange` を先に試し、
/// 無ければ `TextPattern` の選択範囲の先頭を使う。
fn caret_point(uia: &IUIAutomation) -> Option<(i32, i32)> {
    unsafe {
        let element: IUIAutomationElement = uia.GetFocusedElement().ok()?;
        let point = (|| {
            if let Ok(tp2) = element.GetCurrentPatternAs::<IUIAutomationTextPattern2>(UIA_TextPattern2Id) {
                let mut active = BOOL(0);
                if let Ok(range) = tp2.GetCaretRange(&mut active) {
                    if active.as_bool() {
                        if let Some(p) = caret_point_of(&range) {
                            return Some(p);
                        }
                    }
                }
            }
            let tp: IUIAutomationTextPattern =
                element.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId).ok()?;
            let selection = tp.GetSelection().ok()?;
            if selection.Length().ok()? < 1 {
                return None;
            }
            caret_point_of(&selection.GetElement(0).ok()?)
        })()?;
        let bounds = element.CurrentBoundingRectangle().ok()?;
        caret_in_bounds(point, bounds).then_some(point)
    }
}

/// このスレッドの COM の初期化（MTA）。捨てるときに解放する。
struct ComApartment;

impl ComApartment {
    /// 初期化できたときだけ作る（失敗したときに解放を呼ばないよう、`then_some(Self)` のように先に作らない）。
    fn init() -> Option<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok().then(|| Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

/// スレッドが持つ UI Automation。フィールドは宣言の順に捨てられるので、`uia` を COM の解放より先に放す。
struct Automation {
    uia: IUIAutomation,
    _com: ComApartment,
}

/// 位置の問い合わせ。期限を過ぎてから取り出したものは、返事を待つ相手がいないので問い合わせない。
struct Request {
    deadline: Instant,
    reply: Sender<Option<(i32, i32)>>,
}

/// UI Automation の呼び出しを受け持つスレッド（窓を持たない MTA）。
pub struct UiaCaret {
    requests: Option<Sender<Request>>,
    /// 依頼を送ってから、スレッドがそれを片付けるまで立つ（応答しないアプリで止まっている間も立ったまま）。
    /// 立っている間は次の依頼を送らないので、積まれる依頼は1つまで
    busy: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl UiaCaret {
    /// 専用のスレッドを作る。作れなければ `None`。
    pub fn spawn() -> Option<Self> {
        Self::spawn_with(|| {
            let com = ComApartment::init()?;
            let uia = create_automation().ok()?;
            let automation = Automation { uia, _com: com };
            // 構造体ごと取り込む（`automation.uia` と書くと、そのフィールドだけを取り込み、COM がここで解放される）
            Some(move || {
                let automation = &automation;
                caret_point(&automation.uia)
            })
        })
    }

    /// `start` をスレッドで呼んで問い合わせの関数を作り、依頼ごとにそれを呼ぶスレッドを作る（テストでは
    /// 問い合わせの中身を差し替える）。`start` が `None` を返せば、スレッドは依頼を受けずに終わる。
    fn spawn_with<S, P>(start: S) -> Option<Self>
    where
        S: FnOnce() -> Option<P> + Send + 'static,
        P: FnMut() -> Option<(i32, i32)>,
    {
        let (tx, rx) = mpsc::channel::<Request>();
        let busy = Arc::new(AtomicBool::new(false));
        let flag = busy.clone();
        let thread = thread::Builder::new()
            .name("clclr-uia".to_string())
            .spawn(move || {
                if let Some(mut probe) = start() {
                    while let Ok(request) = rx.recv() {
                        let point = (Instant::now() < request.deadline).then(&mut probe);
                        // 返事を受け取った側がすぐ次を問い合わせられるよう、返事より先に下ろす
                        flag.store(false, Ordering::SeqCst);
                        if let Some(point) = point {
                            let _ = request.reply.send(point);
                        }
                    }
                }
                // 口を閉じてから下ろす（この後の依頼は送れずに、送った側が下ろす）
                drop(rx);
                flag.store(false, Ordering::SeqCst);
            })
            .ok()?;
        Some(Self { requests: Some(tx), busy, thread: Some(thread) })
    }

    /// キャレットの位置を問い合わせ、`wait` だけ待つ。前の問い合わせがまだ片付いていなければ
    /// （応答しないアプリで止まっている、初めの準備が終わっていない）、依頼を送らずに `None` を返す。
    pub fn query(&self, wait: Duration) -> Option<(i32, i32)> {
        let requests = self.requests.as_ref()?;
        if self.busy.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            return None;
        }
        let (reply, answer) = mpsc::channel();
        if requests.send(Request { deadline: Instant::now() + wait, reply }).is_err() {
            self.busy.store(false, Ordering::SeqCst);
            return None;
        }
        answer.recv_timeout(wait).ok().flatten()
    }
}

impl Drop for UiaCaret {
    /// 依頼の口を閉じてスレッドを終わらせ、終わるのを待つ。積まれている依頼は1つまでで、期限を過ぎていれば
    /// 問い合わせないので、待つのは問い合わせの最中のものだけ。
    fn drop(&mut self) {
        self.requests = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT { left, top, right, bottom }
    }

    #[test]
    fn caret_inside_bounds_including_edges_is_used() {
        let bounds = rect(228, 251, 728, 451);
        assert!(caret_in_bounds((303, 253), bounds));
        assert!(caret_in_bounds((228, 251), bounds));
        assert!(caret_in_bounds((728, 451), bounds));
    }

    #[test]
    fn caret_outside_bounds_is_not_used() {
        let bounds = rect(228, 251, 728, 451);
        assert!(!caret_in_bounds((227, 300), bounds));
        assert!(!caret_in_bounds((400, 452), bounds));
    }

    #[test]
    fn empty_bounds_reject_any_caret() {
        // 要素の矩形が (0,0)-(0,0) で、キャレットの x が行の先頭になる場合
        assert!(!caret_in_bounds((0, 0), rect(0, 0, 0, 0)));
        assert!(!caret_in_bounds((120, 300), rect(0, 0, 0, 0)));
        assert!(!caret_in_bounds((5, 5), rect(10, 10, 10, 40)));
    }

    #[test]
    fn query_returns_within_wait_and_thread_ends_on_drop() {
        // UI Automation のクライアントが窓などを作りうるので、GUI リソースのテストと直列にする
        let _gui = crate::tray::lock_gui_resource_tests();
        let uia = UiaCaret::spawn().expect("UI Automation のスレッドを作れなかった");
        for _ in 0..2 {
            let started = Instant::now();
            // 前面の窓によって位置が取れるかは変わるので、結果は見ず、待ちが上限で戻ることだけを見る
            let _ = uia.query(QUERY_WAIT);
            assert!(started.elapsed() < QUERY_WAIT + Duration::from_millis(200), "待ちが上限を超えた");
        }
        drop(uia);
    }

    /// 待つ時間を長くしても、依頼を送らずに戻る問い合わせはすぐ戻る（送っていれば、この時間まで待つ）。
    const LONG_WAIT: Duration = Duration::from_secs(10);
    const QUICK: Duration = Duration::from_secs(5);

    /// 問い合わせの最中（応答しないアプリで止まっている）は、次の依頼を送らずにすぐ諦め、終われば次の
    /// 問い合わせに答える。問い合わせの始まりと終わりはチャネルで決める。
    #[test]
    fn query_while_previous_is_running_gives_up_at_once() {
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let uia = UiaCaret::spawn_with(move || {
            Some(move || {
                let _ = started_tx.send(());
                release_rx.recv().ok()?;
                Some((1, 2))
            })
        })
        .unwrap();
        thread::scope(|scope| {
            let first = scope.spawn(|| uia.query(LONG_WAIT));
            started_rx.recv().unwrap();
            let started = Instant::now();
            assert_eq!(uia.query(LONG_WAIT), None);
            assert!(started.elapsed() < QUICK, "最中なのに依頼を送って待った");
            release_tx.send(()).unwrap();
            assert_eq!(first.join().unwrap(), Some((1, 2)));
        });
        release_tx.send(()).unwrap();
        assert_eq!(uia.query(LONG_WAIT), Some((1, 2)), "終わった後の問い合わせに答えない");
    }

    /// 準備の間の問い合わせで送る依頼は最初の1つだけで、期限を過ぎてから取り出したそれは問い合わせない。
    /// 準備が終わってすぐ終了しても、積まれた依頼を問い合わせずに終わる。
    #[test]
    fn expired_request_is_not_probed_and_drop_does_not_wait_for_it() {
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let uia = UiaCaret::spawn_with(move || {
            ready_rx.recv().ok()?;
            Some(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                Some((3, 4))
            })
        })
        .unwrap();
        // 準備が終わらないので、期限を過ぎて戻る。依頼は積まれたまま
        assert_eq!(uia.query(Duration::from_millis(20)), None);
        let started = Instant::now();
        assert_eq!(uia.query(LONG_WAIT), None);
        assert!(started.elapsed() < QUICK, "積まれているのに依頼を送って待った");
        ready_tx.send(()).unwrap();
        drop(uia);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "期限を過ぎた依頼を問い合わせた");
    }

    /// 準備に失敗したスレッドへの問い合わせは、待たずに `None` になり続ける（処理中の印が立ったままにならない）。
    #[test]
    fn failed_start_answers_none_without_waiting() {
        let uia = UiaCaret::spawn_with(|| None::<fn() -> Option<(i32, i32)>>).unwrap();
        for _ in 0..3 {
            let started = Instant::now();
            assert_eq!(uia.query(LONG_WAIT), None);
            assert!(started.elapsed() < QUICK, "準備に失敗したのに待った");
        }
    }

    /// COM の初期化に失敗したときは解放を呼ばない（呼ぶと、そのスレッドの前からの初期化を外してしまう）。
    /// 先に STA で初期化したスレッドでは MTA の初期化が失敗し、その後も STA の初期化が残っている。
    #[test]
    fn failed_com_init_does_not_uninitialize() {
        use windows::Win32::Foundation::S_FALSE;
        use windows::Win32::System::Com::COINIT_APARTMENTTHREADED;
        // STA の初期化は COM の隠し窓を作りうるので、GUI リソースのテストと直列にする
        let _gui = crate::tray::lock_gui_resource_tests();
        thread::spawn(|| unsafe {
            assert!(CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok());
            let failed = ComApartment::init().is_none();
            // まだ初期化されていれば S_FALSE（解放されていれば S_OK で初期化し直す）
            let again = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            if again.is_ok() {
                CoUninitialize();
            }
            CoUninitialize();
            assert!(failed, "STA のスレッドで MTA の初期化ができた");
            assert_eq!(again, S_FALSE, "失敗した初期化で解放した");
        })
        .join()
        .unwrap();
    }
}
