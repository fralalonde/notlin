//! One shared receiver-binding inference for retained getter repair and for the
//! value a Kotlin stdlib scope function binds to its block parameter.
//!
//! A type hint is not a binding. Every answer here is one of three things:
//! `Bound` (a proved declaration or expression type), `Unknown` (something owns
//! the name but its type is unproved), or `Unbound` (nothing in the lexical
//! chain owns it, so only an outer implicit receiver or the file-wide table
//! could). `Unknown` never degrades into `Unbound`: the caller must not consult
//! an outer class property or a global hint that a nearer declaration, an
//! opaque implicit receiver or an open ancestor could override.
//!
//! The lexical walk lives in `property_callsite` and is the single consumer of
//! this module for both proofs. A lambda whose call is not the canonical stdlib
//! value-scope function introduces an implicit receiver: its members shadow the
//! enclosing class, and a member extension of that receiver can even take the
//! `let` name, so the *call namespace* inside such a scope stays unproved.
//! Explicitly typed locals and parameters stay authoritative for direct
//! receiver reads across that barrier; they do not prove call identity.
//!
//! The stdlib proof itself remains structural: exactly a selected callee with
//! one trailing block, the receiver's source hierarchy does not declare the
//! name, no enclosing declaration inherits one, no import or file declaration
//! rebinds it, and the block is not itself nested in an opaque receiver scope.
//! Testing the spelling instead would accept a locally shadowed `let` that
//! binds an entirely different value.

use crate::semantics::java_member_calls::import_header;
use crate::transpiler::kt;
use crate::workspace::{Declaration, MemberKind, SourceFile, SourceIndex};
use std::collections::HashSet;
use tree_sitter::Node;

/// Packages a wildcard import cannot use to shadow a value-scope name: they are
/// already visible through Kotlin's default imports, so importing them again
/// widens nothing.
const DEFAULT_IMPORT_PACKAGES: [&str; 10] = [
    "kotlin",
    "kotlin.annotation",
    "kotlin.collections",
    "kotlin.comparisons",
    "kotlin.io",
    "kotlin.jvm",
    "kotlin.ranges",
    "kotlin.sequences",
    "kotlin.text",
    "java.lang",
];

/// The stdlib functions whose single block parameter receives the call's
/// receiver as a plain value.
const VALUE_SCOPE: [&str; 4] = ["let", "also", "takeIf", "takeUnless"];

/// How a `Bound` type was established.
///
/// `Canonical` is a declared source type or an indexed workspace type.
/// `Legacy` is the older spelling-driven inference: the sequence element hint
/// for a `for` loop and the file-wide binding table. A legacy type still answers
/// a *direct* receiver read, which is the existing migration, but it is not
/// proof of a value's type, so the stdlib value-scope producer never consumes
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Evidence {
    Canonical,
    Legacy,
}

/// What a name or expression denotes at one position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolution {
    /// A declaration or expression with a type, plus how it was proved.
    Bound { ty: String, evidence: Evidence },
    /// A nearer declaration or implicit receiver owns the name, but its type is
    /// not proved. Outer class properties and file-wide hints are barred.
    Unknown,
    /// Nothing in the lexical chain owns the name.
    Unbound,
}

/// One shared answer, plus whether an opaque implicit receiver was crossed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Lookup {
    pub resolution: Resolution,
    /// An enclosing lambda or anonymous object whose implicit receiver was not
    /// proved transparent. Direct receiver reads may still use a lexical
    /// binding, but the call namespace inside this scope is not closed.
    pub opaque_receiver: bool,
    /// A lexical binder (parameter, local, loop variable, lambda parameter)
    /// answered. Only such a name is *shadowed*; the enclosing member and the
    /// file-wide table answer the type of an implicit receiver, not a shadow.
    pub lexical: bool,
}

impl Lookup {
    pub(crate) fn bound(ty: String, opaque_receiver: bool) -> Self {
        Self::with_evidence(ty, Evidence::Canonical, opaque_receiver)
    }

