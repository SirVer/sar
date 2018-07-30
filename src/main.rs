extern crate failure;
extern crate walkdir;
extern crate skim;

use skim::{Skim, SkimOptions};
use std::default::Default;
use std::io::Cursor;
use failure::Error;
use std::env;
use std::fs;
use std::io::{self, BufRead};
use std::path;
use walkdir::WalkDir;

// TODO(sirver): Use https://github.com/jrmuizel/pdf-extract for PDF -> Text extraction.

type Result<T> = ::std::result::Result<T, Error>;

fn report_txt_file(path: &path::Path) -> Result<()> {
    let file = fs::File::open(path)?;
    for (idx, line) in io::BufReader::new(file).lines().enumerate() {
        // The file might be binary, i.e. not UTF-8 parsable.
        if let Ok(line) = line {
            if line.trim().is_empty() {
                continue;
            }
            println!("{}:{}:{}", path.display(), idx + 1, line);
        }
    }
    Ok(())
}

fn report_any_file(path: &path::Path) -> Result<()> {
    println!("{}", path.display());
    Ok(())
}

fn handle_dir(path: impl AsRef<path::Path>) -> Result<()> {
    for entry in WalkDir::new(path.as_ref()) {
        if entry.is_err() {
            continue;
        }
        let entry = entry.unwrap();
        match entry.path().extension().map(|s| s.to_str().unwrap()) {
            Some("md") | Some("txt") => report_txt_file(entry.path()),
            _ => report_any_file(entry.path()),
        }?;
    }
    Ok(())
}

fn main() {
    let home = env::home_dir().expect("HOME not set.");
    let notes_dir = home.join("Dropbox/Tasks/notes");
    handle_dir(notes_dir).unwrap();

    let pdf_dir = home.join("Documents/Finanzen");
    handle_dir(pdf_dir).unwrap();

    let options: SkimOptions = SkimOptions::default().height("50%").multi(true);

    //==================================================
    // first run
    let input = "aaaaa\nbbbb\nccc".to_string();

    let selected_items = Skim::run_with(&options, Some(Box::new(Cursor::new(input))))
        .map(|out| out.selected_items)
        .unwrap_or_else(|| Vec::new());

    for item in selected_items.iter() {
        print!("{}: {}{}", item.get_index(), item.get_output_text(), "\n");
    }

    //==================================================
    // second run
    let input = "11111\n22222\n333333333".to_string();

    let selected_items = Skim::run_with(&options, Some(Box::new(Cursor::new(input))))
        .map(|out| out.selected_items)
        .unwrap_or_else(|| Vec::new());

    for item in selected_items.iter() {
        print!("{}: {}{}", item.get_index(), item.get_output_text(), "\n");
    }
}
