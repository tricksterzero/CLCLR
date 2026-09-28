//! ビューアの検索を行う専用スレッド。
//!
//! メインスレッドは候補の軽い情報（ID・タイトル・全文の在りか）を写して依頼するだけで、全文の
//! 読み込み・小文字化・照合と、全文の置き場（キャッシュ）の管理はこのスレッドで行う
//! （入力全体を走査する重い処理はメインスレッドの外で行い、完了を同期的に待たない）。
//! 結果は一致した項目の ID を候補の順に返し、呼び出し側から受け取った `notify` でメインスレッドを
//! 起こす。
//!
//! 依頼には世代番号を付ける。処理中に新しい検索の依頼が届いたら、今の検索をやめて新しい方へ移る
//! （古い結果は返さない）。メインスレッドも、待っている世代と違う結果は捨てる。
//! スレッドは依頼の送り手がすべて無くなると終わる。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use uuid::Uuid;

use crate::data::{utf16_chars, utf16_text};
use crate::native::model;
use crate::ops::Core;
use crate::storage::Storage;

/// 全文の在りか。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextSource {
    /// テキスト形式がない（タイトルだけで照合する）
    None,
    /// ディスクの blob（CF_UNICODETEXT）
    Blob(String),
    /// ディスクに書かない CF_UNICODETEXT（履歴の resident）。このスレッドがサービスのロックを
    /// 項目ごとに短く取って resident の `Arc` を写し、全文の取り出しはロックの外で行う
    Resident,
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub id: Uuid,
    pub title: Option<String>,
    pub text: TextSource,
}

#[derive(Debug)]
pub struct SearchRequest {
    pub generation: u64,
    /// 小文字化済みの検索文字列（`model::search_needle`）
    pub needle: String,
    pub candidates: Vec<Candidate>,
}

pub enum Command {
    Search(SearchRequest),
    /// 全文のキャッシュを捨てる（ビューアを隠したとき。非表示の常駐メモリを増やさないため）
    ClearCache,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SearchResult {
    pub generation: u64,
    /// 一致した項目の ID（候補の順）
    pub matched: Vec<Uuid>,
}

/// 検索スレッドを起こす。`commands` の送り手がすべて無くなると終わる。
pub fn spawn(
    core: Core,
    commands: Receiver<Command>,
    results: Sender<SearchResult>,
    notify: impl Fn() + Send + 'static,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("clclr-search".to_string())
        .spawn(move || Worker::new(core).run(&commands, &results, &notify))
        .expect("検索スレッドを起動できない")
}

struct Worker {
    storage: Storage,
    core: Core,
    /// 小文字化したディスクの全文（テキスト形式が無い・読めないものは None）。今の候補の分だけ持つ。
    /// メモリだけに持つ全文は入れない（`resident_contains`）
    cache: HashMap<Uuid, Option<String>>,
}

enum Outcome {
    Done(Vec<Uuid>),
    /// 新しい依頼が届いたのでやめた
    Superseded,
}

impl Worker {
    fn new(core: Core) -> Self {
        Self { storage: core.storage(), core, cache: HashMap::new() }
    }

    fn run(&mut self, commands: &Receiver<Command>, results: &Sender<SearchResult>, notify: &dyn Fn()) {
        let mut queue: VecDeque<Command> = VecDeque::new();
        loop {
            let command = match queue.pop_front() {
                Some(c) => c,
                None => match commands.recv() {
                    Ok(c) => c,
                    Err(_) => return,
                },
            };
            // 溜まっている依頼をまとめて受け取り、検索は最新のものだけを行う
            queue.extend(commands.try_iter());
            match command {
                Command::ClearCache => self.cache = HashMap::new(),
                Command::Search(request) => {
                    if queue.iter().any(|c| matches!(c, Command::Search(_))) {
                        continue;
                    }
                    match self.search(&request, commands, &mut queue) {
                        Outcome::Done(matched) => {
                            if results.send(SearchResult { generation: request.generation, matched }).is_err() {
                                return;
                            }
                            notify();
                        }
                        Outcome::Superseded => {}
                    }
                }
            }
        }
    }

