//! 履歴・ピン留めアイテムのファイルI/O。
//!
//! メタデータ（TOML）とblob（バイナリ本体）を分離して保存する。大きなデータ
//! （画像等）を毎回全件読み込まずに済むよう、Entry/Nodeへの復元は必要になった
//! 時点で該当blobだけを読む（`load_entry_data`）形にしている。
//! CF_DIBのblobはWebPロスレスに変換して保存し（dib.rs）、それ以外は生バイナリ。

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::data::{Entry, Format};
use crate::dib;
use crate::store::{PinnedFolder, PinnedNode};

// --- Error ---

#[derive(Debug)]
pub enum StorageError {
    Io(io::Error),
    /// history.toml / pinned.toml の書式の誤り。`detail` は位置（日本語）と `toml` の説明
    /// （`toml_error_detail`）
    Parse { file: &'static str, detail: String },
    Serialize(toml::ser::Error),
    InvalidTimestamp,
    /// pinned.tomlの`parent_id`チェーンが破綻している場合(自己参照・ルートから
    /// 到達不能な孤立した循環・存在しない親IDを参照する孤立ノード等)。原因を
    /// 個別に区別せず、「入力された全ノードがルートから辿って消費されたか」の
    /// 一点で判定する(詳細は`build_tree`のコメント参照)。
    PinnedTreeCorrupt,
    /// history.toml / pinned.toml のスキーマ版がこのビルドの対応範囲（`SCHEMA_VERSION`）より
    /// 新しい。読み飛ばして次の保存で上書きすると新しい形式のデータを失うため、読まずに
    /// エラーにする（新しいビルドで作ったデータフォルダを古いビルドで開いた場合など）。
    UnsupportedSchema { file: &'static str, found: u32 },
    /// 厳密な読み込み（`load_entry_data_strict`）で、メタデータにある形式の blob が読めない
    /// （欠けている・壊れている）
    MissingFormat { format_name: String },
    /// 保存してある画像（WebP）が、元に戻すときの上限（`MAX_RESTORE_RGBA`）を超える
    ImageTooLarge { width: u32, height: u32 },
}

/// 保存してある画像（WebP）を DIB に戻すとき（送る・起動時の復元・ピン留めの複製など）の大きさの上限（画素を RGBA に
/// 換算したバイト数）。書き換えられたファイルで大きな確保をしないため。1回のコピーの合計の上限が既定（320MiB）なら、
/// 取り込める画像は必ずこの中に収まる（24bit の DIB 320MiB でも RGBA で約 427MiB）。合計の上限を上げて取り込んだ、
/// これを超える画像は戻さない
pub const MAX_RESTORE_RGBA: u64 = 512 * 1024 * 1024;

/// WebP の blob を読んだ結果（`Storage::load_webp`）。
#[derive(Debug, PartialEq, Eq)]
pub enum LoadedWebp {
    /// ファイル全体（CLCLR が書く形で、長さが寸法に見合うことを確かめてある）
    Data(Vec<u8>),
    /// 画素を RGBA に換算した大きさが上限を超えるので、全体を読まなかった
    TooLarge { width: u32, height: u32 },
}

impl LoadedWebp {
    /// 読めたファイル全体（上限を超えたものは None）。
    pub fn into_data(self) -> Option<Vec<u8>> {
        match self {
            Self::Data(data) => Some(data),
            Self::TooLarge { .. } => None,
        }
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "ファイルを読み書きできません（{e}）"),
            Self::Parse { file, detail } => write!(f, "{file} の書式が正しくありません（{detail}）"),
            Self::Serialize(e) => write!(f, "保存するデータを書き出せません（{e}）"),
            Self::InvalidTimestamp => write!(f, "保存されている日時が正しくありません"),
            Self::PinnedTreeCorrupt => write!(
                f,
                "pinned.toml のフォルダのつながりが壊れています（親が見つからない、または循環している項目があります）"
            ),
            Self::UnsupportedSchema { file, found } => write!(
                f,
                "{file} はこの版より新しい形式で保存されています（形式の版 {found}、この版が読めるのは \
                 {SCHEMA_VERSION} まで）。新しい版の CLCLR で開いてください"
            ),
            Self::MissingFormat { format_name } => {
                write!(f, "項目のデータ（{format_name}）が見つからないか、壊れています")
            }
            Self::ImageTooLarge { width, height } => write!(
                f,
                "画像が大きすぎるため、元に戻せません（{width} × {height}。戻せるのは、画素を RGBA に換算して {} MiB まで）",
                MAX_RESTORE_RGBA / (1024 * 1024)
            ),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Parse { .. } => None,
            Self::Serialize(e) => Some(e),
            Self::InvalidTimestamp => None,
            Self::PinnedTreeCorrupt => None,
            Self::UnsupportedSchema { .. } => None,
            Self::MissingFormat { .. } | Self::ImageTooLarge { .. } => None,
        }
    }
}

impl From<io::Error> for StorageError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// `toml` の読み込みの誤りを、画面に出す1行の説明にする。位置が分かれば「3 行目、5 文字目: 」を
/// 前に付け（`input` の中の文字で数える）、続けて `toml` の説明を付ける。`toml` の説明は英語の
/// まま（依存クレートの文言は訳さない）。`toml` の `Display` は該当行の抜き出しと `^` の
/// 印を複数行で出すが、メッセージボックスでは文字幅がそろわず位置がずれるので使わない。
pub(crate) fn toml_error_detail(e: &toml::de::Error, input: &str) -> String {
    let message = e.message().trim();
    match e.span().and_then(|span| input.get(..span.start)) {
        Some(before) => {
            let line = before.matches('\n').count() + 1;
            let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
            format!("{line} 行目、{column} 文字目: {message}")
        }
        None => message.to_string(),
    }
}

/// history.toml / pinned.toml の中身を読む（書式の誤りは位置つきの `StorageError::Parse`）。
fn parse_index<T: serde::de::DeserializeOwned>(file: &'static str, text: &str) -> Result<T> {
    toml::from_str(text).map_err(|e| StorageError::Parse { file, detail: toml_error_detail(&e, text) })
}

impl From<toml::ser::Error> for StorageError {
    fn from(e: toml::ser::Error) -> Self {
        Self::Serialize(e)
    }
}

type Result<T> = std::result::Result<T, StorageError>;

// --- TOML DTO ---
//
// ここから下のstructはディスク上のTOML表現であり、data.rs/store.rsのメモリ上の
// 表現（Entry/Node/Folder）とは別物。両者の変換は本ファイルのStorage/tree helperが担う。

/// history.toml / pinned.toml のスキーマ版。ファイル形式を非互換に変えるときに上げる。
/// 0は版の記録がない形式で、そのまま読める。
/// 書き込みは常にこの版で行う。
const SCHEMA_VERSION: u32 = 1;

