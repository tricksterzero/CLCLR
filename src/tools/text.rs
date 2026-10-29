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
    ToHalfwidth,
}

impl TextTransform {
    /// メニュー表示順の全変換。
    pub const ALL: [TextTransform; 8] = [
        Self::ToLower,
        Self::ToUpper,
        Self::Quote,
        Self::Unquote,
        Self::WordBreak,
        Self::PutText,
        Self::DeleteCrlf,
        Self::ToHalfwidth,
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
            Self::ToHalfwidth => "全角英数記号を半角に変換",
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
            Self::ToHalfwidth => to_halfwidth(text),
        }
    }
}

/// 全角の英数字・記号（U+FF01〜U+FF5D）と全角スペース（U+3000）を半角（ASCII）に、全角の円記号（U+FFE5）を
/// 半角の円記号（U+00A5）にする。対応は Unicode の文字データベースの互換分解（`<wide>`）による。カタカナと、
/// ほかの全角の形の記号（￠・￡ など）は変えない。全角チルダ（U+FF5E）も変えない（Windows では日本語の「～」
/// （Shift_JIS の 0x8160）がこの文字になり、「1～3」などの波ダッシュとして使われるため）。
fn to_halfwidth(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{FF01}'..='\u{FF5D}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
            '\u{3000}' => ' ',
            '\u{FFE5}' => '\u{A5}',
            _ => c,
        })
        .collect()
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

/// 日時変換の `%d`・`%t` の置き換え先（送る時点の日付・時刻の文字列）。
pub struct DateText {
    pub date: String,
    pub time: String,
}

impl DateText {
    /// 今の日付・時刻を、設定の書式（GetDateFormatW/GetTimeFormatW形式）で文字列にする。書式が空なら
    /// ロケール既定の表記。書式が正しくなければ、書式そのものを入れる（`format_now`）。
    pub fn now(cfg: &TextToolsConfig) -> Self {
        Self { date: format_now(&cfg.date_format, FormatKind::Date), time: format_now(&cfg.time_format, FormatKind::Time) }
    }
}

/// 日時変換（C版 CLCL の tool_text convert_date）: `%d`→日付、`%t`→時刻に置換する（`now` の文字列で。
/// 今の日時は読まない）。
fn convert_date(text: &str, now: &DateText) -> String {
    text.replace("%d", &now.date).replace("%t", &now.time)
}

/// `%d`・`%t` を含むか。
fn has_date_placeholder(text: &str) -> bool {
    text.contains("%d") || text.contains("%t")
}