    /// 候補を照合する。1件ごとに新しい依頼が届いていないかを見て、検索の依頼が届いていたら
    /// やめる（届いたものは `queue` へ移す）。依頼の送り手が無くなった（終了）ときもやめる。
    /// キャッシュは今の候補の分だけに絞る。
    fn search(&mut self, request: &SearchRequest, commands: &Receiver<Command>, queue: &mut VecDeque<Command>) -> Outcome {
        let live: HashSet<Uuid> = request.candidates.iter().map(|c| c.id).collect();
        self.cache.retain(|id, _| live.contains(id));
        let mut matched = Vec::new();
        for candidate in &request.candidates {
            loop {
                match commands.try_recv() {
                    Ok(c) => queue.push_back(c),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Outcome::Superseded,
                }
            }
            // 新しい検索か、キャッシュを捨てる依頼（ビューアを隠した）が届いたらやめる（隠した後に大きな全文を
            // 読み続けてキャッシュに残さない。結果は隠している間は使われず、表示したときに検索し直す）
            if queue.iter().any(|c| matches!(c, Command::Search(_) | Command::ClearCache)) {
                return Outcome::Superseded;
            }
            let hit = match &candidate.text {
                // メモリだけに持つ全文はキャッシュに写さず、元のデータを小文字化しながら照合する
                // （写すと全文の分のメモリが倍になる）
                TextSource::Resident => {
                    model::search_matches(&request.needle, candidate.title.as_deref(), None)
                        || self.resident_contains(candidate.id, &request.needle)
                }
                TextSource::None | TextSource::Blob(_) => {
                    if !self.cache.contains_key(&candidate.id) {
                        let text = self.load_blob_text_folded(&candidate.text);
                        self.cache.insert(candidate.id, text);
                    }
                    let text = self.cache.get(&candidate.id).and_then(|t| t.as_deref());
                    model::search_matches(&request.needle, candidate.title.as_deref(), text)
                }
            };
            if hit {
                matched.push(candidate.id);
            }
        }
        Outcome::Done(matched)
    }

    /// ディスクの CF_UNICODETEXT の全文を読み、小文字にして返す（キャッシュに入れる。ディスクを毎回
    /// 読まないため）。
    fn load_blob_text_folded(&self, text: &TextSource) -> Option<String> {
        let TextSource::Blob(name) = text else {
            return None;
        };
        Some(model::search_fold(&utf16_text(&self.storage.load_blob(name).ok()?)))
    }

