extern crate tokio_timer;
extern crate termion;
extern crate futures;
extern crate futures_cpupool;

use self::futures_cpupool::CpuPool;

use clap;
use std;
use self::termion::event::Key;
use self::termion::raw::IntoRawMode;
use self::termion::input::TermRead;
use self::termion::clear;
use self::termion::cursor;
use self::futures::Future;
use std::io::{self, Write};

use utils::ok_or_exit;

pub fn handle_interactive_search(_args: &clap::ArgMatches) {
    let stdin = io::stdin();
    let mut stdout = ok_or_exit(io::stdout().into_raw_mode());
    ok_or_exit(write!(stdout, "{}{}", cursor::Goto(1, 1), clear::All));
    let mut term = String::new();
    let pool = CpuPool::new(1);
    let mut most_recent_search = None;
    let interruptable_timer = tokio_timer::Timer::default();

    for k in stdin.keys() {
        match ok_or_exit(k) {
            Key::Char(c) => {
                term.push(c);
            }
            Key::Backspace => {
                term.pop();
            }
            Key::Esc => {
                break;
            }
            _ => println!("unsupported!"),
        }
        ok_or_exit(write!(stdout,
                          "{}{}{}{}",
                          cursor::Show,
                          cursor::Goto(1, 1),
                          clear::CurrentLine,
                          term));
        let term_owned = term.clone();
        ok_or_exit(write!(io::stdout(),
                          "{}{}{}searching {}",
                          cursor::Hide,
                          cursor::Goto(1, 2),
                          clear::CurrentLine,
                          term_owned));
        let waiter = interruptable_timer.sleep(std::time::Duration::from_secs(2))
            .and_then(move |_| {
                ok_or_exit(write!(io::stdout(),
                                  "{}{}{}    {} done !",
                                  cursor::Hide,
                                  cursor::Goto(1, 2),
                                  clear::CurrentLine,
                                  term_owned));
                io::stdout().flush().ok();
                Ok(())
            });
        most_recent_search = Some(pool.spawn(waiter));
        stdout.flush().ok();
    }
    drop(most_recent_search);
}