/// ピン留めアイテムの送出前処理: 設定が有効ならテキスト中の %d/%t を日時に
/// 変換した複製を返す（C版 CLCL の tool_text の日時変換。定型文テンプレート用途。C版の
/// 「データをクリップボードに送る時」相当）。メニュー送出（hotkey.rs）とビューア送出（native/actions.rs）で共用する。
/// 置き換え先の日時は `now` で得る（呼び出し側は `|| DateText::now(cfg)` を渡す）。`now` を呼ぶのは、設定が
/// 有効で、CF_UNICODETEXT に %d/%t があるときだけ（1回）。
///
/// 置き換えたときは CF_UNICODETEXT だけを残す。ほかの形式（形式フィルタで足した HTML Format・
/// Rich Text Format など）は置き換えられないため、残すと貼り付け先がそちらを選び、%d/%t のままの
/// 内容が貼られる。置き換えるものが無ければ、全形式をそのまま返す。
pub fn convert_entry_date(
    mut entry: crate::data::Entry,
    cfg: &TextToolsConfig,
    now: impl FnOnce() -> DateText,
) -> crate::data::Entry {
    let is_text = |fmt: &crate::data::Format| fmt.format_name == "CF_UNICODETEXT";
    let placeholders =
        entry.formats.iter().any(|fmt| is_text(fmt) && has_date_placeholder(&crate::data::utf16_text(&fmt.data)));
    if !cfg.convert_date_on_send || !placeholders {
        return entry;
    }
    let now = now();
    let mut replaced = false;
    for fmt in entry.formats.iter_mut().filter(|fmt| is_text(fmt)) {
        let text = crate::data::utf16_text(&fmt.data);
        let converted = convert_date(&text, &now);
        if converted != text {
            fmt.data = crate::data::utf16_bytes(&converted);
            replaced = true;
        }
    }
    if replaced {
        entry.formats.retain(is_text);
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

    /// テストで渡す決まった日時。
    fn fixed() -> DateText {
        DateText { date: "2026/10/30".to_string(), time: "12:34".to_string() }
    }

    /// 今の日時を読んではいけない場面で渡す（呼ばれたらテストを失敗させる）。
    fn never() -> DateText {
        panic!("今の日時を読んだ")
    }

    /// `%d` は日付、`%t` は時刻の文字列に、どれも（何回出ても）置き換える。無ければそのまま。
    #[test]
    fn convert_date_replaces_every_placeholder_with_given_text() {
        assert_eq!(convert_date("日付:%d 時刻:%t 再び:%d", &fixed()), "日付:2026/10/30 時刻:12:34 再び:2026/10/30");
        assert_eq!(convert_date("そのまま", &fixed()), "そのまま");
        assert_eq!(convert_date("", &fixed()), "");
    }

    /// 今の日時を設定の書式（GetDateFormatW・GetTimeFormatW の形式）で文字列にする。書式が空ならロケール既定の表記。
    #[test]
    fn date_text_now_uses_format_or_locale_default() {
        let mut c = cfg();
        c.date_format = "yyyy-MM-dd".to_string();
        c.time_format = "HH:mm".to_string();
        let now = DateText::now(&c);
        // yyyy-MM-dd → 4桁-2桁-2桁、HH:mm → 2桁:2桁
        assert_eq!((now.date.len(), now.date.as_bytes()[4], now.date.as_bytes()[7]), (10, b'-', b'-'), "{}", now.date);
        assert_eq!((now.time.len(), now.time.as_bytes()[2]), (5, b':'), "{}", now.time);

        let default = DateText::now(&cfg());
        assert!(!default.date.is_empty() && !default.time.is_empty(), "{} {}", default.date, default.time);
    }

    /// 全角の英数字・記号の93文字はどれも ASCII の `!`〜`}` に、全角スペースは半角スペースに、`￥` は U+00A5 になる。
    #[test]
    fn to_halfwidth_maps_fullwidth_ascii_space_and_yen() {
        let fullwidth: String = (0xFF01..=0xFF5D).map(|cp| char::from_u32(cp).unwrap()).collect();
        let ascii: String = (0x21..=0x7D).map(|cp| char::from_u32(cp).unwrap()).collect();
        assert_eq!(to_halfwidth(&fullwidth), ascii);
        assert_eq!(to_halfwidth("\u{3000}"), " ");
        assert_eq!(to_halfwidth("\u{FFE5}"), "\u{A5}");
        assert_eq!(TextTransform::ToHalfwidth.apply("ＡＢＣ　１２３＠ｅｘａｍｐｌｅ．ｊｐ", &cfg()), "ABC 123@example.jp");
    }

    /// カタカナ・全角チルダ（日本語の「～」）・ほかの全角の形の記号・全角の形でない記号・半角の文字は変えない。
    #[test]
    fn to_halfwidth_leaves_other_characters() {
        let unchanged = "カタカナｶﾀｶﾅ漢字1\u{FF5E}3〜ー“”￠￡abc 1\u{A5}\\~";
        assert_eq!(to_halfwidth(unchanged), unchanged);
    }

    fn unicodetext_format(text: &str) -> crate::data::Format {
        crate::data::Format {
            format_name: "CF_UNICODETEXT".to_string(),
            format_id: 13,
            data: crate::data::utf16_bytes(text),
        }
    }

    /// 無効なら何も変えず、今の日時も読まない。
    #[test]
    fn convert_entry_date_is_noop_when_disabled() {
        let mut c = cfg();
        c.convert_date_on_send = false;
        let entry = crate::data::Entry::new(vec![unicodetext_format("%d固定")]);
        let converted = convert_entry_date(entry, &c, never);
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
            data: b"C:\\%d.txt".to_vec(),
        }]);
        let converted = convert_entry_date(entry, &c, never);
        // CF_UNICODETEXTではないため変換対象にならず（今の日時も読まず）、バイト列はそのまま
        assert_eq!(converted.formats[0].data, b"C:\\%d.txt");
    }

    fn html_format(html: &str) -> crate::data::Format {
        crate::data::Format { format_name: "HTML Format".to_string(), format_id: 0xC0FF, data: html.as_bytes().to_vec() }
    }

    fn format_names(entry: &crate::data::Entry) -> Vec<&str> {
        entry.formats.iter().map(|f| f.format_name.as_str()).collect()
    }

    #[test]
    fn convert_entry_date_keeps_only_text_when_replaced() {
        let mut c = cfg();
        c.convert_date_on_send = true;
        let entry = crate::data::Entry::new(vec![html_format("<b>%d</b>"), unicodetext_format("日付:%d 時刻:%t")]);
        let converted = convert_entry_date(entry, &c, fixed);
        // 置き換えられない HTML Format は外し、渡した日時で置き換えたテキストだけを送る
        assert_eq!(format_names(&converted), ["CF_UNICODETEXT"]);
        assert_eq!(crate::data::utf16_text(&converted.formats[0].data), "日付:2026/10/30 時刻:12:34");
    }

    /// 置き換えるものが無ければ全形式をそのまま返し、今の日時も読まない。
    #[test]
    fn convert_entry_date_keeps_all_formats_without_placeholders() {
        let mut c = cfg();
        c.convert_date_on_send = true;
        let entry = crate::data::Entry::new(vec![html_format("<b>%d</b>"), unicodetext_format("固定")]);
        let converted = convert_entry_date(entry, &c, never);
        assert_eq!(format_names(&converted), ["HTML Format", "CF_UNICODETEXT"]);
        assert_eq!(converted.formats[0].data, "<b>%d</b>".as_bytes());
    }

    #[test]
    fn convert_entry_date_keeps_all_formats_when_disabled() {
        let mut c = cfg();
        c.convert_date_on_send = false;
        let entry = crate::data::Entry::new(vec![html_format("<b>%d</b>"), unicodetext_format("%d")]);
        let converted = convert_entry_date(entry, &c, never);
        assert_eq!(format_names(&converted), ["HTML Format", "CF_UNICODETEXT"]);
    }

    #[test]
    fn convert_entry_date_leaves_text_without_placeholders_unchanged() {
        let mut c = cfg();
        c.convert_date_on_send = true;
        let entry = crate::data::Entry::new(vec![unicodetext_format("プレースホルダなし")]);
        let converted = convert_entry_date(entry, &c, never);
        assert_eq!(
            crate::data::utf16_text(&converted.formats[0].data),
            "プレースホルダなし"
        );
    }
}
