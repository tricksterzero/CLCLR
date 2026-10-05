//! 設定の読み書き（TOML）。
//!
//! フィールド名・意味はC版CLCL（Ini.h/Ini.c）を踏襲しつつ、CLCLR が使う項目だけを持つ。
//! 設定画面（`native/settings.rs`）で編集する項目は `validate` で範囲を確かめる。ファイルを読むとき
//! （`load`）は範囲を確かめないので、使う側は手で書いた極端な値でも壊れないようにする。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// --- Config ---

/// アプリ全体の設定。`#[serde(default)]`によりTOMLに存在しないフィールドは
/// `Default::default()`（下記の各Defaultで定義した値）で補われるため、
/// 設定ファイルは変更したい項目だけを書けばよい。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub history: HistoryConfig,
    pub hotkey: HotkeyConfig,
    /// コマンド型ツール（tools/）の設定。サブモジュールと同じ構造で対応させる
    pub tools: ToolsConfig,
    /// `format_filters`に列挙されていない形式に適用されるデフォルト動作。
    /// `Ignore`にすることでホワイトリスト方式（明示的に許可した形式のみ捕捉）になる。
    pub format_filter_default: FilterAction,
    pub format_filters: Vec<FormatFilter>,
    /// 1回のコピーで取り込む形式（形式フィルタで取り込む対象になり、形式ごとの上限も通ったもの）の大きさの
    /// 合計の上限（バイト）。超えたコピーは何も取り込まず、知らせる。0は無制限。以前の版の設定ファイルには
    /// 無いので、既定（`DEFAULT_CAPTURE_TOTAL_LIMIT`）が効く
    pub capture_total_limit: u64,
    pub window_filters: Vec<WindowFilter>,
}

/// コマンド型ツールの設定。`tools/`のサブモジュール（作用対象のデータ種別）ごとに
/// 節を分ける（将来の画像操作は `image` を追加）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    pub text: TextToolsConfig,
}

/// テキスト変換ツール（tools/text.rs）の設定。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TextToolsConfig {
    /// 引用/引用解除で行頭に付ける文字列
    pub quote_char: String,
    /// 挟み込みの前側/後側の文字列
    pub put_text_open: String,
    pub put_text_close: String,
    /// 改行の除去で2行目以降の行頭空白（折り返しインデント）も除去するか
    pub delete_crlf_trim_leading: bool,
    /// テキスト整形の折り返し幅（半角換算）
    pub word_break_width: u32,
    /// ピン留めアイテムの送出時に %d/%t を日時へ自動変換するか
    pub convert_date_on_send: bool,
    /// 日付・時刻の書式（GetDateFormatW/GetTimeFormatW形式。空はロケール既定）
    pub date_format: String,
    pub time_format: String,
}

impl Default for TextToolsConfig {
    fn default() -> Self {
        // 引用符・挟み込み・折り返し幅の既定値はC版tool_textと同じ
        Self {
            quote_char: ">".to_string(),
            put_text_open: "<TAG>".to_string(),
            put_text_close: "</TAG>".to_string(),
            delete_crlf_trim_leading: true,
            word_break_width: 80,
            convert_date_on_send: false,
            date_format: String::new(),
            time_format: String::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    /// クリップボード監視のON/OFF（`AddClipboardFormatListener`の登録/解除に対応）
    pub clipboard_watch: bool,
    pub show_trayicon: bool,
    /// ビューアを常に最前面に表示する（C版 CLCL の tool_utl「最前面に表示」の統合。
    /// ビューアのツールメニューから切替、次回起動にも引き継ぐ）
    pub viewer_always_on_top: bool,
    /// 起動時にビューアを表示しない（トレイ常駐で開始。C版の使用感）。
    /// `show_trayicon`が無効の場合は到達手段を失わないよう無視して表示起動する
    pub start_hidden: bool,
    /// 起動時のクリップボード同期: 空なら履歴の最新を復元、データがあれば
    /// 履歴へ取り込む（取り込みは`clipboard_watch`有効時のみ）
    pub startup_clipboard_sync: bool,
    /// ビューアの操作（送る・テキスト変換・関連付けで開く）が失敗したとき、メッセージボックスで
    /// 知らせる（false ならログだけ）
    pub notify_action_errors: bool,
    /// 起動時に、保存先のフォルダ（exe のフォルダ）の権限を確かめ、ほかのアカウントから読み書きできそうなら警告する
    /// （`folder_security`）。既定は true（`GeneralConfig::default`。項目の無い以前の設定ファイルも true になる）
    pub check_folder_permissions: bool,
    /// ビューアの前回のウィンドウサイズ（論理px）。ウィンドウを隠す時・終了時に
    /// 更新し、次回起動で復元する。最小化・最大化中の値は記録しない
    pub viewer_width: u32,
    pub viewer_height: u32,
    /// 設定画面の前回のサイズ（論理px）。今の設定画面は大きさが固定で使わないが、設定ファイルの互換のため、
    /// 名前・型・既定値は変えずに残す
    pub settings_width: u32,
    pub settings_height: u32,
}

/// ビューアのクライアント領域の大きさの下限（論理px）。窓の最小の大きさ（`WM_GETMINMAXINFO`）と、
/// 設定ファイルから読んだ保存サイズの補正の両方で使う
pub const VIEWER_MIN_WIDTH: u32 = 400;
pub const VIEWER_MIN_HEIGHT: u32 = 300;

/// 設定画面のウィンドウサイズの下限（論理px）。
pub const SETTINGS_MIN_WIDTH: u32 = 300;
pub const SETTINGS_MIN_HEIGHT: u32 = 200;

impl GeneralConfig {
    /// 設定画面の初期サイズ（論理px）。手編集などで下限を下回る値が入っていても
    /// 使えなくならないよう下限へ補正する
    #[allow(dead_code, reason = "今の設定画面は大きさが固定で使わない（設定ファイルの互換のため残す）")]
    pub fn settings_size(&self) -> [u32; 2] {
        [
            self.settings_width.max(SETTINGS_MIN_WIDTH),
            self.settings_height.max(SETTINGS_MIN_HEIGHT),
        ]
    }

    /// 起動時に使うビューアのウィンドウサイズ。手編集などで下限を下回る値が
    /// 入っていても、ウィンドウが使えなくならないよう下限へ補正する
    pub fn viewer_size(&self) -> [f32; 2] {
        [
            self.viewer_width.max(VIEWER_MIN_WIDTH) as f32,
            self.viewer_height.max(VIEWER_MIN_HEIGHT) as f32,
        ]
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeyConfig {
    /// ポップアップメニュー表示のホットキー（C版デフォルト: Alt+C）
    pub popup_menu: Hotkey,
    /// メニュー選択後、元のアクティブウィンドウへ自動ペーストする（C版 ai->paste 相当）
    pub auto_paste: bool,
    /// ポップアップメニューに出す履歴の最大件数
    pub menu_max_items: u32,
    /// ポップアップメニューで「ピン留め(&P)」の子メニューを履歴より上に出す（既定は履歴が上。
    /// 履歴とピン留めの両方を出すメニューだけに効く。ユーザー要望 2026-09-26）
    pub menu_pinned_first: bool,
    /// 修飾キー二度押し（Ctrl×2等）のアクション。既定はすべて無効（C版準拠）。
    /// いずれかが有効な間だけ低レベルキーフック（WH_KEYBOARD_LL）を張る
    pub double_press_ctrl: DoublePressAction,
    pub double_press_shift: DoublePressAction,
    pub double_press_alt: DoublePressAction,
    /// ポップアップメニュー項目のツールチップ（`[hotkey.tooltip]`）
    pub tooltip: MenuTooltipConfig,
}

/// ポップアップメニュー項目にマウスを置く／キーで選ぶと、全文または先頭部分を
/// 別窓で表示する機能の設定。C版の`menu_show_tooltip`・`tooltip_show_delay`・
/// `menu_tooltip_size`に相当し、行数上限はCLCLR独自。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MenuTooltipConfig {
    pub enabled: bool,
    /// 項目を選んでから表示するまでの待ち時間（ミリ秒）
    pub delay_ms: u32,
    /// テキストの表示文字数の上限（超過分は「…」で省略）
    pub max_chars: u32,
    /// テキストの表示行数の上限（超過分は「…」で省略）
    pub max_lines: u32,
}

impl Default for MenuTooltipConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            delay_ms: 500,
            max_chars: 1024,
            max_lines: 20,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DoublePressAction {
    None,
    Menu,
    /// ピン留めだけのメニュー（ルートの中身を直に並べる）
    #[serde(rename = "menu_pinned")]
    MenuPinned,
    /// 履歴だけのメニュー（「ピン留め(&P)」の子メニューを付けない）
    #[serde(rename = "menu_history")]
    MenuHistory,
    Viewer,
}

impl Default for DoublePressAction {
    fn default() -> Self {
        Self::None
    }
}

/// 1つのホットキー割当。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkey {
    pub enabled: bool,
    /// "alt" / "ctrl" / "shift" / "win" の組合せ
    pub modifiers: Vec<String>,
    /// キー（英数字1文字。例: "C"）
    pub key: String,
}

impl Default for Hotkey {
    fn default() -> Self {
        Self {
            enabled: false,
            modifiers: Vec::new(),
            key: String::new(),
        }
    }
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            popup_menu: Hotkey {
                enabled: true,
                modifiers: vec!["alt".to_string()],
                key: "C".to_string(),
            },
            auto_paste: true,
            menu_max_items: 20,
            menu_pinned_first: false,
            double_press_ctrl: DoublePressAction::None,
            double_press_shift: DoublePressAction::None,
            double_press_alt: DoublePressAction::None,
            tooltip: MenuTooltipConfig::default(),
        }
    }
}

