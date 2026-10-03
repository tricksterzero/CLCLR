//! 履歴・ピン留めの操作の入口（`Core`）。
//!
//! 3つの仕組みで、ファイルの読み書き・変換をサービスのロック（`Mutex<HistoryService>`）の外へ
//! 出す（ビューアのメインスレッドが取るサービスのロックは、メモリの
//! 更新と写しを取る一瞬だけにする）。
//!
//! - **受け付け**（`Admission`）: 操作は始める前に登録する（`Ticket`）。終了処理
//!   （`shutdown`）は受け付けを閉じ、登録済みの操作が終わるのを待ってから最後の保存をする。
//!   締め切りは「受け付けに登録できた時点」
//! - **操作用のロック**（`Mutex<OpState>`）: 登録済みの操作を1つずつ順に行う。変換・blob の
//!   読み書き・インデックスの書き込み・blob の削除は、このロックを持ったまま、サービスの
//!   ロックの外で行う。順に並ぶので、古い写しが新しい写しを上書きすることはない
//! - **サービスのロック**: メモリの更新と写し（メタデータ、resident の `Arc`）を取る一瞬だけ
//!
//! ロックの順序は、受け付け（一瞬、他を持たずに）→ 操作用のロック → サービスのロック。
//! サービスのロック・設定のガードを持ったまま `Core` の操作を呼ばない（`read` のクロージャの
//! 中から呼ぶと止まる）。操作用のロック・サービスのロックが poison したら（操作の途中の
//! パニック）、以後の変更と最後の保存は `Err` で断る（壊れているかもしれない状態を書かない）。

use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::config::Config;
use crate::data::Entry;
use crate::datacheck::{self, CleanResult, DataReport, FileEntry, MissingItem, TempFile, TempPlace};
use crate::service::{EntrySource, HistoryService, ImageData};
use crate::storage::{EntryMeta, Storage, StorageError};
use crate::store::{self, HistoryItem, PinnedNode};

#[derive(Debug)]
pub enum OpError {
    /// 受け付けが閉じている（終了処理が始まった）
    Closing,
    /// 操作用のロックかサービスのロックが poison している（前の操作の途中でパニックした）
    Poisoned,
    NotFound,
    /// 移す先・作る先・ピン留めの入れる先のフォルダが無い（対象そのものが無いときは `NotFound`）
    TargetNotFound,
    /// フォルダの名前が（前後の空白を除くと）空
    InvalidName,
    /// 同じ親の中に同じ名前のフォルダがある
    DuplicateName,
    /// 送る・ピン留めの対象に形式が1つもない
    Empty,
    /// 終了処理で、受け付け済みの操作が期限までに終わらなかった（最後の保存はしていない）
    TimedOut,
    /// 最後の保存で、インデックスから消せていない履歴の項目が残った
    Unsaved(usize),
    /// 別の `Core` の受け付けの `Ticket` を渡された（呼び出し側の誤り）
    ForeignTicket,
    /// 項目はメモリから除いたが、履歴のインデックス（history.toml）を書き直せなかった。あとの操作と最後の保存で
    /// 書き直す（それまでに異常終了すると、次の起動で除いた項目が戻る）。利用者に知らせるために返す
    IndexNotSaved(StorageError),
    /// データのチェックの削除で、ディスクの history.toml・pinned.toml に、メモリに無い項目があった（実行中に
    /// 外から書き戻されたなど）。何も消さない
    DataChanged,
    Storage(StorageError),
}

impl fmt::Display for OpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closing => write!(f, "受け付けが閉じています（終了中）"),
            Self::Poisoned => write!(f, "前の操作が途中で失敗したため、変更と保存を止めています"),
            Self::NotFound => write!(f, "項目が見つかりません"),
            Self::TargetNotFound => write!(f, "フォルダが見つかりません"),
            Self::InvalidName => write!(f, "フォルダの名前が空です"),
            Self::DuplicateName => write!(f, "同じ場所に同じ名前のフォルダがあります"),
            Self::Empty => write!(f, "項目に形式がありません"),
            Self::TimedOut => write!(f, "処理中の操作が終わらないため、保存しませんでした"),
            Self::Unsaved(n) => write!(f, "履歴のインデックスから {n} 件を消せていません"),
            Self::ForeignTicket => write!(f, "別の受け付けの登録が渡されました"),
            Self::IndexNotSaved(e) => {
                write!(f, "履歴のファイルへ書けませんでした（項目は消しました。あとで書き直します）: {e}")
            }
            Self::DataChanged => write!(
                f,
                "データフォルダのファイル（history.toml・pinned.toml）が実行中に変わっているため、何も消していません。\
                 CLCLR を再起動してからやり直してください"
            ),
            Self::Storage(e) => write!(f, "{e}"),
        }
    }
}

impl From<StorageError> for OpError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}

/// 履歴・ピン留めの操作の入口。複製して各スレッドで持つ。
#[derive(Clone)]
pub struct Core {
    inner: Arc<Inner>,
}

struct Inner {
    service: Mutex<HistoryService>,
    config: Arc<RwLock<Config>>,
    storage: Storage,
    admission: Mutex<Admission>,
    /// 履歴を保存するか（blob を書き、インデックスを書く）。起動したときの設定で決め、実行中に設定が
    /// 変わっても変えない（完全メモリモードとの切り替えは次の起動から。メモリだけの項目のメタデータは
    /// ファイルの一覧が空なので、実行中に保存へ切り替えると中身の無い項目が書かれうる）。
    /// 保存するまま `save_on_exit` と `save_on_change` を入れ替えるのは、今の設定を読むので
    /// すぐ効く
    persist: bool,
    /// `Admission::inflight` が 0 になったときに通知する
    drained: Condvar,
    ops: Mutex<OpState>,
    /// テストだけで差し込む仕掛け（操作の途中で止める・パニックさせる）
    #[cfg(test)]
    hook: Mutex<Option<Arc<dyn Fn(&'static str) + Send + Sync>>>,
}

struct Admission {
    closing: bool,
    inflight: usize,
}

#[derive(Default)]
struct OpState {
    /// 履歴から除いたが、ディスクのインデックスから消せていない項目（書き込みの失敗）。
    /// 次の操作の始めと最後の保存で、インデックスから消し直し、消せたら blob を消す
    pending_removals: Vec<EntryMeta>,
}

/// 受け付けへの登録。破棄すると登録が外れる（早期の return・エラー・パニックでも）。
/// 操作用のロック・サービスのロックを手放した後に破棄する。
pub struct Ticket {
    inner: Arc<Inner>,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut admission = self.inner.admission.lock().unwrap_or_else(|p| p.into_inner());
        admission.inflight -= 1;
        if admission.inflight == 0 {
            self.inner.drained.notify_all();
        }
    }
}

/// 受け付け済みで、操作用のロックを持っている間。`ops` を先に手放してから `Ticket` を破棄する
/// （フィールドは宣言の順に破棄される。この順序が「操作用のロックを手放してから受け付けを外す」
/// を保証しているので、フィールドを並べ替えない）。
struct Scope<'a> {
    ops: MutexGuard<'a, OpState>,
    _ticket: Ticket,
}

/// 除いた項目の後始末のしかた。
enum Retire {
    /// 永続化が有効: この全件の写しでインデックスを書き直す
    Full(Vec<EntryMeta>),
    /// 完全メモリモード: ディスクのインデックスから、除いた項目の ID だけを消す
    /// （一致する ID が無ければ書かないので、新しいメタデータを書くことにはならない）
    Targeted,
}

impl Core {
    pub fn open(config: Arc<RwLock<Config>>) -> Result<Self, StorageError> {
        Self::open_at(Storage::default_data_dir(), config)
    }

    /// データディレクトリを指定して開く。起動時の切り詰めで押し出した項目も、他の押し出しと
    /// 同じ後始末に通す（失敗したら、あとで消し直す対象として残す）。
    pub fn open_at(data_dir: std::path::PathBuf, config: Arc<RwLock<Config>>) -> Result<Self, StorageError> {
        let cfg = config.read().unwrap_or_else(|p| p.into_inner()).clone();
        let (service, evicted) = HistoryService::open_at(data_dir, &cfg)?;
        let storage = service.storage_handle();
        let core = Core {
            inner: Arc::new(Inner {
                service: Mutex::new(service),
                config,
                storage,
                admission: Mutex::new(Admission { closing: false, inflight: 0 }),
                persist: persist_enabled(&cfg),
                drained: Condvar::new(),
                ops: Mutex::new(OpState::default()),
                #[cfg(test)]
                hook: Mutex::new(None),
            }),
        };
        if !evicted.is_empty() {
            // まだ他のスレッドはないので、ロックが poison していることはない
            let mut ops = core.inner.ops.lock().expect("起動時は他のスレッドがない");
            let retire = if core.inner.persist {
                Retire::Full(core.inner.service.lock().expect("起動時は他のスレッドがない").history_metas())
            } else {
                Retire::Targeted
            };
            // 書けなければ消し直す対象に残る（知らせる先はまだ無い。ログは `retire_items` が書く）
            let _ = retire_items(&core.inner.storage, &mut ops, evicted, retire);
        }
        Ok(core)
    }

    /// サービスのロックを短く取って、メモリの状態を読む（一覧・検索・プレビュー・メニュー）。
    /// ロックが poison していたら None。クロージャの中では I/O をせず、`Core` の操作も呼ばない。
    pub fn read<R>(&self, f: impl FnOnce(&HistoryService) -> R) -> Option<R> {
        self.inner.service.lock().ok().map(|s| f(&s))
    }

    /// blob を読む `Storage`（保存先のパスだけを持つ）。
    pub fn storage(&self) -> Storage {
        self.inner.storage.clone()
    }

    /// 受け付けに登録する（閉じていたら `Closing`）。起動時の同期のように、`Core` の操作の外で
    /// 副作用のある処理（クリップボードへの書き込み）を締め切りの内側に入れるときに使う。
    pub fn admit(&self) -> Result<Ticket, OpError> {
        let mut admission = self.inner.admission.lock().unwrap_or_else(|p| p.into_inner());
        if admission.closing {
            return Err(OpError::Closing);
        }
        admission.inflight += 1;
        Ok(Ticket { inner: Arc::clone(&self.inner) })
    }

