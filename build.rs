use std::fmt::Write as _;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=res/app.rc");
    println!("cargo:rerun-if-changed=res/settings.rc");
    println!("cargo:rerun-if-changed=res/rename.rc");
    println!("cargo:rerun-if-changed=res/resource.h");
    println!("cargo:rerun-if-changed=res/app.manifest");
    println!("cargo:rerun-if-changed=res/icons/app.ico");
    // バージョンは Cargo.toml の version から（version.h）
    println!("cargo:rerun-if-changed=Cargo.toml");
    generate_version_header();
    embed_resource::compile("res/app.rc", embed_resource::NONE)
        .manifest_required()
        .unwrap();
    generate_resource_ids();
    limit_import_search_to_system32();
}

/// exe が静的に import する DLL（KnownDLLs に無い winmm.dll・msimg32.dll など）を、system32 だけから探させる
/// （`/DEPENDENTLOADFLAG:0x800` = `LOAD_LIBRARY_SEARCH_SYSTEM32`。Windows 10 1607 以降で効く）。既定では exe と同じ
/// フォルダが先に探されるので、そこに同じ名前の DLL が置かれていると、それが読まれる（例えば、ダウンロードのフォルダに
/// 置かれていた DLL の隣へ zip を展開して起動した場合）。
fn limit_import_search_to_system32() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
    }
}

/// `OUT_DIR/version.h` に Cargo.toml の version を書く（`res/app.rc` の VERSIONINFO が読む。
/// embed-resource は `OUT_DIR` を rc.exe の include の検索パスに加える）。
fn generate_version_header() {
    let part = |name: &str| -> u16 {
        let value = std::env::var(name).unwrap();
        value.parse().unwrap_or_else(|_| panic!("{name} が 0〜65535 の数でない: {value}"))
    };
    let (major, minor, patch) = (part("CARGO_PKG_VERSION_MAJOR"), part("CARGO_PKG_VERSION_MINOR"), part("CARGO_PKG_VERSION_PATCH"));
    let header = format!(
        "// build.rs が Cargo.toml の version から作る。手で書き換えない。\n\
         #define CLCLR_VERSION_BIN {major},{minor},{patch},0\n\
         #define CLCLR_FILE_VERSION_STR \"{major}.{minor}.{patch}.0\"\n\
         #define CLCLR_PRODUCT_VERSION_STR \"{major}.{minor}.{patch}\"\n"
    );
    let path = Path::new(&std::env::var("OUT_DIR").unwrap()).join("version.h");
    std::fs::write(path, header).unwrap();
}

/// `res/resource.h` の `#define 名前 数` から、Rust の定数（`OUT_DIR/resource_ids.rs`）を作る（ID の正は
/// resource.h。設定画面の `native/settings.rs` が `include!` する）。
fn generate_resource_ids() {
    let header = std::fs::read_to_string("res/resource.h").expect("res/resource.h を読めない");
    let mut out = String::from("// build.rs が res/resource.h から作る。手で書き換えない。\n");
    for line in header.lines() {
        let mut words = line.split_whitespace();
        if words.next() != Some("#define") {
            continue;
        }
        let (Some(name), Some(value), None) = (words.next(), words.next(), words.next()) else {
            panic!("res/resource.h の #define は「#define 名前 数」の形で書く: {line}");
        };
        let value: i32 = value.parse().unwrap_or_else(|_| panic!("res/resource.h の値が数でない: {line}"));
        writeln!(out, "#[allow(dead_code)]\npub const {name}: i32 = {value};").unwrap();
    }
    let path = Path::new(&std::env::var("OUT_DIR").unwrap()).join("resource_ids.rs");
    std::fs::write(path, out).unwrap();
}
