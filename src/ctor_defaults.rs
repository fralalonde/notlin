//! Middle default arguments (N87CB): what a caller that omits one needs from a
//! declaration that is moving to Java.
//!
//! Kotlin lets a caller omit any subset of a constructor's defaulted
//! parameters, by name. Java has no named arguments and one constructor per
//! arity, so an omission is expressible in exactly two ways — and every decision
//! here follows from picking between them, not from the mere presence of a
//! default:
//!
//! - **The default is a language-neutral literal** (`10`, `"x"`, `true`): a
//!   translated call site writes it into the call itself, which becomes the
//!   canonical all-arguments one — `new Example(first, 10, last)`.
//! - **Anything else** (a constructor call, a companion reference, an expression
//!   over other parameters): the emitter writes ONE delegating overload for the
//!   exact omission pattern a caller uses —
//!   `Example(String first, boolean last) { this(first, 10, last); }` — and the
//!   caller drops the omitted arguments, so a retained Kotlin named-argument
//!   call and a translated Java one both land on it. Never all 2^n
//!   combinations: only the shapes call sites actually use.
//!
//! The declaration stays in Kotlin only when neither is available: a pattern
//! that collides with another constructor after erasure, an omitted default that
//! names a parameter the pattern does not supply, or a call shape that could not
//! be read at all. Those are the cases [`CtorDefaultPlan::blocked`] reports; a
//! declaration with a middle default that nobody omits costs nothing.

use tree_sitter::Node;

/// A call's arguments mapped onto the callee's declared parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedArgs {
    /// Argument text by parameter index; `None` where the caller omitted it.
    pub slots: Vec<Option<String>>,
    /// Parameter indices the caller omitted, in declaration order.
    pub omitted: Vec<usize>,
}

/// Map a call's written arguments onto `param_names`.
///
/// Arguments come in source order, as `(name, text)` — `name` is `None` for a
/// positional argument. `None` when a named argument matches no parameter, when
/// a positional argument overflows the parameter list, or when a positional
/// argument follows a named one: in each case the call is not the shape it
/// appears to be, and guessing would emit a wrong argument list.
pub fn resolve_args(
    param_names: &[String],
    args: &[(Option<String>, String)],
) -> Option<ResolvedArgs> {
    let mut slots: Vec<Option<String>> = vec![None; param_names.len()];
    let mut named_seen = false;
    for (name, text) in args {
        match name {
            Some(name) => {
                named_seen = true;
                let index = param_names.iter().position(|param| param == name)?;
                slots[index] = Some(text.clone());
            }
            None => {
                if named_seen {
                    return None;
                }
                let index = slots.iter().position(Option::is_none)?;
                slots[index] = Some(text.clone());
            }
        }
    }
    let omitted = slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| slot.is_none())
        .map(|(index, _)| index)
        .collect();
    Some(ResolvedArgs { slots, omitted })
}

/// The argument list to write for a translated call that omits some parameters,
/// plus the indices left to the declaration's delegating overload.
///
/// An omitted parameter whose default is a language-neutral literal is written
/// into the call, so the call keeps the canonical all-arguments shape. The rest
/// are dropped: the emitter writes one overload per pattern a call site uses,
/// and the overload supplies the default inside the declaration, where every
/// parameter is in scope.
pub fn lower_call_args(
    slots: &[Option<String>],
    defaults: &[Option<String>],
) -> (Vec<String>, Vec<usize>) {
    let mut args = Vec::with_capacity(slots.len());
    let mut left_to_overload = Vec::new();
    for (index, slot) in slots.iter().enumerate() {
        match slot {
            Some(value) => args.push(value.clone()),
            None => match defaults.get(index).and_then(Option::as_deref) {
                Some(default) if is_neutral_literal(default) => {
                    args.push(default.trim().to_string())
                }
                _ => left_to_overload.push(index),
            },
        }
    }
    (args, left_to_overload)
}

