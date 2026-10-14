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
use std::time::Duration;

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

/// 要素の取得・情報の取得それぞれの待ち時間の上限（ミリ秒。既定は2秒・20秒）。応答しないアプリでは、
/// 1回の問い合わせがこの程度で打ち切られる
const UIA_TIMEOUT_MS: u32 = 300;

/// メニューを出す側が応答を待つ時間。打ち切りの上限より少し長くし、打ち切られた結果を受け取る
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

/// UI Automation の呼び出しを受け持つスレッド（窓を持たない MTA）。
pub struct UiaCaret {
    requests: Option<Sender<Sender<Option<(i32, i32)>>>>,
    /// 問い合わせの最中（応答しないアプリで止まっている間も立ったまま）
    busy: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl UiaCaret {
    /// 専用のスレッドを作る。作れなければ `None`。
    pub fn spawn() -> Option<Self> {
        let (tx, rx) = mpsc::channel::<Sender<Option<(i32, i32)>>>();
        let busy = Arc::new(AtomicBool::new(false));
        let flag = busy.clone();
        let thread = thread::Builder::new()
            .name("clclr-uia".to_string())
            .spawn(move || unsafe {
                if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
                    return;
                }
                if let Ok(uia) = create_automation() {
                    for reply in rx {
                        flag.store(true, Ordering::SeqCst);
                        let point = caret_point(&uia);
                        flag.store(false, Ordering::SeqCst);
                        let _ = reply.send(point);
                    }
                }
                CoUninitialize();
            })
            .ok()?;
        Some(Self { requests: Some(tx), busy, thread: Some(thread) })
    }

    /// キャレットの位置を問い合わせ、`wait` だけ待つ。前の問い合わせがまだ終わっていなければ
    /// （応答しないアプリで止まっている）、待たずに `None` を返す。
    pub fn query(&self, wait: Duration) -> Option<(i32, i32)> {
        if self.busy.load(Ordering::SeqCst) {
            return None;
        }
        let (tx, rx) = mpsc::channel();
        self.requests.as_ref()?.send(tx).ok()?;
        rx.recv_timeout(wait).ok().flatten()
    }
}

impl Drop for UiaCaret {
    /// 依頼の口を閉じてスレッドを終わらせ、終わるのを待つ（問い合わせの最中なら、打ち切りの上限まで）。
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
    use std::time::Instant;

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
}
