//! The PDF step of `ir.actions.report`, port of what Odoo does after
//! QWeb has run (`odoo/addons/base/models/ir_actions_report.py`).
//!
//! Odoo renders the report to HTML and hands that HTML to an external
//! binary. This does the same, and the reason is the same: an HTML
//! renderer good enough for a printed invoice is a browser engine, and
//! nothing in the Rust ecosystem is one. Writing a worse one here would
//! mean every report template in every addon had to be rewritten against
//! whatever subset it supported — which is the opposite of a port.
//!
//! So the conversion lives behind [`PdfRenderer`]. What produces the
//! HTML never learns which binary is on the other side, and the day a
//! Rust engine is worth using it is one more implementation rather than
//! a change to the report path.

use rusdoo_core::RusdooError;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a converter gets before it is killed.
///
/// A report is a page somebody is waiting on. A converter that has not
/// answered in this long is not slow, it is stuck — and without a limit
/// it holds a blocking thread for the life of the process.
const CONVERSION_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the wait checks whether the converter is done. Short enough
/// that a fast report is not padded, long enough not to spin.
const POLL: Duration = Duration::from_millis(25);

/// Turning a rendered report into the file somebody prints.
pub trait PdfRenderer: Send + Sync {
    fn render(&self, html: &str) -> Result<Vec<u8>, RusdooError>;

    /// Which converter this is, for the boot log and for the message a
    /// failed conversion shows.
    fn name(&self) -> &str;
}

/// The converters this knows how to drive, in the order they are looked
/// for.
///
/// Not alphabetical and not arbitrary. `wkhtmltopdf` is what Odoo
/// documents, and it is a WebKit fork frozen around 2012 with no
/// maintainer: it gets grid and flexbox wrong, which is most of how a
/// modern template is laid out. It stays in the list because an existing
/// deployment has it; it is not what a new one should get.
const ENGINES: [Engine; 5] = [
    Engine {
        binary: "weasyprint",
        argv: Argv::Positional(&[]),
    },
    Engine {
        binary: "wkhtmltopdf",
        argv: Argv::Positional(&["--quiet"]),
    },
    Engine {
        binary: "chromium",
        argv: Argv::ChromeHeadless,
    },
    Engine {
        binary: "chromium-browser",
        argv: Argv::ChromeHeadless,
    },
    Engine {
        binary: "google-chrome",
        argv: Argv::ChromeHeadless,
    },
];

#[derive(Debug, Clone, Copy)]
struct Engine {
    binary: &'static str,
    argv: Argv,
}

/// How a converter wants to be told about its two files.
#[derive(Debug, Clone, Copy)]
enum Argv {
    /// `<binary> [flags] <input.html> <output.pdf>`
    Positional(&'static [&'static str]),
    /// Chrome's own spelling, which takes the input as a URL
    ChromeHeadless,
}

impl Engine {
    /// The command line for one conversion.
    ///
    /// Built as a list, never as a string: the report's content reaches
    /// the converter through a file, and no part of this is ever handed
    /// to a shell to re-parse.
    fn command(&self, input: &Path, output: &Path) -> Vec<String> {
        match self.argv {
            Argv::Positional(flags) => {
                let mut argv: Vec<String> = flags.iter().map(|f| (*f).to_string()).collect();
                argv.push(input.display().to_string());
                argv.push(output.display().to_string());
                argv
            }
            Argv::ChromeHeadless => vec![
                "--headless".into(),
                "--disable-gpu".into(),
                "--no-sandbox".into(),
                "--no-pdf-header-footer".into(),
                format!("--print-to-pdf={}", output.display()),
                format!("file://{}", input.display()),
            ],
        }
    }
}

/// A converter found on the machine the server runs on.
pub struct ExternalPdf {
    engine: Engine,
    /// the resolved path, so a `PATH` that changes under a running
    /// server cannot make the converter disappear halfway through a day
    binary: PathBuf,
}

impl ExternalPdf {
    /// The converter this machine has, or `None`.
    ///
    /// `RUSDOO_PDF_BIN` names one explicitly and wins over the search,
    /// which is what a container does: the image knows what it installed
    /// and should not be guessing at boot.
    pub fn discover() -> Option<ExternalPdf> {
        if let Ok(named) = std::env::var("RUSDOO_PDF_BIN") {
            let named = named.trim();
            if !named.is_empty() {
                return Self::named(named);
            }
        }
        ENGINES.iter().find_map(|engine| {
            on_path(engine.binary).map(|binary| ExternalPdf {
                engine: *engine,
                binary,
            })
        })
    }

