// 設定画面と、ビューアの入力ダイアログのダイアログとコントロールの ID。ここが正で、build.rs が
// ここから Rust の定数（OUT_DIR/resource_ids.rs）を作る。1行に1つ、「#define 名前 数」の形で書く。

// ダイアログ
#define IDD_SETTINGS        200
#define IDD_PAGE_GENERAL    201
#define IDD_PAGE_HISTORY    202
#define IDD_PAGE_HOTKEY     203
#define IDD_PAGE_TEXT       204
#define IDD_PAGE_FORMAT     205
#define IDD_PAGE_WINDOW     206

// 親のダイアログ
#define IDC_SET_TAB         1000
#define IDC_SET_ERROR       1001

// 全般
#define IDC_GEN_WATCH       1100
#define IDC_GEN_TRAY        1101
#define IDC_GEN_START_HIDDEN 1102
#define IDC_GEN_SYNC        1103
#define IDC_GEN_NOTIFY      1104

// 履歴
#define IDC_HIS_MAX         1200
#define IDC_HIS_MAX_SPIN    1201
#define IDC_HIS_GROUP       1202
#define IDC_HIS_VISIBLE     1203
#define IDC_HIS_VISIBLE_SPIN 1204
#define IDC_HIS_FOLDERS     1205
#define IDC_HIS_FOLDERS_SPIN 1206
#define IDC_HIS_PER_FOLDER  1207
#define IDC_HIS_PER_FOLDER_SPIN 1208
#define IDC_HIS_FORMAT      1209
#define IDC_HIS_TOTAL       1210
#define IDC_HIS_OVERLAP     1211
#define IDC_HIS_INTERVAL    1212
#define IDC_HIS_INTERVAL_SPIN 1213
#define IDC_HIS_SAVE_EXIT   1214
#define IDC_HIS_SAVE_CHANGE 1215
#define IDC_HIS_DELETE_ON_SEND 1216
#define IDC_HIS_SOUND       1217
#define IDC_HIS_SOUND_FILE  1218

// ホットキー
#define IDC_HK_POPUP        1300
#define IDC_HK_CTRL         1301
#define IDC_HK_SHIFT        1302
#define IDC_HK_ALT          1303
#define IDC_HK_WIN          1304
#define IDC_HK_KEY          1305
#define IDC_HK_AUTOPASTE    1306
#define IDC_HK_MENU_ITEMS   1307
#define IDC_HK_MENU_ITEMS_SPIN 1308
#define IDC_HK_TOOLTIP      1309
#define IDC_HK_TIP_DELAY    1310
#define IDC_HK_TIP_DELAY_SPIN 1311
#define IDC_HK_TIP_CHARS    1312
#define IDC_HK_TIP_CHARS_SPIN 1313
#define IDC_HK_TIP_LINES    1314
#define IDC_HK_TIP_LINES_SPIN 1315
#define IDC_HK_DP_CTRL      1316
#define IDC_HK_DP_SHIFT     1317
#define IDC_HK_DP_ALT       1318
#define IDC_HK_PINNED_FIRST 1319

// テキスト変換
#define IDC_TX_QUOTE        1400
#define IDC_TX_WRAP         1401
#define IDC_TX_WRAP_SPIN    1402
#define IDC_TX_OPEN         1403
#define IDC_TX_CLOSE        1404
#define IDC_TX_TRIM         1405
#define IDC_TX_DATE         1406
#define IDC_TX_DATE_FMT     1407
#define IDC_TX_TIME_FMT     1408

// 形式フィルタ
#define IDC_FMT_DEFAULT     1500
#define IDC_FMT_LIST        1501
#define IDC_FMT_ADD         1502
#define IDC_FMT_DELETE      1503
#define IDC_FMT_NAME        1504
#define IDC_FMT_ACTION      1505
#define IDC_FMT_SAVE        1506
#define IDC_FMT_LIMIT       1507
#define IDC_FMT_LIMIT_SIZE  1508

// ウィンドウフィルタ
#define IDC_WIN_LIST        1600
#define IDC_WIN_ADD         1601
#define IDC_WIN_DELETE      1602
#define IDC_WIN_TITLE       1603
#define IDC_WIN_CLASS       1604
#define IDC_WIN_IGNORE      1605

// ピン留めの名前の変更（ビューア）
#define IDD_RENAME          300
#define IDC_RENAME_NAME     3000