/// 読み込んだ版がこのビルドで扱えるか検査する（`SCHEMA_VERSION`以下なら可）。
fn check_schema_version(file: &'static str, found: u32) -> Result<()> {
    if found > SCHEMA_VERSION {
        return Err(StorageError::UnsupportedSchema { file, found });
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct HistoryIndex {
    /// TOMLは値をテーブルより前に書く必要があるため、配列（`[[entries]]`）より先に置く
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    entries: Vec<EntryMeta>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct EntryMeta {
    pub id: Uuid,
    pub title: Option<String>,
    /// UNIX epochからの秒数。TOMLネイティブのfloatで表現できるため
    /// 文字列日付やカスタム型は使わない。
    pub modified: f64,
    /// 重複チェック用コンテンツハッシュ（`Entry::content_hash`）。blobを読まずに
    /// 重複判定するために永続化する。この項目の無いファイルでは0（どの実ハッシュとも
    /// 事実上一致しないため、その項目は重複扱いされないだけで害はない）。
    /// TOMLの整数はi64でu64の上位ビットが立つ値を表現できないため、
    /// 16進文字列（例: "cbf29ce484222325"）として保存する。
    #[serde(default, with = "hash_hex")]
    pub hash: u64,
    /// 一覧表示用テキスト（CF_UNICODETEXTの先頭部分）。一覧描画でblobを
    /// 読まないために保存時に生成する。テキストを含まないエントリはNone。
    #[serde(default)]
    pub preview: Option<String>,
    pub formats: Vec<FormatMeta>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct FormatMeta {
    pub format_name: String,
    pub format_id: u32,
    /// blobs/ディレクトリ内のファイル名。実データはここには持たず、
    /// 必要な時に`Storage::load_blob`で読み込む。
    pub blob: String,
    pub size: u64,
    /// サムネイルのファイル名（CF_DIBで長辺が`dib::THUMB_LONG_EDGE`超の場合のみ）。
    /// 一覧描画はフルblobではなくこちらを読む。
    #[serde(default)]
    pub thumb: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct PinnedIndex {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    nodes: Vec<NodeMeta>,
}

/// pinned.toml上でのNode/Folderのフラット表現。
/// TOMLは再帰的なテーブルの表現に向かないため、`parent_id`+`order`で親子関係と
/// 兄弟順を持たせ、読み込み時に`build_tree`でVec<Node>のツリーへ復元する。
#[derive(Clone, Serialize, Deserialize)]
struct NodeMeta {
    id: Uuid,
    parent_id: Option<Uuid>,
    order: u32,
    /// "item" または "folder"。それ以外の値は`build_children`が書式の誤りとして断る。
    kind: String,
    title: Option<String>,
    modified: Option<f64>,
    #[serde(default)]
    formats: Vec<FormatMeta>,
}

// --- Storage ---

/// 設定・データの基準ディレクトリ = exe と同じフォルダ。
/// ポータブル構成で、config.toml・
/// history.toml・pinned.toml・blobs/ をすべてここに置く。
/// exe パスが取得できない場合はカレントディレクトリにフォールバックする。
pub fn base_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 保存先ディレクトリだけを持つ軽量な値。ロックの外でblobを読むため
/// （メニューのツールチップ）、`HistoryService::storage_handle`が複製を渡す。
#[derive(Clone)]
pub struct Storage {
    data_dir: PathBuf,
}

impl Storage {
    pub fn open(data_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&data_dir)?;
        fs::create_dir_all(data_dir.join("blobs"))?;
        Ok(Self { data_dir })
    }

    /// 既定のデータ置き場（= exe と同じフォルダ。`base_dir`参照）。
    pub fn default_data_dir() -> PathBuf {
        base_dir()
    }

    // --- History ---

    pub fn load_history_index(&self) -> Result<Vec<EntryMeta>> {
        let Some(text) = read_if_exists(&self.data_dir.join("history.toml"))? else {
            return Ok(Vec::new());
        };
        let index: HistoryIndex = parse_index("history.toml", &text)?;
        check_schema_version("history.toml", index.schema_version)?;
        for entry in &index.entries {
            check_blob_names("history.toml", &entry.formats)?;
        }
        Ok(index.entries)
    }

    pub fn save_history_index(&self, entries: &[EntryMeta]) -> Result<()> {
        let index = HistoryIndex {
            schema_version: SCHEMA_VERSION,
            entries: entries.to_vec(),
        };
        let text = toml::to_string_pretty(&index)?;
        write_atomic(&self.data_dir.join("history.toml"), text.as_bytes())?;
        Ok(())
    }

    /// history.tomlから指定idのエントリだけを取り除いて書き戻す。メモリ上の
    /// `self.history`全件を書く`save_history_index`とは異なり、ディスク上に
    /// 既にある内容からの差分除去に限定される。呼び出し元が「完全メモリモードで
    /// 一度もディスクへ書いていない項目」のidを渡しても、その項目はそもそも
    /// ディスク上に存在しないため何も変わらない（メモリ専用モードでは
    /// プレビュー本文を含むメタデータを新規にディスクへ書いてしまわないための
    /// 安全策。HistoryService::persist_evicted_then_remove_blobs参照）。
    /// 対象idが1件も一致しない場合は書き込み自体を行わない（save_on_exit
    /// のみの構成で不要な書き込みを増やさないため）。
    pub fn remove_from_history_index(&self, ids: &[Uuid]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut entries = self.load_history_index()?;
        let before = entries.len();
        entries.retain(|e| !ids.contains(&e.id));
        if entries.len() == before {
            return Ok(());
        }
        self.save_history_index(&entries)
    }

    /// メタデータからEntryを復元する。この時点で該当するblobを実際に読み込む
    /// （履歴一覧を舐めるだけならこれを呼ばず`EntryMeta`のまま扱ってよい）。
    ///
    /// blobは形式ごとの個別ファイルのため一部だけ欠損し得る。読めない形式は
    /// スキップして残りを復元し、1ファイルの欠損がEntry全体（ひいては
    /// `load_pinned`のツリー全体）の失敗に波及しないようにする。全形式が
    /// 読めない場合もformatsが空のEntryとして返し、エントリ自体は保全する
    /// （エラーで弾くと次回保存時にツリーから静かに消えてしまうため）。
    #[allow(dead_code, reason = "送る・ピン留めは厳密な読み込み（load_entry_data_strict）に移し、今は使う所がない（テストが使う）")]
    pub fn load_entry_data(&self, meta: &EntryMeta) -> Result<Entry> {
        let formats = meta.formats.iter().filter_map(|fm| self.load_format(fm).ok()).collect();
        Ok(Entry {
            id: meta.id,
            title: meta.title.clone(),
            modified: timestamp_to_system_time(meta.modified)?,
            formats,
        })
    }

    /// `load_entry_data` の厳密版。メタデータにある形式が1つでも読めなければエラーにする
    /// （送る・ピン留めの複製で、一部の形式だけが欠けたものを正常として扱わないため）。
    /// プレビュー・ツールチップは、読めた分だけを出す `load_entry_data` を使う。
    pub fn load_entry_data_strict(&self, meta: &EntryMeta) -> Result<Entry> {
        let formats = meta.formats.iter().map(|fm| self.load_format(fm)).collect::<Result<Vec<Format>>>()?;
        Ok(Entry {
            id: meta.id,
            title: meta.title.clone(),
            modified: timestamp_to_system_time(meta.modified)?,
            formats,
        })
    }

    /// 1形式分の blob を読んで復元する。読めない・壊れているなら `MissingFormat`、戻すには大きすぎる画像なら
    /// `ImageTooLarge`。
    fn load_format(&self, fm: &FormatMeta) -> Result<Format> {
        let missing = || StorageError::MissingFormat { format_name: fm.format_name.clone() };
        // .webpはCF_DIBの変換保存（save_entry_filtered参照）。CLCLR が書く形でない・寸法に見合わない長さの
        // WebP は全体を読まず、復元に失敗した破損WebPと同じく、blob欠損と同じ扱いにする。`MAX_RESTORE_RGBA` を
        // 超える画像は全体を読まない。生のまま（.bin）の blob は、保存したときの大きさ（`size`）と違えば壊れている
        // （保存の後に短くなった・書き換えられた）ので、全体を読まずに欠損と同じ扱いにする
        let data = if fm.blob.ends_with(".webp") {
            match self.load_webp(&fm.blob, MAX_RESTORE_RGBA).ok_or_else(missing)? {
                LoadedWebp::Data(raw) => dib::webp_to_dib(&raw).map_err(|_| missing())?,
                LoadedWebp::TooLarge { width, height } => return Err(StorageError::ImageTooLarge { width, height }),
            }
        } else {
            self.load_blob_exact(&fm.blob, fm.size).map_err(|_| missing())?
        };
        Ok(Format {
            format_name: fm.format_name.clone(),
            format_id: fm.format_id,
            data,
        })
    }

    /// Entryの各フォーマットをblobとしてディスクに書き出し、対応するメタデータを返す。
    /// blob名は`{entry.id}_{フォーマットのインデックス}.bin`で、同じEntryを再保存すると
    /// 同名ファイルを上書きする（ただしフォーマット数が減った場合、消えたインデックス
    /// 以降のblobはこの関数では削除されず孤児化する。エントリ編集機能を実装する際に対応）。
    pub fn save_entry(&self, entry: &Entry) -> Result<EntryMeta> {
        self.save_entry_filtered(entry, |_| true)
    }

    /// `save_entry`の形式フィルタ付き版。`keep`がfalseを返した形式はディスクに
    /// 書かずメタデータからも除外する（`FormatFilter.save=false`＝プロセス実行中
    /// のみメモリ保持、の実現手段）。blob名は元のインデックスから決まるため、
    /// フィルタの有無で他形式のblob名は変わらない。
    ///
    /// 呼び出し元は常に新規生成の`entry.id`（`Entry::new`直後）でこの関数を呼ぶため
    /// （`pin_by_id`は複製元を参照せず新idで複製、`on_captured`は捕捉のたび新規
    /// エントリ）、blob名は本呼び出しで初めて使われるファイル名になる。途中の形式で
    /// 書き込みが失敗した場合、それより前に本呼び出しで書いたblob（本体・サムネイル
    /// とも）は他から参照されないため、エラーを返す前に削除してから戻る
    /// （孤児化の回避。削除自体の失敗は握り潰す＝ベストエフォート、削除できなくても
    /// 呼び出し元にはもともとのI/Oエラーを伝える）。
    pub fn save_entry_filtered(
        &self,
        entry: &Entry,
        keep: impl Fn(&Format) -> bool,
    ) -> Result<EntryMeta> {
        let mut formats = Vec::new();
        let mut written_blobs: Vec<String> = Vec::new();

        for (i, fmt) in entry.formats.iter().enumerate().filter(|(_, fmt)| keep(fmt)) {
            // CF_DIBはWebPロスレスに変換して保存し、読み込み時は拡張子で
            // 復元経路を判別する。対応外・破損DIBは生バイナリにフォールバック
            let (blob_name, stored, has_webp) = if fmt.format_name == "CF_DIB" {
                match dib::dib_to_webp(&fmt.data) {
                    Ok(webp) => (format!("{}_{i}.webp", entry.id), Cow::Owned(webp), true),
                    Err(_) => (
                        format!("{}_{i}.bin", entry.id),
                        Cow::Borrowed(fmt.data.as_slice()),
                        false,
                    ),
                }
            } else {
                (
                    format!("{}_{i}.bin", entry.id),
                    Cow::Borrowed(fmt.data.as_slice()),
                    false,
                )
            };
            if let Err(e) = self.save_blob(&blob_name, &stored) {
                self.remove_blob_files(&written_blobs);
                return Err(e);
            }
            written_blobs.push(blob_name.clone());

            // サムネイル併産（一覧描画でフルblobを読まないため）。生成不能・
            // 不要（小画像）はNoneで続行し、保存自体は失敗させない
            let thumb = if has_webp {
                match dib::dib_to_thumbnail_webp(&fmt.data) {
                    Ok(Some(data)) => {
                        let name = format!("{}_{i}.thumb.webp", entry.id);
                        if let Err(e) = self.save_blob(&name, &data) {
                            self.remove_blob_files(&written_blobs);
                            return Err(e);
                        }
                        written_blobs.push(name.clone());
                        Some(name)
                    }
                    _ => None,
                }
            } else {
                None
            };

            formats.push(FormatMeta {
                format_name: fmt.format_name.clone(),
                format_id: fmt.format_id,
                blob: blob_name,
                // sizeは常に元データ（DIB）の長さ。blobファイルの実サイズではない
                size: fmt.data.len() as u64,
                thumb,
            });
        }

        Ok(EntryMeta {
            id: entry.id,
            title: entry.title.clone(),
            modified: system_time_to_timestamp(entry.modified),
            // hashはkeepで除外される形式も含めた全形式で計算する。重複チェックは
            // 「エントリの内容」同士の比較であり、永続化の有無とは無関係のため
            hash: entry.content_hash(),
            // previewはこのファイル（history.toml）に平文で載るため、保存対象の
            // 形式だけから生成する（save=falseのテキストをディスクに漏らさない）
            preview: text_preview(entry.formats.iter().filter(|f| keep(f))),
            formats,
        })
    }

    /// entryが参照するblobファイルのみを削除する。history.toml側のインデックスから
    /// エントリ自体を取り除くのは呼び出し側の責務（この関数は行わない）。
    pub fn remove_entry_blobs(&self, meta: &EntryMeta) -> Result<()> {
        for fm in &meta.formats {
            let path = self.blob_path(&fm.blob);
            if path.exists() {
                fs::remove_file(path)?;
            }
            if let Some(thumb) = &fm.thumb {
                let path = self.blob_path(thumb);
                if path.exists() {
                    fs::remove_file(path)?;
                }
            }
        }
        Ok(())
    }

    /// サムネイルWebPをファイル名で読む（無ければNone）。一覧描画用。
    /// FormatMeta.thumbの値だけを受け付ける（.thumb.webp以外は読まない）。
    pub fn load_thumbnail(&self, name: &str, max_rgba: u64) -> Option<LoadedWebp> {
        if !name.ends_with(".thumb.webp") {
            return None;
        }
        self.load_webp(name, max_rgba)
    }

    /// WebP の blob（サムネイルを含む）を、先に見出しと長さを確かめてから読む。CLCLR が書く形でないもの
    /// （`dib::webp_header_dimensions`）・寸法に見合わない長さのもの（`dib::webp_file_len_limit`）・読めないものは
    /// None。画素を RGBA に換算した大きさが `max_rgba` を超えるものは、全体を読まずに `TooLarge`。見出しと全体は
    /// 同じ開いたファイルから読み、読めた長さが確かめた長さと違えば（読む間に書き換えられた）None。
    pub(crate) fn load_webp(&self, name: &str, max_rgba: u64) -> Option<LoadedWebp> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = fs::File::open(self.blob_path(name)).ok()?;
        let len = file.metadata().ok()?.len();
        let mut head = Vec::with_capacity(dib::WEBP_HEADER_LEN);
        (&mut file).take(dib::WEBP_HEADER_LEN as u64).read_to_end(&mut head).ok()?;
        let (width, height) = dib::webp_header_dimensions(&head, len).ok()?;
        if u64::from(width) * u64::from(height) * 4 > max_rgba {
            return Some(LoadedWebp::TooLarge { width, height });
        }
        if len > dib::webp_file_len_limit(width, height) {
            return None;
        }
        file.seek(SeekFrom::Start(0)).ok()?;
        let mut data = Vec::with_capacity(len as usize);
        file.take(len + 1).read_to_end(&mut data).ok()?;
        (data.len() as u64 == len).then_some(LoadedWebp::Data(data))
    }

    /// 生のまま（.bin）の blob を、長さが `size` のときだけ読む（長さが違えば、全体を読まずに `Err`）。
    fn load_blob_exact(&self, name: &str, size: u64) -> Result<Vec<u8>> {
        use std::io::Read;
        let file = fs::File::open(self.blob_path(name))?;
        let invalid = || StorageError::Io(io::Error::new(io::ErrorKind::InvalidData, "保存したときの大きさと違います"));
        if file.metadata()?.len() != size {
            return Err(invalid());
        }
        let mut data = Vec::with_capacity(size as usize);
        file.take(size + 1).read_to_end(&mut data)?;
        if data.len() as u64 != size {
            return Err(invalid());
        }
        Ok(data)
    }

    // --- Pinned ---

    /// ピン留めツリー（メタデータのみ）を読み込む。blobには触れない。
    pub fn load_pinned(&self) -> Result<Vec<PinnedNode>> {
        let Some(text) = read_if_exists(&self.data_dir.join("pinned.toml"))? else {
            return Ok(Vec::new());
        };
        let index: PinnedIndex = parse_index("pinned.toml", &text)?;
        check_schema_version("pinned.toml", index.schema_version)?;
        build_tree(&index.nodes)
    }

    /// ツリー全体をフラット化してpinned.tomlへ書き出す。blobはpin操作時に
    /// 書き込み済みのため、ここではインデックスのみを更新する。
    pub fn save_pinned(&self, nodes: &[PinnedNode]) -> Result<()> {
        let mut flat = Vec::new();
        flatten_tree(nodes, None, &mut flat);
        let index = PinnedIndex {
            schema_version: SCHEMA_VERSION,
            nodes: flat,
        };
        let text = toml::to_string_pretty(&index)?;
        write_atomic(&self.data_dir.join("pinned.toml"), text.as_bytes())?;
        Ok(())
    }

    // --- Blob (internal) ---

    fn blob_path(&self, name: &str) -> PathBuf {
        self.data_dir.join("blobs").join(name)
    }

    pub(crate) fn load_blob(&self, name: &str) -> Result<Vec<u8>> {
        Ok(fs::read(self.blob_path(name))?)
    }

    /// blobの先頭`max_bytes`バイトだけを読む（巨大なテキスト等を全量読まずに
    /// 先頭だけ見たい用途。ファイルがそれより短ければ全体を返す）。
    pub(crate) fn load_blob_prefix(&self, name: &str, max_bytes: usize) -> Result<Vec<u8>> {
        use std::io::Read;
        let mut buf = Vec::new();
        fs::File::open(self.blob_path(name))?
            .take(max_bytes as u64)
            .read_to_end(&mut buf)?;
        Ok(buf)
    }

    /// 保存済みの項目のテキストのプレビュー（取り込み時の `EntryMeta.preview` と同じ作り方。CF_UNICODETEXT の
    /// blob の先頭だけを読む）。テキストの形式が無い・読めなければ None。pinned.toml は preview を持たないので、
    /// ピン留めの自動の名前を作り直すときに使う。
    pub(crate) fn load_text_preview(&self, meta: &EntryMeta) -> Option<String> {
        let text = meta.formats.iter().find(|f| f.format_name == "CF_UNICODETEXT")?;
        // 1文字は UTF-16 で最大 2 単位（4 バイト）
        let data = self.load_blob_prefix(&text.blob, PREVIEW_MAX_CHARS * 4).ok()?;
        let format = Format { format_name: text.format_name.clone(), format_id: text.format_id, data };
        text_preview(std::iter::once(&format))
    }

    // --- データのチェック（`Core::check_data`・`clean_data` が操作用のロックの中で使う） ---

    /// `blobs` の直下の項目を1つずつ調べ、ファイルなら名前と大きさ、ファイルでない（フォルダ・リンク）なら None を
    /// 返す（呼び出し側が項目ごとに途中でやめられるよう、先に全部を集めず、ファイルでない項目も読み飛ばさずに
    /// 返す）。UTF-16 として正しくない名前は置き換え文字で返す（blob の名前の形に
    /// 合わないので、対象外として消さない）。
    pub(crate) fn blob_files(&self) -> Result<impl Iterator<Item = Result<Option<(String, u64)>>>> {
        Ok(fs::read_dir(self.data_dir.join("blobs"))?.map(|entry| -> Result<Option<(String, u64)>> {
            let entry = entry?;
            // `DirEntry::file_type` はリンクを辿らない（リンクは is_file にならない）
            if !entry.file_type()?.is_file() {
                return Ok(None);
            }
            Ok(Some((entry.file_name().to_string_lossy().into_owned(), entry.metadata()?.len())))
        }))
    }

    /// データフォルダの直下のファイル `name` の大きさ。無ければ None（無い以外の誤りは Err）。
    pub(crate) fn data_file_size(&self, name: &str) -> Result<Option<u64>> {
        match fs::symlink_metadata(self.data_dir.join(name)) {
            Ok(m) if m.is_file() => Ok(Some(m.len())),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// blob `name` がファイルとしてあるか。無い（`NotFound`）・ファイルでない（同じ名前のフォルダなど。読めない
    /// ので欠けているのと同じ）ときは false、ほかの誤り（権限など）は Err（調べられないものを「無い」と取り違えて
    /// 項目を消さない）。
    pub(crate) fn blob_exists(&self, name: &str) -> Result<bool> {
        match fs::metadata(self.blob_path(name)) {
            Ok(m) => Ok(m.is_file()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// blob `name` を消す（もう無ければ何もしない）。
    pub(crate) fn remove_blob(&self, name: &str) -> io::Result<()> {
        match fs::remove_file(self.blob_path(name)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    /// データフォルダの直下のファイル `name` を消す（もう無ければ何もしない）。
    pub(crate) fn remove_data_file(&self, name: &str) -> io::Result<()> {
        match fs::remove_file(self.data_dir.join(name)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    /// `save_entry_filtered`が失敗を巻き戻すためのヘルパー。個々の削除失敗は
    /// 握り潰す（ベストエフォート。既にI/Oが不調という状況での削除失敗を
    /// 呼び出し元のエラーに上書きしても情報が増えないため）。
    fn remove_blob_files(&self, names: &[String]) {
        for name in names {
            let _ = fs::remove_file(self.blob_path(name));
        }
    }

    fn save_blob(&self, name: &str, data: &[u8]) -> Result<()> {
        Ok(write_atomic(&self.blob_path(name), data)?)
    }
}

/// blob・サムネイルの名前が `blobs` の中のファイル名だけか（区切り文字・ドライブ・`..` を含まない）。
/// 手で書いた `history.toml`・`pinned.toml` の名前で、`blobs` の外のファイルを読んだり、削除・押し出しで
/// 消したりしない。
fn is_plain_file_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', ':', '\0'])
}

fn check_blob_names(file: &'static str, formats: &[FormatMeta]) -> Result<()> {
    for fm in formats {
        for name in std::iter::once(&fm.blob).chain(fm.thumb.as_ref()) {
            if !is_plain_file_name(name) {
                return Err(StorageError::Parse { file, detail: format!("blob のファイル名が正しくありません（{name}）") });
            }
        }
    }
    Ok(())
}

/// ファイルを文字列で読む。無ければ（`NotFound`）None、ほかの読み取りの誤りは `Err`（`Path::exists` で分けると、
/// 権限などで調べられないファイルを「無い」と取り違え、読めない履歴を空として起動し、後の保存で上書きする）。
fn read_if_exists(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

// --- Atomic write ---

/// 一時ファイル（`<元名>.tmp`）に書き切り、fsyncしてからrenameで置き換える。
/// 書き込み途中のクラッシュ・電源断で既存ファイルが半端な内容に壊れるのを防ぐ
/// （renameによる同一ディレクトリ内の置き換えなので、元の内容か新しい内容のどちらかが
/// 必ず残る）。blobにも適用するのは、書きかけのblobは欠損と違い正常に「読めて」
/// しまい、切り詰められたデータが検知されずに使われるのを避けるため。
/// クラッシュ時に残った.tmpはrenameで上書きされるか無害な孤児になるだけなので、
/// 起動時の掃除は行っていない。
pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);

    let mut file = fs::File::create(&tmp)?;
    file.write_all(data)?;
    file.sync_all()?;
    drop(file);

    fs::rename(&tmp, path)
}

// --- Tree helpers ---
//
// NodeMetaのフラットなリスト(parent_id参照)とVec<PinnedNode>のツリーを相互変換する。
// メタデータの変換のみでblobには一切触れない。
// pinned.tomlは手編集され得るため、`visited`でフォルダidの再訪を検出し、
// ルートから辿れる経路上の循環参照（自己参照や壊れたparent_id）があっても
// スタックオーバーフローせず即座に`StorageError::PinnedTreeCorrupt`を返す。
//
// ただし`visited`だけではルートから辿れない孤立ノードを検出できない
// (例: A.parent_id=Some(B.id)、B.parent_id=Some(A.id)という孤立した循環は
// どちらもparent_id=Noneではないため、build_childrenの再帰に一度も現れず
// 検査自体が素通りする)。そのため`reached`で「実際に処理へ到達したid」を
// 種別を問わず記録し、build_tree側で`flat`の全件数と突き合わせる。一致しない
// 場合はルートから到達不能なノードが存在するということなので、黙って消す
// 代わりにエラーを返す。

fn build_tree(flat: &[NodeMeta]) -> Result<Vec<PinnedNode>> {
    let mut visited = HashSet::new();
    let mut reached = HashSet::new();
    let tree = build_children(flat, None, &mut visited, &mut reached)?;
    if reached.len() != flat.len() {
        return Err(StorageError::PinnedTreeCorrupt);
    }
    Ok(tree)
}

fn build_children(
    flat: &[NodeMeta],
    parent: Option<Uuid>,
    visited: &mut HashSet<Uuid>,
    reached: &mut HashSet<Uuid>,
) -> Result<Vec<PinnedNode>> {
    let mut targets: Vec<&NodeMeta> = flat.iter().filter(|n| n.parent_id == parent).collect();
    targets.sort_by_key(|n| n.order);

    targets
        .into_iter()
        .filter_map(|meta| {
            reached.insert(meta.id);
            match meta.kind.as_str() {
                "item" => Some(check_blob_names("pinned.toml", &meta.formats).map(|()| {
                    PinnedNode::Item(EntryMeta {
                        id: meta.id,
                        title: meta.title.clone(),
                        modified: meta.modified.unwrap_or(0.0),
                        // ピン留めは重複チェックの対象外。一覧表示はtitleを使う
                        hash: 0,
                        preview: None,
                        formats: meta.formats.clone(),
                    })
                })),
                "folder" => {
                    if !visited.insert(meta.id) {
                        return Some(Err(StorageError::PinnedTreeCorrupt));
                    }
                    Some(
                        build_children(flat, Some(meta.id), visited, reached).map(|c| {
                            PinnedNode::Folder(PinnedFolder {
                                id: meta.id,
                                title: meta.title.clone().unwrap_or_default(),
                                children: c,
                            })
                        }),
                    )
                }
                // 知らない種類は黙って捨てない（捨てると、次の保存でファイルからも消える）
                other => Some(Err(StorageError::Parse {
                    file: "pinned.toml",
                    detail: format!("kind が item・folder のどちらでもない項目があります（{other}）"),
                })),
            }
        })
        .collect()
}

fn flatten_tree(nodes: &[PinnedNode], parent_id: Option<Uuid>, out: &mut Vec<NodeMeta>) {
    for (i, node) in nodes.iter().enumerate() {
        match node {
            PinnedNode::Item(meta) => {
                out.push(NodeMeta {
                    id: meta.id,
                    parent_id,
                    order: i as u32,
                    kind: "item".to_string(),
                    title: meta.title.clone(),
                    modified: Some(meta.modified),
                    formats: meta.formats.clone(),
                });
            }
            PinnedNode::Folder(folder) => {
                out.push(NodeMeta {
                    id: folder.id,
                    parent_id,
                    order: i as u32,
                    kind: "folder".to_string(),
                    title: Some(folder.title.clone()),
                    modified: None,
                    formats: Vec::new(),
                });
                flatten_tree(&folder.children, Some(folder.id), out);
            }
        }
    }
}

// --- Hash serialization ---

/// u64ハッシュをTOMLに16進文字列で載せるためのserdeアダプタ。
/// TOML整数はi64のため、u64をそのまま書くと上位ビットが立つ値で
/// シリアライズエラーになる（実機検証で発覚）。
mod hash_hex {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{v:016x}"))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        let s = String::deserialize(d)?;
        u64::from_str_radix(&s, 16).map_err(serde::de::Error::custom)
    }
}

// --- Preview / memory-only meta ---

/// 一覧表示用のテキストプレビューを生成する。CF_UNICODETEXT（UTF-16LE、
/// NUL終端）の先頭200文字まで。改行はそのまま残し、切り詰めはUI側の責務。
const PREVIEW_MAX_CHARS: usize = 200;

fn text_preview<'a>(formats: impl IntoIterator<Item = &'a Format>) -> Option<String> {
    let fmt = formats
        .into_iter()
        .find(|f| f.format_name == "CF_UNICODETEXT")?;
    // 先頭だけを読む（全文を写さない。大きなテキストの取り込みのたびに全文の分を余計に使わないため）。
    // 1文字は UTF-16 で最大 2 単位なので、2倍の単位を読めば `PREVIEW_MAX_CHARS` 文字に足りる
    let units: Vec<u16> = fmt
        .data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .take(PREVIEW_MAX_CHARS * 2)
        .collect();
    let text = String::from_utf16_lossy(&units);
    Some(text.chars().take(PREVIEW_MAX_CHARS).collect())
}

impl EntryMeta {
    /// ディスクに何も書かずにEntryからメタデータだけを作る（完全メモリモード用）。
    /// formatsは空＝blob参照なし。プレビューはディスクに載らないため全形式から生成してよい。
    pub fn memory_only(entry: &Entry) -> Self {
        Self {
            id: entry.id,
            title: entry.title.clone(),
            modified: system_time_to_timestamp(entry.modified),
            hash: entry.content_hash(),
            preview: text_preview(entry.formats.iter()),
            formats: Vec::new(),
        }
    }
}

// --- Timestamp helpers ---

fn system_time_to_timestamp(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs_f64()
}

/// TOMLはNaN/Infinityを正当なfloatリテラルとして受理するため、手編集や破損した
/// ファイル由来の値でもpanicさせない。`try_from_secs_f64`はNaN/±Infinity/負値に
/// 加えてDurationに収まらない巨大値も弾き、SystemTimeへの加算も`checked_add`で
/// オーバーフローを吸収する。
fn timestamp_to_system_time(ts: f64) -> Result<SystemTime> {
    let d = Duration::try_from_secs_f64(ts).map_err(|_| StorageError::InvalidTimestamp)?;
    UNIX_EPOCH
        .checked_add(d)
        .ok_or(StorageError::InvalidTimestamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unicodetext_format(text: &str) -> Format {
        let mut data: Vec<u8> = text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        data.extend_from_slice(&0u16.to_le_bytes()); // NUL終端
        Format {
            format_name: "CF_UNICODETEXT".to_string(),
            format_id: 13,
            data,
        }
    }

    #[test]
    fn text_preview_returns_none_without_unicodetext_format() {
        let formats = vec![Format {
            format_name: "CF_DIB".to_string(),
            format_id: 8,
            data: Vec::new(),
        }];
        assert!(text_preview(formats.iter()).is_none());
    }

    #[test]
    fn text_preview_stops_at_nul_terminator() {
        let mut data: Vec<u8> = "abc".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&('X' as u16).to_le_bytes()); // NUL終端より後のゴミ
        let fmt = Format {
            format_name: "CF_UNICODETEXT".to_string(),
            format_id: 13,
            data,
        };
        assert_eq!(text_preview(std::iter::once(&fmt)).unwrap(), "abc");
    }

    #[test]
    fn text_preview_does_not_truncate_at_exactly_the_limit() {
        let exactly_limit = "あ".repeat(PREVIEW_MAX_CHARS);
        let fmt = unicodetext_format(&exactly_limit);
        let preview = text_preview(std::iter::once(&fmt)).unwrap();
        assert_eq!(preview.chars().count(), PREVIEW_MAX_CHARS);
        assert_eq!(preview, exactly_limit);
    }

    #[test]
    fn text_preview_truncates_text_over_the_limit() {
        let over_limit = "あ".repeat(PREVIEW_MAX_CHARS + 1);
        let fmt = unicodetext_format(&over_limit);
        let preview = text_preview(std::iter::once(&fmt)).unwrap();
        assert_eq!(preview.chars().count(), PREVIEW_MAX_CHARS);
        assert_eq!(preview, "あ".repeat(PREVIEW_MAX_CHARS));
    }

    fn meta_with_id(id: Uuid) -> EntryMeta {
        EntryMeta {
            id,
            title: None,
            modified: 0.0,
            hash: 0,
            preview: None,
            formats: Vec::new(),
        }
    }

    #[test]
    fn remove_from_history_index_is_no_op_for_empty_ids() {
        let dir = std::env::temp_dir().join(format!("clclr-test-{}", Uuid::new_v4()));
        let storage = Storage::open(dir.clone()).unwrap();
        storage.save_history_index(&[meta_with_id(Uuid::new_v4())]).unwrap();
        let path = dir.join("history.toml");
        let before = fs::read_to_string(&path).unwrap();

        storage.remove_from_history_index(&[]).unwrap();

        // 空idsでは早期returnし、load_history_indexすら呼ばれず内容は変化しない
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(before, after);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn remove_from_history_index_skips_write_when_no_id_matches() {
        let dir = std::env::temp_dir().join(format!("clclr-test-{}", Uuid::new_v4()));
        let storage = Storage::open(dir.clone()).unwrap();
        storage.save_history_index(&[meta_with_id(Uuid::new_v4())]).unwrap();
        let path = dir.join("history.toml");
        let before = fs::read_to_string(&path).unwrap();

        // どのidとも一致しない場合は書き込み自体をスキップする（内容が変わらないことで確認）
        storage.remove_from_history_index(&[Uuid::new_v4()]).unwrap();

        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(before, after);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn remove_from_history_index_removes_only_matching_entries() {
        let dir = std::env::temp_dir().join(format!("clclr-test-{}", Uuid::new_v4()));
        let storage = Storage::open(dir.clone()).unwrap();
        let keep = Uuid::new_v4();
        let remove = Uuid::new_v4();
        storage
            .save_history_index(&[meta_with_id(keep), meta_with_id(remove)])
            .unwrap();

        storage.remove_from_history_index(&[remove]).unwrap();

        let loaded = storage.load_history_index().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, keep);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn timestamp_roundtrip() {
        let now = SystemTime::now();
        let restored = timestamp_to_system_time(system_time_to_timestamp(now)).unwrap();
        // f64経由でサブ秒精度が僅かに落ちるため、1ミリ秒以内の一致で判定する
        let diff = now
            .duration_since(restored)
            .unwrap_or_else(|e| e.duration());
        assert!(diff < Duration::from_millis(1));
    }

    #[test]
    fn pinned_meta_tree_roundtrips() {
        let dir = std::env::temp_dir().join(format!("clclr-test-{}", Uuid::new_v4()));
        let storage = Storage::open(dir.clone()).unwrap();

        let item = |title: &str| {
            PinnedNode::Item(EntryMeta {
                id: Uuid::new_v4(),
                title: Some(title.to_string()),
                modified: 100.0,
                hash: 0,
                preview: None,
                formats: Vec::new(),
            })
        };
        let folder_id = Uuid::new_v4();
        let tree = vec![
            item("root-item"),
            PinnedNode::Folder(PinnedFolder {
                id: folder_id,
                title: "仕事".to_string(),
                children: vec![item("child-1"), item("child-2")],
            }),
        ];
        storage.save_pinned(&tree).unwrap();
        let loaded = storage.load_pinned().unwrap();

        assert_eq!(loaded.len(), 2);
        let PinnedNode::Item(first) = &loaded[0] else {
            panic!("expected item");
        };
        assert_eq!(first.title.as_deref(), Some("root-item"));
        let PinnedNode::Folder(folder) = &loaded[1] else {
            panic!("expected folder");
        };
        assert_eq!(folder.id, folder_id);
        assert_eq!(folder.title, "仕事");
        assert_eq!(folder.children.len(), 2);
        let PinnedNode::Item(c2) = &folder.children[1] else {
            panic!("expected item");
        };
        assert_eq!(c2.title.as_deref(), Some("child-2"));

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_replaces_files_and_leaves_no_temp() {
        let dir = std::env::temp_dir().join(format!("clclr-test-{}", Uuid::new_v4()));
        let storage = Storage::open(dir.clone()).unwrap();

        let entry = Entry::new(vec![Format {
            format_name: "CF_UNICODETEXT".to_string(),
            format_id: 13,
            data: b"first".to_vec(),
        }]);
        let entry_id = entry.id;
        // 2回保存して上書き（rename置き換え）の経路も通す
        let meta = storage.save_entry(&entry).unwrap();
        storage.save_entry(&entry).unwrap();
        let nodes = [PinnedNode::Item(meta)];
        storage.save_pinned(&nodes).unwrap();
        storage.save_pinned(&nodes).unwrap();
        storage.save_history_index(&[]).unwrap();
        storage.save_history_index(&[]).unwrap();

        // 保存結果が読み戻せること
        let loaded = storage.load_pinned().unwrap();
        assert_eq!(loaded.len(), 1);
        let PinnedNode::Item(loaded_meta) = &loaded[0] else {
            panic!("expected PinnedNode::Item");
        };
        assert_eq!(loaded_meta.id, entry_id);
        let loaded_entry = storage.load_entry_data(loaded_meta).unwrap();
        assert_eq!(loaded_entry.formats[0].data, b"first");

        // .tmpファイルが残っていないこと
        for d in [dir.clone(), dir.join("blobs")] {
            for e in fs::read_dir(d).unwrap() {
                let p = e.unwrap().path();
                assert!(
                    p.extension().is_none_or(|x| x != "tmp"),
                    "leftover temp file: {}",
                    p.display()
                );
            }
        }

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_entry_filtered_removes_already_written_blobs_on_later_failure() {
        // 複数形式のうち後段の書き込みが失敗した場合、先に本呼び出しで書いた
        // blobが孤児として残らず削除されること(2026-09-16修正)。
        // 2番目の形式のblobは`{id}_1.bin`→書き込み時は一時ファイル
        // `{id}_1.bin.tmp`(write_atomic参照)。そこに同名のディレクトリを
        // 事前に作っておくことで、write_atomicの`File::create`を確実に失敗させる。
        let dir = std::env::temp_dir().join(format!("clclr-test-{}", Uuid::new_v4()));
        let storage = Storage::open(dir.clone()).unwrap();

        let entry = Entry::new(vec![
            Format {
                format_name: "CF_UNICODETEXT".to_string(),
                format_id: 13,
                data: b"first".to_vec(),
            },
            Format {
                format_name: "CF_UNICODETEXT".to_string(),
                format_id: 13,
                data: b"second".to_vec(),
            },
        ]);
        let entry_id = entry.id;

        let blocking_tmp = dir
            .join("blobs")
            .join(format!("{entry_id}_1.bin.tmp"));
        fs::create_dir_all(&blocking_tmp).unwrap();

        let result = storage.save_entry(&entry);
        assert!(result.is_err());

        // 1番目の形式は書き込みに成功していたはずだが、2番目の失敗を受けて
        // ロールバックされ、孤児blobとして残っていないこと
        let first_blob = dir.join("blobs").join(format!("{entry_id}_0.bin"));
        assert!(!first_blob.exists(), "orphaned blob was not rolled back");

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn dib_format_is_stored_as_webp_and_restored() {
        let dir = std::env::temp_dir().join(format!("clclr-test-{}", Uuid::new_v4()));
        let storage = Storage::open(dir.clone()).unwrap();

        // 2x2 32bpp BI_RGB ボトムアップの正規形DIB（dib.rsのテストと同じ構成）
        let mut dib_data = Vec::new();
        dib_data.extend_from_slice(&40u32.to_le_bytes());
        dib_data.extend_from_slice(&2i32.to_le_bytes());
        dib_data.extend_from_slice(&2i32.to_le_bytes());
        dib_data.extend_from_slice(&1u16.to_le_bytes());
        dib_data.extend_from_slice(&32u16.to_le_bytes());
        dib_data.extend_from_slice(&0u32.to_le_bytes());
        dib_data.extend_from_slice(&16u32.to_le_bytes());
        dib_data.extend_from_slice(&[0u8; 16]);
        // 下段: B, 白 / 上段: R, G（BGRX）
        dib_data.extend_from_slice(&[255, 0, 0, 0, 255, 255, 255, 0]);
        dib_data.extend_from_slice(&[0, 0, 255, 0, 0, 255, 0, 0]);

        let entry = Entry::new(vec![
            Format {
                format_name: "CF_DIB".to_string(),
                format_id: 8,
                data: dib_data.clone(),
            },
            Format {
                format_name: "CF_UNICODETEXT".to_string(),
                format_id: 13,
                data: b"text".to_vec(),
            },
        ]);
        let meta = storage.save_entry(&entry).unwrap();

        // CF_DIBは.webp、テキストは.binで保存されること
        assert!(meta.formats[0].blob.ends_with(".webp"));
        assert!(meta.formats[1].blob.ends_with(".bin"));
        let webp_file = fs::read(dir.join("blobs").join(&meta.formats[0].blob)).unwrap();
        assert_eq!(&webp_file[0..4], b"RIFF");
        // sizeは元DIBの長さのまま
        assert_eq!(meta.formats[0].size, dib_data.len() as u64);

        // 読み戻すと元のDIBがバイト同一で復元されること（正規形入力のため）
        let loaded = storage.load_entry_data(&meta).unwrap();
        assert_eq!(loaded.formats[0].data, dib_data);
        assert_eq!(loaded.formats[1].data, b"text");

        // 破損したwebpはその形式だけスキップされること（縮退動作）
        fs::write(dir.join("blobs").join(&meta.formats[0].blob), b"broken").unwrap();
        let degraded = storage.load_entry_data(&meta).unwrap();
        assert_eq!(degraded.formats.len(), 1);
        assert_eq!(degraded.formats[0].format_name, "CF_UNICODETEXT");

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn dib_thumbnail_is_saved_and_removed_with_blobs() {
        let dir = std::env::temp_dir().join(format!("clclr-test-{}", Uuid::new_v4()));
        let storage = Storage::open(dir.clone()).unwrap();

        // 200x50 32bpp BI_RGB（長辺200 > 128 なのでサムネイル対象）
        let (w, h) = (200i32, 50i32);
        let mut dib_data = Vec::new();
        dib_data.extend_from_slice(&40u32.to_le_bytes());
        dib_data.extend_from_slice(&w.to_le_bytes());
        dib_data.extend_from_slice(&h.to_le_bytes());
        dib_data.extend_from_slice(&1u16.to_le_bytes());
        dib_data.extend_from_slice(&32u16.to_le_bytes());
        dib_data.extend_from_slice(&0u32.to_le_bytes());
        dib_data.extend_from_slice(&((w * h * 4) as u32).to_le_bytes());
        dib_data.extend_from_slice(&[0u8; 16]);
        dib_data.extend(std::iter::repeat_n(0x80u8, (w * h * 4) as usize));

        let entry = Entry::new(vec![Format {
            format_name: "CF_DIB".to_string(),
            format_id: 8,
            data: dib_data,
        }]);
        let meta = storage.save_entry(&entry).unwrap();

        let thumb_name = meta.formats[0].thumb.clone().expect("thumbnail expected");
        assert!(thumb_name.ends_with(".thumb.webp"));
        let thumb_path = dir.join("blobs").join(&thumb_name);
        assert!(thumb_path.exists());

        // remove_entry_blobsで本体・サムネイルとも消えること
        storage.remove_entry_blobs(&meta).unwrap();
        assert!(!thumb_path.exists());
        assert!(!dir.join("blobs").join(&meta.formats[0].blob).exists());

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn history_index_roundtrips_large_hash() {
        // i64::MAXを超えるハッシュ（FNV-1aで普通に出る）がTOML経由で往復できること
        let dir = std::env::temp_dir().join(format!("clclr-test-{}", Uuid::new_v4()));
        let storage = Storage::open(dir.clone()).unwrap();

        let meta = EntryMeta {
            id: Uuid::new_v4(),
            title: None,
            modified: 100.0,
            hash: 0xFFFF_FFFF_FFFF_FFFF,
            preview: None,
            formats: Vec::new(),
        };
        storage.save_history_index(std::slice::from_ref(&meta)).unwrap();
        let loaded = storage.load_history_index().unwrap();
        assert_eq!(loaded[0].hash, 0xFFFF_FFFF_FFFF_FFFF);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn load_blob_prefix_reads_only_the_requested_head() {
        let dir = std::env::temp_dir().join(format!("clclr-test-{}", Uuid::new_v4()));
        let storage = Storage::open(dir.clone()).unwrap();
        fs::write(dir.join("blobs").join("x.bin"), b"0123456789").unwrap();

        assert_eq!(storage.load_blob_prefix("x.bin", 4).unwrap(), b"0123");
        // 上限より短いファイルは全体を返す
        assert_eq!(storage.load_blob_prefix("x.bin", 100).unwrap(), b"0123456789");
        assert_eq!(storage.load_blob_prefix("x.bin", 0).unwrap(), b"");
        assert!(storage.load_blob_prefix("missing.bin", 4).is_err());

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn history_index_without_hash_and_preview_loads() {
        // hash/previewフィールドの無いhistory.tomlが読めること
        let without_fields = r#"
[[entries]]
id = "b047dc01-11f5-4ec2-8657-dccefbf48cfd"
modified = 100.5

[[entries.formats]]
format_name = "CF_UNICODETEXT"
format_id = 13
blob = "b047dc01-11f5-4ec2-8657-dccefbf48cfd_0.bin"
size = 4
"#;
        let index: HistoryIndex = toml::from_str(without_fields).unwrap();
        assert_eq!(index.entries.len(), 1);
        assert_eq!(index.entries[0].hash, 0);
        assert_eq!(index.entries[0].preview, None);
        assert_eq!(index.entries[0].formats.len(), 1);
    }

    fn temp_storage() -> (PathBuf, Storage) {
        let dir = std::env::temp_dir().join(format!("clclr-test-{}", Uuid::new_v4()));
        let storage = Storage::open(dir.clone()).unwrap();
        (dir, storage)
    }

    /// WebP の blob は見出しと長さを確かめてから読む: 上限を超えるものは全体を読まずに大きさだけ、寸法に見合わない
    /// 長さ・CLCLR が書く形でないもの・無いものは None。生のままの blob は、長さが保存したときの大きさと同じときだけ読む。
    #[test]
    fn webp_and_bin_blobs_are_checked_before_reading_whole() {
        let (dir, storage) = temp_storage();
        let blobs = dir.join("blobs");
        fs::create_dir_all(&blobs).unwrap();
        let rgb = vec![5u8; 64 * 32 * 3];
        let webp = dib::dib_to_webp(&dib::build_dib_32bpp(64, 32, &rgb, false)).unwrap();
        fs::write(blobs.join("a_0.webp"), &webp).unwrap();
        assert_eq!(storage.load_webp("a_0.webp", u64::MAX), Some(LoadedWebp::Data(webp.clone())));
        assert_eq!(storage.load_webp("a_0.webp", 64 * 32 * 4 - 1), Some(LoadedWebp::TooLarge { width: 64, height: 32 }));
        assert_eq!(storage.load_webp("missing.webp", u64::MAX), None);

        // 見出しの寸法に見合わない長さ（見出しの長さもそろえた、中身の長いファイル）は読まない
        let pad = dib::webp_file_len_limit(64, 32) as usize;
        let mut long = webp.clone();
        long.resize(webp.len() + pad + (pad & 1), 0);
        let (chunk, riff) = ((long.len() - 20) as u32, (long.len() - 8) as u32);
        long[4..8].copy_from_slice(&riff.to_le_bytes());
        long[16..20].copy_from_slice(&chunk.to_le_bytes());
        assert!(dib::webp_header_dimensions(&long, long.len() as u64).is_ok(), "前提: 見出しの長さはそろっている");
        fs::write(blobs.join("b_0.webp"), &long).unwrap();
        assert_eq!(storage.load_webp("b_0.webp", u64::MAX), None);

        // CLCLR が書く形でない WebP
        fs::write(blobs.join("c_0.webp"), b"RIFF\x04\x00\x00\x00WEBP").unwrap();
        assert_eq!(storage.load_webp("c_0.webp", u64::MAX), None);

        fs::write(blobs.join("d_0.bin"), [1u8, 2, 3]).unwrap();
        assert_eq!(storage.load_blob_exact("d_0.bin", 3).unwrap(), [1, 2, 3]);
        assert!(storage.load_blob_exact("d_0.bin", 2).is_err());
        assert!(storage.load_blob_exact("d_0.bin", 4).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn saved_index_files_record_current_schema_version() {
        let (dir, storage) = temp_storage();
        storage.save_history_index(&[]).unwrap();
        storage.save_pinned(&[]).unwrap();
        for name in ["history.toml", "pinned.toml"] {
            let text = fs::read_to_string(dir.join(name)).unwrap();
            assert!(
                text.lines().any(|l| l == format!("schema_version = {SCHEMA_VERSION}")),
                "{name} に schema_version がない:\n{text}"
            );
        }
        // 書いた版で読み戻せる
        assert!(storage.load_history_index().unwrap().is_empty());
        assert!(storage.load_pinned().unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn index_files_without_schema_version_load_as_version_zero() {
        // 版の記録がない形式のファイルはそのまま読め、次の保存で現行の版になる
        let (dir, storage) = temp_storage();
        fs::write(
            dir.join("history.toml"),
            "[[entries]]\nid = \"b047dc01-11f5-4ec2-8657-dccefbf48cfd\"\nmodified = 1.0\nformats = []\n",
        )
        .unwrap();
        fs::write(dir.join("pinned.toml"), "").unwrap();
        let entries = storage.load_history_index().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(storage.load_pinned().unwrap().is_empty());

        storage.save_history_index(&entries).unwrap();
        let text = fs::read_to_string(dir.join("history.toml")).unwrap();
        assert!(text.contains(&format!("schema_version = {SCHEMA_VERSION}")));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn index_files_with_newer_schema_version_are_rejected_not_overwritten() {
        // 新しい版のファイルを読み飛ばして次の保存で上書きするとデータを失う。
        // 読み込みをエラーにし、ファイルには触れない
        let (dir, storage) = temp_storage();
        let future = SCHEMA_VERSION + 1;
        let history = format!("schema_version = {future}\nentries = []\n");
        let pinned = format!("schema_version = {future}\nnodes = []\n");
        fs::write(dir.join("history.toml"), &history).unwrap();
        fs::write(dir.join("pinned.toml"), &pinned).unwrap();

        assert!(matches!(
            storage.load_history_index(),
            Err(StorageError::UnsupportedSchema { file: "history.toml", found }) if found == future
        ));
        assert!(matches!(
            storage.load_pinned(),
            Err(StorageError::UnsupportedSchema { file: "pinned.toml", found }) if found == future
        ));
        assert_eq!(fs::read_to_string(dir.join("history.toml")).unwrap(), history);
        assert_eq!(fs::read_to_string(dir.join("pinned.toml")).unwrap(), pinned);
        fs::remove_dir_all(dir).unwrap();
    }

    /// 画面に出る文言は日本語。
    #[test]
    fn storage_error_messages_are_japanese() {
        assert_eq!(
            StorageError::MissingFormat { format_name: "CF_UNICODETEXT".to_string() }.to_string(),
            "項目のデータ（CF_UNICODETEXT）が見つからないか、壊れています"
        );
        assert_eq!(
            StorageError::UnsupportedSchema { file: "history.toml", found: SCHEMA_VERSION + 1 }.to_string(),
            format!(
                "history.toml はこの版より新しい形式で保存されています（形式の版 {}、この版が読めるのは {} まで）。\
                 新しい版の CLCLR で開いてください",
                SCHEMA_VERSION + 1,
                SCHEMA_VERSION
            )
        );
        let io = StorageError::Io(io::Error::from(io::ErrorKind::PermissionDenied)).to_string();
        assert!(io.starts_with("ファイルを読み書きできません（"), "{io}");
    }

    /// 書式の誤りは、ファイル名と位置（行・文字。全角も1文字と数える）を日本語で出し、`toml` の説明を
    /// 続ける（該当行の抜き出しと `^` の印は出さない）。
    #[test]
    fn parse_error_names_file_and_position() {
        let (dir, storage) = temp_storage();
        // 誤りは `tru` の位置。その前の `  { "あいう" = ` は 12 文字（バイトで数えると 18）
        fs::write(dir.join("history.toml"), "schema_version = 1\nentries = [\n  { \"あいう\" = tru }\n]\n").unwrap();
        let Err(e) = storage.load_history_index() else { panic!("書式の誤りを読めてしまった") };
        let message = e.to_string();
        assert!(message.starts_with("history.toml の書式が正しくありません（3 行目、13 文字目:"), "{message}");
        assert!(!message.contains('^') && !message.contains('\n'), "{message}");
        fs::remove_dir_all(dir).unwrap();
    }

    /// 位置の境界: 行末が CRLF でも前の行の `\r` を数えない、入力の末尾の誤りも位置を出す、位置の
    /// 無い誤りは `toml` の説明だけ。
    #[test]
    fn toml_error_detail_handles_crlf_end_of_input_and_missing_span() {
        let detail = |text: &str| {
            let e = toml::from_str::<toml::Table>(text).unwrap_err();
            toml_error_detail(&e, text)
        };
        assert!(detail("a = 1\r\nb = tru\r\n").starts_with("2 行目、5 文字目: "), "{}", detail("a = 1\r\nb = tru\r\n"));
        assert!(detail("[general").starts_with("1 行目、9 文字目: "), "{}", detail("[general"));
        let no_span = <toml::de::Error as serde::de::Error>::custom("説明だけ");
        assert_eq!(toml_error_detail(&no_span, "a = 1"), "説明だけ");
    }

    #[test]
    fn saved_meta_has_hash_and_preview() {
        let dir = std::env::temp_dir().join(format!("clclr-test-{}", Uuid::new_v4()));
        let storage = Storage::open(dir.clone()).unwrap();

        // "あA" のUTF-16LE + NUL終端
        let mut utf16: Vec<u8> = Vec::new();
        for u in "あA".encode_utf16() {
            utf16.extend_from_slice(&u.to_le_bytes());
        }
        utf16.extend_from_slice(&[0, 0]);

        let entry = Entry::new(vec![Format {
            format_name: "CF_UNICODETEXT".to_string(),
            format_id: 13,
            data: utf16,
        }]);
        let expected_hash = entry.content_hash();
        let meta = storage.save_entry(&entry).unwrap();

        assert_eq!(meta.hash, expected_hash);
        assert_ne!(meta.hash, 0);
        assert_eq!(meta.preview.as_deref(), Some("あA"));

        // save=falseの形式しかない場合、previewはディスクに載らない
        let secret = Entry::new(vec![Format {
            format_name: "CF_UNICODETEXT".to_string(),
            format_id: 13,
            data: b"s\0e\0c\0\0\0".to_vec(),
        }]);
        let meta2 = storage.save_entry_filtered(&secret, |_| false).unwrap();
        assert_eq!(meta2.preview, None);
        assert!(meta2.formats.is_empty());

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn isolated_cycle_unreachable_from_root_is_rejected() {
        // ルートに繋がらないA⇄Bの孤立した循環。`visited`はbuild_children
        // の再帰経路上でしか働かず、この形の循環は一度も再帰に現れないため、それだけでは
        // CyclicTreeにもならず黙って消える
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let flat = vec![
            NodeMeta {
                id: a,
                parent_id: Some(b),
                order: 0,
                kind: "folder".to_string(),
                title: Some("A".to_string()),
                modified: None,
                formats: Vec::new(),
            },
            NodeMeta {
                id: b,
                parent_id: Some(a),
                order: 0,
                kind: "folder".to_string(),
                title: Some("B".to_string()),
                modified: None,
                formats: Vec::new(),
            },
        ];
        assert!(matches!(
            build_tree(&flat),
            Err(StorageError::PinnedTreeCorrupt)
        ));
    }

    #[test]
    fn orphan_node_with_missing_parent_is_rejected() {
        // 循環ではなく、存在しないparent_idを参照するだけの孤立ノード
        let missing_parent = Uuid::new_v4();
        let flat = vec![NodeMeta {
            id: Uuid::new_v4(),
            parent_id: Some(missing_parent),
            order: 0,
            kind: "item".to_string(),
            title: Some("orphan".to_string()),
            modified: Some(0.0),
            formats: Vec::new(),
        }];
        assert!(matches!(
            build_tree(&flat),
            Err(StorageError::PinnedTreeCorrupt)
        ));
    }

    #[test]
    fn timestamp_rejects_invalid_values_without_panic() {
        // 破損・手編集されたTOML由来のあらゆる異常値でpanicせずErrを返すこと
        for ts in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 1.0e20, f64::MAX] {
            assert!(matches!(
                timestamp_to_system_time(ts),
                Err(StorageError::InvalidTimestamp)
            ));
        }
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("clclr-storage-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// blob・サムネイルの名前はファイル名だけを受け付ける。`..`・区切り文字・ドライブを含む名前の
    /// history.toml・pinned.toml は、読むときに書式の誤りとして断る（blobs の外のファイルを読む・消すのを防ぐ）。
    #[test]
    fn blob_names_outside_blobs_are_rejected() {
        let id = Uuid::new_v4();
        for bad in ["../config.toml", "..\\config.toml", "C:x.bin", "..", "sub/x.bin", ""] {
            let dir = temp_dir();
            let storage = Storage::open(dir.clone()).unwrap();
            let history = format!(
                "[[entries]]\nid = \"{id}\"\nmodified = 0.0\n\n[[entries.formats]]\nformat_name = \"CF_TEXT\"\nformat_id = 1\nblob = {bad:?}\nsize = 1\n"
            );
            fs::write(dir.join("history.toml"), history).unwrap();
            assert!(
                matches!(storage.load_history_index(), Err(StorageError::Parse { file: "history.toml", .. })),
                "history.toml の {bad:?} を断っていない"
            );
            let pinned = format!(
                "[[nodes]]\nid = \"{id}\"\norder = 0\nkind = \"item\"\n\n[[nodes.formats]]\nformat_name = \"CF_DIB\"\nformat_id = 8\nblob = \"a.webp\"\nsize = 1\nthumb = {bad:?}\n"
            );
            fs::write(dir.join("pinned.toml"), pinned).unwrap();
            assert!(
                matches!(storage.load_pinned(), Err(StorageError::Parse { file: "pinned.toml", .. })),
                "pinned.toml の {bad:?} を断っていない"
            );
            fs::remove_dir_all(dir).unwrap();
        }
    }

    /// 知らない kind の項目は黙って捨てず、書式の誤りとして断る（捨てると次の保存でファイルからも消える）。
    #[test]
    fn unknown_pinned_kind_is_rejected() {
        let flat = vec![NodeMeta {
            id: Uuid::new_v4(),
            parent_id: None,
            order: 0,
            kind: "itme".to_string(),
            title: Some("誤記".to_string()),
            modified: Some(0.0),
            formats: Vec::new(),
        }];
        assert!(matches!(build_tree(&flat), Err(StorageError::Parse { file: "pinned.toml", .. })));
    }

    /// 厳密な読み込みは、生のまま保存した blob の大きさが保存したときと違えば（短くなった・書き換えられた）、
    /// 欠けたものとして断る。
    #[test]
    fn strict_load_rejects_raw_blob_with_different_size() {
        let dir = temp_dir();
        let storage = Storage::open(dir.clone()).unwrap();
        let entry = Entry::new(vec![Format { format_name: "CF_TEXT".into(), format_id: 1, data: b"abcdef".to_vec() }]);
        let meta = storage.save_entry(&entry).unwrap();
        assert_eq!(storage.load_entry_data_strict(&meta).unwrap().formats[0].data, b"abcdef");
        fs::write(dir.join("blobs").join(&meta.formats[0].blob), b"abc").unwrap();
        assert!(matches!(storage.load_entry_data_strict(&meta), Err(StorageError::MissingFormat { .. })));
        fs::remove_dir_all(dir).unwrap();
    }

    /// 元に戻す画像（WebP）が `MAX_RESTORE_RGBA` を超えるなら、全体を読まずに「大きすぎる」で断る（欠けたものとは
    /// 分ける）。上限までは戻す。
    #[test]
    fn strict_load_rejects_webp_over_restore_limit() {
        let dir = temp_dir();
        let storage = Storage::open(dir.clone()).unwrap();
        let rgb = vec![3u8; 8 * 4 * 3];
        let entry = Entry::new(vec![Format { format_name: "CF_DIB".into(), format_id: 8, data: dib::build_dib_32bpp(8, 4, &rgb, false) }]);
        let meta = storage.save_entry(&entry).unwrap();
        assert!(meta.formats[0].blob.ends_with(".webp"), "前提: WebP で保存した");
        assert_eq!(storage.load_entry_data_strict(&meta).unwrap().formats[0].data, entry.formats[0].data);

        // 見出しだけの VP8L（長さはそろえる）で、辺を上限を超える大きさにする
        let side = 11586u32;
        assert!(u64::from(side) * u64::from(side) * 4 > MAX_RESTORE_RGBA);
        let bits = (side - 1) | ((side - 1) << 14);
        let mut chunk = vec![0x2f];
        chunk.extend(bits.to_le_bytes());
        let mut webp = b"RIFF".to_vec();
        webp.extend((4 + 8 + chunk.len() as u32 + 1).to_le_bytes());
        webp.extend(b"WEBPVP8L");
        webp.extend((chunk.len() as u32).to_le_bytes());
        webp.extend(&chunk);
        webp.push(0);
        fs::write(dir.join("blobs").join(&meta.formats[0].blob), &webp).unwrap();
        match storage.load_entry_data_strict(&meta) {
            Err(e @ StorageError::ImageTooLarge { width, height }) => {
                assert_eq!((width, height), (side, side));
                assert!(e.to_string().contains("512 MiB"), "{e}");
            }
            Err(e) => panic!("{e:?}"),
            Ok(_) => panic!("上限を超える画像を戻した"),
        }
        fs::remove_dir_all(dir).unwrap();
    }

    /// 一覧用のテキストは、サロゲートペアの多い文字列でも `PREVIEW_MAX_CHARS` 文字まで作る（先頭だけを読む）。
    #[test]
    fn text_preview_reads_only_head_but_keeps_limit_with_surrogates() {
        let text = "😀".repeat(PREVIEW_MAX_CHARS + 50);
        let fmt = Format { format_name: "CF_UNICODETEXT".into(), format_id: 13, data: crate::data::utf16_bytes(&text) };
        let preview = text_preview(std::iter::once(&fmt)).unwrap();
        assert_eq!(preview, "😀".repeat(PREVIEW_MAX_CHARS));
        let mixed = format!("a{}", "😀".repeat(PREVIEW_MAX_CHARS));
        let fmt = Format { format_name: "CF_UNICODETEXT".into(), format_id: 13, data: crate::data::utf16_bytes(&mixed) };
        assert_eq!(text_preview(std::iter::once(&fmt)).unwrap(), mixed.chars().take(PREVIEW_MAX_CHARS).collect::<String>());
    }
}
