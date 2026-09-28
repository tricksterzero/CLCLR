//! クリップボードデータの表現（C版 DATA_INFO 相当）。
//!
//! C版は型フラグ(type)で分岐する単一構造体だったが、ここでは
//! テキスト/画像/ファイル等の種別をenumで分けず`data: Vec<u8>`に統一している。
//! 種別ごとの解釈（デコード・表示用テキスト抽出等）はUI層/Entryのメソッドに委ね、
//! ペースト時に型ごとの分岐が二重になるのを避けている。

use std::time::SystemTime;
use uuid::Uuid;

/// クリップボードの1形式分のデータ。
/// バイト列は取得時に正規化済み（GDIハンドル等は変換済み）。
#[derive(Clone)]
pub struct Format {
    /// クリップボード形式名（例: "CF_UNICODETEXT", "CF_DIB"）。標準形式は固定名、
    /// カスタム形式は`RegisterClipboardFormat`で登録された名前がそのまま入る。
    pub format_name: String,
    /// Win32のクリップボード形式ID（`EnumClipboardFormats`が返す値）。
    pub format_id: u32,
    pub data: Vec<u8>,
}

/// 1回のコピー操作で取得した全形式のセット。
///
/// `window_name`（コピー元ウィンドウ名）や`text_cache`はあえて持たせていない。
/// 前者は表示用途のみで必須ではなく、後者はUI層の責務（デコードは表示時に行う）。
pub struct Entry {
    pub id: Uuid,
    pub title: Option<String>,
    pub modified: SystemTime,
    pub formats: Vec<Format>,
}

impl Entry {
    pub fn new(formats: Vec<Format>) -> Self {
        Self {
            id: Uuid::new_v4(),
            title: None,
            modified: SystemTime::now(),
            formats,
        }
    }

    /// 履歴の重複チェック用コンテンツハッシュ。形式名とバイト列だけを見て、
    /// id・タイトル・時刻は含めない。形式の列挙順はコピー元アプリに依存して
    /// 揺れるため、形式名でソートして順序非依存にする。
    ///
    /// FNV-1a 64bit の自前実装を使う。stdの`DefaultHasher`はRustバージョン間の
    /// 安定性が保証されず、永続化（EntryMeta.hash）に使えないため。
    /// 衝突時の影響は「重複でない履歴が1件重複扱いで落ちる」に留まる。
    pub fn content_hash(&self) -> u64 {
        let mut sorted: Vec<&Format> = self.formats.iter().collect();
        sorted.sort_by(|a, b| a.format_name.cmp(&b.format_name));

        const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut hash = FNV_OFFSET;
        let mut feed = |bytes: &[u8]| {
            for &b in bytes {
                hash ^= u64::from(b);
                hash = hash.wrapping_mul(FNV_PRIME);
            }
        };
        for fmt in sorted {
            feed(fmt.format_name.as_bytes());
            // 名前とデータ、および形式間の境界を曖昧にしないための区切り
            feed(&[0]);
            feed(&(fmt.data.len() as u64).to_le_bytes());
            feed(&fmt.data);
        }
        hash
    }
}

/// エントリのデータ種別（種別アイコンの選択に使う。ビューアの一覧とホットキーのメニューで共用）。
/// 優先順は画像＞ファイル＞テキスト＞その他。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryKind {
    Image,
    File,
    Text,
    Other,
}

impl EntryKind {
    pub fn from_formats(has_dib: bool, has_file: bool, has_text: bool) -> Self {
        if has_dib {
            Self::Image
        } else if has_file {
            Self::File
        } else if has_text {
            Self::Text
        } else {
            Self::Other
        }
    }

    /// 形式名の一覧から決める。
    pub fn from_format_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let (mut dib, mut file, mut text) = (false, false, false);
        for name in names {
            match name {
                "CF_DIB" => dib = true,
                "CF_HDROP" => file = true,
                "CF_UNICODETEXT" => text = true,
                _ => {}
            }
        }
        Self::from_formats(dib, file, text)
    }
}

