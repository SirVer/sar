#![feature(rust_2018_preview)]
#![warn(rust_2018_idioms)]


use skim::{Skim, SkimOptions};
use std::default::Default;
use failure::Error;
use std::fs;
use std::io::{self, BufRead, Read, Seek, SeekFrom, BufReader, Cursor};
use std::path::{PathBuf, Path};
use walkdir::WalkDir;
use std::fmt::{self, Display, Formatter};
use itertools::Itertools;
use std::process::Command;
use std::sync::mpsc;
use structopt::StructOpt;

// TODO(sirver): Use https://github.com/jrmuizel/pdf-extract for PDF -> Text extraction.

/// SirVer's archiver. Information retriever and writer.
#[derive(StructOpt, Debug)]
#[structopt(name = "sar")]
struct CommandLineArguments {
    /// Open the selected file. Default is to just dump it.
    #[structopt(short = "o", long = "open")]
    open: bool,

    /// Also look at vim-encrypted files.
    #[structopt(short = "e", long = "encrypted")]
    encrypted: bool,
}

type Result<T> = ::std::result::Result<T, Error>;

trait Item: Display + Send + Sync {
    /// Open the given Item for editing.
    fn open(&self) -> Result<()>;

    /// Display the given Items content.
    fn cat(&self) -> Result<()>;
}

#[derive(Debug,Clone)]
enum TextFileLineItemKind { Plain, VimEncrypted(String) }

#[derive(Debug)]
struct TextFileLineItem {
    path: PathBuf,
    line: String,
    line_index: usize,
    kind: TextFileLineItemKind,
}

impl Display for TextFileLineItem {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.path.display(), self.line_index + 1, self.line)
    }
}

fn call_editor(path: &Path, line_index: usize) -> Result<()> {
    let editor = default_editor::get()?;
    let mut it = editor.split(" ");
    let cmd = it.next().unwrap();
    let mut args: Vec<String> = it.map(|s| s.to_string()).collect();
    args.push(path.to_str().unwrap().to_string());
    args.push(format!("+{}", line_index));
    // TODO(sirver): This kinda hardcodes vim
    // We ignore errors from vim.
    let _ = Command::new(cmd).args(&args).spawn()?.wait();
    Ok(())
}

impl Item for TextFileLineItem {
    fn open(&self) -> Result<()> {
        call_editor(&self.path, self.line_index + 1)
    }

    fn cat(&self) -> Result<()> {
        let output = match self.kind {
            TextFileLineItemKind::Plain => {
                std::fs::read_to_string(&self.path)?
            },
            TextFileLineItemKind::VimEncrypted(ref password) => {
                let output = std::fs::read(&self.path)?;
                let content = vimdecrypt::decrypt(&output, &password)?;
                String::from_utf8(content)?
            },
        };
        println!("{}", output);
        Ok(())
    }
}

fn report_txt_file_with_content(path: &Path, kind: TextFileLineItemKind, content: impl BufRead, tx: &mut mpsc::Sender<Box<dyn Item>>) -> Result<()> {
    for (line_index, line) in content.lines().enumerate() {
        // The file might be binary, i.e. not UTF-8 parsable.
        if let Ok(line) = line {
            if line.trim().is_empty() {
                continue;
            }
            tx.send(Box::new(TextFileLineItem {
                path: path.to_path_buf(),
                kind: kind.clone(),
                line, line_index,
            }) as Box<dyn Item>)?;
        }
    }
    Ok(())
}

