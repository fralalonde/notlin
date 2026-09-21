//! Java source emission: a small indentation-aware writer.

#[derive(Debug, Default)]
pub struct JavaOut {
    pub buf: String,
    pub indent: usize,
}

impl JavaOut {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn line(&mut self, s: impl AsRef<str>) {
        for l in s.as_ref().lines() {
            if l.is_empty() {
                self.buf.push('\n');
            } else {
                self.buf.push_str(&"    ".repeat(self.indent));
                self.buf.push_str(l);
                self.buf.push('\n');
            }
        }
    }

    pub fn open(&mut self, s: impl AsRef<str>) {
        self.line(format!("{} {{", s.as_ref()));
        self.indent += 1;
    }

    pub fn close(&mut self) {
        self.indent = self.indent.saturating_sub(1);
        self.buf.push_str(&"    ".repeat(self.indent));
        self.buf.push_str("}\n");
    }

    pub fn blank(&mut self) {
        self.buf.push('\n');
    }

    pub fn finish(self) -> String {
        self.buf
    }
}