    /// メモリだけに持つ全文が `needle` を含むか。ロックの中では resident の `Arc` だけを写し、照合は
    /// ロックの外で、全文の写しを作らずに行う。
    fn resident_contains(&self, id: Uuid, needle: &str) -> bool {
        let Some(Some(resident)) = self.core.read(|s| s.history.get_by_id(id).map(|i| Arc::clone(&i.resident))) else {
            return false;
        };
        resident
            .iter()
            .find(|f| f.format_name == "CF_UNICODETEXT")
            .is_some_and(|f| model::folded_chars_contain(utf16_chars(&f.data), needle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::ops::tests::{memory_only_config, temp_core, text_entry};
    use std::time::Duration;

    /// 一時フォルダの `Core`。`memory_only` なら何もディスクに書かない（全文は resident）。
    fn temp_service(memory_only: bool) -> (std::path::PathBuf, Core) {
        temp_core(if memory_only { memory_only_config() } else { Config::default() })
    }

    /// 履歴の候補（`app.rs` と同じく、ディスクの blob か resident か）。
    fn candidates(core: &Core) -> Vec<Candidate> {
        core.read(|service| {
            service
                .history
                .iter()
                .map(|item| {
                    let text = match item.meta.formats.iter().find(|f| f.format_name == "CF_UNICODETEXT") {
                        Some(f) => TextSource::Blob(f.blob.clone()),
                        None if item.resident.iter().any(|f| f.format_name == "CF_UNICODETEXT") => TextSource::Resident,
                        None => TextSource::None,
                    };
                    Candidate { id: item.meta.id, title: item.meta.title.clone(), text }
                })
                .collect()
        })
        .unwrap()
    }

    fn worker(core: &Core) -> Worker {
        Worker::new(core.clone())
    }

    fn run_search(worker: &mut Worker, needle: &str, candidates: Vec<Candidate>) -> Vec<Uuid> {
        let (_tx, rx) = std::sync::mpsc::channel();
        let request = SearchRequest { generation: 1, needle: needle.to_string(), candidates };
        match worker.search(&request, &rx, &mut VecDeque::new()) {
            Outcome::Done(m) => m,
            Outcome::Superseded => panic!("やめてしまった"),
        }
    }

    /// ディスクの全文でも、メモリだけに持つ全文でも、大文字小文字を区別せずに照合する。
    /// 一致したものを候補の順に返す。
    #[test]
    fn matches_full_text_from_blob_and_resident_case_insensitively() {
        for memory_only in [false, true] {
            let (dir, service) = temp_service(memory_only);
            for text in ["Alpha one", "beta TWO", "gamma alpha"] {
                service.capture(text_entry(text)).unwrap();
            }
            let cands = candidates(&service);
            let expected: Vec<TextSource> = cands.iter().map(|c| c.text.clone()).collect();
            assert!(
                expected.iter().all(|t| if memory_only { *t == TextSource::Resident } else { matches!(t, TextSource::Blob(_)) }),
                "前提: 全文の在りか {expected:?}"
            );
            let ids: Vec<Uuid> = cands.iter().map(|c| c.id).collect();
            let mut w = worker(&service);
            // 履歴は新しい順: gamma alpha, beta TWO, Alpha one
            assert_eq!(run_search(&mut w, "alpha", cands.clone()), vec![ids[0], ids[2]]);
            assert_eq!(run_search(&mut w, "two", cands), vec![ids[1]]);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// 全文のキャッシュは今の候補の分だけ持ち、候補から外れた項目の分は次の検索で捨てる。
    #[test]
    fn cache_keeps_only_current_candidates() {
        let (dir, service) = temp_service(false);
        for text in ["one", "two", "three"] {
            service.capture(text_entry(text)).unwrap();
        }
        let mut w = worker(&service);
        let cands = candidates(&service);
        assert_eq!(run_search(&mut w, "o", cands.clone()).len(), 2);
        assert_eq!(w.cache.len(), 3);

        let gone = cands[0].id;
        assert_eq!(run_search(&mut w, "o", cands[1..].to_vec()).len(), 2);
        assert_eq!(w.cache.len(), 2);
        assert!(!w.cache.contains_key(&gone));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// メモリだけに持つ全文はキャッシュに写さずに照合する（写すと全文の分のメモリが倍になる）。
    /// 語末のシグマも、検索文字列の σ で見つかる（ディスクの全文と同じ規則）。
    #[test]
    fn resident_text_is_matched_without_caching() {
        let (dir, service) = temp_service(true);
        for text in ["ΟΔΟΣ", "plain"] {
            service.capture(text_entry(text)).unwrap();
        }
        let cands = candidates(&service);
        // 履歴は新しい順: plain, ΟΔΟΣ
        let (plain, sigma) = (cands[0].id, cands[1].id);
        let mut w = worker(&service);
        assert_eq!(run_search(&mut w, &model::search_needle("σ"), cands.clone()), vec![sigma]);
        assert_eq!(run_search(&mut w, "lai", cands), vec![plain]);
        assert!(w.cache.is_empty(), "メモリだけの全文をキャッシュに写している");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 溜まった依頼は最新の検索だけを行い、キャッシュを捨てる依頼は順に処理する。結果には依頼の
    /// 世代を付け、送った後に通知する。送り手がすべて無くなるとスレッドは終わる。
    #[test]
    fn thread_runs_only_the_latest_search_and_ends_when_senders_are_dropped() {
        let (dir, service) = temp_service(false);
        service.capture(text_entry("needle here")).unwrap();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (res_tx, res_rx) = std::sync::mpsc::channel();
        let (note_tx, note_rx) = std::sync::mpsc::channel();
        // スレッドを起こす前に溜めておく
        let cands = candidates(&service);
        cmd_tx.send(Command::Search(SearchRequest { generation: 1, needle: "needle".into(), candidates: cands.clone() })).unwrap();
        cmd_tx.send(Command::ClearCache).unwrap();
        cmd_tx.send(Command::Search(SearchRequest { generation: 2, needle: "nothing".into(), candidates: cands })).unwrap();
        let handle = spawn(service.clone(), cmd_rx, res_tx, move || {
            let _ = note_tx.send(());
        });
        let result = res_rx.recv_timeout(Duration::from_secs(5)).expect("結果が来ない");
        assert_eq!(result, SearchResult { generation: 2, matched: vec![] });
        note_rx.recv_timeout(Duration::from_secs(5)).expect("通知が来ない");
        assert!(res_rx.recv_timeout(Duration::from_millis(200)).is_err(), "古い世代の結果も返している");
        drop(cmd_tx);
        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 照合の途中で新しい検索の依頼が届いたら、今の検索をやめる。
    #[test]
    fn search_is_superseded_by_a_newer_request() {
        let (dir, service) = temp_service(false);
        service.capture(text_entry("x")).unwrap();
        let mut w = worker(&service);
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Command::Search(SearchRequest { generation: 2, needle: "y".into(), candidates: vec![] })).unwrap();
        let request = SearchRequest { generation: 1, needle: "x".into(), candidates: candidates(&service) };
        let mut queue = VecDeque::new();
        assert!(matches!(w.search(&request, &rx, &mut queue), Outcome::Superseded));
        assert_eq!(queue.len(), 1, "届いた依頼を取っておいていない");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 照合の途中でキャッシュを捨てる依頼（ビューアを隠した）が届いても、今の検索をやめる。
    #[test]
    fn search_stops_when_cache_is_to_be_cleared() {
        let (dir, service) = temp_service(false);
        service.capture(text_entry("x")).unwrap();
        let mut w = worker(&service);
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Command::ClearCache).unwrap();
        let request = SearchRequest { generation: 1, needle: "x".into(), candidates: candidates(&service) };
        let mut queue = VecDeque::new();
        assert!(matches!(w.search(&request, &rx, &mut queue), Outcome::Superseded));
        assert!(matches!(queue.front(), Some(Command::ClearCache)), "届いた依頼を取っておいていない");
        let _ = std::fs::remove_dir_all(dir);
    }
}
