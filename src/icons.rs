//! 埋め込みのアイコン（Material Symbols Sharp、Apache-2.0。32px の RGBA、左上原点、R,G,B,A 順）。
//! ビューア（種別アイコン・ツリー）とホットキーのメニューで共用する。static にして、同じ素材のアドレスを1つにする
//! （メニューはアドレスでアイコンのビットマップを共有する）。

use crate::data::EntryKind;

/// 素材の一辺（px）。
pub const SOURCE_SIZE: u32 = 32;

pub static TEXT: &[u8] = include_bytes!("../res/icons/entry_text_32.rgba");
pub static IMAGE: &[u8] = include_bytes!("../res/icons/entry_image_32.rgba");
pub static FILE: &[u8] = include_bytes!("../res/icons/entry_file_32.rgba");
pub static OTHER: &[u8] = include_bytes!("../res/icons/entry_other_32.rgba");
pub static HISTORY: &[u8] = include_bytes!("../res/icons/history_32.rgba");
pub static FOLDER: &[u8] = include_bytes!("../res/icons/folder_32.rgba");
pub static PINNED: &[u8] = include_bytes!("../res/icons/pinned_32.rgba");

/// 種別アイコン。
pub fn for_kind(kind: EntryKind) -> &'static [u8] {
    match kind {
        EntryKind::Text => TEXT,
        EntryKind::Image => IMAGE,
        EntryKind::File => FILE,
        EntryKind::Other => OTHER,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_are_square_rgba_of_source_size() {
        for rgba in [TEXT, IMAGE, FILE, OTHER, HISTORY, FOLDER, PINNED] {
            assert_eq!(rgba.len(), (SOURCE_SIZE * SOURCE_SIZE * 4) as usize);
        }
    }
}