impl HotkeyConfig {
    /// 二度押しアクションが1つでも有効か（キーフックを張るかの判定）。
    pub fn any_double_press(&self) -> bool {
        [
            self.double_press_ctrl,
            self.double_press_shift,
            self.double_press_alt,
        ]
        .iter()
        .any(|a| *a != DoublePressAction::None)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryConfig {
    /// 履歴として保持する最大件数
    pub max: u32,
    pub overlap_check: OverlapCheck,
    /// クリップボード変更を検知してから実際に履歴へ追加するまでの遅延（デバウンス）
    pub add_interval_ms: u64,
    /// アプリ終了時に履歴を保存するか
    pub save_on_exit: bool,
    /// 変更の都度、常に履歴を保存するか（trueならsave_on_exitと無関係に毎回保存）
    pub save_on_change: bool,
    /// ピン留めアイテムをクリップボードへ貼り付けた時、その変更を履歴に追加しないか。
    /// `clipboard::set_clipboard_suppressed`の抑止（書いた変更の番号を記録して取り込まない）と対になる設定。
    pub delete_on_send: bool,
    /// 履歴にアイテムが追加された時に音を鳴らす（C版 CLCL の tool_utl「音を鳴らす」の統合）
    pub sound_on_add: bool,
    /// 追加時に鳴らすWAVファイルのパス（空はシステム音）
    pub sound_file: String,
    /// 履歴の階層表示（C版 tool_history プラグインの統合）
    pub grouping: HistoryGroupingConfig,
}

/// 履歴の階層表示: 古い履歴を「11〜20」のような範囲フォルダにまとめて表示する。
/// C版 tool_history と違い履歴データ自体は組み替えず、表示時に導出する
/// （`store::group_history`）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryGroupingConfig {
    pub enabled: bool,
    /// フォルダに入れず直下に表示する件数
    pub visible_items: u32,
    /// フォルダ数
    pub folders: u32,
    /// フォルダ1つあたりの件数
    pub items_per_folder: u32,
    /// フォルダ名の書式（%1=範囲先頭の通し番号、%2=範囲末尾、%%=%。空は"先頭 - 末尾"）
    pub folder_name_format: String,
}

impl Default for HistoryGroupingConfig {
    fn default() -> Self {
        // 数値・書式ともC版tool_historyの既定値
        Self {
            enabled: false,
            visible_items: 10,
            folders: 5,
            items_per_folder: 10,
            folder_name_format: "%1〜%2".to_string(),
        }
    }
}

impl HistoryGroupingConfig {
    /// 階層表示が保持する総件数（直下 + フォルダ数 × フォルダ内件数）。設定画面の範囲では収まるが、手で書いた
    /// 設定ファイルは読むときに範囲を確かめないので、桁あふれは上限で止める（回り込むと小さな上限で起動時の
    /// 切り詰めが履歴を消す）。
    pub fn total(&self) -> u32 {
        self.visible_items.saturating_add(self.folders.saturating_mul(self.items_per_folder))
    }
}

impl HistoryConfig {
    /// 履歴の実効保持件数（0=無制限）。階層表示が有効な間はその保持総数が`max`より
    /// 優先される。C版では本体の「履歴に残す件数」を手動で整合させる必要があり、
    /// 少なすぎるとフォルダが消える罠があった（tool_history readme記載）。導出に
    /// することでこの不整合を構造的に無くす
    pub fn effective_max(&self) -> u32 {
        if self.grouping.enabled {
            self.grouping.total()
        } else {
            self.max
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FormatFilter {
    pub format_name: String,
    /// この形式をそもそも捕捉するかどうか（Ignoreなら履歴に一切乗らない）
    pub action: FilterAction,
    /// 捕捉した上でディスク永続化するかどうか（falseならプロセス実行中のみメモリ保持）
    pub save: bool,
    /// このバイト数を超えるデータは捕捉しない。0は無制限。
    #[serde(default)]
    pub limit_size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WindowFilter {
    /// 部分一致で判定。空文字はタイトルを条件にしない（class_nameのみで判定）
    #[serde(default)]
    pub title: String,
    /// 完全一致で判定。空文字はクラス名を条件にしない
    #[serde(default)]
    pub class_name: String,
    pub ignore: bool,
}

/// 履歴に追加する前の重複チェックの範囲。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OverlapCheck {
    /// 重複チェックしない（常に新規追加）
    None,
    /// 直近1件とのみ比較する
    Last,
    /// 履歴全件と比較する
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilterAction {
    Add,
    Ignore,
}

// --- Defaults ---

/// 既定の形式フィルタの大きさの上限（バイト）。テキストは UTF-16 のバイト数、ファイルは中身ではなくパスの一覧の大きさ。
pub const DEFAULT_TEXT_LIMIT: u64 = 32 * 1024 * 1024;
pub const DEFAULT_IMAGE_LIMIT: u64 = 256 * 1024 * 1024;
pub const DEFAULT_FILE_LIST_LIMIT: u64 = 1024 * 1024;
/// 既定の1回のコピーの合計の上限（バイト）。既定の形式ごとの上限の和（約 289MiB）より大きくし、既定のまま
/// 使う間は当たらないようにする（形式を足したときと、形式ごとの上限が 0 の設定の歯止め）
pub const DEFAULT_CAPTURE_TOTAL_LIMIT: u64 = 320 * 1024 * 1024;

impl Default for Config {
    fn default() -> Self {
        Self {
            general: GeneralConfig::default(),
            history: HistoryConfig::default(),
            hotkey: HotkeyConfig::default(),
            tools: ToolsConfig::default(),
            format_filter_default: FilterAction::Ignore,
            // 大きさの上限（バイト）: 極端に大きなコピーで、取り込み・保存のメモリとディスクを使い切らないため。
            // 新しく作る設定だけの既定（設定ファイルにある値は変えない）
            format_filters: vec![
                FormatFilter {
                    format_name: "CF_UNICODETEXT".to_string(),
                    action: FilterAction::Add,
                    save: true,
                    limit_size: DEFAULT_TEXT_LIMIT,
                },
                FormatFilter {
                    format_name: "CF_DIB".to_string(),
                    action: FilterAction::Add,
                    save: true,
                    limit_size: DEFAULT_IMAGE_LIMIT,
                },
                FormatFilter {
                    format_name: "CF_HDROP".to_string(),
                    action: FilterAction::Add,
                    save: true,
                    limit_size: DEFAULT_FILE_LIST_LIMIT,
                },
            ],
            capture_total_limit: DEFAULT_CAPTURE_TOTAL_LIMIT,
            window_filters: Vec::new(),
        }
    }
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            clipboard_watch: true,
            show_trayicon: true,
            viewer_always_on_top: false,
            start_hidden: false,
            startup_clipboard_sync: true,
            notify_action_errors: true,
            check_folder_permissions: true,
            // ビューアのクライアント領域の既定の大きさ
            viewer_width: 900,
            viewer_height: 700,
            // 設定画面の既定の大きさ（今の設定画面は使わない。設定ファイルの互換のため残す）
            settings_width: 600,
            settings_height: 640,
        }
    }
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            max: 30,
            overlap_check: OverlapCheck::Last,
            add_interval_ms: 1000,
            save_on_exit: true,
            save_on_change: false,
            delete_on_send: true,
            sound_on_add: false,
            sound_file: String::new(),
            grouping: HistoryGroupingConfig::default(),
        }
    }
}

// --- Load / Save ---

#[derive(Debug)]
pub enum ConfigError {
    Io(io::Error),
    /// 書式の誤り。位置（日本語）と `toml` の説明（`storage::toml_error_detail`）
    Parse(String),
    Serialize(toml::ser::Error),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "ファイルを読み書きできません（{e}）"),
            Self::Parse(detail) => write!(f, "書式が正しくありません（{detail}）"),
            Self::Serialize(e) => write!(f, "設定を書き出せません（{e}）"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Parse(_) => None,
            Self::Serialize(e) => Some(e),
        }
    }
}

