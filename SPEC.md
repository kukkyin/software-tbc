# software-tbc — 設計仕様書 (SPEC)

ソフトウェア TBC（タイムベースコレクタ）を Rust で実装するための設計仕様。
アナログ／レガシー映像（VHS、S-VHS、コンポジット出力のLD・ゲーム機等）のキャプチャ後処理と、
リアルタイムストリーム処理の両方で「普通に使える」実用レベルを目標とする。

- 対象プラットフォーム: Windows 10/11（主対象）、Linux（CI・開発用にサポート）
- 言語: Rust (stable, MSRV 1.75)
- ライセンス: MIT
- 本書の位置付け: 実装の正は本書と各 Issue の受け入れ条件。矛盾する場合は Issue > 本書の順で優先する。

---

## 1. 用語と前提

| 用語 | 意味 |
|---|---|
| フレーム | 1枚の画像データ＋メタデータ（`VideoFrame`）。インターレース時は2フィールドを含む |
| フィールド | 奇数/偶数ラインのみの半フレーム。Top Field / Bottom Field |
| 入力クロック | ソースがフレームを生成する実時間軸。キャプチャデバイスやデコーダ由来で揺らぐ |
| 出力クロック | ターゲットフレームレートで厳密に刻む目標時間軸 |
| ジッタ | 到着時刻・ライン位相の短期的な揺らぎ |
| ドリフト | 入力クロックと公称レートの長期的なずれ（ppm単位） |
| プロックアンプ | 映像のゲイン・黒レベル・彩度・色相を調整する回路。本ソフトではそのソフトウェア相当 |

前提:

- 扱う信号は RAW フレーム列（ヘッダなし）。解像度・ピクセルフォーマット・フレームレートは外部から与えられる。
- 対応フォーマットは `yuv420p` / `yuv422p` / `rgb24`（8bit）に加え、パックド 4:2:2 の
  `uyvy` / `yuyv` を境界変換で対応する（Issue #23、§3）。10bit の `yuv420p10le` /
  `yuv422p10le` も境界変換で対応する（Issue #27、§3）。
- インターレースは 480i/576i を主対象とし、フィールドオーダーを保持したまま処理する。

## 2. ワークスペース構成

```
software-tbc/
├── Cargo.toml                # workspace 定義
├── SPEC.md                   # 本書
├── AGENTS.md                 # 実装エージェント向け規約
├── README.md
├── LICENSE
├── .github/workflows/ci.yml
├── crates/
│   ├── tbc-core/             # 信号処理パイプライン（I/Oなし、純粋な変換）
│   │   └── src/{lib,sync,jitter,field,procamp,guard,telemetry,pool}.rs
│   ├── tbc-io/               # FrameSource / FrameSink と RAW ファイル・パイプ実装
│   │   └── src/lib.rs
│   ├── tbc-sim/              # 擬似不安定映像ジェネレータ＋ベンチマーク基盤
│   │   └── src/lib.rs
│   └── tbc-cli/              # CLI フロントエンド（バイナリ名: software-tbc）
│       └── src/main.rs
└── examples/                 # FFmpeg 連携サンプル等（将来）
```

依存方向は一方向のみ: `tbc-cli → tbc-io → tbc-core`、`tbc-sim → tbc-core`。
`tbc-core` は std のみ（例外: `thiserror`）に依存し、OS API・スレッド生成・時刻取得を行わない
（時刻は呼び出し側が `pts_ns` として注入する）。これにより単体テストとベンチが決定的になる。

## 3. 共通データ構造（tbc-core）

正の定義は `crates/tbc-core/src/lib.rs`。要点:

```rust
pub enum PixelFormat { Yuv420p, Yuv422p, Rgb24, Uyvy, Yuyv, Yuv420p10, Yuv422p10 }  // frame_size(w,h) を提供
pub struct FrameRate { pub num: u32, pub den: u32 }     // 有理数。29.97 = 30000/1001
pub enum FieldOrder { Progressive, TopFieldFirst, BottomFieldFirst }
pub struct VideoFrame {
    pub width: u32, pub height: u32,
    pub format: PixelFormat,
    pub field_order: FieldOrder,
    pub pts_ns: u64,        // 入力クロック上の提示時刻 [ns]
    pub sequence: u64,      // 取り込み順の単調増加番号
    pub data: Vec<u8>,      // §5 のプールに載せ替え予定。validate() でサイズ検証
}
pub enum TbcError { FrameSizeMismatch{..}, InvalidConfig(String), SignalLost }
```

