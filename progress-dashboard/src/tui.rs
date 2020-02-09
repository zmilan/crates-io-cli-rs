use crate::{
    tree::{self, TreeRoot},
    Progress, TreeValue,
};
use futures_timer::Delay;

use futures::{channel::mpsc, future::select, future::Either, SinkExt, StreamExt};
use std::{fmt, io, time::Duration};
use termion::event::Key;
use termion::{input::TermRead, raw::IntoRawMode, screen::AlternateScreen};
use tui::style::{Color, Style};
use tui::{
    backend::TermionBackend,
    buffer::Buffer,
    layout::Rect,
    widgets::{Block, Borders, Widget},
    widgets::{Paragraph, Text},
};
use tui_react::Terminal;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone)]
pub struct Config {
    pub frames_per_second: u8,
}

pub fn render(
    progress: TreeRoot,
    Config { frames_per_second }: Config,
) -> Result<impl std::future::Future<Output = ()>, std::io::Error> {
    let mut terminal = {
        let stdout = io::stdout().into_raw_mode()?;
        let stdout = AlternateScreen::from(stdout);
        let backend = TermionBackend::new(stdout);
        Terminal::new(backend)?
    };

    let duration_per_frame = Duration::from_secs(1) / frames_per_second as u32;
    let (mut key_send, mut key_receive) = mpsc::channel::<Key>(1);

    // This brings blocking key-handling into the async world
    std::thread::spawn(move || -> Result<(), io::Error> {
        for key in io::stdin().keys() {
            let key = key?;
            futures::executor::block_on(key_send.send(key)).ok();
        }
        Ok(())
    });

    let render_fut = async move {
        let mut entries_buf = Vec::new();
        loop {
            let window_size = terminal.pre_render().expect("pre-render to work");
            let buf = terminal.current_buffer_mut();
            progress.sorted_snapshot(&mut entries_buf);

            entries_buf = draw_everything(entries_buf, window_size, buf);
            terminal.post_render().expect("post render to work");

            let delay = Delay::new(duration_per_frame);
            match select(delay, key_receive.next()).await {
                Either::Left(_delay_timed_out) => continue,
                Either::Right((Some(key), _delay)) => match key {
                    Key::Esc | Key::Char('q') | Key::Ctrl('c') | Key::Ctrl('[') => {
                        return ();
                    }
                    _ => continue,
                },
                _ => continue,
            };
        }
    };
    Ok(render_fut)
}

struct ProgressFormat<'a>(&'a Option<Progress>, u16);

impl<'a> fmt::Display for ProgressFormat<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(p) => {
                match p.done_at {
                    Some(done_at) => write!(f, "{} / {}", p.step, done_at),
                    None => write!(f, "{}", p.step),
                }?;
                if let Some(unit) = p.unit {
                    write!(f, " {}", unit)?;
                }
                Ok(())
            }
            None => write!(f, "{:─<width$}", '─', width = self.1 as usize),
        }
    }
}

fn draw_everything(
    entries: Vec<(tree::Key, TreeValue)>,
    window_size: Rect,
    buf: &mut Buffer,
) -> Vec<(tree::Key, TreeValue)> {
    let mut progress_pane = Block::default()
        .title("Progress Tree")
        .borders(Borders::ALL);
    progress_pane.draw(window_size, buf);
    let mut current = progress_pane.inner(window_size);
    let is_overflowing = if entries.len() > current.height as usize {
        current.height = current.height.saturating_sub(1);
        true
    } else {
        false
    };

    let column_end = current.width / 2;
    let max_prefix_len = draw_tree_prefix(&entries, buf, current, column_end);

    let max_prefix_len = max_prefix_len.unwrap_or_default().min(column_end);
    draw_progress(&entries, buf, current, max_prefix_len);

    if is_overflowing {
        let overflow_rect = Rect {
            y: current.height + 1,
            height: 1,
            ..current
        };
        draw_overflow(
            entries.iter().skip(current.height as usize),
            buf,
            overflow_rect,
        );
    }
    entries
}

fn draw_progress(
    entries: &[(tree::Key, TreeValue)],
    buf: &mut Buffer,
    current: Rect,
    max_prefix_len: u16,
) {
    let x_offset = max_prefix_len + 1;
    for (line, (_, TreeValue { progress, .. })) in
        entries.iter().take(current.height as usize).enumerate()
    {
        let max_width = current.width.saturating_sub(x_offset);
        let progress_text = format!("{progress}", progress = ProgressFormat(progress, max_width));
        let progress_text_blocks = progress_text.graphemes(true).count() as u16;

        let y = current.y + line as u16;
        let progress_style = if let Some(fraction) = progress.and_then(|p| p.fraction()) {
            draw_progress_bar(buf, max_width, x_offset, y, fraction)
        } else {
            Style::default().bg(Color::Reset)
        };
        let width = (progress_text_blocks + 2).min(current.width.saturating_sub(x_offset));
        let progress_text = Text::Styled(progress_text.into(), progress_style);
        let progress_rect = Rect {
            x: x_offset,
            y,
            width,
            height: 1,
        };
        Paragraph::new(
            [
                Text::Raw("│".into()),
                Text::Styled(" ".into(), progress_style),
                progress_text,
            ]
            .iter(),
        )
        .draw(progress_rect, buf);
    }
}

fn draw_progress_bar(
    buf: &mut Buffer,
    max_width: u16,
    x_offset: u16,
    y: u16,
    fraction: f32,
) -> Style {
    let fractional_progress_rect = Rect {
        x: x_offset,
        y,
        height: 1,
        width: ((max_width as f32 * fraction) as u16).min(max_width),
    };
    let color = if fraction >= 1.0 {
        Color::Green
    } else {
        Color::Yellow
    };
    tui_react::fill_background(fractional_progress_rect, buf, color);
    Style::default().bg(color).fg(Color::Black)
}

fn draw_tree_prefix(
    entries: &[(tree::Key, TreeValue)],
    buf: &mut Buffer,
    current: Rect,
    column_end: u16,
) -> Option<u16> {
    let mut max_prefix_len = None;
    for (line, (key, value)) in entries.iter().take(current.height as usize).enumerate() {
        let mut tree_prefix = format!(
            "{:>width$} {}",
            '‧',
            value.title,
            width = key.level() as usize
        );
        tree_prefix = tree_prefix
            .graphemes(true)
            .take(column_end.saturating_sub(1) as usize)
            .collect();
        if tree_prefix.len() + 1 >= column_end as usize {
            tree_prefix.push('…');
        }
        max_prefix_len = Some(max_prefix_len.unwrap_or(0).max(tree_prefix.len() as u16));
        let tree_prefix = Text::Raw(tree_prefix.into());
        let line_rect = Rect {
            y: current.y + line as u16,
            height: 1,
            ..current
        };
        Paragraph::new([tree_prefix].iter()).draw(line_rect, buf);
    }
    max_prefix_len
}

fn draw_overflow<'a>(
    entries: impl Iterator<Item = &'a (tree::Key, TreeValue)>,
    buf: &mut Buffer,
    overflow_rect: Rect,
) {
    let (count, mut progress_percent) = entries.fold(
        (0usize, 0f32),
        |(count, progress_percent), (_key, value)| {
            let progress = value
                .progress
                .and_then(|p| p.fraction().map(|f| f * 100.0))
                .unwrap_or_default();
            (count + 1, progress_percent + progress)
        },
    );
    progress_percent /= count as f32;
    Paragraph::new(
        [Text::Raw(
            format!("…and {} more -- {:4.01}%", count, progress_percent).into(),
        )]
        .iter(),
    )
    .draw(overflow_rect, buf);
}
