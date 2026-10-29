//! 履歴に項目を足したときの追加音（C版 CLCL の tool_utl「音を鳴らす」の統合）。鳴らすかは呼び出し側が決める
//! （監視の取り込みの後に、`ops::capture_added` と設定の `history.sound_on_add` を見て `main` が呼ぶ）。

use std::sync::Mutex;

/// 追加音を鳴らす（監視のワーカーのスレッドから呼ばれる）。ファイル未指定・再生失敗時はシステム音にフォールバック
/// （C版と同じ）。ファイルは `sound_file_path` の絶対パスで鳴らす。鳴らし終わるのは待たない（`SND_ASYNC`）。
pub fn play_add_sound(sound_file: &str) {
    use windows::core::PCWSTR;
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FILENAME, SND_NODEFAULT};
    // MessageBeepはwinuser.h由来だがwindows crateではDiagnostics::Debug配下
    use windows::Win32::System::Diagnostics::Debug::MessageBeep;
    use windows::Win32::UI::WindowsAndMessaging::MB_ICONASTERISK;

    // ファイル名のバッファを関数を抜けた後も持つ必要は、PlaySound の文書には無い（SND_ASYNC で呼び出しの後も
    // 生かしておくよう求めているのは SND_MEMORY の音のデータ）。C版がグローバルのバッファを渡すのに合わせて、
    // 次の再生まで持っている
    static SOUND_BUF: Mutex<Vec<u16>> = Mutex::new(Vec::new());

    let played = match sound_file_path(sound_file) {
        None => false,
        Some(path) => {
            use std::os::windows::ffi::OsStrExt;
            let mut buf = SOUND_BUF.lock().unwrap_or_else(|p| p.into_inner());
            *buf = path.as_os_str().encode_wide().chain([0]).collect();
            unsafe { PlaySoundW(PCWSTR(buf.as_ptr()), None, SND_FILENAME | SND_ASYNC | SND_NODEFAULT) }.as_bool()
        }
    };
    if !played {
        let _ = unsafe { MessageBeep(MB_ICONASTERISK) };
    }
}

/// 追加音のファイルを鳴らす絶対パス。相対のパス（`a.wav`・`sub\a.wav`）は exe のフォルダ（`storage::base_dir`）
/// からのパスにする（PlaySound は相対の名前を、作業フォルダのほか Windows のフォルダ・PATH・ネットワークに割り当てた
/// フォルダからも探すので、絶対パスにしてから鳴らす）。ルートからの相対（`\a.wav`）・ドライブの中の相対（`C:a.wav`）は、
/// 作業フォルダから絶対パスにする。空・絶対パスにできないときは None。
fn sound_file_path(sound_file: &str) -> Option<std::path::PathBuf> {
    use std::path::{Component, Path};
    if sound_file.is_empty() {
        return None;
    }
    let path = Path::new(sound_file);
    let path = if path.has_root() || matches!(path.components().next(), Some(Component::Prefix(_))) {
        path.to_path_buf()
    } else {
        crate::storage::base_dir().join(path)
    };
    // 文字列の上での組み立てで、ファイルには触れない（GetFullPathNameW）
    std::path::absolute(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 実際に音が鳴ったかまでは検証できないが、空文字列（システムビープへのフォールバック）、
    /// 存在しないファイル（`PlaySoundW` 失敗→ビープ）のどちらでもパニックせず戻る。
    #[test]
    fn play_add_sound_does_not_panic_for_empty_or_missing_file() {
        play_add_sound("");
        play_add_sound(r"C:\clclr-test-nonexistent-sound-file.wav");
    }

    /// 追加音の相対のパスは exe のフォルダからの絶対パスにする（PlaySound に作業フォルダ・PATH・ネットワークに割り当てた
    /// フォルダを探させない）。絶対パスはそのまま、ルートからの相対も絶対パスにする。空は None。
    #[test]
    fn sound_file_path_resolves_relative_names_to_exe_folder() {
        let base = crate::storage::base_dir();
        for relative in ["a.wav", r"sub\a.wav"] {
            assert_eq!(sound_file_path(relative), Some(std::path::absolute(base.join(relative)).unwrap()), "{relative}");
        }
        let absolute = std::env::current_exe().unwrap().with_file_name("a.wav");
        assert_eq!(sound_file_path(absolute.to_str().unwrap()), Some(absolute));
        let root_relative = sound_file_path(r"\a.wav").unwrap();
        assert!(root_relative.is_absolute() && root_relative.ends_with("a.wav"), "{root_relative:?}");
        assert_eq!(sound_file_path(""), None);
    }
}
