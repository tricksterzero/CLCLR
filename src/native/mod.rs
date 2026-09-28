//! ネイティブ Win32 の UI（ビューア・設定画面・ダイアログ）。依存の向きは `native/` → コアの一方向で、
//! コアのモジュールへ UI の都合を持ち込まない。
//!
//! - `viewer`: ビューアの窓（ツリー・仮想一覧・プレビュー・検索欄・メニューバー・確認ダイアログ）
//! - `app`: ビューアとコア・トレイ・ホットキーの結線（`ViewerHandler` の実装、設定の反映）
//! - `actions`: ビューアの操作（送る・テキスト変換・関連付けで開く・ピン留め・削除）の専用スレッド
//! - `settings`: 設定画面（モードレスのダイアログとタブ）
//! - `images`・`search`: 画像の読み込み・検索の専用スレッド
//! - `model`: 表示用のモデル（Win32 を使わない）

pub mod actions;
pub mod app;
pub mod images;
pub mod model;
pub mod search;
pub mod settings;
#[cfg(test)]
mod testdata;
pub mod viewer;
