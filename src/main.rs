#![feature(rust_2018_preview)]
#![warn(rust_2018_idioms)]

use failure::Error;
use scoped_pool::{Pool, Scope};
use self_update::cargo_crate_version;
use serde_derive::Deserialize;
use skim::{Skim, SkimOptions};
use std::collections::VecDeque;
use std::default::Default;
use std::ffi::OsStr;
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::io::{BufRead, BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use structopt::StructOpt;
use walkdir::WalkDir;

// TODO(sirver): Use https://github.com/jrmuizel/pdf-extract for PDF -> Text extraction.

#[derive(Deserialize, Debug)]
struct ConfigurationFile {
    reading_directories: Vec<String>,
}

/// On MacOs calls 'open -R' on the path, which will reveal it in Finder. On other OSes, will
/// just call through to 'open_path' with the parent of the selected path.
#[cfg(target_os = "macos")]
fn show_path(path: &Path) -> Result<()> {
    let _ = Command::new("open")
        .args(&["-R", path.to_str().unwrap()])
        .spawn()?
        .wait();
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn show_path(path: &Path) -> Result<()> {
    open_path(&path.parent().unwrap())
}

fn open_path(path: &Path) -> Result<()> {
    // TODO(sirver): This is fairly specific.
    let _ = Command::new("open.py")
        .args(&[path.to_str().unwrap()])
        .spawn()?
        .wait();
    Ok(())
}

/// SirVer's archiver. Information retriever and writer.
#[derive(StructOpt, Debug)]
#[structopt(name = "sar")]
struct CommandLineArguments {
    /// Also look at vim-encrypted files.
    #[structopt(short = "e", long = "encrypted")]
    encrypted: bool,

    /// Update the binary from a new release on github and exit.
    #[structopt(long = "update")]
    update: bool,
}

type Result<T> = ::std::result::Result<T, Error>;

trait Item: Display + Send + Sync {
    /// The file of this item.
    fn path(&self) -> &Path;

    /// Open the given Item for editing.
    fn open(&self) -> Result<()>;

    /// Display the given Items content.
    fn cat(&self) -> Result<()>;
}

#[derive(Debug)]
struct AnyFileItem {
    path: PathBuf,
}

impl Display for AnyFileItem {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.path.display())
    }
}

impl Item for AnyFileItem {
    fn path(&self) -> &Path {
        &self.path
    }
    fn open(&self) -> Result<()> {
        println!("{}", self.path.to_str().unwrap());
        Ok(())
    }
    fn cat(&self) -> Result<()> {
        open_path(&self.path)?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
enum TextFileLineItemKind {
    Plain,
    VimEncrypted(String),
}

#[derive(Debug)]
struct TextFileLineItem {
    path: PathBuf,
    line: String,
    line_index: usize,
    kind: TextFileLineItemKind,
}

impl Display for TextFileLineItem {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}",
            self.path.display(),
            self.line_index + 1,
            self.line
        )
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
    fn path(&self) -> &Path {
        &self.path
    }
    fn open(&self) -> Result<()> {
        call_editor(&self.path, self.line_index + 1)
    }

    fn cat(&self) -> Result<()> {
        let output = match self.kind {
            TextFileLineItemKind::Plain => std::fs::read_to_string(&self.path)?,
            TextFileLineItemKind::VimEncrypted(ref password) => {
                let output = std::fs::read(&self.path)?;
                let content = vimdecrypt::decrypt(&output, &password)?;
                String::from_utf8(content)?
            }
        };
        println!("{}", output);
        Ok(())
    }
}

fn report_txt_file_with_content(
    path: PathBuf,
    kind: TextFileLineItemKind,
    content: impl BufRead,
    tx: mpsc::Sender<Box<dyn Item>>,
) -> Result<()> {
    for (line_index, line) in content.lines().enumerate() {
        // The file might be binary, i.e. not UTF-8 parsable.
        if let Ok(line) = line {
            if line.trim().is_empty() {
                continue;
            }
            tx.send(Box::new(TextFileLineItem {
                kind: kind.clone(),
                path: path.clone(),
                line,
                line_index,
            }) as Box<dyn Item>)?;
        }
    }
    Ok(())
}

fn report_txt_file(
    path: PathBuf,
    password: &Option<String>,
    tx: mpsc::Sender<Box<dyn Item>>,
) -> Result<()> {
    if let Some(pw) = password {
        // Enough space for "VimCrypt~".
        let mut buf = vec![0u8; 9];
        let mut file = fs::File::open(&path)?;
        file.read(&mut buf)?;
        if buf == b"VimCrypt~" {
            file.seek(SeekFrom::Start(0))?;
            let mut file_contents = Vec::new();
            file.read_to_end(&mut file_contents)?;
            let content = vimdecrypt::decrypt(&file_contents, pw)?;
            report_txt_file_with_content(
                path,
                TextFileLineItemKind::VimEncrypted(pw.to_string()),
                BufReader::new(Cursor::new(content)),
                tx,
            )?;
            return Ok(());
        }
    }
    let reader = BufReader::new(fs::File::open(&path)?);
    report_txt_file_with_content(path, TextFileLineItemKind::Plain, reader, tx)
}

fn report_any_file(path: PathBuf, tx: mpsc::Sender<Box<dyn Item>>) -> Result<()> {
    tx.send(Box::new(AnyFileItem { path }) as Box<dyn Item>)?;
    Ok(())
}

