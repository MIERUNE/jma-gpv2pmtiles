use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use grib2pmtiles::{ConvertOptions, convert};

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    /// Input GRIB2 file. Gzip-compressed input is detected from the .gz suffix.
    input: PathBuf,

    /// Output PMTiles archive.
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

    /// Minimum output zoom.
    #[arg(long, default_value_t = 0)]
    min_zoom: u8,

    /// Maximum output zoom. By default it is derived from the source grid.
    #[arg(long)]
    max_zoom: Option<u8>,

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
            quantize: cli.quantize,
        },
    )
}
