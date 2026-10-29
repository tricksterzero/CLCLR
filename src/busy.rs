//! スコープの間だけ立てる `Cell<bool>` のフラグ（`BusyGuard`）。メニュー・ダイアログのモーダルループの間や、
//! 再入しうる処理の間を示すフラグに使う（ホットキー・トレイ・ビューア・設定画面・アプリの結線が共用する）。

use std::cell::Cell;

/// フラグを立て、スコープを抜けるとき（早期の return・パニックの巻き戻しでも）に必ず下ろす。下ろすのは false に
/// するだけで、立てる前の値には戻さない（同じフラグを入れ子で使うと、内側が抜けた時点で外側の間も下りる）。
pub(crate) struct BusyGuard<'a>(&'a Cell<bool>);

impl<'a> BusyGuard<'a> {
    /// `flag` を立てる（true にする）。
    pub(crate) fn enter(flag: &'a Cell<bool>) -> Self {
        flag.set(true);
        Self(flag)
    }
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 作ると立ち、破棄すると下りる（もとが立っていても下りる）。
    #[test]
    fn raises_while_alive_and_lowers_on_drop() {
        let flag = Cell::new(false);
        {
            let _busy = BusyGuard::enter(&flag);
            assert!(flag.get());
        }
        assert!(!flag.get());

        let raised = Cell::new(true);
        drop(BusyGuard::enter(&raised));
        assert!(!raised.get(), "立てる前の値には戻さない");
    }

    /// 早期の return とパニックの巻き戻しでも下りる。
    #[test]
    fn lowers_on_early_return_and_panic() {
        fn early(flag: &Cell<bool>, stop: bool) -> bool {
            let _busy = BusyGuard::enter(flag);
            if stop {
                return true;
            }
            false
        }
        let flag = Cell::new(false);
        assert!(early(&flag, true));
        assert!(!flag.get());

        let flag = Cell::new(false);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _busy = BusyGuard::enter(&flag);
            panic!("巻き戻しの確認");
        }));
        assert!(result.is_err());
        assert!(!flag.get());
    }
}