impl From<io::Error> for ConfigError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<toml::ser::Error> for ConfigError {
    fn from(e: toml::ser::Error) -> Self {
        Self::Serialize(e)
    }
}

impl Config {
    /// 設定ファイルが存在しない場合はエラーにせずデフォルト設定を返す（初回起動時など）。
    /// 無いことは読んでみて `NotFound` で見分ける（`Path::exists` は権限などで調べられないときも偽を返し、
    /// 読めないファイルを「無い」と取り違えるため）。ほかの読み取りの誤りは `Err`。
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e.into()),
        };
        toml::from_str(&text).map_err(|e| ConfigError::Parse(crate::storage::toml_error_detail(&e, &text)))
    }

    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        // 書き込み途中のクラッシュで設定ファイルを壊さない（storage.rsと同方式）
        crate::storage::write_atomic(path, text.as_bytes())?;
        Ok(())
    }

    /// 既定の設定ファイルパス（= exe と同じフォルダ。ポータブル構成）。
    pub fn default_path() -> PathBuf {
        crate::storage::base_dir().join("config.toml")
    }

    pub fn should_capture(&self, format_name: &str) -> FilterAction {
        for filter in &self.format_filters {
            if filter.format_name == format_name {
                return filter.action;
            }
        }
        self.format_filter_default
    }

    pub fn should_save(&self, format_name: &str) -> bool {
        for filter in &self.format_filters {
            if filter.format_name == format_name {
                return filter.save;
            }
        }
        false
    }

    pub fn size_limit(&self, format_name: &str) -> u64 {
        for filter in &self.format_filters {
            if filter.format_name == format_name {
                return filter.limit_size;
            }
        }
        0
    }

    /// 設定画面で入力された設定を、保存の前に確かめる。無効な機能の
    /// 値（階層表示・ツールチップが無効な間の数値）も確かめる（ファイルに残り、有効にしたときに使われる
    /// ため）。誤りが無ければ空。
    pub fn validate(&self) -> Vec<ConfigIssue> {
        let mut issues = Vec::new();
        let mut range = |field: &'static str, label: &str, value: u64, min: u64, max: u64| {
            if !(min..=max).contains(&value) {
                issues.push(ConfigIssue {
                    field,
                    row: None,
                    message: format!("「{label}」は {min} から {max} の範囲で入力してください（今の値は {value}）"),
                });
            }
        };
        let h = &self.history;
        range("history.max", "最大件数", u64::from(h.max), 0, 10_000);
        range("history.grouping.visible_items", "直下に表示する件数", u64::from(h.grouping.visible_items), 1, 1_000);
        range("history.grouping.folders", "フォルダ数", u64::from(h.grouping.folders), 1, 100);
        range("history.grouping.items_per_folder", "フォルダ内の件数", u64::from(h.grouping.items_per_folder), 1, 1_000);
        range("history.add_interval_ms", "追加までの遅延", h.add_interval_ms, 1, 10_000);
        let k = &self.hotkey;
        range("hotkey.menu_max_items", "メニューに出す履歴件数", u64::from(k.menu_max_items), 1, 100);
        range("hotkey.tooltip.delay_ms", "表示までの待ち時間", u64::from(k.tooltip.delay_ms), 0, 5_000);
        range("hotkey.tooltip.max_chars", "最大文字数", u64::from(k.tooltip.max_chars), 1, 100_000);
        range("hotkey.tooltip.max_lines", "最大行数", u64::from(k.tooltip.max_lines), 1, 200);
        range("tools.text.word_break_width", "折り返し幅", u64::from(self.tools.text.word_break_width), 1, 1_000);
        if k.popup_menu.enabled && !crate::hotkey::is_valid_hotkey_key(&k.popup_menu.key) {
            issues.push(ConfigIssue {
                field: "hotkey.popup_menu.key",
                row: None,
                message: "ホットキーのキー欄には半角英数字1文字を入力してください（現在の値では登録されません）".to_string(),
            });
        }
        // 再生するときもネットワークの場所は開かずにシステム音にする（`ops::play_add_sound`）。音を鳴らす設定が
        // 無効な間は欄が灰色で直せないので、ホットキーのキーと同じく有効なときだけ確かめる
        if h.sound_on_add && !h.sound_file.is_empty() && !crate::ops::is_local_path(&h.sound_file) {
            issues.push(ConfigIssue {
                field: "history.sound_file",
                row: None,
                message: "WAVファイルには、この PC のドライブにあるファイルを指定してください（ネットワークの場所は使えません）"
                    .to_string(),
            });
        }
        // TOML の整数は符号付き64ビット（形式ごとの上限と同じ）
        if i64::try_from(self.capture_total_limit).is_err() {
            issues.push(ConfigIssue {
                field: "capture_total_limit",
                row: None,
                message: format!("「コピーの合計の上限」は {} 以下で入力してください", i64::MAX),
            });
        }
        // フィルタの効かない行は誤りにする（黙って保存しない）。形式名の比べ方は
        // `should_capture` と同じ完全一致（大文字小文字・前後の空白をそろえると、別の形式を重複と見なす）
        for (i, f) in self.format_filters.iter().enumerate() {
            let n = i + 1;
            // TOML の整数は符号付き64ビット（これを超えると設定を書き出せない。`merge_edit` の変換も失敗する）
            if i64::try_from(f.limit_size).is_err() {
                issues.push(ConfigIssue {
                    field: "format_filters.limit_size",
                    row: Some(i),
                    message: format!("形式フィルタの {n} 行目: 「上限」は {} 以下で入力してください", i64::MAX),
                });
            }
            if f.format_name.trim().is_empty() {
                issues.push(ConfigIssue {
                    field: "format_filters.format_name",
                    row: Some(i),
                    message: format!("形式フィルタの {n} 行目: 形式名を入力してください"),
                });
            } else if let Some(first) = self.format_filters[..i].iter().position(|g| g.format_name == f.format_name) {
                issues.push(ConfigIssue {
                    field: "format_filters.format_name",
                    row: Some(i),
                    message: format!(
                        "形式フィルタの {n} 行目: 形式名「{}」は {} 行目と同じです（最初の行だけが使われます）",
                        f.format_name,
                        first + 1
                    ),
                });
            }
        }
        for (i, w) in self.window_filters.iter().enumerate() {
            if w.title.is_empty() && w.class_name.is_empty() {
                issues.push(ConfigIssue {
                    field: "window_filters.title",
                    row: Some(i),
                    message: format!("ウィンドウフィルタの {} 行目: タイトルかクラス名を入力してください", i + 1),
                });
            }
        }
        issues
    }
}

