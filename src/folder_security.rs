//! 保存先のフォルダ（exe のフォルダ）の権限を確かめる。読むだけで、権限は変えない。
//!
//! 履歴は exe と同じフォルダに暗号化せずに保存するので、ほかのアカウントがこのフォルダを読み書きできると、履歴を
//! 読まれたり、exe を差し替えられたりする。起動のたびに作業スレッドで調べ（`native::actions::spawn_folder_check`）、
//! 安全と確かめられないときだけビューアが警告する。
//!
//! - 「安全」とみなすのは、許可の設定（ACE）の相手が、自分・SYSTEM・Administrators・TrustedInstaller だけのとき。
//!   拒否の設定は考えない（実際のアクセスは、トークンの SID と ACE の並びで決まり、ここでは確かめない。警告が
//!   多めに出る側に倒す）
//! - 調べるのは、フォルダとその中のファイル（exe・設定・履歴・`blobs` の中の全部）と、ドライブのルートまでの
//!   親のフォルダ（削除・改名・権限の変更でフォルダごと差し替えられないか）
//! - 確かめられないもの（ネットワーク・権限を持たないドライブ・リンク・読めない設定・解釈できない ACE）は、
//!   「問題なし」と分けて知らせる
//!
//! この確認は利用者への助言で、改ざんを見つける仕組みではない（設定ファイルを書き換えられる者は、先に確認を
//! 止められる。差し替えられた exe は、この確認より先に動く）。

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};

// --- 判定（Win32 に依存しない） ---

/// 信頼する主体（自分のほか）。
pub const SID_SYSTEM: &str = "S-1-5-18";
pub const SID_ADMINISTRATORS: &str = "S-1-5-32-544";
pub const SID_TRUSTED_INSTALLER: &str = "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";
/// 継承するときに、作った者の SID に置き換わる（ほかの主体が新しく作れるかは、フォルダの書き込みの許可で見る）
const SID_CREATOR_OWNER: &str = "S-1-3-0";
/// 今の所有者を表す（所有者を信頼するときだけ信頼する）
const SID_OWNER_RIGHTS: &str = "S-1-3-4";

/// ACE の継承の印（`ACE_HEADER::AceFlags`）。
const OBJECT_INHERIT: u8 = 0x1;
const CONTAINER_INHERIT: u8 = 0x2;
const INHERIT_ONLY: u8 = 0x8;

/// ACE の種類（`ACE_HEADER::AceType`）。
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_DENIED_ACE_TYPE: u8 = 1;
const ACCESS_DENIED_OBJECT_ACE_TYPE: u8 = 6;
const ACCESS_DENIED_CALLBACK_ACE_TYPE: u8 = 10;
const ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE: u8 = 12;

/// ファイル・フォルダの権限（Microsoft Learn の File Access Rights Constants・Standard Access Rights）。
const FILE_READ_DATA: u32 = 0x1; // フォルダでは一覧
const FILE_WRITE_DATA: u32 = 0x2; // フォルダではファイルの追加
const FILE_APPEND_DATA: u32 = 0x4; // フォルダではフォルダの追加
const FILE_WRITE_EA: u32 = 0x10;
const FILE_DELETE_CHILD: u32 = 0x40;
const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
const DELETE: u32 = 0x1_0000;
const WRITE_DAC: u32 = 0x4_0000;
const WRITE_OWNER: u32 = 0x8_0000;
const GENERIC_ALL: u32 = 0x1000_0000;
const GENERIC_EXECUTE: u32 = 0x2000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const GENERIC_READ: u32 = 0x8000_0000;
/// 汎用の権限をファイルの権限へ読み替えたもの（Microsoft Learn の File Security and Access Rights）。
const FILE_GENERIC_READ: u32 = 0x12_0089;
const FILE_GENERIC_WRITE: u32 = 0x12_0116;
const FILE_GENERIC_EXECUTE: u32 = 0x12_00A0;
const FILE_ALL_ACCESS: u32 = 0x1F_01FF;

/// 書き換え・削除・権限の変更に当たる権限。
const WRITE_RIGHTS: u32 = CONTENT_RIGHTS | PERMISSION_RIGHTS;
/// 書き換え・削除に当たる権限（`WRITE_RIGHTS` のうち権限・所有者の変更でないもの）。
const CONTENT_RIGHTS: u32 = FILE_WRITE_DATA | FILE_APPEND_DATA | FILE_WRITE_EA | FILE_DELETE_CHILD | FILE_WRITE_ATTRIBUTES | DELETE;
/// 権限・所有者の変更に当たる権限。
const PERMISSION_RIGHTS: u32 = WRITE_DAC | WRITE_OWNER;