/// True for a default that can be written at a call site verbatim, in Kotlin and
/// Java alike: a numeric, string, character, boolean or `null` literal with no
/// interpolation. Everything else — a constructor call, a companion reference,
/// an expression over other parameters — belongs inside the delegating overload,
/// where the declaration's own scope and imports apply.
pub fn is_neutral_literal(text: &str) -> bool {
    let text = text.trim();
    if matches!(text, "true" | "false" | "null") {
        return true;
    }
    let body = text.strip_prefix('-').unwrap_or(text);
    let body = body
        .strip_suffix(['L', 'l', 'f', 'F', 'd', 'D'])
        .unwrap_or(body);
    if body.is_empty() {
        return false;
    }
    if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        return !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit() || c == '_');
    }
    // Interpolation (`"a$b"`) and escapes (`"\n"`, `"\$"`) do not read the same
    // in Java; neither is worth guessing at for a default value.
    if body.starts_with('"') && body.ends_with('"') && body.len() >= 2 {
        return !body.contains('$') && !body.contains('\\');
    }
    if body.starts_with('\'') && body.ends_with('\'') && body.len() >= 3 {
        return !body.contains('$') && !body.contains('\\');
    }
    // Kotlin's unsigned literals (`10u`) have no Java form.
    if body.ends_with('u') || body.ends_with('U') {
        return false;
    }
    let mut digits = 0usize;
    let mut dots = 0usize;
    let mut exponent = false;
    for (offset, ch) in body.char_indices() {
        match ch {
            '0'..='9' => digits += 1,
            '_' => {}
            '.' if dots == 0 => dots += 1,
            'e' | 'E' if digits > 0 && !exponent && offset > 0 => exponent = true,
            '+' | '-' if exponent => {}
            _ => return false,
        }
    }
    digits > 0
}

