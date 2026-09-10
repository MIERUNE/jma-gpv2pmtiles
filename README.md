# jma-gpv2pmtiles

気象庁のGPVデータ (GRIB2形式) を直接ベクタータイル (PMTilesアーカイブ) に変換するデモです。

`jma-gpv2pmtiles` directly converts GPV data provided by the Japan Meteorological Agency (JMA) in GRIB2 format into PMTiles archives.

LICENSE: [MIT](./LICENSE-MIT) or [Apache-2.0](./LICENSE-APACHE)

## 使い方

```bash
cargo run --release -- \
  input.grib2.bin.gz \
  output.pmtiles \
  --product hrnowc/intensity \
  --layer-name-pattern 'rain250m_{seq}' \
  --layer-count 12 \
  --quantize "0,1,5,10,20,30,50,80" \
  --omit-zero
```

- 入力に複数種類のプロダクトが含まれる場合は、`--product` で変換対象を明示的に指定します。指定しなかった場合は、選択可能なプロダクト一覧を含むエラーが表示されます。
- `--layer-name-pattern` オプションでレイヤー名のパターンを指定できます。現在のところ、0 から始まる連番である `{seq}` のみ使えます。
- `--layer-count` でレイヤー数の上限を指定できます。時系列順にソートしたあと、この数をレイヤー数の上限とします。
- `--quantize` は値を階級にまとめ、隣接セルを統合してサイズを大きく削減します。詳細は[後述](#値の量子化---quantize)。実数値が必要な場合は外してください。
 
この例では `rain250m_0` から `rain250m_11` までのソースレイヤーが作成されます。

## 値の量子化 (`--quantize`)

値を階級にまとめてから変換します。タイル容量の 97% 以上がジオメトリなので、**階級を粗くして隣接セルを統合する**のが最も効くサイズ削減です。

```bash
cargo run --release -- input.grib2.bin.gz output.pmtiles \
  --product hrnowc/intensity \
  --quantize "0,1,5,10,20,30,50,80" \
  --omit-zero
```

- 数値は**物理単位での階級の下限値**(境界値)です。`step` 式の stops と同じ数字を書けるので、スタイルとタイルで定義が二重管理になりません。
- **最後の階級は上に開いています**(`80` 以上はすべて `80`)。**最初の境界を下回る値は最初の階級に入ります**。
- `境界:代表値` の形式で、出力する値を個別に指定できます。階級の中央値を出したい場合など:
  ```bash
  --quantize "0:0,1:0.5,2:1.5,4:3"
  ```
- 複数バンドを持つプロダクトでは、バンド名を前置して必要な数だけ繰り返します:
  ```bash
  --quantize "u=-50,0,50" --quantize "v=-50,0,50"
  ```
- すべての代表値が整数なら、属性は MVT の `sint` として出力されます(小数を含む場合は `double`)。
- `--omit-zero` は量子化前の物理値が完全に `0` のセルを省略します。代表値が `0` でも、元の値が正なら省略されません。
- `--omit-class` は量子化後の代表値を指定して、その階級全体を省略します。たとえば `--quantize "0,1,5" --omit-class 0` は `1` 未満の階級を省略します。
- 境界・代表値・省略した入力値と階級は PMTiles のメタデータに記録されます。

### 例（降水強度の階級）

#### 気象庁ライク

```bash
--quantize "0,1,5,10,20,30,50,80" \
--omit-zero
```

#### Yahoo! JAPANライク

```bash
--rename value=DN \
--quantize "0:1,1:2,2:3,4:4,8:5,12:6,16:7,24:8,32:9,40:10,48:11,56:12,64:13,80:14" \
--omit-zero
```

## 対応している気象庁GPVプロダクト

`--product` には以下の値を指定できます。入力ファイルに含まれるプロダクトだけが選択候補として表示されます。

| 種別 | `--product` に指定する値 |
| --- | --- |
| 三十分大気解析GPV | `atm30min/temp`, `atm30min/wind` |
| 天気分布予報 | `tenkibunpu/maxtemp`, `tenkibunpu/mintemp`, `tenkibunpu/precip`, `tenkibunpu/snowfall`, `tenkibunpu/temp`, `tenkibunpu/weather` |
| 日本近海海流予報格子点資料 | `current/current` |
| 沿岸波浪モデル（CWM） | `cwm/swell1`, `cwm/swell2`, `cwm/wave`, `cwm/wind`, `cwm/windwave` |
| 土壌雨量指数 | `dojoshisu` |
| 全球数値予報モデル（GSM） | `gsm/altitude`, `gsm/cloud`, `gsm/humidity`, `gsm/precip`, `gsm/pressure`, `gsm/pressure-msl`, `gsm/radiation`, `gsm/temp`, `gsm/updraft`, `gsm/wind` |
| 全球波浪モデル（GWM） | `gwm/swell1`, `gwm/swell2`, `gwm/wave`, `gwm/wind`, `gwm/windwave` |
| 高解像度降水ナウキャスト | `hrnowc/intensity`, `hrnowc/intensity-error`, `hrnowc/precip`, `hrnowc/precip-error`, `hrnowc/echotops` |
| 高解像度雲情報 | `hrcloud/altitude`, `hrcloud/cloud`, `hrcloud/type`, `hrcloud/ice`, `hrcloud/qc` |
| 表面雨量指数 | `hyomenshisu` |
| 危険度分布（キキクル） | `kikikuru-dosha`, `kikikuru/flood`, `kikikuru/inundation`, `kikikuru/tougou` |
| 黄砂予測 | `kousa/column`, `kousa/low` |
| 局地数値予報モデル（LFM） | `lfm/altitude`, `lfm/cloud`, `lfm/humidity`, `lfm/precip`, `lfm/pressure`, `lfm/mslpressure`, `lfm/radiation`, `lfm/temp`, `lfm/updraft`, `lfm/wind` |
| メソアンサンブル予報システム（MEPS） | `meps/altitude`, `meps/humidity`, `meps/precip`, `meps/pressure`, `meps/mslpressure`, `meps/radiation`, `meps/temp`, `meps/updraft`, `meps/wind` |
| メソ数値予報モデル（MSM） | `msm/altitude`, `msm/cloud`, `msm/humidity`, `msm/precip`, `msm/pressure`, `msm/mslpressure`, `msm/radiation`, `msm/temp`, `msm/updraft`, `msm/wind` |
| 海氷 | `ocean-jp-ice/cover`, `ocean-jp-ice/drift`, `ocean-jp-ice/thickness` |
| 日本近海の海洋データ | `ocean-jp/current`, `ocean-jp/height`, `ocean-jp/salinity`, `ocean-jp/temp` |
| 北西太平洋の海洋データ | `ocean-np/current`, `ocean-np/height`, `ocean-np/salinity`, `ocean-np/temp` |
| 降水量 | `precipitation`, `precipitation-15h` |
| 積雪 | `snow/snowdepth`, `snow/snowfall` |
| 海面水温 | `sst/temp`, `sst-daily/temp`, `sst-himawari/temp` |
| 推計気象分布 | `suikei/sunshine`, `suikei/temp`, `suikei/weather` |
| 雷ナウキャスト | `thunder-nowc` |
| 潮汐・沿岸気象 | `tide/tide`, `tide/astronomical`, `tide-guidance/guidance`, `tide/pressure`, `tide/wind` |
| 竜巻発生確度ナウキャスト | `tornado-nowc` |
| 台風 | `typhoon-storm` |
| 紫外線 | `uv/uvi`, `uv/uvic`, `uv/ozone` |
| 波浪モデル（WEM） | `wem/wave` |

## ラスター出力 (Advanced)

`--raster` を指定すると、値を色チャンネルへ格納した PNG の PMTiles を出力します。複数の予報時刻を選択した場合は、出力パスのプレースホルダーを置換して1時刻につき1アーカイブを作成します。

```bash
cargo run --release -- input.grib2.bin.gz 'wind_{valid_time}.pmtiles' \
  --product msm/wind \
  --raster 'vector-field:100'
```

| プレースホルダー | 展開例 | 意味 |
| --- | --- | --- |
| `{seq}` | `0` | `--layer-seq-start` から始まる連番 |
| `{valid_time}` | `20191012090500` | 予報対象時刻（UTC、`YYYYMMDDHHMMSS`） |
| `{reference_time}` | `20191012090000` | 予報の基準時刻（同形式） |

プレースホルダーは併用できます。1時刻だけならプレースホルダーのない通常の出力パスも使用できます。複数時刻では `{seq}` または `{valid_time}` が必要で、展開後のパスが重複する場合もファイルを書き始める前にエラーになります。

| 指定 | バンド数 | 格納方法 |
| --- | ---: | --- |
| `vector-field:<component-limit>` | 2 | 符号付きベクトルを R、G、格子マスクを B に格納。上限は**成分ごと**なので、両成分が上限のとき合成速度はその √2 倍 |
| `scalar16:<min>,<max>` | 1 | スカラーを R、G、格子マスクを B に格納 |
| `scalar24:<base>,<interval>` | 1 | スカラーを24bitの R、G、B に格納。マスクなし |

### 復元方法

復元に必要な情報は `raster_encoding` メタデータに記録されるので、クライアント側に決め打ちの定数は不要です。係数は**テクセル空間**（サンプラーが返す `0..1`）で書かれているため、チャンネルが8bitであることを知る必要もありません。復元は2形式のどちらかです。

```glsl
// decode.kind = "linear-packed" — 複数チャンネルで1つの値（scalar16 / scalar24）
float value    = dot(texel.rgb, uWeights) + uOffset;

// decode.kind = "linear-channels" — 各チャンネルが独立した値（vector-field）
vec2  velocity = texel.rg * uScale + uOffset;

// mask.kind = "channel-gte" — 格子の内外。scalar24 は mask が null
float onGrid   = step(uThreshold, texel.b);
```

サンプル空間はトップレベルの `sample_space`（`normalized-texel`）に記録されます。`decode` は演算だけを持ち、格納方法は `container` に分かれているので、将来コンテナが変わっても `decode` と `mask` はそのまま使えます。

ほかに `schema_version`、`sampling`、`tile_size` / `buffer` / `image_size`、`bands`、`level`、ベクトルなら `components` と `units`、変換時に実測した `source_range`、および `product`、`sequence`、`reference_time`、`valid_time` が入ります。

### 時系列の生成と manifest

`--manifest` を付けると、生成した全アーカイブの索引を JSON で書き出します。二時刻を補間する風のアニメーションは、これだけで復号とUV計算を設定できます。

```bash
cargo run --release -- input.grib2.bin.gz 'wind_{valid_time}.pmtiles' \
  --product msm/wind --layer-count 12 \
  --raster 'vector-field:50' \
  --manifest wind.json
```

manifest は**自己完結**しています。`encoding` にアーカイブ側と同じ完全な descriptor（`schema_version` / `sampling` / `tile_size` / `buffer` / `image_size` / `bands` / `components` / `units` / `level` / `decode` / `mask`）が入り、`minzoom`・`maxzoom`・`bounds` も持つので、**どの PMTiles も開かずにクライアントを初期化できます**。

`source_range` だけは時刻ごとに変わりうるので、`times` の各要素に入ります。アーカイブの URL は manifest からの相対パスです。

### テクスチャとして読み込むときの条件

画像ではなく数値なので、画像パイプラインの便利機能はすべて破壊要因になります。**特に色空間変換は、後述の丸め誤差より桁違いに大きく値を壊します。**

- **色空間変換をしない** — display-P3 への変換や sRGB のトーンカーブが乗ると全チャンネルが書き換わります
- **premultiply をしない** — 全タイルを不透明にしてあるので事故は起きませんが、`createImageBitmap` には `premultiplyAlpha: "none"` を明示してください
- **UNORM として `0..1` で読む** — 係数がこの空間で書かれています
- **`highp float` を使う** — `mediump` は仮数10bitしか保証されず、packed の重みすら保持できません

`sampling.mipmaps: false` は、mipmap レベルを生成・保持しないことではなく、**値の復元時に mipmap レベルをサンプリングしないこと**を要求します。たとえば現行の MapLibre GL JS の通常の `raster` ソースはアップロード時に mipmap チェーンを生成し、この生成コストと VRAM 使用量を公開 API から無効化できません。カスタムレイヤーでは minification filter に mipmap を使わないことで復元値を守れますが、消費側の実装によってはこの契約が破られうる点に注意してください。

### 精度

テクセル空間へ畳み込んだ副作用として、無風が厳密なゼロにはなりません。`component_limit=100` を f32 で評価した実測で **約 7.6e-6 m/s**（FMA有無とも同程度）、1時間で 2.8 cm 相当です。1コードの刻み 0.79 m/s に対して5桁小さく、実用上は無視できます。

`scalar24` は24bitを使い切るため、シェーダーの f32 側が先に限界に達します。`interval=0.1` の実測で、terrain-RGB 相当の範囲なら誤差 **0.003** ですが、24bitを使い切ると **0.175**（約2 interval）まで開きます。エンコード自体は `interval` ぴったりですが、**復元は範囲次第**です。広い範囲と細かい刻みの両方が要る場合は、範囲を絞った `scalar16` のほうが向いています。