/// 汎用の権限（GENERIC_*）を、ファイルの個別の権限へ広げる（`MapGenericMask` と同じ読み替え）。
fn map_generic(mask: u32) -> u32 {
    let mut out = mask & !(GENERIC_ALL | GENERIC_EXECUTE | GENERIC_WRITE | GENERIC_READ);
    for (generic, specific) in [
        (GENERIC_READ, FILE_GENERIC_READ),
        (GENERIC_WRITE, FILE_GENERIC_WRITE),
        (GENERIC_EXECUTE, FILE_GENERIC_EXECUTE),
        (GENERIC_ALL, FILE_ALL_ACCESS),
    ] {
        if mask & generic != 0 {
            out |= specific;
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AceKind {
    Allow,
    Deny,
    /// 解釈しない種類（条件付きの許可など）。確かめられないものとして扱う
    Other(u8),
}

impl AceKind {
    fn from_type(ace_type: u8) -> Self {
        match ace_type {
            ACCESS_ALLOWED_ACE_TYPE => Self::Allow,
            ACCESS_DENIED_ACE_TYPE
            | ACCESS_DENIED_OBJECT_ACE_TYPE
            | ACCESS_DENIED_CALLBACK_ACE_TYPE
            | ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE => Self::Deny,
            other => Self::Other(other),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ace {
    pub kind: AceKind,
    pub flags: u8,
    pub mask: u32,
    /// 文字列の SID（`S-1-...`）。種類が `Other` のときは空のことがある
    pub sid: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dacl {
    /// DACL が無い・NULL（誰にでもすべてを許す）
    Null,
    Entries(Vec<Ace>),
}

/// 1つの項目の所有者と DACL。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Security {
    pub owner: String,
    pub dacl: Dacl,
}

/// 項目の役目（どの権限を問題にするか）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// 設定・履歴のファイル（読めるのも、書けるのも問題）
    DataFile,
    /// CLCLR.exe（書ける・消せるのが問題。読めるのは問題にしない）
    Exe,
    /// 保存先のフォルダ・`blobs`（ファイルの追加・削除・権限の変更と、中に新しく作るものへ引き継がれる許可が問題。
    /// 一覧を読めるのは問題にしない）
    Folder,
    /// 親のフォルダ（それ自身と、その中の項目の削除・改名・権限の変更が問題）。ドライブのルートは改名できない
    Ancestor { root: bool },
}

/// 問題の種類。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Concern {
    /// ほかの主体が中身を読める
    Read,
    /// ほかの主体が書き換え・削除をできる（フォルダでは、中の項目の追加・削除を含む）
    Write,
    /// 中に新しく作るファイル・フォルダへ、ほかの主体への許可が引き継がれる
    Inherit,
    /// 親のフォルダで、ほかの主体がそのフォルダかその中の項目を削除・改名できる
    Replace,
    /// ほかの主体が権限・所有者を変えられる
    Permissions,
    /// 所有者が信頼する主体でない（所有者は権限を変えられる）
    Owner,
    /// DACL が無い（誰でもすべての操作ができる）
    NullDacl,
}

/// 自分（このプロセスのユーザー）の SID と、信頼する主体の判定。
#[derive(Clone, Debug)]
pub struct Trusted {
    user: String,
}

impl Trusted {
    pub fn new(user_sid: impl Into<String>) -> Self {
        Self { user: user_sid.into() }
    }

    fn owner(&self, sid: &str) -> bool {
        sid == self.user || [SID_SYSTEM, SID_ADMINISTRATORS, SID_TRUSTED_INSTALLER].contains(&sid)
    }

    /// ACE の相手を信頼するか（OWNER RIGHTS は所有者を信頼するときだけ）。
    fn grantee(&self, sid: &str, owner_trusted: bool) -> bool {
        self.owner(sid) || sid == SID_CREATOR_OWNER || (sid == SID_OWNER_RIGHTS && owner_trusted)
    }
}

/// 1つの項目の判定の結果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Evaluation {
    /// 問題と、その相手の SID（並びは出てきた順、重複なし）
    pub concerns: Vec<(Concern, Vec<String>)>,
    /// 解釈できない ACE があった
    pub unknown_ace: bool,
}

impl Evaluation {
    fn add(&mut self, concern: Concern, sid: &str) {
        match self.concerns.iter_mut().find(|(c, _)| *c == concern) {
            Some((_, sids)) => {
                if !sids.iter().any(|s| s == sid) {
                    sids.push(sid.to_string());
                }
            }
            None => self.concerns.push((concern, vec![sid.to_string()])),
        }
    }
}

/// 1つの項目を役目に従って判定する。
pub fn evaluate(security: &Security, role: Role, trusted: &Trusted) -> Evaluation {
    let mut out = Evaluation::default();
    let owner_trusted = trusted.owner(&security.owner);
    if !owner_trusted {
        out.add(Concern::Owner, &security.owner);
    }
    let aces = match &security.dacl {
        Dacl::Null => {
            out.concerns.push((Concern::NullDacl, Vec::new()));
            return out;
        }
        Dacl::Entries(aces) => aces,
    };
    for ace in aces {
        match ace.kind {
            AceKind::Allow => {}
            AceKind::Deny => continue,
            AceKind::Other(_) => {
                out.unknown_ace = true;
                continue;
            }
        }
        if trusted.grantee(&ace.sid, owner_trusted) {
            continue;
        }
        let mask = map_generic(ace.mask);
        // 継承専用の ACE は、その項目自身には効かない
        let effective = ace.flags & INHERIT_ONLY == 0;
        match role {
            Role::DataFile => {
                if effective && mask & FILE_READ_DATA != 0 {
                    out.add(Concern::Read, &ace.sid);
                }
                if effective && mask & CONTENT_RIGHTS != 0 {
                    out.add(Concern::Write, &ace.sid);
                }
            }
            Role::Exe => {
                if effective && mask & CONTENT_RIGHTS != 0 {
                    out.add(Concern::Write, &ace.sid);
                }
            }
            Role::Folder => {
                if effective && mask & CONTENT_RIGHTS != 0 {
                    out.add(Concern::Write, &ace.sid);
                }
                // ファイルへ引き継がれる ACE は読み取り・書き込みのどちらも、フォルダだけへ引き継がれる ACE は書き込みを
                // 問題にする（フォルダの読み取りは一覧）
                let to_files = ace.flags & OBJECT_INHERIT != 0 && mask & (FILE_READ_DATA | WRITE_RIGHTS) != 0;
                let to_folders = ace.flags & CONTAINER_INHERIT != 0 && mask & WRITE_RIGHTS != 0;
                if to_files || to_folders {
                    out.add(Concern::Inherit, &ace.sid);
                }
            }
            Role::Ancestor { root } => {
                let delete_self = if root { 0 } else { DELETE };
                if effective && mask & (delete_self | FILE_DELETE_CHILD) != 0 {
                    out.add(Concern::Replace, &ace.sid);
                }
            }
        }
        // どの役目でも、権限・所有者を変えられれば、ほかの許可を自分に付けられる
        if effective && mask & PERMISSION_RIGHTS != 0 {
            out.add(Concern::Permissions, &ace.sid);
        }
    }
    out
}

// --- 結果 ---

/// 見つかった問題（1つの項目の1つの種類）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub path: PathBuf,
    pub concern: Concern,
    /// 相手の SID（所有者のときは所有者。DACL が無いときは空）
    pub sids: Vec<String>,
    /// 親のフォルダの問題か（保存先のフォルダとその中の項目なら false）
    pub ancestor: bool,
}

/// 確かめられなかったもの。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unverified {
    /// ネットワーク上のフォルダ
    Remote,
    /// ドライブが権限の設定を持たない（FAT・exFAT など）
    NoAcl,
    /// 実際のパスが起動したパスと違う（リンク・別名のドライブを経由している）
    OtherPath(PathBuf),
    /// リンク（ジャンクション・シンボリックリンクなど）
    Link(PathBuf),
    /// 権限を読めない
    Unreadable { path: PathBuf, error: String },
    /// 解釈できない種類の ACE がある
    UnknownAce(PathBuf),
    /// `blobs` の中のフォルダが深すぎるので、中を確かめていない（`BLOBS_MAX_DEPTH`）
    TooDeep(PathBuf),
}

/// 確かめた結果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FolderReport {
    /// 保存先のフォルダ
    pub dir: PathBuf,
    pub findings: Vec<Finding>,
    pub unverified: Vec<Unverified>,
    /// SID からアカウント名（引けたものだけ）
    pub names: BTreeMap<String, String>,
}

impl FolderReport {
    /// 安全と確かめられたか（問題も、確かめられなかったものも無い）。
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty() && self.unverified.is_empty()
    }

    /// SID の表示名（アカウント名が引けなければ SID）。
    pub fn name<'a>(&'a self, sid: &'a str) -> &'a str {
        self.names.get(sid).map_or(sid, String::as_str)
    }

    fn push(&mut self, path: &Path, evaluation: Evaluation, ancestor: bool) {
        for (concern, sids) in evaluation.concerns {
            self.findings.push(Finding { path: path.to_path_buf(), concern, sids, ancestor });
        }
        if evaluation.unknown_ace {
            self.unverified.push(Unverified::UnknownAce(path.to_path_buf()));
        }
    }
}

