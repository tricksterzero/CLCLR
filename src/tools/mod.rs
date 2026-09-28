//! コマンド型ツール群（C版 CLCL のプラグインのうち、アイテムに適用する操作の統合先）。
//!
//! サブモジュールは「作用する対象のデータ種別」で分ける: テキストに作用する
//! ものは text.rs、将来の画像操作は image.rs 等。設定も同じ構造で対応させる
//! （`[tools.text]` ← config.rs の ToolsConfig.text）。

pub mod text;
