//! Declaration analysis for the resident (Step-DLL) backend.
//!
//! The resident runner keeps every session variable in a heap slot and reaches
//! it from later steps through a pointer such as `(*cs_p3)`. To declare
//! `cs_p3` with the right type without ever reconstructing that type, the
//! declarator is reused: replacing its identifier with `(*cs_p3)` turns
//! `int a[5]` into `int (*cs_p3)[5]` and `int (*fp)(double)` into
//! `int (*(*cs_p3))(double)`. A real C parser is therefore needed only to find
//! declarators, their names, and what kind of top-level construct an input is.

use tree_sitter::{Node, Parser, Tree};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarDecl {
    pub name: String,
    /// Type specifiers and qualifiers, without storage classes or the body of
    /// an inline struct definition.
    pub declspec: String,
    /// The declarator without its initializer.
    pub declarator: String,
    /// Byte range of the identifier inside `declarator`.
    pub name_range: (usize, usize),
    /// Offset inside `declarator` where an inferred element count belongs
    /// (`int a[] = {…}`), if the outermost dimension is unsized.
    pub dim_insert: Option<usize>,
    /// The declarator with its initializer, as typed.
    pub init: String,
}

impl VarDecl {
    pub fn initializer(&self) -> Option<&str> {
        self.init
            .strip_prefix(&self.declarator)?
            .trim()
            .strip_prefix('=')
            .map(str::trim)
    }

    /// The original declaration with only its declared name changed.
    pub fn init_decl(&self, name: &str) -> String {
        let mut init = self.init.clone();
        init.replace_range(self.name_range.0..self.name_range.1, name);
        format!("{} {init}", self.declspec)
    }

    /// A complete pointer type even for an array whose bound is inferred.
    /// `sizeof` does not evaluate a non-VLA compound literal's initializer.
    pub fn storage_pointer_decl(&self, pointer: &str) -> String {
        if self.dim_insert.is_none() {
            return self.pointer_decl(pointer, None);
        }
        let mut abstract_decl = self.declarator.clone();
        abstract_decl.replace_range(self.name_range.0..self.name_range.1, "");
        let init = self.initializer().unwrap_or("");
        let init = if init.starts_with('{') {
            init.to_string()
        } else {
            format!("{{{init}}}")
        };
        let literal = format!("({} {abstract_decl}){init}", self.declspec);
        let mut declarator = self.declarator.clone();
        declarator.insert_str(
            self.dim_insert.expect("inferred dimension"),
            &format!("sizeof({literal}) / sizeof(({literal})[0])"),
        );
        declarator.replace_range(
            self.name_range.0..self.name_range.1,
            &format!("(*{pointer})"),
        );
        format!("{} {declarator}", self.declspec)
    }

