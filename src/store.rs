//! 履歴・ピン留めアイテムの保管構造。
//!
//! 履歴（History）もピン留め（PinnedNode）もフルデータは持たず、メタデータ
//! （EntryMeta）だけをメモリに置く（遅延ロード。起動時間とメモリ常駐量を
//! 抑えるため）。永続化・blob読み込みはservice.rsがstorage.rsと接続して行う。
//!
//! ピン留め（C版の regist 相当。CLCLRでは「ピン留め」に改称）:
//! C版はchild/nextによる連結リストでツリーを表現していたが、ここではVecに置き換えている。
//! ディスク上ではフラットな`parent_id`+`order`の形（storage.rsのNodeMeta）で保存され、
//! 読み込み時にこのツリー構造へ復元される。

use std::collections::VecDeque;
use std::ops::Range;
use std::sync::Arc;

use uuid::Uuid;

use crate::config::{HistoryConfig, HistoryGroupingConfig, OverlapCheck};
use crate::data::Format;
use crate::storage::EntryMeta;

/// ピン留めツリーの各ノード（メタデータのみ。実データはblobから遅延ロード）。
/// Cloneは、`ops::Core` のピン留めの操作がツリーの写しを変えて `save_pinned` で保存し、
/// 保存できてからメモリへ反映する（失敗したらメモリを変えない）ために使う。
#[derive(Clone)]
pub enum PinnedNode {
    Item(EntryMeta),
    Folder(PinnedFolder),
}

/// ピン留めアイテムの整理用フォルダ。
#[derive(Clone)]
pub struct PinnedFolder {
    pub id: Uuid,
    pub title: String,
    pub children: Vec<PinnedNode>,
}

impl PinnedNode {
    pub fn id(&self) -> Uuid {
        match self {
            Self::Item(meta) => meta.id,
            Self::Folder(folder) => folder.id,
        }
    }
}

/// ピン留めツリーから指定フォルダ（Noneはルート）の子リストを引く。
pub fn find_children(nodes: &[PinnedNode], folder_id: Option<Uuid>) -> Option<&[PinnedNode]> {
    let Some(target) = folder_id else {
        return Some(nodes);
    };
    for node in nodes {
        if let PinnedNode::Folder(folder) = node {
            if folder.id == target {
                return Some(&folder.children);
            }
            if let Some(found) = find_children(&folder.children, Some(target)) {
                return Some(found);
            }
        }
    }
    None
}

/// ピン留めツリーから指定idのノードを取り除いて返す（削除・unpin用）。
pub fn remove_node(nodes: &mut Vec<PinnedNode>, id: Uuid) -> Option<PinnedNode> {
    if let Some(pos) = nodes.iter().position(|n| n.id() == id) {
        return Some(nodes.remove(pos));
    }
    for node in nodes {
        if let PinnedNode::Folder(folder) = node {
            if let Some(removed) = remove_node(&mut folder.children, id) {
                return Some(removed);
            }
        }
    }
    None
}

/// `find_item` の書き換えられる版（名前の変更用）。
pub fn find_item_mut(nodes: &mut [PinnedNode], id: Uuid) -> Option<&mut EntryMeta> {
    for node in nodes {
        match node {
            PinnedNode::Item(meta) if meta.id == id => return Some(meta),
            PinnedNode::Folder(folder) => {
                if let Some(found) = find_item_mut(&mut folder.children, id) {
                    return Some(found);
                }
            }
            PinnedNode::Item(_) => {}
        }
    }
    None
}

/// ピン留めツリーから指定idのアイテムのメタを引く（ペースト・プレビュー用）。
pub fn find_item(nodes: &[PinnedNode], id: Uuid) -> Option<&EntryMeta> {
    for node in nodes {
        match node {
            PinnedNode::Item(meta) if meta.id == id => return Some(meta),
            PinnedNode::Folder(folder) => {
                if let Some(found) = find_item(&folder.children, id) {
                    return Some(found);
                }
            }
            PinnedNode::Item(_) => {}
        }
    }
    None
}