impl fmt::Display for FolderReport {
    /// ログに書く文（出せなかったとき）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "保存先のフォルダの権限の確認（{}）: 問題 {} 件、確かめられないもの {} 件",
            self.dir.display(),
            self.findings.len(),
            self.unverified.len()
        )
    }
}

/// 設定・履歴のファイルの名前か（保存先のフォルダの直下。一時ファイル `<名前>.tmp` を含む）。
fn is_data_file_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let base = lower.strip_suffix(".tmp").unwrap_or(&lower);
    matches!(base, "config.toml" | "history.toml" | "pinned.toml" | "panic.log")
}

// --- 読み取り（Win32） ---

/// 項目の権限・属性を読む手段（テストでは差し替えない。実際のファイルで確かめる）。
mod win {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, LocalFree, ERROR_SUCCESS, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSidToSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
    };
    use windows::Win32::Security::{
        GetAce, GetSecurityDescriptorControl, GetTokenInformation, LookupAccountSidW, TokenUser, ACCESS_ALLOWED_ACE,
        ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        SID_NAME_USE, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FindClose, FindFirstFileW, GetDriveTypeW, GetFinalPathNameByHandleW, GetVolumeInformationW,
        GetVolumePathNameW, FILE_FLAG_BACKUP_SEMANTICS, FILE_NAME_NORMALIZED, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING, WIN32_FIND_DATAW,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use super::{Ace, AceKind, Dacl, Security};

    /// `SECURITY_DESCRIPTOR_CONTROL` の SE_DACL_PRESENT。
    const SE_DACL_PRESENT: u16 = 0x4;
    /// `GetDriveTypeW` の DRIVE_REMOTE。
    const DRIVE_REMOTE: u32 = 4;
    /// `GetVolumeInformationW` の FILE_PERSISTENT_ACLS。
    const FILE_PERSISTENT_ACLS: u32 = 0x8;
    /// `FILE_READ_ATTRIBUTES`（属性を読むだけで開く）。
    const FILE_READ_ATTRIBUTES: u32 = 0x80;

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain([0]).collect()
    }

    /// `LocalFree` で放すメモリ（`GetNamedSecurityInfoW`・`ConvertSidToStringSidW` などが返すもの）。
    struct Local(*mut core::ffi::c_void);

    impl Drop for Local {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let _ = LocalFree(Some(HLOCAL(self.0)));
                }
            }
        }
    }

    /// SID を文字列（`S-1-...`）にする。
    fn sid_string(sid: PSID) -> Result<String, String> {
        let mut text = PWSTR::null();
        unsafe { ConvertSidToStringSidW(sid, &mut text) }.map_err(|e| e.to_string())?;
        let _free = Local(text.0.cast());
        unsafe { text.to_string() }.map_err(|e| e.to_string())
    }

    /// このプロセスのユーザーの SID。
    pub fn current_user_sid() -> Result<String, String> {
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).map_err(|e| e.to_string())?;
            let mut len = 0u32;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
            let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
            let result = GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr().cast()), len, &mut len);
            let _ = CloseHandle(token);
            result.map_err(|e| e.to_string())?;
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            sid_string(user.User.Sid)
        }
    }

    /// 項目の所有者と DACL を読む。
    pub fn read_security(path: &Path) -> Result<Security, String> {
        let name = wide(path);
        let mut owner = PSID::default();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        let error = unsafe {
            GetNamedSecurityInfoW(
                PCWSTR(name.as_ptr()),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                Some(&mut owner),
                None,
                Some(&mut dacl),
                None,
                &mut descriptor,
            )
        };
        if error != ERROR_SUCCESS {
            return Err(windows::core::Error::from_hresult(error.to_hresult()).to_string());
        }
        let _free = Local(descriptor.0);
        if owner.is_invalid() {
            return Err("所有者が無い".to_string());
        }
        let owner = sid_string(owner)?;
        let mut control = 0u16;
        let mut revision = 0u32;
        unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) }.map_err(|e| e.to_string())?;
        if control & SE_DACL_PRESENT == 0 || dacl.is_null() {
            return Ok(Security { owner, dacl: Dacl::Null });
        }
        let count = unsafe { (*dacl).AceCount };
        let mut aces = Vec::with_capacity(count as usize);
        for i in 0..u32::from(count) {
            let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
            unsafe { GetAce(dacl, i, &mut raw) }.map_err(|e| e.to_string())?;
            let header = unsafe { &*(raw as *const ACE_HEADER) };
            let kind = AceKind::from_type(header.AceType);
            // 基本の許可・拒否（ACCESS_ALLOWED_ACE と ACCESS_DENIED_ACE は同じ形）だけ相手と権限を読む（ほかの形の
            // 拒否は判定で使わず、解釈しない種類は確かめられないものにする）
            let (mask, sid) = if header.AceType == super::ACCESS_ALLOWED_ACE_TYPE || header.AceType == super::ACCESS_DENIED_ACE_TYPE {
                let ace = unsafe { &*(raw as *const ACCESS_ALLOWED_ACE) };
                (ace.Mask, sid_string(PSID(&ace.SidStart as *const u32 as *mut _))?)
            } else {
                (0, String::new())
            };
            aces.push(Ace { kind, flags: header.AceFlags, mask, sid });
        }
        Ok(Security { owner, dacl: Dacl::Entries(aces) })
    }

    /// リンク（ジャンクション・シンボリックリンクなど）か。クラウドのファイル（OneDrive など）の印は除く
    /// （`IO_REPARSE_TAG_CLOUD_*`）。読めなければ Err。
    pub fn is_link(path: &Path, attributes: u32) -> Result<bool, String> {
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
            return Ok(false);
        }
        let name = wide(path);
        let mut data = WIN32_FIND_DATAW::default();
        let find = unsafe { FindFirstFileW(PCWSTR(name.as_ptr()), &mut data) }.map_err(|e| e.to_string())?;
        unsafe {
            let _ = FindClose(find);
        }
        // IO_REPARSE_TAG_CLOUD と IO_REPARSE_TAG_CLOUD_1〜F（0x9000001A・0x9000101A … 0x9000F01A）
        Ok(data.dwReserved0 & 0xFFFF_0FFF != 0x9000_001A)
    }

    /// ボリュームのルート（`C:\` など）。
    pub fn volume_root(path: &Path) -> Result<Vec<u16>, String> {
        let name = wide(path);
        let mut root = [0u16; 1024];
        unsafe { GetVolumePathNameW(PCWSTR(name.as_ptr()), &mut root) }.map_err(|e| e.to_string())?;
        let len = root.iter().position(|&c| c == 0).unwrap_or(root.len());
        Ok(root[..=len.min(root.len() - 1)].to_vec())
    }

    /// ネットワークのドライブか。
    pub fn is_remote(root: &[u16]) -> bool {
        unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) == DRIVE_REMOTE }
    }

    /// ボリュームが権限の設定を持つか。
    pub fn has_persistent_acls(root: &[u16]) -> Result<bool, String> {
        let mut flags = 0u32;
        unsafe { GetVolumeInformationW(PCWSTR(root.as_ptr()), None, None, None, Some(&mut flags), None) }
            .map_err(|e| e.to_string())?;
        Ok(flags & FILE_PERSISTENT_ACLS != 0)
    }

    /// 開いて得た実際のパス（`\\?\` を除く）。
    pub fn final_path(path: &Path) -> Result<String, String> {
        let name = wide(path);
        let handle = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )
        }
        .map_err(|e| e.to_string())?;
        let mut buf = vec![0u16; 32_768];
        let len = unsafe { GetFinalPathNameByHandleW(handle, &mut buf, FILE_NAME_NORMALIZED) } as usize;
        unsafe {
            let _ = CloseHandle(handle);
        }
        if len == 0 || len >= buf.len() {
            return Err("実際のパスを読めない".to_string());
        }
        let text = String::from_utf16_lossy(&buf[..len]);
        Ok(match text.strip_prefix(r"\\?\UNC\") {
            Some(rest) => format!(r"\\{rest}"),
            None => text.strip_prefix(r"\\?\").unwrap_or(&text).to_string(),
        })
    }

    /// SID のアカウント名（`DOMAIN\name`）。引けなければ None。
    pub fn account_name(sid: &str) -> Option<String> {
        let text: Vec<u16> = sid.encode_utf16().chain([0]).collect();
        let mut psid = PSID::default();
        unsafe { ConvertStringSidToSidW(PCWSTR(text.as_ptr()), &mut psid) }.ok()?;
        let _free = Local(psid.0);
        let (mut name, mut domain) = ([0u16; 256], [0u16; 256]);
        let (mut name_len, mut domain_len) = (name.len() as u32, domain.len() as u32);
        let mut kind = SID_NAME_USE::default();
        unsafe {
            LookupAccountSidW(
                PCWSTR::null(),
                psid,
                Some(PWSTR(name.as_mut_ptr())),
                &mut name_len,
                Some(PWSTR(domain.as_mut_ptr())),
                &mut domain_len,
                &mut kind,
            )
        }
        .ok()?;
        let name = String::from_utf16_lossy(&name[..name_len as usize]);
        let domain = String::from_utf16_lossy(&domain[..domain_len as usize]);
        Some(if domain.is_empty() { name } else { format!(r"{domain}\{name}") })
    }
}

