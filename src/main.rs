use std::{
    collections::HashMap,
    fmt::Display,
    fs::File,
    io::{stdout, BufWriter, Write},
    path::PathBuf,
    str::FromStr,
};

use clap::Parser;
use coitrees::{COITree, Interval, IntervalTree};
use itertools::Itertools;
use log::LevelFilter;
use simple_logger::SimpleLogger;

type RegionIntervals = HashMap<String, Vec<Interval<Option<String>>>>;

struct RegionIntervalTrees(HashMap<String, COITree<Option<String>, usize>>);

impl Display for RegionIntervalTrees {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (region, itvs) in self.0.iter() {
            writeln!(
                f,
                "{region}: [{}]",
                itvs.iter()
                    .map(|itv| format!(
                        "({}, {}, {:?})",
                        itv.first,
                        itv.last,
                        itv.metadata.as_ref()
                    ))
                    .join(",")
            )?;
        }
        Ok(())
    }
}

mod cli;
mod concensus;
mod io;

use cli::Args;

fn main() -> eyre::Result<()> {
    let args = Args::parse();
    let log_level = LevelFilter::from_str(&args.log_level)?;
    SimpleLogger::new().with_level(log_level).init()?;

    let paf_records = io::read_paf(args.paf)?;
    let ref_roi_records = io::read_bed(args.ref_roi_bed, |_| None)?;
    let ref_misasm_records = io::read_bed(Some(args.ref_misasm_bed), |rec| Some(rec.to_owned()))?;
    let qry_misasm_records = io::read_bed(Some(args.qry_misasm_bed), |rec| Some(rec.to_owned()))?;

    let mut new_ctgs = concensus::get_concensus(
        &paf_records,
        ref_roi_records,
        ref_misasm_records,
        qry_misasm_records,
    )?;

    let (ref_fai, ref_fa_gzi) = io::get_faidx(&args.ref_fa)?;
    let (qry_fai, qry_fa_gzi) = io::get_faidx(&args.query_fa)?;

    let output_fa: Box<dyn Write> =
        if let Some(outfile) = args.output_fa.filter(|fpath| *fpath != PathBuf::from("-")) {
            Box::new(BufWriter::new(File::create(outfile)?))
        } else {
            Box::new(BufWriter::new(stdout().lock()))
        };

    let output_bed = if let Some(output_bed) = args.output_bed {
        Some(BufWriter::new(File::create(output_bed)?))
    } else {
        None
    };

    io::update_contig_boundaries(&mut new_ctgs, &ref_fai, &qry_fai)?;
    io::write_consensus_fa(
        new_ctgs,
        &args.ref_fa,
        &ref_fai,
        ref_fa_gzi.as_ref(),
        &args.query_fa,
        &qry_fai,
        qry_fa_gzi.as_ref(),
        output_fa,
        output_bed,
    )?;

    Ok(())
}