    /// The legacy inference answered: a direct read may use the type, but no
    /// proof may consume it as evidence.
    pub(crate) fn legacy_bound(ty: String, opaque_receiver: bool) -> Self {
        Self::with_evidence(ty, Evidence::Legacy, opaque_receiver)
    }

    fn with_evidence(ty: String, evidence: Evidence, opaque_receiver: bool) -> Self {
        Self {
            resolution: Resolution::Bound { ty, evidence },
            opaque_receiver,
            lexical: true,
        }
    }

    pub(crate) fn unknown(opaque_receiver: bool) -> Self {
        Self {
            resolution: Resolution::Unknown,
            opaque_receiver,
            lexical: true,
        }
    }

    pub(crate) fn unbound() -> Self {
        Self {
            resolution: Resolution::Unbound,
            opaque_receiver: false,
            lexical: false,
        }
    }

    /// No lexical binder answered and an opaque implicit receiver was crossed:
    /// the name is not shadowed, and no wider hint may answer either.
    pub(crate) fn opaque_unknown() -> Self {
        Self {
            resolution: Resolution::Unknown,
            opaque_receiver: true,
            lexical: false,
        }
    }

    /// An answer from the enclosing member or the file-wide table: a type for
    /// an implicit receiver, never a lexical shadow.
    pub(crate) fn implicit(resolution: Resolution) -> Self {
        Self {
            resolution,
            opaque_receiver: false,
            lexical: false,
        }
    }

    /// The file-wide binding table answered: a legacy hint for an implicit
    /// receiver, never a lexical binder and never canonical evidence.
    pub(crate) fn implicit_legacy(ty: String) -> Self {
        Self {
            resolution: Resolution::Bound {
                ty,
                evidence: Evidence::Legacy,
            },
            opaque_receiver: false,
            lexical: false,
        }
    }

    /// The same answer, recorded as a lexical binder's.
    pub(crate) fn lexical(mut self) -> Self {
        self.lexical = true;
        self
    }

    /// A nearer declaration owns the name, so a read of it is not this
    /// receiver's property.
    pub(crate) fn is_bound(&self) -> bool {
        self.lexical && !matches!(self.resolution, Resolution::Unbound)
    }

    pub(crate) fn ty(&self) -> Option<&str> {
        match &self.resolution {
            Resolution::Bound { ty, .. } => Some(ty),
            Resolution::Unknown | Resolution::Unbound => None,
        }
    }

    /// The type, only when a declared or indexed proof established it. This is
    /// the accessor the stdlib value-scope producer consumes; `ty` stays for
    /// the legacy direct receiver repairs.
    pub(crate) fn canonical_ty(&self) -> Option<&str> {
        match &self.resolution {
            Resolution::Bound {
                ty,
                evidence: Evidence::Canonical,
            } => Some(ty),
            _ => None,
        }
    }
}

/// Whether a declared type reference can be resolved in the file namespace.
/// Lexical type parameters and local/nested classifiers bind before that
/// namespace; a same-named indexed classifier is not their meaning. Bounds are
/// deliberately not guessed here. The caller must still resolve the nominal
/// candidate through the index before using it as receiver evidence.
pub(crate) fn file_scoped_type_reference(origin: Node<'_>, ty: &str, source: &str) -> bool {
    let head = ty
        .trim()
        .split('<')
        .next()
        .unwrap_or("")
        .trim_end_matches('?');
    let root = head.split('.').next().unwrap_or("").trim_matches('`');
    if root.is_empty() {
        return false;
    }
    let mut current = Some(origin);
    while let Some(scope) = current {
        if let Some(parameters) = kt::child(scope, "type_parameters") {
            for parameter in parameters.named_children(&mut parameters.walk()) {
                if parameter.kind() != "type_parameter" {
                    continue;
                }
                let Some(name) = parameter.child_by_field_name("name").or_else(|| {
                    parameter
                        .named_children(&mut parameter.walk())
                        .find(|child| child.kind() == "identifier")
                }) else {
                    return false;
                };
                if kt::text(name, source).trim_matches('`') == root {
                    return false;
                }
            }
        }
        if matches!(scope.kind(), "block" | "lambda_literal" | "class_body") {
            for declaration in scope.named_children(&mut scope.walk()) {
                if !matches!(
                    declaration.kind(),
                    "class_declaration" | "object_declaration" | "type_alias"
                ) {
                    continue;
                }
                if scope.kind() != "class_body" && declaration.start_byte() >= origin.start_byte() {
                    continue;
                }
                if declaration
                    .child_by_field_name("name")
                    .is_some_and(|name| kt::text(name, source).trim_matches('`') == root)
                {
                    return false;
                }
            }
        }
        current = scope.parent();
    }
    true
}