注記（Issue #23）: パイプライン内部の `Yuv422p` 表現は Y0 Cb Y1 Cr のパックド
バイト順であり、平面配置ではない。`Uyvy` / `Yuyv` は入出力境界でのみ現れる形式で、
`VideoFrame::to_yuv422p` / `from_yuv422p` により内部表現と可逆変換する
（4:2:2 同士のためバイト並べ替えのみでビット完全）。パックド形式は偶数幅必須。

注記（Issue #27）: `Yuv420p10` / `Yuv422p10` はプレーナ配置・u16 リトルエンディアン・
下位10ビットに値を持つ境界形式（FFmpeg の `yuv420p10le` / `yuv422p10le`）で、
`VideoFrame::to_8bit` / `from_8bit` により 8bit の内部表現と変換する。内部処理は
8bit のままである点に注意（ネイティブ 16bit 処理は将来拡張、§10）。スケーリングは
10→8 が `(v * 255 + 511) / 1023`、8→10 が `(v * 1023 + 127) / 255`（端点が正確に
対応する最近傍丸め）で、8→10→8 の往復は無損失、10→8→10 の往復誤差は数 LSB 以内。
上位ビットに値を格納する変種（p010 等）は対象外。幾何制約は 8bit 版と同じ
（420 系は偶数幅・偶数高さ、422 系は偶数幅）。

エラー方針:

- ライブラリクレートは `thiserror` による型付きエラー。`anyhow` は `tbc-cli` 内でのみ可。
- パニックは「プログラムのバグ」以外で起こさない。入力データ起因の失敗はすべて `Result` で返す。
- `f64` の時刻計算は禁止（ドリフト蓄積防止）。時刻は `u64` ナノ秒＋有理数レートで扱い、
  丸めは「最も近い偶数」ではなく単純な四捨五入で統一する。

## 4. コア処理パイプライン（tbc-core）

処理順序は固定: **取り込み → リングバッファ → レート整列 → ヘッドスイッチング
マスク（§4.7）→ ラインジッタ補正 → フィールド処理 → プロックアンプ →
ガード（信号断検出）→ 出力**。
テレメトリは各段の通過時にカウンタを更新する。

### 4.1 フレーム同期・レート整列（`sync.rs`）

目的: 入力クロックのジッタ／ドリフト／ドロップを吸収し、出力クロックに厳密整列した列を出す。

- SPSC（単一生産者・単一消費者）リングバッファ。容量は `buffer_frames`（既定8）。
  生産者・消費者ともロックフリー（原子カウンタ＋事前確保スロット）。§5参照。
- 出力スケジューリング: 出力フレーム *k* の目標時刻は `t_k = t_0 + k * (den/num) 秒`。
  `t_0` は最初の入力フレームの `pts_ns` をバッファ中心に置くよう初期化する
  （初期遅延 = `buffer_frames/2` フレーム分）。
- ドリフト推定: 入力到着間隔の指数移動平均（EMA, α=1/256）から実効入力レートを推定し、
  推定ドリフトが ±500ppm を超えたら目標時刻の基準 `t_0` を緩やかに再アンカーする
  （1フレームあたり最大 1/8 フレーム分まで。急激な再アンカーは禁止）。
- レートポリシー（`RatePolicy`）:
  - `HoldDrop`（既定）: 出力時刻に最も近い過去フレームを保持。遅れたフレームはドロップ。
    重複・ドロップはテレメトリに計上する。
  - `Blend`: 前後2フレームを時間重み `w = (t_k - t_a)/(t_b - t_a)` で線形ブレンド。
    `out = a*(1-w) + b*w`（各成分、u8→u16で計算し丸めて u8）。
  - `MotionInterpolate`: 将来実装。当面は `Blend` にフォールバックし、その旨をログに残す。
- アンダーフロー（出力時刻にフレーム未到着）: ガード段（§4.5）の信号断ではなく
  「直前フレームの保持」として扱い、保持継続時間が `signal_loss_timeout_ms`（既定200ms）を
  超えた場合のみ信号断に昇格する。

