#![feature(rust_2018_preview)]
#![warn(rust_2018_idioms)]


use skim::{Skim, SkimOptions};
use std::default::Default;
use failure::Error;
use std::fs;
use std::io::{BufRead, Read, Seek, SeekFrom, BufReader, Cursor};
use std::path::{PathBuf, Path};
use walkdir::WalkDir;
use std::fmt::{self, Display, Formatter};
use std::process::Command;
use std::sync::mpsc;
use scoped_pool::Pool;
use structopt::StructOpt;
use std::collections::VecDeque;

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
    if let Some(pw) = password {
        // Enough space for "VimCrypt~".
        let mut buf = vec![0u8; 9];
        let mut file = fs::File::open(path)?;
        file.read(&mut buf)?;
        if buf == b"VimCrypt~" {
            file.seek(SeekFrom::Start(0))?;
            let mut file_contents = Vec::new();
            file.read_to_end(&mut file_contents)?;
            let content = vimdecrypt::decrypt(&file_contents, pw)?;
            report_txt_file_with_content(path, TextFileLineItemKind::VimEncrypted(pw.to_string()), BufReader::new(Cursor::new(content)), tx)?;
            return Ok(());
        }
    }
    report_txt_file_with_content(path, TextFileLineItemKind::Plain, BufReader::new(fs::File::open(path)?), tx)
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
    // TODO(sirver): Queing all data into memory is hardly a wise approach. Instead keep a Deque of
    // strings we need to feed in read.
    buffer: VecDeque<Vec<u8>>,
}

impl std::io::Read for SkimAdaptor {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.buffer.is_empty() {
            // TODO(sirver): Not very elegant.
            if let Ok(item) = self.rx.recv() {
                self.buffer.push_back(item.to_string().into_bytes());
                self.items_tx.send(item).unwrap();
            };
            while let Ok(item) = self.rx.try_recv() {
                self.buffer.push_back(item.to_string().into_bytes());
                self.items_tx.send(item).unwrap();
            };
        }
        if self.buffer.is_empty() {
            return Ok(0);
        }
        let mut item = self.buffer.pop_front().unwrap();
        let len = item.len();
        // TODO(sirver): This is not necessarily always true.
        assert!(len + 1 < buf.len());
        buf[0..len].clone_from_slice(&mut item);
        buf[len] = b'\n';
        println!("#sirver len: {:#?}", len);
        Ok(len + 1)
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

    let pool = Pool::new(10);
    pool.scoped(|scope| {
        let home = dirs::home_dir().expect("HOME not set.");
        let notes_dir = home.join("Dropbox/Tasks/notes");
        let tx_clone = tx.clone();
        let pass_ref = &pass;
        scope.execute(move || {
            handle_dir(notes_dir, pass_ref, tx_clone).unwrap();
        });

        let pdf_dir = home.join("Documents/Finanzen");
        let tx_clone = tx.clone();
        scope.execute(move || {
            handle_dir(pdf_dir, pass_ref, tx_clone).unwrap();
        });

        let secrets_dir = home.join("Documents/Secrets");
        let tx_clone = tx.clone();
        scope.execute(move || {
            handle_dir(secrets_dir, pass_ref, tx_clone).unwrap();
        });
        drop(tx);

        // NOCOM(#sirver): this is just for repros
        // scope.execute(move || {
            // handle_dir("/", pass_ref, tx).unwrap();
        // });

        // TODO(sirver): this feels weird. somehow this should be the main thread that continues.
        // Maybe we do not want a scoped pool, really, but just a regular thread pool.
        scope.execute(move || {
            let (items_tx, items_rx) = mpsc::channel();
            let adaptor = SkimAdaptor { rx, items_tx, buffer: VecDeque::new() };

            let options: SkimOptions<'_> = SkimOptions::default().multi(false)
                .expect("ctrl-e".to_string());

            // TODO(sirver): This should stream eventually.
            let skim_output = match Skim::run_with(&options, Some(Box::new(BufReader::new(adaptor)))) {
                None => return,
                Some(s) => s,
            };

            match skim_output.accept_key.as_ref().map(|s| s as &str) {
                // TODO(sirver): Implement creating a new note.
                Some("ctrl-e") => unimplemented!(),
                Some("") | None => {
                    let first_selection = skim_output.selected_items.first().unwrap().get_index();
                    let selected_item = items_rx.into_iter().nth(first_selection).unwrap();
                    if args.open {
                        selected_item.open().unwrap();
                    } else {
                        selected_item.cat().unwrap();
                    };
                },
                Some(unexpected_str) => {
                    // Skim should guarantee that this never happens.
                    unreachable!("Got unexpected: {:?}", unexpected_str);
                },
            };
        });
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_adaptor() {
        let (tx, rx) = mpsc::channel();
        let (items_tx, _items_rx) = mpsc::channel();

        let mut adaptor = SkimAdaptor { rx, items_tx, buffer: VecDeque::new() };

        tx.send(Box::new(TextFileLineItem { path: PathBuf::from("/tmp/blub.txt"), kind: TextFileLineItemKind::Plain, line: "foo bar".into(), line_index: 10 }) as Box<dyn Item>).unwrap();

        let mut buf = vec![0u8; 256];
        assert_eq!(25, adaptor.read(&mut buf).unwrap());
        assert_eq!(&buf[..25], b"/tmp/blub.txt:11:foo bar\n");

        tx.send(Box::new(TextFileLineItem { path: PathBuf::from("/tmp/blub1.txt"), kind: TextFileLineItemKind::Plain, line: "foo bar blub".into(), line_index: 10 }) as Box<dyn Item>).unwrap();
        drop(tx);

        assert_eq!(31, adaptor.read(&mut buf).unwrap());
        assert_eq!(&buf[..31], b"/tmp/blub1.txt:11:foo bar blub\n");

        assert_eq!(0, adaptor.read(&mut buf).unwrap());
        assert_eq!(0, adaptor.read(&mut buf).unwrap());
        assert_eq!(0, adaptor.read(&mut buf).unwrap());
        assert_eq!(0, adaptor.read(&mut buf).unwrap());
        assert_eq!(0, adaptor.read(&mut buf).unwrap());
    }
}