/// Erasure key for a parameter type list: type arguments dropped, nullability
/// dropped, whitespace removed. Two constructors with the same key cannot
/// coexist in one Java class, so the emitter must not write both.
pub fn erasure_key(param_types: &[String]) -> String {
    param_types
        .iter()
        .map(|ty| {
            let mut out = String::with_capacity(ty.len());
            let mut depth = 0usize;
            for ch in ty.chars() {
                match ch {
                    '<' => depth += 1,
                    '>' => depth = depth.saturating_sub(1),
                    '?' => {}
                    c if depth == 0 => out.push(c),
                    _ => {}
                }
            }
            out.chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The parameter a default expression names that the omission pattern does not
/// supply, if there is one.
///
/// A delegating overload's body evaluates the omitted defaults, so a default can
/// only name parameters that overload takes. `(val a: Int = 10, val b: Int =
/// a + 1)` with both omitted is the case Java cannot express: `b`'s default
/// needs `a`, and no constructor can compute a local before `this(...)`.
pub fn unsupplied_reference(
    default: &str,
    param_names: &[String],
    supplied: &[usize],
) -> Option<String> {
    identifiers(default).into_iter().find(|name| {
        param_names
            .iter()
            .position(|param| param == name)
            .is_some_and(|index| !supplied.contains(&index))
    })
}

/// Identifiers in an expression, ignoring qualified names (`x.location` yields
/// `x`, not `location`: the member is not a constructor parameter) and anything
/// inside a string literal.
fn identifiers(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.char_indices().peekable();
    let mut quote: Option<char> = None;
    while let Some((_, ch)) = chars.next() {
        if let Some(open) = quote {
            if ch == '\\' {
                chars.next();
            } else if ch == open {
                quote = None;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            continue;
        }
        if ch == '.' {
            // The member name after the dot belongs to the receiver, not to the
            // constructor's parameter list.
            while let Some((_, next)) = chars.peek() {
                if next.is_alphanumeric() || *next == '_' {
                    chars.next();
                } else {
                    break;
                }
            }
            continue;
        }
        if ch.is_alphabetic() || ch == '_' {
            let mut name = String::from(ch);
            while let Some((_, next)) = chars.peek() {
                if next.is_alphanumeric() || *next == '_' {
                    name.push(*next);
                    chars.next();
                } else {
                    break;
                }
            }
            out.push(name);
        }
    }
    out
}

/// What a declaration with defaulted constructor parameters needs so that its
/// callers keep compiling once it is Java.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CtorDefaultPlan {
    /// Omission patterns the emitter must write a delegating overload for.
    pub overloads: Vec<Vec<usize>>,
    /// Why no set of overloads can serve the callers, when that is the case.
    /// Set only for a declaration that has to stay in Kotlin.
    pub blocked: Option<String>,
}

/// Everything the constructor says, in the terms its callers see it.
pub struct CtorShape<'a> {
    pub param_names: &'a [String],
    /// Raw default text per parameter (`None` = the parameter has no default).
    pub defaults: &'a [Option<String>],
    /// Raw parameter type text, for the erasure check.
    pub param_types: &'a [String],
    /// Omission patterns call sites actually use.
    pub patterns: &'a [Vec<usize>],
    /// Trailing patterns the emitter writes anyway (a `@JvmOverloads` request,
    /// or the in-place ladder that keeps Kotlin's positional ABI). They are
    /// already covered, but they still occupy signatures.
    pub ladder: &'a [Vec<usize>],
    /// The class declares another constructor of its own, whose signature this
    /// pass cannot read: emitting a pattern overload could duplicate it.
    pub has_secondary_constructor: bool,
}

/// Decide the delegating overloads a declaration needs, or why it cannot have
/// them.
///
/// One overload per omission pattern a call site uses, minus the ones the ladder
/// already covers. `blocked` carries the reason to show when a needed overload
/// cannot be written.
pub fn plan_ctor_defaults(shape: &CtorShape<'_>) -> CtorDefaultPlan {
    let needed: Vec<Vec<usize>> = shape
        .patterns
        .iter()
        .filter(|pattern| !shape.ladder.contains(pattern))
        .cloned()
        .collect();
    if needed.is_empty() {
        // Nothing to add: the ladder (if any) covers what callers do, and every
        // omission is served by the ladder or by a literal.
        return CtorDefaultPlan::default();
    }
    if shape.has_secondary_constructor {
        return CtorDefaultPlan {
            overloads: Vec::new(),
            blocked: Some(
                "it declares another constructor, whose signature a delegating overload could duplicate"
                    .to_string(),
            ),
        };
    }
    for pattern in &needed {
        let supplied: Vec<usize> = (0..shape.param_names.len())
            .filter(|index| !pattern.contains(index))
            .collect();
        for index in pattern {
            let name = shape.param_names.get(*index).cloned().unwrap_or_default();
            let Some(Some(default)) = shape.defaults.get(*index) else {
                return CtorDefaultPlan {
                    overloads: Vec::new(),
                    blocked: Some(format!(
                        "`{name}` has no default for the omission pattern a caller uses"
                    )),
                };
            };
            if let Some(missing) = unsupplied_reference(default, shape.param_names, &supplied) {
                return CtorDefaultPlan {
                    overloads: Vec::new(),
                    blocked: Some(format!(
                        "its default for `{name}` names `{missing}`, which the omission pattern a caller uses does not supply"
                    )),
                };
            }
        }
    }
    // A pattern overload must differ from the full constructor — arity already
    // says so — and from every other constructor the emitter writes. Equal-arity
    // patterns are compared after erasure: `List<A>` and `List<B>` cannot both be
    // a parameter of the same constructor.
    let signature = |pattern: &Vec<usize>| {
        let types: Vec<String> = (0..shape.param_names.len())
            .filter(|index| !pattern.contains(index))
            .filter_map(|index| shape.param_types.get(index).cloned())
            .collect();
        (types.len(), erasure_key(&types))
    };
    let mut seen: Vec<(usize, String)> = Vec::new();
    for pattern in needed.iter().chain(shape.ladder.iter()) {
        let signature = signature(pattern);
        if seen.contains(&signature) {
            return CtorDefaultPlan {
                overloads: Vec::new(),
                blocked: Some(
                    "the delegating overloads its callers need collide after type erasure"
                        .to_string(),
                ),
            };
        }
        seen.push(signature);
    }
    CtorDefaultPlan {
        overloads: needed,
        blocked: None,
    }
}

/// The trailing omission patterns a run that preserves Kotlin's positional
/// constructor ABI writes: `omit the last k`, for every k up to the run of
/// trailing defaults. Empty when no default is trailing.
pub fn trailing_patterns(defaults: &[Option<String>]) -> Vec<Vec<usize>> {
    let trailing = defaults
        .iter()
        .rev()
        .take_while(|default| default.is_some())
        .count();
    (1..=trailing)
        .map(|omitted| (defaults.len() - omitted..defaults.len()).collect())
        .collect()
}

/// A constructor-shaped call in a Kotlin file, with the byte ranges a rewrite
/// needs.
///
/// The same rule the index records omission evidence with: a bare, capitalized
/// callee with no trailing lambda. `Outer.Inner(...)`, a factory call and a call
/// through a value all look alike at this granularity, so none is guessed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    pub callee: String,
    /// Byte range of the `value_arguments` node.
    pub range: (usize, usize),
    /// Written arguments, in source order.
    pub args: Vec<CallArg>,
    /// The call's shape could not be read (a spread argument, or a positional
    /// argument after a named one): the omission set is unknown.
    pub unknown: bool,
    /// 1-based line.
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallArg {
    /// Parameter name for a named argument.
    pub name: Option<String>,
    /// Byte range of the argument's value expression.
    pub value_range: (usize, usize),
    /// The argument is a spread (`*args`).
    pub spread: bool,
}

/// Every constructor-shaped call in a Kotlin file.
/// Constructor calls in one Kotlin file.
pub fn call_sites(source: &str) -> Vec<CallSite> {
    call_sites_in(&crate::transpiler::parse_tree(source), source)
}

/// [`call_sites`] against an already-parsed tree: the index parses once and must
/// not pay for a second parse just to fill its constructor-call evidence.
pub fn call_sites_in(tree: &tree_sitter::Tree, source: &str) -> Vec<CallSite> {
    let mut out = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if let Some(site) = (node.kind() == "call_expression")
            .then(|| call_site(node, source))
            .flatten()
        {
            out.push(site);
        }
        for child in node.children(&mut node.walk()) {
            stack.push(child);
        }
    }
    out.sort_by_key(|site| site.range.0);
    out
}

fn call_site(node: Node<'_>, source: &str) -> Option<CallSite> {
    let children: Vec<Node<'_>> = {
        let mut cursor = node.walk();
        node.children(&mut cursor).collect()
    };
    // A trailing lambda is a function call, never a constructor.
    if children.iter().any(|child| child.kind().contains("lambda")) {
        return None;
    }
    let callee_node = children.iter().find(|child| child.is_named())?;
    if callee_node.kind() != "identifier" {
        return None;
    }
    let callee = text_of(*callee_node, source)?;
    if !callee
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
    {
        return None;
    }
    let arguments = children
        .iter()
        .find(|child| child.kind() == "value_arguments")?;
    let mut args = Vec::new();
    let mut unknown = false;
    let mut seen_named = false;
    let mut cursor = arguments.walk();
    for argument in arguments.children(&mut cursor) {
        if argument.kind() != "value_argument" {
            continue;
        }
        let inner: Vec<Node<'_>> = {
            let mut inner_cursor = argument.walk();
            argument.children(&mut inner_cursor).collect()
        };
        if inner
            .iter()
            .any(|child| child.kind() == "spread_expression")
        {
            unknown = true;
            continue;
        }
        // `name = value`: the named child immediately before the `=` token.
        let mut name: Option<String> = None;
        let mut previous: Option<Node<'_>> = None;
        for child in &inner {
            if !child.is_named() && child.kind() == "=" {
                name = previous.and_then(|node| text_of(node, source));
                if previous.is_some() && name.is_none() {
                    unknown = true;
                }
                break;
            }
            if child.is_named() {
                previous = Some(*child);
            }
        }
        let Some(value) = inner.iter().rev().find(|child| child.is_named()) else {
            unknown = true;
            continue;
        };
        if name.is_some() {
            seen_named = true;
        } else if seen_named {
            // Kotlin rejects a positional argument after a named one: the call
            // is not a plain constructor shape.
            unknown = true;
        }
        args.push(CallArg {
            name,
            value_range: (value.start_byte(), value.end_byte()),
            spread: false,
        });
    }
    Some(CallSite {
        callee,
        range: (arguments.start_byte(), arguments.end_byte()),
        args,
        unknown,
        line: node.start_position().row + 1,
    })
}

