//! Terminal styling, centralised so `--no-color` has one switch to flip.

pub struct Ui {
    pub color: bool,
}

/// Startup banner, slant style. Kept under 60 columns so it survives
/// narrow terminals without wrapping.
const BANNER: &str = r#"   ______          _____    __  __    ______    __     __
  / ____/         / ___/   / / / /   / ____/   / /    / /
 / /      _____   \__ \   / /_/ /   / __/     / /    / /
/ /___   /_____/ ___/ /  / __  /   / /___    / /___ / /___
\____/          /____/  /_/ /_/   /_____/   /_____//_____/
"#;

impl Ui {
    fn paint(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    /// The startup ASCII art, colored when the terminal is.
    pub fn banner(&self) -> String {
        self.paint("1;36", BANNER)
    }

    /// The `In [n]:` prompt.
    pub fn prompt_in(&self, n: usize) -> String {
        self.paint("1;32", &format!("In [{n}]: "))
    }

    /// Printable width of that prompt, excluding color codes — the column
    /// where the user's code starts, and the alignment base for
    /// continuation-line auto-indent.
    pub fn prompt_width(n: usize) -> usize {
        format!("In [{n}]: ").len()
    }

    pub fn out_label(&self, n: usize) -> String {
        self.paint("1;31", &format!("Out[{n}]: "))
    }

    /// Marker shown only when c-shell, rather than the C program, has to end
    /// a partial terminal line before printing diagnostics, a value or prompt.
    pub fn inserted_newline_marker(&self) -> String {
        self.dim("↵")
    }

    pub fn err(&self, s: &str) -> String {
        self.paint("31", s)
    }

    pub fn warn(&self, s: &str) -> String {
        self.paint("33", s)
    }

    pub fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }

    pub fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
}