    /// Declaration text that makes `pointer` point at an object of this
    /// variable's type. `dim` completes an unsized outermost array dimension.
    pub fn pointer_decl(&self, pointer: &str, dim: Option<usize>) -> String {
        let mut declarator = self.declarator.clone();
        if let (Some(at), Some(count)) = (self.dim_insert, dim) {
            declarator.insert_str(at, &count.to_string());
        }
        let (start, end) = self.name_range;
        // Inserting a dimension happens after the identifier, so the identifier
        // range is still valid.
        declarator.replace_range(start..end, &format!("(*{pointer})"));
        format!("{} {}", self.declspec, declarator)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Preproc,
    Type,
    Other,
}

/// A construct that is re-included verbatim in every later translation unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileItem {
    pub kind: ItemKind,
    pub text: String,
    /// Macro names this item defines.
    pub defines: Vec<String>,
    /// Struct/union/enum tags, enumerators and typedef names it defines.
    pub types: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuncDef {
    pub name: String,
    /// `int f(int a)` without body or storage class, no semicolon.
    pub proto: String,
    /// Complete definition without storage class.
    pub text: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Plan {
    pub items: Vec<FileItem>,
    pub funcs: Vec<FuncDef>,
    pub vars: Vec<VarDecl>,
    /// A trailing `;` had to be appended for the input to parse.
    pub rewritten: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Analysis {
    Plan(Plan),
    /// An expression or statement list: compiled inside the step function.
    Statement,
    Unsupported(String),
}

fn parse_clean(text: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_c::LANGUAGE.into()).ok()?;
    let tree = parser.parse(text, None)?;
    (!tree.root_node().has_error()).then_some(tree)
}

pub fn analyze(input: &str) -> Analysis {
    // Preprocessor directives only parse when terminated by a newline.
    let trimmed = input.trim();
    let line = format!("{trimmed}\n");
    let (tree, src, rewritten) = match parse_clean(&line) {
        Some(tree) => (tree, line, false),
        None => {
            let with = format!("{trimmed};\n");
            match parse_clean(&with) {
                Some(tree) => (tree, with, true),
                None => return Analysis::Statement,
            }
        }
    };
    let mut plan = Plan {
        rewritten,
        ..Plan::default()
    };
    let mut statements = 0usize;
    let root = tree.root_node();
    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor) {
        let result = match node.kind() {
            "comment" => Ok(()),
            kind if kind.starts_with("preproc_") => {
                plan.items.push(preproc_item(&node, &src));
                Ok(())
            }
            "struct_specifier" | "union_specifier" | "enum_specifier" => {
                plan.items.push(FileItem {
                    kind: ItemKind::Type,
                    text: format!("{};", text(&node, &src)),
                    defines: Vec::new(),
                    types: specifier_names(&node, &src),
                });
                Ok(())
            }
            "type_definition" => {
                plan.items.push(typedef_item(&node, &src));
                Ok(())
            }
            "function_definition" => function(&node, &src, &mut plan),
            "declaration" => declaration(&node, &src, &mut plan),
            _ => {
                statements += 1;
                Ok(())
            }
        };
        if let Err(reason) = result {
            return Analysis::Unsupported(reason);
        }
    }
    let declared = !plan.items.is_empty() || !plan.funcs.is_empty() || !plan.vars.is_empty();
    if statements > 0 && declared {
        return Analysis::Unsupported(
            "enter declarations and statements as separate inputs".to_string(),
        );
    }
    if !declared {
        return Analysis::Statement;
    }
    Analysis::Plan(plan)
}

fn text<'a>(node: &Node, src: &'a str) -> &'a str {
    &src[node.byte_range()]
}

fn preproc_item(node: &Node, src: &str) -> FileItem {
    let body = text(node, src).trim_end().to_string();
    FileItem {
        kind: ItemKind::Preproc,
        defines: crate::lex::defined_macros(&body),
        types: Vec::new(),
        text: body,
    }
}

/// Tags and enumerators a type specifier defines (only when it has a body).
fn specifier_names(node: &Node, src: &str) -> Vec<String> {
    let mut names = Vec::new();
    if !matches!(
        node.kind(),
        "struct_specifier" | "union_specifier" | "enum_specifier"
    ) {
        return names;
    }
    if let Some(name) = node.child_by_field_name("name") {
        // Tags live in their own namespace, apart from typedef names.
        let keyword = node.kind().trim_end_matches("_specifier");
        names.push(format!("{keyword} {}", text(&name, src)));
    }
    if node.kind() == "enum_specifier"
        && let Some(body) = node.child_by_field_name("body")
    {
        let mut cursor = body.walk();
        for enumerator in body.named_children(&mut cursor) {
            if let Some(name) = enumerator.child_by_field_name("name") {
                names.push(text(&name, src).to_string());
            }
        }
    }
    names
}

fn typedef_item(node: &Node, src: &str) -> FileItem {
    let mut types = Vec::new();
    if let Some(ty) = node.child_by_field_name("type") {
        types.extend(specifier_names(&ty, src));
    }
    let mut cursor = node.walk();
    for declarator in node.children_by_field_name("declarator", &mut cursor) {
        if let Some((name, _)) = find_name(&declarator) {
            types.push(text(&name, src).to_string());
        }
    }
    FileItem {
        kind: ItemKind::Type,
        text: text(node, src).trim_end().to_string(),
        defines: Vec::new(),
        types,
    }
}

/// The identifier a declarator declares and the kind of its direct parent.
fn find_name<'t>(declarator: &Node<'t>) -> Option<(Node<'t>, &'static str)> {
    let mut current = *declarator;
    let mut parent = "";
    loop {
        if matches!(
            current.kind(),
            "identifier" | "type_identifier" | "field_identifier"
        ) {
            return Some((current, static_kind(parent)));
        }
        parent = current.kind();
        current = current
            .child_by_field_name("declarator")
            .or_else(|| current.named_child(0))?;
    }
}

