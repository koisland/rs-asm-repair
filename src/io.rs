use core::str;
use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, BufWriter, Write},
    path::Path,
};

use coitrees::{COITree, Interval, IntervalTree};
use eyre::{Context, ContextCompat};
use itertools::Itertools;
use noodles::{
    bgzf::{self, IndexedReader},
    fasta,
};
use paf::{PafRecord, Reader};

use super::interval::{Contig, ContigType, RegionIntervalTrees, RegionIntervals};

pub fn read_bed(
    bed: Option<impl AsRef<Path>>,
    metadata_fn: impl Fn(&str) -> Option<String>,
) -> eyre::Result<RegionIntervalTrees> {
    let mut intervals: RegionIntervals = HashMap::new();
    let mut trees: RegionIntervalTrees = RegionIntervalTrees(HashMap::new());

    let Some(bed) = bed else {
        return Ok(trees);
    };
    let bed_fh = File::open(bed)?;
    let bed_reader = BufReader::new(bed_fh);

    for line in bed_reader.lines() {
        let line = line?;

        let bed3: Option<(&str, &str, &str)> = line.splitn(3, '\t').collect_tuple();
        let bedn: Option<(&str, &str, &str, &str)> = line.splitn(4, '\t').collect_tuple();

        let (name, start, stop, other_cols) = if let Some((name, start, stop, other_cols)) = bedn {
            (name, start, stop, other_cols)
        } else if let Some((name, start, stop)) = bed3 {
            (name, start, stop, "")
        } else {
            log::error!("Invalid line: {line}");
            continue;
        };
        let (first, last) = (start.parse::<i32>()?, stop.parse::<i32>()?);

        intervals
            .entry(name.to_owned())
            .and_modify(|intervals| {
                intervals.push(Interval {
                    first,
                    last,
                    metadata: metadata_fn(other_cols),
                })
            })
            .or_insert_with(|| {
                vec![Interval {
                    first,
                    last,
                    metadata: metadata_fn(other_cols),
                }]
            });
    }
    for (roi, intervals) in intervals.into_iter() {
        trees.0.entry(roi).or_insert(COITree::new(&intervals));
    }
    Ok(trees)
}

pub fn read_paf(paf: impl AsRef<Path>) -> eyre::Result<Vec<PafRecord>> {
    Ok(Reader::from_path(&paf)?
        .records()
        .flatten()
        .filter(|r| r.mapping_quality() == 60 && r.strand() == '+')
        .collect_vec())
}

pub enum FastaReader {
    Bgzip(fasta::io::Reader<IndexedReader<File>>),
    Standard(fasta::io::Reader<BufReader<File>>),
}

pub struct FastaReaderHandle {
    reader: FastaReader,
    fai: fasta::fai::Index,
}

impl FastaReaderHandle {
    pub fn new(infile: impl AsRef<Path>) -> eyre::Result<Self> {
        let (fai, gzi) = Self::get_faidx(&infile)?;
        let fh = Self::read_fa(&infile, gzi.as_ref())?;
        Ok(Self { reader: fh, fai })
    }

    fn get_faidx(
        fa: &impl AsRef<Path>,
    ) -> eyre::Result<(fasta::fai::Index, Option<bgzf::gzi::Index>)> {
        // https://www.ginkgobioworks.com/2023/03/17/even-more-rapid-retrieval-from-very-large-files-with-rust/
        let fa_path = fa.as_ref().canonicalize()?;
        let is_bgzipped = fa_path.extension().and_then(|e| e.to_str()) == Some("gz");
        let fai_fname = fa_path.with_extension(if is_bgzipped { "gz.fai" } else { "fa.fai" });
        let fai = fasta::fai::read(fai_fname);
        if is_bgzipped {
            let index_reader = bgzf::indexed_reader::Builder::default()
                .build_from_path(fa)
                .with_context(|| format!("Failed to read gzi for {fa_path:?}"))?;
            let gzi = index_reader.index().clone();

            if let Ok(fai) = fai {
                log::debug!("Existing fai index found for {fa_path:?}");
                return Ok((fai, Some(gzi)));
            }
            log::debug!("No existing faidx for {fa_path:?}. Generating...");
            let mut records = Vec::new();
            let mut indexer = fasta::io::Indexer::new(index_reader);
            while let Some(record) = indexer.index_record()? {
                records.push(record);
            }

            Ok((fasta::fai::Index::from(records), Some(gzi)))
        } else {
            if let Ok(fai) = fai {
                return Ok((fai, None));
            }
            log::debug!("No existing faidx for {fa_path:?}. Generating...");
            Ok((fasta::index(fa)?, None))
        }
    }