/// ノード配下の全EntryMetaを集める（フォルダ削除時のblob後始末用）。
pub fn collect_item_metas(node: &PinnedNode, out: &mut Vec<EntryMeta>) {
    match node {
        PinnedNode::Item(meta) => out.push(meta.clone()),
        PinnedNode::Folder(folder) => {
            for child in &folder.children {
                collect_item_metas(child, out);
            }
        }
    }
}

/// ピン留めツリーから指定idのフォルダを引く（項目のidでは None）。
pub fn find_folder(nodes: &[PinnedNode], id: Uuid) -> Option<&PinnedFolder> {
    nodes.iter().find_map(|node| match node {
        PinnedNode::Folder(folder) if folder.id == id => Some(folder),
        PinnedNode::Folder(folder) => find_folder(&folder.children, id),
        PinnedNode::Item(_) => None,
    })
}

/// 指定のフォルダ（None はルート）の子リストを書き換え用に引く。フォルダが無ければ None。
pub fn children_mut(nodes: &mut Vec<PinnedNode>, parent: Option<Uuid>) -> Option<&mut Vec<PinnedNode>> {
    let Some(target) = parent else {
        return Some(nodes);
    };
    for node in nodes.iter_mut() {
        if let PinnedNode::Folder(folder) = node {
            if folder.id == target {
                return Some(&mut folder.children);
            }
            if let Some(found) = children_mut(&mut folder.children, parent) {
                return Some(found);
            }
        }
    }
    None
}

/// 指定idのノードの親（外の None は見つからない、内の None はルート直下）。
pub fn parent_of(nodes: &[PinnedNode], id: Uuid) -> Option<Option<Uuid>> {
    fn search(nodes: &[PinnedNode], id: Uuid, parent: Option<Uuid>) -> Option<Option<Uuid>> {
        for node in nodes {
            if node.id() == id {
                return Some(parent);
            }
            if let PinnedNode::Folder(folder) = node {
                if let Some(found) = search(&folder.children, id, Some(folder.id)) {
                    return Some(found);
                }
            }
        }
        None
    }
    search(nodes, id, None)
}

/// フォルダ名の正規化（前後の空白を除く）。作成・名前の変更・同じ名前の判定で共用する
/// （比較は正規化した後の完全一致で、大文字小文字を区別する）。
pub fn normalize_folder_name(name: &str) -> &str {
    name.trim()
}

/// `siblings` に、`except` 以外で正規化した名前が `name`（正規化済み）と同じフォルダがあるか。
pub fn has_sibling_folder_named(siblings: &[PinnedNode], name: &str, except: Option<Uuid>) -> bool {
    siblings.iter().any(|node| match node {
        PinnedNode::Folder(folder) => Some(folder.id) != except && normalize_folder_name(&folder.title) == name,
        PinnedNode::Item(_) => false,
    })
}

/// 履歴の1エントリ。フルデータは持たない。
pub struct HistoryItem {
    pub meta: EntryMeta,
    /// ディスクに書かない形式（`FormatFilter.save=false`、セッション内のみ保持）。
    /// 永続化モード自体が無効な場合は全形式がここに入る（完全メモリモード）。
    /// `Arc` で持つのは、サービスのロックの中では `Arc` の複製だけを取り、大きなデータの
    /// 写し・切り出しをロックの外で行うため。`Format` の定義は変えない
    pub resident: Arc<Vec<Format>>,
}

/// クリップボード履歴（先頭が最新）。
pub struct History {
    items: VecDeque<HistoryItem>,
}

impl History {
    pub fn new() -> Self {
        Self {
            items: VecDeque::new(),
        }
    }

    /// 永続化された履歴からの復元用。`items`は新しい順で渡す。
    pub fn from_items(items: Vec<HistoryItem>) -> Self {
        Self {
            items: items.into(),
        }
    }