/// 設定の入力の誤り（`Config::validate`）。`field` は設定の鍵の道筋（例 `"history.max"`）で、設定画面が
/// 該当の入力欄を探すのに使う。`row` は配列（フィルタ）の何行目か（0 始まり。配列でなければ None）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigIssue {
    pub field: &'static str,
    pub row: Option<usize>,
    pub message: String,
}

/// 設定画面の OK で保存する設定を作る。項目ごとに、開いたときの値（`base`）・
/// 設定画面で編集した値（`draft`）・今の値（`current`）を比べ、設定画面で変えた項目（`draft != base`）は
/// `draft`、変えていない項目は `current` を使う。設定画面に無い項目（ビューアの大きさ・最前面など）は
/// 画面で変えられないので `current` が残り、開いている間にトレイから監視を切り替えた結果も、画面で監視を
/// 触っていなければ残る。
///
/// 3つとも `Config` から `toml::Value` にして比べる（`#[serde(default)]` の省略鍵も既定値が入るので、鍵は
/// そろう）。表は鍵ごとに、それ以外（数値・文字・真偽・配列）は1つの値として比べる。配列
/// （`format_filters`・`window_filters`・`modifiers`）は丸ごと1つの値: これらを変える経路は設定画面だけ
/// なので、行ごとに合わせる必要が無い。ほかの経路で配列を変えるようにするときは、合わせ方を見直す。
pub fn merge_edit(base: &Config, draft: &Config, current: &Config) -> Result<Config, ConfigError> {
    let to_value = |c: &Config| toml::Value::try_from(c).map_err(ConfigError::Serialize);
    let merged = merge_value(&to_value(base)?, &to_value(draft)?, &to_value(current)?);
    // 文字列を経由して `Config::load` と同じ `toml::from_str` で読む。`merged.try_into()`（`toml::Value` から読む）に
    // すると、設定の全構造体について別の読み方が作られ、exe が約 150KB 増える（実測）
    let text = toml::to_string(&merged).map_err(ConfigError::Serialize)?;
    toml::from_str(&text).map_err(|e| ConfigError::Parse(e.to_string()))
}

fn merge_value(base: &toml::Value, draft: &toml::Value, current: &toml::Value) -> toml::Value {
    match (base, draft, current) {
        (toml::Value::Table(b), toml::Value::Table(d), toml::Value::Table(c)) => {
            let mut out = c.clone();
            for (key, dv) in d {
                let merged = match (b.get(key), c.get(key)) {
                    (Some(bv), Some(cv)) => merge_value(bv, dv, cv),
                    // 型が同じなので鍵はそろうはず。そろわなければ編集した値を使う
                    _ => dv.clone(),
                };
                out.insert(key.clone(), merged);
            }
            toml::Value::Table(out)
        }
        _ if draft == base => current.clone(),
        _ => draft.clone(),
    }
}

impl Config {