    pub fn fetch(&mut self, ctg_name: &str, start: u32, stop: u32) -> eyre::Result<fasta::Record> {
        let start_pos = noodles::core::Position::new(start.clamp(1, u32::MAX) as usize).unwrap();
        let stop_pos = noodles::core::Position::new(stop.clamp(1, u32::MAX) as usize).unwrap();
        let region = noodles::core::Region::new(ctg_name, start_pos..=stop_pos);
        match &mut self.reader {
            FastaReader::Bgzip(reader) => Ok(reader.query(&self.fai, &region)?),
            FastaReader::Standard(reader) => Ok(reader.query(&self.fai, &region)?),
        }
    }

    fn read_fa(
        fa: &impl AsRef<Path>,
        fa_gzi: Option<&bgzf::gzi::Index>,
    ) -> eyre::Result<FastaReader> {
        let fa_file = std::fs::File::open(fa);
        if let Some(fa_gzi) = fa_gzi {
            Ok(FastaReader::Bgzip(
                fa_file
                    .map(|file| bgzf::IndexedReader::new(file, fa_gzi.to_vec()))
                    .map(fasta::io::Reader::new)?,
            ))
        } else {
            Ok(FastaReader::Standard(
                fa_file
                    .map(std::io::BufReader::new)
                    .map(fasta::io::Reader::new)?,
            ))
        }
    }
}

pub fn update_contig_boundaries(
    ctgs: &mut HashMap<String, Vec<Contig>>,
    ref_fh: &FastaReaderHandle,
    qry_fh: &FastaReaderHandle,
) -> eyre::Result<()> {
    let ref_lengths: HashMap<&str, u64> = ref_fh
        .fai
        .as_ref()
        .iter()
        .flat_map(|rec| str::from_utf8(rec.name()).map(|name| (name, rec.length())))
        .collect();
    let qry_lengths: HashMap<&str, u64> = qry_fh
        .fai
        .as_ref()
        .iter()
        .flat_map(|rec| str::from_utf8(rec.name()).map(|name| (name, rec.length())))
        .collect();

    for (_tname, ctgs) in ctgs.iter_mut() {
        // Update boundary coordinates of first and last contigs.
        if let Some(ctg) = ctgs.get_mut(0) {
            ctg.start = 0;
        }
        let last_idx = ctgs.len().saturating_sub(1);
        if let Some(ctg) = ctgs.get_mut(last_idx) {
            let lengths = if ctg.category == ContigType::Target {
                &ref_lengths
            } else {
                &qry_lengths
            };
            let ctg_length = lengths.get(&ctg.name.as_ref()).with_context(|| {
                format!(
                    "Contig {} doesn't exist in {:?} fai.",
                    ctg.name, ctg.category
                )
            })?;

            ctg.stop = *ctg_length as u32
        }
    }

    Ok(())
}

pub fn write_consensus_fa(
    regions: HashMap<String, Vec<Contig>>,
    ref_fh: &mut FastaReaderHandle,
    qry_fh: &mut FastaReaderHandle,
    mut output_fa: Box<dyn Write>,
    mut output_bed: Option<BufWriter<File>>,
) -> eyre::Result<()> {
    for (name, ctgs) in regions {
        // Write header.
        writeln!(output_fa, ">{name}")?;

        for ctg in ctgs {
            let fa_fh = match ctg.category {
                ContigType::Target => &mut *ref_fh,
                ContigType::Query => &mut *qry_fh,
            };

            // Skip invalid coordinates.
            if ctg.start >= ctg.stop {
                continue;
            }

            let seq = fa_fh.fetch(&ctg.name, ctg.start, ctg.stop)?;

            output_fa.write_all(seq.sequence().as_ref())?;

            if let Some(bed_fh) = output_bed.as_mut() {
                writeln!(
                    bed_fh,
                    "{}\t{}\t{}\t{:?}\t{}",
                    ctg.name, ctg.start, ctg.stop, ctg.category, name
                )?;
            }
        }
        writeln!(output_fa)?;
    }
    Ok(())
}
