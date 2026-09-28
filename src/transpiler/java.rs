//! Java source emission: a small indentation-aware writer.

#[derive(Debug, Default)]
pub struct JavaOut {
    pub buf: String,
    pub indent: usize,
    profile_output: Option<std::time::Duration>,
}

impl JavaOut {
    pub fn new() -> Self {
        Self {
            profile_output: std::env::var_os("NOTLIN_PROFILE").map(|_| std::time::Duration::ZERO),
            ..Self::default()
        }
    }

    pub fn line(&mut self, s: impl AsRef<str>) {
        let started = self.profile_output.map(|_| std::time::Instant::now());
        for l in s.as_ref().lines() {
            if l.is_empty() {
                self.buf.push('\n');
            } else {
                self.buf.push_str(&"    ".repeat(self.indent));
                self.buf.push_str(l);
                self.buf.push('\n');
            }
        }
        self.record_output(started);
    }

    pub fn open(&mut self, s: impl AsRef<str>) {
        self.line(format!("{} {{", s.as_ref()));
        self.indent += 1;
    }

    pub fn close(&mut self) {
        let started = self.profile_output.map(|_| std::time::Instant::now());
        self.indent = self.indent.saturating_sub(1);
        self.buf.push_str(&"    ".repeat(self.indent));
        self.buf.push_str("}\n");
        self.record_output(started);
    }

    pub fn blank(&mut self) {
        let started = self.profile_output.map(|_| std::time::Instant::now());
        self.buf.push('\n');
        self.record_output(started);
    }

    /// Close the current block and immediately open a continuation clause
    /// on the same line (`} else {`, `} catch (e) {`). Unlike `close()`,
    /// this writes only ONE closing brace (no standalone bracket).
    pub fn close_then(&mut self, s: impl AsRef<str>) {
        let started = self.profile_output.map(|_| std::time::Instant::now());
        // One closing brace at the opener's depth + 1 continuation header
        // re-opening for the clause body. Unlike close() this does NOT
        // double-write `}` (close() + close_then stacked a stray one).
        self.indent = self.indent.saturating_sub(1);
        self.buf.push_str(&"    ".repeat(self.indent));
        self.buf.push_str(&format!("}} {} {{\n", s.as_ref()));
        self.indent += 1;
        self.record_output(started);
    }

    pub fn finish(self) -> String {
        if let Some(elapsed) = self.profile_output {
            crate::transpiler::record_java_output_profile(elapsed);
        }
        self.buf
    }

    fn record_output(&mut self, started: Option<std::time::Instant>) {
        if let (Some(total), Some(started)) = (&mut self.profile_output, started) {
            *total += started.elapsed();
        }
    }
}
