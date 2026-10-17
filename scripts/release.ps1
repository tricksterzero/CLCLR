<#
.SYNOPSIS
CLCLR の配布用の zip と、リリースのページの文面を作る（GitHub には出さない）。

.DESCRIPTION
次の順に行い、どこかで失敗したら止まる。

1. 作業ツリーに未コミットの変更が無く、main の最新（origin/main と同じ）であることを確かめる
2. Cargo.toml の版を読み、CHANGELOG.md にその版の節（「## v版（公開日）」）があることと、タグ v<版> が
   origin にまだ無いことを確かめる
3. cargo test（debug）と cargo build --release。どちらも C ランタイムを静的にリンクし（-C target-feature=+crt-static）、
   Control Flow Guard を付ける（-C control-flow-guard）。exe はこのビルドで cargo が知らせたものを使い、版と x64 で
   あることと、C ランタイムの DLL を import していないことと、CFG と CET シャドウスタック互換（build.rs が付ける）の
   印があることを確かめる。release のビルドでは
   Cargo のホームのパスを「cargo」に置き換え（--remap-path-prefix）、exe にユーザーのフォルダと Cargo のホームの
   パスが残っていないことを確かめる（探すのはこの2つのパスだけ）。どちらも --locked を付け、Cargo.lock を書き換える
   必要があれば（Cargo.toml と食い違うときなど）止まる
4. <出力先>\dist\CLCLR-<版>-x64.zip を作る（CLCLR.exe・README.md・LICENSE・THIRD-PARTY-NOTICES.md の4つ）。
   作った zip の項目がちょうどその4つで、中身が元のファイルと同じであることと、テスト・ビルドの間に作業ツリーと
   コミットが変わっていないことを確かめる
5. zip と exe（zip の中のもの）の SHA-256 を出し、リリースのページの文面 <出力先>\dist\release-notes-<版>.md を作る
6. ビルドに使う道具の目録（Rust のツールチェーン、Visual Studio と MSVC、Windows SDK、静的にリンクする C ランタイムの
   ライブラリ、道具を探すときに読むレジストリの値、道具の選び方に関わる環境変数と Cargo の設定ファイル）を、テストの
   前と zip を作った後に取って比べ、
   違っていれば止める（下書きでも止める）。目録と exe の SHA-256・Rich ヘッダーを <出力先>\dist\build-tools-<版>.json
   に書く

GitHub へのリリース（タグの作成と gh release create）は外から見える操作なので、このスクリプトでは行わず、
最後に実行するコマンドを表示するだけにする（リポジトリは origin、タグの位置は確かめたコミットの SHA で指定する）。

.PARAMETER Draft
下書き。main 以外のブランチ・未コミットの変更・origin/main との違い・CHANGELOG.md の公開日の「未定」・
既にあるタグ・GitHub 以外の origin を、止めずに警告だけにする（試しに作るとき用）。下書きの zip は配らない。

.PARAMETER BuildToolsBaseline
前に書き出した build-tools-<版>.json。テストの前に取った目録をこのファイルの目録と比べ、違っていれば止める
（下書きでは警告だけにする）。下書きでないときは必須（省くと、採用した道具との比較をせずに進んでしまうため止める）。

.EXAMPLE
powershell -ExecutionPolicy Bypass -File scripts\release.ps1 -BuildToolsBaseline <採用した目録の build-tools-<版>.json>

