//! CF_HDROP（DROPFILES構造体）の解釈。
//! ビューアとメニューのツールチップ（`menu_tooltip.rs`）が共用するため、UI から独立させている。

/// CF_HDROP（DROPFILES構造体 + ファイル名リスト）からパス一覧を取り出す。
/// DROPFILES: pFiles(u32) + pt(POINT, 8バイト) + fNC(BOOL) + fWide(BOOL)、
/// オフセットpFilesから二重NUL終端の文字列リストが続く。
pub fn parse_hdrop(data: &[u8]) -> Vec<String> {
    let mut paths = Vec::new();
    if data.len() < 20 {
        return paths;
    }
    let p_files = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let f_wide = u32::from_le_bytes([data[16], data[17], data[18], data[19]]) != 0;
    if p_files >= data.len() {
        return paths;
    }
    if f_wide {
        let units: Vec<u16> = data[p_files..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        for chunk in units.split(|&u| u == 0) {
            if chunk.is_empty() {
                break; // 連続NUL＝リスト終端
            }
            paths.push(String::from_utf16_lossy(chunk));
        }
    } else {
        // ANSIリストは現代のWindowsでは実質使われないため、ロスの少ないUTF-8解釈で妥協する
        for chunk in data[p_files..].split(|&b| b == 0) {
            if chunk.is_empty() {
                break;
            }
            paths.push(String::from_utf8_lossy(chunk).into_owned());
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::parse_hdrop;

    /// DROPFILES構造体（wide版）を組み立てる。pFiles=20固定（ヘッダ直後から開始）。
    fn dropfiles_wide(paths: &[&str]) -> Vec<u8> {
        let mut data = vec![0u8; 20];
        data[0..4].copy_from_slice(&20u32.to_le_bytes()); // pFiles
        data[16..20].copy_from_slice(&1u32.to_le_bytes()); // fWide = TRUE
        for p in paths {
            for u in p.encode_utf16() {
                data.extend_from_slice(&u.to_le_bytes());
            }
            data.extend_from_slice(&0u16.to_le_bytes());
        }
        data.extend_from_slice(&0u16.to_le_bytes()); // リスト終端の追加NUL
        data
    }

    /// DROPFILES構造体（ANSI版）。fWideはゼロ初期化のままFALSE。
    fn dropfiles_ansi(paths: &[&str]) -> Vec<u8> {
        let mut data = vec![0u8; 20];
        data[0..4].copy_from_slice(&20u32.to_le_bytes());
        for p in paths {
            data.extend_from_slice(p.as_bytes());
            data.push(0);
        }
        data.push(0);
        data
    }


    #[test]
    fn parse_hdrop_wide_extracts_multiple_paths() {
        let data = dropfiles_wide(&[r"C:\a.txt", r"D:\dir\b.png"]);
        assert_eq!(parse_hdrop(&data), vec![r"C:\a.txt", r"D:\dir\b.png"]);
    }

    #[test]
    fn parse_hdrop_ansi_extracts_multiple_paths() {
        let data = dropfiles_ansi(&[r"C:\a.txt", r"D:\dir\b.png"]);
        assert_eq!(parse_hdrop(&data), vec![r"C:\a.txt", r"D:\dir\b.png"]);
    }

    #[test]
    fn parse_hdrop_returns_empty_for_data_shorter_than_header() {
        assert!(parse_hdrop(&[0u8; 19]).is_empty());
    }

    #[test]
    fn parse_hdrop_returns_empty_for_out_of_range_offset() {
        let mut data = vec![0u8; 20];
        // pFilesがdataの範囲外を指す不正な入力
        data[0..4].copy_from_slice(&999u32.to_le_bytes());
        assert!(parse_hdrop(&data).is_empty());
    }

    #[test]
    fn parse_hdrop_returns_empty_for_empty_list() {
        // リストがいきなり終端（二重NUL）＝ファイル0件
        assert!(parse_hdrop(&dropfiles_wide(&[])).is_empty());
    }
}