fn handle_dir(
    scope: &Scope<'a>,
    path: impl AsRef<Path>,
    password: &'a Option<String>,
    tx: mpsc::Sender<Box<dyn Item>>,
) -> Result<()> {
    for entry in WalkDir::new(path.as_ref()) {
        if entry.is_err() {
            continue;
        }
        let path = entry.unwrap().path().to_path_buf();
        let tx_clone = tx.clone();
        scope.execute(move || {
            match path.extension().and_then(OsStr::to_str) {
                Some("md") | Some("txt") => report_txt_file(path, password, tx_clone),
                _ => report_any_file(path, tx_clone),
            }.unwrap()
        });
    }
    Ok(())
}

#[derive(Debug)]
struct SkimAdaptor {
    rx: mpsc::Receiver<Box<dyn Item>>,
    items_tx: mpsc::Sender<Box<dyn Item>>,
    buffer: VecDeque<Vec<u8>>,
}

impl std::io::Read for SkimAdaptor {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.buffer.is_empty() {
            // We want to ensure that if we do not have anything to 'read', we want to wait for at
            // least one item to arrive. If all crawler threads are already done, we do not have
            // any more items and all 'tx' will have been dropped. This means that 'revc' will
            // return with an error immediately.
            if let Ok(item) = self.rx.recv() {
                self.buffer.push_back(item.to_string().into_bytes());
                self.items_tx.send(item).unwrap();
            };
            while let Ok(item) = self.rx.try_recv() {
                self.buffer.push_back(item.to_string().into_bytes());
                self.items_tx.send(item).unwrap();
            }
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
        Ok(len + 1)
    }
}

fn update() -> Result<()> {
    let target = self_update::get_target()?;
    self_update::backends::github::Update::configure()?
        .repo_owner("SirVer")
        .repo_name("sar")
        .target(&target)
        .bin_name("sar")
        .show_download_progress(true)
        .show_output(false)
        .no_confirm(true)
        .current_version(cargo_crate_version!())
        .build()?
        .update()?;
    Ok(())
}

#[derive(Debug)]
enum Exit {
    CreateNew,
    /// Sometimes also called Reveal.
    Show,
    Open,
    Cat,
}

fn main() -> Result<()> {
    let args = CommandLineArguments::from_args();

    if args.update {
        update()?;
        return Ok(());
    }
    let configuration_file: ConfigurationFile = {
        let home = dirs::home_dir().expect("HOME not set.");
        toml::from_str(&std::fs::read_to_string(home.join(".sarrc"))?)?
    };

    let pass = if args.encrypted {
        Some(rpassword::prompt_password_stdout("Password: ").unwrap())
    } else {
        None
    };

    let (tx, rx) = mpsc::channel();

    let pool = Pool::new(10);
    pool.scoped(|scope| {
        for dir in configuration_file.reading_directories {
            let tx_clone = tx.clone();
            let pass_ref = &pass;
            scope.recurse(move |scope| {
                let full_directory = shellexpand::tilde(&dir);
                handle_dir(scope, &*full_directory, pass_ref, tx_clone).unwrap();
            });
        }
        drop(tx);

        // TODO(sirver): this feels weird. somehow this should be the main thread that continues.
        // Maybe we do not want a scoped pool, really, but just a regular thread pool.
        scope.execute(move || {
            let (items_tx, items_rx) = mpsc::channel();
            let adaptor = SkimAdaptor {
                rx,
                items_tx,
                buffer: VecDeque::new(),
            };

            let options: SkimOptions<'_> = SkimOptions::default()
                .multi(false)
                .expect("ctrl-n,ctrl-s,ctrl-o".to_string());

            let skim_output =
                match Skim::run_with(&options, Some(Box::new(BufReader::new(adaptor)))) {
                    None => return,
                    Some(s) => s,
                };

            let exit_mode = match skim_output.accept_key.as_ref().map(|s| s as &str) {
                Some("ctrl-n") => Exit::CreateNew,
                Some("ctrl-s") => Exit::Show,
                Some("ctrl-o") => Exit::Open,
                Some("") | None => Exit::Cat,
                Some(unexpected_str) => {
                    // Skim should guarantee that this never happens.
                    unreachable!("Got unexpected: {:?}", unexpected_str);
                }
            };

            let first_selection = skim_output.selected_items.first().unwrap().get_index();
            let selected_item = items_rx.into_iter().nth(first_selection).unwrap();
            match exit_mode {
                // TODO(sirver): Implement creating a new note.
                Exit::CreateNew => unimplemented!(),
                Exit::Show => show_path(&selected_item.path()),
                Exit::Open => selected_item.open(),
                Exit::Cat => selected_item.cat(),
            }.unwrap()
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

        let mut adaptor = SkimAdaptor {
            rx,
            items_tx,
            buffer: VecDeque::new(),
        };

        tx.send(Box::new(TextFileLineItem {
            path: PathBuf::from("/tmp/blub.txt"),
            kind: TextFileLineItemKind::Plain,
            line: "foo bar".into(),
            line_index: 10,
        }) as Box<dyn Item>).unwrap();

        let mut buf = vec![0u8; 256];
        assert_eq!(25, adaptor.read(&mut buf).unwrap());
        assert_eq!(&buf[..25], b"/tmp/blub.txt:11:foo bar\n");

        tx.send(Box::new(TextFileLineItem {
            path: PathBuf::from("/tmp/blub1.txt"),
            kind: TextFileLineItemKind::Plain,
            line: "foo bar blub".into(),
            line_index: 10,
        }) as Box<dyn Item>).unwrap();
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
