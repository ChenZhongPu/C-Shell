//! Accumulated session state: everything typed so far that compiled.

use crate::codegen::Slot;
use crate::lex;

#[derive(Default)]
pub struct Session {
    /// `#include`s, `#define`s, function definitions, type definitions.
    pub file_items: Vec<String>,
    /// Statements and block-scope declarations, in order, inside `main`.
    pub stmts: Vec<String>,
    /// Raw input lines as typed, for `%history`.
    pub history: Vec<String>,
    /// Input counter driving the `In [n]` prompt. Like IPython it advances on
    /// every input including failed ones, so `_n` always lines up with the
    /// `Out[n]` the user saw.
    pub counter: usize,
    /// Which list each accepted input went into, so `%undo` can put it back.
    log: Vec<Slot>,
}

impl Session {
    /// Record an input that compiled and ran.
    ///
    /// A pure expression is evaluated and forgotten: it asked a question
    /// rather than changing anything, so replaying it would only make every
    /// later evaluation slower. Anything that might mutate state is kept.
    pub fn commit(&mut self, text: &str, slot: Slot) {
        let stored = match slot {
            Slot::FileScope => {
                self.file_items.push(text.to_string());
                self.log.push(slot);
                return;
            }
            Slot::Stmt => text.to_string(),
            Slot::Expr if lex::may_have_side_effects(text) => {
                // The cast keeps the side effects without tripping a
                // warning about a computed value going unused.
                format!("(void)({text});")
            }
            Slot::Expr => return,
        };
        self.stmts.push(indent(&stored));
        self.log.push(slot);
    }

    /// Drop the most recently accepted input, returning what was removed.
    pub fn undo(&mut self) -> Option<String> {
        let slot = self.log.pop()?;
        match slot {
            Slot::FileScope => self.file_items.pop(),
            Slot::Stmt | Slot::Expr => self.stmts.pop().map(|s| s.trim().to_string()),
        }
    }

    pub fn reset(&mut self) {
        self.file_items.clear();
        self.stmts.clear();
        self.log.clear();
        self.counter = 0;
    }
}

fn indent(s: &str) -> String {
    s.lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}