/// Rewrite the constructor calls of a retained Kotlin file that Java can no
/// longer serve as written: named arguments lowered to the declared order, and
/// the parameters a caller omits dropped, so the call lands on the delegating
/// overload the emitter wrote for that pattern.
///
/// `target` answers with the emitted Java constructor's parameter names, in
/// declaration order. `None` leaves the call alone — a declaration that stayed
/// Kotlin still has nameable parameters and real defaults, so its call sites are
/// already valid.
pub fn rewrite(source: &str, target: &dyn Fn(&str) -> Option<Vec<String>>) -> (String, usize) {
    let tree = crate::transpiler::parse_tree(source);
    let sites = call_sites_in(&tree, source);
    let mut edits = Vec::new();
    for site in &sites {
        if site.unknown {
            continue;
        }
        let Some(params) = target(&site.callee) else {
            continue;
        };
        let written: Vec<(Option<String>, String)> = site
            .args
            .iter()
            .filter_map(|arg| {
                let text = source.get(arg.value_range.0..arg.value_range.1)?;
                Some((arg.name.clone(), text.to_string()))
            })
            .collect();
        let Some(resolved) = resolve_args(&params, &written) else {
            continue;
        };
        // Every parameter supplied positionally and in declared order: the call
        // is already the shape Java wants.
        if resolved.omitted.is_empty() && written.iter().all(|(name, _)| name.is_none()) {
            continue;
        }
        // The arguments that stay, in declared order. Omitted parameters are
        // dropped, not filled: their defaults live inside the declaration, and
        // the overload the emitter wrote for this pattern supplies them with
        // every parameter in scope.
        let mut ordered: Vec<(usize, String)> = Vec::new();
        for (index, slot) in resolved.slots.iter().enumerate() {
            let Some(text) = slot else {
                continue;
            };
            ordered.push((index, text.clone()));
        }
        let replacement = format!(
            "({})",
            ordered
                .into_iter()
                .map(|(_, text)| text)
                .collect::<Vec<_>>()
                .join(", ")
        );
        edits.push(crate::smart_cast::Edit {
            start: site.range.0,
            end: site.range.1,
            text: replacement,
        });
    }
    if edits.is_empty() {
        return (source.to_string(), 0);
    }
    let rewritten = edits.len();
    (crate::smart_cast::apply_edits(source, edits), rewritten)
}

