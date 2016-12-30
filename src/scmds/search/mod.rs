extern crate curl;
extern crate termion;
extern crate futures;
extern crate tokio_core;
extern crate tokio_curl;
extern crate futures_cpupool;

use clap;
use std::cmp;
use std::str;
use std::sync::{Mutex, Arc};
use rustc_serialize::json;
use std::thread;
use self::futures_cpupool::CpuPool;
use self::curl::easy::Easy;
use self::termion::terminal_size;
use self::termion::event::Key;
use self::termion::raw::IntoRawMode;
use self::termion::input::TermRead;
use self::termion::clear;
use self::termion::cursor;
use self::tokio_core::reactor::Core;
use self::futures::{Sink, Stream, Future};
use self::futures::sync::mpsc;
use self::tokio_curl::Session;
use std::io::{self, Write};
use std::fmt::{self, Display};
use std::iter;
use std::default::Default;

use utils::ok_or_exit;

const INFO_LINE: cursor::Goto = cursor::Goto(1, 2);
const CONTENT_LINE: cursor::Goto = cursor::Goto(1, 3);
const PAGE_SIZE: usize = 20;

fn sanitize(input: &str) -> String {
    input.chars()
         .map(|c| if c == '\n' { ' ' } else { c })
         .collect()
}

#[derive(RustcDecodable, Default)]
struct Meta {
    total: u32,
    page_size: Option<usize>,
}

#[derive(RustcDecodable, Debug, Clone, Default)]
struct Crate {
    description: String,
    downloads: u32,
    max_version: String,
    name: String,
}

impl Display for Crate {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.name.is_empty() {
            write!(f, "{clear}", clear = clear::AfterCursor)
        } else {
            write!(f,
                   "{name} | {desc:.80} | {downloads} | {version}",
                   name = self.name,
                   desc = sanitize(&self.description),
                   downloads = self.downloads,
                   version = self.max_version)
        }
    }
}

#[derive(RustcDecodable, Default)]
struct SearchResult {
    crates: Vec<Crate>,
    meta: Meta,
}

impl SearchResult {
    fn with_page_size(page_size: usize) -> SearchResult {
        SearchResult {
            meta: Meta { page_size: Some(page_size), ..Default::default() },
            ..Default::default()
        }
    }
    fn from_data(buf: &[u8], page_size: usize) -> Result<SearchResult, json::DecoderError> {
        str::from_utf8(buf)
            .map_err(|e| json::DecoderError::ApplicationError(format!("{}", e)))
            .and_then(json::decode)
            .map(|mut v: SearchResult| {
                v.meta.page_size = Some(page_size);
                v
            })
    }
}

impl Display for SearchResult {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let (mw, _mh) = terminal_size().map(|(w, h)| (w as usize, h as usize)).unwrap_or((80, 20));
        for krate in self.crates
            .iter()
            .cloned()
            .chain(iter::repeat(Crate::default()))
            .take(self.meta.page_size.as_ref().cloned().unwrap()) {
            let krate = format!("{}", krate);
            write!(f,
                   "{clear}{:.max$}{down}{left}",
                   krate,
                   clear = clear::CurrentLine,
                   down = cursor::Down(1),
                   left = cursor::Left(cmp::max(krate.len(), mw as usize) as u16),
                   max = mw)?;
        }
        Ok(())
    }
}

pub fn handle_interactive_search(_args: &clap::ArgMatches) {
    let stdin = io::stdin();
    let mut stdout = ok_or_exit(io::stdout().into_raw_mode());
    ok_or_exit(write!(stdout, "{}{}", cursor::Goto(1, 1), clear::All));
    promptf("", &mut stdout);
    usage();

    let mut term = String::new();
    let (sender, receiver) = mpsc::channel(10);
    let pool = CpuPool::new(1);

    let t = thread::spawn(|| {
        let mut reactor = ok_or_exit(Core::new());
        let session = Session::new(reactor.handle());
        let search_terms = receiver.and_then(|term| {
            let mut req = Easy::new();
            ok_or_exit(req.get(true));
            let url = format!("https://crates.io/api/v1/crates?page=1&per_page={}&q={}&sort=",
                              PAGE_SIZE,
                              req.url_encode(String::as_bytes(&term)));
            ok_or_exit(req.url(&url));
            let buf = Arc::new(Mutex::new(Vec::new()));
            let buf_handle = buf.clone();
            ok_or_exit(req.write_function(move |data| {
                buf_handle.lock().unwrap().extend_from_slice(data);
                Ok(data.len())
            }));
            info(&"searching ...");
            session.perform(req)
                   .map_err(|e| {
                       info(&e);
                       ()
                   })
                   .map(move |r| {
                       let result = SearchResult::from_data(&buf.lock().unwrap(), PAGE_SIZE);
                       (r, result)
                   })
        })
                                   .for_each(|(_response, search)| {
                                       let search: SearchResult = ok_or_exit(search);
                                       info(&format!("{} results in total, showing {} max",
                                                     search.meta.total,
                                                     search.meta.page_size.as_ref().unwrap()));
                                       if search.crates.is_empty() {
                                           let last = usage();
                                           write!(io::stdout(),
                                                  "{gotolast} - 0 results found",
                                                  gotolast = cursor::Goto(last as u16, INFO_LINE.1))
                                               .ok();
                                       } else {
                                           write!(io::stdout(), "{goto}{}", search, goto = CONTENT_LINE).ok();
                                       }
                                       io::stdout().flush().ok();
                                       Ok(())
                                   });
        reactor.run(search_terms).ok();
    });

    let mut ongoing_search = None;
    for k in stdin.keys() {
        match ok_or_exit(k) {
            Key::Char('\n') => {
                term.clear();
            }
            Key::Char(c) => {
                term.push(c);
            }
            Key::Backspace => {
                term.pop();
            }
            Key::Esc => {
                break;
            }
            key @ _ => {
                info(&format!("unsupported key sequence: {:?}", key));
                continue;
            }
        }

        promptf(&term, &mut stdout);
        if term.is_empty() {
            usage();
            write!(stdout,
                   "{goto}{}",
                   SearchResult::with_page_size(PAGE_SIZE),
                   goto = CONTENT_LINE)
                .ok();
            stdout.flush().ok();
        } else {
            ongoing_search = Some(pool.spawn(sender.clone().send(term.clone())));
        }
    }
    drop(ongoing_search);
    drop(sender);
    t.join().unwrap();
    reset_terminal();

    fn reset_terminal() {
        write!(io::stdout(), "{}{}", cursor::Show, clear::All).ok();
    }

    fn usage() -> usize {
        info(&"(<ESC> to abort, <enter> to clear) Please enter your search term.")
    }

    fn info(item: &Display) -> usize {
        let buf = format!("{}", item);
        write!(io::stdout(),
               "{hide}{goto}{clear}{}",
               buf,
               hide = cursor::Hide,
               goto = INFO_LINE,
               clear = clear::CurrentLine)
            .ok();
        io::stdout().flush().ok();
        buf.len()
    }

    fn promptf(term: &str, stdout: &mut io::Stdout) {
        write!(stdout,
               "{show}{goto}{clear}crates.io: {}",
               term,
               show = cursor::Show,
               goto = cursor::Goto(1, 1),
               clear = clear::CurrentLine)
            .ok();
        stdout.flush().ok();

    }
}