pub use win::current_user_sid;

/// 拡張長のパスの接頭辞を外したパス（`\\?\C:\…` → `C:\…`）。ほかの形（`\\?\UNC\…` などネットワークの場所を含む）は
/// そのまま。
fn without_verbatim_disk_prefix(dir: &Path) -> PathBuf {
    use std::path::{Component, Prefix};
    let mut components = dir.components();
    match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::VerbatimDisk(letter) => {
                let mut out = PathBuf::from(format!("{}:\\", char::from(letter)));
                out.extend(components.filter(|c| !matches!(c, Component::RootDir)));
                out
            }
            _ => dir.to_path_buf(),
        },
        _ => dir.to_path_buf(),
    }
}

/// 保存先のフォルダ `dir`（`exe_name` は CLCLR.exe のファイル名）を確かめる。読むだけで、何も変えない。
pub fn check(dir: &Path, exe_name: &OsStr) -> FolderReport {
    // exe の場所が `\\?\C:\…` の形で届いても、ローカルのパスとして確かめる（`\\` で始まるのでネットワークとみなさない）。
    // 結果の `dir` も同じ形にする（見つかった項目のパスと比べて、本文の書き分けに使うため）
    let dir = &without_verbatim_disk_prefix(dir);
    let mut report = FolderReport { dir: dir.to_path_buf(), ..FolderReport::default() };
    let unreadable = |error: String| Unverified::Unreadable { path: dir.to_path_buf(), error };
    let trusted = match current_user_sid() {
        Ok(sid) => Trusted::new(sid),
        Err(e) => {
            report.unverified.push(unreadable(format!("自分のアカウントを確かめられません（{e}）")));
            return report;
        }
    };
    if dir.as_os_str().to_string_lossy().starts_with(r"\\") {
        report.unverified.push(Unverified::Remote);
        return report;
    }
    match win::volume_root(dir) {
        Ok(root) if win::is_remote(&root) => {
            report.unverified.push(Unverified::Remote);
            return report;
        }
        Ok(root) => match win::has_persistent_acls(&root) {
            Ok(true) => {}
            Ok(false) => {
                report.unverified.push(Unverified::NoAcl);
                return report;
            }
            Err(e) => {
                report.unverified.push(unreadable(e));
                return report;
            }
        },
        Err(e) => {
            report.unverified.push(unreadable(e));
            return report;
        }
    }
    match win::final_path(dir) {
        Ok(actual) if !same_path(&actual, dir) => report.unverified.push(Unverified::OtherPath(PathBuf::from(actual))),
        Ok(_) => {}
        Err(e) => report.unverified.push(unreadable(e)),
    }
    check_ancestors(dir, &trusted, &mut report);
    check_contents(dir, exe_name, &trusted, &mut report);
    // 同じ SID は1回だけ引く（引けなかった SID も、見つかった数だけ引き直さない）
    let sids: BTreeSet<String> = report.findings.iter().flat_map(|f| f.sids.iter().cloned()).collect();
    for sid in sids {
        if let Some(name) = win::account_name(&sid) {
            report.names.insert(sid, name);
        }
    }
    report
}

/// 実際のパスと起動したパスが同じか（大文字・小文字と末尾の区切りは区別しない）。
fn same_path(actual: &str, dir: &Path) -> bool {
    let norm = |s: &str| s.trim_end_matches('\\').to_uppercase();
    norm(actual) == norm(&dir.to_string_lossy())
}

/// 項目の属性（リンクか）と権限を読んで判定し、結果に足す。中を調べてよい（リンクでない）なら true。読めないものは
/// 確かめられないものにし、調べている間に消えた（保存の置き換え・削除。`NotFound` で確かめたとき）ものだけを数えない。
fn inspect(path: &Path, attributes: u32, role: Role, trusted: &Trusted, report: &mut FolderReport) -> bool {
    match win::is_link(path, attributes) {
        Ok(false) => {}
        Ok(true) => {
            report.unverified.push(Unverified::Link(path.to_path_buf()));
            return false;
        }
        Err(_) if is_gone(path) => return false,
        Err(error) => {
            report.unverified.push(Unverified::Unreadable { path: path.to_path_buf(), error });
            return false;
        }
    }
    match win::read_security(path) {
        Ok(security) => report.push(path, evaluate(&security, role, trusted), matches!(role, Role::Ancestor { .. })),
        Err(_) if is_gone(path) => return false,
        Err(error) => report.unverified.push(Unverified::Unreadable { path: path.to_path_buf(), error }),
    }
    true
}