    /// フォアグラウンドウィンドウのタイトル/クラス名がignoreフィルタに一致するか判定する。
    /// タイトルは部分一致、クラス名は完全一致（クラス名は固定文字列のため揺れがない）。
    /// title/class_nameが両方空のフィルタは「条件未指定」として無視する（どのウィンドウにも
    /// 一致させない）。空欄チェックを条件スキップとして扱う設計上、両方空だと全ウィンドウに
    /// 一致してしまうため。設定画面はこの行を OK で誤りにする（`validate`）が、
    /// 手で書いた設定ファイルには残りうるので、ここでも弾く（弾かないと監視が
    /// 原因不明のまま全停止する）。
    pub fn is_window_ignored(&self, title: &str, class_name: &str) -> bool {
        self.window_filters.iter().any(|wf| {
            wf.ignore
                && (!wf.title.is_empty() || !wf.class_name.is_empty())
                && (wf.title.is_empty() || title.contains(&wf.title))
                && (wf.class_name.is_empty() || class_name == wf.class_name)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 設定ファイルの誤り（手で直すときに起きやすい値の型の誤りと、構文の誤り）は、位置を日本語で
    /// 出し、`toml` の説明を1行で続ける。
    #[test]
    fn load_reports_parse_error_position_in_japanese() {
        let dir = std::env::temp_dir().join(format!("clclr-config-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        fs::write(&path, "[general]\nclipboard_watch = \"はい\"\n").unwrap();
        let message = Config::load(&path).unwrap_err().to_string();
        assert!(message.starts_with("書式が正しくありません（2 行目、19 文字目: "), "{message}");
        assert!(!message.contains('\n'), "{message}");

        fs::write(&path, "[general\n").unwrap();
        let message = Config::load(&path).unwrap_err().to_string();
        assert!(message.starts_with("書式が正しくありません（1 行目、"), "{message}");

        fs::remove_dir_all(dir).unwrap();
    }

    /// 無いときだけ既定の設定。あるのに読めない（ここではフォルダ）ときは既定にせず誤りを返す（読めない設定で
    /// 起動を続けると、既定の保持件数で履歴を切り詰めるため）。
    #[test]
    fn load_uses_default_only_when_missing() {
        let dir = std::env::temp_dir().join(format!("clclr-config-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let missing = Config::load(&dir.join("config.toml")).unwrap();
        assert_eq!(missing.history.max, Config::default().history.max);
        fs::create_dir_all(dir.join("config.toml")).unwrap();
        assert!(matches!(Config::load(&dir.join("config.toml")), Err(ConfigError::Io(_))));
        fs::remove_dir_all(dir).unwrap();
    }

    /// 手で書いた大きな値でも、階層表示の保持総数は桁あふれで小さな値に回り込まない（回り込むと起動時の
    /// 切り詰めが履歴を消す）。
    #[test]
    fn history_grouping_total_saturates() {
        let g = HistoryGroupingConfig {
            visible_items: u32::MAX,
            folders: 100_000,
            items_per_folder: 100_000,
            ..HistoryGroupingConfig::default()
        };
        assert_eq!(g.total(), u32::MAX);
        let g = HistoryGroupingConfig { visible_items: 1, folders: 70_000, items_per_folder: 70_000, ..g };
        assert_eq!(g.total(), u32::MAX);
    }

    /// 項目の無い以前の設定ファイルでも、操作の失敗は知らせる（既定値 true）。
    #[test]
    fn notify_action_errors_defaults_to_true_for_older_files() {
        let config: Config = toml::from_str("[general]\nclipboard_watch = false\n").unwrap();
        assert!(config.general.notify_action_errors);
        assert!(!config.general.clipboard_watch);
    }

    /// 新しく作る設定の既定の形式フィルタには、大きさの上限がある（テキスト 32MiB・画像 256MiB・ファイルの一覧 1MiB）。
    /// 設定ファイルにある値（0 = 無制限を含む）は、そのまま使う。
    #[test]
    fn default_format_filters_have_size_limits() {
        let c = Config::default();
        assert_eq!(c.size_limit("CF_UNICODETEXT"), 32 * 1024 * 1024);
        assert_eq!(c.size_limit("CF_DIB"), 256 * 1024 * 1024);
        assert_eq!(c.size_limit("CF_HDROP"), 1024 * 1024);
        let text = "[[format_filters]]\nformat_name = \"CF_UNICODETEXT\"\naction = \"add\"\nsave = true\nlimit_size = 0\n";
        let saved: Config = toml::from_str(text).unwrap();
        assert_eq!(saved.size_limit("CF_UNICODETEXT"), 0);
    }

    /// 1回のコピーの合計の上限: 既定は 320MiB で、既定の形式ごとの上限の和より大きい（既定のまま使う間は当たらない）。
    /// 項目の無い以前の設定ファイル（形式ごとの上限が 0 のものを含む）にも既定が効き、書いた値（0 = 無制限を含む）は
    /// そのまま使い、保存して読み戻しても変わらない。
    #[test]
    fn capture_total_limit_defaults_and_round_trips() {
        let c = Config::default();
        assert_eq!(c.capture_total_limit, 320 * 1024 * 1024);
        let sum: u64 = c.format_filters.iter().map(|f| f.limit_size).sum();
        assert!(sum < c.capture_total_limit, "{sum}");

        let older = "format_filter_default = \"ignore\"\n[[format_filters]]\nformat_name = \"CF_DIB\"\naction = \"add\"\nsave = true\nlimit_size = 0\n";
        let older: Config = toml::from_str(older).unwrap();
        assert_eq!(older.capture_total_limit, DEFAULT_CAPTURE_TOTAL_LIMIT);

        for limit in [0, 1234] {
            let written: Config = toml::from_str(&format!("capture_total_limit = {limit}\n")).unwrap();
            assert_eq!(written.capture_total_limit, limit);
            let again: Config = toml::from_str(&toml::to_string(&written).unwrap()).unwrap();
            assert_eq!(again.capture_total_limit, limit);
        }
    }

    /// 合計の上限も、TOML に書ける範囲（符号付き64ビット）を超えたら誤りにする。
    #[test]
    fn capture_total_limit_over_i64_is_an_issue() {
        let mut c = Config::default();
        c.capture_total_limit = i64::MAX as u64;
        assert!(c.validate().is_empty());
        c.capture_total_limit = i64::MAX as u64 + 1;
        let got: Vec<(&str, Option<usize>)> = c.validate().iter().map(|i| (i.field, i.row)).collect();
        assert_eq!(got, [("capture_total_limit", None)]);
    }

    /// 項目の無い以前の設定ファイルでも、保存先のフォルダの権限は確かめる（既定値 true）。
    #[test]
    fn check_folder_permissions_defaults_to_true_for_older_files() {
        let config: Config = toml::from_str("[general]\nclipboard_watch = false\n").unwrap();
        assert!(config.general.check_folder_permissions);
        let config: Config = toml::from_str("").unwrap();
        assert!(config.general.check_folder_permissions);
        let config: Config = toml::from_str("[general]\ncheck_folder_permissions = false\n").unwrap();
        assert!(!config.general.check_folder_permissions);
    }

    #[test]
    fn history_grouping_total_sums_visible_and_folder_capacity() {
        let mut g = HistoryGroupingConfig::default();
        g.visible_items = 10;
        g.folders = 5;
        g.items_per_folder = 10;
        assert_eq!(g.total(), 60);
    }

    #[test]
    fn hotkey_any_double_press_true_only_when_some_action_is_set() {
        let mut hk = HotkeyConfig::default();
        assert!(!hk.any_double_press()); // 既定は全てNone
        hk.double_press_shift = DoublePressAction::Viewer;
        assert!(hk.any_double_press());
        hk.double_press_shift = DoublePressAction::None;
        hk.double_press_alt = DoublePressAction::Menu;
        assert!(hk.any_double_press());
        hk.double_press_alt = DoublePressAction::MenuPinned;
        assert!(hk.any_double_press());
    }

    /// 二度押しの値の書き方（config.toml）: `none`・`menu`・`viewer` と、
    /// `menu_pinned`・`menu_history`。書いて読み戻すと同じ値になる。
    #[test]
    fn double_press_actions_serialize_as_snake_case_names() {
        let cfg: Config = toml::from_str(
            "[hotkey]\ndouble_press_ctrl = \"menu_pinned\"\ndouble_press_shift = \"menu_history\"\ndouble_press_alt = \"viewer\"\n",
        )
        .unwrap();
        assert_eq!(cfg.hotkey.double_press_ctrl, DoublePressAction::MenuPinned);
        assert_eq!(cfg.hotkey.double_press_shift, DoublePressAction::MenuHistory);
        assert_eq!(cfg.hotkey.double_press_alt, DoublePressAction::Viewer);
        let text = toml::to_string(&cfg).unwrap();
        assert!(text.contains("double_press_ctrl = \"menu_pinned\"") && text.contains("double_press_shift = \"menu_history\""), "{text}");
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.hotkey.double_press_ctrl, DoublePressAction::MenuPinned);
        assert_eq!(back.hotkey.double_press_shift, DoublePressAction::MenuHistory);
    }

    #[test]
    fn menu_tooltip_defaults_follow_c_version() {
        let tt = MenuTooltipConfig::default();
        assert!(tt.enabled);
        assert_eq!(tt.delay_ms, 500); // C版 tooltip_show_delay
        assert_eq!(tt.max_chars, 1024); // C版 menu_tooltip_size
        assert_eq!(tt.max_lines, 20);
    }

    #[test]
    fn config_without_tooltip_section_loads_default_tooltip() {
        // ツールチップ追加前のconfig.tomlが読めて、既定値になること
        let cfg: Config = toml::from_str("[hotkey]\nmenu_max_items = 7\n").unwrap();
        assert_eq!(cfg.hotkey.menu_max_items, 7);
        assert_eq!(cfg.hotkey.tooltip, MenuTooltipConfig::default());
    }

    /// ピン留めを上に出す設定: 項目の無い以前の設定ファイルは既定（履歴が上）で読め、書いて読み戻すと同じ値になる。
    #[test]
    fn menu_pinned_first_defaults_to_false_and_round_trips() {
        assert!(!Config::default().hotkey.menu_pinned_first);
        let cfg: Config = toml::from_str("[hotkey]\nmenu_max_items = 7\n").unwrap();
        assert!(!cfg.hotkey.menu_pinned_first);
        let cfg: Config = toml::from_str("[hotkey]\nmenu_pinned_first = true\n").unwrap();
        assert!(cfg.hotkey.menu_pinned_first);
        let back: Config = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert!(back.hotkey.menu_pinned_first);
    }

    #[test]
    fn partial_tooltip_section_keeps_other_defaults() {
        let cfg: Config =
            toml::from_str("[hotkey.tooltip]\nenabled = false\ndelay_ms = 250\n").unwrap();
        assert!(!cfg.hotkey.tooltip.enabled);
        assert_eq!(cfg.hotkey.tooltip.delay_ms, 250);
        assert_eq!(cfg.hotkey.tooltip.max_chars, 1024);
        assert_eq!(cfg.hotkey.tooltip.max_lines, 20);
    }

    #[test]
    fn tooltip_config_round_trips_through_toml() {
        let mut cfg = Config::default();
        cfg.hotkey.tooltip.max_lines = 5;
        let text = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.hotkey.tooltip, cfg.hotkey.tooltip);
    }

    #[test]
    fn viewer_default_size_fits_default_settings_window() {
        // 既定のビューアは、既定の設定画面を制限（縮小）せずに開ける大きさであること
        let general = GeneralConfig::default();
        assert_eq!(general.viewer_size(), [900.0, 700.0]);
        let [settings_w, settings_h] = general.settings_size();
        let [viewer_w, viewer_h] = general.viewer_size();
        assert!(settings_w as f32 <= viewer_w && settings_h as f32 <= viewer_h);
    }

    #[test]
    fn config_without_viewer_size_loads_default_size() {
        // サイズ保存追加前のconfig.tomlが読めて、既定サイズになること
        let cfg: Config = toml::from_str("[general]\nshow_trayicon = false\n").unwrap();
        assert!(!cfg.general.show_trayicon);
        assert_eq!(cfg.general.viewer_size(), [900.0, 700.0]);
    }

    #[test]
    fn viewer_size_round_trips_through_toml() {
        let mut cfg = Config::default();
        cfg.general.viewer_width = 1024;
        cfg.general.viewer_height = 700;
        let back: Config = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back.general.viewer_size(), [1024.0, 700.0]);
    }

    #[test]
    fn settings_size_defaults_and_round_trips() {
        assert_eq!(GeneralConfig::default().settings_size(), [600, 640]);
        // 保存機能の追加前のconfig.tomlが読めて、既定サイズになること
        let cfg: Config = toml::from_str("[general]\nshow_trayicon = false\n").unwrap();
        assert_eq!(cfg.general.settings_size(), [600, 640]);

        let mut cfg = Config::default();
        cfg.general.settings_width = 800;
        cfg.general.settings_height = 900;
        let back: Config = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back.general.settings_size(), [800, 900]);
    }

    #[test]
    fn settings_size_is_clamped_to_minimum() {
        let mut general = GeneralConfig::default();
        general.settings_width = 0;
        general.settings_height = 10;
        assert_eq!(
            general.settings_size(),
            [SETTINGS_MIN_WIDTH, SETTINGS_MIN_HEIGHT]
        );
    }

    #[test]
    fn viewer_size_is_clamped_to_minimum() {
        let mut general = GeneralConfig::default();
        general.viewer_width = 10;
        general.viewer_height = 0;
        assert_eq!(
            general.viewer_size(),
            [VIEWER_MIN_WIDTH as f32, VIEWER_MIN_HEIGHT as f32]
        );
    }

    fn format_filter(name: &str, action: FilterAction, save: bool, limit_size: u64) -> FormatFilter {
        FormatFilter {
            format_name: name.to_string(),
            action,
            save,
            limit_size,
        }
    }

    #[test]
    fn should_capture_returns_matching_filter_action() {
        let mut config = Config::default();
        config.format_filters = vec![format_filter("CF_TEXT", FilterAction::Ignore, false, 0)];
        assert_eq!(config.should_capture("CF_TEXT"), FilterAction::Ignore);
    }

    #[test]
    fn should_capture_falls_back_to_format_filter_default() {
        let mut config = Config::default();
        config.format_filters = Vec::new();
        config.format_filter_default = FilterAction::Add;
        assert_eq!(config.should_capture("CF_UNKNOWN"), FilterAction::Add);
        config.format_filter_default = FilterAction::Ignore;
        assert_eq!(config.should_capture("CF_UNKNOWN"), FilterAction::Ignore);
    }

    #[test]
    fn should_save_returns_matching_filter_save_flag() {
        let mut config = Config::default();
        config.format_filters = vec![format_filter("CF_TEXT", FilterAction::Add, true, 0)];
        assert!(config.should_save("CF_TEXT"));
    }

    #[test]
    fn should_save_defaults_to_false_when_unmatched() {
        // format_filter_defaultとは独立してfalse固定（should_captureとの非対称性）
        let mut config = Config::default();
        config.format_filters = Vec::new();
        config.format_filter_default = FilterAction::Add;
        assert!(!config.should_save("CF_UNKNOWN"));
    }

    #[test]
    fn size_limit_returns_matching_filter_limit() {
        let mut config = Config::default();
        config.format_filters = vec![format_filter("CF_TEXT", FilterAction::Add, true, 1024)];
        assert_eq!(config.size_limit("CF_TEXT"), 1024);
    }

    #[test]
    fn size_limit_defaults_to_zero_when_unmatched() {
        let mut config = Config::default();
        config.format_filters = Vec::new();
        assert_eq!(config.size_limit("CF_UNKNOWN"), 0);
    }

    #[test]
    fn effective_max_uses_plain_max_when_grouping_disabled() {
        let mut config = Config::default();
        config.history.max = 30;
        config.history.grouping.enabled = false;
        assert_eq!(config.history.effective_max(), 30);
    }

    #[test]
    fn effective_max_uses_grouping_total_when_enabled() {
        let mut config = Config::default();
        config.history.max = 30;
        config.history.grouping.enabled = true;
        config.history.grouping.visible_items = 10;
        config.history.grouping.folders = 5;
        config.history.grouping.items_per_folder = 10;
        // maxではなく導出総数（10 + 5*10 = 60）が優先される
        assert_eq!(config.history.effective_max(), 60);
    }

    fn filter(title: &str, class_name: &str, ignore: bool) -> WindowFilter {
        WindowFilter {
            title: title.to_string(),
            class_name: class_name.to_string(),
            ignore,
        }
    }

    /// 回帰: `ui/settings.rs`の「フィルタを追加」ボタンが生成する
    /// `WindowFilter { title: "", class_name: "", ignore: true }`が、
    /// 追加直後の保存で全ウィンドウを無言で除外してしまわないこと。
    #[test]
    fn empty_filter_matches_nothing() {
        let mut config = Config::default();
        config.window_filters.push(filter("", "", true));
        assert!(!config.is_window_ignored("任意のタイトル", "AnyClass"));
        assert!(!config.is_window_ignored("", ""));
    }

    #[test]
    fn title_only_filter_matches_by_partial_title() {
        let mut config = Config::default();
        config.window_filters.push(filter("メモ帳", "", true));
        assert!(config.is_window_ignored("無題 - メモ帳", "Notepad"));
        assert!(!config.is_window_ignored("エクスプローラー", "CabinetWClass"));
    }

    #[test]
    fn class_name_only_filter_matches_by_exact_class() {
        let mut config = Config::default();
        config.window_filters.push(filter("", "Notepad", true));
        assert!(config.is_window_ignored("無題 - メモ帳", "Notepad"));
        assert!(!config.is_window_ignored("無題 - メモ帳", "NotepadSub"));
    }

    #[test]
    fn both_fields_filter_requires_both_to_match() {
        let mut config = Config::default();
        config.window_filters.push(filter("メモ帳", "Notepad", true));
        assert!(config.is_window_ignored("無題 - メモ帳", "Notepad"));
        assert!(!config.is_window_ignored("無題 - メモ帳", "OtherClass"));
        assert!(!config.is_window_ignored("エクスプローラー", "Notepad"));
    }

    #[test]
    fn ignore_false_never_matches() {
        let mut config = Config::default();
        config.window_filters.push(filter("メモ帳", "Notepad", false));
        assert!(!config.is_window_ignored("無題 - メモ帳", "Notepad"));
    }

    // --- 設定画面の入力の確かめと合わせ方 ---

    /// 各数値の範囲の端（下限・上限は通り、外れは誤り）。既定の設定には誤りが無い。
    #[test]
    fn validate_checks_each_range_at_its_ends() {
        assert!(Config::default().validate().is_empty(), "{:?}", Config::default().validate());
        type Set = fn(&mut Config, u64);
        let cases: [(&str, u64, u64, Set); 10] = [
            ("history.max", 0, 10_000, |c, v| c.history.max = v as u32),
            ("history.grouping.visible_items", 1, 1_000, |c, v| c.history.grouping.visible_items = v as u32),
            ("history.grouping.folders", 1, 100, |c, v| c.history.grouping.folders = v as u32),
            ("history.grouping.items_per_folder", 1, 1_000, |c, v| c.history.grouping.items_per_folder = v as u32),
            ("history.add_interval_ms", 1, 10_000, |c, v| c.history.add_interval_ms = v),
            ("hotkey.menu_max_items", 1, 100, |c, v| c.hotkey.menu_max_items = v as u32),
            ("hotkey.tooltip.delay_ms", 0, 5_000, |c, v| c.hotkey.tooltip.delay_ms = v as u32),
            ("hotkey.tooltip.max_chars", 1, 100_000, |c, v| c.hotkey.tooltip.max_chars = v as u32),
            ("hotkey.tooltip.max_lines", 1, 200, |c, v| c.hotkey.tooltip.max_lines = v as u32),
            ("tools.text.word_break_width", 1, 1_000, |c, v| c.tools.text.word_break_width = v as u32),
        ];
        for (field, min, max, set) in cases {
            let fields_for = |value: u64| {
                let mut c = Config::default();
                set(&mut c, value);
                c.validate().into_iter().map(|i| i.field).collect::<Vec<_>>()
            };
            assert!(fields_for(min).is_empty(), "{field} の下限 {min} が誤りになった");
            assert!(fields_for(max).is_empty(), "{field} の上限 {max} が誤りになった");
            assert_eq!(fields_for(max + 1), [field], "{field} の上限を超えた値");
            if min > 0 {
                assert_eq!(fields_for(min - 1), [field], "{field} の下限を下回った値");
            }
        }
    }

    /// ホットキーのキーは、ホットキーが有効なときだけ半角英数字1文字かを確かめる。
    #[test]
    fn validate_checks_hotkey_key_only_when_enabled() {
        let mut c = Config::default();
        c.hotkey.popup_menu.key = "あ".to_string();
        c.hotkey.popup_menu.enabled = true;
        let issues = c.validate();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].field, "hotkey.popup_menu.key");
        c.hotkey.popup_menu.enabled = false;
        assert!(c.validate().is_empty());
    }

