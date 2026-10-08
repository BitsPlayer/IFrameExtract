//! Pacman-style progress: left-aligned phase, `[#####-----]` bar, percent.

use iframe_extract::report::{self, Reporter};
use std::io::{self, IsTerminal, Stderr, Write};
use std::time::{Duration, Instant};

const LABEL_W: usize = 32;
const BAR_W: usize = 25;
const MIN_REDRAW: Duration = Duration::from_millis(50);

pub struct PacmanBar {
    enabled: bool,
    tty: bool,
    err: Stderr,
    label: String,
    active: bool,
    last_len: usize,
    last_draw: Instant,
}

impl PacmanBar {
    pub fn new(enabled: bool) -> Self {
        let err = io::stderr();
        let tty = enabled && err.is_terminal();
        Self {
            enabled,
            tty,
            err,
            label: String::new(),
            active: false,
            last_len: 0,
            last_draw: Instant::now() - MIN_REDRAW,
        }
    }

    pub fn header(&mut self, line: &str) {
        if !self.enabled {
            return;
        }
        let _ = writeln!(self.err, ":: {line}");
        let _ = self.err.flush();
    }

    pub fn summary(&mut self, line: &str) {
        if !self.enabled {
            return;
        }
        let _ = writeln!(self.err, ":: {line}");
        let _ = self.err.flush();
    }

    fn line(label: &str, pct: u32, detail: &str) -> String {
        let bar = report::render_bar(pct, BAR_W);
        if detail.is_empty() {
            format!("{label:<LABEL_W$} {bar} {pct:3}%")
        } else {
            format!("{label:<LABEL_W$} {bar} {pct:3}%  {detail}")
        }
    }

    fn paint(&mut self, text: &str, newline: bool) {
        if self.tty {
            let pad = self.last_len.saturating_sub(text.len());
            let _ = write!(self.err, "\r{text}{:pad$}", "");
            if newline {
                let _ = writeln!(self.err);
                self.last_len = 0;
            } else {
                self.last_len = text.len();
            }
        } else if newline {
            let _ = writeln!(self.err, "{text}");
        }
        let _ = self.err.flush();
        self.last_draw = Instant::now();
    }

    fn clear_line(&mut self) {
        if self.tty && self.last_len > 0 {
            let _ = write!(self.err, "\r{:width$}\r", "", width = self.last_len);
            self.last_len = 0;
        }
    }
}

impl Reporter for PacmanBar {
    fn phase(&mut self, name: &str) {
        if !self.enabled {
            return;
        }
        if self.active {
            self.finish_phase("");
        }
        self.label = name.to_string();
        self.active = true;
        self.last_draw = Instant::now() - MIN_REDRAW;
        self.progress(0, 1, "");
    }

    fn progress(&mut self, done: u64, total: u64, detail: &str) {
        if !self.enabled || !self.active {
            return;
        }
        let pct = report::percent(done, total);
        let force = pct >= 100 || done == 0;
        if !force && self.last_draw.elapsed() < MIN_REDRAW {
            return;
        }
        if !self.tty {
            return;
        }
        let text = Self::line(&self.label, pct, detail);
        self.paint(&text, false);
    }

    fn finish_phase(&mut self, detail: &str) {
        if !self.enabled {
            self.active = false;
            return;
        }
        if !self.active && detail.is_empty() {
            return;
        }
        let text = Self::line(&self.label, 100, detail);
        self.paint(&text, true);
        self.active = false;
    }

    fn note(&mut self, msg: &str) {
        if !self.enabled {
            return;
        }
        self.clear_line();
        let _ = writeln!(self.err, "  {msg}");
        let _ = self.err.flush();
        self.last_draw = Instant::now() - MIN_REDRAW;
    }
}

impl Drop for PacmanBar {
    fn drop(&mut self) {
        if self.tty && self.last_len > 0 {
            let _ = writeln!(self.err);
        }
    }
}
