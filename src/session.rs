//! Accumulated session state: everything typed so far that compiled.

use crate::codegen::Slot;
use crate::lex;
use crate::proc::StdinEvent;

#[derive(Clone)]
enum LogEntry {
    AddedFile {
        stdin_events: usize,
    },
    AddedStmt {
        opened_scope: bool,
        stdin_events: usize,
    },
    ReplacedFile {
        index: usize,
        previous: String,
        stdin_events: usize,
    },
}

#[derive(Default, Clone)]
pub struct Session {
    /// `#include`s, `#define`s, function definitions, type definitions.
    pub file_items: Vec<String>,
    /// Statements and block-scope declarations, in order, inside `main`.
    /// A literal `{` entry begins each compiler-approved shadowing epoch;
    /// codegen closes all such scopes after the newest input.
    pub stmts: Vec<String>,
    /// Number of captured stdin events associated with each `stmts` entry.
    /// The generated replay uses the flattened tape; `%src` uses these counts
    /// only to add redacted annotations at the right input boundary.
    stmt_stdin_events: Vec<usize>,
    /// Captured program-input lines, never persisted or exposed by `%src`.
    stdin_tape: Vec<StdinEvent>,
    /// Number of shadowing scopes currently open in `stmts`.
    pub(crate) scope_depth: usize,
    /// Input counter driving the `In [n]` prompt. Like IPython it advances on
    /// every input including failed ones, so `_n` always lines up with the
    /// `Out[n]` the user saw.
    pub counter: usize,
    /// C inputs indexed by their one-based `In[n]`, including failed inputs.
    /// This archive exists only so `%edit [n]` can recover multi-line text; it
    /// is cleared by `%reset` and is never persisted or listed as history.
    inputs: Vec<String>,
    /// The external-side-effect replay warning is intentionally once per
    /// session; `%reset` starts a fresh warning epoch.
    external_replay_warning_shown: bool,
    /// How each accepted input changed the generated program, so `%undo` can
    /// reverse appends, scope openings and file-scope replacements exactly.
    log: Vec<LogEntry>,
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
                self.log.push(LogEntry::AddedFile { stdin_events: 0 });
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
        self.stmt_stdin_events.push(0);
        self.log.push(LogEntry::AddedStmt {
            opened_scope: false,
            stdin_events: 0,
        });
    }

    /// Record a declaration that compiled only after entering a nested block.
    /// Every later statement remains in that block until codegen closes it at
    /// the end of `main`, giving C shadowing the shape of REPL rebinding.
    pub fn commit_scoped(&mut self, text: &str) {
        self.stmts.push(indent("{"));
        self.stmt_stdin_events.push(0);
        self.stmts.push(indent(text));
        self.stmt_stdin_events.push(0);
        self.scope_depth += 1;
        self.log.push(LogEntry::AddedStmt {
            opened_scope: true,
            stdin_events: 0,
        });
    }

    /// Replace a compiler-selected file-scope item in place. Keeping its index
    /// preserves declaration order for older functions/types that refer to it.
    pub fn replace_file(&mut self, index: usize, text: &str) {
        let previous = std::mem::replace(&mut self.file_items[index], text.to_string());
        self.log.push(LogEntry::ReplacedFile {
            index,
            previous,
            stdin_events: 0,
        });
    }

    /// Attach fresh program-input lines to the state-changing input that was
    /// just committed. scanf-bearing expressions are retained as statements,
    /// so successful reads always have a journal entry here.
    pub fn attach_stdin_events(&mut self, events: Vec<StdinEvent>) {
        if events.is_empty() {
            return;
        }
        let count = events.len();
        match self.log.last_mut().expect("stdin input must change state") {
            LogEntry::AddedStmt { stdin_events, .. } => {
                *stdin_events += count;
                *self
                    .stmt_stdin_events
                    .last_mut()
                    .expect("statement stdin annotation") += count;
            }
            LogEntry::AddedFile { stdin_events } | LogEntry::ReplacedFile { stdin_events, .. } => {
                *stdin_events += count;
            }
        }
        self.stdin_tape.extend(events);
    }

    pub fn stdin_tape(&self) -> &[StdinEvent] {
        &self.stdin_tape
    }

    pub fn stmt_stdin_event_count(&self, index: usize) -> usize {
        self.stmt_stdin_events.get(index).copied().unwrap_or(0)
    }

    /// Drop the most recently accepted state-changing input, returning what
    /// was removed. Pure queries and magic commands never enter this log.
    pub fn undo(&mut self) -> Option<String> {
        let (removed, stdin_events) = match self.log.pop()? {
            LogEntry::AddedFile { stdin_events } => (self.file_items.pop(), stdin_events),
            LogEntry::AddedStmt {
                opened_scope,
                stdin_events,
            } => {
                let removed = self.stmts.pop().map(|s| s.trim().to_string());
                self.stmt_stdin_events.pop();
                if opened_scope {
                    let brace = self.stmts.pop();
                    self.stmt_stdin_events.pop();
                    debug_assert_eq!(brace.as_deref().map(str::trim), Some("{"));
                    self.scope_depth = self.scope_depth.saturating_sub(1);
                }
                (removed, stdin_events)
            }
            LogEntry::ReplacedFile {
                index,
                previous,
                stdin_events,
            } => {
                let removed = std::mem::replace(&mut self.file_items[index], previous);
                (Some(removed), stdin_events)
            }
        };
        self.stdin_tape
            .truncate(self.stdin_tape.len().saturating_sub(stdin_events));
        removed
    }

    pub fn remember_input(&mut self, number: usize, text: &str) {
        debug_assert_eq!(number, self.inputs.len() + 1);
        self.inputs.push(text.to_string());
    }

    pub fn input(&self, number: usize) -> Option<&str> {
        number
            .checked_sub(1)
            .and_then(|index| self.inputs.get(index).map(String::as_str))
    }

    pub fn last_input(&self) -> Option<&str> {
        self.inputs.last().map(String::as_str)
    }

    pub fn external_replay_warning_shown(&self) -> bool {
        self.external_replay_warning_shown
    }

    pub fn mark_external_replay_warning_shown(&mut self) {
        self.external_replay_warning_shown = true;
    }

    /// Every identifier the session mentions, deduplicated and sorted —
    /// the completion vocabulary. Single letters are omitted: offering `x`
    /// as a completion of `x` helps nobody.
    pub fn identifiers(&self) -> Vec<String> {
        let mut seen = std::collections::BTreeSet::new();
        for text in self.file_items.iter().chain(self.stmts.iter()) {
            for w in lex::identifiers(text) {
                if w.len() >= 2 {
                    seen.insert(w);
                }
            }
        }
        seen.into_iter().collect()
    }

    pub fn reset(&mut self) {
        self.file_items.clear();
        self.stmts.clear();
        self.stmt_stdin_events.clear();
        self.stdin_tape.clear();
        self.scope_depth = 0;
        self.log.clear();
        self.counter = 0;
        self.inputs.clear();
        self.external_replay_warning_shown = false;
    }
}