    /// A converter the operator named, by binary name or by full path.
    ///
    /// The argv still comes from the table: what `RUSDOO_PDF_BIN` says is
    /// *where* the converter is, not how it is called, and a name this
    /// does not recognise is refused rather than driven with somebody
    /// else's flags.
    fn named(named: &str) -> Option<ExternalPdf> {
        let path = Path::new(named);
        let stem = path.file_name()?.to_str()?;
        let engine = *ENGINES.iter().find(|engine| engine.binary == stem)?;
        let binary = if path.is_absolute() {
            path.is_file().then(|| path.to_path_buf())?
        } else {
            on_path(stem)?
        };
        Some(ExternalPdf { engine, binary })
    }
}

impl PdfRenderer for ExternalPdf {
    fn name(&self) -> &str {
        self.engine.binary
    }

    fn render(&self, html: &str) -> Result<Vec<u8>, RusdooError> {
        // both files, and both removed however this ends: a report that
        // failed must not leave the document on disk for whoever reads
        // the temp directory next
        let job = Scratch::new(self.engine.binary)?;
        std::fs::write(&job.input, html)
            .map_err(|error| convert_err(self.engine.binary, format!("writing the source: {error}")))?;

        let mut child = std::process::Command::new(&self.binary)
            .args(self.engine.command(&job.input, &job.output))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|error| convert_err(self.engine.binary, format!("could not start: {error}")))?;

        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if started.elapsed() >= CONVERSION_TIMEOUT {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(convert_err(
                            self.engine.binary,
                            format!("no answer in {}s", CONVERSION_TIMEOUT.as_secs()),
                        ));
                    }
                    std::thread::sleep(POLL);
                }
                Err(error) => {
                    return Err(convert_err(self.engine.binary, format!("lost it: {error}")))
                }
            }
        };
        if !status.success() {
            return Err(convert_err(
                self.engine.binary,
                format!("exited {status}"),
            ));
        }
        let pdf = std::fs::read(&job.output).map_err(|error| {
            convert_err(self.engine.binary, format!("wrote no document: {error}"))
        })?;
        // a converter that exits 0 having written nothing usable is a
        // converter that failed; saying so beats serving a file the
        // reader refuses without explaining why
        if !pdf.starts_with(b"%PDF") {
            return Err(convert_err(
                self.engine.binary,
                "what it wrote is not a PDF".to_string(),
            ));
        }
        Ok(pdf)
    }
}

/// The two temp files of one conversion, removed when it ends.
struct Scratch {
    input: PathBuf,
    output: PathBuf,
}

impl Scratch {
    fn new(engine: &str) -> Result<Scratch, RusdooError> {
        // a directory of its own, so two reports converting at once
        // cannot be handed each other's files
        let dir = std::env::temp_dir().join(format!("rusdoo-report-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)
            .map_err(|error| convert_err(engine, format!("no scratch directory: {error}")))?;
        Ok(Scratch {
            input: dir.join("report.html"),
            output: dir.join("report.pdf"),
        })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some(dir) = self.input.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn convert_err(engine: &str, detail: String) -> RusdooError {
    RusdooError::Validation(format!("{engine}: {detail}"))
}

/// Where `name` is on `PATH`, if it is there and executable.
fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_engine_is_told_about_both_files() {
        let input = Path::new("/tmp/x/report.html");
        let output = Path::new("/tmp/x/report.pdf");
        for engine in ENGINES {
            let argv = engine.command(input, output);
            let line = argv.join(" ");
            assert!(
                line.contains("report.html"),
                "{} was not told its source: {line}",
                engine.binary
            );
            assert!(
                line.contains("report.pdf"),
                "{} was not told where to write: {line}",
                engine.binary
            );
            // the report's own content never becomes part of a command
            // line, so nothing here needs quoting and nothing here may
            // be re-parsed by a shell
            assert!(
                !line.contains("&&") && !line.contains(';'),
                "{} builds something shell-shaped: {line}",
                engine.binary
            );
        }
    }

    #[test]
    fn a_named_binary_this_does_not_know_is_refused() {
        // driving an unknown converter with wkhtmltopdf's flags would
        // either fail obscurely or, worse, half-work
        assert!(ExternalPdf::named("definitely-not-a-converter").is_none());
        assert!(ExternalPdf::named("/usr/bin/definitely-not-a-converter").is_none());
    }

    /// The real thing, when the machine has one. Self-skipping like the
    /// database tests: a CI image without a converter is not a failure.
    #[test]
    fn a_real_converter_turns_html_into_a_pdf() {
        let Some(renderer) = ExternalPdf::discover() else {
            eprintln!("skipped: no PDF converter on PATH");
            return;
        };
        let pdf = renderer
            .render("<html><body><h1>Invoice INV/0001</h1></body></html>")
            .unwrap_or_else(|error| panic!("{} could not convert: {error}", renderer.name()));
        assert!(pdf.starts_with(b"%PDF"), "not a PDF: {:?}", &pdf[..8.min(pdf.len())]);
        assert!(pdf.len() > 400, "suspiciously small: {} bytes", pdf.len());
    }
}
