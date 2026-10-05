<#
.SYNOPSIS
CLCLR の配布用の zip と、リリースのページの文面を作る（GitHub には出さない）。

.DESCRIPTION
次の順に行い、どこかで失敗したら止まる。

1. 作業ツリーに未コミットの変更が無く、main の最新（origin/main と同じ）であることを確かめる
2. Cargo.toml の版を読み、CHANGELOG.md にその版の節（「## 版（公開日）」）があることと、タグ v<版> が
   origin にまだ無いことを確かめる
3. cargo test（debug）と cargo build --release。どちらも C ランタイムを静的にリンクする（-C target-feature=+crt-static）。
   exe はこのビルドで cargo が知らせたものを使い、版と x64 であることと、C ランタイムの DLL を import していない
   ことを確かめる。release のビルドでは Cargo のホームのパスを「cargo」に置き換え（--remap-path-prefix）、
   exe にユーザーのフォルダと Cargo のホームのパスが残っていないことを確かめる（探すのはこの2つのパスだけ）
4. <出力先>\dist\CLCLR-<版>-x64.zip を作る（CLCLR.exe・README.md・LICENSE・THIRD-PARTY-NOTICES.md の4つ）。
   作った zip の項目がちょうどその4つで、中身が元のファイルと同じであることと、テスト・ビルドの間に作業ツリーと
   コミットが変わっていないことを確かめる
5. zip と exe（zip の中のもの）の SHA-256 を出し、リリースのページの文面 <出力先>\dist\release-notes-<版>.md を作る

GitHub へのリリース（タグの作成と gh release create）は外から見える操作なので、このスクリプトでは行わず、
最後に実行するコマンドを表示するだけにする（リポジトリは origin、タグの位置は確かめたコミットの SHA で指定する）。

.PARAMETER Draft
下書き。main 以外のブランチ・未コミットの変更・origin/main との違い・CHANGELOG.md の公開日の「未定」・
既にあるタグ・GitHub 以外の origin を、止めずに警告だけにする（試しに作るとき用）。下書きの zip は配らない。

.EXAMPLE
powershell -ExecutionPolicy Bypass -File scripts\release.ps1
#>
[CmdletBinding()]
param(
    [switch]$Draft
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

$root = Split-Path -Parent $PSScriptRoot
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

# CHANGELOG.md の「## 版（公開日）」の節（公開日と本文）
function Get-ChangelogSection([string]$changelog, [string]$version) {
    $lines = @(Get-Content -LiteralPath $changelog -Encoding UTF8)
    $pattern = '^## ' + [regex]::Escape($version) + '（(.+)）\s*$'
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
        throw "CHANGELOG.md に「## $version（公開日）」の節がありません"
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
        throw "CHANGELOG.md の「## $version」の節が空です"
    }
    return [pscustomobject]@{ Date = $date; Body = $body }
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
    # C ランタイム（CRT）を exe に静的にリンクする。Visual C++ の実行環境（VCRUNTIME140.dll）が入っていない PC でも
    # 動き、exe の隣に置かれた同じ名前の DLL も関係しなくなる。テストも同じ指定で通す
    $crtStatic = '-Ctarget-feature=+crt-static'

    Write-Step 'テスト（cargo test。配布のビルドと同じく CRT を静的にリンクする）'
    $env:CARGO_ENCODED_RUSTFLAGS = $crtStatic
    try {
        Invoke-Checked 'cargo test' { cargo test }
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
    # CARGO_ENCODED_RUSTFLAGS は、指定を 0x1F の文字で区切って並べる
    $env:CARGO_ENCODED_RUSTFLAGS = "--remap-path-prefix=$cargoHome=cargo" + [char]0x1F + $crtStatic
    try {
        # exe の場所は CARGO_TARGET_DIR・CARGO_BUILD_TARGET などで変わるので、決め打ちにせず、このビルドで cargo が
        # 知らせた成果物（compiler-artifact の executable）を使う。診断は人が読む形で stderr に出る
        $buildMessages = @(& cargo build --release --message-format=json-render-diagnostics)
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
    # CRT を静的にリンクしたので、Visual C++ の実行環境と UCRT の DLL は import していないはず
    $runtimeImports = @(Get-ImportedDll $exe | Where-Object { $_ -match '^(vcruntime|msvcp|ucrtbase|api-ms-win-crt-)' })
    if ($runtimeImports.Count -gt 0) {
        throw "CLCLR.exe が C ランタイムの DLL を import しています（$($runtimeImports -join '、')）。CRT の静的リンクが効いていません"
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
$draftNote## CLCLR $version（$($section.Date)）

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
    Write-Host ''
    if ($Draft) {
        Write-Host '下書きなので、GitHub へ出すコマンドは表示しません。' -ForegroundColor Yellow
    } else {
        Write-Host "GitHub へ出すときは、文面を確かめてから次を実行します（$repo にタグ $tag を作り、コミット $head に付けます）:"
        $ghArgs = @(
            'gh release create', (ConvertTo-PsLiteral $tag), (ConvertTo-PsLiteral $zip),
            '--repo', (ConvertTo-PsLiteral $repo),
            '--title', (ConvertTo-PsLiteral "CLCLR $version"),
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