/// 文字列を CF_UNICODETEXT のバイト列（UTF-16LE・NUL終端）にする。
pub fn utf16_bytes(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// CF_UNICODETEXT（UTF-16LE・NUL終端）の文字を1つずつ返す（`utf16_text` と同じ文字。全文の文字列を
/// 作らない。メモリだけに持つ全文の検索で写しを作らないため）。
pub fn utf16_chars(data: &[u8]) -> impl Iterator<Item = char> + '_ {
    let units = data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0);
    char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
}

/// CF_UNICODETEXT（UTF-16LE・NUL終端）を文字列化する。
pub fn utf16_text(data: &[u8]) -> String {
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(name: &str, data: &[u8]) -> Format {
        Format {
            format_name: name.to_string(),
            format_id: 0,
            data: data.to_vec(),
        }
    }

    /// 1文字ずつ返す関数は、文字列化と同じ文字を返す（NUL で止まる、対になっていないサロゲートは
    /// 置き換え文字）。
    #[test]
    fn utf16_chars_matches_utf16_text() {
        let mut data = utf16_bytes("Aあ😀");
        data.truncate(data.len() - 2);
        data.extend(0xD800u16.to_le_bytes());
        data.extend(u16::from(b'z').to_le_bytes());
        data.extend([0, 0]);
        data.extend(utf16_bytes("NUL の後"));
        assert_eq!(utf16_chars(&data).collect::<String>(), utf16_text(&data));
        assert_eq!(utf16_text(&data), "Aあ😀\u{FFFD}z");
    }

    #[test]
    fn utf16_bytes_encodes_utf16le_with_nul_terminator() {
        // 'A'=0x0041, 'B'=0x0042、末尾はNUL終端（いずれもリトルエンディアン）
        assert_eq!(
            utf16_bytes("AB"),
            vec![0x41, 0x00, 0x42, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn utf16_bytes_and_utf16_text_roundtrip() {
        let original = "検索abc";
        assert_eq!(utf16_text(&utf16_bytes(original)), original);
    }

    #[test]
    fn utf16_text_decodes_and_stops_at_nul() {
        let mut data: Vec<u8> = "検索abc".encode_utf16().flat_map(u16::to_le_bytes).collect();
        data.extend_from_slice(&[0, 0]); // NUL終端
        data.extend_from_slice(&[b'x', 0]); // 終端後のゴミは読まない
        assert_eq!(utf16_text(&data), "検索abc");
    }

    #[test]
    fn entry_kind_prefers_image_over_file_over_text_over_other() {
        assert_eq!(EntryKind::from_formats(true, true, true), EntryKind::Image);
        assert_eq!(EntryKind::from_formats(false, true, true), EntryKind::File);
        assert_eq!(EntryKind::from_formats(false, false, true), EntryKind::Text);
        assert_eq!(EntryKind::from_formats(false, false, false), EntryKind::Other);
        assert_eq!(EntryKind::from_format_names(["CF_UNICODETEXT", "CF_HDROP"]), EntryKind::File);
        assert_eq!(EntryKind::from_format_names(["CF_LOCALE"]), EntryKind::Other);
    }

    #[test]
    fn content_hash_is_order_independent() {
        let a = Entry::new(vec![fmt("CF_UNICODETEXT", b"x"), fmt("CF_HDROP", b"y")]);
        let b = Entry::new(vec![fmt("CF_HDROP", b"y"), fmt("CF_UNICODETEXT", b"x")]);
        assert_eq!(a.content_hash(), b.content_hash());
    }

    #[test]
    fn content_hash_ignores_id_and_time_but_not_content() {
        let a = Entry::new(vec![fmt("CF_UNICODETEXT", b"x")]);
        let b = Entry::new(vec![fmt("CF_UNICODETEXT", b"x")]);
        let c = Entry::new(vec![fmt("CF_UNICODETEXT", b"y")]);
        let d = Entry::new(vec![fmt("CF_TEXT", b"x")]);
        assert_eq!(a.content_hash(), b.content_hash());
        assert_ne!(a.content_hash(), c.content_hash());
        assert_ne!(a.content_hash(), d.content_hash());
    }
}
