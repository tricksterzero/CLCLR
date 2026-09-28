//! ビューアの表示モデル（Win32 を使わない純粋な処理）。
//!
//! 一覧の行・ツリーの構成は、サービスのロックを取った短い間にここで作り、描画はこの写し
//! （スナップショット）だけを読む（描画中に blob を読んだりロックを取ったりしないため）。

use std::time::{SystemTime, UNIX_EPOCH};

use uuid::Uuid;

use crate::config::HistoryGroupingConfig;
use crate::service::HistoryService;
use crate::storage::EntryMeta;
use crate::menu_draw::TreeGuide;
use crate::store::{self, PinnedNode};

/// 一覧に表示する対象（ツリーの項目1つに対応する）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    History,
    /// 履歴の階層表示の導出フォルダ（`store::group_history` の結果の添字）
    HistoryGroup(usize),
    /// None はピン留めのルート、Some はそのフォルダ
    Pinned(Option<Uuid>),
}

/// エントリのデータ種別（ホットキーのメニューと共用するので、コアの `data.rs` に置いている）。
pub use crate::data::EntryKind;

/// 行の右クリックメニューに出す項目。
/// 「クリップボードへ送る」と「削除」はいつも出す。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RowMenu {
    /// 「ピン留めに追加」（履歴・階層表示のフォルダを表示しているときだけ）
    pub can_pin: bool,
    /// 「画像を関連付けで開く」「画像を書き出してフォルダで表示」（CF_DIB があるときだけ）
    pub has_image: bool,
    /// 「テキスト変換」の子メニュー（CF_UNICODETEXT があるときだけ）
    pub has_text: bool,
    /// ピン留めの入れる先（`pin_targets`。先頭はルート）。フォルダがあれば「ピン留めに追加」を入れる先を
    /// 選ぶ子メニューにし、ピン留めの行には「移動」の子メニューを出す
    pub pin_targets: Vec<PinTarget>,
    /// 行がピン留めの項目なら、今いる所（内の None はルート）。「移動」で灰色にする
    pub current: Option<Option<Uuid>>,
}

/// ピン留めの入れる先（ルートかフォルダ）。`depth` はルートが 0、ルート直下のフォルダが 1。`guide` は
/// 名前の手前に描くツリーの線（左の段から。ルートは空。メニューが行の高さいっぱいに描く）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinTarget {
    pub folder: Option<Uuid>,
    pub title: String,
    pub depth: usize,
    pub guide: Vec<TreeGuide>,
}

/// 入れる先のルートの表示名。
pub const PIN_ROOT_LABEL: &str = "ピン留め（直下）";

/// ピン留めの入れる先の並び（ロック中に呼ぶ。メタデータだけ）。先頭はルート、続けてフォルダを
/// 深さ優先の順（ツリーと同じ順）。線は、フォルダだけを兄弟として数えて引く（項目は並べないため）。
pub fn pin_targets(nodes: &[PinnedNode]) -> Vec<PinTarget> {
    fn walk(nodes: &[PinnedNode], depth: usize, lead: &[TreeGuide], out: &mut Vec<PinTarget>) {
        let folders: Vec<&store::PinnedFolder> = nodes
            .iter()
            .filter_map(|node| match node {
                PinnedNode::Folder(folder) => Some(folder),
                PinnedNode::Item(_) => None,
            })
            .collect();
        for (i, folder) in folders.iter().enumerate() {
            let last = i + 1 == folders.len();
            let mut guide = lead.to_vec();
            guide.push(if last { TreeGuide::Last } else { TreeGuide::Branch });
            out.push(PinTarget { folder: Some(folder.id), title: folder.title.clone(), depth, guide });
            let mut lead = lead.to_vec();
            lead.push(if last { TreeGuide::Blank } else { TreeGuide::Pipe });
            walk(&folder.children, depth + 1, &lead, out);
        }
    }
    let mut out = vec![PinTarget { folder: None, title: PIN_ROOT_LABEL.to_string(), depth: 0, guide: Vec::new() }];
    walk(nodes, 1, &[], &mut out);
    out
}

