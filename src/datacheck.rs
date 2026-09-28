//! データのチェック（blobs と history.toml・pinned.toml の食い違い）の結果の型と、ファイル名の判定。
//!
//! 走査と削除は `ops::Core::check_data`・`Core::clean_data`（操作用のロックの中）が行う。見つけるのは
//! (a) どこからも参照されない blob・サムネイル、(b) 参照している blob が無い項目、(c) 書き込みの途中で残った
//! 一時ファイル。CLCLR の blob の名前の形に合わないファイルと config.toml.tmp は「対象外」として消さない。

use uuid::Uuid;

/// ファイルの名前と大きさ。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub size: u64,
}

/// 一時ファイルの置き場所。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TempPlace {
    /// `blobs` の中（blob の名前＋`.tmp`）
    Blobs,
    /// データフォルダの直下（`history.toml.tmp`・`pinned.toml.tmp`）
    DataDir,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TempFile {
    pub place: TempPlace,
    pub name: String,
    pub size: u64,
}

/// 参照している blob（サムネイルは除く）のどれかが無い項目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingItem {
    pub id: Uuid,
    /// ピン留めのアイテムか（違えば履歴の項目）
    pub pinned: bool,
    /// 表示名（名前・テキストの1行目。無ければ形式名）
    pub label: String,
}

/// チェックの結果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DataReport {
    /// (a) どこからも参照されない blob・サムネイル
    pub orphans: Vec<FileEntry>,
    /// (b) データが欠けた項目
    pub missing: Vec<MissingItem>,
    /// (c) 一時ファイル
    pub temps: Vec<TempFile>,
    /// 対象外（名前が CLCLR の blob の形に合わない `blobs` の中のファイル、config.toml.tmp）。消さない
    pub ignored: Vec<String>,
}

impl DataReport {
    /// 消すものが無いか。
    pub fn is_clean(&self) -> bool {
        self.orphans.is_empty() && self.missing.is_empty() && self.temps.is_empty()
    }
}

/// 削除の結果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CleanResult {
    /// 消したファイル（(a)・(c)、(b) の項目の blob は含めない）の数と合計の大きさ
    pub files_removed: usize,
    pub bytes_removed: u64,
    /// 履歴・ピン留めから消した項目の数
    pub items_removed: usize,
    /// 消せなかったもの（名前と理由）
    pub failures: Vec<String>,
    /// 終了の要求で途中でやめた
    pub interrupted: bool,
    /// 項目は消したが、履歴のファイルを書き直せなかった（あとで書き直す）
    pub index_error: Option<String>,
}

/// CLCLR の blob の名前の形か（`<UUID>_<数>.webp`・`.bin`・`.thumb.webp`。UUID は `-` 区切りの 36 文字）。
/// 大文字・小文字は区別しない。
pub(crate) fn is_blob_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let Some(stem) = lower
        .strip_suffix(".thumb.webp")
        .or_else(|| lower.strip_suffix(".webp"))
        .or_else(|| lower.strip_suffix(".bin"))
    else {
        return false;
    };
    let Some((id, index)) = stem.rsplit_once('_') else {
        return false;
    };
    id.len() == 36 && Uuid::parse_str(id).is_ok() && !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit())
}

/// blob の一時ファイルの名前の形か（blob の名前＋`.tmp`。`storage::write_atomic` が作る）。
pub(crate) fn is_blob_temp_name(name: &str) -> bool {
    name.to_ascii_lowercase().strip_suffix(".tmp").is_some_and(is_blob_name)
}

/// 比べるための名前（ASCII の小文字）。blob の名前は ASCII の英数字と `-`・`_`・`.` だけ。
pub(crate) fn name_key(name: &str) -> String {
    name.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_names_are_recognized_case_insensitively() {
        let id = Uuid::new_v4();
        for name in [format!("{id}_0.webp"), format!("{id}_12.bin"), format!("{id}_3.thumb.webp")] {
            assert!(is_blob_name(&name), "{name}");
            assert!(is_blob_name(&name.to_ascii_uppercase()), "{name}");
            assert!(!is_blob_temp_name(&name), "{name}");
            assert!(is_blob_temp_name(&format!("{name}.tmp")), "{name}");
            assert!(is_blob_temp_name(&format!("{name}.TMP")), "{name}");
        }
        let simple = id.simple().to_string();
        for name in [
            "desktop.ini".to_string(),
            "notes.tmp".to_string(),
            format!("{id}_0.png"),
            format!("{id}_.bin"),
            format!("{id}_x.bin"),
            format!("{id}.bin"),
            format!("{simple}_0.bin"),
            format!("{id}_0.bin.bak"),
            "あ_0.bin".to_string(),
        ] {
            assert!(!is_blob_name(&name), "{name}");
            assert!(!is_blob_temp_name(&format!("{name}.tmp")), "{name}");
        }
    }
}
