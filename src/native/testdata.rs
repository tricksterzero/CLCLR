//! 検証用テストデータの生成（通常のテストでは実行しない）。
//!
//! 実際の `Core::open_at` と `capture` を通して履歴を作るので、blob・サムネイル・
//! スキーマ版は本番と同じ形になる。実行方法:
//!
//! ```powershell
//! $env:CLCLR_GEN_DIR = '<出力先のフォルダ>'
//! cargo test --release generate_r1_test_data -- --ignored
//! ```
//!
//! 出力先には `config.toml`（1,000件を保持する設定。監視・起動時同期・ホットキーは切る）も書く。
//! 出力先が空でない場合は何もしない（既存データへの追記や上書きを避ける）。

use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use crate::config::Config;
use crate::data::{utf16_bytes, Entry, Format};
use crate::ops::Core;
use crate::store::{PinnedFolder, PinnedNode};

/// 生成するデータの件数（履歴の保持件数と一致させる）。
const HISTORY_COUNT: usize = 1000;
/// 巨大テキストの UTF-16 単位数（約10MB）。
const HUGE_TEXT_UNITS: usize = 5 * 1024 * 1024;

/// 乱数の代わりの線形合同法（依存を足さず、毎回同じデータを作るため）。
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn config_for_test_data() -> Config {
    let mut c = Config::default();
    // 常用している CLCLR と同時に動かすため、監視・起動時同期・ホットキー・二度押しを切る
    c.general.clipboard_watch = false;
    c.general.startup_clipboard_sync = false;
    c.general.start_hidden = true;
    c.general.show_trayicon = true;
    c.hotkey.popup_menu.enabled = false;
    // 1,000件を階層表示で保持する（直下100件 + 100件のフォルダ9つ）
    c.history.max = HISTORY_COUNT as u32;
    c.history.grouping.enabled = true;
    c.history.grouping.visible_items = 100;
    c.history.grouping.folders = 9;
    c.history.grouping.items_per_folder = 100;
    c.history.sound_on_add = false;
    c
}

fn text_format(text: &str) -> Format {
    Format { format_name: "CF_UNICODETEXT".to_string(), format_id: 13, data: utf16_bytes(text) }
}

/// 32bpp の DIB（BITMAPINFOHEADER + ボトムアップの BGRA）。`translucent` なら左から右へ
/// アルファを下げる。
fn dib(w: u32, h: u32, seed: u32, translucent: bool) -> Format {
    let mut v = Vec::with_capacity(40 + (w * h * 4) as usize);
    v.extend(40u32.to_le_bytes());
    v.extend((w as i32).to_le_bytes());
    v.extend((h as i32).to_le_bytes());
    v.extend(1u16.to_le_bytes());
    v.extend(32u16.to_le_bytes());
    v.extend(0u32.to_le_bytes()); // BI_RGB
    v.extend((w * h * 4).to_le_bytes());
    v.extend([0u8; 16]);
    for y in 0..h {
        for x in 0..w {
            let r = ((x * 255 / w.max(1)) + seed) as u8;
            let g = ((y * 255 / h.max(1)) + seed * 3) as u8;
            let b = (seed * 7) as u8;
            let a = if translucent { 255 - (x * 255 / w.max(1)) as u8 } else { 255 };
            v.extend([b, g, r, a]);
        }
    }
    Format { format_name: "CF_DIB".to_string(), format_id: 8, data: v }
}

fn hdrop(paths: &[String]) -> Format {
    let mut v = vec![0u8; 20];
    v[0..4].copy_from_slice(&20u32.to_le_bytes()); // pFiles
    v[16..20].copy_from_slice(&1u32.to_le_bytes()); // fWide
    for p in paths {
        v.extend(p.encode_utf16().flat_map(u16::to_le_bytes));
        v.extend([0u8, 0]);
    }
    v.extend([0u8, 0]);
    Format { format_name: "CF_HDROP".to_string(), format_id: 15, data: v }
}

const JA_WORDS: [&str; 12] = [
    "クリップボード", "履歴", "日本語の文章", "東京都千代田区", "会議の議事録", "メモ",
    "設定を保存しました", "よろしくお願いいたします", "変換候補", "全角・半角", "①から⑩", "𠮷野家",
];
const EN_WORDS: [&str; 10] = [
    "clipboard", "history", "Rust", "Win32", "ListView", "owner draw", "virtual list", "search",
    "preview", "manifest",
];