impl RowMenu {
    /// 形式名の一覧から作る（入れる先は空。呼び出し側が `pin_targets`・`current` を入れる）。
    pub fn from_formats<'a>(names: impl IntoIterator<Item = &'a str>, can_pin: bool) -> Self {
        let mut menu = Self { can_pin, has_image: false, has_text: false, pin_targets: Vec::new(), current: None };
        for name in names {
            match name {
                "CF_DIB" => menu.has_image = true,
                "CF_UNICODETEXT" => menu.has_text = true,
                _ => {}
            }
        }
        menu
    }
}

/// 操作の対象の行。`pinned` は、その行がピン留めの項目か（表示元の今の設定ではなく、行を作った
/// ときの表示元で決まる。検索の結果を待つ間に表示元を切り替えても、画面の行と対象がずれない）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowTarget {
    pub id: Uuid,
    pub pinned: bool,
}

/// 行に対する操作（Enter・ダブルクリック・Delete・右クリックメニュー）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowCommand {
    Send,
    /// 履歴の行をピン留めへ（内の値は入れる先。None はルート）
    Pin(Option<Uuid>),
    /// ピン留めの行を移す（入れる先。None はルート）
    Move(Option<Uuid>),
    OpenImage,
    /// 画像を書き出し、そのファイルを選んだ状態でフォルダを開く
    OpenImageLocation,
    Transform(crate::tools::text::TextTransform),
    Delete,
    /// ピン留めの行の名前の変更。ビューアが名前を聞いてから `ViewerHandler::on_rename_pinned` で
    /// 伝えるので、`on_row_command` には来ない
    Rename,
}

/// メニューバーの「ツール」の操作（履歴のクリアは、ビューアが確認を済ませてから伝える）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolCommand {
    ClearHistory,
    ClearClipboard,
    /// データのチェック
    CheckData,
    ToggleTopmost,
}

/// 一覧の1行分。
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub id: Uuid,
    /// 1行目（タイトル、なければテキストの先頭行、なければ種別の仮名）
    pub label: String,
    /// 取り込んだ時刻（UNIX 秒）。2行目の経過時間は、描くたびにここから作る（`detail`）
    pub modified: f64,
    /// 形式名の一覧（2行目の後半。「, 」でつないだもの）
    pub formats: String,
    pub kind: EntryKind,
    /// サムネイルのファイル名
    pub thumb: Option<String>,
    /// ピン留めの項目か（操作の対象をピン留めから探す。RowTarget）
    pub pinned: bool,
}

impl Row {
    /// 操作の対象。
    pub fn target(&self) -> RowTarget {
        RowTarget { id: self.id, pinned: self.pinned }
    }

    /// 2行目（経過時間 — 形式名の一覧）。`now` は UNIX 秒。形式名が無ければ空。
    pub fn detail(&self, now: f64) -> String {
        if self.formats.is_empty() {
            return String::new();
        }
        let age = relative_age_from_secs((now - self.modified).max(0.0) as u64);
        format!("{age} — {}", self.formats)
    }
}

/// テキストのプレビューがないエントリの表示名（画像・ファイル等）。
pub fn format_fallback_label(meta: &EntryMeta) -> String {
    let has = |name: &str| meta.formats.iter().any(|f| f.format_name == name);
    if has("CF_DIB") {
        "（画像）".to_string()
    } else if has("CF_HDROP") {
        "（ファイル）".to_string()
    } else {
        "（データ）".to_string()
    }
}

/// 経過時間の表示。タイムゾーンを持ち込まないため絶対時刻は出さない。
pub fn relative_age_from_secs(secs: u64) -> String {
    match secs {
        0..=59 => "たった今".to_string(),
        60..=3599 => format!("{}分前", secs / 60),
        3600..=86399 => format!("{}時間前", secs / 3600),
        _ => format!("{}日前", secs / 86400),
    }
}