/// Indexed member types are resolved in their declaring owner's lexical type
/// scope, never in a consumer method's scope or a same-named file classifier.
///
/// `Declaration.type_params` only carries the owner's OWN parameters, so a
/// member typed by an enclosing class's parameter (an `inner class`) or by the
/// enclosing generic function's parameter (a local class) would otherwise pass
/// and its spelling would bind to whatever classifier carries that name. The
/// owner's exact syntax node is recovered from the indexed source text — the
/// same speculative overlay the index holds, never disk — and its full ancestor
/// chain decides. A missing source file, an ambiguous duplicate declaration
/// name, or a node that cannot be matched to the owner's cached symbol is not a
/// proof: the answer is `false`, so the caller emits no canonical evidence.
pub(crate) fn file_scoped_member_type(index: &SourceIndex, owner: &Declaration, ty: &str) -> bool {
    let Some(file) = index.declaration_source_file(owner) else {
        return false;
    };
    let source = file.source_text();
    let symbol = crate::semantics::workspace_symbol(index, owner);
    if symbol.kind.starts_with("ambiguous:") || symbol.name.starts_with('<') {
        return false;
    }
    let expected = symbol.stable_key();
    let tree = crate::transpiler::parse_tree(source);
    let mut declaring = None;
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if crate::semantics::declaration_node_kind(node.kind())
            && crate::semantics::symbol_id_for_node(source, node, &file.path).stable_key()
                == expected
        {
            // A second node with the owner's exact identity means the source
            // does not say which declaration this member belongs to.
            if declaring.is_some() {
                return false;
            }
            declaring = Some(node);
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    let Some(declaring) = declaring else {
        return false;
    };
    file_scoped_type_reference(declaring, ty, source)
}

/// The caller's canonical receiver inference, reused by every proof here.
pub(crate) trait ReceiverTypes {
    fn lookup(&self, node: Node<'_>) -> Lookup;
}

/// What one lambda does with a name, from its own binders only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Binding<'t> {
    /// This lambda's parameter list and implicit `it` do not bind the name.
    Unbound,
    /// This lambda binds the name, but no value type could be proved.
    Unknown,
    /// The parameter list declares `name: Type`.
    Bound(String),
    /// A proven stdlib value-scope call binds `name` to this receiver.
    Value(Node<'t>),
}

/// The binding one lambda establishes, with its exact source span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LambdaInput<'t> {
    pub binding: Binding<'t>,
    pub span: (usize, usize),
}

/// How a lambda changes what a bare identifier means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Scope<'t> {
    /// A proven stdlib value-scope block: it binds only its parameter and
    /// introduces no implicit receiver.
    Value(Node<'t>),
    /// Every other block — a receiver lambda, a member or extension call, an
    /// anonymous object, or an unproved call shape — gives the block an
    /// implicit receiver that can own a bare name, including a member
    /// extension named like the stdlib function.
    Opaque,
}

/// What `name` is bound to inside `lambda`.
pub(crate) fn binding<'t>(
    index: &SourceIndex,
    file: &SourceFile,
    source: &str,
    lambda: Node<'t>,
    name: &str,
    receiver_type: &dyn ReceiverTypes,
) -> LambdaInput<'t> {
    let span = (lambda.start_byte(), lambda.end_byte());
    let unbound = LambdaInput {
        binding: Binding::Unbound,
        span,
    };
    let unknown = LambdaInput {
        binding: Binding::Unknown,
        span,
    };
    let Some(parameters) = lambda
        .named_children(&mut lambda.walk())
        .find(|child| child.kind() == "lambda_parameters")
    else {
        // A lambda without a parameter list binds exactly one implicit name.
        if name != "it" {
            return unbound;
        }
        return match value_binding(index, file, source, lambda, receiver_type) {
            Some(receiver) => LambdaInput {
                span,
                binding: Binding::Value(receiver),
            },
            None => unknown,
        };
    };

    let declared = parameters
        .named_children(&mut parameters.walk())
        .collect::<Vec<_>>();
    let mut matched = None;
    let mut destructures = false;
    for parameter in &declared {
        match parameter_binding(*parameter, source) {
            Some((bound, ty)) if bound == name => matched = Some(ty),
            Some(_) => {}
            // A destructuring parameter can bind the name without exposing it as
            // a single identifier, so the lambda may own it after all.
            None => destructures = true,
        }
    }
    let Some(matched) = matched else {
        return if destructures { unknown } else { unbound };
    };
    // These functions take a single block parameter, so a wider parameter list
    // cannot be one of them and its value stays unproved.
    if declared.len() != 1 {
        return unknown;
    }
    match matched {
        ParameterType::Typed(ty) if file_scoped_type_reference(lambda, &ty, source) => {
            // A written declared type is canonical only through the same
            // import-aware binding consumers use; carry its qualified class.
            match crate::workspace::canonical_type_binding(index, file, &ty) {
                Some(binding) => LambdaInput {
                    span,
                    binding: Binding::Bound(binding.qualified),
                },
                None => unknown,
            }
        }
        ParameterType::Typed(_) => unknown,
        ParameterType::Nullable => unknown,
        ParameterType::Untyped => match value_binding(index, file, source, lambda, receiver_type) {
            Some(receiver) => LambdaInput {
                span,
                binding: Binding::Value(receiver),
            },
            None => unknown,
        },
    }
}

