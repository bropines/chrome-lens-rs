//! A small CLI over the library, mostly for eyeballing a response.
//!
//!     chrome_lens_ocr <image> [--lang <ocr-language>] [--to <target>] [--json]
//!
//! Set `RUST_LOG=debug` to see the request being made.

use std::process::ExitCode;

use chrome_lens_ocr::{LensClient, ScanOptions};

const USAGE: &str = "\
usage: chrome_lens_ocr <image> [options]

  --lang <code>   OCR language. Omitted means \"not stated\", which is what
                  Chromium sends when the user has chosen nothing - not \"en\".
  --to <code>     Translate into this language. Without it Lens returns no
                  translation at all, whatever else is set.
  --from <code>   Source language for the translation. Omitted means detect.
  --flatten       Flatten transparency onto white instead of picking white or
                  black by the content's luminance.
";

struct Args {
    image: String,
    options: ScanOptions,
}

fn parse_args() -> Result<Args, String> {
    let mut image: Option<String> = None;
    let mut options = ScanOptions::default();
    let mut argv = std::env::args().skip(1);

    while let Some(arg) = argv.next() {
        let mut value = |flag: &str| -> Result<String, String> {
            argv.next().ok_or_else(|| format!("{flag} needs a value"))
        };
        match arg.as_str() {
            "--lang" => options.ocr_language = Some(value("--lang")?),
            "--to" => options.target_language = Some(value("--to")?),
            "--from" => options.source_language = Some(value("--from")?),
            "--flatten" => {
                options.alpha_mode = chrome_lens_ocr::image_processor::AlphaMode::Flatten;
            }
            "-h" | "--help" => return Err(String::new()),
            other if other.starts_with('-') => return Err(format!("unknown option {other}")),
            other if image.is_none() => image = Some(other.to_owned()),
            other => return Err(format!("unexpected argument {other}")),
        }
    }

    Ok(Args {
        image: image.ok_or_else(|| "no image given".to_owned())?,
        options,
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    env_logger::init();

    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            if !message.is_empty() {
                eprintln!("chrome_lens_ocr: {message}\n");
            }
            eprint!("{USAGE}");
            // 64 is EX_USAGE: a wrong command line, not a failed scan.
            return ExitCode::from(64);
        }
    };

    let client = LensClient::new(None);
    let page = match client.scan_path(&args.image, &args.options).await {
        Ok(page) => page,
        Err(error) => {
            eprintln!("chrome_lens_ocr: {error:#}");
            return ExitCode::FAILURE;
        }
    };

    println!("{}", page.full_text);

    if let Some(language) = &page.language {
        eprintln!("\n[language: {language}]");
    }
    eprintln!(
        "[{} paragraph(s), uploaded at {}x{}]",
        page.paragraphs.len(),
        page.image_width,
        page.image_height
    );

    if page.translation.is_some() {
        println!("\n--- translation ---");
        for (index, paragraph) in page.paragraphs.iter().enumerate() {
            match &paragraph.translation {
                Some(text) => println!("{text}"),
                None => eprintln!("[paragraph {} came back untranslated]", index + 1),
            }
        }
    }

    ExitCode::SUCCESS
}