    /// 追加音のファイルは、音を鳴らす設定が有効なときだけ、ネットワークの場所でないかを確かめる。
    #[test]
    fn validate_rejects_network_sound_file_only_when_enabled() {
        let mut c = Config::default();
        c.history.sound_file = r"\\server\share\a.wav".to_string();
        assert!(c.validate().is_empty());
        c.history.sound_on_add = true;
        let issues = c.validate();
        assert_eq!(issues.iter().map(|i| i.field).collect::<Vec<_>>(), ["history.sound_file"]);
        c.history.sound_file = r"C:\Windows\Media\chimes.wav".to_string();
        assert!(c.validate().is_empty());
        c.history.sound_file.clear();
        assert!(c.validate().is_empty());
    }

    /// フィルタの効かない行: 空の形式名（空白だけも）、前の行と同じ形式名（完全一致。
    /// 大文字小文字・空白の違いは別の形式）、タイトルとクラス名が両方空のウィンドウ行（除外のオン・オフによらず）。
    /// 誤りは何行目かを持つ。
    #[test]
    fn validate_rejects_filter_rows_that_never_apply() {
        let mut c = Config::default();
        c.format_filters = vec![
            format_filter("CF_TEXT", FilterAction::Add, true, 0),
            format_filter(" ", FilterAction::Add, true, 0),
            format_filter("cf_text", FilterAction::Add, true, 0),
            format_filter("CF_TEXT ", FilterAction::Add, true, 0),
            format_filter("CF_TEXT", FilterAction::Ignore, false, 5),
        ];
        c.window_filters = vec![filter("", "", true), filter("メモ帳", "", true), filter("", "", false)];
        let got: Vec<(&str, Option<usize>)> = c.validate().iter().map(|i| (i.field, i.row)).collect();
        assert_eq!(
            got,
            [
                ("format_filters.format_name", Some(1)),
                ("format_filters.format_name", Some(4)),
                ("window_filters.title", Some(0)),
                ("window_filters.title", Some(2)),
            ]
        );
        let issues = c.validate();
        assert_eq!(issues[0].message, "形式フィルタの 2 行目: 形式名を入力してください");
        assert_eq!(issues[1].message, "形式フィルタの 5 行目: 形式名「CF_TEXT」は 1 行目と同じです（最初の行だけが使われます）");
        assert_eq!(issues[2].message, "ウィンドウフィルタの 1 行目: タイトルかクラス名を入力してください");
        assert!(Config::default().validate().iter().all(|i| i.row.is_none()));
    }