/// 項目が無いと確かめられたか（`Path::exists` は権限の誤りでも偽を返すので使わない）。
fn is_gone(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

/// 項目の属性。無ければ None（数えない）、ほかの誤りは確かめられないものにして None。
fn attributes(path: &Path, report: &mut FolderReport) -> Option<u32> {
    use std::os::windows::fs::MetadataExt;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Some(metadata.file_attributes()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            report.unverified.push(Unverified::Unreadable { path: path.to_path_buf(), error: e.to_string() });
            None
        }
    }
}

/// フォルダの中の項目（名前と属性）。列挙できなかったもの・1件ずつの誤りは確かめられないものにする。
fn entries(dir: &Path, report: &mut FolderReport) -> Vec<(PathBuf, std::ffi::OsString, u32)> {
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(e) => {
            report.unverified.push(Unverified::Unreadable { path: dir.to_path_buf(), error: e.to_string() });
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for entry in read {
        match entry {
            Ok(entry) => {
                let path = entry.path();
                if let Some(attrs) = attributes(&path, report) {
                    out.push((path, entry.file_name(), attrs));
                }
            }
            Err(e) => report.unverified.push(Unverified::Unreadable { path: dir.to_path_buf(), error: e.to_string() }),
        }
    }
    out
}

/// `FILE_ATTRIBUTE_DIRECTORY`。
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// 親のフォルダ（ドライブのルートまで）を確かめる。
fn check_ancestors(dir: &Path, trusted: &Trusted, report: &mut FolderReport) {
    for ancestor in dir.ancestors().skip(1) {
        let root = ancestor.parent().is_none();
        if let Some(attrs) = attributes(ancestor, report) {
            inspect(ancestor, if root { 0 } else { attrs }, Role::Ancestor { root }, trusted, report);
        }
    }
}

/// 保存先のフォルダと、その中の exe・設定・履歴（`blobs` の中の全部）を確かめる。
pub(crate) fn check_contents(dir: &Path, exe_name: &OsStr, trusted: &Trusted, report: &mut FolderReport) {
    let Some(attrs) = attributes(dir, report) else {
        // 読めないときは `attributes` が知らせた。無いときだけここで知らせる
        if is_gone(dir) {
            report.unverified.push(Unverified::Unreadable { path: dir.to_path_buf(), error: "フォルダが見つかりません".to_string() });
        }
        return;
    };
    if !inspect(dir, attrs, Role::Folder, trusted, report) {
        return;
    }
    for (path, name, attrs) in entries(dir, report) {
        let name = name.to_string_lossy();
        if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
            if name.eq_ignore_ascii_case("blobs") && inspect(&path, attrs, Role::Folder, trusted, report) {
                check_blobs(&path, trusted, report);
            }
        } else if name.eq_ignore_ascii_case(&exe_name.to_string_lossy()) {
            inspect(&path, attrs, Role::Exe, trusted, report);
        } else if is_data_file_name(&name) {
            inspect(&path, attrs, Role::DataFile, trusted, report);
        }
    }
}

/// `blobs` の中のフォルダをたどる深さの上限（`blobs` の直下が 1 段目）。CLCLR は中にフォルダを作らないので、外から
/// 作られた深いフォルダで、スタックやメモリを使い切らないための歯止め。
const BLOBS_MAX_DEPTH: usize = 16;