/// 現在時刻（UNIX 秒）。`Row::detail` の `now` に渡す。
pub fn unix_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// `EntryMeta` から一覧の1行を作る。`extra_formats` はディスクに書かない形式
/// （`HistoryItem::resident`）の名前。ピン留めの行は、呼び出し側が `pinned` を立てる。
pub fn row_from_meta(meta: &EntryMeta, extra_formats: &[&str]) -> Row {
    let label = meta
        .title
        .as_deref()
        .or(meta.preview.as_deref())
        .map(|s| s.lines().next().unwrap_or("").to_string())
        .unwrap_or_else(|| format_fallback_label(meta));
    let names: Vec<&str> = meta
        .formats
        .iter()
        .map(|f| f.format_name.as_str())
        .chain(extra_formats.iter().copied())
        .collect();
    Row {
        id: meta.id,
        label,
        modified: meta.modified,
        formats: names.join(", "),
        kind: EntryKind::from_format_names(names.iter().copied()),
        thumb: meta.formats.iter().find_map(|f| f.thumb.clone()),
        pinned: false,
    }
}

/// 表示対象の行を作る（検索なし）。サービスのロックを取った状態で呼ぶ。
pub fn rows_for(service: &HistoryService, source: Source, grouping: &HistoryGroupingConfig) -> Vec<Row> {
    let history_row = |item: &store::HistoryItem| {
        let extra: Vec<&str> = item.resident.iter().map(|f| f.format_name.as_str()).collect();
        row_from_meta(&item.meta, &extra)
    };
    match source {
        Source::History => {
            let visible = if grouping.enabled {
                store::group_history(service.history.len(), grouping).0
            } else {
                service.history.len()
            };
            service.history.iter().take(visible).map(history_row).collect()
        }
        Source::HistoryGroup(i) => {
            let (_, groups) = store::group_history(service.history.len(), grouping);
            let range = groups.get(i).map_or(0..0, |g| g.range.clone());
            service.history.iter().skip(range.start).take(range.len()).map(history_row).collect()
        }
        Source::Pinned(folder) => store::find_children(&service.pinned, folder)
            .unwrap_or(&[])
            .iter()
            .filter_map(|node| match node {
                PinnedNode::Item(meta) => Some(Row { pinned: true, ..row_from_meta(meta, &[]) }),
                // フォルダはツリー側から辿る
                PinnedNode::Folder(_) => None,
            })
            .collect(),
    }
}

/// ツリーの右クリックメニューに出すもの（右クリックした項目ごと）。
/// 履歴の階層表示のフォルダにはメニューを出さない。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeMenu {
    /// 履歴の根: 「履歴のクリア...」（メニューバーと同じ確認と処理）
    History,
    /// ピン留めの根: 「フォルダの作成」
    PinnedRoot,
    /// ピン留めのフォルダ: 「フォルダの作成」「名前の変更」「削除...」。削除の確認に出す名前と、中の項目・
    /// フォルダの数（下のフォルダの分を含む。確認の直前に取り直す）
    PinnedFolder { id: Uuid, title: String, items: usize, folders: usize },
}

/// ツリーの項目に対する操作（確認・名前の入力はビューアが済ませてからハンドラへ伝える）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeCommand {
    /// ピン留めのフォルダを中身ごと消す
    DeleteFolder(Uuid),
    /// `parent`（None はピン留めの根）の中にフォルダを作る（名前は前後の空白を除いて空でない）
    CreateFolder { parent: Option<Uuid>, title: String },
    /// ピン留めのフォルダの名前を変える（名前は前後の空白を除いて空でない）
    RenameFolder { id: Uuid, title: String },
}

/// ピン留めのフォルダの右クリックメニューを作る（ロック中に呼ぶ。メタデータを数えるだけ）。
/// 見つからなければ None。
pub fn pinned_folder_menu(nodes: &[PinnedNode], id: Uuid) -> Option<TreeMenu> {
    let folder = store::find_folder(nodes, id)?;
    let (items, folders) = count_nodes(&folder.children);
    Some(TreeMenu::PinnedFolder { id, title: folder.title.clone(), items, folders })
}