    /// 上限は TOML で表せる範囲（符号付き64ビット）まで。超えると保存も合わせ方もできないので、誤りにする。
    #[test]
    fn validate_limits_size_to_toml_integer_range() {
        let mut c = Config::default();
        c.format_filters[1].limit_size = i64::MAX as u64;
        assert!(c.validate().is_empty());
        assert!(merge_edit(&c, &c, &c).is_ok());
        c.format_filters[1].limit_size = i64::MAX as u64 + 1;
        let got: Vec<(&str, Option<usize>)> = c.validate().iter().map(|i| (i.field, i.row)).collect();
        assert_eq!(got, [("format_filters.limit_size", Some(1))]);
        assert!(merge_edit(&c, &c, &c).is_err(), "前提: 表せない値は変換できない");
    }

    fn toml_text(c: &Config) -> String {
        toml::to_string(c).unwrap()
    }

    /// 既定でない値をあちこちに入れた設定。
    fn unusual_config() -> Config {
        let mut c = Config::default();
        c.general.clipboard_watch = false;
        c.general.viewer_width = 1234;
        c.general.notify_action_errors = false;
        c.history.max = 77;
        c.history.grouping.enabled = true;
        c.history.grouping.folder_name_format = "[%1-%2]".to_string();
        c.history.overlap_check = OverlapCheck::All;
        c.history.sound_file = r"C:\a.wav".to_string();
        c.hotkey.popup_menu.modifiers = vec!["ctrl".to_string(), "shift".to_string()];
        c.hotkey.popup_menu.key = "V".to_string();
        c.hotkey.double_press_alt = DoublePressAction::Viewer;
        c.hotkey.menu_pinned_first = true;
        c.hotkey.tooltip.max_lines = 3;
        c.tools.text.date_format = "yyyy".to_string();
        c.format_filter_default = FilterAction::Add;
        c.format_filters.push(FormatFilter {
            format_name: "X".to_string(),
            action: FilterAction::Ignore,
            save: false,
            limit_size: 99,
        });
        c.window_filters.push(filter("t", "c", true));
        c
    }

