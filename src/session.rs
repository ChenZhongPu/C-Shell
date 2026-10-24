//! Accumulated session state: everything typed so far that compiled.

use crate::codegen::Slot;
use crate::lex;
use crate::proc::StdinEvent;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_REVISION: AtomicU64 = AtomicU64::new(1);

#[derive(Default, Clone)]
pub struct Session {
    /// `#include`s, `#define`s, function definitions, type definitions.
    file_items: Vec<String>,
    /// Statements and block-scope declarations, in order, inside `main`.
    /// A literal `{` entry begins each compiler-approved shadowing epoch;
    /// codegen closes all such scopes after the newest input.
    stmts: Vec<String>,
    revision: u64,
    names: BTreeMap<String, usize>,
    macros: BTreeMap<String, usize>,
    pure_functions: std::cell::OnceCell<BTreeSet<String>>,
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
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn file_items(&self) -> &[String] {
        &self.file_items
    }
    pub fn stmts(&self) -> &[String] {
        &self.stmts
    }

    pub fn uses_stdin_tape(&self) -> bool {
        lex::STDIN_APIS
            .iter()
            .any(|name| self.names.contains_key(*name))
    }

    fn changed(&mut self) {
        self.revision = NEXT_REVISION.fetch_add(1, Ordering::Relaxed);
    }

    fn index(&mut self, text: &str, file: bool, insert: bool) {
        for name in lex::identifiers(text)
            .into_iter()
            .filter(|name| name.len() >= 2)
        {
            update_count(&mut self.names, name, insert);
        }
        if file {
            for name in lex::defined_macros(text) {
                update_count(&mut self.macros, name, insert);
            }
            self.pure_functions.take();
        }
    }
    /// Record an input that compiled and ran.
    ///
    /// A pure expression is evaluated and forgotten: it asked a question
    /// rather than changing anything, so replaying it would only make every
    /// later evaluation slower. Anything that might mutate state is kept.
    pub fn commit(&mut self, text: &str, slot: Slot) {
        let stored = match slot {
            Slot::FileScope => {
                self.index(text, true, true);
                self.file_items.push(text.to_string());
                self.changed();
                return;
            }
            Slot::Stmt => text.to_string(),
            Slot::Expr if self.must_replay(text) => {
                // The cast keeps the side effects without tripping a
                // warning about a computed value going unused.
                format!("(void)({text});")
            }
            Slot::Expr => return,
        };
        self.index(&stored, false, true);
        self.stmts.push(indent(&stored));
        self.stmt_stdin_events.push(0);
        self.changed();
    }

    /// Macro expansion can hide both calls and mutations from the lexical
    /// heuristic. Treat any retained macro reference conservatively.
    pub fn references_macro(&self, text: &str) -> bool {
        lex::identifiers(text)
            .iter()
            .any(|name| self.macros.contains_key(name))
    }

    pub fn must_replay(&self, text: &str) -> bool {
        lex::may_have_side_effects(text) || self.references_macro(text)
    }

    /// Fresh stdin events are runtime evidence that the expression must be
    /// retained, including macros originating in a user header.
    pub fn commit_with_stdin(&mut self, text: &str, slot: Slot, events: Vec<StdinEvent>) {
        if slot == Slot::Expr && !events.is_empty() {
            self.commit(&format!("(void)({text});"), Slot::Stmt);
        } else {
            self.commit(text, slot);
        }
        self.attach_stdin_events(events);
    }

    /// Record a declaration that compiled only after entering a nested block.
    /// Every later statement remains in that block until codegen closes it at
    /// the end of `main`, giving C shadowing the shape of REPL rebinding.
    pub fn commit_scoped(&mut self, text: &str) {
        self.index(text, false, true);
        self.stmts.push(indent("{"));
        self.stmt_stdin_events.push(0);
        self.stmts.push(indent(text));
        self.stmt_stdin_events.push(0);
        self.scope_depth += 1;
        self.changed();
    }

    /// Replace a compiler-selected file-scope item in place. Keeping its index
    /// preserves declaration order for older functions/types that refer to it.
    pub fn replace_file(&mut self, index: usize, text: &str) {
        let old = std::mem::replace(&mut self.file_items[index], text.to_string());
        self.index(&old, true, false);
        self.index(text, true, true);
        self.changed();
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
        self.changed();
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
        if self.references_macro(source) {
            return true;
        }
        let pure = self.pure_functions.get_or_init(|| {
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
                        (self.references_macro(body)
                            || lex::may_have_side_effects_with_pure_calls(body, |called| {
                                pure.contains(called)
                            }))
                        .then_some(*name)
                    })
                    .collect::<Vec<_>>();
                let old_len = pure.len();
                pure.retain(|name| !impure.contains(name));
                if pure.len() == old_len {
                    break;
                }
            }

            pure.into_iter().map(str::to_string).collect()
        });
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
        self.names.keys().cloned().collect()
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
        self.names.clear();
        self.macros.clear();
        self.pure_functions.take();
        self.changed();
    }
}

fn update_count(index: &mut BTreeMap<String, usize>, name: String, insert: bool) {
    if insert {
        *index.entry(name).or_default() += 1;
    } else if let Some(count) = index.get_mut(&name) {
        *count -= 1;
        if *count == 0 {
            index.remove(&name);
        }
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
    fn incremental_indexes_follow_replacement_shadowing_and_reset() {
        let mut session = Session::default();
        session.commit("#define OLD_MACRO shared_name", Slot::FileScope);
        session.commit("int shared_name = 1;", Slot::Stmt);
        let before = session.revision();
        session.replace_file(0, "#define NEW_MACRO other_name");
        assert_ne!(before, session.revision());
        assert!(!session.references_macro("OLD_MACRO"));
        assert!(session.references_macro("NEW_MACRO"));
        assert!(session.identifiers().contains(&"shared_name".to_string()));
        assert!(!session.identifiers().contains(&"OLD_MACRO".to_string()));
        session.commit_scoped("int inner_name = 2;");
        let expected = session
            .file_items
            .iter()
            .chain(session.stmts.iter())
            .flat_map(|source| lex::identifiers(source))
            .filter(|name| name.len() >= 2)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        assert_eq!(session.identifiers(), expected);
        let mut branch = session.clone();
        session.commit("int left = 1;", Slot::Stmt);
        branch.commit("int right = 1;", Slot::Stmt);
        assert_ne!(
            session.revision(),
            branch.revision(),
            "divergent cloned sessions shared a cache revision"
        );
        session.reset();
        assert!(session.identifiers().is_empty());
        assert!(!session.references_macro("NEW_MACRO"));
    }

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