.EXAMPLE
powershell -ExecutionPolicy Bypass -File scripts\release.ps1 -Draft
#>
[CmdletBinding()]
param(
    [switch]$Draft,
    [string]$BuildToolsBaseline
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

$root = Split-Path -Parent $PSScriptRoot
# 相対パスは、リポジトリのフォルダへ移る前の、呼び出したときのフォルダを基点にする
if ($BuildToolsBaseline) {
    $BuildToolsBaseline = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($BuildToolsBaseline)
} elseif (-not $Draft) {
    throw '-BuildToolsBaseline に、採用した道具の目録のファイル（build-tools-<版>.json）を渡してください（下書きでないときは必須です）'
}
$packageFiles = @('README.md', 'LICENSE', 'THIRD-PARTY-NOTICES.md')

function Write-Step([string]$message) {
    Write-Host "== $message" -ForegroundColor Cyan
}

# 下書きなら警告だけ、そうでなければ止める
function Stop-OrWarn([string]$message) {
    if ($Draft) {
        Write-Host "警告（下書きなので続けます）: $message" -ForegroundColor Yellow
    } else {
        throw $message
    }
}

# ネイティブのコマンドを実行し、終了コードが 0 でなければ止める（出力はそのまま画面に出す）
function Invoke-Checked([string]$label, [scriptblock]$command) {
    & $command
    if ($LASTEXITCODE -ne 0) {
        throw "$label が失敗しました（終了コード $LASTEXITCODE）"
    }
}

# git を実行して出力の行を返す。終了コードが 0 でなければ止める
function Invoke-Git([string[]]$arguments) {
    $output = @(& git @arguments)
    if ($LASTEXITCODE -ne 0) {
        throw "git $($arguments -join ' ') が失敗しました（終了コード $LASTEXITCODE）"
    }
    return ,$output
}

# git を実行して1行の出力を返す（空なら止める）
function Get-GitLine([string[]]$arguments) {
    $output = Invoke-Git $arguments
    if ($output.Count -eq 0 -or -not "$($output[0])".Trim()) {
        throw "git $($arguments -join ' ') の出力が空です"
    }
    return "$($output[0])".Trim()
}

# 表示するコマンド用に、PowerShell の単一引用符で囲む（$ などを展開させない）
function ConvertTo-PsLiteral([string]$text) {
    return "'" + ($text -replace "'", "''") + "'"
}

# origin の URL から gh の --repo に渡す github.com/owner/repo を取り出す（GitHub でなければ $null）。
# ホストまで書くのは、GH_HOST などで gh の既定のホストが別になっていても github.com を指すため
function Get-GitHubRepo([string]$url) {
    if ($url -match '^(?:https://github\.com/|git@github\.com:|ssh://git@github\.com/)([^/]+)/([^/]+?)(?:\.git)?/?$') {
        return "github.com/$($Matches[1])/$($Matches[2])"
    }
    return $null
}

# exe が x64 の PE か（PE ヘッダーの Machine が IMAGE_FILE_MACHINE_AMD64 = 0x8664）
function Test-X64Exe([string]$path) {
    $bytes = [System.IO.File]::ReadAllBytes($path)
    if ($bytes.Length -lt 0x40) {
        return $false
    }
    $peOffset = [System.BitConverter]::ToInt32($bytes, 0x3C)
    if ($peOffset -lt 0 -or $peOffset + 6 -gt $bytes.Length) {
        return $false
    }
    $signature = [System.BitConverter]::ToUInt32($bytes, $peOffset)
    $machine = [System.BitConverter]::ToUInt16($bytes, $peOffset + 4)
    return ($signature -eq 0x00004550 -and $machine -eq 0x8664)
}

# exe（x64 の PE。PE32+）に Control Flow Guard の印があるか（オプションヘッダーの DllCharacteristics の
# IMAGE_DLLCHARACTERISTICS_GUARD_CF = 0x4000。PE32+ ではオプションヘッダーの先頭から 70 バイト目）
function Test-GuardCf([string]$path) {
    $bytes = [System.IO.File]::ReadAllBytes($path)
    $optional = [System.BitConverter]::ToInt32($bytes, 0x3C) + 24
    if ([System.BitConverter]::ToUInt16($bytes, $optional) -ne 0x20B) {
        throw "$path が PE32+ の実行ファイルではありません"
    }
    return (([System.BitConverter]::ToUInt16($bytes, $optional + 70) -band 0x4000) -ne 0)
}

# exe（x64 の PE。PE32+）に CET シャドウスタック互換の印があるか（デバッグのディレクトリの、種類 20 =
# IMAGE_DEBUG_TYPE_EX_DLLCHARACTERISTICS の項目が指す値の、ビット 0 = IMAGE_DLLCHARACTERISTICS_EX_CET_COMPAT）
function Test-CetCompat([string]$path) {
    $bytes = [System.IO.File]::ReadAllBytes($path)
    $pe = [System.BitConverter]::ToInt32($bytes, 0x3C)
    $sectionCount = [System.BitConverter]::ToUInt16($bytes, $pe + 6)
    $optionalSize = [System.BitConverter]::ToUInt16($bytes, $pe + 20)
    $optional = $pe + 24
    if ([System.BitConverter]::ToUInt16($bytes, $optional) -ne 0x20B) {
        throw "$path が PE32+ の実行ファイルではありません"
    }
    # データディレクトリの7つ目（デバッグ）。項目は 28 バイトずつ
    $debugRva = [uint64][System.BitConverter]::ToUInt32($bytes, $optional + 112 + 8 * 6)
    $debugSize = [System.BitConverter]::ToUInt32($bytes, $optional + 112 + 8 * 6 + 4)
    if ($debugRva -eq 0) {
        return $false
    }
    $directory = $null
    for ($i = 0; $i -lt $sectionCount; $i++) {
        $s = $optional + $optionalSize + $i * 40
        $va = [uint64][System.BitConverter]::ToUInt32($bytes, $s + 12)
        $size = [uint64][Math]::Max([System.BitConverter]::ToUInt32($bytes, $s + 8), [System.BitConverter]::ToUInt32($bytes, $s + 16))
        if ($debugRva -ge $va -and $debugRva -lt $va + $size) {
            $directory = [int]([System.BitConverter]::ToUInt32($bytes, $s + 20) + $debugRva - $va)
        }
    }
    if ($null -eq $directory) {
        throw "デバッグのディレクトリがどのセクションにもありません（$path）"
    }
    for ($entry = $directory; $entry -lt $directory + $debugSize; $entry += 28) {
        if ([System.BitConverter]::ToUInt32($bytes, $entry + 12) -eq 20) {
            # PointerToRawData（ファイルの中の位置）
            $data = [int][System.BitConverter]::ToUInt32($bytes, $entry + 24)
            return (([System.BitConverter]::ToUInt32($bytes, $data) -band 1) -ne 0)
        }
    }
    return $false
}

# exe（x64 の PE。PE32+）が静的に import している DLL の名前（import のディレクトリから読む）
function Get-ImportedDll([string]$path) {
    $bytes = [System.IO.File]::ReadAllBytes($path)
    $pe = [System.BitConverter]::ToInt32($bytes, 0x3C)
    $sectionCount = [System.BitConverter]::ToUInt16($bytes, $pe + 6)
    $optionalSize = [System.BitConverter]::ToUInt16($bytes, $pe + 20)
    $optional = $pe + 24
    if ([System.BitConverter]::ToUInt16($bytes, $optional) -ne 0x20B) {
        throw "$path が PE32+ の実行ファイルではありません"
    }
    # データディレクトリの2つ目（import）。PE32+ ではオプションヘッダーの先頭から 112 バイト目から並ぶ
    $importRva = [System.BitConverter]::ToUInt32($bytes, $optional + 112 + 8)
    $sections = @(for ($i = 0; $i -lt $sectionCount; $i++) {
        $s = $optional + $optionalSize + $i * 40
        [pscustomobject]@{
            Va   = [uint64][System.BitConverter]::ToUInt32($bytes, $s + 12)
            Size = [uint64][Math]::Max([System.BitConverter]::ToUInt32($bytes, $s + 8), [System.BitConverter]::ToUInt32($bytes, $s + 16))
            Raw  = [uint64][System.BitConverter]::ToUInt32($bytes, $s + 20)
        }
    })
    $toOffset = {
        param([uint64]$rva)
        foreach ($section in $sections) {
            if ($rva -ge $section.Va -and $rva -lt $section.Va + $section.Size) {
                return [int]($section.Raw + $rva - $section.Va)
            }
        }
        throw ('RVA 0x{0:X} がどのセクションにもありません（{1}）' -f $rva, $path)
    }
    $names = @()
    $descriptor = & $toOffset $importRva
    while ($true) {
        $nameRva = [System.BitConverter]::ToUInt32($bytes, $descriptor + 12)
        if ($nameRva -eq 0) {
            break
        }
        $start = & $toOffset $nameRva
        $end = [Array]::IndexOf($bytes, [byte]0, $start)
        $names += [System.Text.Encoding]::ASCII.GetString($bytes, $start, $end - $start)
        $descriptor += 20
    }
    return $names
}

# ファイルの中に、指定したパスが UTF-8 か UTF-16 で含まれていれば、見つかったパスを返す。区切りの \ と / は
# 区別しない（混ざっていても見つける）。大文字・小文字は ASCII の文字だけ区別しない（ASCII 以外の文字はバイト列の
# 一致で探す）。探すのは指定したパスだけで、ビルドした環境のパスが全く無いことまでは確かめない
function Find-LocalPath([string]$path, [string[]]$needles) {
    # 1バイトを1文字に写す（Latin-1）ので、バイト列のまま文字列として探せる。/（0x2F）を \（0x5C）にそろえる
    # （UTF-16 の / も下位のバイトが 0x2F なので、同じ置き換えでそろう）
    $latin1 = [System.Text.Encoding]::GetEncoding(28591)
    $text = $latin1.GetString([System.IO.File]::ReadAllBytes($path)).Replace('/', '\')
    $found = @()
    foreach ($needle in $needles | Where-Object { $_ } | Select-Object -Unique) {
        $normalized = $needle.Replace('/', '\')
        $forms = @(
            $latin1.GetString([System.Text.Encoding]::UTF8.GetBytes($normalized)),
            $latin1.GetString([System.Text.Encoding]::Unicode.GetBytes($normalized))
        )
        if (@($forms | Where-Object { $text.IndexOf($_, [System.StringComparison]::OrdinalIgnoreCase) -ge 0 }).Count -gt 0) {
            $found += $needle
        }
    }
    return $found
}

# フォルダの短い名前（8.3 形式）のパス。取れなければ $null
function Get-ShortFolderPath([string]$path) {
    try {
        $short = (New-Object -ComObject Scripting.FileSystemObject).GetFolder($path).ShortPath
        if ($short -and $short -ne $path) {
            return $short
        }
    } catch {
    }
    return $null
}

# zip の項目の SHA-256（大文字の16進）
function Get-ZipEntryHash([System.IO.Compression.ZipArchiveEntry]$entry) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    $stream = $entry.Open()
    try {
        return ([System.BitConverter]::ToString($sha.ComputeHash($stream)) -replace '-', '')
    } finally {
        $stream.Dispose()
        $sha.Dispose()
    }
}

# Cargo.toml の [package] の節の version
function Get-PackageVersion([string]$cargoToml) {
    $inPackage = $false
    foreach ($line in Get-Content -LiteralPath $cargoToml -Encoding UTF8) {
        # [table] と [[array-of-tables]]（[[bin]] など）の見出し
        if ($line -match '^\s*\[\[?\s*([^\[\]]+?)\s*\]\]?\s*(#.*)?$') {
            $inPackage = ($Matches[1] -eq 'package')
            continue
        }
        if ($inPackage -and $line -match '^\s*version\s*=\s*"([^"]+)"') {
            return $Matches[1]
        }
    }
    throw 'Cargo.toml の [package] に version がありません'
}

# CHANGELOG.md の「## v版（公開日）」の節（公開日と本文）
function Get-ChangelogSection([string]$changelog, [string]$version) {
    $lines = @(Get-Content -LiteralPath $changelog -Encoding UTF8)
    $pattern = '^## v' + [regex]::Escape($version) + '（(.+)）\s*$'
    $start = -1
    $date = $null
    for ($i = 0; $i -lt $lines.Count; $i++) {
        if ($lines[$i] -match $pattern) {
            $start = $i
            $date = $Matches[1]
            break
        }
    }
    if ($start -lt 0) {
        throw "CHANGELOG.md に「## v$version（公開日）」の節がありません"
    }
    $end = $lines.Count
    for ($j = $start + 1; $j -lt $lines.Count; $j++) {
        if ($lines[$j] -match '^## ') {
            $end = $j
            break
        }
    }
    $body = ''
    if ($end - 1 -ge $start + 1) {
        $body = ($lines[($start + 1)..($end - 1)] -join "`n").Trim()
    }
    if (-not $body) {
        throw "CHANGELOG.md の「## v$version」の節が空です"
    }
    return [pscustomobject]@{ Date = $date; Body = $body }
}

# JSON から読んだオブジェクトのプロパティ（無ければ $null。Set-StrictMode の下でも止めない）
function Get-JsonProperty($object, [string]$name) {
    if ($null -ne $object -and $object.PSObject.Properties[$name]) {
        return $object.PSObject.Properties[$name].Value
    }
    return $null
}

# ファイルの版（ファイルが無ければ $null）
function Get-FileVersionOrNull([string]$path) {
    if (Test-Path -LiteralPath $path -PathType Leaf) {
        # -Force: 隠しファイルも読む（-Force が無いと、Test-Path が真でも Get-Item が見つからないと言って止まる）
        return (Get-Item -LiteralPath $path -Force).VersionInfo.FileVersion
    }
    return $null
}

# ファイルのパスと SHA-256（ファイルが無ければ $null）
function Get-FileRecord([string]$path) {
    if (Test-Path -LiteralPath $path -PathType Leaf) {
        return [ordered]@{ path = $path; sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash }
    }
    return $null
}

# 実行ファイルの版と SHA-256（ファイルが無ければ $null）
function Get-ExeRecord([string]$path) {
    if (Test-Path -LiteralPath $path -PathType Leaf) {
        return [ordered]@{ version = Get-FileVersionOrNull $path; sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash }
    }
    return $null
}

# HKLM のレジストリの文字列の値。表示（64 ビット・32 ビット）を指定して読む。キーか値が無ければ $null。読めない
# とき（アクセス拒否など）は、有無を確かめられていないので止める
function Get-RegistryValue([Microsoft.Win32.RegistryView]$view, [string]$subKey, [string]$name) {
    $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine, $view)
    try {
        $key = $base.OpenSubKey($subKey)
        if ($null -eq $key) {
            return $null
        }
        try {
            return $key.GetValue($name)
        } finally {
            $key.Dispose()
        }
    } finally {
        $base.Dispose()
    }
}

# 既定のツールセットを決めるファイル（VC\Auxiliary\Build\Microsoft.VCToolsVersion.default.txt）の中身。末尾の空白
# を除く。ファイルが無ければ $null。リンカーを探す側は、ファイル全体を UTF-8 として読み（BOM は消さない）、末尾の
# 空白を除いた文字列をツールセットの版にする。同じ名前のフォルダがある、読めない、UTF-8 として正しくないときは止める
function Get-DefaultToolsetText([string]$installationPath) {
    $path = Join-Path $installationPath 'VC\Auxiliary\Build\Microsoft.VCToolsVersion.default.txt'
    if (Test-Path -LiteralPath $path -PathType Container) {
        throw "既定のツールセットのファイルと同じ名前のフォルダがあります（$path）"
    }
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        return $null
    }
    $bytes = [System.IO.File]::ReadAllBytes($path)
    # 2つ目の引数: 正しくないバイトを置き換えずに例外にする
    $utf8 = New-Object System.Text.UTF8Encoding($false, $true)
    return $utf8.GetString($bytes).TrimEnd()
}