    /// 3つが同じなら、全項目が変わらずに戻る（直列化の往復で値が変わらない）。鍵を省略した
    /// ファイルから読んだ設定も同じ。
    #[test]
    fn merge_edit_round_trips_every_field() {
        let c = unusual_config();
        assert_eq!(toml_text(&merge_edit(&c, &c, &c).unwrap()), toml_text(&c));
        let old: Config = toml::from_str("[general]\nclipboard_watch = false\n\n[[format_filters]]\nformat_name = \"CF_TEXT\"\naction = \"add\"\nsave = true\n").unwrap();
        assert_eq!(toml_text(&merge_edit(&old, &old, &old).unwrap()), toml_text(&old));
    }

    /// 読み戻しを文字列経由にしても（exe を小さくするため）、`toml::Value` から直接読んだとき（
    /// `try_into`）と結果が変わらない。引用符・バックスラッシュ・改行・制御文字・空の文字列と、複数行のフィルタ、
    /// 上限の境目で比べる。
    #[test]
    fn merge_edit_reads_back_same_as_value_try_into() {
        let mut c = unusual_config();
        let tricky = ["\"quoted\" \\ back", "改行\nと\tタブ\r\n", "制御\u{1}\u{7f}文字", "'''\"\"\"", "  前後の空白  ", ""];
        c.tools.text.quote_char = tricky[0].to_string();
        c.tools.text.put_text_open = tricky[1].to_string();
        c.tools.text.put_text_close = tricky[2].to_string();
        c.history.grouping.folder_name_format = tricky[3].to_string();
        c.tools.text.time_format = tricky[4].to_string();
        for (i, s) in tricky.iter().enumerate() {
            c.format_filters.push(format_filter(&format!("F{i}{s}"), FilterAction::Ignore, i % 2 == 0, i as u64 * 1000));
            c.window_filters.push(filter(s, &format!("C{i}{s}"), i % 2 == 1));
        }
        c.format_filters.push(format_filter("MAX", FilterAction::Add, true, i64::MAX as u64));
        let mut draft = c.clone();
        draft.tools.text.quote_char = "\u{1f}>>\n".to_string();
        let merged = merge_edit(&c, &draft, &c).unwrap();
        let direct: Config = toml::Value::try_from(&draft).unwrap().try_into().unwrap();
        assert_eq!(toml_text(&merged), toml_text(&direct));
        assert_eq!(toml_text(&merged), toml_text(&draft));
    }

    /// 設定画面で変えた項目はドラフト、変えていない項目は今の値。開いている間にトレイで監視を切り替えた
    /// 結果や、設定画面に無い項目（ビューアの大きさ）は今の値が残る。配列は丸ごと1つの値。
    #[test]
    fn merge_edit_takes_edited_fields_from_draft_and_others_from_current() {
        let base = Config::default();
        let mut draft = base.clone();
        draft.history.max = 50;
        draft.hotkey.popup_menu.modifiers = vec!["ctrl".to_string()];
        let mut current = base.clone();
        current.general.clipboard_watch = !base.general.clipboard_watch; // トレイで切り替えた
        current.general.viewer_width = 999; // 開いている間にビューアの大きさを保存した
        let merged = merge_edit(&base, &draft, &current).unwrap();
        assert_eq!(merged.history.max, 50);
        assert_eq!(merged.hotkey.popup_menu.modifiers, ["ctrl"]);
        assert_eq!(merged.general.clipboard_watch, current.general.clipboard_watch);
        assert_eq!(merged.general.viewer_width, 999);

        // 画面でも監視を変えていれば、画面の値を使う
        let mut draft2 = base.clone();
        draft2.general.clipboard_watch = !base.general.clipboard_watch;
        let mut current2 = base.clone();
        current2.general.clipboard_watch = !base.general.clipboard_watch;
        current2.general.clipboard_watch = base.general.clipboard_watch; // トレイで2回切り替えて元へ
        let merged2 = merge_edit(&base, &draft2, &current2).unwrap();
        assert_eq!(merged2.general.clipboard_watch, draft2.general.clipboard_watch);
    }
}