    fn begin(&self) -> Result<Scope<'_>, OpError> {
        let ticket = self.admit()?;
        let mut ops = self.inner.ops.lock().map_err(|_| OpError::Poisoned)?;
        retry_pending(&self.inner.storage, &mut ops);
        Ok(Scope { ops, _ticket: ticket })
    }

    /// 設定の写し（操作用のロックを取った後に取る。1つの操作の中では同じ写しを使う）。
    fn cfg(&self) -> Config {
        self.inner.config.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn with_service<R>(&self, f: impl FnOnce(&mut HistoryService) -> R) -> Result<R, OpError> {
        let mut service = self.inner.service.lock().map_err(|_| OpError::Poisoned)?;
        Ok(f(&mut service))
    }

    /// 除いた項目の後始末のしかた（永続化が有効なら全件の写しをサービスのロックの中で取る）。
    fn retire_kind(&self) -> Result<Retire, OpError> {
        if self.inner.persist {
            Ok(Retire::Full(self.with_service(|s| s.history_metas())?))
        } else {
            Ok(Retire::Targeted)
        }
    }

    #[cfg(test)]
    fn hook(&self, point: &'static str) {
        let hook = self.inner.hook.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(hook) = hook {
            hook(point);
        }
    }

    #[cfg(not(test))]
    fn hook(&self, _point: &'static str) {}

    /// テスト用: 操作の途中（`hook` を呼ぶ所）で呼ばれる仕掛けを差し込む。
    #[cfg(test)]
    fn set_hook(&self, f: impl Fn(&'static str) + Send + Sync + 'static) {
        *self.inner.hook.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(f));
    }

    /// テスト用: 受け付け済みの操作の数。
    #[cfg(test)]
    fn inflight(&self) -> usize {
        self.inner.admission.lock().unwrap_or_else(|p| p.into_inner()).inflight
    }

    // --- 書く操作 ---

    /// クリップボードから取り込んだ項目を履歴に足す（監視スレッドから呼ぶ）。
    pub fn capture(&self, entry: Entry) -> Result<(), OpError> {
        let mut scope = self.begin()?;
        let cfg = self.cfg();
        self.hook("capture:prepare");
        // 変換・blob の書き込みはサービスのロックの外
        let persist = self.inner.persist;
        let item = prepare_capture(&self.inner.storage, &cfg, persist, entry)?;
        let (evicted, snapshot) = self.with_service(|s| {
            let evicted = s.add_item(item, &cfg.history);
            // 押し出しがあれば後始末のために、save_on_change なら毎回、全件の写しを取る。
            // save_on_exit だけで押し出しが無ければ、インデックスは終了時まで書かない
            let snapshot = (persist && (!evicted.is_empty() || cfg.history.save_on_change)).then(|| s.history_metas());
            (evicted, snapshot)
        })?;
        if cfg.history.sound_on_add {
            play_add_sound(&cfg.history.sound_file);
        }
        if !evicted.is_empty() {
            let retire = snapshot.map_or(Retire::Targeted, Retire::Full);
            // 監視からの取り込みには知らせる先が無い（ログは `retire_items` が書く。消し直す対象に残る）
            let _ = retire_items(&self.inner.storage, &mut scope.ops, evicted, retire);
        } else if let Some(metas) = snapshot {
            if let Err(e) = self.inner.storage.save_history_index(&metas) {
                eprintln!("履歴インデックスの保存に失敗: {e}");
            }
        }
        Ok(())
    }

    /// 履歴の項目を消す（ID で照合する。一覧を写してから操作するまでの間に監視が項目を
    /// 足しても、別の項目を消さない）。
    /// インデックスを書けなかったときは `IndexNotSaved`（項目はメモリから除いてある）。
    pub fn delete_history(&self, id: Uuid) -> Result<(), OpError> {
        let mut scope = self.begin()?;
        let item = self.with_service(|s| s.remove_item(id))?.ok_or(OpError::NotFound)?;
        let retire = self.retire_kind()?;
        retire_items(&self.inner.storage, &mut scope.ops, vec![item], retire).map_err(OpError::IndexNotSaved)
    }

    /// 履歴を全部消す（C版 CLCL の tool_utl「履歴のクリア」の統合）。インデックスを書けなかったときは `IndexNotSaved`。
    pub fn clear_history(&self) -> Result<(), OpError> {
        let mut scope = self.begin()?;
        let removed = self.with_service(|s| s.clear_items())?;
        let retire = self.retire_kind()?;
        retire_items(&self.inner.storage, &mut scope.ops, removed, retire).map_err(OpError::IndexNotSaved)
    }

    /// 設定の保持件数へ切り詰める（設定の反映から、操作スレッドで呼ぶ）。インデックスを書けなかった
    /// ときは `IndexNotSaved`。
    pub fn trim(&self) -> Result<(), OpError> {
        let mut scope = self.begin()?;
        let cfg = self.cfg();
        let evicted = self.with_service(|s| s.trim_items(&cfg.history))?;
        if !evicted.is_empty() {
            let retire = self.retire_kind()?;
            retire_items(&self.inner.storage, &mut scope.ops, evicted, retire).map_err(OpError::IndexNotSaved)?;
        }
        Ok(())
    }

    /// 履歴の項目をピン留めに複製し、`to`（None はルート）のフォルダの末尾に入れる。元の項目を
    /// 参照せず、新しい ID・新しい blob の独立した項目として保存する（履歴の押し出し・削除が
    /// ピン留めの blob を消さないため）。入れる先は blob を書く前に確かめる（操作用のロックで順に
    /// 並ぶので、確かめてから足すまでに消えない）。pinned.toml へ書けてからメモリに反映する
    /// （失敗したらメモリは変えず、書いた blob を消す）。
    pub fn pin(&self, id: Uuid, to: Option<Uuid>) -> Result<(), OpError> {
        let _scope = self.begin()?;
        let (source, target_exists) = self.with_service(|s| {
            let target_exists = to.is_none_or(|folder| store::find_folder(&s.pinned, folder).is_some());
            (s.history_source(id), target_exists)
        })?;
        let source = source.ok_or(OpError::NotFound)?;
        if !target_exists {
            return Err(OpError::TargetNotFound);
        }
        let entry = source.load_strict()?.ok_or(OpError::Empty)?;
        let copy = Entry::new(entry.formats);
        // ピン留めは明示的な保存の意思なので、save=false の形式も含めて全形式を書く
        let mut meta = self.inner.storage.save_entry(&copy)?;
        // 一覧に出すタイトル。preview は pinned.toml に残らないので title に写す
        if meta.title.is_none() {
            meta.title = meta.preview.as_deref().and_then(auto_title);
        }
        let mut nodes = self.with_service(|s| s.pinned.clone())?;
        // 入れる先は上で確かめた（操作用のロックの中なので、写しにもある）
        let Some(children) = store::children_mut(&mut nodes, to) else {
            remove_blobs(&self.inner.storage, &meta);
            return Err(OpError::TargetNotFound);
        };
        children.push(PinnedNode::Item(meta.clone()));
        if let Err(e) = self.inner.storage.save_pinned(&nodes) {
            remove_blobs(&self.inner.storage, &meta);
            return Err(e.into());
        }
        // 操作用のロックで順に並ぶので、写しを取ってからここまでの間にピン留めは変わらない
        self.with_service(|s| s.set_pinned(nodes))?;
        Ok(())
    }

    /// ピン留めの項目（またはフォルダ）を消す。pinned.toml へ書けてからメモリに反映し、配下の
    /// blob を消す（失敗したらメモリも blob も変えない）。
    pub fn delete_pinned(&self, id: Uuid) -> Result<(), OpError> {
        let _scope = self.begin()?;
        let mut nodes = self.with_service(|s| s.pinned.clone())?;
        let node = store::remove_node(&mut nodes, id).ok_or(OpError::NotFound)?;
        self.inner.storage.save_pinned(&nodes)?;
        self.with_service(|s| s.set_pinned(nodes))?;
        let mut metas = Vec::new();
        store::collect_item_metas(&node, &mut metas);
        for meta in &metas {
            remove_blobs(&self.inner.storage, meta);
        }
        Ok(())
    }

    /// `parent`（None はルート）の子の末尾に、`title` の名前のフォルダを作る。名前は
    /// 前後の空白を除いて保存する。pinned.toml へ書けてからメモリに反映する。
    pub fn create_folder(&self, parent: Option<Uuid>, title: &str) -> Result<(), OpError> {
        let _scope = self.begin()?;
        let name = store::normalize_folder_name(title);
        if name.is_empty() {
            return Err(OpError::InvalidName);
        }
        let mut nodes = self.with_service(|s| s.pinned.clone())?;
        let children = store::children_mut(&mut nodes, parent).ok_or(OpError::TargetNotFound)?;
        if store::has_sibling_folder_named(children, name, None) {
            return Err(OpError::DuplicateName);
        }
        children.push(PinnedNode::Folder(store::PinnedFolder {
            id: Uuid::new_v4(),
            title: name.to_string(),
            children: Vec::new(),
        }));
        self.inner.storage.save_pinned(&nodes)?;
        self.with_service(|s| s.set_pinned(nodes))?;
        Ok(())
    }

    /// フォルダの名前を変える（判定の順）: フォルダでなければ `NotFound`、
    /// 空なら `InvalidName`、今の名前と（正規化して）同じなら何もしない（既にある同名の兄弟が
    /// あってもエラーにしない）、自分以外の兄弟に同じ名前があれば `DuplicateName`。
    pub fn rename_folder(&self, id: Uuid, title: &str) -> Result<(), OpError> {
        let _scope = self.begin()?;
        let mut nodes = self.with_service(|s| s.pinned.clone())?;
        let current = store::find_folder(&nodes, id).ok_or(OpError::NotFound)?.title.clone();
        let name = store::normalize_folder_name(title);
        if name.is_empty() {
            return Err(OpError::InvalidName);
        }
        if name == store::normalize_folder_name(&current) {
            return Ok(());
        }
        let parent = store::parent_of(&nodes, id).ok_or(OpError::NotFound)?;
        let siblings = store::children_mut(&mut nodes, parent).ok_or(OpError::NotFound)?;
        if store::has_sibling_folder_named(siblings, name, Some(id)) {
            return Err(OpError::DuplicateName);
        }
        for node in siblings.iter_mut() {
            if let PinnedNode::Folder(folder) = node {
                if folder.id == id {
                    folder.title = name.to_string();
                }
            }
        }
        self.inner.storage.save_pinned(&nodes)?;
        self.with_service(|s| s.set_pinned(nodes))?;
        Ok(())
    }

    /// ピン留めのアイテムの名前（`title`）を変える。アイテムでなければ `NotFound`。名前は前後の
    /// 空白を除き、空なら自動の名前（ピン留めしたときと同じく、テキストの1行目。テキストが無い・blob を
    /// 読めなければ名前なし）に戻す。今と同じなら保存しない。pinned.toml へ書けてからメモリに反映する。
    pub fn rename_pinned_item(&self, id: Uuid, name: &str) -> Result<(), OpError> {
        let _scope = self.begin()?;
        let mut nodes = self.with_service(|s| s.pinned.clone())?;
        let meta = store::find_item_mut(&mut nodes, id).ok_or(OpError::NotFound)?;
        let name = name.trim();
        let title = if name.is_empty() {
            // blob の読み込みは操作用のロックの中・サービスのロックの外
            self.inner.storage.load_text_preview(meta).as_deref().and_then(auto_title)
        } else {
            Some(name.to_string())
        };
        if meta.title == title {
            return Ok(());
        }
        meta.title = title;
        self.inner.storage.save_pinned(&nodes)?;
        self.with_service(|s| s.set_pinned(nodes))?;
        Ok(())
    }

    /// ピン留めの項目を `to`（None はルート）のフォルダの末尾へ移す。移すのは項目だけ
    /// （フォルダの移動は範囲外。フォルダの ID なら `NotFound`）。対象が無ければ `NotFound`（入れる
    /// 先も無いときもこちらを優先）、入れる先が無ければ `TargetNotFound`、今いる所と同じなら何も
    /// しない。blob は触らない。
    pub fn move_pinned(&self, id: Uuid, to: Option<Uuid>) -> Result<(), OpError> {
        let _scope = self.begin()?;
        let mut nodes = self.with_service(|s| s.pinned.clone())?;
        if store::find_item(&nodes, id).is_none() {
            return Err(OpError::NotFound);
        }
        if to.is_some_and(|folder| store::find_folder(&nodes, folder).is_none()) {
            return Err(OpError::TargetNotFound);
        }
        if store::parent_of(&nodes, id) == Some(to) {
            return Ok(());
        }
        let node = store::remove_node(&mut nodes, id).ok_or(OpError::NotFound)?;
        store::children_mut(&mut nodes, to).ok_or(OpError::TargetNotFound)?.push(node);
        self.inner.storage.save_pinned(&nodes)?;
        self.with_service(|s| s.set_pinned(nodes))?;
        Ok(())
    }

    /// ピン留めの項目（またはフォルダ）を、同じ親の中で隣と入れ替える（`store::shift_node`。隣が項目か
    /// フォルダかは問わない）。対象が無ければ `NotFound`、先頭・末尾で動かせなければ何もしない。blob は
    /// 触らない。pinned.toml へ書けてからメモリに反映する。
    pub fn reorder_pinned(&self, id: Uuid, direction: store::Direction) -> Result<(), OpError> {
        let _scope = self.begin()?;
        let mut nodes = self.with_service(|s| s.pinned.clone())?;
        if !store::shift_node(&mut nodes, id, direction).ok_or(OpError::NotFound)? {
            return Ok(());
        }
        self.inner.storage.save_pinned(&nodes)?;
        self.with_service(|s| s.set_pinned(nodes))?;
        Ok(())
    }

    // --- 読む操作 ---

    /// 送るために項目を読む（厳密な読み込み）。読んでいる間は操作用のロックを持つので、押し出し・
    /// 削除でその blob が消されない。blob が1つでも読めない、または形式が1つもなければ `Err`。
    pub fn load_for_send(&self, id: Uuid, pinned: bool) -> Result<Entry, OpError> {
        let ticket = self.admit()?;
        self.load_for_send_admitted(&ticket, id, pinned)
    }

    /// `load_for_send` を、呼び出し側が持つ受け付けの登録（`ticket`）の下で行う。読み込みの後の
    /// 書き込みまでを1つの登録で覆うときに使う（ビューアの操作）。受け付けは取り直さない
    /// ので、登録の後に終了処理が受け付けを閉じても読み込みは続く。`ticket` はこの `Core`（の複製）の
    /// ものでなければ `ForeignTicket`。
    pub fn load_for_send_admitted(&self, ticket: &Ticket, id: Uuid, pinned: bool) -> Result<Entry, OpError> {
        self.read_source_admitted(ticket, id, pinned, |source| source.load_strict()?.ok_or(OpError::Empty))
    }

    /// 画像をファイルへ書き出すために読む（`EntrySource::load_image`）。受け付けの登録と読んでいる
    /// 間の扱いは `load_for_send_admitted` と同じ。画像が無ければ `Ok(None)`。
    pub fn load_image_admitted(&self, ticket: &Ticket, id: Uuid, pinned: bool) -> Result<Option<ImageData>, OpError> {
        self.read_source_admitted(ticket, id, pinned, |source| Ok(source.load_image()?))
    }

    /// 登録（`ticket`）の下で、操作用のロックを持って項目の読み込み元を取り、`read` で読む。
    fn read_source_admitted<T>(
        &self,
        ticket: &Ticket,
        id: Uuid,
        pinned: bool,
        read: impl FnOnce(EntrySource) -> Result<T, OpError>,
    ) -> Result<T, OpError> {
        if !Arc::ptr_eq(&self.inner, &ticket.inner) {
            return Err(OpError::ForeignTicket);
        }
        let mut ops = self.inner.ops.lock().map_err(|_| OpError::Poisoned)?;
        retry_pending(&self.inner.storage, &mut ops);
        let source = self
            .with_service(|s| if pinned { s.pinned_source(id) } else { s.history_source(id) })?
            .ok_or(OpError::NotFound)?;
        read(source)
    }

    // --- データのチェック ---

    /// 受け付けが閉じた（終了の要求）か。走査・削除の途中で確かめ、閉じていれば止める（セッション終了の
    /// 待ちを長引かせない。1回の読み書きが長引けば間に合わないことはある）。
    fn closing(&self) -> bool {
        self.inner.admission.lock().unwrap_or_else(|p| p.into_inner()).closing
    }

    fn check_closing(&self) -> Result<(), OpError> {
        if self.closing() { Err(OpError::Closing) } else { Ok(()) }
    }

    /// blobs と history.toml・pinned.toml の食い違いを調べる（何も消さない）。操作用のロックの中で行うので、
    /// 取り込み・削除・ピン留めと並ばない（書きかけの blob・一時ファイルを数えない）。
    pub fn check_data(&self) -> Result<DataReport, OpError> {
        let scope = self.begin()?;
        Ok(self.scan_data(&scope.ops)?.report)
    }

    /// `plan`（前に `check_data` が返したもの）のうち、今もう一度調べても同じく見つかるものだけを消す。
    /// ディスクのインデックスにメモリに無い項目があれば、何も消さずに `DataChanged`。(b) の項目は1件ずつ履歴・
    /// ピン留めから消し（インデックス・pinned.toml を書き直してから）、全部の結果が決まった後に、その blob のうち、
    /// 残った項目・消し直し待ち・ディスクのインデックスのどれからも参照されないものだけを消す。
    pub fn clean_data(&self, plan: &DataReport) -> Result<CleanResult, OpError> {
        let mut scope = self.begin()?;
        let scan = self.scan_data(&scope.ops)?;
        if !scan.consistent {
            return Err(OpError::DataChanged);
        }
        let storage = &self.inner.storage;
        let mut result = CleanResult::default();

        // (a) 参照されないファイル
        let planned: HashSet<String> = plan.orphans.iter().map(|f| datacheck::name_key(&f.name)).collect();
        for file in scan.report.orphans.iter().filter(|f| planned.contains(&datacheck::name_key(&f.name))) {
            if self.closing() {
                result.interrupted = true;
                return Ok(result);
            }
            match storage.remove_blob(&file.name) {
                Ok(()) => {
                    result.files_removed += 1;
                    // 合計は上限で止める（極端な大きさのファイルでも debug でパニックして操作用のロックを
                    // poison させない）
                    result.bytes_removed = result.bytes_removed.saturating_add(file.size);
                }
                Err(e) => result.failures.push(format!("blobs\\{}: {e}", file.name)),
            }
        }
        // (c) 一時ファイル
        let planned: HashSet<(TempPlace, String)> =
            plan.temps.iter().map(|t| (t.place, datacheck::name_key(&t.name))).collect();
        for temp in scan.report.temps.iter().filter(|t| planned.contains(&(t.place, datacheck::name_key(&t.name)))) {
            if self.closing() {
                result.interrupted = true;
                return Ok(result);
            }
            let (removed, shown) = match temp.place {
                TempPlace::Blobs => (storage.remove_blob(&temp.name), format!("blobs\\{}", temp.name)),
                TempPlace::DataDir => (storage.remove_data_file(&temp.name), temp.name.clone()),
            };
            match removed {
                Ok(()) => {
                    result.files_removed += 1;
                    result.bytes_removed = result.bytes_removed.saturating_add(temp.size);
                }
                Err(e) => result.failures.push(format!("{shown}: {e}")),
            }
        }

        // (b) データが欠けた項目。1件ずつ、メモリから除いてインデックス・pinned.toml を書き直す（1件の途中では
        // 止めず、件と件の間で終了の要求を確かめる）
        let planned: HashSet<(Uuid, bool)> = plan.missing.iter().map(|m| (m.id, m.pinned)).collect();
        let targets: Vec<&MissingItem> = scan.report.missing.iter().filter(|m| planned.contains(&(m.id, m.pinned))).collect();
        // インデックス・pinned.toml から消せた項目（blob を消す候補）
        let mut removed_history: Vec<EntryMeta> = Vec::new();
        let mut removed_pinned: Vec<EntryMeta> = Vec::new();
        for target in targets {
            self.hook("clean:item");
            if self.closing() {
                result.interrupted = true;
                break;
            }
            if target.pinned {
                let mut nodes = self.with_service(|s| s.pinned.clone())?;
                let Some(node) = store::remove_node(&mut nodes, target.id) else {
                    continue;
                };
                match storage.save_pinned(&nodes) {
                    Ok(()) => {
                        self.with_service(|s| s.set_pinned(nodes))?;
                        result.items_removed += 1;
                        store::collect_item_metas(&node, &mut removed_pinned);
                    }
                    Err(e) => result
                        .failures
                        .push(format!("pinned.toml を書けないため、ピン留めの「{}」を消していません: {e}", target.label)),
                }
            } else {
                let Some(item) = self.with_service(|s| s.remove_item(target.id))? else {
                    continue;
                };
                let saved = if self.inner.persist {
                    let all = self.with_service(|s| s.history_metas())?;
                    storage.save_history_index(&all)
                } else {
                    storage.remove_from_history_index(&[item.meta.id])
                };
                result.items_removed += 1;
                match saved {
                    Ok(()) => removed_history.push(item.meta),
                    Err(e) => {
                        // `retire_items` と同じく、blob は消さずに消し直し待ちに残す
                        result.index_error = Some(e.to_string());
                        scope.ops.pending_removals.push(item.meta);
                    }
                }
            }
        }
        // blob は、全部の書き直しの結果が決まってから消す。実際に消せた項目以外（残った項目・保存に失敗して残った
        // 項目・今の消し直し待ち・ディスクのインデックス）から参照されるものは消さない。
        // メモリの分は、ID で除くのではなく消した後の今の状態から取る（同じ ID の別の項目の参照も残す）。
        // ディスクの分は、走査のときの写しから消した項目の ID を除く（消した項目はもう書き直した）
        let removed_history_ids: HashSet<Uuid> = removed_history.iter().map(|m| m.id).collect();
        let removed_pinned_ids: HashSet<Uuid> = removed_pinned.iter().map(|m| m.id).collect();
        let (history_now, pinned_now) = self.with_service(|s| (s.history_metas(), item_metas(&s.pinned)))?;
        let disk_history = scan.disk_history.iter().filter(|m| !removed_history_ids.contains(&m.id));
        let disk_pinned = scan.disk_pinned.iter().filter(|m| !removed_pinned_ids.contains(&m.id));
        let protected = reference_keys(
            history_now.iter().chain(&pinned_now).chain(disk_history).chain(disk_pinned).chain(&scope.ops.pending_removals),
        );
        removed_history.extend(removed_pinned);
        if !self.remove_unprotected_blobs(&removed_history, &protected, &mut result) {
            result.interrupted = true;
        }
        Ok(result)
    }

    /// 消した項目の blob・サムネイルのうち、`protected` に無いものを消す（もう無いものは何もしない）。終了の要求が
    /// 来たら途中でやめて false（残った blob は参照されないので、次のチェックで見つかる）。
    fn remove_unprotected_blobs(&self, metas: &[EntryMeta], protected: &HashSet<String>, result: &mut CleanResult) -> bool {
        for meta in metas {
            for name in meta.formats.iter().flat_map(|f| std::iter::once(&f.blob).chain(f.thumb.as_ref())) {
                self.hook("clean:blob");
                if self.closing() {
                    return false;
                }
                if protected.contains(&datacheck::name_key(name)) {
                    continue;
                }
                if let Err(e) = self.inner.storage.remove_blob(name) {
                    result.failures.push(format!("blobs\\{name}: {e}"));
                }
            }
        }
        true
    }

    /// 走査（`check_data`・`clean_data` が操作用のロックの中で呼ぶ）。サービスのロックでは写しを取るだけで、
    /// ファイルの読み書きはその外。
    fn scan_data(&self, ops: &OpState) -> Result<DataScan, OpError> {
        self.check_closing()?;
        self.hook("check:scan");
        let (history, pinned) = self.with_service(|s| (s.history_metas(), s.pinned.clone()))?;
        let storage = &self.inner.storage;
        // ディスクのインデックスも参照に入れる（外から編集・復元された場合の保険。読めなければ何もしない）
        let disk_history = storage.load_history_index()?;
        let disk_pinned = storage.load_pinned()?;
        self.check_closing()?;
        let pinned = item_metas(&pinned);
        let disk_pinned = item_metas(&disk_pinned);
        let scan_refs = DataScan {
            report: DataReport::default(),
            consistent: false,
            history,
            pinned,
            pending: ops.pending_removals.clone(),
            disk_history,
            disk_pinned,
        };
        let referenced = scan_refs.references();
        let mut scan = scan_refs;

        let mut report = DataReport::default();
        for file in storage.blob_files()? {
            self.hook("check:file");
            self.check_closing()?;
            let Some((name, size)) = file? else {
                continue;
            };
            if datacheck::is_blob_temp_name(&name) {
                report.temps.push(TempFile { place: TempPlace::Blobs, name, size });
            } else if datacheck::is_blob_name(&name) {
                if !referenced.contains(&datacheck::name_key(&name)) {
                    report.orphans.push(FileEntry { name, size });
                }
            } else {
                report.ignored.push(format!("blobs\\{name}"));
            }
        }
        // データフォルダの一時ファイル（config.toml.tmp は設定の反映が操作用のロックの外で書くので、対象外）
        for name in ["history.toml.tmp", "pinned.toml.tmp"] {
            if let Some(size) = storage.data_file_size(name)? {
                report.temps.push(TempFile { place: TempPlace::DataDir, name: name.to_string(), size });
            }
        }
        if storage.data_file_size("config.toml.tmp")?.is_some() {
            report.ignored.push("config.toml.tmp".to_string());
        }
        // (b) 参照している blob（サムネイルは除く）が無い項目。ファイルがあるかは実際に調べる。同じ ID の項目が
        // 2つ以上ある（手で編集した）ときは、ID で消すと別の項目を消しうるので、その ID は対象にしない
        let (duplicate_history, duplicate_pinned) = (duplicate_ids(&scan.history), duplicate_ids(&scan.pinned));
        let items = scan
            .history
            .iter()
            .filter(|m| !duplicate_history.contains(&m.id))
            .map(|m| (m, false))
            .chain(scan.pinned.iter().filter(|m| !duplicate_pinned.contains(&m.id)).map(|m| (m, true)));
        for (meta, pinned) in items {
            self.check_closing()?;
            let mut missing = false;
            for format in &meta.formats {
                if !storage.blob_exists(&format.blob)? {
                    missing = true;
                    break;
                }
            }
            if missing {
                report.missing.push(MissingItem { id: meta.id, pinned, label: item_label(meta) });
            }
        }
        // ディスクの項目がすべてメモリ（履歴は消し直し待ちも）にあるか（無ければ削除はしない）
        let memory_history: HashSet<Uuid> = scan.history.iter().chain(&scan.pending).map(|m| m.id).collect();
        let memory_pinned: HashSet<Uuid> = scan.pinned.iter().map(|m| m.id).collect();
        scan.consistent = scan.disk_history.iter().all(|m| memory_history.contains(&m.id))
            && scan.disk_pinned.iter().all(|m| memory_pinned.contains(&m.id));
        scan.report = report;
        Ok(scan)
    }

    // --- 終了 ---

    /// 受け付けを閉じ、受け付け済みの操作が終わるのを待ってから、最後の保存をする。
    /// `timeout` が Some なら（セッション終了。4秒）、その期限までに終わらなければ
    /// 保存せずに `TimedOut` を返す（処理中の操作と同じファイルへ書くと順序が壊れるため）。
    /// 期限は始めに1回だけ決め、起きるたびに残り時間を計算し直す。時間切れでも受け付けは閉じた
    /// まま（処理中の操作は止めない）。2回目以降の呼び出しも、同じく待って保存する。
    pub fn shutdown(&self, timeout: Option<Duration>) -> Result<(), OpError> {
        let deadline = timeout.map(|t| Instant::now() + t);
        {
            let mut admission = self.inner.admission.lock().unwrap_or_else(|p| p.into_inner());
            admission.closing = true;
            while admission.inflight > 0 {
                admission = match deadline {
                    None => self.inner.drained.wait(admission).unwrap_or_else(|p| p.into_inner()),
                    Some(deadline) => {
                        let now = Instant::now();
                        if now >= deadline {
                            return Err(OpError::TimedOut);
                        }
                        self.inner
                            .drained
                            .wait_timeout(admission, deadline - now)
                            .unwrap_or_else(|p| p.into_inner())
                            .0
                    }
                };
            }
        }
        // 受け付け済みの操作は終わったので、操作用のロックは待たない
        let mut ops = self.inner.ops.lock().map_err(|_| OpError::Poisoned)?;
        retry_pending(&self.inner.storage, &mut ops);
        let metas = self.with_service(|s| s.history_metas())?;
        if self.inner.persist {
            self.inner.storage.save_history_index(&metas)?;
            // 全件を書けたので、消し直し待ちの項目はもうインデックスに無い。blob を消して片付ける（消し直しだけが
            // 失敗して全件の保存が成功したとき、`Unsaved` にしない）
            for meta in std::mem::take(&mut ops.pending_removals) {
                remove_blobs(&self.inner.storage, &meta);
            }
        }
        if !ops.pending_removals.is_empty() {
            return Err(OpError::Unsaved(ops.pending_removals.len()));
        }
        Ok(())
    }
}

/// 取り込んだ項目を履歴の1件にする（ロックの外で呼ぶ。変換と blob の書き込みをする）。
fn prepare_capture(storage: &Storage, cfg: &Config, persist: bool, entry: Entry) -> Result<HistoryItem, StorageError> {
    if persist {
        // save=true の形式はディスクへ、save=false の形式は resident へ振り分ける
        let meta = storage.save_entry_filtered(&entry, |f| cfg.should_save(&f.format_name))?;
        let resident = entry.formats.into_iter().filter(|f| !cfg.should_save(&f.format_name)).collect();
        Ok(HistoryItem { meta, resident: Arc::new(resident) })
    } else {
        // 完全メモリモード: 何もディスクに書かず全形式をメモリに持つ
        Ok(HistoryItem { meta: EntryMeta::memory_only(&entry), resident: Arc::new(entry.formats) })
    }
}

/// 履歴から除いた項目の後始末（明示の削除・履歴のクリア・押し出し・重複の置き換え・切り詰め・
/// 起動時の切り詰めのすべてが通る）。インデックスから参照を消してから blob を消す（逆の順序だと
/// blob だけ消えてインデックスに残る「ゴースト」になる）。インデックスを更新できなければ blob は
/// 消さずに残し、あとで消し直す対象にする。完全メモリモードでは、blob の有無で絞らず、除いた
/// 項目の ID をすべてインデックスから消す（全形式が save=false で blob の無い、保存済みの項目も
/// インデックスには載っているため）。
/// インデックスを書けなかったときはその誤りを返す（項目は消し直す対象に残す）。
fn retire_items(storage: &Storage, ops: &mut OpState, items: Vec<HistoryItem>, retire: Retire) -> Result<(), StorageError> {
    if items.is_empty() {
        return Ok(());
    }
    let metas: Vec<EntryMeta> = items.into_iter().map(|i| i.meta).collect();
    let result = match retire {
        Retire::Full(all) => storage.save_history_index(&all),
        Retire::Targeted => {
            let ids: Vec<Uuid> = metas.iter().map(|m| m.id).collect();
            storage.remove_from_history_index(&ids)
        }
    };
    match result {
        Ok(()) => {
            for meta in &metas {
                remove_blobs(storage, meta);
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("履歴のインデックスを更新できないため、blob の削除を見送りました（あとで消し直します）: {e}");
            ops.pending_removals.extend(metas);
            Err(e)
        }
    }
}

/// 消し直す対象が残っていれば、インデックスから ID を消し、消せたら blob を消す。
fn retry_pending(storage: &Storage, ops: &mut OpState) {
    if ops.pending_removals.is_empty() {
        return;
    }
    let ids: Vec<Uuid> = ops.pending_removals.iter().map(|m| m.id).collect();
    match storage.remove_from_history_index(&ids) {
        Ok(()) => {
            for meta in std::mem::take(&mut ops.pending_removals) {
                remove_blobs(storage, &meta);
            }
        }
        Err(e) => eprintln!("履歴のインデックスから消し直せませんでした（次に消し直します）: {e}"),
    }
}

/// データのチェックの走査の結果と、消すときに使う参照の写し。
struct DataScan {
    report: DataReport,
    /// ディスクの history.toml・pinned.toml の項目がすべてメモリにある（外から書き戻されていない）
    consistent: bool,
    history: Vec<EntryMeta>,
    /// ピン留めのアイテム（フォルダの中も）
    pinned: Vec<EntryMeta>,
    pending: Vec<EntryMeta>,
    disk_history: Vec<EntryMeta>,
    disk_pinned: Vec<EntryMeta>,
}

impl DataScan {
    /// 走査の時点で参照されている blob・サムネイルの名前（比べるための小文字。メモリ・ディスクのインデックス・
    /// 消し直し待ちのすべて）。
    fn references(&self) -> HashSet<String> {
        reference_keys(self.history.iter().chain(&self.pinned).chain(&self.disk_history).chain(&self.disk_pinned).chain(&self.pending))
    }
}

/// 2回以上出てくる ID。
fn duplicate_ids(metas: &[EntryMeta]) -> HashSet<Uuid> {
    let mut seen = HashSet::new();
    metas.iter().filter(|m| !seen.insert(m.id)).map(|m| m.id).collect()
}

/// 項目が参照する blob・サムネイルの名前（比べるための小文字）。
fn reference_keys<'a>(metas: impl IntoIterator<Item = &'a EntryMeta>) -> HashSet<String> {
    metas
        .into_iter()
        .flat_map(|m| &m.formats)
        .flat_map(|f| std::iter::once(&f.blob).chain(f.thumb.as_ref()))
        .map(|name| datacheck::name_key(name))
        .collect()
}

/// ピン留めの木のアイテム（フォルダの中も）のメタデータ。
fn item_metas(nodes: &[PinnedNode]) -> Vec<EntryMeta> {
    let mut out = Vec::new();
    for node in nodes {
        store::collect_item_metas(node, &mut out);
    }
    out
}

/// データのチェックの結果に出す項目の名前（名前・テキストの1行目の先頭 50 文字。無ければ形式名）。
fn item_label(meta: &EntryMeta) -> String {
    match meta.title.as_deref().or(meta.preview.as_deref()).map(|s| s.lines().next().unwrap_or("")) {
        Some(line) if !line.is_empty() => line.chars().take(50).collect(),
        _ => {
            let names: Vec<&str> = meta.formats.iter().map(|f| f.format_name.as_str()).collect();
            format!("（{}）", names.join(", "))
        }
    }
}

/// ピン留めのアイテムの自動の名前:テキストのプレビューの1行目の先頭60文字（1行目が空なら None）。
fn auto_title(preview: &str) -> Option<String> {
    let title: String = preview.lines().next().unwrap_or("").chars().take(60).collect();
    (!title.is_empty()).then_some(title)
}

fn remove_blobs(storage: &Storage, meta: &EntryMeta) {
    if let Err(e) = storage.remove_entry_blobs(meta) {
        eprintln!("blobの削除に失敗: {e}");
    }
}

/// 履歴の永続化（blob書き込み）が有効か。save_on_exitのみの構成でも
/// blobはキャプチャ時に書く必要がある（遅延ロードでペースト時に読み戻すため）。
/// インデックスの書き込みタイミングだけがsave_on_change/save_on_exitで異なる。
fn persist_enabled(config: &Config) -> bool {
    config.history.save_on_change || config.history.save_on_exit
}

/// 履歴追加音を鳴らす（C版 CLCL の tool_utl「音を鳴らす」の統合。watcherスレッドから呼ばれる）。
/// ファイル未指定・再生失敗時はシステム音にフォールバック（C版と同じ）。
fn play_add_sound(sound_file: &str) {
    use windows::core::PCWSTR;
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FILENAME, SND_NODEFAULT};
    // MessageBeepはwinuser.h由来だがwindows 0.58ではDiagnostics::Debug配下
    use windows::Win32::System::Diagnostics::Debug::MessageBeep;
    use windows::Win32::UI::WindowsAndMessaging::MB_ICONASTERISK;

    // SND_ASYNCの再生はこの関数を抜けた後も続くため、ファイル名バッファは
    // 次回再生まで生かしておく（C版がグローバルバッファを渡すのと同じ理由）
    static SOUND_BUF: Mutex<Vec<u16>> = Mutex::new(Vec::new());

    let played = if sound_file.is_empty() {
        false
    } else {
        let mut buf = SOUND_BUF.lock().unwrap_or_else(|p| p.into_inner());
        *buf = sound_file.encode_utf16().chain([0]).collect();
        unsafe { PlaySoundW(PCWSTR(buf.as_ptr()), None, SND_FILENAME | SND_ASYNC | SND_NODEFAULT) }.as_bool()
    };
    if !played {
        let _ = unsafe { MessageBeep(MB_ICONASTERISK) };
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::data::Format;

    /// CF_UNICODETEXT だけの項目（UTF-16LE + NUL 終端。実際の CF_UNICODETEXT と同じ形）。
    pub(crate) fn text_entry(text: &str) -> Entry {
        let mut data: Vec<u8> = text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        data.extend_from_slice(&[0, 0]);
        Entry::new(vec![Format { format_name: "CF_UNICODETEXT".to_string(), format_id: 13, data }])
    }

    /// 完全メモリモード（save_on_change・save_on_exit が両方 false）の設定。
    pub(crate) fn memory_only_config() -> Config {
        let mut config = Config::default();
        config.history.save_on_exit = false;
        config.history.save_on_change = false;
        config
    }

    /// 一時フォルダの `Core`（呼び出し側が最後にフォルダを消す）。
    pub(crate) fn temp_core(config: Config) -> (std::path::PathBuf, Core) {
        let dir = std::env::temp_dir().join(format!("clclr-test-{}", Uuid::new_v4()));
        let core = Core::open_at(dir.clone(), Arc::new(RwLock::new(config))).unwrap();
        (dir, core)
    }

    fn reopen(dir: &std::path::Path, config: Config) -> Core {
        Core::open_at(dir.to_path_buf(), Arc::new(RwLock::new(config))).unwrap()
    }

    pub(crate) fn front_id(core: &Core) -> Uuid {
        core.read(|s| s.history.front().unwrap().meta.id).unwrap()
    }

    pub(crate) fn blob_path(dir: &std::path::Path, meta: &EntryMeta) -> std::path::PathBuf {
        dir.join("blobs").join(&meta.formats[0].blob)
    }

    fn index_text(dir: &std::path::Path) -> String {
        std::fs::read_to_string(dir.join("history.toml")).unwrap_or_default()
    }

    /// `write_atomic` の一時ファイル（`<名前>.tmp`）と同じ名前のフォルダを作り、そのファイルへの
    /// 書き込みを失敗させる。
    fn block_write(dir: &std::path::Path, name: &str) {
        std::fs::create_dir_all(dir.join(format!("{name}.tmp"))).unwrap();
    }

    fn unblock_write(dir: &std::path::Path, name: &str) {
        std::fs::remove_dir_all(dir.join(format!("{name}.tmp"))).unwrap();
    }

    use std::sync::mpsc;

    /// 取り込みを `capture:prepare`（変換の直前、操作用のロックを持った状態）で止める仕掛け。
    /// 止まったら `entered` に届き、`release` へ1回送るごとに1件ずつ進む。
    fn gate(core: &Core) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let entered_tx = Mutex::new(entered_tx);
        let release_rx = Mutex::new(release_rx);
        core.set_hook(move |point| {
            if point == "capture:prepare" {
                let _ = entered_tx.lock().unwrap().send(());
                let _ = release_rx.lock().unwrap().recv_timeout(Duration::from_secs(10));
            }
        });
        (entered_rx, release_tx)
    }

    fn blob_names(dir: &std::path::Path) -> std::collections::BTreeSet<std::ffi::OsString> {
        std::fs::read_dir(dir.join("blobs")).unwrap().map(|e| e.unwrap().file_name()).collect()
    }

    pub(crate) fn pinned_item(core: &Core, index: usize) -> EntryMeta {
        core.read(|s| match &s.pinned[index] {
            PinnedNode::Item(meta) => meta.clone(),
            PinnedNode::Folder(_) => panic!("expected item"),
        })
        .unwrap()
    }

    /// 登録（`Ticket`）の下での読み込みは、別の `Core` の登録を断り、同じ `Core` の複製の登録は
    /// 通す。登録の後に終了処理が受け付けを閉じても読み込みは続き、終了処理は登録が外れるまで待つ。
    #[test]
    fn load_admitted_checks_ticket_owner_and_continues_after_closing() {
        let (dir_a, a) = temp_core(Config::default());
        let (dir_b, b) = temp_core(Config::default());
        a.capture(text_entry("A")).unwrap();
        let id = front_id(&a);
        let foreign = b.admit().unwrap();
        assert!(matches!(a.load_for_send_admitted(&foreign, id, false), Err(OpError::ForeignTicket)));
        drop(foreign);

        let ticket = a.clone().admit().unwrap();
        let shutdown = {
            let a = a.clone();
            std::thread::spawn(move || a.shutdown(None))
        };
        // 受け付けが閉じるまで待つ（閉じる前に取れた登録はすぐ外す）
        let deadline = Instant::now() + Duration::from_secs(5);
        while let Ok(probe) = a.admit() {
            drop(probe);
            assert!(Instant::now() < deadline, "受け付けが閉じない");
            std::thread::sleep(Duration::from_millis(5));
        }
        let entry = a.load_for_send_admitted(&ticket, id, false).unwrap();
        assert_eq!(entry.formats.len(), 1);
        std::thread::sleep(Duration::from_millis(50));
        assert!(!shutdown.is_finished(), "登録が残っているのに終了処理が進んだ");
        drop(ticket);
        shutdown.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(dir_a);
        let _ = std::fs::remove_dir_all(dir_b);
    }

    /// 実際に音が鳴ったかまでは検証できないが、空文字列（システムビープへのフォールバック）と
    /// 存在しないファイル（`PlaySoundW` 失敗→ビープ）のどちらでもパニックせず戻る。
    #[test]
    fn play_add_sound_does_not_panic_for_empty_or_missing_file() {
        play_add_sound("");
        play_add_sound(r"C:\clclr-test-nonexistent-sound-file.wav");
    }

    /// 履歴・ピン留めを変える操作のたびに変更番号が変わる。読むだけ・送るための読み込みでは変わらない。
    #[test]
    fn revision_changes_on_every_mutation() {
        let (dir, core) = temp_core(Config::default());
        let revision = || core.read(|s| s.revision()).unwrap();
        let last = std::cell::Cell::new(revision());
        let changed = |what: &str| {
            assert_ne!(revision(), last.get(), "{what} で変更番号が変わっていない");
            last.set(revision());
        };
        core.capture(text_entry("一件目")).unwrap();
        changed("取り込み");
        let id = front_id(&core);
        core.load_for_send(id, false).unwrap();
        assert_eq!(revision(), last.get(), "読むだけで変わった");
        core.pin(id, None).unwrap();
        changed("ピン留め");
        let pinned_id = pinned_item(&core, 0).id;
        core.delete_pinned(pinned_id).unwrap();
        changed("ピン留めの削除");
        core.delete_history(id).unwrap();
        changed("削除");
        core.capture(text_entry("二件目")).unwrap();
        changed("取り込み");
        core.clear_history().unwrap();
        changed("履歴のクリア");
        let _ = std::fs::remove_dir_all(dir);
    }

    // --- データのチェック ---

    fn write_blob(dir: &std::path::Path, name: &str, data: &[u8]) {
        std::fs::write(dir.join("blobs").join(name), data).unwrap();
    }

    fn names(files: &[FileEntry]) -> Vec<String> {
        let mut v: Vec<String> = files.iter().map(|f| f.name.clone()).collect();
        v.sort();
        v
    }

    /// 見つけるもの: 参照されない blob（大文字・小文字は区別しない）、blob が無い項目（履歴・ピン留め）、
    /// blob・インデックスの一時ファイル。形に合わないファイル・フォルダ・config.toml.tmp は対象外。消し直し待ちの
    /// 項目の blob は参照ありとみなす。チェックは何も消さない。
    #[test]
    fn check_data_classifies_files_and_items() {
        // 変更の都度保存する設定（項目がインデックスに載るので、書けないと消し直しも失敗し続け、消し直し待ちに残る）
        let mut config = Config::default();
        config.history.save_on_change = true;
        let (dir, core) = temp_core(config);
        core.capture(text_entry("残る")).unwrap();
        let kept = core.read(|s| s.history.front().unwrap().meta.clone()).unwrap();
        core.capture(text_entry("欠ける")).unwrap();
        let broken = core.read(|s| s.history.front().unwrap().meta.clone()).unwrap();
        core.create_folder(None, "棚").unwrap();
        core.pin(kept.id, Some(folder_id(&core, "棚"))).unwrap();
        let pinned = core.read(|s| item_metas(&s.pinned)[0].clone()).unwrap();
        // 消し直し待ち（インデックスを書けずに履歴から除いた項目。blob は残る）
        core.capture(text_entry("消し直し待ち")).unwrap();
        let pending = core.read(|s| s.history.front().unwrap().meta.clone()).unwrap();
        block_write(&dir, "history.toml");
        assert!(matches!(core.delete_history(pending.id), Err(OpError::IndexNotSaved(_))));
        std::fs::remove_file(blob_path(&dir, &broken)).unwrap();
        std::fs::remove_file(blob_path(&dir, &pinned)).unwrap();
        // 参照ありの blob の名前を大文字にしても、参照ありのまま（欠けていない）
        let upper = kept.formats[0].blob.to_ascii_uppercase();
        std::fs::rename(blob_path(&dir, &kept), dir.join("blobs").join(&upper)).unwrap();
        let orphan = format!("{}_0.bin", Uuid::new_v4());
        write_blob(&dir, &orphan, b"abc");
        let orphan_thumb = format!("{}_0.thumb.webp", Uuid::new_v4()).to_ascii_uppercase();
        write_blob(&dir, &orphan_thumb, b"t");
        let blob_temp = format!("{}_1.webp.tmp", Uuid::new_v4());
        write_blob(&dir, &blob_temp, b"tmp!");
        for other in ["notes.tmp", "desktop.ini"] {
            write_blob(&dir, other, b"x");
        }
        std::fs::create_dir(dir.join("blobs").join(format!("{}_0.bin", Uuid::new_v4()))).unwrap();
        std::fs::write(dir.join("pinned.toml.tmp"), b"12345").unwrap();
        std::fs::write(dir.join("config.toml.tmp"), b"c").unwrap();

        let report = core.check_data().unwrap();
        let mut expected = vec![orphan.clone(), orphan_thumb.clone()];
        expected.sort();
        assert_eq!(names(&report.orphans), expected);
        assert_eq!(report.orphans.iter().map(|f| f.size).sum::<u64>(), 4);
        let mut missing: Vec<(Uuid, bool)> = report.missing.iter().map(|m| (m.id, m.pinned)).collect();
        missing.sort();
        let mut expected = vec![(broken.id, false), (pinned.id, true)];
        expected.sort();
        assert_eq!(missing, expected);
        assert!(report.missing.iter().any(|m| m.label == "欠ける"));
        let mut temps: Vec<(TempPlace, String, u64)> = report.temps.iter().map(|t| (t.place, t.name.clone(), t.size)).collect();
        temps.sort_by(|a, b| a.1.cmp(&b.1));
        let mut expected = vec![(TempPlace::Blobs, blob_temp.clone(), 4), (TempPlace::DataDir, "pinned.toml.tmp".to_string(), 5)];
        expected.sort_by(|a, b| a.1.cmp(&b.1));
        assert_eq!(temps, expected);
        let mut ignored = report.ignored.clone();
        ignored.sort();
        assert_eq!(ignored, ["blobs\\desktop.ini", "blobs\\notes.tmp", "config.toml.tmp"]);
        // 何も消していない
        assert!(dir.join("blobs").join(&orphan).exists() && dir.join("pinned.toml.tmp").exists());
        assert!(blob_path(&dir, &pending).exists(), "消し直し待ちの blob を数えた・消した");
        unblock_write(&dir, "history.toml");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 削除: 計画にあり、今もう一度調べても見つかるものだけを消す（計画の後に増えた孤児・参照されている名前は
    /// 消さない）。欠けた項目は履歴・ピン留めとインデックス・pinned.toml から消える。何も無くなれば次のチェックは空。
    #[test]
    fn clean_data_removes_only_planned_findings_that_still_exist() {
        let (dir, core) = temp_core(Config::default());
        core.capture(text_entry("残る")).unwrap();
        let kept = core.read(|s| s.history.front().unwrap().meta.clone()).unwrap();
        core.capture(text_entry("欠ける")).unwrap();
        let broken = core.read(|s| s.history.front().unwrap().meta.clone()).unwrap();
        core.pin(kept.id, None).unwrap();
        let pinned = pinned_item(&core, 0);
        std::fs::remove_file(blob_path(&dir, &broken)).unwrap();
        std::fs::remove_file(blob_path(&dir, &pinned)).unwrap();
        let orphan = format!("{}_0.bin", Uuid::new_v4());
        write_blob(&dir, &orphan, b"abc");
        std::fs::write(dir.join("history.toml.tmp"), b"xy").unwrap();

        let mut plan = core.check_data().unwrap();
        // 計画の後に増えた孤児は消さない。参照されている名前を計画に紛れ込ませても消さない
        let later = format!("{}_0.bin", Uuid::new_v4());
        write_blob(&dir, &later, b"z");
        plan.orphans.push(FileEntry { name: kept.formats[0].blob.clone(), size: 0 });
        let result = core.clean_data(&plan).unwrap();
        assert_eq!(result.files_removed, 2);
        assert_eq!(result.bytes_removed, 5);
        assert_eq!(result.items_removed, 2);
        assert!(result.failures.is_empty() && !result.interrupted && result.index_error.is_none(), "{result:?}");
        assert!(!dir.join("blobs").join(&orphan).exists() && !dir.join("history.toml.tmp").exists());
        assert!(dir.join("blobs").join(&later).exists(), "計画に無いものを消した");
        assert!(blob_path(&dir, &kept).exists(), "参照されている blob を消した");
        assert!(core.read(|s| s.history.get_by_id(broken.id).is_none()).unwrap());
        assert_eq!(core.read(|s| s.pinned.len()), Some(0));
        assert!(!index_text(&dir).contains(&broken.id.to_string()));
        assert!(!std::fs::read_to_string(dir.join("pinned.toml")).unwrap().contains(&pinned.id.to_string()));

        std::fs::remove_file(dir.join("blobs").join(&later)).unwrap();
        assert!(core.check_data().unwrap().is_clean());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 手で編集して2つの項目が同じ blob を参照しているとき、欠けた項目を消しても、もう一方が使う blob は残す。
    #[test]
    fn clean_data_keeps_blob_shared_with_remaining_item() {
        let dir = std::env::temp_dir().join(format!("clclr-test-{}", Uuid::new_v4()));
        let storage = Storage::open(dir.clone()).unwrap();
        let (shared, gone) = (format!("{}_0.bin", Uuid::new_v4()), format!("{}_1.bin", Uuid::new_v4()));
        write_blob(&dir, &shared, b"shared");
        let format = |blob: &str| crate::storage::FormatMeta {
            format_name: "CF_HDROP".into(),
            format_id: 15,
            blob: blob.to_string(),
            size: 0,
            thumb: None,
        };
        let meta = |formats| EntryMeta { id: Uuid::new_v4(), title: None, modified: 0.0, hash: 0, preview: None, formats };
        let broken = meta(vec![format(&shared), format(&gone)]);
        let other = meta(vec![format(&shared)]);
        storage.save_history_index(&[broken.clone(), other.clone()]).unwrap();
        let core = reopen(&dir, Config::default());
        let plan = core.check_data().unwrap();
        assert_eq!(plan.missing.iter().map(|m| m.id).collect::<Vec<_>>(), [broken.id]);
        assert!(plan.orphans.is_empty());
        core.clean_data(&plan).unwrap();
        assert!(core.read(|s| s.history.get_by_id(broken.id).is_none() && s.history.get_by_id(other.id).is_some()).unwrap());
        assert!(dir.join("blobs").join(&shared).exists(), "残る項目が使う blob を消した");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 欠けた履歴の項目とピン留めのアイテムが同じ blob を共有しているとき、片方のファイルの保存に失敗して
    /// その項目が残ったら、共有している blob は消さない。どちらの向きの失敗でも同じ。
    #[test]
    fn clean_data_keeps_shared_blob_when_one_side_fails_to_save() {
        for blocked in ["pinned.toml", "history.toml"] {
            let dir = std::env::temp_dir().join(format!("clclr-test-{}", Uuid::new_v4()));
            let storage = Storage::open(dir.clone()).unwrap();
            let shared = format!("{}_0.bin", Uuid::new_v4());
            write_blob(&dir, &shared, b"shared");
            let format = |blob: String| crate::storage::FormatMeta {
                format_name: "CF_HDROP".into(),
                format_id: 15,
                blob,
                size: 0,
                thumb: None,
            };
            let meta = |gone: String| EntryMeta {
                id: Uuid::new_v4(),
                title: None,
                modified: 0.0,
                hash: 0,
                preview: None,
                formats: vec![format(shared.clone()), format(gone)],
            };
            let history = meta(format!("{}_1.bin", Uuid::new_v4()));
            let pinned = meta(format!("{}_1.bin", Uuid::new_v4()));
            storage.save_history_index(std::slice::from_ref(&history)).unwrap();
            storage.save_pinned(&[PinnedNode::Item(pinned.clone())]).unwrap();
            let mut config = Config::default();
            config.history.save_on_change = true;
            let core = reopen(&dir, config);
            let plan = core.check_data().unwrap();
            assert_eq!(plan.missing.len(), 2, "{blocked}");
            block_write(&dir, blocked);
            let result = core.clean_data(&plan).unwrap();
            assert!(dir.join("blobs").join(&shared).exists(), "{blocked} の保存に失敗したのに共有の blob を消した");
            if blocked == "pinned.toml" {
                assert_eq!(result.failures.len(), 1, "{result:?}");
                assert_eq!(core.read(|s| s.pinned.len()), Some(1));
                assert!(core.read(|s| s.history.get_by_id(history.id).is_none()).unwrap());
            } else {
                assert!(result.index_error.is_some(), "{result:?}");
                assert_eq!(core.read(|s| s.pinned.len()), Some(0));
            }
            unblock_write(&dir, blocked);
            drop(core);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// 欠けた項目の削除は1件ずつで、件と件の間で終了の要求を確かめて止まる（1件目は消し、2件目は残す）。
    #[test]
    fn clean_data_stops_between_items_when_admission_closes() {
        let (dir, core) = temp_core(Config::default());
        let mut broken = Vec::new();
        for text in ["一", "二"] {
            core.capture(text_entry(text)).unwrap();
            let meta = core.read(|s| s.history.front().unwrap().meta.clone()).unwrap();
            std::fs::remove_file(blob_path(&dir, &meta)).unwrap();
            broken.push(meta.id);
        }
        let plan = core.check_data().unwrap();
        assert_eq!(plan.missing.len(), 2);
        let calls = Arc::new(Mutex::new(0usize));
        let (closed_tx, closed_rx) = mpsc::channel::<()>();
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        {
            let calls = Arc::clone(&calls);
            let (closed_rx, entered_tx) = (Mutex::new(closed_rx), Mutex::new(entered_tx));
            core.set_hook(move |point| {
                if point == "clean:item" {
                    let n = {
                        let mut calls = calls.lock().unwrap();
                        *calls += 1;
                        *calls
                    };
                    if n == 2 {
                        let _ = entered_tx.lock().unwrap().send(());
                        let _ = closed_rx.lock().unwrap().recv_timeout(Duration::from_secs(10));
                    }
                }
            });
        }
        let cleaner = {
            let (core, plan) = (core.clone(), plan.clone());
            std::thread::spawn(move || core.clean_data(&plan))
        };
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let shutdown = {
            let core = core.clone();
            std::thread::spawn(move || core.shutdown(None))
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !core.closing() {
            assert!(Instant::now() < deadline, "受け付けが閉じない");
            std::thread::sleep(Duration::from_millis(5));
        }
        closed_tx.send(()).unwrap();
        let result = cleaner.join().unwrap().unwrap();
        assert!(result.interrupted);
        assert_eq!(result.items_removed, 1);
        shutdown.join().unwrap().unwrap();
        let remaining = core.read(|s| broken.iter().filter(|id| s.history.get_by_id(**id).is_some()).count()).unwrap();
        assert_eq!(remaining, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// ディスクの history.toml に、メモリに無い項目がある（実行中に外から書き戻された）ときは、何も消さない。
    #[test]
    fn clean_data_refuses_when_disk_index_has_unknown_items() {
        let (dir, core) = temp_core(Config::default());
        core.capture(text_entry("欠ける")).unwrap();
        let broken = core.read(|s| s.history.front().unwrap().meta.clone()).unwrap();
        std::fs::remove_file(blob_path(&dir, &broken)).unwrap();
        let orphan = format!("{}_0.bin", Uuid::new_v4());
        write_blob(&dir, &orphan, b"abc");
        let plan = core.check_data().unwrap();
        let outside = EntryMeta { id: Uuid::new_v4(), title: None, modified: 0.0, hash: 0, preview: None, formats: vec![] };
        core.storage().save_history_index(&[broken.clone(), outside]).unwrap();
        assert!(matches!(core.clean_data(&plan), Err(OpError::DataChanged)));
        assert!(dir.join("blobs").join(&orphan).exists());
        assert!(core.read(|s| s.history.get_by_id(broken.id).is_some()).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 走査の途中で終了の要求（受け付けが閉じる）が来たら、`Closing` で抜け、終了処理は待たされない。
    #[test]
    fn check_data_stops_when_admission_closes() {
        let (dir, core) = temp_core(Config::default());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (entered_tx, release_rx) = (Mutex::new(entered_tx), Mutex::new(release_rx));
        core.set_hook(move |point| {
            if point == "check:scan" {
                let _ = entered_tx.lock().unwrap().send(());
                let _ = release_rx.lock().unwrap().recv_timeout(Duration::from_secs(10));
            }
        });
        let checker = {
            let core = core.clone();
            std::thread::spawn(move || core.check_data())
        };
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let shutdown = {
            let core = core.clone();
            std::thread::spawn(move || core.shutdown(None))
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !core.closing() {
            assert!(Instant::now() < deadline, "受け付けが閉じない");
            std::thread::sleep(Duration::from_millis(5));
        }
        release_tx.send(()).unwrap();
        assert!(matches!(checker.join().unwrap(), Err(OpError::Closing)));
        shutdown.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 形式ごとの blob の名前を指定した項目のメタデータ（手で編集したインデックスを作る）。
    fn meta_with_blobs(id: Uuid, blobs: &[&str]) -> EntryMeta {
        let formats = blobs
            .iter()
            .map(|blob| crate::storage::FormatMeta {
                format_name: "CF_HDROP".into(),
                format_id: 15,
                blob: blob.to_string(),
                size: 0,
                thumb: None,
            })
            .collect();
        EntryMeta { id, title: None, modified: 0.0, hash: 0, preview: None, formats }
    }

    /// `run` を別のスレッドで始め、`point` の差し込み点に1回目に来たところで受け付けを閉じ（終了処理）、それから
    /// 続けさせる。`run` の結果を返す。
    fn run_until_closed_at<T: Send + 'static>(
        core: &Core,
        point: &'static str,
        run: impl FnOnce(Core) -> T + Send + 'static,
    ) -> T {
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (entered_tx, release_rx) = (Mutex::new(Some(entered_tx)), Mutex::new(release_rx));
        core.set_hook(move |p| {
            if p == point {
                if let Some(tx) = entered_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                    let _ = release_rx.lock().unwrap().recv_timeout(Duration::from_secs(10));
                }
            }
        });
        let worker = {
            let core = core.clone();
            std::thread::spawn(move || run(core))
        };
        entered_rx.recv_timeout(Duration::from_secs(5)).expect("差し込み点に来ない");
        let shutdown = {
            let core = core.clone();
            std::thread::spawn(move || core.shutdown(None))
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !core.closing() {
            assert!(Instant::now() < deadline, "受け付けが閉じない");
            std::thread::sleep(Duration::from_millis(5));
        }
        release_tx.send(()).unwrap();
        let result = worker.join().unwrap();
        shutdown.join().unwrap().unwrap();
        result
    }

    /// blobs の列挙の途中でも、終了の要求（受け付けが閉じる）が来たら `Closing` で抜ける。ファイルでない項目
    /// （フォルダ）だけでも、項目ごとに確かめる。
    #[test]
    fn check_data_stops_while_listing_files_when_admission_closes() {
        let (dir, core) = temp_core(Config::default());
        for _ in 0..3 {
            std::fs::create_dir(dir.join("blobs").join(format!("{}_0.bin", Uuid::new_v4()))).unwrap();
        }
        let result = run_until_closed_at(&core, "check:file", |core| core.check_data());
        assert!(matches!(result, Err(OpError::Closing)), "{result:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 欠けた項目を消した後の blob の削除の途中でも、終了の要求が来たらやめる（項目は消えたまま、残った blob は
    /// 次のチェックで孤児として見つかる）。
    #[test]
    fn clean_data_stops_before_removing_blobs_when_admission_closes() {
        let dir = std::env::temp_dir().join(format!("clclr-test-{}", Uuid::new_v4()));
        let storage = Storage::open(dir.clone()).unwrap();
        let (present, gone) = (format!("{}_0.bin", Uuid::new_v4()), format!("{}_1.bin", Uuid::new_v4()));
        write_blob(&dir, &present, b"abc");
        let broken = meta_with_blobs(Uuid::new_v4(), &[&present, &gone]);
        storage.save_history_index(std::slice::from_ref(&broken)).unwrap();
        let core = reopen(&dir, Config::default());
        let plan = core.check_data().unwrap();
        assert_eq!(plan.missing.len(), 1);
        let result = run_until_closed_at(&core, "clean:blob", move |core| core.clean_data(&plan)).unwrap();
        assert!(result.interrupted && result.items_removed == 1, "{result:?}");
        assert!(core.read(|s| s.history.get_by_id(broken.id).is_none()).unwrap());
        assert!(dir.join("blobs").join(&present).exists(), "終了の要求の後に blob を消した");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 手で編集して履歴に同じ ID の項目が2つある（片方だけ blob が欠けている）ときは、その ID を欠けた項目に数えず、
    /// 計画に紛れ込ませても消さない（ID で消すと、残るはずの項目やその blob を消しうる）。
    #[test]
    fn clean_data_skips_items_with_duplicate_ids() {
        let dir = std::env::temp_dir().join(format!("clclr-test-{}", Uuid::new_v4()));
        let storage = Storage::open(dir.clone()).unwrap();
        let (shared, gone) = (format!("{}_0.bin", Uuid::new_v4()), format!("{}_1.bin", Uuid::new_v4()));
        write_blob(&dir, &shared, b"shared");
        let id = Uuid::new_v4();
        storage.save_history_index(&[meta_with_blobs(id, &[&shared, &gone]), meta_with_blobs(id, &[&shared])]).unwrap();
        let core = reopen(&dir, Config::default());
        let mut plan = core.check_data().unwrap();
        assert!(plan.missing.is_empty(), "重複した ID を欠けた項目に数えた: {plan:?}");
        plan.missing.push(MissingItem { id, pinned: false, label: String::new() });
        let result = core.clean_data(&plan).unwrap();
        assert_eq!(result.items_removed, 0, "{result:?}");
        assert_eq!(core.read(|s| s.history.iter().filter(|i| i.meta.id == id).count()), Some(2));
        assert!(dir.join("blobs").join(&shared).exists(), "残る項目が使う blob を消した");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 参照している blob の場所がファイルでなくフォルダなら、読めないので欠けた項目に数える。
    #[test]
    fn check_data_counts_blob_replaced_by_folder_as_missing() {
        let (dir, core) = temp_core(Config::default());
        core.capture(text_entry("フォルダに置き換わる")).unwrap();
        let meta = core.read(|s| s.history.front().unwrap().meta.clone()).unwrap();
        let path = blob_path(&dir, &meta);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let report = core.check_data().unwrap();
        assert_eq!(report.missing.iter().map(|m| m.id).collect::<Vec<_>>(), [meta.id]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// ピン留めのアイテムの名前の変更: 前後の空白を除いて付け、空にするとテキストの1行目
    /// （blob から作り直す）に戻る。テキストの無い項目は名前なしに戻る。フォルダの中のアイテムも変えられ、
    /// 再起動しても残る。今と同じ名前なら保存しない。フォルダ・知らない ID は `NotFound`。保存に失敗したら
    /// メモリは変えない。
    #[test]
    fn rename_pinned_item_sets_trims_and_restores_auto_title() {
        let (dir, core) = temp_core(Config::default());
        core.create_folder(None, "棚").unwrap();
        let shelf = folder_id(&core, "棚");
        core.capture(text_entry("一行目\n二行目")).unwrap();
        core.pin(front_id(&core), Some(shelf)).unwrap();
        // テキストの無い項目（中身は名前に関係しない）
        let data = Entry::new(vec![Format { format_name: "CF_HDROP".into(), format_id: 15, data: vec![1, 2, 3] }]);
        core.capture(data).unwrap();
        core.pin(front_id(&core), None).unwrap();
        let find = |id: Uuid| core.read(|s| store::find_item(&s.pinned, id).cloned()).flatten().unwrap();
        let text_id = core.read(|s| match &s.pinned[0] {
            PinnedNode::Folder(f) => match &f.children[0] {
                PinnedNode::Item(meta) => meta.id,
                PinnedNode::Folder(_) => unreachable!(),
            },
            PinnedNode::Item(_) => unreachable!(),
        }).unwrap();
        let data_id = pinned_item(&core, 1).id;
        assert_eq!(find(text_id).title.as_deref(), Some("一行目"));
        assert_eq!(find(data_id).title, None);

        core.rename_pinned_item(text_id, "  名前  ").unwrap();
        assert_eq!(find(text_id).title.as_deref(), Some("名前"));
        let revision = core.read(|s| s.revision()).unwrap();
        core.rename_pinned_item(text_id, "名前").unwrap();
        assert_eq!(core.read(|s| s.revision()).unwrap(), revision, "同じ名前で保存した");
        drop(core);
        let core = reopen(&dir, Config::default());
        let find = |id: Uuid| core.read(|s| store::find_item(&s.pinned, id).cloned()).flatten().unwrap();
        assert_eq!(find(text_id).title.as_deref(), Some("名前"), "再起動で名前が消えた");

        core.rename_pinned_item(text_id, "   ").unwrap();
        assert_eq!(find(text_id).title.as_deref(), Some("一行目"), "自動の名前に戻っていない");
        core.rename_pinned_item(data_id, "画像").unwrap();
        assert_eq!(find(data_id).title.as_deref(), Some("画像"));
        core.rename_pinned_item(data_id, "").unwrap();
        assert_eq!(find(data_id).title, None);

        assert!(matches!(core.rename_pinned_item(folder_id(&core, "棚"), "x"), Err(OpError::NotFound)));
        assert!(matches!(core.rename_pinned_item(Uuid::new_v4(), "x"), Err(OpError::NotFound)));

        block_write(&dir, "pinned.toml");
        assert!(core.rename_pinned_item(data_id, "失敗").is_err());
        assert_eq!(find(data_id).title, None, "保存に失敗したのにメモリを変えた");
        unblock_write(&dir, "pinned.toml");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// ピン留めは新しい ID・新しい blob の複製。履歴を消してもピン留めの blob は残り、ピン留めを
    /// 消すと blob も消える。
    #[test]
    fn pin_duplicates_entry_with_new_id_and_blobs() {
        let (dir, core) = temp_core(Config::default());
        core.capture(text_entry("ピン留めテスト")).unwrap();
        let history_id = front_id(&core);
        core.pin(history_id, None).unwrap();
        assert_eq!(core.read(|s| s.pinned.len()), Some(1));
        let meta = pinned_item(&core, 0);
        assert_ne!(meta.id, history_id);
        assert_eq!(meta.title.as_deref(), Some("ピン留めテスト"));
        let pinned_blob = blob_path(&dir, &meta);
        assert!(pinned_blob.exists());
        assert!(dir.join("pinned.toml").exists());

        core.delete_history(history_id).unwrap();
        assert!(pinned_blob.exists());
        assert_eq!(core.load_for_send(meta.id, true).unwrap().formats.len(), 1);

        core.delete_pinned(meta.id).unwrap();
        assert!(core.read(|s| s.pinned.is_empty()).unwrap());
        assert!(!pinned_blob.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// pinned.toml へ書けなければ、ピン留めの追加はメモリも変更番号も変えず、書いた blob を消す。
    #[test]
    fn pin_failure_changes_nothing_and_removes_written_blobs() {
        let (dir, core) = temp_core(Config::default());
        core.capture(text_entry("追加の失敗")).unwrap();
        let id = front_id(&core);
        let (before_blobs, before_revision) = (blob_names(&dir), core.read(|s| s.revision()).unwrap());
        block_write(&dir, "pinned.toml");
        assert!(matches!(core.pin(id, None), Err(OpError::Storage(_))));
        assert!(core.read(|s| s.pinned.is_empty()).unwrap());
        assert_eq!(core.read(|s| s.revision()).unwrap(), before_revision, "取り消した途中の状態を公開した");
        assert_eq!(blob_names(&dir), before_blobs, "書いた blob が孤児として残った");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// pinned.toml へ書けなければ、ピン留めの削除はメモリも変更番号も blob も変えない。
    #[test]
    fn delete_pinned_failure_changes_nothing() {
        let (dir, core) = temp_core(Config::default());
        core.capture(text_entry("削除の失敗")).unwrap();
        core.pin(front_id(&core), None).unwrap();
        let meta = pinned_item(&core, 0);
        let before_revision = core.read(|s| s.revision()).unwrap();
        block_write(&dir, "pinned.toml");
        assert!(core.delete_pinned(meta.id).is_err());
        assert_eq!(core.read(|s| s.pinned.len()), Some(1));
        assert_eq!(core.read(|s| s.revision()).unwrap(), before_revision);
        assert!(blob_path(&dir, &meta).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    // --- ピン留めのフォルダ ---

    /// ピン留めのツリーから名前でフォルダの ID を引く（テスト用。最初に見つかったもの）。
    pub(crate) fn folder_id(core: &Core, title: &str) -> Uuid {
        fn find(nodes: &[PinnedNode], title: &str) -> Option<Uuid> {
            nodes.iter().find_map(|node| match node {
                PinnedNode::Folder(f) if f.title == title => Some(f.id),
                PinnedNode::Folder(f) => find(&f.children, title),
                PinnedNode::Item(_) => None,
            })
        }
        core.read(|s| find(&s.pinned, title)).flatten().unwrap_or_else(|| panic!("フォルダ {title} が無い"))
    }

    fn folder_names(core: &Core, parent: Option<Uuid>) -> Vec<String> {
        core.read(|s| {
            store::find_children(&s.pinned, parent)
                .unwrap()
                .iter()
                .filter_map(|n| match n {
                    PinnedNode::Folder(f) => Some(f.title.clone()),
                    PinnedNode::Item(_) => None,
                })
                .collect()
        })
        .unwrap()
    }

    /// フォルダの作成: ルート・フォルダの中の末尾に、前後の空白を除いた名前で作る。空の名前・同じ親の
    /// 同じ名前（前後の空白を除いて比べる。大文字小文字は区別）・無い親は断り、何も変えない。開き直しても残る。
    #[test]
    fn create_folder_validates_name_and_parent() {
        let (dir, core) = temp_core(Config::default());
        core.create_folder(None, "  仕事 ").unwrap();
        let work = folder_id(&core, "仕事");
        core.create_folder(Some(work), "中").unwrap();
        core.create_folder(None, "遊び").unwrap();
        assert_eq!(folder_names(&core, None), ["仕事", "遊び"]);
        assert_eq!(folder_names(&core, Some(work)), ["中"]);

        let revision = core.read(|s| s.revision()).unwrap();
        assert!(matches!(core.create_folder(None, " \u{3000} "), Err(OpError::InvalidName)));
        assert!(matches!(core.create_folder(None, "仕事 "), Err(OpError::DuplicateName)));
        assert!(matches!(core.create_folder(Some(Uuid::new_v4()), "新"), Err(OpError::TargetNotFound)));
        assert_eq!(core.read(|s| s.revision()).unwrap(), revision, "断ったのに変わった");
        core.create_folder(None, "しごと").unwrap();
        core.create_folder(Some(work), "仕事").unwrap(); // 親が違えば同じ名前でよい
        drop(core);

        let core = reopen(&dir, Config::default());
        assert_eq!(folder_names(&core, None), ["仕事", "遊び", "しごと"]);
        assert_eq!(folder_names(&core, Some(work)), ["中", "仕事"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 名前の変更の判定の順: フォルダでなければ NotFound、空なら InvalidName、今の名前と
    /// 同じなら何もしない（既にある同名の兄弟があっても）、自分以外の兄弟と同じなら DuplicateName。
    #[test]
    fn rename_folder_checks_in_order() {
        let (dir, core) = temp_core(Config::default());
        core.capture(text_entry("項目")).unwrap();
        core.pin(front_id(&core), None).unwrap();
        let item = pinned_item(&core, 0).id;
        core.create_folder(None, "A").unwrap();
        core.create_folder(None, "B").unwrap();
        let a = folder_id(&core, "A");

        assert!(matches!(core.rename_folder(item, "X"), Err(OpError::NotFound)));
        assert!(matches!(core.rename_folder(Uuid::new_v4(), "X"), Err(OpError::NotFound)));
        assert!(matches!(core.rename_folder(a, "  "), Err(OpError::InvalidName)));
        assert!(matches!(core.rename_folder(a, " B "), Err(OpError::DuplicateName)));
        let revision = core.read(|s| s.revision()).unwrap();
        core.rename_folder(a, " A ").unwrap();
        assert_eq!(core.read(|s| s.revision()).unwrap(), revision, "同じ名前なのに保存した");
        core.rename_folder(a, " 新しい名前 ").unwrap();
        assert_eq!(folder_names(&core, None), ["新しい名前", "B"]);

        // 手で書いた pinned.toml にある同名の兄弟: 名前を変えずに確定しても断らない
        let dup = vec![
            PinnedNode::Folder(store::PinnedFolder { id: Uuid::new_v4(), title: "同じ".into(), children: vec![] }),
            PinnedNode::Folder(store::PinnedFolder { id: Uuid::new_v4(), title: "同じ".into(), children: vec![] }),
        ];
        let first = dup[0].id();
        core.storage().save_pinned(&dup).unwrap();
        drop(core);
        let core = reopen(&dir, Config::default());
        core.rename_folder(first, "同じ").unwrap();
        assert!(matches!(core.rename_folder(first, "別"), Ok(())));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// ピン留めの入れる先: フォルダの末尾に入れる。入れる先が無ければ TargetNotFound で、blob を残さない。
    #[test]
    fn pin_into_folder_and_missing_target_leaves_no_blob() {
        let (dir, core) = temp_core(Config::default());
        core.capture(text_entry("入れる")).unwrap();
        let id = front_id(&core);
        core.create_folder(None, "箱").unwrap();
        let folder = folder_id(&core, "箱");
        core.pin(id, Some(folder)).unwrap();
        assert_eq!(core.read(|s| store::find_children(&s.pinned, Some(folder)).unwrap().len()), Some(1));

        let (before_blobs, revision) = (blob_names(&dir), core.read(|s| s.revision()).unwrap());
        assert!(matches!(core.pin(id, Some(Uuid::new_v4())), Err(OpError::TargetNotFound)));
        assert_eq!(blob_names(&dir), before_blobs, "入れる先が無いのに blob を残した");
        assert_eq!(core.read(|s| s.revision()).unwrap(), revision);
        assert!(matches!(core.pin(Uuid::new_v4(), Some(Uuid::new_v4())), Err(OpError::NotFound)));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 移動: 項目だけを、フォルダ・ルートの末尾へ移す。blob は変えない。今いる所なら何もしない。
    /// 対象が無い（両方無いときも）・フォルダを移そうとした → NotFound、入れる先が無い → TargetNotFound。
    /// 書けなければメモリを変えない。開き直しても移した先に残る。
    #[test]
    fn move_pinned_moves_items_only_and_validates() {
        let (dir, core) = temp_core(Config::default());
        core.capture(text_entry("移す")).unwrap();
        core.pin(front_id(&core), None).unwrap();
        let meta = pinned_item(&core, 0);
        core.create_folder(None, "箱").unwrap();
        let folder = folder_id(&core, "箱");
        let blob = blob_path(&dir, &meta);

        let revision = core.read(|s| s.revision()).unwrap();
        core.move_pinned(meta.id, None).unwrap();
        assert_eq!(core.read(|s| s.revision()).unwrap(), revision, "今いる所へ移して保存した");
        assert!(matches!(core.move_pinned(folder, None), Err(OpError::NotFound)), "フォルダを移した");
        assert!(matches!(core.move_pinned(Uuid::new_v4(), Some(folder)), Err(OpError::NotFound)));
        assert!(matches!(core.move_pinned(Uuid::new_v4(), Some(Uuid::new_v4())), Err(OpError::NotFound)));
        assert!(matches!(core.move_pinned(meta.id, Some(Uuid::new_v4())), Err(OpError::TargetNotFound)));

        block_write(&dir, "pinned.toml");
        assert!(core.move_pinned(meta.id, Some(folder)).is_err());
        assert_eq!(core.read(|s| store::parent_of(&s.pinned, meta.id)), Some(Some(None)), "書けないのに移した");
        unblock_write(&dir, "pinned.toml");

        core.move_pinned(meta.id, Some(folder)).unwrap();
        assert_eq!(core.read(|s| store::parent_of(&s.pinned, meta.id)), Some(Some(Some(folder))));
        assert!(blob.exists());
        drop(core);
        let core = reopen(&dir, Config::default());
        assert_eq!(core.read(|s| store::parent_of(&s.pinned, meta.id)), Some(Some(Some(folder))));
        core.move_pinned(meta.id, None).unwrap();
        assert_eq!(core.read(|s| store::parent_of(&s.pinned, meta.id)), Some(Some(None)));
        assert_eq!(core.load_for_send(meta.id, true).unwrap().formats.len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 並べ替え: 同じ親の中で、項目もフォルダも隣と入れ替える。端では保存せず、無い ID は NotFound。
    /// 書けなければメモリを変えない。開き直しても並びが残る。
    #[test]
    fn reorder_pinned_swaps_items_and_folders_and_persists() {
        use store::Direction::{Down, Up};
        let (dir, core) = temp_core(Config::default());
        core.capture(text_entry("項目")).unwrap();
        core.pin(front_id(&core), None).unwrap();
        let item = pinned_item(&core, 0).id;
        core.create_folder(None, "箱").unwrap();
        let folder = folder_id(&core, "箱");
        let order = |core: &Core| core.read(|s| s.pinned.iter().map(PinnedNode::id).collect::<Vec<_>>()).unwrap();
        assert_eq!(order(&core), [item, folder]);

        let revision = core.read(|s| s.revision()).unwrap();
        core.reorder_pinned(item, Up).unwrap();
        core.reorder_pinned(folder, Down).unwrap();
        assert_eq!(core.read(|s| s.revision()).unwrap(), revision, "端で保存した");
        assert!(matches!(core.reorder_pinned(Uuid::new_v4(), Up), Err(OpError::NotFound)));

        block_write(&dir, "pinned.toml");
        assert!(core.reorder_pinned(folder, Up).is_err());
        assert_eq!(order(&core), [item, folder], "書けないのに並べ替えた");
        unblock_write(&dir, "pinned.toml");

        core.reorder_pinned(folder, Up).unwrap();
        assert_eq!(order(&core), [folder, item]);
        drop(core);
        let core = reopen(&dir, Config::default());
        assert_eq!(order(&core), [folder, item]);
        core.reorder_pinned(folder, Down).unwrap();
        assert_eq!(order(&core), [item, folder]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 終了時だけ保存する構成（既定）:押し出しのない取り込みではインデックスを書かず、削除は
    /// 明示的な破壊操作なので即座に書く。終了で最新が書かれる。
    #[test]
    fn save_on_exit_only_writes_index_on_delete_and_shutdown() {
        let (dir, core) = temp_core(Config::default());
        core.capture(text_entry("削除テスト1")).unwrap();
        core.capture(text_entry("削除テスト2")).unwrap();
        assert!(!dir.join("history.toml").exists(), "押し出しのない取り込みでインデックスを書いた");
        core.delete_history(front_id(&core)).unwrap();
        let index = index_text(&dir);
        assert!(index.contains("削除テスト1") && !index.contains("削除テスト2"));
        core.capture(text_entry("削除テスト3")).unwrap();
        core.shutdown(None).unwrap();
        assert!(index_text(&dir).contains("削除テスト3"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 保存するかどうかは起動したときの設定で決まり、実行中に完全メモリモードとの間で切り替えても
    /// 変わらない。保存する設定で起動 → 実行中に両方オフ: 取り込みは blob を
    /// 書き、終了時にインデックスを書く。完全メモリモードで起動 → 実行中に保存する設定: 取り込みは
    /// メモリだけで、終了時にインデックスを書かない（中身の無い項目を書かない）。
    #[test]
    fn persist_mode_is_fixed_at_startup() {
        let dir = std::env::temp_dir().join(format!("clclr-test-{}", Uuid::new_v4()));
        let config = Arc::new(RwLock::new(Config::default()));
        let core = Core::open_at(dir.clone(), Arc::clone(&config)).unwrap();
        *config.write().unwrap() = memory_only_config();
        core.capture(text_entry("保存で起動")).unwrap();
        let meta = core.read(|s| s.history.front().unwrap().meta.clone()).unwrap();
        assert!(!meta.formats.is_empty() && blob_path(&dir, &meta).exists(), "起動時の値に反して blob を書いていない");
        core.shutdown(None).unwrap();
        assert!(index_text(&dir).contains("保存で起動"), "起動時の値に反してインデックスを書いていない");
        drop(core);
        let _ = std::fs::remove_dir_all(&dir);

        let config = Arc::new(RwLock::new(memory_only_config()));
        let core = Core::open_at(dir.clone(), Arc::clone(&config)).unwrap();
        *config.write().unwrap() = Config::default();
        core.capture(text_entry("メモリで起動")).unwrap();
        let meta = core.read(|s| s.history.front().unwrap().meta.clone()).unwrap();
        assert!(meta.formats.is_empty(), "起動時の値に反して blob を書いた");
        core.shutdown(None).unwrap();
        assert!(!index_text(&dir).contains("メモリで起動"), "中身の無い項目をインデックスへ書いた");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 完全メモリモードでも、以前に保存したインデックスの参照は、blob を消す前に取り除く。
    /// 新しいメタデータは書かない。
    #[test]
    fn memory_mode_delete_removes_stale_index_ref_without_writing_new_metadata() {
        let (dir, core) = temp_core(Config::default());
        core.capture(text_entry("既存エントリ")).unwrap();
        core.shutdown(None).unwrap();
        drop(core);

        let core = reopen(&dir, memory_only_config());
        core.capture(text_entry("メモリだけの項目")).unwrap();
        let meta = core.read(|s| s.history.iter().nth(1).unwrap().meta.clone()).unwrap();
        let blob = blob_path(&dir, &meta);
        assert!(blob.exists());
        core.delete_history(meta.id).unwrap();
        assert!(!blob.exists());
        assert!(!index_text(&dir).contains("既存エントリ"));
        core.shutdown(None).unwrap();
        assert!(!index_text(&dir).contains("メモリだけの項目"), "完全メモリモードでメタデータを書いた");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 全形式が save=false（blob が無く、形式0のメタデータだけがインデックスに載った項目）でも、
    /// 完全メモリモードの削除でインデックスから消える（blob の有無で絞っていた穴の修正）。
    /// 再起動後はメモリの内容が無いので、送ろうとすると形式0として断る。
    #[test]
    fn memory_mode_delete_removes_saved_item_without_blobs() {
        let unsaved = |text: &str| {
            let mut e = text_entry(text);
            e.formats[0].format_name = "CF_TEST_UNSAVED".to_string();
            e
        };
        let (dir, core) = temp_core(Config::default());
        core.capture(unsaved("blob の無い項目")).unwrap();
        let id = front_id(&core);
        assert!(core.read(|s| s.history.front().unwrap().meta.formats.is_empty()).unwrap(), "前提: blob がある");
        core.shutdown(None).unwrap();
        drop(core);
        // プレビューの本文は CF_UNICODETEXT からしか作らないので、ID で確かめる
        assert!(index_text(&dir).contains(&id.to_string()), "前提: インデックスに載っていない");

        let core = reopen(&dir, memory_only_config());
        assert_eq!(front_id(&core), id);
        assert!(matches!(core.load_for_send(id, false), Err(OpError::Empty)));
        core.delete_history(id).unwrap();
        assert!(!index_text(&dir).contains(&id.to_string()));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// インデックスを書けなければ blob は残し、書けるようになった後の操作で消し直す。
    #[test]
    fn failed_index_update_keeps_blob_and_is_retried_later() {
        let (dir, core) = temp_core(Config::default());
        core.capture(text_entry("孤児化テスト")).unwrap();
        let meta = core.read(|s| s.history.front().unwrap().meta.clone()).unwrap();
        core.shutdown(None).unwrap();
        drop(core);
        let core = reopen(&dir, Config::default());

        block_write(&dir, "history.toml");
        // メモリからは除き、書けなかったことを返す（利用者に知らせる）
        assert!(matches!(core.delete_history(meta.id), Err(OpError::IndexNotSaved(_))));
        assert!(core.read(|s| s.history.is_empty()).unwrap());
        assert!(blob_path(&dir, &meta).exists(), "インデックスを書けないのに blob を消した");

        unblock_write(&dir, "history.toml");
        core.capture(text_entry("次の操作")).unwrap();
        assert!(!blob_path(&dir, &meta).exists(), "書けるようになっても消し直していない");
        assert!(!index_text(&dir).contains("孤児化テスト"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 消し直し（インデックスから ID を除く読み書き）だけが失敗しても、最後の全件の保存が成功すれば、その項目は
    /// インデックスに無いので blob を消して `Ok`（`Unsaved` にしない）。
    #[test]
    fn shutdown_cleans_pending_after_full_save_succeeds() {
        let (dir, core) = temp_core(Config::default());
        core.capture(text_entry("消し直し待ち")).unwrap();
        let meta = core.read(|s| s.history.front().unwrap().meta.clone()).unwrap();
        core.shutdown(None).unwrap();
        drop(core);
        let core = reopen(&dir, Config::default());
        block_write(&dir, "history.toml");
        assert!(matches!(core.delete_history(meta.id), Err(OpError::IndexNotSaved(_))));
        unblock_write(&dir, "history.toml");
        // 消し直しは history.toml を読んで ID を除くので、読めない内容にして消し直しだけを失敗させる
        std::fs::write(dir.join("history.toml"), "壊れた [内容").unwrap();
        core.shutdown(None).unwrap();
        assert!(!blob_path(&dir, &meta).exists(), "全件を保存できたのに blob を残した");
        assert!(!index_text(&dir).contains("消し直し待ち"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 起動時の切り詰めで押し出した項目も同じ後始末を通り、失敗したら最後の保存で消し直す。
    #[test]
    fn startup_trim_failure_is_retried_on_shutdown() {
        let (dir, core) = temp_core(Config::default());
        core.capture(text_entry("古い項目")).unwrap();
        core.capture(text_entry("新しい項目")).unwrap();
        core.shutdown(None).unwrap();
        drop(core);

        let mut config = memory_only_config();
        config.history.max = 1;
        config.history.grouping.enabled = false;
        block_write(&dir, "history.toml");
        let core = reopen(&dir, config);
        assert_eq!(core.read(|s| s.history.len()), Some(1));
        assert!(index_text(&dir).contains("古い項目"), "前提: 起動時の後始末が失敗していない");
        unblock_write(&dir, "history.toml");
        core.shutdown(None).unwrap();
        assert!(!index_text(&dir).contains("古い項目"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// ID で照合するので、一覧を写した後に取り込みが割り込んでも、別の項目を消さない・複製しない。
    #[test]
    fn delete_and_pin_target_correct_entry_even_after_new_capture() {
        let (dir, core) = temp_core(Config::default());
        core.capture(text_entry("旧いエントリ")).unwrap();
        let old_id = front_id(&core);
        core.capture(text_entry("新しいエントリ")).unwrap();
        core.pin(old_id, None).unwrap();
        assert_eq!(pinned_item(&core, 0).title.as_deref(), Some("旧いエントリ"));
        core.delete_history(old_id).unwrap();
        let rest = core.read(|s| s.history.front().unwrap().meta.preview.clone()).unwrap();
        assert_eq!(rest.as_deref(), Some("新しいエントリ"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 履歴のクリアは、項目・blob・インデックスの参照を消す。
    #[test]
    fn clear_history_removes_items_blobs_and_index() {
        let (dir, core) = temp_core(Config::default());
        core.capture(text_entry("クリア1")).unwrap();
        core.capture(text_entry("クリア2")).unwrap();
        let meta = core.read(|s| s.history.front().unwrap().meta.clone()).unwrap();
        core.clear_history().unwrap();
        assert!(core.read(|s| s.history.is_empty()).unwrap());
        assert!(!blob_path(&dir, &meta).exists());
        assert!(!index_text(&dir).contains("クリア"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// ピン留めは再起動しても残り、データも読める。
    #[test]
    fn pinned_tree_survives_restart() {
        let (dir, core) = temp_core(Config::default());
        core.capture(text_entry("再起動テスト")).unwrap();
        core.pin(front_id(&core), None).unwrap();
        core.shutdown(None).unwrap();
        drop(core);
        let core = reopen(&dir, Config::default());
        let meta = pinned_item(&core, 0);
        assert_eq!(meta.title.as_deref(), Some("再起動テスト"));
        assert!(!core.load_for_send(meta.id, true).unwrap().formats.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 送る・ピン留めの読み込みは厳密: blob が1つでも欠けていれば失敗する。
    #[test]
    fn load_for_send_and_pin_fail_when_a_blob_is_missing() {
        let (dir, core) = temp_core(Config::default());
        core.capture(text_entry("欠ける項目")).unwrap();
        let meta = core.read(|s| s.history.front().unwrap().meta.clone()).unwrap();
        std::fs::remove_file(blob_path(&dir, &meta)).unwrap();
        assert!(matches!(
            core.load_for_send(meta.id, false),
            Err(OpError::Storage(StorageError::MissingFormat { .. }))
        ));
        assert!(matches!(core.pin(meta.id, None), Err(OpError::Storage(StorageError::MissingFormat { .. }))));
        assert!(core.read(|s| s.pinned.is_empty()).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 取り込みが変換の途中（操作用のロックを持った状態）で止まっている間も、別のスレッドは
    /// サービスのロックを取って読める（メインスレッドが待たない）。
    #[test]
    fn read_is_possible_while_capture_is_preparing() {
        let (dir, core) = temp_core(Config::default());
        core.capture(text_entry("既存")).unwrap();
        let (entered, release) = gate(&core);
        let worker = {
            let core = core.clone();
            std::thread::spawn(move || core.capture(text_entry("変換中")).unwrap())
        };
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let started = Instant::now();
        assert_eq!(core.read(|s| s.history.len()), Some(1));
        assert!(started.elapsed() < Duration::from_secs(1), "読み取りが取り込みを待った");
        release.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(core.read(|s| s.history.len()), Some(2));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 取り込みの変換中に頼んだ削除は、取り込みが終わってから行われる。
    #[test]
    fn delete_requested_during_capture_runs_after_it() {
        let (dir, core) = temp_core(Config::default());
        core.capture(text_entry("消す項目")).unwrap();
        let target = front_id(&core);
        let (entered, release) = gate(&core);
        let capture = {
            let core = core.clone();
            std::thread::spawn(move || core.capture(text_entry("取り込み中")).unwrap())
        };
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let delete = {
            let core = core.clone();
            std::thread::spawn(move || {
                core.delete_history(target).unwrap();
                done_tx.send(()).unwrap();
            })
        };
        assert!(done_rx.recv_timeout(Duration::from_millis(200)).is_err(), "取り込みの途中で削除が進んだ");
        release.send(()).unwrap();
        capture.join().unwrap();
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        delete.join().unwrap();
        let previews = core.read(|s| s.history.iter().map(|i| i.meta.preview.clone().unwrap()).collect::<Vec<_>>());
        assert_eq!(previews.unwrap(), vec!["取り込み中".to_string()]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 終了処理は、受け付けに登録済みで操作用のロックを待っている取り込みまで終わるのを待ってから
    /// 保存する。終了処理が始まった後の取り込みは断る。
    #[test]
    fn shutdown_waits_for_admitted_operations_then_saves() {
        let (dir, core) = temp_core(Config::default());
        let (entered, release) = gate(&core);
        let first = {
            let core = core.clone();
            std::thread::spawn(move || core.capture(text_entry("一件目")).unwrap())
        };
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let second = {
            let core = core.clone();
            std::thread::spawn(move || core.capture(text_entry("二件目")).unwrap())
        };
        // 二件目が受け付けに登録されて、操作用のロックを待つまで待つ
        let until = Instant::now() + Duration::from_secs(5);
        while core.inflight() < 2 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(core.inflight(), 2);
        let (done_tx, done_rx) = mpsc::channel();
        let shutdown = {
            let core = core.clone();
            std::thread::spawn(move || done_tx.send(core.shutdown(None)).unwrap())
        };
        assert!(done_rx.recv_timeout(Duration::from_millis(200)).is_err(), "受け付け済みの操作を待たずに保存した");
        assert!(matches!(core.capture(text_entry("締め切り後")), Err(OpError::Closing)));
        release.send(()).unwrap();
        release.send(()).unwrap();
        first.join().unwrap();
        second.join().unwrap();
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        shutdown.join().unwrap();
        let index = index_text(&dir);
        assert!(index.contains("一件目") && index.contains("二件目") && !index.contains("締め切り後"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// セッション終了の上限: 受け付け済みの操作が期限までに終わらなければ、保存せずに `TimedOut`。
    #[test]
    fn shutdown_times_out_without_saving() {
        let (dir, core) = temp_core(Config::default());
        let (entered, release) = gate(&core);
        let worker = {
            let core = core.clone();
            std::thread::spawn(move || core.capture(text_entry("終わらない取り込み")).unwrap())
        };
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let started = Instant::now();
        assert!(matches!(core.shutdown(Some(Duration::from_millis(150))), Err(OpError::TimedOut)));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!dir.join("history.toml").exists(), "時間切れなのに保存した");
        release.send(()).unwrap();
        worker.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 操作の途中のパニックで操作用のロックが poison したら、以後の変更と最後の保存を断る。
    /// 受け付けの登録はパニックでも外れる（終了処理が待ち続けない）。
    #[test]
    fn panic_during_operation_stops_later_changes_and_saving() {
        let (dir, core) = temp_core(Config::default());
        core.set_hook(|point| {
            if point == "capture:prepare" {
                panic!("テスト用のパニック");
            }
        });
        let worker = {
            let core = core.clone();
            std::thread::spawn(move || {
                let _ = core.capture(text_entry("途中で落ちる"));
            })
        };
        assert!(worker.join().is_err(), "前提: パニックしていない");
        core.set_hook(|_| {});
        assert_eq!(core.inflight(), 0);
        assert!(matches!(core.capture(text_entry("次")), Err(OpError::Poisoned)));
        assert!(matches!(core.shutdown(None), Err(OpError::Poisoned)));
        assert!(!dir.join("history.toml").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}