/// How `lambda` itself changes bare-identifier meaning, using the full
/// canonical call proof rather than the selected name alone.
pub(crate) fn scope<'t>(
    index: &SourceIndex,
    file: &SourceFile,
    source: &str,
    lambda: Node<'t>,
    receiver_type: &dyn ReceiverTypes,
) -> Scope<'t> {
    match value_receiver(index, file, source, lambda, receiver_type) {
        Some(receiver) => Scope::Value(receiver),
        None => Scope::Opaque,
    }
}

/// The implicit receiver a declaration itself installs, independent of any
/// lambda: an extension function's receiver and an extension property's
/// receiver can own a bare name or a selected call at their body position.
pub(crate) fn extension_receiver_scope(node: Node<'_>) -> bool {
    match node.kind() {
        "function_declaration" => function_has_extension_receiver(node),
        "property_declaration" => property_has_extension_receiver(node),
        _ => false,
    }
}

/// Whether entering `node` installs an implicit receiver for bare names and
/// selected calls. This is the ONE scope list: the lexical walk in
/// `property_callsite` and the call-environment proof below both read it, so a
/// typed local that answers before a barrier cannot hide that barrier.
pub(crate) fn implicit_receiver(node: Node<'_>) -> bool {
    matches!(node.kind(), "object_literal" | "anonymous_object") || extension_receiver_scope(node)
}

/// An extension function spells its receiver type before its name.
fn function_has_extension_receiver(function: Node<'_>) -> bool {
    let Some(name) = function.child_by_field_name("name") else {
        return true;
    };
    function.named_children(&mut function.walk()).any(|child| {
        child.end_byte() <= name.start_byte()
            && matches!(
                child.kind(),
                "user_type" | "nullable_type" | "function_type" | "parenthesized_type"
            )
    })
}

