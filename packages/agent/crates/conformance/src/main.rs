//! `ankor-conformance`: runs the scenarios against the agent and compares each transcript with its
//! golden one. `ankor-conformance --help` lists the options.

use std::path::PathBuf;
use std::process::ExitCode;

use ankor_conformance::golden::{self, Verdict};
use ankor_conformance::runner::{self, BUILT_AGENT_IMAGE, BUILT_CS2_IMAGE, Suite};
use ankor_conformance::scenarios;

const HELP: &str = "ankor-conformance [--scenario NAME]... [--update] [--out DIR] [--golden DIR] [--keep] [--list]

Runs each scenario against the agent and compares its normalised transcript with the golden one.

  --scenario NAME   run only this scenario (repeatable); all of them by default
  --update          write each transcript as the golden one instead of comparing
  --out DIR         where transcripts, diffs and the agent's log go (default target/conformance)
  --golden DIR      where the golden transcripts are (default the crate's golden/)
  --keep            leave the sandbox container running afterwards
  --list            list the scenarios and exit

Images, from the workstation's Docker daemon, which scripts/conformance.sh builds:";

struct Options {
    names: Vec<String>,
    out: PathBuf,
    golden: PathBuf,
    update: bool,
    keep: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let mut options = Options { names: Vec::new(), out: PathBuf::from("target/conformance"), golden: golden::default_dir(), update: false, keep: false };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        // A flag that takes a value and has none is refused: ignored, `--update --scenario` would
        // rewrite every golden.
        let value_of = |value: Option<String>| {
            if value.is_none() {
                eprintln!("error: {arg} needs a value");
            }
            value
        };

        match arg.as_str() {
            "--scenario" => match value_of(args.next()) {
                Some(name) => options.names.push(name),
                None => return ExitCode::FAILURE,
            },
            "--update" => options.update = true,
            "--out" => match value_of(args.next()) {
                Some(out) => options.out = PathBuf::from(out),
                None => return ExitCode::FAILURE,
            },
            "--golden" => match value_of(args.next()) {
                Some(golden) => options.golden = PathBuf::from(golden),
                None => return ExitCode::FAILURE,
            },
            "--keep" => options.keep = true,
            "--list" => {
                for scenario in scenarios::ALL {
                    println!("{:<14} {}", scenario.name, scenario.about);
                }
                return ExitCode::SUCCESS;
            }
            _ => {
                println!("{HELP}");
                println!("  {BUILT_AGENT_IMAGE}");
                println!("  {BUILT_CS2_IMAGE}");
                return if arg == "--help" { ExitCode::SUCCESS } else { ExitCode::FAILURE };
            }
        }
    }
    match run(&options).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(options: &Options) -> anyhow::Result<bool> {
    let selected = runner::select(&options.names)?;
    let images = runner::images();
    println!("starting the sandbox");
    let mut suite = Suite::start(&images, &options.out, &options.golden, options.update).await?;
    let mut all_passed = true;
    let mut summary = Vec::new();
    for scenario in selected {
        println!("--- {}: {}", scenario.name, scenario.about);
        let outcome = suite.run(scenario).await?;
        if let Some(error) = &outcome.error {
            println!("{error}");
        }
        let verdict = match &outcome.verdict {
            Some(Verdict::Match) => "match",
            Some(Verdict::Updated) => "updated",
            Some(Verdict::Missing) => "MISSING GOLDEN",
            Some(Verdict::Differs(report)) => {
                println!("{report}");
                "DIFFER"
            }
            None => "ERROR",
        };
        println!("{}: {verdict}", scenario.name);
        all_passed &= outcome.passed();
        summary.push((scenario.name, verdict));
    }
    if !options.keep {
        suite.finish().await;
    }
    println!("\nsummary (transcripts in {}):", options.out.display());
    for (name, verdict) in summary {
        println!("  {name:<14} {verdict}");
    }
    Ok(all_passed)
}
