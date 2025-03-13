use std::path::PathBuf;

use clap::Parser;
use itertools::Itertools;
use packed_seq::{AsciiSeqVec, SeqVec};
use tracing::{info, trace};

#[derive(clap::Parser, Debug)]
struct Args {
    paths: Vec<PathBuf>,
    #[clap(long)]
    bin: bool,

    /// k-mer length
    #[clap(short, default_value_t = 31)]
    k: usize,

    /// Sketch size
    #[clap(short, default_value_t = 10000)]
    s: usize,
    /// Store bottom-b bits of each element. Must be multiple of 8.
    #[clap(short, default_value_t = 16)]
    b: usize,
}

fn main() {
    init_trace();

    let args = Args::parse();
    let paths = collect_paths(args.paths);
    let q = paths.len();

    let k = args.k;
    let s = args.s;
    let b = args.b;

    let masher = simd_mash::Masher::new_rc(k, s, b);

    let mut bottom_mashes = vec![];
    let mut bin_mashes = vec![];
    let start = std::time::Instant::now();

    for path in paths {
        trace!("Sketching {path:?}");
        let mut seq = AsciiSeqVec::default();
        let mut reader = needletail::parse_fastx_file(path).unwrap();
        let start = std::time::Instant::now();
        while let Some(r) = reader.next() {
            // let record = r
            //     .unwrap()
            //     .seq();
            // .iter()
            // .filter_map(|&b| if b == b'N' { None } else { Some(b) })
            // .collect::<Vec<_>>();
            // seq.push_ascii(&record);
            seq.push_ascii(&r.unwrap().seq());
            // FIXME: Skip adjacent k-mers.
        }
        trace!("Reading & filtering took {:?}", start.elapsed());
        let start = std::time::Instant::now();
        if args.bin {
            bin_mashes.push(masher.bin_mash(seq.as_slice()));
        } else {
            bottom_mashes.push(masher.bottom_mash(seq.as_slice()));
        };
        trace!("sketching itself took {:?}", start.elapsed());
    }
    let t = start.elapsed();
    info!("Sketching {q} seqs took {t:?} ({:?} avg)", t / q as u32);

    let start = std::time::Instant::now();
    let dists = if args.bin {
        bin_mashes
            .iter()
            .tuple_combinations()
            .map(|(s1, s2)| s1.similarity(s2))
            .collect_vec()
    } else {
        bottom_mashes
            .iter()
            .tuple_combinations()
            .map(|(s1, s2)| s1.similarity(s2))
            .collect_vec()
    };
    let t = start.elapsed();
    let cnt = q * (q - 1) / 2;
    info!(
        "Computing {cnt} dists took {t:?} ({:?} avg)",
        t / cnt.max(1) as u32
    );
    info!(
        "Params {:?}",
        Args {
            paths: vec![],
            ..args
        }
    );
    for dist in dists {
        println!("{dist}");
    }
}

fn init_trace() {
    use tracing::level_filters::LevelFilter;
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(LevelFilter::TRACE.into())
                .from_env_lossy(),
        )
        .init();
}

fn collect_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut res = vec![];
    for path in paths {
        if path.is_dir() {
            res.extend(path.read_dir().unwrap().map(|entry| entry.unwrap().path()));
        } else {
            res.push(path);
        }
    }
    res
}