/// i 番目（古い順）のエントリの形式を作る。
fn formats_for(i: usize, rng: &mut Lcg) -> Vec<Format> {
    match i {
        // 新しい方から5番目あたりに約10MBのテキストを置く（選びやすくするため）
        _ if i == HISTORY_COUNT - 5 => {
            let line = "0123456789 The quick brown fox jumps over the lazy dog. いろはにほへと ちりぬるを\r\n";
            let units_per_line = line.encode_utf16().count();
            let text: String = line.repeat(HUGE_TEXT_UNITS / units_per_line);
            vec![text_format(&text)]
        }
        // 60件ごとに改行のない長い1行
        _ if i % 60 == 7 => vec![text_format(&"改行のない長い文字列".repeat(300))],
        // 16件ごとに画像（大きさを変える。大きいものはサムネイルが作られる）
        _ if i % 16 == 3 => {
            let sizes = [(16, 16), (64, 48), (200, 150), (800, 600), (1920, 1080)];
            let (w, h) = sizes[(i / 16) % sizes.len()];
            vec![dib(w, h, i as u32, (i / 16) % 3 == 0)]
        }
        // 50件ごとにファイル一覧
        _ if i % 50 == 11 => {
            let n = 1 + (i / 50) % 5;
            let paths: Vec<String> =
                (0..n).map(|k| format!("C:\\Users\\test\\Documents\\資料{i}_{k}.txt")).collect();
            vec![hdrop(&paths)]
        }
        // それ以外は短いテキスト（日本語・英語・複数行を混ぜる。重複で消えないよう番号を入れる）
        _ => {
            let words = 3 + rng.below(12) as usize;
            let mut s = format!("#{i} ");
            for k in 0..words {
                if rng.below(2) == 0 {
                    s.push_str(JA_WORDS[rng.below(JA_WORDS.len() as u64) as usize]);
                } else {
                    s.push_str(EN_WORDS[rng.below(EN_WORDS.len() as u64) as usize]);
                    s.push(' ');
                }
                if k % 5 == 4 && rng.below(3) == 0 {
                    s.push_str("\r\n");
                }
            }
            vec![text_format(&s)]
        }
    }
}

#[test]
#[ignore = "検証用データを生成するときだけ手動で実行する（CLCLR_GEN_DIR が必要）"]
fn generate_r1_test_data() {
    let dir = std::path::PathBuf::from(
        std::env::var_os("CLCLR_GEN_DIR").expect("CLCLR_GEN_DIR に出力先を指定する"),
    );
    if dir.exists() && std::fs::read_dir(&dir).unwrap().next().is_some() {
        panic!("出力先 {} が空でない（既存データを守るため中止）", dir.display());
    }
    std::fs::create_dir_all(&dir).unwrap();

    let cfg = config_for_test_data();
    cfg.save(&dir.join("config.toml")).unwrap();
    let config = Arc::new(RwLock::new(cfg));
    let core = Core::open_at(dir.clone(), Arc::clone(&config)).unwrap();

    let mut rng = Lcg(20260923);
    let now = SystemTime::now();
    for i in 0..HISTORY_COUNT {
        let mut entry = Entry::new(formats_for(i, &mut rng));
        // 古い順に作るので、1件あたり3分ずつ過去へずらす
        entry.modified = now - Duration::from_secs(((HISTORY_COUNT - i) * 180) as u64);
        core.capture(entry).unwrap();
    }
    assert_eq!(core.read(|s| s.history.len()), Some(HISTORY_COUNT), "件数上限か重複判定で減っている");

    // ピン留め: 新しい方から12件を複製し、入れ子のフォルダへ振り分ける
    let ids: Vec<_> = core.read(|s| s.history.iter().take(12).map(|item| item.meta.id).collect()).unwrap();
    for id in &ids {
        core.pin(*id, None).unwrap();
    }
    let mut items: Vec<PinnedNode> = core.read(|s| s.pinned.clone()).unwrap();
    let nested: Vec<PinnedNode> = items.drain(..3).collect();
    let work: Vec<PinnedNode> = items.drain(..4).collect();
    let mut work_children = work;
    work_children.push(PinnedNode::Folder(PinnedFolder {
        id: uuid::Uuid::new_v4(),
        title: "テンプレート".to_string(),
        children: nested,
    }));
    let mut root = vec![PinnedNode::Folder(PinnedFolder {
        id: uuid::Uuid::new_v4(),
        title: "仕事".to_string(),
        children: work_children,
    })];
    root.push(PinnedNode::Folder(PinnedFolder {
        id: uuid::Uuid::new_v4(),
        title: "空のフォルダ".to_string(),
        children: Vec::new(),
    }));
    root.extend(items); // 残り5件はルート直下
    // フォルダへの振り分けは Core の操作にないので、pinned.toml へ直接書く（最後の保存は履歴の
    // インデックスだけを書くので、この pinned.toml はそのまま残る）
    core.storage().save_pinned(&root).unwrap();

    core.shutdown(None).unwrap();
}