/// 項目の数とフォルダの数（下のフォルダの分を含む）。
fn count_nodes(nodes: &[PinnedNode]) -> (usize, usize) {
    nodes.iter().fold((0, 0), |(items, folders), node| match node {
        PinnedNode::Item(_) => (items + 1, folders),
        PinnedNode::Folder(folder) => {
            let (i, f) = count_nodes(&folder.children);
            (items + i, folders + 1 + f)
        }
    })
}

/// ピン留めのフォルダの削除の確認の文言（中身があれば、一緒に消えるものの数を添える）。
pub fn delete_folder_message(title: &str, items: usize, folders: usize) -> String {
    let inside = match (items, folders) {
        (0, 0) => String::new(),
        (i, 0) => format!("中にある項目 {i} 件も削除されます。"),
        (0, f) => format!("中にあるフォルダ {f} 個も削除されます。"),
        (i, f) => format!("中にある項目 {i} 件とフォルダ {f} 個も削除されます。"),
    };
    format!("フォルダ「{title}」を削除します。{inside}よろしいですか？")
}

/// ツリーの「履歴」の表示名（件数はビューアが添える）。
pub const HISTORY_LABEL: &str = "履歴";

/// ツリーの1項目。
#[derive(Clone, Debug, PartialEq)]
pub struct TreeNode {
    pub label: String,
    pub source: Source,
    pub children: Vec<TreeNode>,
}

/// ツリーの構成を作る: 「履歴」（階層表示が有効なら導出フォルダを子に持つ）と
/// 「ピン留め」（入れ子のフォルダを子に持つ）。
pub fn build_tree(history_len: usize, grouping: &HistoryGroupingConfig, pinned: &[PinnedNode]) -> Vec<TreeNode> {
    let history_children = if grouping.enabled {
        store::group_history(history_len, grouping)
            .1
            .into_iter()
            .enumerate()
            .map(|(i, g)| TreeNode { label: g.title, source: Source::HistoryGroup(i), children: Vec::new() })
            .collect()
    } else {
        Vec::new()
    };
    vec![
        TreeNode { label: HISTORY_LABEL.to_string(), source: Source::History, children: history_children },
        TreeNode { label: "ピン留め".to_string(), source: Source::Pinned(None), children: pinned_folders(pinned) },
    ]
}

fn pinned_folders(nodes: &[PinnedNode]) -> Vec<TreeNode> {
    nodes
        .iter()
        .filter_map(|node| match node {
            PinnedNode::Folder(folder) => Some(TreeNode {
                label: folder.title.clone(),
                source: Source::Pinned(Some(folder.id)),
                children: pinned_folders(&folder.children),
            }),
            PinnedNode::Item(_) => None,
        })
        .collect()
}

/// 行の並びが変わったあと、前に選んでいた項目の新しい位置を探す（選択は UUID で保つ）。
pub fn index_of(rows: &[Row], id: Uuid) -> Option<usize> {
    rows.iter().position(|r| r.id == id)
}

/// 検索の一致判定。対象はタイトルとテキスト全文（C版 tool_find と同じくテキスト形式のみ）で、
/// 大文字小文字を区別しない。`needle` は `search_needle`、`text_folded` は `search_fold` の結果を渡す。
pub fn search_matches(needle: &str, title: Option<&str>, text_folded: Option<&str>) -> bool {
    title.is_some_and(|t| search_fold(t).contains(needle)) || text_folded.is_some_and(|t| t.contains(needle))
}

