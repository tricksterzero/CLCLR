//! 履歴・ピン留めのメモリ上の状態（常駐サービス）。
//!
//! 履歴はメモリにメタデータ（+非永続形式）だけを持ち、blobの読み込みは
//! ペースト・プレビュー時まで遅延する。
//!
//! この型はメモリの状態と、その写しを取る操作だけを持つ。ファイルの読み書き・変換を伴う
//! 操作（取り込み・削除・ピン留め・保存）は `ops::Core` が、受け付け・操作用のロックの下で、
//! この型のロック（サービスのロック）の外で行う（サービスのロックの
//! 中ではメモリの更新と写しを取ることだけをする）。状態を変えるメソッドは `pub(crate)` で、
//! `&mut self` を取れるのは `Core` の中だけ（`Core::read` は `&HistoryService` しか渡さない）。

use std::sync::Arc;

use uuid::Uuid;

use crate::config::{Config, HistoryConfig};
use crate::data::{Entry, Format};
use crate::storage::{EntryMeta, Storage, StorageError};
use crate::store::{self, History, HistoryItem, PinnedNode};

pub struct HistoryService {
    pub history: History,
    /// ピン留めツリー（メタデータのみ。履歴と同じ遅延ロード方式）
    pub pinned: Vec<PinnedNode>,
    storage: Storage,
    /// 履歴・ピン留めを変える操作のたびに増える番号（`revision`）。`history`・`pinned` を
    /// 直接書き換えた場合は増えないので、変更はこの型のメソッドを通す
    revision: u64,
}

impl HistoryService {
    /// データディレクトリを開いて履歴とピン留めを読む。前回の起動から設定の保持件数が
    /// 縮んでいれば切り詰め、押し出した項目を返す（その後始末＝インデックスの更新と blob の
    /// 削除は、呼び出し元の `Core` が他の押し出しと同じ経路で行う）。
    pub(crate) fn open_at(data_dir: std::path::PathBuf, cfg: &Config) -> Result<(Self, Vec<HistoryItem>), StorageError> {
        let storage = Storage::open(data_dir)?;
        let items = storage
            .load_history_index()?
            .into_iter()
            .map(|meta| HistoryItem { meta, resident: Arc::default() })
            .collect();
        let mut history = History::from_items(items);
        let evicted = history.trim_to_max(&cfg.history);
        let pinned = storage.load_pinned()?;
        Ok((Self { history, pinned, storage, revision: 0 }, evicted))
    }

