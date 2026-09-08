use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use grib2pmtiles::{ConvertOptions, convert};

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    /// Input GRIB2 file. Gzip-compressed input is detected from the .gz suffix.
    input: PathBuf,

    /// Output PMTiles archive. With --raster and several forecast times, this
    /// must contain {seq} or {valid_time}, such as wind_{valid_time}.pmtiles.
    output: PathBuf,

    /// Product to convert, such as hrnowc/intensity. Required when the input contains several products.
    #[arg(long)]
    product: Option<String>,

    /// MVT source-layer name pattern. Must contain the {seq} placeholder.
    #[arg(long, default_value = "layer_{seq}")]
    layer_name_pattern: String,

    /// Keep only the first N value products after sorting by valid time.
    #[arg(long)]
    layer_count: Option<usize>,

    /// First number substituted for {seq}, so the numbering can match the
    /// forecast hours of the input. For example --layer-seq-start 1 turns an
    /// FH01-06 input into layers 1 through 6.
    #[arg(long, default_value_t = 0)]
    layer_seq_start: usize,

    /// Minimum output zoom.
    #[arg(long, default_value_t = 0)]
    min_zoom: u8,

    /// Maximum output zoom. By default it is derived from the source grid.
    #[arg(long)]
    max_zoom: Option<u8>,

    /// Drop products that are analyses rather than forecasts. Nowcast inputs
    /// lead with the observed field, whose valid time is before the reference
    /// time, which otherwise becomes the first layer.
    #[arg(long)]
    skip_analysis: bool,

    /// Keep only products at least this many minutes ahead of the reference
    /// time. The first nowcast step is valid at the reference time itself, so
    /// --min-lead-time 5 starts the layers at the first real forecast.
    #[arg(long, value_name = "MINUTES")]
    min_lead_time: Option<i64>,

    /// Rename a band as <FROM>=<TO>, such as --rename value=DN. The name is
    /// used as the MVT attribute key and the metadata field name, and
    /// --quantize refers to the new name. Repeat for several bands.
    #[arg(long, value_name = "FROM=TO")]
    rename: Vec<String>,

    /// Quantize values into classes before building polygons, which merges
    /// neighbouring cells and shrinks the geometry.
    ///
    /// Boundaries are inclusive lower bounds in physical units, optionally
    /// followed by the value the class should emit: --quantize "0,1,2,4,8" or
    /// --quantize "0:0,1:0.5,2:1.5". The last class is open ended, and values
    /// below the first boundary join the first class. Prefix with a band name
    /// and repeat the option when the product has several bands, such as
    /// --quantize "u=-50,0,50".
    #[arg(long, value_name = "SPEC")]
    quantize: Vec<String>,

    /// Leave cells whose quantized class emits one of these values out of the
    /// tile entirely, geometry included. Requires --quantize. Prefix with a
    /// band name when the product has several bands, and repeat as needed.
    #[arg(long, value_name = "VALUES")]
    omit_class: Vec<String>,

    /// Before quantization, leave cells whose physical value is exactly zero
    /// out of the tile on every band. Ignored by bands which cannot represent
    /// zero.
    #[arg(long)]
    omit_zero: bool,

    /// Write RGBA raster tiles instead of MVT, encoding band values into the
    /// colour channels so that a shader can read them back.
    ///
    /// The parameters belong to the archive rather than to a tile, so that
    /// every tile decodes the same way and the field does not jump at a seam.
    /// The channels, offset, scale and mask needed to decode them are recorded
    /// as structured archive metadata.
    ///
    ///   vector-field:<component-limit>
    ///                               two bands (u, v) for a particle layer.
    ///                               Signed-normalised into red and green with
    ///                               the on-grid flag in blue. The limit is per
    ///                               component, so a field at the limit in both
    ///                               directions has a magnitude sqrt(2) times
    ///                               larger.
    ///   scalar16:<min>,<max>        one band across red and green, on-grid
    ///                               flag in blue. For a regional model, whose
    ///                               grid stops partway across a tile.
    ///   scalar24:<base>,<interval>  one band across all 24 colour bits. More
    ///                               precise, but no room is left for a mask.
    ///                               -10000,0.1 is Mapbox terrain-RGB exactly.
    ///
    /// One image cannot hold a sequence in a tile. When several forecast times
    /// are selected, the output path must contain {seq} or {valid_time}; one
    /// PMTiles archive is written for each time. {reference_time} is also
    /// replaced, and timestamps use UTC YYYYMMDDHHMMSS.
    #[arg(long, value_name = "SPEC", verbatim_doc_comment)]
    raster: Option<String>,

    /// Write an index of the archives --raster produced to this path.
    ///
    /// Lists each forecast time with the archive holding it, and the encoding
    /// they share, so that a client can blend between two times without having
    /// to discover the set first. The archive paths are relative to the
    /// manifest.
    #[arg(long, value_name = "PATH")]
    manifest: Option<PathBuf>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().compact().init();
    let cli = Cli::parse();
    convert(
        &cli.input,
        &cli.output,
        &ConvertOptions {
            product: cli.product,
            layer_name_pattern: cli.layer_name_pattern,
            layer_count: cli.layer_count,
            min_zoom: cli.min_zoom,
            max_zoom: cli.max_zoom,
            layer_seq_start: cli.layer_seq_start,
            rename: cli.rename,
            quantize: cli.quantize,
            omit_class: cli.omit_class,
            omit_zero: cli.omit_zero,
            raster: cli.raster,
            manifest: cli.manifest,
            skip_analysis: cli.skip_analysis,
            min_lead_time: cli.min_lead_time,
        },
    )
}
