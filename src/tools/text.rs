//! テキスト変換ツール（C版 CLCL の tool_text プラグインの統合先）。
//!
//! 各変換は純関数で、ビューアの「テキスト変換」サブメニューから選択アイテムの
//! テキストに適用され、結果はクリップボードへ送出される（非破壊。履歴エントリは
//! 書き換えない。変換結果は通常のコピーと同様に新しい履歴として入る）。
//!
//! 新しい変換の追加手順: 変換関数を書き、`TextTransform` にバリアントを足し、
//! `ALL`・`label`・`apply` に1行ずつ追加する。
//!
//! C版との差分: 実行のたびに設定ダイアログを出す方式はやめ、設定画面の値
//! （`[tools.text]`）を使う。禁則文字セットはC版既定相当の固定値。

use crate::config::TextToolsConfig;

/// メニューに出すテキスト変換の種類。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextTransform {
    ToLower,
    ToUpper,
    Quote,
    Unquote,
    WordBreak,
    PutText,
    DeleteCrlf,
}

impl TextTransform {
    /// メニュー表示順の全変換。
    pub const ALL: [TextTransform; 7] = [
        Self::ToLower,
        Self::ToUpper,
        Self::Quote,
        Self::Unquote,
        Self::WordBreak,
        Self::PutText,
        Self::DeleteCrlf,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::ToLower => "小文字に変換",
            Self::ToUpper => "大文字に変換",
            Self::Quote => "引用",
            Self::Unquote => "引用解除",
            Self::WordBreak => "テキスト整形（折り返し）",
            Self::PutText => "テキストの挟み込み",
            Self::DeleteCrlf => "改行の除去",
        }
    }

    pub fn apply(self, text: &str, cfg: &TextToolsConfig) -> String {
        match self {
            Self::ToLower => text.to_lowercase(),
            Self::ToUpper => text.to_uppercase(),
            Self::Quote => quote(text, &cfg.quote_char),
            Self::Unquote => unquote(text, &cfg.quote_char),
            Self::WordBreak => word_break(text, cfg.word_break_width as usize),
            Self::PutText => format!("{}{}{}", cfg.put_text_open, text, cfg.put_text_close),
            Self::DeleteCrlf => delete_crlf(text, cfg.delete_crlf_trim_leading),
        }
    }
}

/// 各行頭に引用符を付加する。
fn quote(text: &str, quote_char: &str) -> String {
    text.split_inclusive('\n')
        .map(|line| format!("{quote_char}{line}"))
        .collect()
}

/// 各行頭の引用符を除去する（付いていない行はそのまま）。
fn unquote(text: &str, quote_char: &str) -> String {
    if quote_char.is_empty() {
        return text.to_string();
    }
    text.split_inclusive('\n')
        .map(|line| line.strip_prefix(quote_char).unwrap_or(line))
        .collect()
}

/// 改行（CR/LF）を除去して1行にする。`trim_leading`なら2行目以降の行頭空白
/// （折り返しインデント）も除去する。先頭行のインデントは意図的なものとして残す。
fn delete_crlf(text: &str, trim_leading: bool) -> String {
    // CRLF・LF・単独の CR のどれも改行として扱う
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(text.len());
    for (i, segment) in text.split_inclusive('\n').enumerate() {
        let line = segment.trim_end_matches('\n');
        let line = if trim_leading && i > 0 {
            line.trim_start_matches([' ', '\t', '　'])
        } else {
            line
        };
        out.push_str(line);
    }
    out
}

/// 行末禁則: 行末に置けず次行へ追い出す文字（開き括弧類。C版 s_oida/m_oida 相当）
const KINSOKU_PUSH: &str = "（〔［｛〈《「『【\\$([{￥";
/// 行頭禁則: 行頭に置けず前行末にぶら下げる文字（句読点・閉じ括弧・小書きかな等。
/// C版 s_bura/m_bura 相当）
const KINSOKU_HANG: &str = "、。，．・？！゛゜ヽヾゝゞ々ー）〕］｝〉》」』】,.?!%:;)]}ぁぃぅぇぉっゃゅょゎァィゥェォッャュョヮヵヶ";

/// 表示幅の概算（半角=1、全角=2。ASCII と半角カナ以外は全角とみなす）。
fn char_width(c: char) -> usize {
    if c.is_ascii() || (0xFF61..=0xFF9F).contains(&(c as u32)) {
        1
    } else {
        2
    }
}