### 4.2 走査線ジッタ補正（`jitter.rs`）

目的: ラインごとの水平位相ずれ（VHSのヘッド切替ノイズや同期不安定）を検出し補正する。

- 検出: 各ラインについて、ライン先頭の「同期相当領域」がない RAW 入力では、
  代替として**前ラインとの位相相関**を用いる。各ラインの輝度列 `Y_l[x]` に対し、
  参照ライン（前フレームの同ライン、なければ前ライン）との間で
  探索範囲 ±`max_shift_px`（既定 ±8px）の正規化相互相関を計算し、
  ピーク位置を放物線補間で 1/256 px 精度（`LineShift.shift_q8`）まで推定する。
- 外れ値除去: 推定シフト列にメディアンフィルタ（窓3）を適用し、単発の誤検出を落とす。
- 補正: 推定シフトの逆方向へラインをサブピクセルシフトする。補間は線形。
  端部はエッジ画素の複製（クランプ）で埋める。色差は輝度と同一シフトを適用する
  （4:2:2/4:2:0 では色差座標にスケールを掛ける）。
- 無効化条件: フレーム全体の相関ピーク平均が閾値（既定 0.3）未満なら、そのフレームは
  補正をスキップする（静止画・暗転での誤補正防止）。

### 4.3 フィールド処理（`field.rs`）

- `FieldOrder` をメタデータとして厳密に保持し、プロックアンプ等のライン走査処理は
  フィールド単位のライン順序を壊さない。
- オプション `weave_check`: 2フィールド間の動き量（ライン差分絶対値の平均）が閾値未満なら
  プログレッシブ相当として扱い、ジッタ補正の参照にフレーム全体を使えるようにする。
- 簡易デインターレース（オプション、既定オフ）: Bob（フィールドをライン補間で2倍化）方式のみ。
  高品質デインターレース（Yadif等）は範囲外。
- フィールド順序の自動判定（Issue #26、`FieldOrderDetector`）: 前フレームの各フィールドを
  前後の反対フィールドから（垂直補間で位置を合わせ時間平均して）予測し、予測誤差の小さい
  仮説を真の順序とする。十分な動きと明確な差が揃った時点で確定し、以後は覆らない。
  静止・低動き・短尺・プログレッシブ相当では確定せず、設定のフォールバック順序を使う。

### 4.4 プロックアンプ（`procamp.rs`）

全段で 8bit→内部 u16 演算、最終段でクランプ＋丸め。LUT化できるものはフレーム単位で
256要素LUTを再生成し、画素ループはLUT引きのみにする。

- 輝度: `Y' = clamp( (Y - 16) * luma_gain + 16 + pedestal )`（限定レンジ前提。
  フルレンジ入力では `pedestal` のみ加算するモード `range: Full` を用意）
- ガンマ: `Y'' = 255 * (Y'/255)^(1/gamma)` をLUT化（Yのみ。色差には適用しない）
- 彩度・色相: 色差平面で回転＋スケール:
  `Cb' = (Cb-128)*sat*cos(h) - (Cr-128)*sat*sin(h) + 128`
  `Cr' = (Cb-128)*sat*sin(h) + (Cr-128)*sat*cos(h) + 128`
- クロマシフト（Issue #30）: `chroma_shift_px` で色差を輝度に対して水平に
  サブピクセル移動し、コンポジット系ソースの色遅延（色ズレ）を補正する。
  色差サンプル列を線形補間でシフト（端はクランプ）、回転・スケールの前に適用。
  RGB 入力には適用しない。
- RGB入力時は YCbCr 変換行列（BT.601、限定レンジ）を前段に挿入し、出力時に逆変換する。
  変換式は ITU-R BT.601 の係数を固定小数点（小数部8bit）で実装する。

### 4.5 ガード：信号断・異常検出（`guard.rs`）

- 検出条件（いずれか）: (a) §4.1 の保持継続が `signal_loss_timeout_ms` 超過、
  (b) フレーム全体の輝度分散が閾値未満かつ色差がほぼ無彩色（無信号ノイズ/ブルーバック相当は除く）、
  (c) 入力フレームの `validate()` 失敗が連続3回。