/// `blobs` の中の全部を確かめる（中のフォルダは CLCLR が作らないが、あればその中もたどる。リンクはたどらない）。
/// 再帰せず、たどるフォルダの一覧で回す。`BLOBS_MAX_DEPTH` より深いフォルダは、中を確かめずに「確かめられない」にする。
fn check_blobs(folder: &Path, trusted: &Trusted, report: &mut FolderReport) {
    let mut pending = vec![(folder.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = pending.pop() {
        for (path, _, attrs) in entries(&dir, report) {
            if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
                if depth >= BLOBS_MAX_DEPTH {
                    report.unverified.push(Unverified::TooDeep(path));
                } else if inspect(&path, attrs, Role::Folder, trusted, report) {
                    pending.push((path, depth + 1));
                }
            } else {
                inspect(&path, attrs, Role::DataFile, trusted, report);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "S-1-5-21-1-2-3-1001";
    const OTHER: &str = "S-1-5-21-1-2-3-1002";
    const AUTHENTICATED_USERS: &str = "S-1-5-11";
    const USERS: &str = "S-1-5-32-545";

    fn allow(sid: &str, flags: u8, mask: u32) -> Ace {
        Ace { kind: AceKind::Allow, flags, mask, sid: sid.to_string() }
    }

    fn sec(owner: &str, aces: Vec<Ace>) -> Security {
        Security { owner: owner.to_string(), dacl: Dacl::Entries(aces) }
    }

    fn trusted() -> Trusted {
        Trusted::new(ME)
    }

    fn concerns(e: &Evaluation) -> Vec<Concern> {
        e.concerns.iter().map(|(c, _)| *c).collect()
    }

    /// 既定のユーザーのフォルダの形（自分・SYSTEM・Administrators の全権限）は、どの役目でも問題なし。
    #[test]
    fn owner_only_folder_is_clean_in_every_role() {
        let s = sec(
            ME,
            vec![
                allow(ME, OBJECT_INHERIT | CONTAINER_INHERIT, FILE_ALL_ACCESS),
                allow(SID_SYSTEM, OBJECT_INHERIT | CONTAINER_INHERIT, FILE_ALL_ACCESS),
                allow(SID_ADMINISTRATORS, OBJECT_INHERIT | CONTAINER_INHERIT, GENERIC_ALL),
                allow(SID_CREATOR_OWNER, OBJECT_INHERIT | CONTAINER_INHERIT | INHERIT_ONLY, GENERIC_ALL),
            ],
        );
        for role in [Role::DataFile, Role::Exe, Role::Folder, Role::Ancestor { root: false }, Role::Ancestor { root: true }] {
            assert_eq!(evaluate(&s, role, &trusted()), Evaluation::default(), "{role:?}");
        }
    }

    /// `C:\` の直下に作ったフォルダの形（Authenticated Users の M と、子への継承専用の汎用権限、Users の RX）。
    #[test]
    fn folder_under_drive_root_is_flagged() {
        let m = 0x0013_01BF;
        let s = sec(
            ME,
            vec![
                allow(SID_ADMINISTRATORS, OBJECT_INHERIT | CONTAINER_INHERIT, FILE_ALL_ACCESS),
                allow(USERS, OBJECT_INHERIT | CONTAINER_INHERIT, 0x0012_00A9),
                allow(AUTHENTICATED_USERS, 0, m),
                allow(AUTHENTICATED_USERS, OBJECT_INHERIT | CONTAINER_INHERIT | INHERIT_ONLY, 0xE001_0000),
            ],
        );
        let t = trusted();
        let folder = evaluate(&s, Role::Folder, &t);
        assert_eq!(concerns(&folder), [Concern::Inherit, Concern::Write]);
        assert_eq!(folder.concerns[0].1, [USERS, AUTHENTICATED_USERS], "Users の RX はファイルへ引き継がれて読める");
        assert_eq!(folder.concerns[1].1, [AUTHENTICATED_USERS]);
        // 親として: M は DELETE を含む（改名できる）。Users の RX は問題にしない
        let ancestor = evaluate(&s, Role::Ancestor { root: false }, &t);
        assert_eq!(ancestor.concerns, [(Concern::Replace, vec![AUTHENTICATED_USERS.to_string()])]);
        // ファイルとして（継承された形: 継承専用でない）
        let file = sec(ME, vec![allow(USERS, 0x10, 0x0012_00A9), allow(AUTHENTICATED_USERS, 0x10, m)]);
        let data = evaluate(&file, Role::DataFile, &t);
        assert_eq!(data.concerns, [(Concern::Read, vec![USERS.to_string(), AUTHENTICATED_USERS.to_string()]), (Concern::Write, vec![AUTHENTICATED_USERS.to_string()])]);
        let exe = evaluate(&file, Role::Exe, &t);
        assert_eq!(exe.concerns, [(Concern::Write, vec![AUTHENTICATED_USERS.to_string()])], "exe は読めるだけなら問題にしない");
    }

    /// ドライブのルート: 継承専用の ACE は自身に効かない。改名はできないので DELETE は問題にしないが、子の削除・
    /// 権限の変更は問題にする。
    #[test]
    fn drive_root_ignores_inherit_only_and_own_delete() {
        let t = trusted();
        let root = sec(
            SID_TRUSTED_INSTALLER,
            vec![
                allow(AUTHENTICATED_USERS, OBJECT_INHERIT | CONTAINER_INHERIT | INHERIT_ONLY, 0xE001_0000),
                allow(AUTHENTICATED_USERS, 0, FILE_APPEND_DATA),
                allow(USERS, OBJECT_INHERIT | CONTAINER_INHERIT, 0x0012_00A9),
            ],
        );
        assert_eq!(evaluate(&root, Role::Ancestor { root: true }, &t), Evaluation::default());
        let deletes = sec(SID_SYSTEM, vec![allow(OTHER, 0, DELETE)]);
        assert_eq!(evaluate(&deletes, Role::Ancestor { root: true }, &t), Evaluation::default());
        assert_eq!(concerns(&evaluate(&deletes, Role::Ancestor { root: false }, &t)), [Concern::Replace]);
        let cases: [(u32, &[Concern]); 4] = [
            (FILE_DELETE_CHILD, &[Concern::Replace]),
            (WRITE_DAC, &[Concern::Permissions]),
            (WRITE_OWNER, &[Concern::Permissions]),
            (GENERIC_ALL, &[Concern::Replace, Concern::Permissions]),
        ];
        for (mask, expected) in cases {
            let s = sec(SID_SYSTEM, vec![allow(OTHER, 0, mask)]);
            assert_eq!(concerns(&evaluate(&s, Role::Ancestor { root: true }, &t)), expected, "{mask:#x}");
        }
    }

    /// 書き換え・削除と、権限・所有者の変更は別の問題として出す（「変更」の権限 M では権限を変えられない）。
    #[test]
    fn permission_change_is_reported_separately() {
        let t = trusted();
        let m = 0x0013_01BF;
        for role in [Role::DataFile, Role::Exe, Role::Folder, Role::Ancestor { root: false }] {
            let only_dac = evaluate(&sec(ME, vec![allow(OTHER, 0, WRITE_DAC)]), role, &t);
            assert_eq!(concerns(&only_dac), [Concern::Permissions], "{role:?}");
            let modify = evaluate(&sec(ME, vec![allow(OTHER, 0, m)]), role, &t);
            assert!(!concerns(&modify).contains(&Concern::Permissions), "{role:?}");
        }
        let full = evaluate(&sec(ME, vec![allow(OTHER, 0, FILE_ALL_ACCESS)]), Role::DataFile, &t);
        assert_eq!(concerns(&full), [Concern::Read, Concern::Write, Concern::Permissions]);
    }

    /// DACL が無い・所有者がほかの主体・解釈できない ACE・OWNER RIGHTS・拒否の ACE。
    #[test]
    fn null_dacl_owner_unknown_ace_and_owner_rights() {
        let t = trusted();
        let null = Security { owner: ME.to_string(), dacl: Dacl::Null };
        for role in [Role::DataFile, Role::Exe, Role::Folder, Role::Ancestor { root: false }, Role::Ancestor { root: true }] {
            assert_eq!(concerns(&evaluate(&null, role, &t)), [Concern::NullDacl], "{role:?}");
        }
        // 所有者がほかのアカウント: OWNER RIGHTS の許可も信頼しない
        let other_owner = sec(OTHER, vec![allow(SID_OWNER_RIGHTS, 0, FILE_GENERIC_READ)]);
        let e = evaluate(&other_owner, Role::DataFile, &t);
        assert_eq!(e.concerns, [(Concern::Owner, vec![OTHER.to_string()]), (Concern::Read, vec![SID_OWNER_RIGHTS.to_string()])]);
        // 所有者が自分なら OWNER RIGHTS は自分
        let mine = sec(ME, vec![allow(SID_OWNER_RIGHTS, 0, FILE_ALL_ACCESS)]);
        assert_eq!(evaluate(&mine, Role::DataFile, &t), Evaluation::default());
        // Administrators の所有は問題にしない
        assert_eq!(evaluate(&sec(SID_ADMINISTRATORS, vec![]), Role::Folder, &t), Evaluation::default());
        // 拒否の ACE は考えない（許可があれば問題にする）
        let denied = sec(ME, vec![Ace { kind: AceKind::Deny, flags: 0, mask: FILE_ALL_ACCESS, sid: OTHER.to_string() }, allow(OTHER, 0, FILE_GENERIC_READ)]);
        assert_eq!(concerns(&evaluate(&denied, Role::DataFile, &t)), [Concern::Read]);
        // 解釈できない ACE（条件付きの許可など）は確かめられないもの
        let unknown = sec(ME, vec![Ace { kind: AceKind::from_type(9), flags: 0, mask: 0, sid: String::new() }]);
        let e = evaluate(&unknown, Role::DataFile, &t);
        assert!(e.unknown_ace && e.concerns.is_empty());
    }

    /// 継承の印: ファイルへ引き継がれる読み取り、フォルダだけへ引き継がれる一覧（問題にしない）と書き込み。
    #[test]
    fn folder_inheritance_distinguishes_files_and_folders() {
        let t = trusted();
        let to_files_read = sec(ME, vec![allow(OTHER, OBJECT_INHERIT | INHERIT_ONLY, FILE_GENERIC_READ)]);
        assert_eq!(concerns(&evaluate(&to_files_read, Role::Folder, &t)), [Concern::Inherit]);
        let to_folders_list = sec(ME, vec![allow(OTHER, CONTAINER_INHERIT | INHERIT_ONLY, FILE_GENERIC_READ)]);
        assert_eq!(evaluate(&to_folders_list, Role::Folder, &t), Evaluation::default());
        let to_folders_write = sec(ME, vec![allow(OTHER, CONTAINER_INHERIT | INHERIT_ONLY, FILE_GENERIC_WRITE)]);
        assert_eq!(concerns(&evaluate(&to_folders_write, Role::Folder, &t)), [Concern::Inherit]);
        // 継承されない、フォルダ自身の一覧の読み取りは問題にしない
        let list = sec(ME, vec![allow(OTHER, 0, FILE_GENERIC_READ)]);
        assert_eq!(evaluate(&list, Role::Folder, &t), Evaluation::default());
    }

    #[test]
    fn generic_rights_are_mapped_to_file_rights() {
        assert_eq!(map_generic(GENERIC_READ), FILE_GENERIC_READ);
        assert_eq!(map_generic(GENERIC_WRITE) & FILE_WRITE_DATA, FILE_WRITE_DATA);
        assert_eq!(map_generic(GENERIC_ALL), FILE_ALL_ACCESS);
        assert_eq!(map_generic(0xE001_0000) & (DELETE | FILE_READ_DATA | FILE_WRITE_DATA), DELETE | FILE_READ_DATA | FILE_WRITE_DATA);
        assert_eq!(map_generic(FILE_READ_DATA), FILE_READ_DATA);
    }

    /// 信頼する主体の SID が、実在するアカウントを指す（定数の打ち誤りを見つける）。TrustedInstaller はサービスの名前から
    /// 決まる SID で、名前は言語によらない。SYSTEM・Administrators の名前は言語で変わりうるので、引けることだけを見る。
    #[test]
    fn trusted_sids_resolve_to_their_accounts() {
        assert_eq!(win::account_name(SID_TRUSTED_INSTALLER).as_deref(), Some(r"NT SERVICE\TrustedInstaller"));
        assert!(win::account_name(SID_SYSTEM).is_some());
        assert!(win::account_name(SID_ADMINISTRATORS).is_some());
    }

    #[test]
    fn data_file_names() {
        for name in ["config.toml", "History.toml", "pinned.toml.tmp", "panic.log", "panic.log.tmp", "CONFIG.TOML.TMP"] {
            assert!(is_data_file_name(name), "{name}");
        }
        for name in ["README.md", "LICENSE", "config.toml.bak", "CLCLR.exe"] {
            assert!(!is_data_file_name(name), "{name}");
        }
    }

    // --- 実際のファイル ---

    /// SDDL で権限を付けたフォルダを一時フォルダに作る（テストが作ったフォルダの権限だけを変える）。
    struct TempTree(PathBuf);

    impl TempTree {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("clclr-folder-security-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(dir.join("blobs")).unwrap();
            for name in ["CLCLR.exe", "config.toml", "history.toml", "README.md"] {
                std::fs::write(dir.join(name), b"x").unwrap();
            }
            std::fs::write(dir.join("blobs").join("a_0.bin"), b"x").unwrap();
            Self(dir)
        }

        /// `path` の DACL を SDDL のものにする（継承を切る。中の項目は継承し直す）。
        fn set_dacl(&self, path: &Path, sddl: &str) {
            use std::os::windows::ffi::OsStrExt;
            use windows::core::PCWSTR;
            use windows::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HLOCAL};
            use windows::Win32::Security::Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
            };
            use windows::Win32::Security::{
                GetSecurityDescriptorDacl, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
            };
            let text: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
            let mut sd = PSECURITY_DESCRIPTOR::default();
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(text.as_ptr()), SDDL_REVISION_1, &mut sd, None).unwrap();
                let (mut present, mut defaulted) = (Default::default(), Default::default());
                let mut dacl = std::ptr::null_mut();
                GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted).unwrap();
                let name: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
                let e = SetNamedSecurityInfoW(
                    PCWSTR(name.as_ptr()),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    None,
                    None,
                    Some(dacl),
                    None,
                );
                let _ = LocalFree(Some(HLOCAL(sd.0)));
                assert_eq!(e, ERROR_SUCCESS, "権限を付けられない");
            }
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            // 自分の全権限に戻してから消す
            let me = current_user_sid().unwrap();
            self.set_dacl(&self.0, &format!("D:P(A;OICI;FA;;;{me})"));
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn contents(dir: &Path) -> FolderReport {
        let trusted = Trusted::new(current_user_sid().unwrap());
        let mut report = FolderReport { dir: dir.to_path_buf(), ..FolderReport::default() };
        check_contents(dir, OsStr::new("CLCLR.exe"), &trusted, &mut report);
        report
    }

    /// 自分・SYSTEM・Administrators だけのフォルダは問題なし。Authenticated Users の変更・Users の読み取りを
    /// 付けると、フォルダ・exe・設定・blob のそれぞれで見つける（README は見ない）。
    #[test]
    fn real_folder_is_checked_with_its_contents() {
        let tree = TempTree::new();
        let me = current_user_sid().unwrap();
        tree.set_dacl(&tree.0, &format!("D:P(A;OICI;FA;;;{me})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"));
        let clean = contents(&tree.0);
        assert!(clean.is_clean(), "{clean:?}");

        tree.set_dacl(&tree.0, &format!("D:P(A;OICI;FA;;;{me})(A;;0x1301bf;;;AU)(A;OICIIO;GRGWGXSD;;;AU)(A;OICI;0x1200a9;;;BU)"));
        let found = contents(&tree.0);
        let at = |name: &str| -> Vec<Concern> {
            found.findings.iter().filter(|f| f.path.file_name().unwrap() == name).map(|f| f.concern).collect()
        };
        assert_eq!(at(tree.0.file_name().unwrap().to_str().unwrap()), [Concern::Write, Concern::Inherit]);
        assert_eq!(at("CLCLR.exe"), [Concern::Write]);
        assert_eq!(at("config.toml"), [Concern::Read, Concern::Write]);
        assert_eq!(at("a_0.bin"), [Concern::Read, Concern::Write]);
        assert!(at("README.md").is_empty());
        assert!(found.unverified.is_empty(), "{:?}", found.unverified);

        // 保護された blob だけがほかに読める（フォルダは自分だけ）
        tree.set_dacl(&tree.0, &format!("D:P(A;OICI;FA;;;{me})"));
        tree.set_dacl(&tree.0.join("blobs").join("a_0.bin"), &format!("D:P(A;;FA;;;{me})(A;;FR;;;WD)"));
        let found = contents(&tree.0);
        assert_eq!(found.findings.len(), 1, "{found:?}");
        assert_eq!((found.findings[0].concern, found.findings[0].sids.clone()), (Concern::Read, vec!["S-1-1-0".to_string()]));
    }

    /// `blobs` の中のフォルダもたどる。`panic.log.tmp` も見る。権限を読めない項目・中を列挙できないフォルダは、
    /// 黙って飛ばさずに確かめられないものにする（OWNER RIGHTS で所有者の暗黙の権限を外して、読めない状態を作る）。
    #[test]
    fn nested_and_unreadable_items_are_not_skipped() {
        let tree = TempTree::new();
        let me = current_user_sid().unwrap();
        let owner_only = format!("D:P(A;OICI;FA;;;{me})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)");
        tree.set_dacl(&tree.0, &owner_only);
        let sub = tree.0.join("blobs").join("sub");
        std::fs::create_dir(&sub).unwrap();
        let nested = sub.join("b_0.bin");
        std::fs::write(&nested, b"x").unwrap();
        let panic_tmp = tree.0.join("panic.log.tmp");
        std::fs::write(&panic_tmp, b"x").unwrap();
        let clean = contents(&tree.0);
        assert!(clean.is_clean(), "{clean:?}");

        let readable = format!("D:P(A;;FA;;;{me})(A;;FR;;;WD)");
        tree.set_dacl(&nested, &readable);
        tree.set_dacl(&panic_tmp, &readable);
        let found = contents(&tree.0);
        let mut read: Vec<&Path> = found.findings.iter().filter(|f| f.concern == Concern::Read).map(|f| f.path.as_path()).collect();
        read.sort();
        let mut expected = vec![nested.as_path(), panic_tmp.as_path()];
        expected.sort();
        assert_eq!(read, expected, "{found:?}");

        // 権限を読めないファイル（所有者にも READ_CONTROL が無い）。READ_CONTROL が無いと権限を戻せない（実機で
        // 確かめた）ので、戻さずに消す（親のフォルダの子の削除の権限で消せる）
        tree.set_dacl(&nested, "D:P(A;;WDSD;;;OW)");
        let found = contents(&tree.0);
        std::fs::remove_file(&nested).unwrap();
        assert!(found.unverified.iter().any(|u| matches!(u, Unverified::Unreadable { path, .. } if path == &nested)), "{found:?}");

        // 中を列挙できないフォルダ（権限は読めるが一覧が無い）。戻せなかったときも消せるよう、中を空にしてから
        tree.set_dacl(&sub, "D:P(A;;WDSDRC;;;OW)");
        let found = contents(&tree.0);
        tree.set_dacl(&sub, &owner_only);
        assert!(found.unverified.iter().any(|u| matches!(u, Unverified::Unreadable { path, .. } if path == &sub)), "{found:?}");
        assert!(!found.findings.iter().any(|f| f.path == sub), "{found:?}");
    }

    /// `blobs` の中のフォルダは `BLOBS_MAX_DEPTH` 段までたどり、それより深いフォルダは中を確かめずに「確かめられない」に
    /// する（黙って飛ばさない）。上限の段の中のファイルは確かめる。
    #[test]
    fn blobs_deeper_than_limit_is_unverified() {
        let tree = TempTree::new();
        let me = current_user_sid().unwrap();
        tree.set_dacl(&tree.0, &format!("D:P(A;OICI;FA;;;{me})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"));
        let mut deepest_checked = tree.0.join("blobs");
        for _ in 0..BLOBS_MAX_DEPTH {
            deepest_checked.push("d");
        }
        let too_deep = deepest_checked.join("d");
        std::fs::create_dir_all(&too_deep).unwrap();
        let file = deepest_checked.join("b_0.bin");
        std::fs::write(&file, b"x").unwrap();
        tree.set_dacl(&file, &format!("D:P(A;;FA;;;{me})(A;;FR;;;WD)"));
        let found = contents(&tree.0);
        assert_eq!(found.unverified, [Unverified::TooDeep(too_deep)], "{found:?}");
        assert_eq!(found.findings.iter().map(|f| (f.path.clone(), f.concern)).collect::<Vec<_>>(), [(file, Concern::Read)]);
    }

    /// `blobs` がジャンクション（フォルダの外を指す）なら、中は確かめずに確かめられないものとして知らせる。
    #[test]
    fn junction_is_unverified() {
        let tree = TempTree::new();
        let outside = TempTree::new();
        std::fs::remove_dir_all(tree.0.join("blobs")).unwrap();
        let status = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(tree.0.join("blobs"))
            .arg(outside.0.join("blobs"))
            .output()
            .unwrap();
        assert!(status.status.success(), "ジャンクションを作れない: {status:?}");
        let found = contents(&tree.0);
        assert!(found.unverified.contains(&Unverified::Link(tree.0.join("blobs"))), "{found:?}");
        assert!(!found.findings.iter().any(|f| f.path.starts_with(outside.0.join("blobs"))));
        std::fs::remove_dir(tree.0.join("blobs")).unwrap();
    }

    /// `\\?\C:\…` の形のパスは、接頭辞を外したローカルのパスとして確かめる（ネットワークとみなさない）。`\\?\UNC\…` と
    /// ほかの形はそのまま。
    #[test]
    fn verbatim_disk_path_is_checked_as_local() {
        assert_eq!(without_verbatim_disk_prefix(Path::new(r"\\?\C:\Tools\CLCLR")), PathBuf::from(r"C:\Tools\CLCLR"));
        assert_eq!(without_verbatim_disk_prefix(Path::new(r"\\?\d:\")), PathBuf::from(r"d:\"));
        for same in [r"C:\Tools\CLCLR", r"\\?\UNC\server\share\CLCLR", r"\\server\share\CLCLR"] {
            assert_eq!(without_verbatim_disk_prefix(Path::new(same)), PathBuf::from(same), "{same}");
        }
        let tree = TempTree::new();
        let verbatim = PathBuf::from(format!(r"\\?\{}", tree.0.display()));
        let (plain, extended) = (check(&tree.0, OsStr::new("CLCLR.exe")), check(&verbatim, OsStr::new("CLCLR.exe")));
        assert!(!extended.unverified.contains(&Unverified::Remote), "{extended:?}");
        // 結果の保存先も外した形（見つかった項目のパスと同じ形。警告の本文の書き分けが同じになる）
        assert_eq!(extended, plain);
    }

    /// 実際の保存先（このテストの exe のフォルダ）でも、パニックせずに結果を返す（内容は環境による）。
    #[test]
    fn check_runs_on_real_exe_folder() {
        let exe = std::env::current_exe().unwrap();
        let report = check(exe.parent().unwrap(), exe.file_name().unwrap());
        assert_eq!(report.dir, exe.parent().unwrap());
        for sid in report.findings.iter().flat_map(|f| &f.sids) {
            assert!(!report.name(sid).is_empty());
        }
    }
}