fn static_kind(kind: &str) -> &'static str {
    match kind {
        "function_declarator" => "function_declarator",
        "array_declarator" => "array_declarator",
        "pointer_declarator" => "pointer_declarator",
        "parenthesized_declarator" => "parenthesized_declarator",
        _ => "",
    }
}

/// Offset (relative to `declarator`) just after the `[` of an unsized array
/// dimension in the declarator chain.
fn empty_dimension(declarator: &Node) -> Option<usize> {
    let mut current = *declarator;
    loop {
        if current.kind() == "array_declarator" && current.child_by_field_name("size").is_none() {
            let mut cursor = current.walk();
            let open = current.children(&mut cursor).find(|c| c.kind() == "[");
            return open.map(|open| open.end_byte() - declarator.start_byte());
        }
        current = current
            .child_by_field_name("declarator")
            .or_else(|| current.named_child(0))?;
    }
}

fn strip_ranges(src: &str, start: usize, end: usize, mut cuts: Vec<(usize, usize)>) -> String {
    cuts.sort_by_key(|range| std::cmp::Reverse(range.0));
    let mut out = src[start..end].to_string();
    for (cut_start, cut_end) in cuts {
        if cut_start >= start && cut_end <= end {
            out.replace_range(cut_start - start..cut_end - start, "");
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn storage_cuts(node: &Node, limit: usize) -> Vec<(usize, usize)> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| child.kind() == "storage_class_specifier" && child.end_byte() <= limit)
        .map(|child| (child.start_byte(), child.end_byte()))
        .collect()
}

fn function(node: &Node, src: &str, plan: &mut Plan) -> Result<(), String> {
    let declarator = node
        .child_by_field_name("declarator")
        .ok_or("unsupported function definition")?;
    let body = node
        .child_by_field_name("body")
        .ok_or("unsupported function definition")?;
    let (name, _) = find_name(&declarator).ok_or("unsupported function definition")?;
    let cuts = storage_cuts(node, body.start_byte());
    let proto = strip_ranges(src, node.start_byte(), body.start_byte(), cuts);
    let text = format!("{proto} {}", self::text(&body, src));
    plan.funcs.push(FuncDef {
        name: self::text(&name, src).to_string(),
        proto,
        text,
    });
    Ok(())
}

fn declaration(node: &Node, src: &str, plan: &mut Plan) -> Result<(), String> {
    let mut cursor = node.walk();
    let declarators: Vec<Node> = node
        .children_by_field_name("declarator", &mut cursor)
        .collect();
    let type_node = node.child_by_field_name("type");
    if declarators.is_empty() {
        let types = type_node
            .map(|ty| specifier_names(&ty, src))
            .unwrap_or_default();
        plan.items.push(FileItem {
            kind: ItemKind::Type,
            text: text(node, src).trim_end().to_string(),
            defines: Vec::new(),
            types,
        });
        return Ok(());
    }

    let functions = declarators
        .iter()
        .filter(|d| {
            let target = if d.kind() == "init_declarator" {
                d.child_by_field_name("declarator")
            } else {
                Some(**d)
            };
            target
                .and_then(|t| find_name(&t))
                .is_some_and(|(_, parent)| parent == "function_declarator")
        })
        .count();
    if functions > 0 {
        if functions != declarators.len() {
            return Err("enter function prototypes and variables as separate inputs".to_string());
        }
        plan.items.push(FileItem {
            kind: ItemKind::Other,
            text: text(node, src).trim_end().to_string(),
            defines: Vec::new(),
            types: Vec::new(),
        });
        return Ok(());
    }

    let first = declarators[0].start_byte();
    let mut cuts = storage_cuts(node, first);
    if let Some(ty) = type_node
        && let Some(body) = ty.child_by_field_name("body")
        && matches!(
            ty.kind(),
            "struct_specifier" | "union_specifier" | "enum_specifier"
        )
    {
        if ty.child_by_field_name("name").is_none() {
            return Err("variables of an anonymous struct, union or enum type are not supported; give the type a tag or a typedef name".to_string());
        }
        cuts.push((body.start_byte(), body.end_byte()));
        plan.items.push(FileItem {
            kind: ItemKind::Type,
            text: format!("{};", text(&ty, src)),
            defines: Vec::new(),
            types: specifier_names(&ty, src),
        });
    }
    let declspec = strip_ranges(src, node.start_byte(), first, cuts);

    for declarator in declarators {
        let inner = if declarator.kind() == "init_declarator" {
            declarator
                .child_by_field_name("declarator")
                .ok_or("unsupported declaration")?
        } else {
            declarator
        };
        let (name, _) = find_name(&inner).ok_or("unsupported declaration")?;
        let base = inner.start_byte();
        plan.vars.push(VarDecl {
            name: text(&name, src).to_string(),
            declspec: declspec.clone(),
            declarator: text(&inner, src).to_string(),
            name_range: (name.start_byte() - base, name.end_byte() - base),
            dim_insert: empty_dimension(&inner),
            init: text(&declarator, src).to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(input: &str) -> Plan {
        match analyze(input) {
            Analysis::Plan(plan) => plan,
            other => panic!("expected a plan for {input:?}, got {other:?}"),
        }
    }

    #[test]
    fn scalars_and_arrays_get_pointer_declarations() {
        let p = plan("int x = 42;");
        assert_eq!(p.vars.len(), 1);
        assert_eq!(p.vars[0].name, "x");
        assert_eq!(p.vars[0].pointer_decl("cs_p0", None), "int (*cs_p0)");

        let p = plan("static const char *names[3] = {\"a\", \"b\", \"c\"};");
        assert_eq!(
            p.vars[0].pointer_decl("cs_p1", None),
            "const char (*(*cs_p1)[3])".replace("(*(*cs_p1)[3])", "*(*cs_p1)[3]")
        );
    }

    #[test]
    fn function_pointers_and_multiple_declarators() {
        let p = plan("int (*fp)(double) = 0, y;");
        assert_eq!(p.vars.len(), 2);
        assert_eq!(
            p.vars[0].pointer_decl("cs_p2", None),
            "int (*(*cs_p2))(double)"
        );
        assert_eq!(p.vars[1].name, "y");
    }

    #[test]
    fn unsized_arrays_report_where_the_count_goes() {
        let p = plan("int a[] = {1, 2, 3};");
        assert!(p.vars[0].dim_insert.is_some());
        assert_eq!(p.vars[0].pointer_decl("cs_p3", Some(3)), "int (*cs_p3)[3]");
        let p = plan("char m[][4] = {\"ab\", \"cd\"};");
        assert_eq!(
            p.vars[0].pointer_decl("cs_p4", Some(2)),
            "char (*cs_p4)[2][4]"
        );
    }

    #[test]
    fn inline_struct_definitions_split_into_type_and_variable() {
        let p = plan("struct P { int x; int y; } p = {1, 2};");
        assert_eq!(p.items.len(), 1);
        assert_eq!(p.items[0].types, vec!["struct P".to_string()]);
        assert_eq!(p.vars[0].declspec, "struct P");
        assert!(matches!(
            analyze("struct { int x; } anon;"),
            Analysis::Unsupported(_)
        ));
    }

    #[test]
    fn functions_types_and_preprocessor_lines_are_file_items() {
        let p = plan("static int add(int a, int b) { return a + b; }");
        assert_eq!(p.funcs[0].name, "add");
        assert_eq!(p.funcs[0].proto, "int add(int a, int b)");
        let p = plan("typedef struct Node { int v; } Node;");
        assert!(p.items[0].types.contains(&"Node".to_string()));
        assert!(p.items[0].types.contains(&"struct Node".to_string()));
        let p = plan("#define N 10");
        assert_eq!(p.items[0].defines, vec!["N".to_string()]);
        let p = plan("int twice(int);");
        assert_eq!(p.items[0].kind, ItemKind::Other);
        let p = plan("enum Color { RED, GREEN };");
        assert_eq!(
            p.items[0].types,
            vec![
                "enum Color".to_string(),
                "RED".to_string(),
                "GREEN".to_string()
            ]
        );
    }

    #[test]
    fn expressions_and_statements_stay_in_the_step() {
        assert_eq!(analyze("1 + 1"), Analysis::Statement);
        assert_eq!(analyze("x = x + 1;"), Analysis::Statement);
        assert_eq!(
            analyze("for (int i = 0; i < 3; i++) puts(\"a\");"),
            Analysis::Statement
        );
        assert!(matches!(
            analyze("int a = 1; a++;"),
            Analysis::Unsupported(_)
        ));
    }

    #[test]
    fn a_missing_semicolon_is_repaired_for_declarations() {
        let p = plan("int x = 5");
        assert!(p.rewritten);
        assert_eq!(p.vars[0].name, "x");
    }
}