/// 検索のための小文字化。1文字ずつ `char::to_lowercase` する（`str::to_lowercase` は語末のシグマだけ
/// 前後を見て変えるので、1文字ずつ照合する `folded_chars_contain` と結果がずれる。検索文字列・
/// タイトル・全文をすべてこの規則にそろえる）。
pub fn search_fold(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

/// 検索欄の入力から、一致判定に使う文字列を作る（前後の空白を除いて小文字化）。空なら絞り込まない。
pub fn search_needle(input: &str) -> String {
    search_fold(input.trim())
}

/// `chars`（元の文字）を1文字ずつ小文字化しながら、`needle`（`search_needle` の結果）を含むかを
/// 調べる。`search_fold` した全文に `contains` するのと同じ結果で、全文の写しを作らない（メモリだけに
/// 持つ全文の検索）。照合は KMP 法（戻らずに1回だけ読む）。
pub fn folded_chars_contain(chars: impl Iterator<Item = char>, needle: &str) -> bool {
    let pattern: Vec<char> = needle.chars().collect();
    if pattern.is_empty() {
        return true;
    }
    // failure[i]: pattern[..=i] の、真の接頭辞かつ接尾辞である最長の長さ
    let mut failure = vec![0usize; pattern.len()];
    let mut k = 0;
    for i in 1..pattern.len() {
        while k > 0 && pattern[i] != pattern[k] {
            k = failure[k - 1];
        }
        if pattern[i] == pattern[k] {
            k += 1;
        }
        failure[i] = k;
    }
    let mut matched = 0;
    for c in chars.flat_map(char::to_lowercase) {
        while matched > 0 && c != pattern[matched] {
            matched = failure[matched - 1];
        }
        if c == pattern[matched] {
            matched += 1;
            if matched == pattern.len() {
                return true;
            }
        }
    }
    false
}

/// プレビューに出すテキストの上限（UTF-16 の単位数）。巨大なテキストは先頭だけを表示する。
pub const PREVIEW_MAX_UNITS: usize = 65_536;

/// プレビューの EDIT に入れる文字列を作る。EDIT は CRLF でしか改行しないため、LF・CR 単独の
/// 改行を CRLF にそろえる。`truncated` なら、先頭だけであることを末尾に書き添える。
pub fn preview_text(text: &str, truncated: bool, total_units: usize) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str("\r\n");
            }
            '\n' => out.push_str("\r\n"),
            _ => out.push(c),
        }
    }
    if truncated {
        out.push_str(&format!(
            "\r\n\r\n（先頭の {PREVIEW_MAX_UNITS} 文字だけを表示しています。全体は約 {total_units} 文字）"
        ));
    }
    out
}

/// ファイル一覧（CF_HDROP）のプレビューの文字列。パスを1行ずつ並べ、`truncated`（一覧の先頭だけを
/// 読んだ）なら、先頭の件数だけであることを書き添える。
pub fn preview_paths(paths: &[String], truncated: bool) -> String {
    let mut out = paths.join("\r\n");
    if truncated {
        out.push_str(&format!("\r\n\r\n（一覧が大きいため、先頭の {} 件だけを表示しています）", paths.len()));
    }
    out
}

/// プレビューに出すものが無いとき（選んでいない、プレビューできる形式が無い、読めない）の文字列。
pub const NO_PREVIEW: &str = "（プレビューなし）";