/// An extension property spells its receiver type before the property
/// declaration: `val Context.info: String`. The property's own name lives on
/// the `variable_declaration` child, so the extension shape is read from the
/// direct children rather than from a name field.
fn property_has_extension_receiver(property: Node<'_>) -> bool {
    let Some(variable) = kt::child(property, "variable_declaration") else {
        return false;
    };
    property.named_children(&mut property.walk()).any(|child| {
        child.id() != variable.id()
            && child.end_byte() <= variable.start_byte()
            && matches!(
                child.kind(),
                "user_type" | "nullable_type" | "function_type" | "parenthesized_type"
            )
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParameterType {
    Typed(String),
    /// A declared nullable type: `it.someProperty` would not compile on a
    /// nullable receiver, so a non-null call shape is not something this proof
    /// can assert.
    Nullable,
    Untyped,
}

fn parameter_binding(node: Node<'_>, source: &str) -> Option<(String, ParameterType)> {
    if node.kind() != "variable_declaration" {
        return None;
    }
    let mut name = None;
    let mut ty = None;
    let mut nullable = false;
    for child in node.named_children(&mut node.walk()) {
        match child.kind() {
            "identifier" if name.is_none() => {
                name = Some(kt::text(child, source).trim_matches('`').to_string());
            }
            "user_type" if ty.is_none() => {
                let declared = kt::text(child, source).trim().to_string();
                if !declared.is_empty() {
                    ty = Some(declared);
                }
            }
            "nullable_type" => nullable = true,
            _ => {}
        }
    }
    let ty = if nullable {
        ParameterType::Nullable
    } else {
        ty.map_or(ParameterType::Untyped, ParameterType::Typed)
    };
    Some((name?, ty))
}

/// The receiver of a proven value-scope call, or `None` when the call identity
/// (or the type of what it binds) is not proved.
fn value_binding<'t>(
    index: &SourceIndex,
    file: &SourceFile,
    source: &str,
    lambda: Node<'t>,
    receiver_type: &dyn ReceiverTypes,
) -> Option<Node<'t>> {
    value_receiver(index, file, source, lambda, receiver_type)
}

/// The receiver of the canonical stdlib value-scope call whose trailing block
/// is `lambda`, with its full environment proof.
fn value_receiver<'t>(
    index: &SourceIndex,
    file: &SourceFile,
    source: &str,
    lambda: Node<'t>,
    receiver_type: &dyn ReceiverTypes,
) -> Option<Node<'t>> {
    let mut argument = lambda;
    let mut call = lambda.parent()?;
    if call.kind() == "annotated_lambda" {
        argument = call;
        call = call.parent()?;
    }
    if call.kind() != "call_expression" {
        return None;
    }
    let named = call.named_children(&mut call.walk()).collect::<Vec<_>>();
    // Exactly a selected callee and the trailing block: no argument list, no
    // type arguments, no second argument.
    let [callee, block] = named.as_slice() else {
        return None;
    };
    if block.id() != argument.id() || callee.kind() != "navigation_expression" {
        return None;
    }
    let selected = callee
        .named_children(&mut callee.walk())
        .collect::<Vec<_>>();
    let [receiver, selected_name] = selected.as_slice() else {
        return None;
    };
    if receiver.kind() != "identifier" || selected_name.kind() != "identifier" {
        return None;
    }
    let name = kt::text(*selected_name, source);
    let name = name.trim_matches('`');
    if !VALUE_SCOPE.contains(&name) {
        return None;
    }
    if &source[receiver.end_byte()..selected_name.start_byte()] != "." {
        return None;
    }
    // The call namespace has to be closed before the name means the stdlib
    // function: an opaque implicit receiver in the environment can own a member
    // extension of exactly this name.
    if opaque_call_environment(index, file, source, call, receiver_type) {
        return None;
    }
    let binding = receiver_type.lookup(*receiver);
    // The receiver's *identity* may be a lexical binder while its type is only
    // a legacy inference (a loop element hint, the file-wide table). Binding
    // the block parameter to it needs declared or indexed evidence.
    let receiver_ty = binding.canonical_ty()?;
    let declaration = index.resolve_type(file, receiver_ty)?;
    if hierarchy_declares(index, file, declaration, name) {
        return None;
    }
    // A member extension declared (or inherited) by any enclosing declaration
    // is an implicit receiver's candidate for this call, so the name is not the
    // stdlib one even when the value receiver's own hierarchy is clean.
    for owner in enclosing_declarations(file, source, call) {
        if hierarchy_declares(index, file, owner, name)
            || nested_classifier_claims(index, owner, name)
        {
            return None;
        }
    }
    if !stdlib_name_is_unclaimed(index, file, source, call, name) {
        return None;
    }
    Some(*receiver)
}