# フォルダの中のフォルダの名前（フォルダが無ければ空）。隠しフォルダも含める（道具を探す側は隠しフォルダも見る）
function Get-SubfolderName([string]$path) {
    if (Test-Path -LiteralPath $path -PathType Container) {
        return @(Get-ChildItem -LiteralPath $path -Directory -Force | ForEach-Object { $_.Name } | Sort-Object)
    }
    return @()
}

# ビルドに使う道具の目録。Rust のツールチェーン、Visual Studio と MSVC のツールセット（リンカーの版と SHA-256、
# 静的にリンクする C ランタイムのライブラリ、既定のツールセット）、Windows SDK（版、rc.exe、静的にリンクする UCRT
# のライブラリ）、リンカーが SDK を探すときに読むレジストリの値、道具の選び方に関わる環境変数と Cargo の設定ファイル。
# どれも exe の中身を左右する。見るのは入っているものと選び方の指定で、ビルドで実際にどれが使われたかまでは確かめない
function Get-BuildToolInventory {
    $rustc = @(& rustc --version --verbose)
    if ($LASTEXITCODE -ne 0 -or $rustc.Count -eq 0) {
        throw "rustc --version --verbose が失敗しました（終了コード $LASTEXITCODE）"
    }
    $cargo = @(& cargo --version)
    if ($LASTEXITCODE -ne 0 -or $cargo.Count -eq 0) {
        throw "cargo --version が失敗しました（終了コード $LASTEXITCODE）"
    }

    # Visual Studio のインスタンス（vswhere は Visual Studio のインストーラーに付く。無ければ空）。プレビュー版も
    # 含める（リンカーを探す側は、プレビュー版も候補にする）
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    $instances = @()
    if (Test-Path -LiteralPath $vswhere -PathType Leaf) {
        $json = @(& $vswhere -all -prerelease -products '*' -format json)
        if ($LASTEXITCODE -ne 0) {
            throw "vswhere が失敗しました（終了コード $LASTEXITCODE）"
        }
        # JSON の配列を1つずつに分ける（ConvertFrom-Json は配列を1つのオブジェクトとして出す）
        $instances = @(($json -join "`n") | ConvertFrom-Json | ForEach-Object { $_ })
    }
    # 既定のツールセットのファイルは、VCToolsVersion が無いときだけ記録する（あればツールセットはそれで決まり、
    # ファイルは読まれない）
    $useDefaultToolset = -not [Environment]::GetEnvironmentVariable('VCToolsVersion')
    $visualStudio = @(foreach ($instance in $instances | Sort-Object { Get-JsonProperty $_ 'installationPath' }) {
        $installationPath = Get-JsonProperty $instance 'installationPath'
        $msvcRoot = Join-Path $installationPath 'VC\Tools\MSVC'
        $toolsets = @(foreach ($toolset in Get-SubfolderName $msvcRoot) {
            $toolsetDir = Join-Path $msvcRoot $toolset
            [ordered]@{
                version      = $toolset
                link         = Get-ExeRecord (Join-Path $toolsetDir 'bin\HostX64\x64\link.exe')
                cvtres       = Get-ExeRecord (Join-Path $toolsetDir 'bin\HostX64\x64\cvtres.exe')
                libcmt       = Get-FileRecord (Join-Path $toolsetDir 'lib\x64\libcmt.lib')
                libvcruntime = Get-FileRecord (Join-Path $toolsetDir 'lib\x64\libvcruntime.lib')
                # リンカーは SDK より先にこれらのフォルダを探すので、ここに libucrt.lib があると SDK のものより先に使う
                libucrtInLib    = Test-Path -LiteralPath (Join-Path $toolsetDir 'lib\x64\libucrt.lib') -PathType Leaf
                libucrtInAtlmfc = Test-Path -LiteralPath (Join-Path $toolsetDir 'atlmfc\lib\x64\libucrt.lib') -PathType Leaf
            }
        })
        $defaultToolset = $null
        if ($useDefaultToolset) {
            $defaultToolset = Get-DefaultToolsetText $installationPath
        }
        [ordered]@{
            displayName           = Get-JsonProperty $instance 'displayName'
            productDisplayVersion = Get-JsonProperty (Get-JsonProperty $instance 'catalog') 'productDisplayVersion'
            installationVersion   = Get-JsonProperty $instance 'installationVersion'
            installationPath      = $installationPath
            defaultToolset        = $defaultToolset
            toolsets              = $toolsets
        }
    })

    # Windows SDK（Windows 10 以降の SDK の場所は、レジストリの KitsRoot10 にある）。embed-resource（rc.exe を探す）は
    # 64 ビットの表示を読む。キーか値が無いときだけ「SDK なし」にする（読めないときは止める）
    $installedRootsKey = 'SOFTWARE\Microsoft\Windows Kits\Installed Roots'
    $kitsRoot = Get-RegistryValue ([Microsoft.Win32.RegistryView]::Registry64) $installedRootsKey 'KitsRoot10'
    # リンカーを探す側（rustc の中のもの）は 32 ビットの表示を読み、UCRT の場所を KitsRoot10 から、import ライブラリ
    # の場所を Microsoft SDKs\Windows\v10.0 の InstallationFolder から決める
    $registry = [ordered]@{
        kitsRoot10_32              = Get-RegistryValue ([Microsoft.Win32.RegistryView]::Registry32) $installedRootsKey 'KitsRoot10'
        sdk10InstallationFolder_32 = Get-RegistryValue ([Microsoft.Win32.RegistryView]::Registry32) 'SOFTWARE\Microsoft\Microsoft SDKs\Windows\v10.0' 'InstallationFolder'
    }
    $windowsSdk = $null
    if ($kitsRoot) {
        $binDir = Join-Path $kitsRoot 'bin'
        # bin の下のフォルダ（版、または x64 などの直下の置き場）の x64\rc.exe をすべて記録する（並びは名前の順）。
        # embed-resource は、環境変数 RC などで指定されていなければ、bin の下をファイルシステムが列挙した順に見て、
        # 最初に x64\rc.exe があるものを使う（この並びと同じとは限らない）
        $rcFiles = @(foreach ($folder in Get-SubfolderName $binDir) {
            $rc = Join-Path $binDir "$folder\x64\rc.exe"
            if (Test-Path -LiteralPath $rc -PathType Leaf) {
                [ordered]@{ path = $rc; version = Get-FileVersionOrNull $rc; sha256 = (Get-FileHash -LiteralPath $rc -Algorithm SHA256).Hash }
            }
        })
        $libDir = Join-Path $kitsRoot 'Lib'
        $windowsSdk = [ordered]@{
            root    = $kitsRoot
            lib     = @(Get-SubfolderName $libDir)
            # リンカーを探す側は、ucrt のフォルダがある版から UCRT を、um\x64\kernel32.lib がある版から import
            # ライブラリを選ぶ
            libVersions = @(foreach ($version in Get-SubfolderName $libDir) {
                [ordered]@{
                    version  = $version
                    ucrt     = Test-Path -LiteralPath (Join-Path $libDir "$version\ucrt") -PathType Container
                    kernel32 = Test-Path -LiteralPath (Join-Path $libDir "$version\um\x64\kernel32.lib") -PathType Leaf
                }
            })
            include = @(Get-SubfolderName (Join-Path $kitsRoot 'Include'))
            bin     = @(Get-SubfolderName $binDir)
            rc      = $rcFiles
            libucrt = @(foreach ($version in Get-SubfolderName $libDir) {
                $record = Get-FileRecord (Join-Path $libDir "$version\ucrt\x64\libucrt.lib")
                if ($record) {
                    $record
                }
            })
        }
    }

    # 道具の選び方に関わる環境変数（PATH は、ほかのソフトを入れるだけで変わるので含めない）。リンカーと rc.exe を
    # 探す側、cargo（使う rustc）、リンカー自身（LINK・_LINK_ の引数）が読むもの
    $environment = [ordered]@{}
    foreach ($variable in 'VCINSTALLDIR', 'VCToolsInstallDir', 'WindowsSdkDir', 'LIB', 'INCLUDE', 'RC', 'RC_x86_64-pc-windows-msvc', 'RC_x86_64_pc_windows_msvc', 'CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER', 'CARGO_BUILD_TARGET', 'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_BUILD_RUSTFLAGS', 'RUSTUP_TOOLCHAIN', 'VSTEL_MSBuildProjectFullPath', 'VCToolsVersion', 'WindowsSDKVersion', 'RUSTC', 'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER', 'CARGO_BUILD_RUSTC', 'CARGO_BUILD_RUSTC_WRAPPER', 'CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER', 'LINK', '_LINK_') {
        $environment[$variable] = [Environment]::GetEnvironmentVariable($variable)
    }
    # Spectre 対策版の C ランタイム（lib\spectre）を使うかどうか。値が spectre のときだけ効く
    $environment['VSCMD_ARG_VCVARS_SPECTRE=spectre'] = ([Environment]::GetEnvironmentVariable('VSCMD_ARG_VCVARS_SPECTRE') -ceq 'spectre')

    # Cargo の設定ファイル（リポジトリのフォルダから上の各フォルダの .cargo と、Cargo のホーム）。中身は SHA-256 で
    # 比べる（リンカーや rustflags の指定が変われば変わる）
    $configFiles = @()
    $folder = $root
    while ($folder) {
        foreach ($file in 'config.toml', 'config') {
            $record = Get-FileRecord (Join-Path $folder ".cargo\$file")
            if ($record) {
                $configFiles += $record
            }
        }
        $folder = Split-Path -Parent $folder
    }
    $cargoHomeDir = if ($env:CARGO_HOME) { $env:CARGO_HOME } elseif ($env:USERPROFILE) { Join-Path $env:USERPROFILE '.cargo' } else { $null }
    if ($cargoHomeDir) {
        foreach ($file in 'config.toml', 'config') {
            $record = Get-FileRecord (Join-Path $cargoHomeDir $file)
            if ($record) {
                $configFiles += $record
            }
        }
    }

    return [ordered]@{
        rust         = [ordered]@{ rustc = @($rustc | ForEach-Object { "$_".Trim() }); cargo = "$($cargo[0])".Trim() }
        visualStudio = $visualStudio
        windowsSdk   = $windowsSdk
        registry     = $registry
        environment  = $environment
        cargoConfig  = $configFiles
    }
}

