//! UI スレッド（自分の窓とメッセージループを持つスレッド。ホットキー・トレイ）の止め方。
//!
//! 止める側（メインスレッド。ビューア窓の持ち主）が `join` でただ待つと、止めようとしている
//! スレッドがメインスレッドの窓へメッセージを送った（`SendMessage` の類）ときに、互いに待って
//! 止まる（ホットキースレッドの前面化は、前面のスレッドへ
//! `AttachThreadInput` して `SetForegroundWindow` を呼ぶので、ビューアが前面ならメインスレッドへ
//! 活性化のメッセージが送られる）。ここでは、送られたメッセージに応じながら、スレッドが終わるまで
//! 待つ。

use std::os::windows::io::AsRawHandle;
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Foundation::{GetLastError, HANDLE, HWND, LPARAM, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    MsgWaitForMultipleObjects, PeekMessageW, PostMessageW, MSG, PM_NOREMOVE, QS_SENDMESSAGE,
};

/// 止める依頼を投げ直す間隔。
pub(crate) const REPOST_INTERVAL: Duration = Duration::from_millis(500);

/// `hwnd` へ `shutdown_msg` を投げ、`thread` が終わるまで待って join する（上限なし）。
///
/// 待つ間、このスレッドの窓へ送られたメッセージとシステムの内部の事象は処理する
/// （`PeekMessage(PM_NOREMOVE)`。投稿されたメッセージは取り除かず、配送もしない。Microsoft Learn の
/// PeekMessageW・MsgWaitForMultipleObjects）。ビューア窓は呼ぶ前に後片付け中にしておく
/// （`viewer::begin_teardown`。送られたメッセージでアプリの処理を呼ばない）。
///
/// `repost_interval` ごとに `shutdown_msg` を投げ直す。止めようとしているスレッドがメニューの
/// モーダルループの中なら、そのたびに `EndMenu` をやり直すことになる（できれば、の扱い。`EndMenu` の
/// 失敗からの回復は保証しない）。投げ直しの失敗（窓がもう無い）は無視して待ち続ける。
///
/// 待つ処理そのものが失敗したら（`WAIT_FAILED`）、ログを書いて `join` で待つ。この経路では送られた
/// メッセージに応じられないので、#10 の相互待ちは解けない（失敗のときだけの縮退）。
pub(crate) fn stop_ui_thread(hwnd: HWND, shutdown_msg: u32, thread: JoinHandle<()>, repost_interval: Duration) {
    let post = || unsafe {
        let _ = PostMessageW(Some(hwnd), shutdown_msg, WPARAM(0), LPARAM(0));
    };
    post();
    let handle = HANDLE(thread.as_raw_handle());
    let wait_ms = u32::try_from(repost_interval.as_millis()).unwrap_or(u32::MAX);
    loop {
        let result = unsafe { MsgWaitForMultipleObjects(Some(&[handle]), false, wait_ms, QS_SENDMESSAGE) };
        if result == WAIT_OBJECT_0 {
            break;
        } else if result.0 == WAIT_OBJECT_0.0 + 1 {
            // 送られたメッセージ・システムの事象。1回処理して待ち直す（確かめた後の入力は「古い」扱いに
            // なり、新しい入力が来るまで待ちは起きない）
            let mut msg = MSG::default();
            unsafe {
                let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
            }
        } else if result == WAIT_TIMEOUT {
            post();
        } else {
            let error = unsafe { GetLastError() };
            eprintln!(
                "スレッドの終わりを待てませんでした（{result:?}、{error:?}）。送られたメッセージに応じずに待ちます"
            );
            break;
        }
    }
    let _ = thread.join();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Instant;
    use windows::core::w;
    use windows::Win32::Foundation::LRESULT;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, PostQuitMessage,
        RegisterClassW, SendMessageW, HWND_MESSAGE, PM_REMOVE, WINDOW_EX_STYLE, WM_APP, WM_USER, WNDCLASSW,
        WS_OVERLAPPED,
    };

    const WM_TEST_SHUTDOWN: u32 = WM_USER + 50;
    const WM_TEST_PING: u32 = WM_APP + 50;
    const WM_TEST_POSTED: u32 = WM_APP + 51;

    thread_local! {
        /// このスレッドの窓が受けた WM_TEST_PING・WM_TEST_POSTED の数
        static PINGS: Cell<usize> = const { Cell::new(0) };
        static POSTED: Cell<usize> = const { Cell::new(0) };
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match msg {
            WM_TEST_PING => {
                PINGS.with(|p| p.set(p.get() + 1));
                LRESULT(1)
            }
            WM_TEST_POSTED => {
                POSTED.with(|p| p.set(p.get() + 1));
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    fn register() {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: unsafe { GetModuleHandleW(None) }.unwrap().into(),
            lpszClassName: w!("CLCLR_UiThreadTest"),
            ..Default::default()
        };
        unsafe { RegisterClassW(&wc) };
    }

    /// このスレッドの窓（メッセージ専用）。
    fn own_window() -> HWND {
        register();
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("CLCLR_UiThreadTest"),
                None,
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
            .unwrap()
        }
    }

    struct SendHwnd(isize);
    unsafe impl Send for SendHwnd {}

    /// 止める依頼（WM_TEST_SHUTDOWN）を受けたら `on_shutdown` を行い、`quit_after` 回目で抜ける UI スレッド。
    /// 窓と、受けた止める依頼の数を返す。
    fn spawn_ui(
        quit_after: usize,
        on_shutdown: impl Fn(usize) + Send + 'static,
    ) -> (HWND, JoinHandle<()>, Arc<AtomicUsize>) {
        let (tx, rx) = mpsc::channel();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        let thread = std::thread::spawn(move || {
            let hwnd = own_window();
            tx.send(SendHwnd(hwnd.0 as isize)).unwrap();
            let mut msg = MSG::default();
            while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
                if msg.message == WM_TEST_SHUTDOWN {
                    let n = seen.fetch_add(1, Ordering::SeqCst) + 1;
                    on_shutdown(n);
                    if n >= quit_after {
                        unsafe {
                            let _ = DestroyWindow(hwnd);
                            PostQuitMessage(0);
                        }
                    }
                    continue;
                }
                unsafe { DispatchMessageW(&msg) };
            }
        });
        let hwnd = HWND(rx.recv().unwrap().0 as *mut _);
        (hwnd, thread, count)
    }

    /// 止める依頼を受けたらすぐ終わるスレッドは、そのまま止まる。
    #[test]
    fn stops_thread_that_quits_on_request() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (hwnd, thread, count) = spawn_ui(1, |_| {});
        stop_ui_thread(hwnd, WM_TEST_SHUTDOWN, thread, Duration::from_millis(500));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    /// 止める依頼を受けたスレッドが、終わる前に止める側の窓へ `SendMessage` しても、止める側がそれに
    /// 応じるので止まらない（ただ join で待つと、互いに待って止まる。#10 の仕組み）。止める側の窓へ
    /// 投稿されたメッセージは、待つ間は配送されずに列に残る。見張りの別スレッドで、止まったときに
    /// テストを失敗させる。
    #[test]
    fn answers_sent_messages_while_waiting_and_leaves_posted_ones() {
        let _gui = crate::tray::lock_gui_resource_tests();
        PINGS.with(|p| p.set(0));
        POSTED.with(|p| p.set(0));
        let waiter = own_window();
        let waiter_raw = waiter.0 as isize;
        unsafe {
            let _ = PostMessageW(Some(waiter), WM_TEST_POSTED, WPARAM(0), LPARAM(0));
        }
        let (hwnd, thread, _count) = spawn_ui(1, move |_| unsafe {
            SendMessageW(HWND(waiter_raw as *mut _), WM_TEST_PING, None, None);
        });
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let watchdog = std::thread::spawn(move || done_rx.recv_timeout(Duration::from_secs(10)).is_ok());
        let started = Instant::now();
        stop_ui_thread(hwnd, WM_TEST_SHUTDOWN, thread, Duration::from_millis(500));
        let _ = done_tx.send(());
        assert!(watchdog.join().unwrap(), "止める側が送られたメッセージに応じず、互いに待った");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(PINGS.with(Cell::get), 1, "送られたメッセージに応じていない");
        assert_eq!(POSTED.with(Cell::get), 0, "待つ間に投稿されたメッセージを配送した");
        // 列に残っている
        let mut msg = MSG::default();
        let left = unsafe { PeekMessageW(&mut msg, Some(waiter), WM_TEST_POSTED, WM_TEST_POSTED, PM_REMOVE) };
        assert!(left.as_bool(), "投稿されたメッセージが列から消えた");
        unsafe {
            let _ = DestroyWindow(waiter);
        }
    }

    /// すぐには終わらないスレッドへは、止める依頼を投げ直し、終わったら戻る。
    #[test]
    fn reposts_shutdown_until_thread_ends() {
        let _gui = crate::tray::lock_gui_resource_tests();
        let (hwnd, thread, count) = spawn_ui(3, |_| {});
        let started = Instant::now();
        stop_ui_thread(hwnd, WM_TEST_SHUTDOWN, thread, Duration::from_millis(100));
        assert_eq!(count.load(Ordering::SeqCst), 3, "投げ直していない");
        assert!(started.elapsed() >= Duration::from_millis(150));
    }
}