/// True when the call sits inside a scope whose implicit receiver was not
/// proved transparent: any enclosing lambda that is not a canonical value-scope
/// block, or an anonymous object.
fn opaque_call_environment(
    index: &SourceIndex,
    file: &SourceFile,
    source: &str,
    call: Node<'_>,
    receiver_type: &dyn ReceiverTypes,
) -> bool {
    let mut current = call.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "lambda_literal" => match scope(index, file, source, parent, receiver_type) {
                Scope::Value(_) => {}
                Scope::Opaque => return true,
            },
            // The same shared classifier the lexical walk uses. A typed local
            // that answered before this barrier must not hide it: the callable
            // namespace is still unproved here.
            _ if implicit_receiver(parent) => return true,
            "class_declaration"
            | "object_declaration"
            | "enum_class_declaration"
            | "interface_declaration" => return false,
            _ => {}
        }
        current = parent.parent();
    }
    false
}

/// The declarations lexically enclosing `node`, innermost first.
fn enclosing_declarations<'a>(
    file: &'a SourceFile,
    source: &str,
    node: Node<'_>,
) -> Vec<&'a Declaration> {
    let mut owners = Vec::new();
    let mut current = Some(node);
    while let Some(candidate) = current {
        if matches!(
            candidate.kind(),
            "class_declaration"
                | "object_declaration"
                | "enum_class_declaration"
                | "interface_declaration"
        ) && let Some(name) = candidate.child_by_field_name("name")
        {
            let name = kt::text(name, source);
            let mut declarations = file
                .declarations
                .iter()
                .filter(|declaration| declaration.name == name);
            if let Some(declaration) = declarations.next()
                && declarations.next().is_none()
            {
                owners.push(declaration);
            }
        }
        current = candidate.parent();
    }
    owners
}

fn hierarchy_declares(
    index: &SourceIndex,
    file: &SourceFile,
    declaration: &Declaration,
    name: &str,
) -> bool {
    let mut stack = vec![declaration];
    let mut visited = HashSet::new();
    while let Some(current) = stack.pop() {
        if !visited.insert(current as *const Declaration as usize) {
            continue;
        }
        if current.members.iter().any(|member| {
            member.name.trim_matches('`') == name
                && matches!(
                    member.kind,
                    MemberKind::Method | MemberKind::Property | MemberKind::Constructor
                )
        }) {
            return true;
        }
        // A classifier declared directly inside this hierarchy node owns its
        // simple name here, even though the member inventory does not list it.
        if nested_classifier_claims(index, current, name) {
            return true;
        }
        let context = index.declaration_source_file(current).unwrap_or(file);
        for supertype in &current.supertypes {
            match index.resolve_type(context, supertype) {
                Some(resolved) => stack.push(resolved),
                // An ancestor outside the index is not an ancestor without
                // members. A compiled library base can own `name` itself (a
                // `let(block: (Other) -> Key)` member makes this a different
                // function), so the hierarchy is only closed when the ancestor
                // is a canonical methodless JVM marker.
                None if index.canonical_methodless_external_marker(context, supertype) => {}
                None => return true,
            }
        }
    }
    false
}