# 目録を比べるための文字列（JSON に直し、読み直してからもう一度直して、ファイルから読んだものと形をそろえる）
function ConvertTo-InventoryText($inventory) {
    return ($inventory | ConvertTo-Json -Depth 10 -Compress | ConvertFrom-Json | ConvertTo-Json -Depth 10 -Compress)
}

# exe（PE）の Rich ヘッダーの項目（製品 ID・ビルド番号・数。リンクしたオブジェクトとライブラリを作った道具の番号）。
# 無ければ空
function Get-RichHeaderEntry([string]$path) {
    $bytes = [System.IO.File]::ReadAllBytes($path)
    $pe = [System.BitConverter]::ToInt32($bytes, 0x3C)
    # "Rich" の印と、その後ろの鍵（項目はこの鍵との XOR で書かれている）
    $rich = -1
    for ($i = 0x80; $i + 8 -le $pe; $i += 4) {
        if ([System.BitConverter]::ToUInt32($bytes, $i) -eq 0x68636952) {
            $rich = $i
            break
        }
    }
    if ($rich -lt 0) {
        return @()
    }
    $key = [System.BitConverter]::ToUInt32($bytes, $rich + 4)
    # 先頭の "DanS" の印（鍵との XOR で書かれている）
    $start = -1
    for ($i = $rich - 4; $i -ge 0x80; $i -= 4) {
        if (([System.BitConverter]::ToUInt32($bytes, $i) -bxor $key) -eq 0x536E6144) {
            $start = $i
            break
        }
    }
    if ($start -lt 0) {
        throw "Rich ヘッダーの先頭が見つかりません（$path）"
    }
    $entries = @(for ($offset = $start + 16; $offset -lt $rich; $offset += 8) {
        $compId = [System.BitConverter]::ToUInt32($bytes, $offset) -bxor $key
        $count = [System.BitConverter]::ToUInt32($bytes, $offset + 4) -bxor $key
        '{0:x4} {1} {2}' -f ($compId -shr 16), ($compId -band 0xFFFF), $count
    })
    return @($entries | Sort-Object)
}

