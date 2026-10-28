//! Bind preprocessed step references to persistent storage. Preprocessing first
//! makes references hidden by user/header macros visible, while the syntax tree
//! distinguishes members, declarators and locally shadowed names.

use std::collections::HashMap;
use tree_sitter::{Node, Parser};

pub(super) fn rewrite(source: &str, step: usize, bindings: &[(String, String)]) -> String {
    if bindings.is_empty() {
        return source.to_string();
    }
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_c::LANGUAGE.into())
        .expect("C grammar");
    let tree = parser.parse(source, None).expect("C syntax tree");
    let name = format!("cs_step_{step}");
    let mut cursor = tree.root_node().walk();
    let entry = tree.root_node().named_children(&mut cursor).find(|node| {
        node.kind() == "function_definition"
            && node
                .child_by_field_name("declarator")
                .and_then(declared_name)
                .is_some_and(|id| source[id.byte_range()] == name)
    });
    let Some(body) = entry.and_then(|node| node.child_by_field_name("body")) else {
        return source.to_string();
    };
    let mut resolver = Resolver {
        source,
        bindings: bindings
            .iter()
            .map(|(p, n)| (p.as_str(), n.as_str()))
            .collect(),
        scopes: vec![HashMap::new()],
        edits: Vec::new(),
    };
    resolver.visit(body);
    let mut result = source.to_string();
    for (start, end, text) in resolver.edits.into_iter().rev() {
        result.replace_range(start..end, &text);
    }
    result
}

fn declared_name(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        if matches!(node.kind(), "identifier" | "type_identifier") {
            return Some(node);
        }
        node = node
            .child_by_field_name("declarator")
            .or_else(|| node.named_child(0))?;
    }
}

struct Resolver<'a> {
    source: &'a str,
    bindings: HashMap<&'a str, &'a str>,
    /// None means a local declaration hides a persistent name.
    scopes: Vec<HashMap<String, Option<String>>>,
    edits: Vec<(usize, usize, String)>,
}

impl Resolver<'_> {
    fn bind(&mut self, name: Node<'_>) {
        let name = &self.source[name.byte_range()];
        let scope = self.scopes.last_mut().expect("scope");
        scope.insert(name.to_string(), None);
        if let Some(original) = self.bindings.get(name) {
            scope.insert((*original).to_string(), Some(format!("(*{name})")));
        }
    }

    fn children(&mut self, node: Node<'_>) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.visit(child);
        }
    }

    fn declarator(&mut self, node: Node<'_>, name: Option<Node<'_>>) {
        if Some(node) == name {
            return;
        }
        if node.kind() == "parameter_list" {
            self.scopes.push(HashMap::new());
            self.children(node);
            self.scopes.pop();
            return;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if Some(child) == name
                || child.child_by_field_name("declarator").is_some()
                || matches!(child.kind(), "parameter_list" | "parenthesized_declarator")
            {
                self.declarator(child, name);
            } else {
                self.visit(child);
            }
        }
    }

    fn declaration(&mut self, node: Node<'_>) {
        let mut cursor = node.walk();
        for (index, child) in node.children(&mut cursor).enumerate() {
            if node.field_name_for_child(index as u32) != Some("declarator") {
                if child.is_named() {
                    self.visit(child);
                }
                continue;
            }
            let decl = if child.kind() == "init_declarator" {
                child.child_by_field_name("declarator").unwrap_or(child)
            } else {
                child
            };
            let name = declared_name(decl);
            // An object's scope begins after its declarator, before its init.
            self.declarator(decl, name);
            if let Some(name) = name {
                self.bind(name);
            }
            if let Some(value) = child.child_by_field_name("value") {
                self.visit(value);
            }
        }
    }

    fn visit(&mut self, node: Node<'_>) {
        match node.kind() {
            "compound_statement" | "for_statement" => {
                self.scopes.push(HashMap::new());
                self.children(node);
                self.scopes.pop();
            }
            "declaration" | "type_definition" | "parameter_declaration" => self.declaration(node),
            "enumerator" => {
                if let Some(name) = node.child_by_field_name("name") {
                    self.bind(name);
                }
                if let Some(value) = node.child_by_field_name("value") {
                    self.visit(value);
                }
            }
            "identifier" => {
                let name = &self.source[node.byte_range()];
                if let Some(Some(replacement)) =
                    self.scopes.iter().rev().find_map(|scope| scope.get(name))
                {
                    self.edits
                        .push((node.start_byte(), node.end_byte(), replacement.clone()));
                }
            }
            // Members and labels have separate C namespaces.
            "field_identifier" | "statement_identifier" | "type_identifier" => {}
            _ => self.children(node),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binds_storage_and_preserves_member_and_local_names() {
        let source = "void cs_step_0(void **cs_slot) { int (*cs_p0) = cs_slot[0]; x++; p.x++; { int x = 9; x++; } x++; }";
        assert_eq!(
            rewrite(source, 0, &[("cs_p0".into(), "x".into())]),
            "void cs_step_0(void **cs_slot) { int (*cs_p0) = cs_slot[0]; (*cs_p0)++; p.x++; { int x = 9; x++; } (*cs_p0)++; }"
        );
    }

    #[test]
    fn binds_inferred_array_storage() {
        let src = "void cs_step_0(void **slots) { int (*cs_p0)[sizeof((int[]){1, 2}) / sizeof(((int[]){1, 2})[0])]; cs_p0 = slots[0]; int *p = &a[1]; }";
        let got = rewrite(src, 0, &[("cs_p0".into(), "a".into())]);
        assert!(got.contains("&(*cs_p0)[1]"), "{got}");
    }
}