/// A classifier declared *directly inside* `owner` owns its simple name at that
/// receiver position: `info.let { … }` then constructs an inner `let` instead of
/// calling the stdlib extension. Nested classifiers are absent from the member
/// inventory, so this reads canonical owner identity from the workspace symbol
/// instead of testing a spelling or any foreign package name. Every nested
/// classifier kind is conservative: a class, interface, object, enum or
/// annotation directly inside the receiver all leave the binding unproved.
fn nested_classifier_claims(index: &SourceIndex, owner: &Declaration, name: &str) -> bool {
    let Some(file) = index.declaration_source_file(owner) else {
        return false;
    };
    let symbol = crate::semantics::workspace_symbol(index, owner);
    let mut nested = symbol.owner_path;
    nested.push(symbol.name.trim_matches('`').to_string());
    let claims = |candidate: &str, owner_path: &[String]| {
        candidate.trim_matches('`') == name && owner_path == nested.as_slice()
    };
    if index.declarations_named(name).any(|declaration| {
        declaration.package == owner.package
            && index
                .declaration_source_file(declaration)
                .is_some_and(|declaration_file| declaration_file.path == file.path)
            && claims(
                &declaration.name,
                &crate::semantics::workspace_symbol(index, declaration).owner_path,
            )
    }) {
        return true;
    }
    // A nested classifier is not always part of the declaration list; the
    // per-file symbol inventory carries the same nesting identity.
    crate::semantics::workspace_symbols_for_file(file)
        .iter()
        .any(|candidate| claims(&candidate.name, &candidate.owner_path))
}

/// No import, file declaration or same-package declaration owns `name`, so the
/// only declaration left for a bare selected call is the implicit stdlib one.
fn stdlib_name_is_unclaimed(
    index: &SourceIndex,
    file: &SourceFile,
    source: &str,
    call: Node<'_>,
    name: &str,
) -> bool {
    let mut root = call;
    while let Some(parent) = root.parent() {
        root = parent;
    }
    !import_rebinds(root, source, name)
        && !file_declares_name(root, source, name)
        // Only a *top-level* same-package declaration claims the name
        // package-wide. A classifier nested inside some other owner is that
        // owner's member, which the receiver-hierarchy proof decides; a nested
        // `let` in an unrelated class must not disable every repair in the
        // package.
        && !index.declarations_named(name).any(|declaration| {
            declaration.package == file.package
                && crate::semantics::workspace_symbol(index, declaration)
                    .owner_path
                    .is_empty()
        })
}

fn import_rebinds(root: Node<'_>, source: &str, name: &str) -> bool {
    root.named_children(&mut root.walk())
        .filter(|child| child.kind() == "import")
        .any(|import| match import_header(source, import) {
            // An import shape the shared reader does not understand proves
            // nothing: it could bind `name`, so the stdlib binding stays
            // unproved instead of being read as a foreign absence.
            None => true,
            Some(header) => match header.alias.as_deref() {
                Some(alias) => alias == name,
                // A wildcard imports every public name of its package. The
                // grammar spells that as a `*` token beside the path, never as
                // part of the path node, so this reads the tree rather than
                // the header text. Only a package Kotlin already imports by
                // default adds nothing a wildcard could rebind.
                None if header.wildcard => {
                    let package = header.path.replace('`', "");
                    !DEFAULT_IMPORT_PACKAGES.contains(&package.as_str())
                }
                None => header
                    .path
                    .rsplit('.')
                    .next()
                    .is_some_and(|last| last.trim_matches('`') == name),
            },
        })
}

/// Any function, property, variable or classifier this file declares under the
/// name could own a selected call. This is a file-wide scan on purpose: a local
/// `fun let` in a sibling scope, or a lambda parameter named `let`, is enough to
/// make the stdlib binding unprovable.
fn file_declares_name(root: Node<'_>, source: &str, name: &str) -> bool {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let declared = match node.kind() {
            "function_declaration"
            | "class_declaration"
            | "object_declaration"
            | "interface_declaration"
            | "type_alias" => node
                .child_by_field_name("name")
                .is_some_and(|node| kt::text(node, source).trim_matches('`') == name),
            "property_declaration" | "variable_declaration" | "parameter" | "class_parameter" => {
                let declaration = if node.kind() == "property_declaration" {
                    kt::child(node, "variable_declaration")
                } else {
                    Some(node)
                };
                declaration.is_some_and(|declaration| {
                    declaration
                        .named_children(&mut declaration.walk())
                        .find(|child| child.kind() == "identifier")
                        .is_some_and(|child| kt::text(child, source).trim_matches('`') == name)
                })
            }
            _ => false,
        };
        if declared {
            return true;
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    false
}