/// 画像が大きく（`images::MAX_IMAGE_BYTES` を超える）、プレビューを省略したときの文字列。
pub fn image_too_large_note(width: u32, height: u32) -> String {
    format!("（画像が大きいため、プレビューを省略しました。{width} × {height} ピクセルの画像です）")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::FormatMeta;
    use crate::store::PinnedFolder;

    fn meta(title: Option<&str>, preview: Option<&str>, formats: &[&str], modified: f64) -> EntryMeta {
        EntryMeta {
            id: Uuid::new_v4(),
            title: title.map(str::to_string),
            modified,
            hash: 0,
            preview: preview.map(str::to_string),
            formats: formats
                .iter()
                .map(|n| FormatMeta {
                    format_name: n.to_string(),
                    format_id: 0,
                    blob: String::new(),
                    size: 0,
                    thumb: None,
                })
                .collect(),
        }
    }

    #[test]
    fn relative_age_from_secs_boundaries() {
        assert_eq!(relative_age_from_secs(0), "たった今");
        assert_eq!(relative_age_from_secs(59), "たった今");
        assert_eq!(relative_age_from_secs(60), "1分前");
        assert_eq!(relative_age_from_secs(3599), "59分前");
        assert_eq!(relative_age_from_secs(3600), "1時間前");
        assert_eq!(relative_age_from_secs(86399), "23時間前");
        assert_eq!(relative_age_from_secs(86400), "1日前");
        assert_eq!(relative_age_from_secs(86400 * 3), "3日前");
    }

    #[test]
    fn row_label_prefers_title_then_first_line_of_preview_then_fallback() {
        let m = meta(Some("題名"), Some("本文"), &["CF_UNICODETEXT"], 0.0);
        assert_eq!(row_from_meta(&m, &[]).label, "題名");
        let m = meta(None, Some("一行目\r\n二行目"), &["CF_UNICODETEXT"], 0.0);
        assert_eq!(row_from_meta(&m, &[]).label, "一行目");
        let m = meta(None, None, &["CF_DIB"], 0.0);
        assert_eq!(row_from_meta(&m, &[]).label, "（画像）");
        let m = meta(None, None, &["CF_HDROP"], 0.0);
        assert_eq!(row_from_meta(&m, &[]).label, "（ファイル）");
    }

    #[test]
    fn row_detail_lists_age_and_formats_including_resident() {
        let m = meta(None, Some("x"), &["CF_UNICODETEXT"], 1000.0);
        let row = row_from_meta(&m, &["HTML Format"]);
        assert_eq!(row.detail(1000.0 + 120.0), "2分前 — CF_UNICODETEXT, HTML Format");
        assert_eq!(row.kind, EntryKind::Text);
    }

    /// 2行目の経過時間は、同じ行でも渡した時刻で変わる（描くたびに作るため、表示したままでも
    /// 古くならない）。形式名が無い行は2行目が空。
    #[test]
    fn row_detail_follows_the_given_time() {
        let row = row_from_meta(&meta(None, Some("x"), &["CF_UNICODETEXT"], 1000.0), &[]);
        assert_eq!(row.detail(1000.0 + 30.0), "たった今 — CF_UNICODETEXT");
        assert_eq!(row.detail(1000.0 + 3600.0 * 2.0), "2時間前 — CF_UNICODETEXT");
        let no_formats = Row { formats: String::new(), ..row };
        assert_eq!(no_formats.detail(5000.0), "");
    }

    #[test]
    fn build_tree_adds_history_groups_only_when_enabled_and_nests_pinned_folders() {
        let inner = PinnedNode::Folder(PinnedFolder { id: Uuid::new_v4(), title: "内".into(), children: vec![] });
        let outer_id = Uuid::new_v4();
        let pinned = vec![
            PinnedNode::Item(meta(None, Some("a"), &["CF_UNICODETEXT"], 0.0)),
            PinnedNode::Folder(PinnedFolder { id: outer_id, title: "外".into(), children: vec![inner] }),
        ];
        let mut grouping = HistoryGroupingConfig::default();
        grouping.enabled = false;
        let tree = build_tree(100, &grouping, &pinned);
        assert!(tree[0].children.is_empty());
        assert_eq!(tree[1].children.len(), 1);
        assert_eq!(tree[1].children[0].source, Source::Pinned(Some(outer_id)));
        assert_eq!(tree[1].children[0].children[0].label, "内");

        grouping.enabled = true; // 既定: 直下10件 + 10件のフォルダ5つ
        let tree = build_tree(100, &grouping, &pinned);
        assert_eq!(tree[0].children.len(), 5);
        assert_eq!(tree[0].children[2].source, Source::HistoryGroup(2));
    }

    /// ピン留めのフォルダのメニューは、入れ子の中のフォルダも名前で見つけ、中の項目・フォルダを
    /// 下の階層まで数える。無いフォルダ（項目の ID を含む）には出さない。
    #[test]
    fn pinned_folder_menu_finds_nested_folder_and_counts_contents() {
        let item = |t: &str| PinnedNode::Item(meta(None, Some(t), &["CF_UNICODETEXT"], 0.0));
        let empty_id = Uuid::new_v4();
        let inner_id = Uuid::new_v4();
        let outer_id = Uuid::new_v4();
        let loose = item("x");
        let loose_id = loose.id();
        let inner = PinnedNode::Folder(PinnedFolder {
            id: inner_id,
            title: "内".into(),
            children: vec![item("a"), PinnedNode::Folder(PinnedFolder { id: empty_id, title: "空".into(), children: vec![] })],
        });
        let pinned = vec![
            loose,
            PinnedNode::Folder(PinnedFolder { id: outer_id, title: "外".into(), children: vec![item("b"), inner, item("c")] }),
        ];
        assert_eq!(
            pinned_folder_menu(&pinned, outer_id),
            Some(TreeMenu::PinnedFolder { id: outer_id, title: "外".into(), items: 3, folders: 2 })
        );
        assert_eq!(
            pinned_folder_menu(&pinned, inner_id),
            Some(TreeMenu::PinnedFolder { id: inner_id, title: "内".into(), items: 1, folders: 1 })
        );
        assert_eq!(
            pinned_folder_menu(&pinned, empty_id),
            Some(TreeMenu::PinnedFolder { id: empty_id, title: "空".into(), items: 0, folders: 0 })
        );
        assert_eq!(pinned_folder_menu(&pinned, loose_id), None);
        assert_eq!(pinned_folder_menu(&pinned, Uuid::new_v4()), None);
    }

    /// 入れる先の並び: 先頭はルート（深さ 0）、続けてフォルダを深さ優先（ツリーと同じ順）で、項目は含めない。
    #[test]
    fn pin_targets_list_root_then_folders_depth_first() {
        let item = |t: &str| PinnedNode::Item(meta(None, Some(t), &["CF_UNICODETEXT"], 0.0));
        let (a, a1, b) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let pinned = vec![
            item("x"),
            PinnedNode::Folder(PinnedFolder {
                id: a,
                title: "A".into(),
                children: vec![item("y"), PinnedNode::Folder(PinnedFolder { id: a1, title: "A1".into(), children: vec![] })],
            }),
            PinnedNode::Folder(PinnedFolder { id: b, title: "B".into(), children: vec![] }),
        ];
        let listed: Vec<(Option<Uuid>, String, usize)> =
            pin_targets(&pinned).into_iter().map(|t| (t.folder, t.title, t.depth)).collect();
        assert_eq!(
            listed,
            [
                (None, PIN_ROOT_LABEL.to_string(), 0),
                (Some(a), "A".to_string(), 1),
                (Some(a1), "A1".to_string(), 2),
                (Some(b), "B".to_string(), 1),
            ]
        );
    }

    /// ツリーの線: 最後の兄弟は └、それ以外は ├。続きのある段は │、無い段は全角の空白。兄弟に数えるのは
    /// フォルダだけ（最後のフォルダの後ろに項目があっても └）。
    #[test]
    fn pin_targets_draw_tree_guides() {
        let item = |t: &str| PinnedNode::Item(meta(None, Some(t), &["CF_UNICODETEXT"], 0.0));
        let folder = |title: &str, children: Vec<PinnedNode>| {
            PinnedNode::Folder(PinnedFolder { id: Uuid::new_v4(), title: title.into(), children })
        };
        let pinned = vec![
            folder("仕事", vec![folder("テンプレート", vec![]), folder("下書き", vec![folder("古い", vec![])])]),
            folder("空のフォルダ", vec![folder("中", vec![]), item("y")]),
            item("x"),
        ];
        // 読みやすさのため、段を罫線の文字（空白は「・」）にして比べる
        let draw = |g: &TreeGuide| match g {
            TreeGuide::Pipe => '│',
            TreeGuide::Blank => '・',
            TreeGuide::Branch => '├',
            TreeGuide::Last => '└',
        };
        let lines: Vec<String> = pin_targets(&pinned)
            .into_iter()
            .map(|t| format!("{}{}", t.guide.iter().map(draw).collect::<String>(), t.title))
            .collect();
        assert_eq!(lines, [PIN_ROOT_LABEL, "├仕事", "│├テンプレート", "│└下書き", "│・└古い", "└空のフォルダ", "・└中"]);
    }

    #[test]
    fn delete_folder_message_mentions_contents_only_when_present() {
        assert_eq!(delete_folder_message("A", 0, 0), "フォルダ「A」を削除します。よろしいですか？");
        assert_eq!(
            delete_folder_message("A", 3, 0),
            "フォルダ「A」を削除します。中にある項目 3 件も削除されます。よろしいですか？"
        );
        assert_eq!(
            delete_folder_message("A", 0, 2),
            "フォルダ「A」を削除します。中にあるフォルダ 2 個も削除されます。よろしいですか？"
        );
        assert_eq!(
            delete_folder_message("A", 3, 2),
            "フォルダ「A」を削除します。中にある項目 3 件とフォルダ 2 個も削除されます。よろしいですか？"
        );
    }

    #[test]
    fn search_matches_title_and_text_case_insensitively() {
        let needle = search_needle("  RuSt ");
        assert_eq!(needle, "rust");
        assert!(search_matches(&needle, Some("Learning Rust"), None));
        assert!(search_matches(&needle, None, Some("about rust code")));
        assert!(!search_matches(&needle, Some("python"), Some("go")));
        assert!(!search_matches(&needle, None, None));
    }

    /// 1文字ずつ小文字化しながらの照合は、小文字化した全文に `contains` するのと同じ結果になる
    /// （重なりのある繰り返し、語末のシグマ、小文字化で2文字になる文字、全角、空の検索文字列）。
    #[test]
    fn folded_chars_contain_matches_folded_contains() {
        let texts = ["aaab", "abababc", "ΟΔΟΣ ΟΔΟΣ", "İstanbul", "クリップボードCLIPBOARD", "", "abc"];
        let needles = ["aab", "ababc", "abac", "σ", "οδοσ ", "i̇s", "i", "ボードclip", "abcd", "", "x"];
        for text in texts {
            for input in needles {
                let needle = search_needle(input);
                assert_eq!(
                    folded_chars_contain(text.chars(), &needle),
                    search_fold(text).contains(&needle),
                    "text={text:?} needle={needle:?}"
                );
            }
        }
        // 語末のシグマも 1文字ずつの規則でそろえるので、検索文字列の σ で見つかる
        assert!(search_matches(&search_needle("σ"), Some("ΟΔΟΣ"), None));
    }

    #[test]
    fn preview_text_normalizes_newlines_to_crlf() {
        assert_eq!(preview_text("a\nb\r\nc\rd", false, 0), "a\r\nb\r\nc\r\nd");
    }

    #[test]
    fn preview_text_appends_note_only_when_truncated() {
        assert_eq!(preview_text("abc", false, 3), "abc");
        let t = preview_text("abc", true, 100_000);
        assert!(t.starts_with("abc\r\n\r\n（先頭の 65536 文字"));
        assert!(t.contains("約 100000 文字"));
    }

    #[test]
    fn index_of_finds_row_by_id_after_reorder() {
        let a = row_from_meta(&meta(None, Some("a"), &[], 0.0), &[]);
        let b = row_from_meta(&meta(None, Some("b"), &[], 0.0), &[]);
        let new = row_from_meta(&meta(None, Some("new"), &[], 0.0), &[]);
        let rows = vec![new, a.clone(), b];
        assert_eq!(index_of(&rows, a.id), Some(1));
        assert_eq!(index_of(&rows, Uuid::new_v4()), None);
    }
}