fn report_txt_file(path: &Path, password: &Option<String>, tx: &mut mpsc::Sender<Box<dyn Item>>) -> Result<()> {
    match password {
        None => report_txt_file_with_content(path, TextFileLineItemKind::Plain, BufReader::new(fs::File::open(path)?), tx),
        Some(pw) => {
            // Enough space for "VimCrypt~".
            let mut buf = vec![0u8; 9];
            let mut file = fs::File::open(path)?;
            file.read(&mut buf)?;
            if buf == b"VimCrypt~" {
                file.seek(SeekFrom::Start(0))?;
                // NOCOM(#sirver): contents need to be dumped correctly.
                let mut file_contents = Vec::new();
                file.read_to_end(&mut file_contents)?;
                let content = vimdecrypt::decrypt(&file_contents, pw)?;
                report_txt_file_with_content(path, TextFileLineItemKind::VimEncrypted(pw.to_string()), BufReader::new(Cursor::new(content)), tx)?;
            }
            Ok(())
        }
    }
}

fn handle_dir(path: impl AsRef<Path>, password: &Option<String>, mut tx: mpsc::Sender<Box<dyn Item>>) -> Result<()> {
    for entry in WalkDir::new(path.as_ref()) {
        if entry.is_err() {
            continue;
        }
        let entry = entry.unwrap();
        match entry.path().extension().map(|s| s.to_str().unwrap()) {
            Some("md") | Some("txt") => report_txt_file(entry.path(), &password, &mut tx)?,
            _ => continue,
            // _ => report_any_file(entry.path()),
        };
    }
    Ok(())
}

#[derive(Debug)]
struct SkimAdaptor {
    rx: mpsc::Receiver<Box<dyn Item>>,
    items_tx: mpsc::Sender<Box<dyn Item>>,
    buffer: Vec<u8>,
    nread: usize,
}

impl SkimAdaptor {
    fn crank(&mut self) {
        while let Ok(item) = self.rx.try_recv() {
            self.buffer.append(&mut item.to_string().into_bytes());
            self.buffer.extend(b"\n");
            self.items_tx.send(item).unwrap();
        };
    }
}

impl std::io::Read for SkimAdaptor {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.crank();
        let num_bytes = buf.len().min(self.buffer.len() - self.nread);
        buf.clone_from_slice(&self.buffer[self.nread..self.nread + num_bytes]);
        Ok(num_bytes)
    }
}

impl BufRead for SkimAdaptor {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.crank();
        Ok(&self.buffer[self.nread..])
    }

    fn consume(&mut self, size: usize) {
        self.nread += size;
    }
}

fn main() -> Result<()> {
    let args = CommandLineArguments::from_args();

    let pass = if args.encrypted {
        Some(rpassword::prompt_password_stdout("Password: ").unwrap())
    } else {
        None
    };

    let (tx, rx) = mpsc::channel();

    let home = dirs::home_dir().expect("HOME not set.");
    let notes_dir = home.join("Dropbox/Tasks/notes");
    handle_dir(notes_dir, &pass, tx.clone()).unwrap();

    let pdf_dir = home.join("Documents/Finanzen");
    handle_dir(pdf_dir, &pass, tx.clone()).unwrap();

    let secrets_dir = home.join("Documents/Secrets");
    handle_dir(secrets_dir, &pass, tx).unwrap();

    let (items_tx, items_rx) = mpsc::channel();
    let adaptor = SkimAdaptor { rx, items_tx, buffer: Vec::new(), nread: 0 };


    let options: SkimOptions<'_> = SkimOptions::default().multi(false)
        .expect("ctrl-e".to_string());

    // TODO(sirver): This should stream eventually.
    let skim_output = match Skim::run_with(&options, Some(Box::new(adaptor))) {
        None => return Ok(()),
        Some(s) => s,
    };

    match skim_output.accept_key.as_ref().map(|s| s as &str) {
        // TODO(sirver): Implement creating a new note.
        Some("ctrl-e") => unimplemented!(),
        Some("") | None => {
            let first_selection = skim_output.selected_items.first().unwrap().get_index();
            let selected_item = items_rx.into_iter().nth(first_selection).unwrap();
            if args.open {
                selected_item.open()?;
            } else {
                selected_item.cat()?;
            };
        },
        Some(unexpected_str) => {
            // Skim should guarantee that this never happens.
            unreachable!("Got unexpected: {:?}", unexpected_str);
        },
    };
    Ok(())
}