/// 指定幅（半角換算）でテキストを折り返す。禁則処理つき:
/// 行頭禁則文字は前行末にぶら下げ、行末禁則文字は次行へ追い出す。
/// 既存の改行は段落区切りとして維持する。
fn word_break(text: &str, width: usize) -> String {
    if width == 0 {
        return text.to_string();
    }
    let mut out = String::new();
    for segment in text.split_inclusive('\n') {
        let content = segment.trim_end_matches(['\r', '\n']);
        let ending = &segment[content.len()..];

        let mut col = 0usize; // 現在行の幅
        let mut line_chars = 0usize; // 現在行の文字数（追い出しで行が空になるのを防ぐ）
        for c in content.chars() {
            let w = char_width(c);
            if col > 0 && col + w > width {
                if KINSOKU_HANG.contains(c) {
                    // ぶら下げ: 行頭禁則文字は幅超過でも現在行に置く
                    out.push(c);
                    col += w;
                    line_chars += 1;
                    continue;
                }
                // 追い出し: 行末の禁則文字を次行へ持ち越す（行が空にならない範囲で）
                let mut carried = String::new();
                while line_chars > 1 {
                    let Some(last) = out.chars().last() else {
                        break;
                    };
                    if !KINSOKU_PUSH.contains(last) {
                        break;
                    }
                    out.pop();
                    carried.insert(0, last);
                    line_chars -= 1;
                }
                out.push('\n');
                col = carried.chars().map(char_width).sum();
                line_chars = carried.chars().count();
                out.push_str(&carried);
            }
            out.push(c);
            col += w;
            line_chars += 1;
        }
        out.push_str(ending);
    }
    out
}

/// 日時変換（C版 CLCL の tool_text convert_date）: `%d`→日付、`%t`→時刻に置換する。
/// ピン留めアイテムの送出時に自動適用される（定型文テンプレート用途。C版の
/// 「データをクリップボードに送る時」相当）。書式が空ならロケール既定の表記。
pub fn convert_date(text: &str, date_format: &str, time_format: &str) -> String {
    if !text.contains("%d") && !text.contains("%t") {
        return text.to_string();
    }
    let date = format_now(date_format, FormatKind::Date);
    let time = format_now(time_format, FormatKind::Time);
    text.replace("%d", &date).replace("%t", &time)
}

/// ピン留めアイテムの送出前処理: 設定が有効ならテキスト中の %d/%t を日時に
/// 変換した複製を返す（C版 CLCL の tool_text の日時変換。定型文テンプレート用途）。
/// メニュー送出（hotkey.rs）とビューア送出（native/actions.rs）で共用する。
pub fn convert_entry_date(mut entry: crate::data::Entry, cfg: &TextToolsConfig) -> crate::data::Entry {
    if !cfg.convert_date_on_send {
        return entry;
    }
    for fmt in &mut entry.formats {
        if fmt.format_name == "CF_UNICODETEXT" {
            let converted = convert_date(
                &crate::data::utf16_text(&fmt.data),
                &cfg.date_format,
                &cfg.time_format,
            );
            fmt.data = crate::data::utf16_bytes(&converted);
        }
    }
    entry
}

enum FormatKind {
    Date,
    Time,
}