    /// 履歴・ピン留めの変更番号。表示側は前に見た値と比べて、変わっていなければ一覧を
    /// 作り直さない。値そのものに意味はなく、変更のたびに異なる値になることだけを約束する
    /// （変更がなくても増えることはある）。
    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn bump_revision(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// blobを読む`Storage`の複製を返す。サービスのロックを手放した後にI/Oや復号を
    /// 行う呼び出し向け。`Storage`は保存先パスだけを持つので複製は安い。
    pub fn storage_handle(&self) -> Storage {
        self.storage.clone()
    }

    // --- メモリの更新（`Core` が操作用のロックを持って、サービスのロックの中で呼ぶ） ---

    /// 履歴の先頭に足し、重複の置き換え・上限による押し出しで除いた項目を返す。
    pub(crate) fn add_item(&mut self, item: HistoryItem, config: &HistoryConfig) -> Vec<HistoryItem> {
        let evicted = self.history.add(item, config);
        self.bump_revision();
        evicted
    }

    pub(crate) fn remove_item(&mut self, id: Uuid) -> Option<HistoryItem> {
        let removed = self.history.remove_by_id(id);
        if removed.is_some() {
            self.bump_revision();
        }
        removed
    }

    pub(crate) fn clear_items(&mut self) -> Vec<HistoryItem> {
        let removed = self.history.clear();
        self.bump_revision();
        removed
    }

    pub(crate) fn trim_items(&mut self, config: &HistoryConfig) -> Vec<HistoryItem> {
        let evicted = self.history.trim_to_max(config);
        if !evicted.is_empty() {
            self.bump_revision();
        }
        evicted
    }

    /// ピン留めを差し替える（pinned.toml へ書けた後に呼ぶ。保存が成功してからメモリに反映する）。
    pub(crate) fn set_pinned(&mut self, nodes: Vec<PinnedNode>) {
        self.pinned = nodes;
        self.bump_revision();
    }

    // --- 写し（サービスのロックの中で呼ぶ。I/O はしない） ---

    /// 履歴のインデックスに書く全件のメタデータ。
    pub(crate) fn history_metas(&self) -> Vec<EntryMeta> {
        self.history.iter().map(|i| i.meta.clone()).collect()
    }

    /// 履歴の項目の読み込み元（メタデータと resident の `Arc`）。
    pub fn history_source(&self, id: Uuid) -> Option<EntrySource> {
        let item = self.history.get_by_id(id)?;
        Some(EntrySource {
            storage: self.storage.clone(),
            meta: item.meta.clone(),
            resident: Arc::clone(&item.resident),
        })
    }

    /// ピン留めの項目の読み込み元（ピン留めは resident を持たない）。
    pub fn pinned_source(&self, id: Uuid) -> Option<EntrySource> {
        let meta = store::find_item(&self.pinned, id)?;
        Some(EntrySource { storage: self.storage.clone(), meta: meta.clone(), resident: Arc::default() })
    }
}

/// 項目を読むための写し（サービスのロックの中で取り、読み込みはロックの外で行う）。
pub struct EntrySource {
    storage: Storage,
    pub meta: EntryMeta,
    pub resident: Arc<Vec<Format>>,
}

impl EntrySource {
    /// 送る・ピン留めのための厳密な読み込み。ディスクの blob が1つでも読めなければエラー。
    /// blob と resident を合わせた後に形式が1つもなければ `None`（空のクリップボードで上書き
    /// しないため。blob が無く resident だけの項目は通る）。
    pub fn load_strict(self) -> Result<Option<Entry>, StorageError> {
        let mut entry = self.storage.load_entry_data_strict(&self.meta)?;
        entry.formats.extend(self.resident.iter().cloned());
        Ok((!entry.formats.is_empty()).then_some(entry))
    }

    /// 画像（CF_DIB）をファイルへ書き出すために読む。WebP の blob で保存されていれば、その中身を
    /// 変換せずにそのまま返す（CLCLR が書く形の WebP で、長さが寸法に見合うことだけを確かめる。画素の復号まではしないので、
    /// 中身の壊れは開いたアプリが扱う）。それ以外（元の DIB のままの blob、メモリだけに
    /// 持つ画像）は `load_strict` と同じく読んで DIB を返す。画像が無ければ `None`。
    pub fn load_image(self) -> Result<Option<ImageData>, StorageError> {
        let webp = self
            .meta
            .formats
            .iter()
            .find(|f| f.format_name == "CF_DIB" && f.blob.ends_with(".webp"))
            .map(|f| f.blob.clone());
        if let Some(name) = webp {
            // 欠けている・壊れているときは、厳密な読み込みと同じエラーにする
            let missing = || StorageError::MissingFormat { format_name: "CF_DIB".to_string() };
            // CLCLR が書く形で、長さが寸法に見合うものだけ（`Storage::load_webp`。書き出すだけなので大きさの上限は無い）
            let data = self.storage.load_webp(&name, u64::MAX).and_then(|w| w.into_data()).ok_or_else(missing)?;
            return Ok(Some(ImageData::Webp(data)));
        }
        Ok(self
            .load_strict()?
            .and_then(|entry| entry.formats.into_iter().find(|f| f.format_name == "CF_DIB"))
            .map(|f| ImageData::Dib(f.data)))
    }
}

/// 書き出す画像のデータ（`EntrySource::load_image`）。
#[derive(Debug, PartialEq, Eq)]
pub enum ImageData {
    /// 保存してある WebP のファイルの中身（そのまま `.webp` に書ける）
    Webp(Vec<u8>),
    /// CF_DIB の中身（BMP にして書く）
    Dib(Vec<u8>),
}