- 動作（`SignalLossPolicy`）: `LastFrameHold`（既定）/ `Blackout` / `TestPattern`（カラーバー生成）。
- 復帰: 良好なフレームが2フレーム連続で到着したら通常処理へ復帰し、イベントをログに残す。
- 信号断中も出力クロックは止めない（下流のFFmpeg等がタイムアウトしないため）。

### 4.6 テレメトリ（`telemetry.rs`）

`Telemetry` スナップショット（正は `telemetry.rs`）:

- `latency_ms`: 出力フレームの（出力時刻 − 入力 `pts_ns`）の直近値
- `buffer_fill_ratio`: リングバッファ充填率 0.0–1.0
- `dropped_frames` / `duplicated_frames`: 累計
- `jitter_ms`: 到着間隔の標準偏差（Welford法でオンライン推定）
- `frames_in` / `frames_out`: 累計

出力経路: `tbc-core` は `TelemetrySink` トレイト（`fn report(&mut self, &Telemetry)`）を受け取り、
周期は呼び出し側が決める。`tbc-cli` は1秒間隔で JSON Lines として stderr に出す
（`tracing` + 構造化フィールド。フォーマットは §7 の `telemetry_interval_ms` で制御）。

### 4.7 ヘッドスイッチングマスク（`head_switch.rs`）

VHS 等でフレーム下部に現れるヘッドスイッチングノイズを、直上の正常ラインで
隠蔽する段（Issue #29）。レート整列の直後・ジッタ補正の前に置き、ノイズが
ライン別ジッタ推定を汚さないようにする。

- 方式は2つ: `replicate`（マスク領域の全ラインを直上の正常ラインで置換）と
  `interpolate`（各マスクラインを直上2つの出力ラインの平均で再帰生成）。
- 設定は `head_switch_lines`（0 = 無効、既定）と `head_switch_mode`。
  ライン数はフレーム高さ未満でなければならない。
- 420 では色差平面の下部 ceil(N/2) ラインも同様に隠蔽する。10bit 形式は
  境界変換後の 8bit フレームに対して適用される。

### 4.8 幾何変換（`geometry.rs`）

入力・処理・出力の幾何を分離する（Issue #31）。処理寸法は従来どおり
`width`/`height`（有効領域）。入力寸法 `input_width`/`input_height`（未指定なら
処理寸法）でソースを読み、リーダーで境界変換の直後に
`crop_left`/`crop_top` を原点とする処理ウィンドウを切り出す。以降のパイプラインは
処理幾何しか見ない。出力は `output_width`/`output_height`（未指定なら処理寸法）の
黒キャンバスへ `pad_left`/`pad_top` の位置に処理済みフレームを置く（emit 時、
ワイヤ形式変換の直後）。制約: 両ウィンドウが相手の幾何に収まること。オフセットは
色差のサブサンプリングに合わせ、4:2:0 では両方向とも偶数、4:2:2 系では left が
偶数（RGB は無制約）。違反は設定エラー（終了コード 2）。

## 5. メモリ・パフォーマンス設計

- **事前確保**: 起動時に `buffer_frames + 2` 枚分のフレームバッファをプールとして確保し、
  稼働中のヒープ確保は原則禁止（プール枯渇時のみ確保し、テレメトリに警告計上）。
- **ゼロコピー方針**: パイプライン内部ではバッファの所有権を移動する（コピーしない）。
  ブレンド等で2入力が必要な段のみ、プールから出力バッファを借りて書き込む。
- **遅延要件**: 追加遅延の上限は `buffer_frames/2 + 1` フレーム分（既定設定で約5フレーム、
  29.97fps で約167ms）。`low-latency` プリセットでは `buffer_frames=2`（約2フレーム、約67ms）。
- **スループット要件**: 720×480 yuv422p で、2020年以降の一般的なx86 CPU 1コアあたり
  リアルタイムの4倍速以上（ベンチで検証、§8）。
- **スレッドモデル**: `tbc-core` はシングルスレッド前提（`&mut self`）。並行化は
  `tbc-cli` が「入力スレッド → SPSCリング → 処理・出力スレッド」で構成する。

## 6. 入出力（tbc-io / tbc-cli）

`FrameSource` / `FrameSink` トレイト（正は `crates/tbc-io/src/lib.rs`）:

```rust
pub trait FrameSource { fn next_frame(&mut self) -> Result<Option<VideoFrame>, IoError>; }
pub trait FrameSink   { fn write_frame(&mut self, f: &VideoFrame) -> Result<(), IoError>;
                        fn flush(&mut self) -> Result<(), IoError>; }
```

実装済み／予定:

- `RawReader<R: Read>` / `RawWriter<W: Write>`: ヘッダなしRAW。EOFは「フレーム境界でのEOF」
  （`Ok(None)`）と「フレーム途中のEOF」（`TruncatedFrame`）を区別する（実装済み）。
- 連番ファイル（`frame_%06d.raw` 形式）: Phase 3 Issue。
- プラグインアダプタ: Windows キャプチャ（DirectShow/Media Foundation）、プレビュー、
  外部出力は、本トレイトを実装する別クレートとして後付けできる設計とする。
  本リポジトリのスコープではトレイト定義とモック実装まで。

CLI（`tbc-cli`、バイナリ名 `software-tbc`）:

- 引数: `--input`（`-`=stdin）、`--output`（`-`=stdout）、`--width/--height/--format`、
  `--preset`、`--config`（TOML）、`--stamp nominal|wall-clock`、`--standard ntsc|pal|1080i`、
  `--field-order progressive|top-first|bottom-first|auto`、
  `--head-switch-lines N`、`--head-switch-mode replicate|interpolate`、
  幾何分離（§4.8）: `--input-width/--input-height`、`--crop-left/--crop-top`、
  `--output-width/--output-height`、`--pad-left/--pad-top`。
  優先順位: CLI引数 > 設定ファイル > プリセット > 既定値。
- `--standard` は規格プリセット（Issue #25）: 幾何・目標レート・フィールド順序を一括指定する。
  ntsc = 720×480, 30000/1001, bottom-first / pal = 720×576, 25/1, top-first /
  1080i = 1920×1080, 30000/1001, top-first。解決順序は設定ファイルより上、
  個別の CLI フラグより下に位置し、個別フラグで上書きできる。
- `--field-order auto`（Issue #26）: 取り込み直後のフレーム列から §4.3 の検出器で
  順序を判定し、確定した順序を全出力フレームのメタデータに適用する。確定するまでは
  （確定しない場合も）他レイヤーで解決された順序がフォールバックになる。
  具体値を指定すると自動判定は無効になる。
- `--stamp` は入力フレームの `pts_ns` 生成方式（Issue #20）。`nominal`（既定）は
  連番×公称レートで決定的。`wall-clock` は到着実時刻（単調クロック、初回フレーム=0）とし、
  ライブ入力の実ドリフトや到着ジッタを §4.1 の同期・ドリフト推定が見られるようになる。
  連番ファイル入力には到着時刻が存在しないため、`wall-clock` 指定時は警告して
  `nominal` にフォールバックする。
- 標準入出力はバイナリモード固定。Windows では `SetConsoleCP` 等に依存しない
  （パイプ前提のためコンソール入出力は行わない）。
- 終了コード: 0=正常終了（EOF到達）、2=引数/設定エラー、3=入出力エラー、4=信号断で異常終了
  （`--fail-on-signal-loss` 指定時）。

FFmpeg 連携例:

```powershell
# ファイルの後処理: 不安定なキャプチャを整列して H.264 化
ffmpeg -i capture.mp4 -f rawvideo -pix_fmt yuv422p -s 720x480 -r 30000/1001 - |
  software-tbc --width 720 --height 480 --format yuv422p --preset vhs |
  ffmpeg -f rawvideo -pix_fmt yuv422p -s 720x480 -r 30000/1001 -i - -c:v libx264 out.mp4

# リアルタイム: キャプチャデバイス → TBC → 配信/録画（デバイス指定は環境依存）
ffmpeg -f dshow -i video="capture device" -f rawvideo -pix_fmt yuv422p - |
  software-tbc --preset low-latency | ffmpeg -f rawvideo ... 
```

## 7. 設定ファイルとプリセット

TOML 形式。`TbcConfig`（§3）＋CLI節をシリアライズしたもの。

```toml
# tbc.toml
[width]  # （実際はフラットキー。ここでは構造の説明用に省略記法）
```

実際のスキーマ（フラット）:

```toml
width = 720
height = 480
input_width = 720               # 入力寸法（省略時 = 処理寸法、§4.8）
input_height = 486
crop_left = 0
crop_top = 6
output_width = 720              # 出力寸法（省略時 = 処理寸法、§4.8）
output_height = 480
pad_left = 0
pad_top = 0
format = "yuv422p"              # yuv420p | yuv422p | rgb24 | uyvy | yuyv | yuv420p10le | yuv422p10le
field_order = "bottom-first"    # progressive | top-first | bottom-first | auto（自動判定、§4.3）
target_rate = "30000/1001"      # 有理数文字列
rate_policy = "hold-drop"       # hold-drop | blend | motion-interpolate
buffer_frames = 8
jitter_correction = true
head_switch_lines = 0           # 下部マスクのライン数（0 = 無効、§4.7）
head_switch_mode = "replicate"  # replicate | interpolate
signal_loss = "last-frame-hold" # last-frame-hold | blackout | test-pattern
signal_loss_timeout_ms = 200
telemetry_interval_ms = 1000
stamp = "nominal"               # nominal | wall-clock（§6、Issue #20）

[procamp]
luma_gain = 1.0
pedestal = 0.0
saturation = 1.0
hue_degrees = 0.0
gamma = 1.0
chroma_shift_px = 0.0           # 色差の水平シフト（輝度px単位、§4.4）
```

プリセット（`--preset`）:

| 名前 | 想定 | 主な変更点 |
|---|---|---|
| `vhs` | VHS/S-VHS キャプチャ | buffer 12、ジッタ補正ON（max_shift 12px）、blend、pedestal +2 |
| `svhs` | 安定したS-VHS/放送録画 | buffer 8、ジッタ補正ON、hold-drop |
| `unstable-composite` | 同期の弱いコンポジット信号 | buffer 16、timeout 500ms、test-pattern切替 |
| `low-latency` | リアルタイムプレビュー/配信 | buffer 2、hold-drop、ジッタ補正OFF |

## 8. 検証・ベンチマーク（tbc-sim）

実機なしで定量評価するための基盤。`DegradeParams`（正は `crates/tbc-sim/src/lib.rs`）:

- 水平ジッタ: ラインごとにランダムウォークする位相ずれ（±`line_jitter_px`）
- クロックドリフト: 到着間隔を `clock_drift_ppm` で伸縮
- フレーム欠落: 確率 `drop_probability` でフレームを間引き
- ドロップアウト: 確率 `dropout_probability` でラインをノイズ（乱数輝度）に置換
- ペデスタルずれ: 全画素に `pedestal_shift` を加算

乱数はシード固定可能な自前実装（xorshift）とし、外部クレートに依存しない（再現性のため）。

評価指標（ベンチ／統合テストで使用）:

- **整列誤差**: 出力フレームの目標時刻と実効時刻のずれ（ms、最大・平均）
- **ジッタ残差**: 補正後フレームのライン位相ずれ推定値（px、RMS）
- **処理時間**: フレームあたり処理時間（µs、段別）— criterion によるベンチ
- **往復テスト**: 「劣化付与 → TBC処理 → 既知の正解フレームと比較（PSNR）」で回帰を検出する

## 9. テスト方針

- 単体テストは各モジュールと同一ファイル（`#[cfg(test)]`）。数値処理は
  「小さい既知入力 → 期待出力」の表形式テストを基本とする。
- プロパティテスト（将来）: 任意フレーム列に対し「出力フレーム数は目標レートから
  ±1フレーム以内」「出力は単調な時刻列」等の不変条件を検証する。
- CI（`.github/workflows/ci.yml`）: Windows / Linux の両方で
  `cargo fmt --check`、`cargo clippy -- -D warnings`、`cargo test --workspace` を実行する。

## 10. 範囲外・将来拡張

- HDR、BT.709/2020（10bit は境界変換で対応済み。内部処理まで 16bit 化する
  ネイティブ対応は将来拡張）
- 高品質デインターレース（Yadif相当）、モーション補間（光フロー）
- 音声の同期・リサンプル（映像のみ扱う。音声は FFmpeg 側で遅延調整する前提）
- GUI プレビュー（トレイト経由の別クレートで対応可能な設計にはする）
- ハードウェア TBC との等価性保証（目標は「実用上十分」であって放送規格準拠ではない）