/// Raw default text per primary-constructor parameter, in declaration order
/// (`None` where the parameter has no default).
pub fn class_param_default_texts(decl: Node<'_>, source: &str) -> Vec<Option<String>> {
    let Some(parameters) = class_parameters(decl) else {
        return Vec::new();
    };
    let mut cursor = parameters.walk();
    parameters
        .children(&mut cursor)
        .filter(|child| child.kind() == "class_parameter")
        .map(|parameter| {
            let has_default = parameter
                .children(&mut parameter.walk())
                .any(|child| !child.is_named() && child.kind() == "=");
            if !has_default {
                return None;
            }
            let value = parameter
                .children(&mut parameter.walk())
                .filter(|child| child.is_named())
                .last()?;
            text_of(value, source)
        })
        .collect()
}

/// Raw parameter type text per primary-constructor parameter, in declaration
/// order. Read from the syntax rather than from the index's members: a
/// constructor parameter that is not a `val` contributes no member, so the
/// member list is not aligned with the parameter list.
pub fn class_param_types(decl: Node<'_>, source: &str) -> Vec<String> {
    let Some(parameters) = class_parameters(decl) else {
        return Vec::new();
    };
    let mut cursor = parameters.walk();
    parameters
        .children(&mut cursor)
        .filter(|child| child.kind() == "class_parameter")
        .filter_map(|parameter| {
            let type_node = parameter
                .children(&mut parameter.walk())
                .find(|child| child.kind().contains("type"))?;
            text_of(type_node, source)
        })
        .collect()
}

fn class_parameters(decl: Node<'_>) -> Option<Node<'_>> {
    let constructor = decl
        .child_by_field_name("primary_constructor")
        .or_else(|| {
            decl.children(&mut decl.walk())
                .find(|child| child.kind() == "primary_constructor")
        })?;
    constructor
        .children(&mut constructor.walk())
        .find(|child| child.kind() == "class_parameters")
}

fn text_of(node: Node<'_>, source: &str) -> Option<String> {
    node.utf8_text(source.as_bytes()).ok().map(str::to_string)
}