    /// エントリを先頭（最新）に追加し、ポリシーで押し出されたエントリを返す
    /// （呼び出し側はこれを使ってblob等の後始末をする）。
    ///
    /// 重複チェックはコンテンツハッシュ（`EntryMeta.hash`）で行い、一致した
    /// 既存エントリは削除して新エントリで置き換える。ユーザーからはC版の
    /// 「重複時は既存項目を先頭へ移動」と同じ見え方になるが、idとタイム
    /// スタンプは新しいものに更新される。
    pub fn add(&mut self, item: HistoryItem, config: &HistoryConfig) -> Vec<HistoryItem> {
        let mut evicted = Vec::new();

        // hash=0は「ハッシュ未計算」（ハッシュを持たないファイル由来）の意味なので重複判定しない。
        // 実データのFNV-1aが0になる確率は無視できる
        let dup = |existing: &HistoryItem| {
            item.meta.hash != 0 && existing.meta.hash == item.meta.hash
        };
        match config.overlap_check {
            OverlapCheck::None => {}
            OverlapCheck::Last => {
                if self.items.front().is_some_and(&dup) {
                    evicted.extend(self.items.pop_front());
                }
            }
            OverlapCheck::All => {
                if let Some(pos) = self.items.iter().position(&dup) {
                    evicted.extend(self.items.remove(pos));
                }
            }
        }

        self.items.push_front(item);
        evicted.extend(self.trim_to_max(config));
        evicted
    }

    /// 最大件数を超えた分を末尾（最古）から押し出す。復元直後の適用にも使う。
    /// 上限は`effective_max`（階層表示が有効ならその保持総数、無効なら`max`）。
    /// 0で無制限（limit_size等、本プロジェクトの「0=無制限」の慣例に合わせる）。
    pub fn trim_to_max(&mut self, config: &HistoryConfig) -> Vec<HistoryItem> {
        let mut evicted = Vec::new();
        let max = config.effective_max() as usize;
        if max > 0 {
            while self.items.len() > max {
                evicted.extend(self.items.pop_back());
            }
        }
        evicted
    }

    pub fn front(&self) -> Option<&HistoryItem> {
        self.items.front()
    }

    /// idでエントリを引く（UIのピン留め操作用）。indexではなくidで照合するのは、
    /// UIがスナップショットを取ってから実際に操作されるまでの間にバックグラウンド
    /// 監視が新規エントリを追加すると、index基準では別のエントリを指してしまう
    /// レースを避けるため。
    pub fn get_by_id(&self, id: Uuid) -> Option<&HistoryItem> {
        self.items.iter().find(|item| item.meta.id == id)
    }

    /// idでエントリを取り除いて返す（UIの削除操作用）。id照合の理由は`get_by_id`と同じ。
    pub fn remove_by_id(&mut self, id: Uuid) -> Option<HistoryItem> {
        let pos = self.items.iter().position(|item| item.meta.id == id)?;
        self.items.remove(pos)
    }

    /// 全エントリを取り除いて返す（履歴のクリア用。呼び出し側がblobを後始末する）。
    pub fn clear(&mut self) -> Vec<HistoryItem> {
        self.items.drain(..).collect()
    }

