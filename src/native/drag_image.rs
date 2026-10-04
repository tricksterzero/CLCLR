//! ドラッグ中の行の絵。ビューアの一覧の行・ツリーのフォルダをドラッグしている間、カーソルに付いて動く半透明の小窓。
//!
//! 絵はビューアが描いたビットマップをそのまま写す（描くのは `viewer`）。窓はビューアを持ち主にして、持ち主より
//! 常に前に出し、持ち主と一緒に破棄されるようにする（Microsoft Learn の Window Features の Owned Windows）。
//! `WS_EX_LAYERED` で半透明にし（`SetLayeredWindowAttributes`）、`WS_EX_TRANSPARENT` でマウスを下の窓へ
//! 素通しし、`WS_EX_NOACTIVATE` でフォーカスを奪わない。

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, COLORREF, ERROR_CLASS_ALREADY_EXISTS, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, DeleteDC, DeleteObject, EndPaint, SelectObject, UpdateWindow,
    HBITMAP, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowLongPtrW, RegisterClassW, SetLayeredWindowAttributes,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, GWLP_USERDATA, LWA_ALPHA, MA_NOACTIVATE, SWP_NOACTIVATE,
    SWP_NOSIZE, SWP_NOZORDER, SW_SHOWNOACTIVATE, WM_ERASEBKGND, WM_MOUSEACTIVATE, WM_NCDESTROY, WM_NCHITTEST,
    WM_PAINT, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};

const CLASS_NAME: PCWSTR = w!("CLCLR_DragImage");

/// 不透明度（0 は透明、255 は不透明）
const ALPHA: u8 = 0xC0;

/// 窓の `GWLP_USERDATA` に `Box` で置く絵。`WM_NCDESTROY` で解放する。
struct Picture {
    bitmap: HBITMAP,
    width: i32,
    height: i32,
}

/// `bitmap`（大きさ `width`×`height`）を写す小窓を、画面座標 (`x`, `y`) を左上にして出す。`bitmap` の持ち主は
/// 窓になり、窓の破棄で解放する（作れなければここで解放して None）。
pub fn show(owner: HWND, bitmap: HBITMAP, width: i32, height: i32, x: i32, y: i32) -> Option<HWND> {
    let free = || unsafe {
        let _ = DeleteObject(bitmap.into());
    };
    let Some(hwnd) = create(owner, width, height, x, y) else {
        free();
        return None;
    };
    let picture = Box::into_raw(Box::new(Picture { bitmap, width, height }));
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, picture as isize);
        // 作った直後のレイヤードウィンドウは、これを呼ぶまで見えない（Microsoft Learn の Layered Windows）
        if SetLayeredWindowAttributes(hwnd, COLORREF(0), ALPHA, LWA_ALPHA).is_err() {
            let _ = DestroyWindow(hwnd);
            return None;
        }
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = UpdateWindow(hwnd);
    }
    Some(hwnd)
}

fn create(owner: HWND, width: i32, height: i32, x: i32, y: i32) -> Option<HWND> {
    unsafe {
        let hinstance = GetModuleHandleW(None).ok()?.into();
        let class = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinstance, lpszClassName: CLASS_NAME, ..Default::default() };
        // テストなどで2回目以降に呼ばれたときは登録済みでよい
        if RegisterClassW(&class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            return None;
        }
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            CLASS_NAME,
            PCWSTR::null(),
            WS_POPUP,
            x,
            y,
            width,
            height,
            Some(owner),
            None,
            Some(hinstance),
            None,
        )
        .ok()
    }
}

/// 小窓の左上を画面座標 (`x`, `y`) へ動かす。
pub fn move_to(hwnd: HWND, x: i32, y: i32) {
    unsafe {
        let _ = SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

/// 小窓を破棄する（絵は `WM_NCDESTROY` で解放される）。
pub fn destroy(hwnd: HWND) {
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_NCHITTEST => LRESULT(-1), // HTTRANSPARENT
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                if let Some(picture) = (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Picture).as_ref() {
                    let mem = CreateCompatibleDC(Some(hdc));
                    let old = SelectObject(mem, picture.bitmap.into());
                    let _ = BitBlt(hdc, 0, 0, picture.width, picture.height, Some(mem), 0, 0, SRCCOPY);
                    SelectObject(mem, old);
                    let _ = DeleteDC(mem);
                }
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                let picture = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Picture;
                if !picture.is_null() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    let picture = Box::from_raw(picture);
                    let _ = DeleteObject(picture.bitmap.into());
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
