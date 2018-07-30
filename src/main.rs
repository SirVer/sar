#![feature(rust_2018_preview)]
#![warn(rust_2018_idioms)]

use skim::{Skim, SkimOptions};
use std::default::Default;
use std::io::Cursor;
use failure::Error;
use std::fs;
use std::io::{self, BufRead};
use std::path::{PathBuf, Path};
use walkdir::WalkDir;
use std::fmt::{self, Display, Formatter};
use itertools::Itertools;
use std::process::Command;

// TODO(sirver): Use https://github.com/jrmuizel/pdf-extract for PDF -> Text extraction.

type Result<T> = ::std::result::Result<T, Error>;

trait Item: Display {
    fn handle_selection(&self);
}


#[derive(Debug)]
struct TextFileLineItem {
    path: PathBuf,
    line: String,
    line_index: usize,
}

impl Display for TextFileLineItem {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.path.display(), self.line_index + 1, self.line)
    }
}

impl Item for TextFileLineItem {
    fn handle_selection(&self) {
        let editor = default_editor::get().unwrap();
        let mut it = editor.split(" ");
        let cmd = it.next().unwrap();
        let mut args: Vec<String> = it.map(|s| s.to_string()).collect();
        args.push(self.path.to_str().unwrap().to_string());
        args.push(format!("+{}", self.line_index + 1));
        // TODO(sirver): This kinda hardcodes vim
        // We ignore errors from vim.
        let _ = Command::new(cmd).args(&args).spawn().unwrap().wait();
    }
}

fn report_txt_file(path: &Path) -> Result<Vec<Box<dyn Item>>> {
    let mut lines = Vec::new();
    let file = fs::File::open(path)?;
    for (line_index, line) in io::BufReader::new(file).lines().enumerate() {
        // The file might be binary, i.e. not UTF-8 parsable.
        if let Ok(line) = line {
            if line.trim().is_empty() {
                continue;
            }
            lines.push(Box::new(TextFileLineItem {
                path: path.to_path_buf(),
                line, line_index,
            }) as Box<dyn Item>);
        }
    }
    Ok(lines)
}

// fn report_any_file(path: &Path) -> Result<Vec<Box<dyn Item>>> {
    // println!("{}", path.display());
    // Ok(())
// }

fn handle_dir(path: impl AsRef<Path>) -> Result<Vec<Box<dyn Item>>> {
    let mut result = Vec::new();
    for entry in WalkDir::new(path.as_ref()) {
        if entry.is_err() {
            continue;
        }
        let entry = entry.unwrap();
        let mut new_results = match entry.path().extension().map(|s| s.to_str().unwrap()) {
            Some("md") | Some("txt") => report_txt_file(entry.path()),
            _ => continue,
            // _ => report_any_file(entry.path()),
        }?;
        result.append(&mut new_results);
    }
    Ok(result)
}

fn main() {
    let home = dirs::home_dir().expect("HOME not set.");
    let notes_dir = home.join("Dropbox/Tasks/notes");
    let mut results = Vec::new();
    results.append(&mut handle_dir(notes_dir).unwrap());

    let pdf_dir = home.join("Documents/Finanzen");
    results.append(&mut handle_dir(pdf_dir).unwrap());

    let options: SkimOptions<'_> = SkimOptions::default().height("50%").multi(false);

    let input = results.iter().map(|n| n.to_string()).join("\n");

    let selected_items = Skim::run_with(&options, Some(Box::new(Cursor::new(input))))
        .map(|out| out.selected_items)
        .unwrap_or_else(|| Vec::new());

    let first_selection = selected_items.first().unwrap().get_index();
    let selected_item = &results[first_selection];
    selected_item.handle_selection();


    // //==================================================
    // // second run
    // let input = "11111\n22222\n333333333".to_string();

    // let selected_items = Skim::run_with(&options, Some(Box::new(Cursor::new(input))))
        // .map(|out| out.selected_items)
        // .unwrap_or_else(|| Vec::new());

    // for item in selected_items.iter() {
        // print!("{}: {}{}", item.get_index(), item.get_output_text(), "\n");
    // }
}