/// 現在日時をWin32の書式（GetDateFormatW/GetTimeFormatW、例: "yyyy/MM/dd"）で
/// 文字列化する。書式が空ならロケール既定。
fn format_now(format: &str, kind: FormatKind) -> String {
    use windows::core::PCWSTR;
    use windows::Win32::Globalization::{GetDateFormatW, GetTimeFormatW};

    // LOCALE_USER_DEFAULT（winnls.h）。windows crateは定数を別featureに置くため直接定義
    const LOCALE_USER_DEFAULT: u32 = 0x0400;

    let format_wide: Vec<u16>;
    let format_ptr = if format.is_empty() {
        PCWSTR::null()
    } else {
        format_wide = format.encode_utf16().chain([0]).collect();
        PCWSTR(format_wide.as_ptr())
    };
    let mut buf = [0u16; 128];
    let written = unsafe {
        match kind {
            FormatKind::Date => {
                GetDateFormatW(LOCALE_USER_DEFAULT, 0, None, format_ptr, Some(&mut buf))
            }
            FormatKind::Time => {
                GetTimeFormatW(LOCALE_USER_DEFAULT, 0, None, format_ptr, Some(&mut buf))
            }
        }
    };
    if written <= 0 {
        // 書式不正等。空文字ではなく書式そのものを残してユーザーが気づけるようにする
        return format.to_string();
    }
    String::from_utf16_lossy(&buf[..written as usize - 1]) // -1はNUL終端分
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> TextToolsConfig {
        TextToolsConfig::default()
    }

    #[test]
    fn quote_prefixes_each_line() {
        assert_eq!(quote("a\r\nb\nc", ">"), ">a\r\n>b\n>c");
        assert_eq!(quote("", ">"), "");
    }

    #[test]
    fn unquote_strips_only_present_prefix() {
        assert_eq!(unquote(">a\r\nb\n>c", ">"), "a\r\nb\nc");
    }

    #[test]
    fn quote_unquote_roundtrip() {
        let original = "一行目\r\n二行目";
        assert_eq!(unquote(&quote(original, "> "), "> "), original);
    }

    #[test]
    fn delete_crlf_joins_and_trims_continuation_indent() {
        // 先頭行のインデントは残り、継続行のインデントだけ消える
        assert_eq!(delete_crlf("  a\r\n  b\n\tc", true), "  abc");
        assert_eq!(delete_crlf("a\r\n b", false), "a b");
        // 単独の CR も改行として除く
        assert_eq!(delete_crlf("a\rb\r\n\r c", true), "abc");
    }

    #[test]
    fn put_text_wraps() {
        let out = TextTransform::PutText.apply("中身", &cfg());
        assert_eq!(out, "<TAG>中身</TAG>");
    }

    #[test]
    fn word_break_wraps_by_display_width() {
        // 幅4: ASCIIは4文字、全角は2文字で折り返し
        assert_eq!(word_break("abcdef", 4), "abcd\nef");
        assert_eq!(word_break("あいう", 4), "あい\nう");
    }

    #[test]
    fn word_break_hangs_forbidden_line_head_chars() {
        // 句点は行頭に来ない（前行末にぶら下げ）
        assert_eq!(word_break("あい。う", 4), "あい。\nう");
    }

    #[test]
    fn word_break_pushes_forbidden_line_end_chars() {
        // 行末に来る開き括弧は次行へ追い出される（「 が あ の行末に残らない）
        assert_eq!(word_break("あ「いう", 4), "あ\n「い\nう");
        // 行頭に自然に収まる開き括弧はそのまま
        assert_eq!(word_break("あい「う」", 4), "あい\n「う」");
    }

    #[test]
    fn word_break_preserves_existing_newlines() {
        assert_eq!(word_break("ab\r\ncd", 10), "ab\r\ncd");
    }

    #[test]
    fn convert_date_replaces_placeholders() {
        let out = convert_date("日付:%d 時刻:%t", "", "");
        assert!(!out.contains("%d") && !out.contains("%t"));
        assert!(out.starts_with("日付:") && out.len() > "日付: 時刻:".len());
        // プレースホルダなしはそのまま（日時取得もしない）
        assert_eq!(convert_date("そのまま", "", ""), "そのまま");
    }

    #[test]
    fn convert_date_uses_explicit_format() {
        let out = convert_date("%d", "yyyy-MM-dd", "");
        // yyyy-MM-dd → 4桁-2桁-2桁
        assert_eq!(out.len(), 10);
        assert_eq!(out.as_bytes()[4], b'-');
        assert_eq!(out.as_bytes()[7], b'-');
    }

    fn unicodetext_format(text: &str) -> crate::data::Format {
        crate::data::Format {
            format_name: "CF_UNICODETEXT".to_string(),
            format_id: 13,
            data: crate::data::utf16_bytes(text),
        }
    }

    #[test]
    fn convert_entry_date_is_noop_when_disabled() {
        let mut c = cfg();
        c.convert_date_on_send = false;
        let entry = crate::data::Entry::new(vec![unicodetext_format("%d固定")]);
        let converted = convert_entry_date(entry, &c);
        assert_eq!(
            crate::data::utf16_text(&converted.formats[0].data),
            "%d固定"
        );
    }

    #[test]
    fn convert_entry_date_only_touches_unicodetext_format() {
        let mut c = cfg();
        c.convert_date_on_send = true;
        let entry = crate::data::Entry::new(vec![crate::data::Format {
            format_name: "CF_HDROP".to_string(),
            format_id: 15,
            data: b"C:\\a.txt".to_vec(),
        }]);
        let converted = convert_entry_date(entry, &c);
        // CF_UNICODETEXTではないため変換対象にならず、バイト列はそのまま
        assert_eq!(converted.formats[0].data, b"C:\\a.txt");
    }

    #[test]
    fn convert_entry_date_leaves_text_without_placeholders_unchanged() {
        let mut c = cfg();
        c.convert_date_on_send = true;
        let entry = crate::data::Entry::new(vec![unicodetext_format("プレースホルダなし")]);
        let converted = convert_entry_date(entry, &c);
        assert_eq!(
            crate::data::utf16_text(&converted.formats[0].data),
            "プレースホルダなし"
        );
    }
}
