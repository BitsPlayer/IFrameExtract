//! Progress callbacks so the CLI can draw a pacman-style bar without the library
//! knowing about terminals.

pub trait Reporter {
    fn phase(&mut self, name: &str);
    fn progress(&mut self, done: u64, total: u64, detail: &str);
    fn finish_phase(&mut self, detail: &str);
    fn note(&mut self, msg: &str);
}

pub struct Silent;

impl Reporter for Silent {
    fn phase(&mut self, _: &str) {}
    fn progress(&mut self, _: u64, _: u64, _: &str) {}
    fn finish_phase(&mut self, _: &str) {}
    fn note(&mut self, _: &str) {}
}

pub fn percent(done: u64, total: u64) -> u32 {
    if total == 0 {
        100
    } else {
        ((done.saturating_mul(100)) / total).min(100) as u32
    }
}

pub fn render_bar(percent: u32, width: usize) -> String {
    let filled = (percent as usize * width) / 100;
    let mut s = String::with_capacity(width + 2);
    s.push('[');
    for i in 0..width {
        s.push(if i < filled { '#' } else { '-' });
    }
    s.push(']');
    s
}

pub fn human_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let x = n as f64;
    if x < K {
        format!("{n} B")
    } else if x < K * K {
        format!("{:.1} KiB", x / K)
    } else if x < K * K * K {
        format!("{:.1} MiB", x / (K * K))
    } else {
        format!("{:.2} GiB", x / (K * K * K))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_fills_with_hashes() {
        assert_eq!(render_bar(0, 10), "[----------]");
        assert_eq!(render_bar(100, 10), "[##########]");
        assert_eq!(render_bar(50, 10), "[#####-----]");
    }

    #[test]
    fn percent_is_capped() {
        assert_eq!(percent(0, 10), 0);
        assert_eq!(percent(5, 10), 50);
        assert_eq!(percent(10, 10), 100);
        assert_eq!(percent(0, 0), 100);
    }
}