Push-Location -LiteralPath $root
try {
    Write-Step '作業ツリーと main の状態を確かめる'
    $branch = Get-GitLine @('rev-parse', '--abbrev-ref', 'HEAD')
    if ($branch -ne 'main') {
        Stop-OrWarn "今のブランチが main ではありません（$branch）"
    }
    $dirty = Invoke-Git @('status', '--porcelain')
    if ($dirty.Count -gt 0) {
        Stop-OrWarn "未コミットの変更があります（$($dirty.Count) 件）"
    }
    Invoke-Git @('fetch', '--quiet', 'origin') | Out-Null
    $head = Get-GitLine @('rev-parse', 'HEAD')
    $remoteHead = Get-GitLine @('rev-parse', 'origin/main')
    if ($head -ne $remoteHead) {
        Stop-OrWarn 'HEAD が origin/main と違います（push していないコミットか、取り込んでいない変更があります）'
    }
    $originUrl = Get-GitLine @('remote', 'get-url', 'origin')
    $repo = Get-GitHubRepo $originUrl
    if (-not $repo) {
        Stop-OrWarn "origin が GitHub のリポジトリではありません（$originUrl）"
    }

    Write-Step '版と変更履歴を確かめる'
    $version = Get-PackageVersion (Join-Path $root 'Cargo.toml')
    $section = Get-ChangelogSection (Join-Path $root 'CHANGELOG.md') $version
    if ($section.Date -eq '未定') {
        Stop-OrWarn "CHANGELOG.md の $version の公開日が「未定」のままです"
    } else {
        $parsed = [datetime]::MinValue
        if (-not [datetime]::TryParseExact($section.Date, 'yyyy-MM-dd', [System.Globalization.CultureInfo]::InvariantCulture, [System.Globalization.DateTimeStyles]::None, [ref]$parsed)) {
            throw "CHANGELOG.md の $version の公開日が YYYY-MM-DD の実在する日付ではありません（$($section.Date)）"
        }
    }
    $tag = "v$version"
    $remoteTags = Invoke-Git @('ls-remote', '--tags', 'origin', "refs/tags/$tag")
    if ($remoteTags.Count -gt 0) {
        Stop-OrWarn "タグ $tag が origin に既にあります"
    }
    Write-Host "版: $version  公開日: $($section.Date)"

    # テストと release のビルドでは、コンパイラへの指定を CARGO_ENCODED_RUSTFLAGS で渡す。外から rustflags の環境変数
    # が設定されていると、どちらかが黙って無視される（Cargo は rustflags の出どころを足し合わせず、最初の1つだけを使う）
    # ので止める
    foreach ($variable in 'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_BUILD_RUSTFLAGS') {
        if ([Environment]::GetEnvironmentVariable($variable)) {
            throw "環境変数 $variable が設定されています。消してから実行してください"
        }
    }
    # 配布のビルドの指定（テストも同じ指定で通す。CARGO_ENCODED_RUSTFLAGS は、指定を 0x1F の文字で区切って並べる）
    # - C ランタイム（CRT）を exe に静的にリンクする。Visual C++ の実行環境（VCRUNTIME140.dll）が入っていない PC でも
    #   動き、exe の隣に置かれた同じ名前の DLL も関係しなくなる
    # - Control Flow Guard（CFG）を付ける。間接呼び出しの呼び先を Windows が確かめ、正しい呼び先でなければプロセスを
    #   終える（メモリが壊されたときに、呼び先を書き換えて処理を乗っ取られないようにする）
    $hardening = '-Ctarget-feature=+crt-static' + [char]0x1F + '-Ccontrol-flow-guard'
    # テストとビルドには --locked を付け、コミットした Cargo.lock の依存の版のままで行う（Cargo.lock を書き換える
    # 必要があれば、コンパイルを始める前に止まる）

    Write-Step 'ビルドに使う道具の目録を取る'
    $toolsBefore = Get-BuildToolInventory
    $toolsBeforeText = ConvertTo-InventoryText $toolsBefore
    if ($BuildToolsBaseline) {
        if (-not (Test-Path -LiteralPath $BuildToolsBaseline -PathType Leaf)) {
            throw "比べる目録のファイルがありません（$BuildToolsBaseline）"
        }
        $baseline = Get-JsonProperty ([System.IO.File]::ReadAllText($BuildToolsBaseline) | ConvertFrom-Json) 'inventory'
        if ($null -eq $baseline) {
            throw "比べる目録のファイルに inventory がありません（$BuildToolsBaseline）"
        }
        if ((ConvertTo-InventoryText $baseline) -cne $toolsBeforeText) {
            Stop-OrWarn "ビルドに使う道具の目録が $BuildToolsBaseline と違います"
        } else {
            Write-Host "目録は $BuildToolsBaseline と同じです"
        }
    }
    if ($toolsBefore.windowsSdk) {
        foreach ($sdk in $toolsBefore.windowsSdk['lib']) {
            Write-Host "Windows SDK: $sdk"
        }
    } else {
        Write-Host 'Windows SDK: 見つかりません（レジストリの KitsRoot10 がありません）'
    }
    foreach ($instance in $toolsBefore.visualStudio) {
        Write-Host "Visual Studio: $($instance.displayName) $($instance.productDisplayVersion)"
        foreach ($toolset in $instance.toolsets) {
            $linkVersion = if ($toolset.link) { $toolset.link['version'] } else { 'なし' }
            Write-Host "  MSVC $($toolset.version)（link.exe $linkVersion）"
        }
    }

    Write-Step 'テスト（cargo test。配布のビルドと同じく CRT の静的リンクと CFG の指定で）'
    $env:CARGO_ENCODED_RUSTFLAGS = $hardening
    try {
        Invoke-Checked 'cargo test' { cargo test --locked }
    } finally {
        Remove-Item Env:CARGO_ENCODED_RUSTFLAGS -ErrorAction SilentlyContinue
    }

    Write-Step 'release のビルド（cargo build --release）'
    # 依存クレートのソースのパス（パニックの場所として exe に入る）から、ビルドした人のフォルダの名前を消すため、
    # Cargo のホームを「cargo」に置き換えてビルドする
    # ユーザーのフォルダが空やドライブのルートだと、置き換えも確かめも意味を持たないので止める
    $userProfile = if ($env:USERPROFILE) { [System.IO.Path]::GetFullPath($env:USERPROFILE).TrimEnd('\') } else { '' }
    if (-not $userProfile -or $userProfile -match '^[A-Za-z]:$' -or -not (Test-Path -LiteralPath $userProfile -PathType Container)) {
        throw "ユーザーのフォルダ（USERPROFILE）が使えません（$env:USERPROFILE）"
    }
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $userProfile '.cargo' }
    $cargoHome = [System.IO.Path]::GetFullPath($cargoHome).TrimEnd('\')
    $env:CARGO_ENCODED_RUSTFLAGS = "--remap-path-prefix=$cargoHome=cargo" + [char]0x1F + $hardening
    try {
        # exe の場所は CARGO_TARGET_DIR・CARGO_BUILD_TARGET などで変わるので、決め打ちにせず、このビルドで cargo が
        # 知らせた成果物（compiler-artifact の executable）を使う。診断は人が読む形で stderr に出る
        $buildMessages = @(& cargo build --release --locked --message-format=json-render-diagnostics)
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build --release が失敗しました（終了コード $LASTEXITCODE）"
        }
    } finally {
        Remove-Item Env:CARGO_ENCODED_RUSTFLAGS -ErrorAction SilentlyContinue
    }
    $executables = @(
        $buildMessages |
            Where-Object { $_.StartsWith('{') } |
            ForEach-Object { $_ | ConvertFrom-Json } |
            Where-Object { $_.reason -eq 'compiler-artifact' -and $_.target.name -ceq 'CLCLR' -and @($_.target.kind) -contains 'bin' -and $_.executable } |
            ForEach-Object { $_.executable }
    )
    if ($executables.Count -ne 1) {
        throw "cargo build の出力から CLCLR の exe を1つに決められません（$($executables.Count) 件）"
    }
    $exe = $executables[0]
    if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
        throw "$exe ができていません"
    }
    # zip と文面の置き場（出力先の基点の dist）
    $metadata = @(& cargo metadata --format-version 1 --no-deps)
    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata が失敗しました（終了コード $LASTEXITCODE）"
    }
    $targetDir = ($metadata -join "`n" | ConvertFrom-Json).target_directory
    if (-not $targetDir) {
        throw 'cargo metadata の出力に target_directory がありません'
    }
    $fileVersion = (Get-Item -LiteralPath $exe).VersionInfo.FileVersion
    if ($fileVersion -ne "$version.0") {
        throw "CLCLR.exe のファイルバージョン（$fileVersion）が Cargo.toml の版（$version）と合いません"
    }
    if (-not (Test-X64Exe $exe)) {
        throw "CLCLR.exe が x64 の実行ファイルではありません（$exe）"
    }
    # CRT を静的にリンクしたので、Visual C++ の実行環境と UCRT の DLL は import していないはず（古い C ランタイムの
    # msvcrt.dll・msvcr<版>.dll も、混ざっていないことを確かめる）
    $runtimeImports = @(Get-ImportedDll $exe | Where-Object { $_ -match '^(vcruntime|msvcp|msvcr|ucrtbase|api-ms-win-crt-)' })
    if ($runtimeImports.Count -gt 0) {
        throw "CLCLR.exe が C ランタイムの DLL を import しています（$($runtimeImports -join '、')）。CRT の静的リンクが効いていません"
    }
    if (-not (Test-GuardCf $exe)) {
        throw "CLCLR.exe に Control Flow Guard（CFG）の印がありません。CFG の指定が効いていません"
    }
    # CET シャドウスタック互換の印（build.rs がリンカへ /CETCOMPAT を渡す）
    if (-not (Test-CetCompat $exe)) {
        throw "CLCLR.exe に CET シャドウスタック互換の印がありません（build.rs の /CETCOMPAT が効いていません）"
    }
    $leaks = @(Find-LocalPath $exe @($userProfile, $cargoHome, (Get-ShortFolderPath $userProfile), (Get-ShortFolderPath $cargoHome)))
    if ($leaks.Count -gt 0) {
        throw "CLCLR.exe にビルドした環境のパスが残っています（$($leaks -join '、')）"
    }

    Write-Step 'zip を作る'
    $name = "CLCLR-$version-x64"
    $dist = Join-Path $targetDir 'dist'
    $zip = Join-Path $dist "$name.zip"
    if (-not (Test-Path -LiteralPath $dist)) {
        New-Item -ItemType Directory -Path $dist | Out-Null
    }
    if (Test-Path -LiteralPath $zip) {
        Remove-Item -LiteralPath $zip -Force
    }
    # 前の実行の目録のファイルを、この実行の zip の隣に残さない（この実行の目録は、取り直して比べた後に書く）
    foreach ($suffix in '', '-before', '-after') {
        $oldToolsPath = Join-Path $dist "build-tools-$version$suffix.json"
        if (Test-Path -LiteralPath $oldToolsPath) {
            Remove-Item -LiteralPath $oldToolsPath -Force
        }
    }
    # zip の項目の名前と元のファイル
    $sources = [ordered]@{ 'CLCLR.exe' = $exe }
    foreach ($file in $packageFiles) {
        $sources[$file] = Join-Path $root $file
    }
    # 元のファイルの SHA-256（zip の中身と照らす）
    $expected = @{}
    foreach ($entryName in $sources.Keys) {
        $expected[$entryName] = (Get-FileHash -LiteralPath $sources[$entryName] -Algorithm SHA256).Hash
    }
    # Compress-Archive は隠しファイルを黙って飛ばし、-Path はパスの [ ] をワイルドカードとして読むので使わず、
    # 項目の名前を指定して1つずつ入れる
    $archive = [System.IO.Compression.ZipFile]::Open($zip, [System.IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($entryName in $sources.Keys) {
            [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile($archive, $sources[$entryName], $entryName, [System.IO.Compression.CompressionLevel]::Optimal) | Out-Null
        }
    } finally {
        $archive.Dispose()
    }

    # 作った zip の項目がちょうど4つ（名前は大文字・小文字まで一致）で、中身が元と同じことを確かめる
    $archive = [System.IO.Compression.ZipFile]::OpenRead($zip)
    try {
        $entries = @($archive.Entries)
        $names = @($entries | ForEach-Object { $_.FullName } | Sort-Object -CaseSensitive)
        $wanted = @($sources.Keys | Sort-Object -CaseSensitive)
        if (($names -join '|') -cne ($wanted -join '|')) {
            throw "zip の項目が想定と違います（$($names -join '・')）"
        }
        foreach ($entry in $entries) {
            if ((Get-ZipEntryHash $entry) -ne $expected[$entry.FullName]) {
                throw "zip の中の $($entry.FullName) が元のファイルと違います"
            }
        }
    } finally {
        $archive.Dispose()
    }

    # テスト・ビルドの間に作業ツリーやコミットが変わっていないか（変わっていれば zip は $head の成果物と言えない）
    $dirtyAfter = Invoke-Git @('status', '--porcelain')
    $headAfter = Get-GitLine @('rev-parse', 'HEAD')
    if ($headAfter -ne $head -or ($dirtyAfter -join "`n") -cne ($dirty -join "`n")) {
        Stop-OrWarn 'テスト・ビルドの間に作業ツリーかコミットが変わりました'
    }

    # テスト・ビルドの間に道具が変わっていないか（変わっていれば、成果物がどの道具で作られたかを言えないので、
    # 下書きでも止める）
    Write-Step 'ビルドに使う道具の目録を取り直して比べる'
    $beforePath = Join-Path $dist "build-tools-$version-before.json"
    try {
        $toolsAfter = Get-BuildToolInventory
    } catch {
        # 取り直せなかったときも、比べる元（テストの前の目録）を残してから止まる
        [System.IO.File]::WriteAllText($beforePath, (($toolsBefore | ConvertTo-Json -Depth 10) -replace "`r`n", "`n"), (New-Object System.Text.UTF8Encoding $false))
        throw "ビルドに使う道具の目録を取り直せませんでした（テストの前の目録は $beforePath）。この実行の成果物は使えません: $($_.Exception.Message)"
    }
    if ((ConvertTo-InventoryText $toolsAfter) -cne $toolsBeforeText) {
        $afterPath = Join-Path $dist "build-tools-$version-after.json"
        [System.IO.File]::WriteAllText($afterPath, (($toolsAfter | ConvertTo-Json -Depth 10) -replace "`r`n", "`n"), (New-Object System.Text.UTF8Encoding $false))
        [System.IO.File]::WriteAllText($beforePath, (($toolsBefore | ConvertTo-Json -Depth 10) -replace "`r`n", "`n"), (New-Object System.Text.UTF8Encoding $false))
        throw "テスト・ビルドの間に、ビルドに使う道具の目録が変わりました（$beforePath と $afterPath を比べてください）。この実行の成果物は使えません"
    }
    $buildTools = [ordered]@{
        inventory = $toolsBefore
        exe       = [ordered]@{ path = $exe; sha256 = $expected['CLCLR.exe']; rich = @(Get-RichHeaderEntry $exe) }
    }
    $buildToolsPath = Join-Path $dist "build-tools-$version.json"
    [System.IO.File]::WriteAllText($buildToolsPath, (($buildTools | ConvertTo-Json -Depth 10) -replace "`r`n", "`n"), (New-Object System.Text.UTF8Encoding $false))

    Write-Step 'SHA-256 とリリースの文面'
    $zipHash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash
    # zip の中の exe と同じことを上で確かめた値
    $exeHash = $expected['CLCLR.exe']
    $commit = Get-GitLine @('rev-parse', '--short', 'HEAD')
    if ($dirty.Count -gt 0) {
        $commit = "$commit（未コミットの変更あり）"
    }
    $rustc = @(& rustc --version)
    if ($LASTEXITCODE -ne 0 -or $rustc.Count -eq 0) {
        throw "rustc --version が失敗しました（終了コード $LASTEXITCODE）"
    }
    $rustc = "$($rustc[0])".Trim()
    $draftNote = ''
    if ($Draft) {
        $draftNote = "> 下書き（scripts\release.ps1 -Draft で作ったもの）。この zip は配らない。`n`n"
    }
    $notes = @"
$draftNote## CLCLR $tag（$($section.Date)）

$($section.Body)

### ダウンロード

``$name.zip`` を展開し、``CLCLR.exe`` を書き込みできるフォルダに置いて起動してください。zip には ``CLCLR.exe``・``README.md``・``LICENSE``・``THIRD-PARTY-NOTICES.md`` が入っています。動作環境と使い方は README.md を見てください。

``CLCLR.exe`` にはデジタル署名がありません。初めて起動するとき、Windows の SmartScreen が警告を出すことがあります。

### SHA-256

| ファイル | SHA-256 |
|---|---|
| ``$name.zip`` | ``$zipHash`` |
| ``CLCLR.exe`` | ``$exeHash`` |

ビルド: コミット ``$commit``、``$rustc``
"@
    $notesPath = Join-Path $dist "release-notes-$version.md"
    [System.IO.File]::WriteAllText($notesPath, ($notes -replace "`r`n", "`n"), (New-Object System.Text.UTF8Encoding $false))

    Write-Host ''
    Write-Host "zip:   $zip"
    Write-Host "  SHA-256 $zipHash"
    Write-Host "exe:   $exe"
    Write-Host "  SHA-256 $exeHash"
    Write-Host "文面:  $notesPath"
    Write-Host "目録:  $buildToolsPath"
    Write-Host ''
    if ($Draft) {
        Write-Host '下書きなので、GitHub へ出すコマンドは表示しません。' -ForegroundColor Yellow
    } else {
        Write-Host "GitHub へ出すときは、文面を確かめてから次を実行します（$repo にタグ $tag を作り、コミット $head に付けます）:"
        $ghArgs = @(
            'gh release create', (ConvertTo-PsLiteral $tag), (ConvertTo-PsLiteral $zip),
            '--repo', (ConvertTo-PsLiteral $repo),
            '--title', (ConvertTo-PsLiteral "CLCLR $tag"),
            '--notes-file', (ConvertTo-PsLiteral $notesPath),
            '--target', (ConvertTo-PsLiteral $head)
        )
        Write-Host "  $($ghArgs -join ' ')"
    }
} catch {
    Write-Host "中止: $($_.Exception.Message)" -ForegroundColor Red
    exit 1
} finally {
    Pop-Location
}
