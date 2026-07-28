//! Accumulated session state: everything typed so far that compiled.

use crate::codegen::Slot;
use crate::lex;
use crate::proc::StdinEvent;

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
    }

    /// Replace a compiler-selected file-scope item in place. Keeping its index
    /// preserves declaration order for older functions/types that refer to it.
    pub fn replace_file(&mut self, index: usize, text: &str) {
        self.file_items[index] = text.to_string();
    }

    /// Attach fresh program-input lines to the state-changing input that was
    /// just committed. scanf-bearing expressions are retained as statements,
    /// so successful reads always have a journal entry here.
    pub fn attach_stdin_events(&mut self, events: Vec<StdinEvent>) {
        if events.is_empty() {
            return;
        }
        let count = events.len();
        *self
            .stmt_stdin_events
            .last_mut()
            .expect("stdin input must change state") += count;
        self.stdin_tape.extend(events);
    }

    pub fn stdin_tape(&self) -> &[StdinEvent] {
        &self.stdin_tape
    }

    pub fn stmt_stdin_event_count(&self, index: usize) -> usize {
        self.stmt_stdin_events.get(index).copied().unwrap_or(0)
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

    /// Whether source may affect state, treating retained functions whose
    /// bodies only call other known-pure retained functions as pure.
    pub fn may_have_side_effects(&self, source: &str) -> bool {
        let functions = self
            .file_items
            .iter()
            .filter_map(|item| lex::function_definition(item))
            .collect::<Vec<_>>();
        let mut pure = functions
            .iter()
            .map(|(name, _)| *name)
            .collect::<std::collections::BTreeSet<_>>();

        loop {
            let impure = functions
                .iter()
                .filter_map(|(name, body)| {
                    lex::may_have_side_effects_with_pure_calls(body, |called| pure.contains(called))
                        .then_some(*name)
                })
                .collect::<Vec<_>>();
            let old_len = pure.len();
            pure.retain(|name| !impure.contains(name));
            if pure.len() == old_len {
                break;
            }
        }

        lex::may_have_side_effects_with_pure_calls(source, |called| pure.contains(called))
    }

    pub fn mark_external_replay_warning_shown(&mut self) {
        self.external_replay_warning_shown = true;
    }

    /// Whether an expression has unambiguous UTF-8 source spelling.
    ///
    /// C23 makes `char8_t` the same type as `unsigned char`, so the generated
    /// C program cannot recover the typedef spelling through `_Generic`.
    /// Retained declarations are searched from the innermost/latest binding
    /// back to file scope; a later declaration of another type stops the
    /// search and prevents a stale UTF-8 classification.
    pub fn is_explicit_utf8_array_expr(&self, expression: &str) -> bool {
        if lex::is_explicit_u8_string(expression) {
            return true;
        }
        let Some(name) = lex::plain_identifier(expression) else {
            return false;
        };
        for source in self.stmts.iter().rev().chain(self.file_items.iter().rev()) {
            if let Some(is_utf8) = lex::explicit_utf8_array_declaration(source, name) {
                return is_utf8;
            }
        }
        false
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
    fn explicit_utf8_binding_respects_shadowing() {
        let mut session = Session::default();
        session.commit(r#"constexpr char8_t text[] = u8"hello";"#, Slot::Stmt);
        assert!(session.is_explicit_utf8_array_expr("text"));
        assert!(session.is_explicit_utf8_array_expr(r#"u8"direct""#));
        assert!(!session.is_explicit_utf8_array_expr("text + 1"));

        session.commit_scoped("unsigned char text[] = { 1, 2, 0 };");
        assert!(!session.is_explicit_utf8_array_expr("text"));
    }

    #[test]
    fn recursive_pure_functions_do_not_count_as_side_effects() {
        let mut session = Session::default();
        session.commit(
            "long long fib(int n) { if (n <= 1) return n; return fib(n - 1) + fib(n - 2); }",
            Slot::FileScope,
        );
        assert!(!session.may_have_side_effects("fib(8)"));

        session.commit(
            "int impure(void) { static int n; return ++n; }",
            Slot::FileScope,
        );
        assert!(session.may_have_side_effects("impure()"));
        assert!(session.may_have_side_effects("unknown()"));
    }
}