    /// 新しい順に走査する。
    pub fn iter(&self) -> impl Iterator<Item = &HistoryItem> {
        self.items.iter()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[allow(dead_code, reason = "len と対で持つ（clippy の len_without_is_empty）。今はテストだけが使う")]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

// --- 履歴の階層表示（C版 tool_history プラグインの統合） ---

/// 階層表示の導出フォルダ1つ分。`range`は履歴リスト（新しい順）のインデックス範囲。
pub struct HistoryGroup {
    pub title: String,
    pub range: Range<usize>,
}

/// 履歴の件数と設定から階層表示の区画を導出する。
/// 戻り値は（直下に表示する件数, フォルダ列）。空になる範囲のフォルダは作らない。
///
/// C版 tool_history は履歴追加のたびに履歴ツリーへフォルダを物理的に作って
/// アイテムを移動していた（フォルダ名は固定で毎回リセット＝実質は導出値）。
/// CLCLRの履歴はフラットなので、同じ見た目を表示時の計算で得る。データを
/// 動かさないため、無効化すれば元のフラット表示に戻る。
pub fn group_history(len: usize, cfg: &HistoryGroupingConfig) -> (usize, Vec<HistoryGroup>) {
    let visible = len.min(cfg.visible_items as usize);
    let per_folder = cfg.items_per_folder as usize;
    let mut groups = Vec::new();
    if per_folder == 0 {
        return (visible, groups);
    }
    for i in 0..cfg.folders as usize {
        let start = cfg.visible_items as usize + i * per_folder;
        if start >= len {
            break;
        }
        // タイトルの番号は実件数ではなく設定上の固定範囲（C版準拠）
        groups.push(HistoryGroup {
            title: group_title(&cfg.folder_name_format, start + 1, start + per_folder),
            range: start..(start + per_folder).min(len),
        });
    }
    (visible, groups)
}

/// フォルダ名の書式展開: %1=範囲先頭の通し番号、%2=範囲末尾、%%=%。
/// 未知の%指定子は捨てる。空の書式は「先頭 - 末尾」（いずれもC版準拠）。
fn group_title(format: &str, st: usize, en: usize) -> String {
    if format.is_empty() {
        return format!("{st} - {en}");
    }
    let mut out = String::new();
    let mut chars = format.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some('1') => out.push_str(&st.to_string()),
            Some('2') => out.push_str(&en.to_string()),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 指定idを持つ最小構成のEntryMeta（ピン留めツリーのアイテムノード用）
    fn entry_meta(id: Uuid) -> EntryMeta {
        EntryMeta {
            id,
            title: None,
            modified: 0.0,
            hash: 0,
            preview: None,
            formats: Vec::new(),
        }
    }

    fn pinned_item(id: Uuid) -> PinnedNode {
        PinnedNode::Item(entry_meta(id))
    }

    fn pinned_folder(id: Uuid, children: Vec<PinnedNode>) -> PinnedNode {
        PinnedNode::Folder(PinnedFolder {
            id,
            title: "folder".to_string(),
            children,
        })
    }

    /// ルート直下にアイテムA、フォルダ（id=folder_id、直下にアイテムB、
    /// さらに子フォルダ（id=nested_folder_id、直下にアイテムC）を持つ）というツリー。
    fn nested_tree() -> (Vec<PinnedNode>, Uuid, Uuid, Uuid, Uuid, Uuid) {
        let item_a = Uuid::new_v4();
        let item_b = Uuid::new_v4();
        let item_c = Uuid::new_v4();
        let nested_folder_id = Uuid::new_v4();
        let folder_id = Uuid::new_v4();
        let tree = vec![
            pinned_item(item_a),
            pinned_folder(
                folder_id,
                vec![
                    pinned_item(item_b),
                    pinned_folder(nested_folder_id, vec![pinned_item(item_c)]),
                ],
            ),
        ];
        (tree, item_a, item_b, item_c, folder_id, nested_folder_id)
    }

    #[test]
    fn find_children_root_returns_top_level_when_folder_id_is_none() {
        let (tree, ..) = nested_tree();
        let children = find_children(&tree, None).unwrap();
        assert_eq!(children.len(), 2);
    }

    #[test]
    fn find_children_finds_nested_folder_by_id() {
        let (tree, _a, _b, item_c, folder_id, nested_folder_id) = nested_tree();
        let children = find_children(&tree, Some(nested_folder_id)).unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id(), item_c);
        // folder_id自体の子（item_bとネストフォルダ）も引ける
        assert_eq!(find_children(&tree, Some(folder_id)).unwrap().len(), 2);
    }

    #[test]
    fn find_children_returns_none_for_unknown_id() {
        let (tree, ..) = nested_tree();
        assert!(find_children(&tree, Some(Uuid::new_v4())).is_none());
    }

    #[test]
    fn remove_node_removes_top_level_item() {
        let (mut tree, item_a, ..) = nested_tree();
        let removed = remove_node(&mut tree, item_a).unwrap();
        assert_eq!(removed.id(), item_a);
        assert_eq!(tree.len(), 1);
    }

    #[test]
    fn remove_node_removes_deeply_nested_item() {
        let (mut tree, _a, _b, item_c, folder_id, nested_folder_id) = nested_tree();
        let removed = remove_node(&mut tree, item_c).unwrap();
        assert_eq!(removed.id(), item_c);
        // 兄弟・親フォルダは残る
        assert!(find_children(&tree, Some(nested_folder_id)).unwrap().is_empty());
        assert_eq!(find_children(&tree, Some(folder_id)).unwrap().len(), 2);
    }

    #[test]
    fn remove_node_can_remove_a_folder_with_its_children() {
        let (mut tree, _a, item_b, item_c, folder_id, _nested) = nested_tree();
        let removed = remove_node(&mut tree, folder_id).unwrap();
        assert_eq!(removed.id(), folder_id);
        assert_eq!(tree.len(), 1);
        // フォルダごと外れているのでツリー全体から子アイテムは見つからない
        assert!(find_item(&tree, item_b).is_none());
        assert!(find_item(&tree, item_c).is_none());
    }

    #[test]
    fn remove_node_returns_none_for_unknown_id_and_leaves_tree_intact() {
        let (mut tree, ..) = nested_tree();
        let before = tree.len();
        assert!(remove_node(&mut tree, Uuid::new_v4()).is_none());
        assert_eq!(tree.len(), before);
    }

    /// 親を調べる: ルート直下は内の None、入れ子はそのフォルダ、無ければ外の None。フォルダを探す・子の並びを
    /// 書き換え用に引く関数は、項目の ID・無い ID では None。
    #[test]
    fn parent_of_find_folder_and_children_mut() {
        let (mut tree, item_a, item_b, item_c, folder_id, nested_folder_id) = nested_tree();
        assert_eq!(parent_of(&tree, item_a), Some(None));
        assert_eq!(parent_of(&tree, item_b), Some(Some(folder_id)));
        assert_eq!(parent_of(&tree, item_c), Some(Some(nested_folder_id)));
        assert_eq!(parent_of(&tree, nested_folder_id), Some(Some(folder_id)));
        assert_eq!(parent_of(&tree, Uuid::new_v4()), None);
        assert_eq!(find_folder(&tree, nested_folder_id).map(|f| f.id), Some(nested_folder_id));
        assert!(find_folder(&tree, item_c).is_none());
        assert_eq!(children_mut(&mut tree, None).map(|c| c.len()), Some(2));
        assert_eq!(children_mut(&mut tree, Some(nested_folder_id)).map(|c| c.len()), Some(1));
        assert!(children_mut(&mut tree, Some(item_a)).is_none());
        assert!(children_mut(&mut tree, Some(Uuid::new_v4())).is_none());
    }

    /// 同じ名前の判定は前後の空白を除いて比べ、大文字小文字を区別し、除く ID は比べない。項目は数えない。
    #[test]
    fn sibling_folder_name_check_normalizes_and_skips_self() {
        let id = Uuid::new_v4();
        let siblings = vec![PinnedNode::Folder(PinnedFolder { id, title: " Work ".into(), children: vec![] })];
        assert!(has_sibling_folder_named(&siblings, "Work", None));
        assert!(!has_sibling_folder_named(&siblings, "work", None));
        assert!(!has_sibling_folder_named(&siblings, "Work", Some(id)));
        assert_eq!(normalize_folder_name("\u{3000} 名前 \t"), "名前");
    }

    #[test]
    fn find_item_finds_top_level_and_deeply_nested_items() {
        let (tree, item_a, item_b, item_c, ..) = nested_tree();
        assert_eq!(find_item(&tree, item_a).unwrap().id, item_a);
        assert_eq!(find_item(&tree, item_b).unwrap().id, item_b);
        assert_eq!(find_item(&tree, item_c).unwrap().id, item_c);
    }

    #[test]
    fn find_item_does_not_match_folder_ids() {
        let (tree, _a, _b, _c, folder_id, nested_folder_id) = nested_tree();
        assert!(find_item(&tree, folder_id).is_none());
        assert!(find_item(&tree, nested_folder_id).is_none());
    }

    #[test]
    fn find_item_returns_none_for_unknown_id() {
        let (tree, ..) = nested_tree();
        assert!(find_item(&tree, Uuid::new_v4()).is_none());
    }

    #[test]
    fn collect_item_metas_for_single_item_yields_that_item() {
        let id = Uuid::new_v4();
        let node = pinned_item(id);
        let mut out = Vec::new();
        collect_item_metas(&node, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, id);
    }

    #[test]
    fn collect_item_metas_for_folder_recurses_through_all_descendants() {
        let (tree, item_a, item_b, item_c, folder_id, _nested) = nested_tree();
        // ルートのフォルダノード（=tree[1]）配下から、孫まで含めて2件集まる
        assert_eq!(find_item(&tree, item_a).unwrap().id, item_a); // ルート直下は対象外の確認
        let folder_node = tree.into_iter().find(|n| n.id() == folder_id).unwrap();
        let mut out = Vec::new();
        collect_item_metas(&folder_node, &mut out);
        let ids: Vec<Uuid> = out.iter().map(|m| m.id).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&item_b));
        assert!(ids.contains(&item_c));
    }

    /// 指定ハッシュを持つ最小構成のHistoryItem
    fn item(hash: u64) -> HistoryItem {
        HistoryItem {
            meta: EntryMeta {
                id: Uuid::new_v4(),
                title: None,
                modified: 0.0,
                hash,
                preview: None,
                formats: Vec::new(),
            },
            resident: Arc::default(),
        }
    }

    fn config(max: u32, overlap_check: OverlapCheck) -> HistoryConfig {
        HistoryConfig {
            max,
            overlap_check,
            ..Default::default()
        }
    }

    #[test]
    fn max_evicts_oldest() {
        let cfg = config(2, OverlapCheck::None);
        let mut h = History::new();
        let first = item(1);
        let first_id = first.meta.id;
        h.add(first, &cfg);
        h.add(item(2), &cfg);
        let evicted = h.add(item(3), &cfg);

        assert_eq!(h.len(), 2);
        assert_eq!(h.front().unwrap().meta.hash, 3);
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].meta.id, first_id);
    }

    #[test]
    fn max_zero_is_unlimited() {
        let cfg = config(0, OverlapCheck::None);
        let mut h = History::new();
        for i in 1..=50 {
            h.add(item(i), &cfg);
        }
        assert_eq!(h.len(), 50);
    }

    #[test]
    fn overlap_last_replaces_front() {
        let cfg = config(10, OverlapCheck::Last);
        let mut h = History::new();
        h.add(item(7), &cfg);
        let evicted = h.add(item(7), &cfg);

        assert_eq!(h.len(), 1);
        assert_eq!(evicted.len(), 1);
    }

    #[test]
    fn overlap_last_ignores_non_adjacent_duplicate() {
        let cfg = config(10, OverlapCheck::Last);
        let mut h = History::new();
        h.add(item(7), &cfg);
        h.add(item(8), &cfg);
        let evicted = h.add(item(7), &cfg);

        assert_eq!(h.len(), 3);
        assert!(evicted.is_empty());
    }

    #[test]
    fn overlap_all_removes_duplicate_anywhere() {
        let cfg = config(10, OverlapCheck::All);
        let mut h = History::new();
        h.add(item(7), &cfg);
        h.add(item(8), &cfg);
        let evicted = h.add(item(7), &cfg);

        assert_eq!(h.len(), 2);
        assert_eq!(evicted.len(), 1);
        assert_eq!(h.front().unwrap().meta.hash, 7);
    }

    #[test]
    fn hash_zero_never_dedups() {
        // ハッシュを持たないファイル由来のhash=0同士は重複扱いしない
        let cfg = config(10, OverlapCheck::All);
        let mut h = History::new();
        h.add(item(0), &cfg);
        let evicted = h.add(item(0), &cfg);

        assert_eq!(h.len(), 2);
        assert!(evicted.is_empty());
    }

    #[test]
    fn clear_returns_all_items() {
        let cfg = config(10, OverlapCheck::None);
        let mut h = History::new();
        h.add(item(1), &cfg);
        h.add(item(2), &cfg);
        let removed = h.clear();
        assert_eq!(removed.len(), 2);
        assert!(h.is_empty());
    }

    #[test]
    fn grouping_enabled_trim_uses_derived_total() {
        let mut cfg = config(2, OverlapCheck::None);
        cfg.grouping.enabled = true;
        cfg.grouping.visible_items = 2;
        cfg.grouping.folders = 1;
        cfg.grouping.items_per_folder = 2;
        let mut h = History::new();
        for i in 1..=5 {
            h.add(item(i), &cfg);
        }
        // maxの2ではなく導出総数（2 + 1×2 = 4）で保持される
        assert_eq!(h.len(), 4);
    }

    fn grouping(visible: u32, folders: u32, per_folder: u32) -> HistoryGroupingConfig {
        HistoryGroupingConfig {
            enabled: true,
            visible_items: visible,
            folders,
            items_per_folder: per_folder,
            folder_name_format: "%1〜%2".to_string(),
        }
    }

    #[test]
    fn group_history_splits_ranges() {
        let (visible, groups) = group_history(25, &grouping(10, 5, 10));
        assert_eq!(visible, 10);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].range, 10..20);
        assert_eq!(groups[0].title, "11〜20");
        // 端数のフォルダも範囲はlenで切り、タイトルは設定上の固定範囲のまま
        assert_eq!(groups[1].range, 20..25);
        assert_eq!(groups[1].title, "21〜30");
    }

    #[test]
    fn group_history_no_groups_when_fits() {
        let (visible, groups) = group_history(7, &grouping(10, 5, 10));
        assert_eq!(visible, 7);
        assert!(groups.is_empty());
    }

    #[test]
    fn group_history_caps_at_folder_count() {
        // フォルダ数を超える履歴はトリム前の過渡状態でのみ存在し、区画には入らない
        let (visible, groups) = group_history(100, &grouping(2, 2, 3));
        assert_eq!(visible, 2);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[1].range, 5..8);
    }

    #[test]
    fn group_title_expands_placeholders() {
        assert_eq!(group_title("%1〜%2", 11, 20), "11〜20");
        assert_eq!(group_title("", 11, 20), "11 - 20");
        // %%はリテラル%、未知の%指定子は捨てる（C版準拠）
        assert_eq!(group_title("%%1=%1 %x", 3, 4), "%1=3 ");
    }

    #[test]
    fn trim_to_max_for_restore() {
        // 復元後にmaxを縮めた状況: trim_to_maxが古い方から押し出す
        let cfg = config(2, OverlapCheck::None);
        let mut h = History::from_items(vec![item(3), item(2), item(1)]);
        let evicted = h.trim_to_max(&cfg);

        assert_eq!(h.len(), 2);
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].meta.hash, 1);
        assert_eq!(h.front().unwrap().meta.hash, 3);
    }
}