fn indent(s: &str) -> String {
    s.lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_inputs_are_available_until_reset() {
        let mut session = Session::default();
        session.remember_input(1, "int broken = ;");
        session.remember_input(2, "40 + 2");
        assert_eq!(session.input(1), Some("int broken = ;"));
        assert_eq!(session.input(2), Some("40 + 2"));
        assert_eq!(session.input(0), None);
        assert_eq!(session.input(3), None);
        assert_eq!(session.last_input(), Some("40 + 2"));
        session.reset();
        assert_eq!(session.input(1), None);
    }

    #[test]
    fn undo_closes_a_shadowing_scope() {
        let mut session = Session::default();
        session.commit("int x = 1;", Slot::Stmt);
        session.commit_scoped("int x = 2;");
        session.attach_stdin_events(vec![StdinEvent {
            bytes: b"2\n".to_vec(),
            eof: false,
        }]);
        assert_eq!(session.stdin_tape().len(), 1);
        assert_eq!(session.stmt_stdin_event_count(2), 1);
        assert_eq!(session.scope_depth, 1);
        assert_eq!(session.undo().as_deref(), Some("int x = 2;"));
        assert_eq!(session.scope_depth, 0);
        assert_eq!(session.stmts.len(), 1);
        assert!(session.stdin_tape().is_empty());
    }

    #[test]
    fn undo_restores_a_replaced_file_item() {
        let mut session = Session::default();
        session.commit("int f(void) { return 1; }", Slot::FileScope);
        session.replace_file(0, "int f(void) { return 2; }");
        assert_eq!(session.undo().as_deref(), Some("int f(void) { return 2; }"));
        assert_eq!(session.file_items[0], "int f(void) { return 1; }");
    }
}
