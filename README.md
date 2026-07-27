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
  --layer-count 12
```

- 入力に複数種類のプロダクトが含まれる場合は、`--product` で変換対象を明示的に指定します。指定しなかった場合は、選択可能なプロダクト一覧を含むエラーが表示されます。
- `--layer-name-pattern` オプションでレイヤー名のパターンを指定できます。現在のところ、0 から始まる連番である `{seq}` のみ使えます。
- `--layer-count` でレイヤー数の上限を指定できます。時系列順にソートしたあと、この数をレイヤー数の上限とします。
 
この例では `rain250m_0` から `rain250m_11` までのソースレイヤーが作成されます。

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
