//! Expression translation: Kotlin expression tree -> Java expression text.

mod binary;
mod call;
mod container_lambdas;
mod eager_collections;
mod navigation;
mod safe_call;
mod string;

pub(crate) use navigation::rewrite_reified_type_args;

use crate::transpiler::kt;
use crate::transpiler::unit::Unit;

pub struct Expr<'a, 'src, 'tree> {
    pub unit: &'a mut Unit<'src, 'tree>,
}

impl<'a, 'src, 'tree> Expr<'a, 'src, 'tree> {
    /// Name of the enclosing class/interface/object while a nested property
    /// getter or function body is being emitted.
    pub(crate) fn enclosing_type_name(&self) -> Option<String> {
        let mut current = self.unit.current_decl;
        while let Some(node) = current {
            if matches!(
                node.kind(),
                "class_declaration" | "interface_declaration" | "object_declaration"
            ) {
                return kt::field(node, "name").map(|name| self.unit.text(name).to_string());
            }
            current = node.parent();
        }
        None
    }

    pub(crate) fn enclosing_record_has_component(&self, component: &str) -> bool {
        if self.unit.lombok {
            return false;
        }
        let mut current = self.unit.current_decl;
        while let Some(node) = current {
            if node.kind() == "class_declaration" {
                let is_data = kt::child(node, "modifiers")
                    .is_some_and(|modifiers| self.unit.text(modifiers).contains("data"));
                if !is_data {
                    return false;
                }
                return kt::field(node, "name")
                    .map(|name| self.unit.text(name))
                    .filter(|owner| self.unit.record_types.contains(*owner))
                    .and_then(|owner| self.unit.data_components.get(owner))
                    .is_some_and(|components| {
                        components
                            .iter()
                            .any(|(_, candidate)| candidate == component)
                    });
            }
            current = node.parent();
        }
        false
    }

    /// Whether `name` is a `val`/`var` primary-constructor property of the
    /// nearest enclosing class. This check is deliberately bounded by the
    /// nearest class/object/companion receiver: a companion getter must not
    /// accidentally bind to an outer instance property instead of an imported
    /// top-level declaration with the same name.
    fn enclosing_constructor_property(&self, name: &str) -> bool {
        let mut current = self.unit.current_decl;
        while let Some(node) = current {
            if matches!(
                node.kind(),
                "class_declaration" | "object_declaration" | "companion_object"
            ) {
                if node.kind() != "class_declaration" {
                    return false;
                }
                let Some(parameters) = kt::child(node, "primary_constructor")
                    .and_then(|constructor| kt::child(constructor, "class_parameters"))
                else {
                    return false;
                };
                return parameters
                    .named_children(&mut parameters.walk())
                    .any(|parameter| {
                        parameter.kind() == "class_parameter"
                            && (kt::child(parameter, "val").is_some()
                                || kt::child(parameter, "var").is_some())
                            && parameter
                                .child_by_field_name("name")
                                .or_else(|| kt::child(parameter, "identifier"))
                                .is_some_and(|identifier| self.unit.text(identifier).trim() == name)
                    });
            }
            current = node.parent();
        }
        false
    }

    pub fn transpile(&mut self, node: tree_sitter::Node) -> String {
        if let Some(alias) = self
            .unit
            .safe_receiver_aliases
            .get(&(node.start_byte(), node.end_byte()))
        {
            return alias.clone();
        }
        let file = self
            .unit
            .workspace_file
            .as_deref()
            .unwrap_or(self.unit.file);
        if let Some(merge) = self.unit.semantic_provider.and_then(|provider| {
            crate::semantics::map_algebra::establish(self.unit.source, file, node, provider)
        }) {
            let left = self.transpile(merge.left);
            let right = self.transpile(merge.right);
            // The RHS may be a target-typed collector. Select the Map overload
            // explicitly, rather than allowing javac to infer Pair/Sequence.
            // Both operands have already been proved to be standard maps.
            return format!("kotlin.collections.MapsKt.plus({left}, (java.util.Map) ({right}))");
        }
        if let Some(java) = self.safe_call_expression(node) {
            return java;
        }
        if self.is_unit_singleton(node) {
            return "kotlin.Unit.INSTANCE".to_string();
        }
        match node.kind() {
            "string_literal" => self.string_literal(node),
            // Kotlin and Java use the same lexical form for character
            // literals, including standard escapes such as `\\n` and `\\uXXXX`.
            "character_literal" => self.unit.text(node).trim().to_string(),
            "number_literal" | "boolean_literal" | "hex_literal" | "long_literal"
            | "real_literal" | "float_literal" => {
                let raw = self.unit.text(node).trim().to_string();
                if matches!(node.kind(), "float_literal" | "real_literal") {
                    let clean = raw.replace('_', "");
                    let single = clean.ends_with(['f', 'F']);
                    let numeric = clean.trim_end_matches(['f', 'F']);
                    let parsed = if single {
                        numeric.parse::<f32>().map(f64::from)
                    } else {
                        numeric.parse::<f64>()
                    };
                    if let Ok(value) = parsed {
                        if value.is_infinite() {
                            // Constant arithmetic also works when a local
                            // value shadows the `java` package in expressions.
                            let sign = if value.is_sign_negative() { "-" } else { "" };
                            let suffix = if single { "f" } else { "" };
                            return format!("({sign}1.0{suffix} / 0.0{suffix})");
                        }
                        // Kotlin permits rounded-to-zero constants which javac
                        // rejects as nonzero literals too small for their type.
                        let mantissa = numeric.split(['e', 'E']).next().unwrap_or(numeric);
                        if value == 0.0 && mantissa.chars().any(|c| c.is_ascii_digit() && c != '0')
                        {
                            return if single { "0.0f" } else { "0.0" }.to_string();
                        }
                    }
                }
                // Kotlin unsigned suffixes (u/U/L/UL/uL) have no Java
                // equivalent for the plain literal; strip to the base type.
                if raw.ends_with("uL")
                    || raw.ends_with("UL")
                    || raw.ends_with("Lu")
                    || raw.ends_with("LU")
                {
                    // unsigned markers vanish in Java
                    raw.trim_end_matches(['u', 'U', 'L']).to_string()
                } else {
                    raw
                }
            }
            "identifier" => {
                let name = self.unit.text(node).trim().to_string();
                if let Some(alias) = self.unit.local_expression_aliases.get(&name) {
                    return alias.clone();
                }
                // Lexical bindings win before every implicit-receiver or
                // workspace-name lookup, even when their type is unknown.
                if crate::function_callsite::local_shadow(node, &name, self.unit.source) {
                    return self.narrowed_receiver(&name, &name);
                }
                let implicit_this = self.narrowed_receiver("this", "this");
                // Bare self-reference inside an object body: Kotlin resolves
                // `Registry` to the singleton; Java needs `Registry.INSTANCE`.
                let value = if self.unit.current_object.as_deref() == Some(name.as_str()) {
                    format!("{}.INSTANCE", name)
                } else if let Some(access) = self.unit.semantic_provider.and_then(|provider| {
                    crate::semantics::inherited_property_receiver_access(
                        self.unit.source,
                        node,
                        self.unit
                            .workspace_file
                            .as_deref()
                            .unwrap_or(self.unit.file),
                        provider,
                    )
                }) {
                    let mut path = access.contract_owner.owner_path.clone();
                    path.push(access.contract_owner.name.clone());
                    let owner = if access.contract_owner.package.is_empty() {
                        path.join(".")
                    } else {
                        format!("{}.{}", access.contract_owner.package, path.join("."))
                    };
                    format!(
                        "(({owner}) {implicit_this}).{}()",
                        crate::workspace::property_accessor_name(&access.getter_contract.name)
                    )
                } else if !self.unit.var_types.contains_key(&name)
                    && self.enclosing_record_has_component(&name)
                {
                    // A data class is emitted as a Java record. Its implicit
                    // component reads use `component()`, not JavaBean
                    // `getComponent()` accessors.
                    format!("{implicit_this}.{}()", name)
                } else if !self.unit.var_types.contains_key(&name)
                    && let Some(getter) = self.unit.self_getters.get(&name)
                {
                    // Not a local/param in this scope: a bare identifier
                    // naming a property member of the enclosing type reads
                    // through its accessor (implicit `this`).
                    format!("{implicit_this}.{}()", getter)
                } else if !self.unit.var_types.contains_key(&name)
                    && self.enclosing_constructor_property(&name)
                {
                    // Constructor properties are class members even while a
                    // computed property getter is being emitted (its local
                    // variable type scope is intentionally empty). Bind this
                    // before workspace-level top-level-property lookup so an
                    // unrelated imported `data` cannot capture the name.
                    format!("{implicit_this}.{name}")
                } else if !self.unit.var_types.contains_key(&name)
                    && let Some(literal) = self.inline_top_level_string_const(&name, node)
                {
                    literal
                } else if !self.unit.var_types.contains_key(&name)
                    && let Some(owner) = self
                        .unit
                        .workspace
                        .and_then(|w| w.find_property_owner(&name))
                {
                    // Indexed property owner: same accessor shape whether
                    // the owner translated or stayed Kotlin. Conservative
                    // shape: only fire when no local of that name exists.
                    let _ = owner;
                    let mut cap = name.clone();
                    if let Some(first) = cap.chars().next() {
                        cap = first.to_uppercase().collect::<String>() + &cap[1..];
                    }
                    format!("{implicit_this}.get{}()", cap)
                } else if self
                    .unit
                    .workspace
                    .and_then(|w| w.object_declaration_named(&name))
                    .is_some_and(|obj| {
                        // Java calls both generated and retained Kotlin object
                        // values through their public singleton field. A
                        // retained Kotlin object is still a Java class with
                        // `INSTANCE`; its source-level bare-value syntax does
                        // not survive this Java emission boundary.
                        obj.name != self.unit.current_object.as_deref().unwrap_or("")
                    })
                {
                    format!("{}.INSTANCE", name)
                } else {
                    name.clone()
                };
                self.narrowed_receiver(&name, &value)
            }
            // `this` inside an extension function body refers to the receiver
            // parameter (emitted as a regular first param, so `this` must map
            // to it in the static method's body).
            "this_expression" => {
                let receiver = self
                    .unit
                    .ext_receiver_name
                    .clone()
                    .unwrap_or_else(|| "this".to_string());
                self.narrowed_receiver(self.unit.text(node).trim(), &receiver)
            }
            // `super<T>` qualifier: Java drops the explicit supertype — an
            // interface super-access is just `T.super.member` (a Kotlin class
            // super-access is the plain-`super` form, which falls through).
            "super_expression" => self.transpile_super_qualifier(node),
            "navigation_expression" => self.navigation(node),
            "call_expression" => self.call(node),
            "infix_expression" => self.infix_expr(node),
            "binary_expression" => self.binary(node),
            "parenthesized_expression" | "parenthesized" => {
                let inner = self
                    .unit
                    .expression_children(node)
                    .first()
                    .copied()
                    .map(|c| self.transpile(c))
                    .unwrap_or_default();
                format!("({})", inner)
            }
            // elvis_expression: use binary()'s unified ternary rewrite
            "elvis_expression" => self.binary(node),
            "range_expression" => {
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "range used as a value has no direct Java equivalent",
                );
                let mut cursor = node.walk();
                let kids: Vec<String> = node
                    .children(&mut cursor)
                    .filter(|c| c.is_named())
                    .map(|c| self.transpile(c))
                    .collect();
                format!("List.of({})", kids.join(", "))
            }
            "lambda_literal" => self.lambda(node),
            "when_expression" => self.when_expr(node),
            "is_expression" => {
                // `x is T` -> `x instanceof T`; branch-local narrowing is
                // supplied by the enclosing condition lowering.
                let mut cursor = node.walk();
                let kids: Vec<_> = node.children(&mut cursor).collect();
                let target = kids.iter().find(|c| c.is_named()).copied();
                let ty = kids
                    .iter()
                    .filter(|c| c.is_named())
                    .nth(1)
                    .map(|t| box_primitive(&kt::java_type(*t, self.unit.source)))
                    .unwrap_or_else(|| "Object".to_string());
                match target {
                    Some(t) => {
                        let t_java = self.transpile(t);
                        let check = format!("{} instanceof {}", t_java, ty);
                        if self.unit.text(node).contains("!is") {
                            format!("!({})", check)
                        } else {
                            check
                        }
                    }
                    None => "false".to_string(),
                }
            }
            "indexing_expression" | "index_expression" => self.indexing(node),
            "jump_expression" => self.jump(node),
            "throw_expression" => self.throw_expr(node),
            "if_expression" => self.if_expr(node),
            // Kotlin vararg spread `*expr`. Java has no spread syntax, so the
            // leaked `*` would be a syntax error — never pass the operator
            // through. The practical cases:
            //   `arrayOf(*arr)`  (spread as the only/first factory arg) is an
            //     array copy — handled in call.rs's array-factory arm;
            //   everything else passes the array itself and records the
            //     approximation (the callee must accept the array).
            "spread_expression" => {
                let mut cursor = node.walk();
                let inner = node
                    .children(&mut cursor)
                    .find(|c| c.is_named())
                    .map(|c| self.transpile(c))
                    .unwrap_or_else(|| "null".to_string());
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "vararg spread `*expr` lowered to the array itself (Java has no spread at call sites; callee must accept the array)",
                );
                inner
            }
            "as_expression" => self.as_expr(node),
            "unary_expression" => self.unary_expr(node),
            _ => {
                // Last resort: try to copy verbatim text if it's plausible Java,
                // else emit null and warn.
                let text = self.unit.text(node);
                if text
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._()<>[]{}\" ',:+-*/%=!&|?;".contains(c))
                    && !text.contains("val")
                    && !text.contains("var")
                    && !text.contains("fun ")
                {
                    self.unit.diags.warn_approx(
                        node,
                        self.unit.file,
                        format!("expression kind '{}' passed through verbatim", node.kind()),
                    );
                    text.to_string()
                } else {
                    self.unit.diag_untranslatable(
                        node,
                        format!("expression kind '{}' not supported", node.kind()),
                    );
                    "null /* notlin: unsupported */".to_string()
                }
            }
        }
    }

    /// Unit is a singleton value, independently of the void descriptor used by
    /// Unit-returning methods. Source imports establish the type binding; value
    /// parameters, locals and properties can still shadow that spelling.
    fn is_unit_singleton(&self, node: tree_sitter::Node) -> bool {
        if !kt::is_canonical_unit_reference(node, self.unit.source) {
            return false;
        }
        let spelling = self.unit.text(node).trim().replace('`', "");
        let root_name = spelling.split('.').next().unwrap_or(&spelling);
        if self.unit.var_types.contains_key(root_name)
            || self.unit.self_getters.contains_key(root_name)
        {
            return false;
        }
        if spelling == "Unit"
            && let Some(provider) = self.unit.semantic_provider
        {
            let file = self
                .unit
                .workspace_file
                .as_deref()
                .unwrap_or(self.unit.file);
            let package = self
                .unit
                .workspace
                .and_then(|index| index.source_file(file))
                .and_then(|file| file.package.as_deref())
                .unwrap_or("");
            if provider.has_type_alias_in_package(package, "Unit")
                || !matches!(
                    provider.resolve_type_in_file(file, package, "Unit"),
                    crate::semantics::FactStatus::Unknown
                )
            {
                return false;
            }
        }
        let mut enclosing = node.parent();
        while let Some(scope) = enclosing {
            // Search declarations in lexical scopes, without descending into
            // sibling functions/classes whose locals cannot bind this use.
            let mut pending: Vec<_> = scope.named_children(&mut scope.walk()).collect();
            while let Some(candidate) = pending.pop() {
                if matches!(
                    candidate.kind(),
                    "parameter" | "class_parameter" | "variable_declaration"
                ) && kt::field(candidate, "name")
                    .or_else(|| kt::child(candidate, "identifier"))
                    .is_some_and(|name| self.unit.text(name).trim().trim_matches('`') == root_name)
                {
                    return false;
                }
                if !matches!(
                    candidate.kind(),
                    "function_declaration"
                        | "class_declaration"
                        | "object_declaration"
                        | "lambda_literal"
                        | "function_body"
                ) {
                    pending.extend(candidate.named_children(&mut candidate.walk()));
                }
            }
            enclosing = scope.parent();
        }
        true
    }

    pub fn transpile_target(&mut self, node: tree_sitter::Node) -> String {
        match node.kind() {
            "identifier" => {
                let name = self.unit.text(node).trim().to_string();
                if let Some(setter) = self.interface_property_setter(&name, node) {
                    self.unit.pending_setter = true;
                    return format!("this.{setter}(");
                }
                // Bare self-reference inside an object body: Kotlin resolves
                // `Registry` to the singleton; Java needs `Registry.INSTANCE`.
                if self.unit.current_object.as_deref() == Some(name.as_str()) {
                    format!("{}.INSTANCE", name)
                } else if let Some(setter) = self.inherited_mutable_property_setter(&name, node) {
                    self.unit.pending_setter = true;
                    format!("this.{setter}(")
                } else if self.inherited_property_target_is_unproven(&name, node) {
                    self.unit.diag_untranslatable(
                        node,
                        format!(
                            "inherited property `{name}` has no verified writable Java accessor"
                        ),
                    );
                    "null /* notlin: unresolved inherited property write */".to_string()
                } else {
                    name
                }
            }
            "navigation_expression" => {
                if let Some((receiver, member)) = self.unit.nav_base_member(node)
                    && let Some(alias) = self
                        .unit
                        .safe_receiver_aliases
                        .get(&(receiver.start_byte(), receiver.end_byte()))
                        .cloned()
                {
                    let receiver_type = self.safe_receiver_java_type(receiver);
                    if let Some(workspace) = self.unit.workspace
                        && let Some(file) = workspace.source_file(
                            self.unit
                                .workspace_file
                                .as_deref()
                                .unwrap_or(self.unit.file),
                        )
                        && let Some(owner) = workspace.resolve_type(file, &receiver_type)
                    {
                        use crate::workspace::{MemberKind, SourceLanguage};
                        let setter = format!("set{}", crate::transpiler::unit::capitalize(&member));
                        let property = owner.members.iter().find(|candidate| {
                            candidate.name == member && candidate.kind == MemberKind::Property
                        });
                        if property.is_some_and(|property| property.is_jvm_field)
                            || owner.members.iter().any(|candidate| {
                                candidate.name == member
                                    && candidate.kind == MemberKind::Field
                                    && candidate.visibility.as_deref() == Some("public")
                            })
                        {
                            return format!("{alias}.{member}");
                        }
                        let lombok_setter = owner.language == SourceLanguage::Java
                            && workspace
                                .declaration_source_file(owner)
                                .is_some_and(|source| {
                                    workspace.has_default_lombok_accessors(&source.path)
                                        || self.unit.semantic_provider.is_some_and(|provider| {
                                            provider
                                                .java_property_read(
                                                    &crate::semantics::workspace_symbol(
                                                        workspace, owner,
                                                    ),
                                                    &member,
                                                )
                                                .is_some()
                                        })
                                })
                            && workspace
                                .declaration_source_file(owner)
                                .is_some_and(|source| {
                                    crate::lombok_getter_contracts::has_plain_public_setter(
                                        source.source_text(),
                                        &owner.name,
                                        &member,
                                    )
                                });
                        if lombok_setter
                            || (owner.language == SourceLanguage::Kotlin
                                && property.is_some_and(|property| {
                                    property.is_mutable
                                        && !property.setter_name_renamed
                                        && !property.has_custom_accessor
                                }))
                            || owner.members.iter().any(|candidate| {
                                candidate.kind == MemberKind::Method
                                    && candidate.name == setter
                                    && candidate.parameter_types.len() == 1
                                    && candidate.visibility.as_deref() == Some("public")
                            })
                        {
                            self.unit.pending_setter = true;
                            return format!("{alias}.{setter}(");
                        }
                    }
                    self.unit.diag_untranslatable(
                        node,
                        format!("safe assignment has no established writable field or setter for {receiver_type}"),
                    );
                    return "null".into();
                }
                // Assignment target: the property-read path emits getter calls
                // (`h.late = "x"` -> `h.getLate() = "x"`, illegal Java). A
                // known class property rewrites to its setter; unknown/local
                // targets pass through.
                let raw = self.unit.text(node).replace("?.", ".");
                if let Some(dot) = raw.rfind('.') {
                    let member = &raw[dot + 1..];
                    // Inside the setter for the SAME property
                    // (`fun setEquipmentSet(v) { this.equipmentSet = ... }`),
                    // `this.member = ...` is a FIELD write — rewriting it to
                    // `this.setMember(...)` re-enters the setter (infinite
                    // recursion) and flips the parameter type.
                    let in_own_setter =
                        self.unit.current_function_name.as_deref().is_some_and(|f| {
                            f.starts_with("set")
                                && f[3..]
                                    .chars()
                                    .next()
                                    .map(|c| c.to_ascii_lowercase().to_string())
                                    .map(|l| format!("{l}{}", &f[4..]))
                                    .is_some_and(|prop| prop == member)
                        });
                    if in_own_setter {
                        // plain field write: emit the raw `this.member` text
                        return raw;
                    }
                    if member
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_lowercase())
                    {
                        if let Some(setter) = self.unit.class_props.get(member) {
                            if setter.is_empty() {
                                self.unit.diag_untranslatable(
                                    node,
                                    format!(
                                        "assignment to `{}` is rejected in Kotlin (val or `private set`); declaration taints",
                                        member
                                    ),
                                );
                            } else {
                                self.unit.pending_setter = true;
                                return format!("{}{}(", &raw[..dot + 1], setter);
                            }
                        } else if let Some(setter) =
                            self.receiver_owned_property_setter(node, member)
                        {
                            self.unit.pending_setter = true;
                            return format!("{}{}(", &raw[..dot + 1], setter);
                        }
                    }
                }
                self.transpile(node)
            }
            _ => self.transpile(node),
        }
    }

    /// Assignment target resolution for a property OWNED BY THE RECEIVER's
    /// class: `other.parent = v` inside a method of any other class. The
    /// same-class registry (`class_props`) cannot know the receiver's
    /// properties, and falling through to the read path emits the illegal
    /// `other.getParent() = v`. Allowed only when the receiver spelling has a
    /// proven local/parameter type resolving to exactly one indexed Kotlin
    /// class whose plain public `var` property exposes the ordinary
    /// JavaBean setter ABI. `@JvmField` properties have no setter method,
    /// a custom accessor block can hide the setter (`private set`), and
    /// `@JvmName` can rename it — indexed as `setter_name_renamed`; all of
    /// those keep today's behavior. Declaration-site annotations such as
    /// serialization or tooling markers do not change the setter name and
    /// do NOT block this rewrite. Returns just the setter name; the caller
    /// composes `base + setter + "("`.
    fn receiver_owned_property_setter(
        &self,
        node: tree_sitter::Node,
        member: &str,
    ) -> Option<String> {
        let workspace = self.unit.workspace?;
        let declaring = self
            .unit
            .workspace_file
            .as_deref()
            .unwrap_or(self.unit.file);
        let source_file = workspace.source_file(declaring)?;
        let base = node.named_children(&mut node.walk()).next()?;
        if !matches!(base.kind(), "identifier" | "simple_identifier") {
            return None;
        }
        // Only a two-segment target (`other.parent`): a chained receiver base
        // would itself need read-path lowering that the raw spelling cannot
        // carry.
        if node.named_children(&mut node.walk()).count() != 2 {
            return None;
        }
        let receiver = self.unit.text(base).trim();
        if self.unit.text(node).contains("?.") {
            // Safe-call assignment targets need their own lowering; the
            // plain setter form would drop the null short-circuit.
            return None;
        }
        let source = self.unit.source;
        let call_scope = node.parent()?;
        let declared_type = crate::semantics::lexical_parameter_type(source, call_scope, receiver)
            .or_else(|| {
                self.unit.var_types.get(receiver).map(|stored| {
                    crate::semantics::TypeRef::from_java_name(
                        stored.strip_prefix("@Nullable ").unwrap_or(stored).trim(),
                    )
                })
            })?;
        if declared_type.nullable {
            // A nullable receiver needs a proven null check to write through
            // its setter; smart-cast rewiring is not this path's job.
            return None;
        }
        let type_spelling = match &declared_type.ty {
            crate::semantics::KotlinType::Named { name, .. } => name.clone(),
            crate::semantics::KotlinType::Primitive(name) => name.clone(),
            _ => return None,
        };
        if type_spelling.is_empty() {
            return None;
        }
        let declaration = workspace.resolve_type(source_file, &type_spelling)?;
        if declaration.language != crate::workspace::SourceLanguage::Kotlin {
            return None;
        }
        let property = declaration.members.iter().find(|candidate| {
            candidate.kind == crate::workspace::MemberKind::Property
                && candidate.name == member
                && !candidate.is_static
        })?;
        if !property.is_mutable
            || property.is_jvm_field
            || property.has_custom_accessor
            || property.setter_name_renamed
            || !matches!(
                property.visibility.as_deref(),
                None | Some("public") | Some("protected")
            )
        {
            return None;
        }
        // Ambiguous receiver type names (two indexed declarations resolving
        // through the same spelling) have no provable setter ABI.
        if workspace
            .declarations_named(&type_spelling)
            .filter(|candidate| {
                candidate.kind == declaration.kind && candidate.package == declaration.package
            })
            .count()
            != 1
        {
            return None;
        }
        Some(format!(
            "set{}",
            crate::transpiler::unit::capitalize(member)
        ))
    }

    /// A bare assignment in a translated subclass cannot write an inherited
    /// Kotlin property's private Java field. Rewrite only when the indexed
    /// supertype chain proves the inherited property is mutable and exposes
    /// the ordinary JavaBean setter ABI. Locals/parameters and properties
    /// declared on the current class keep their existing target handling.
    pub(crate) fn interface_property_setter(
        &self,
        name: &str,
        node: tree_sitter::Node,
    ) -> Option<String> {
        if self.unit.var_types.contains_key(name)
            || crate::semantics::lexical_parameter_type(self.unit.source, node, name).is_some()
        {
            return None;
        }
        let owner = std::iter::successors(node.parent(), |node| node.parent())
            .find(|node| node.kind() == "class_declaration")?;
        kt::child(owner, "interface")?;
        self.unit
            .class_props
            .get(name)
            .filter(|setter| !setter.is_empty())
            .cloned()
    }

    pub(crate) fn inherited_mutable_property_setter(
        &self,
        name: &str,
        node: tree_sitter::Node,
    ) -> Option<String> {
        if self.unit.var_types.contains_key(name)
            || crate::semantics::lexical_parameter_type(self.unit.source, node, name).is_some()
            || self.unit.class_props.contains_key(name)
        {
            return None;
        }
        let workspace = self.unit.workspace?;
        let path = self
            .unit
            .workspace_file
            .as_deref()
            .unwrap_or(self.unit.file);
        let owner_name = self.enclosing_type_name()?;
        let mut ancestor = node.parent();
        while let Some(scope) = ancestor {
            if matches!(
                scope.kind(),
                "lambda_literal" | "function_literal" | "anonymous_function"
            ) {
                // The receiver stack is not established for this scope.
                return None;
            }
            if scope.kind() == "function_declaration" {
                break;
            }
            ancestor = scope.parent();
        }
        workspace.inherited_mutable_property_setter_in_file(path, &owner_name, name)
    }

    fn inherited_property_target_is_unproven(&self, name: &str, node: tree_sitter::Node) -> bool {
        if self.unit.var_types.contains_key(name)
            || crate::semantics::lexical_parameter_type(self.unit.source, node, name).is_some()
            || self.unit.class_props.contains_key(name)
        {
            return false;
        }
        let Some(workspace) = self.unit.workspace else {
            return false;
        };
        let Some(owner_name) = self.enclosing_type_name() else {
            return false;
        };
        let path = self
            .unit
            .workspace_file
            .as_deref()
            .unwrap_or(self.unit.file);
        workspace
            .inherited_property_names_in_file(path, &owner_name)
            .iter()
            .any(|property| property == name)
    }

    fn when_expr(&mut self, node: tree_sitter::Node) -> String {
        if let Some(lowering) = self
            .unit
            .enum_when_lowerings
            .get(&node.start_byte())
            .cloned()
        {
            return self.planned_enum_when(node, &lowering);
        }
        self.unit.diags.warn_approx(
            node,
            self.unit.file,
            "when-expression lowered to switch or if/else",
        );
        let mut cursor = node.walk();
        let kids: Vec<_> = node.children(&mut cursor).collect();
        let subject = kids
            .iter()
            .find(|c| c.kind() == "when_subject")
            .and_then(|ws| ws.children(&mut ws.walk()).find(|c| c.is_named()))
            .or_else(|| {
                kids.iter()
                    .find(|c| c.is_named() && c.kind() != "when_entry")
                    .copied()
            });
        let entries: Vec<_> = kids
            .iter()
            .filter(|c| c.kind() == "when_entry")
            .copied()
            .collect();
        let subject_java = subject
            .as_ref()
            .map(|s| self.transpile(*s))
            .unwrap_or_default();
        let arms: Vec<(Vec<tree_sitter::Node>, Option<tree_sitter::Node>)> = entries
            .iter()
            .map(|entry| {
                let e_named: Vec<_> = entry
                    .children(&mut entry.walk())
                    .filter(|c| c.is_named())
                    .collect();
                let result = e_named.last().copied();
                let conditions = e_named[..e_named.len().saturating_sub(1)].to_vec();
                (conditions, result)
            })
            .collect();
        if let Some(lowered) = self.when_switch(subject, &subject_java, &arms) {
            return lowered;
        }
        self.when_if_else(node, subject, &subject_java, &arms)
    }

    fn planned_enum_when(
        &mut self,
        node: tree_sitter::Node,
        lowering: &crate::semantics::EnumWhenLowering,
    ) -> String {
        assert_eq!(lowering.location.snapshot_hash, self.unit.source_hash);
        assert_eq!(
            (lowering.location.start_byte, lowering.location.end_byte),
            (node.start_byte(), node.end_byte())
        );
        let locate = |location: &crate::semantics::SourceLocation| {
            assert_eq!(location.snapshot_hash, self.unit.source_hash);
            let expression = node
                .named_descendant_for_byte_range(location.start_byte, location.end_byte)
                .expect("planned when expression exists in its snapshot");
            assert_eq!(
                (expression.start_byte(), expression.end_byte()),
                (location.start_byte, location.end_byte)
            );
            expression
        };
        let subject = locate(&lowering.subject);
        let arms = lowering
            .arms
            .iter()
            .map(|arm| {
                (
                    arm.labels
                        .iter()
                        .map(|label| label.name.clone())
                        .collect::<Vec<_>>(),
                    locate(&arm.result),
                )
            })
            .collect::<Vec<_>>();
        let default = lowering.default_result.as_ref().map(locate);
        let subject_java = self.transpile(subject);
        let mut out = format!("switch ({subject_java}) {{\n");
        for (labels, result) in arms {
            out.push_str(&format!(
                "case {} -> {};\n",
                labels.join(", "),
                self.when_arm_body(Some(result))
            ));
        }
        let default = default
            .map(|result| self.when_arm_body(Some(result)))
            .unwrap_or_else(|| "throw new kotlin.NoWhenBranchMatchedException()".into());
        out.push_str(&format!("default -> {default};\n}}"));
        out
    }

    fn when_switch(
        &mut self,
        subject: Option<tree_sitter::Node>,
        subject_java: &str,
        arms: &[(Vec<tree_sitter::Node>, Option<tree_sitter::Node>)],
    ) -> Option<String> {
        if subject.is_none() || subject_java.is_empty() || arms.is_empty() {
            return None;
        }
        let mut cases = Vec::new();
        let mut default_arm = None;
        for (conditions, result) in arms {
            let is_else = conditions.is_empty()
                || conditions
                    .iter()
                    .all(|c| self.unit.text(*c).trim() == "else");
            if is_else {
                default_arm = Some(*result);
                continue;
            }
            let mut labels = Vec::new();
            for condition in conditions {
                labels.push(self.switch_case_label(*condition)?);
            }
            cases.push((labels, *result));
        }
        if cases.is_empty() {
            return None;
        }
        let mut out = format!("switch ({subject_java}) {{\n");
        for (labels, result) in cases {
            out.push_str(&format!(
                "case {} -> {};\n",
                labels.join(", "),
                self.when_arm_body(result)
            ));
        }
        out.push_str(&format!(
            "default -> {};\n}}",
            default_arm
                .flatten()
                .map(|result| self.when_arm_body(Some(result)))
                .unwrap_or_else(|| "throw new IllegalStateException()".to_string())
        ));
        Some(out)
    }

    fn switch_case_label(&self, node: tree_sitter::Node) -> Option<String> {
        match node.kind() {
            "number_literal" | "string_literal" => Some(self.unit.text(node).trim().to_string()),
            "navigation_expression" => node
                .children(&mut node.walk())
                .filter(|child| child.kind() == "identifier")
                .last()
                .map(|child| self.unit.text(child).trim().to_string())
                .filter(|label| !label.is_empty()),
            "identifier" => {
                let label = self.unit.text(node).trim().to_string();
                (!label.is_empty() && label != "else").then_some(label)
            }
            _ => None,
        }
    }

    fn when_arm_body(&mut self, result: Option<tree_sitter::Node>) -> String {
        match result {
            Some(node) if node.kind() == "throw_expression" => self.throw_expr(node),
            Some(node) => self.transpile(node),
            None => "null".to_string(),
        }
    }

    fn when_if_else(
        &mut self,
        node: tree_sitter::Node,
        subject: Option<tree_sitter::Node>,
        subject_java: &str,
        arms: &[(Vec<tree_sitter::Node>, Option<tree_sitter::Node>)],
    ) -> String {
        let yield_return = node
            .parent()
            .is_some_and(|parent| matches!(parent.kind(), "return_expression" | "function_body"));
        let mut out = String::new();
        let mut emitted_branch = false;
        for (conditions, result) in arms {
            let is_else = conditions.is_empty()
                || conditions
                    .iter()
                    .all(|c| self.unit.text(*c).trim() == "else");
            let mut cast_type = None;
            let mut cond_parts = Vec::new();
            for condition in conditions {
                let text = self.unit.text(*condition);
                if text.trim() == "else" {
                    continue;
                } else if condition.kind() == "range_test" {
                    cond_parts.push(self.range_test_cond(*condition, subject_java));
                } else if condition.kind() == "type_test" {
                    let ty = condition
                        .children(&mut condition.walk())
                        .find(|child| child.is_named())
                        .map(|child| self.unit.text(child).trim().replace(' ', ""))
                        .unwrap_or_default();
                    cond_parts.push(format!("{subject_java} instanceof {ty}"));
                    cast_type = Some(ty);
                } else {
                    let rendered = self.transpile(*condition);
                    if subject.is_some() && rendered != "true" {
                        cond_parts.push(format!("Objects.equals({subject_java}, {rendered})"));
                    } else {
                        cond_parts.push(rendered);
                    }
                }
            }
            let mut body = self.when_arm_body(*result);
            if let (Some(ty), Some(_)) = (&cast_type, subject) {
                body = body.replace(
                    &format!("{subject_java}."),
                    &format!("(({}) {}).", ty, subject_java),
                );
            }
            let body = if body.trim_start().starts_with("throw ") {
                format!("{body};")
            } else if yield_return {
                format!("return {body};")
            } else {
                format!("{body};")
            };
            if is_else {
                out.push_str(&format!("else {{\n{body}\n}}"));
            } else if !emitted_branch {
                let cond = if cond_parts.is_empty() {
                    "true".to_string()
                } else {
                    cond_parts.join(" || ")
                };
                out.push_str(&format!("if ({cond}) {{\n{body}\n}}"));
                emitted_branch = true;
            } else {
                let cond = cond_parts.join(" || ");
                out.push_str(&format!(" else if ({cond}) {{\n{body}\n}}"));
            }
        }
        if out.is_empty() {
            "null".to_string()
        } else {
            out
        }
    }

    /// A `super_expression` (`super`, or a qualified `SuperType.super`):
    /// the qualifying supertype matters for member access policy
    /// (`super.<member>` must become `SupName.super.member` in Java). The
    /// AST qualifier is authoritative when present; otherwise the
    /// enclosing declaration's supertype list supplies it via the index
    /// (`navigation()` resolves and taints). Pure text here: any
    /// workspace-policy decision happens in `navigation()`.
    fn transpile_super_qualifier(&mut self, node: tree_sitter::Node) -> String {
        match kt::child(node, "user_type") {
            Some(ty) => kt::java_type(ty, self.unit.source).trim().to_string(),
            None => "super".to_string(),
        }
    }

    fn throw_expr(&mut self, node: tree_sitter::Node) -> String {
        let ctor = node
            .children(&mut node.walk())
            .find(|child| child.is_named())
            .map(|child| self.transpile(child))
            .unwrap_or_else(|| "RuntimeException()".to_string());
        if ctor.trim_start().starts_with("new ") {
            format!("throw {ctor}")
        } else {
            format!("throw new {ctor}")
        }
    }

    /// `x as T` -> `(T) x`; `x as? T` -> a null-safe ternary cast. Kotlin `as`
    /// on a receiver that is smart-cast-safe is always a hard cast in Java.
    fn as_expr(&mut self, node: tree_sitter::Node) -> String {
        let mut cursor = node.walk();
        let kids: Vec<_> = node.children(&mut cursor).collect();
        let left = kids.iter().find(|c| c.is_named()).copied();
        let ty = kids
            .iter()
            .filter(|c| c.is_named())
            .nth(1)
            .map(|t| box_primitive(&kt::java_type(*t, self.unit.source)))
            .unwrap_or_else(|| "Object".to_string());
        let nullable = kids.iter().any(|c| self.unit.text(*c).trim() == "as?");
        let Some(left) = left else {
            self.unit
                .diag_untranslatable(node, "`as` cast with no subject expression".to_string());
            return "null".to_string();
        };
        let l_java = self.transpile(left);
        if nullable {
            self.unit.diags.warn_approx(
                node,
                self.unit.file,
                format!("`as?` cast to `{ty}` lowered to instanceof-check cast"),
            );
            format!(
                "({} instanceof {} ? ({}) {} : null)",
                l_java, ty, ty, l_java
            )
        } else {
            format!("(({}) {})", ty, l_java)
        }
    }

    /// `expr!!` (branching not-null assertion) and Java-expressible postfix
    /// unary forms (`++`/`-`/`+`/`!`). `!!` has no Java operator; the honest
    /// lowering is `Objects.requireNonNull(expr)` so NPE timing is preserved.
    fn unary_expr(&mut self, node: tree_sitter::Node) -> String {
        let mut cursor = node.walk();
        let kids: Vec<_> = node.children(&mut cursor).collect();
        let is_not_null = kids
            .iter()
            .any(|c| c.kind() == "!!" || self.unit.text(*c).trim() == "!!");
        if is_not_null {
            let arg = kids
                .iter()
                .find(|c| c.kind() == "argument" || c.is_named())
                .copied();
            let Some(arg) = arg else {
                self.unit.diag_untranslatable(
                    node,
                    "`!!` assertion with no subject expression".to_string(),
                );
                return "null".to_string();
            };
            let a_java = self.transpile(arg);
            if a_java.contains("?") || a_java.contains(" instanceof ") {
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "`!!` on a lowered (ternary/instanceof) expression: null-check is skipped inside the cast",
                );
                return a_java;
            }
            self.unit.diags.warn_approx(
                node,
                self.unit.file,
                "`!!` lowered to Objects.requireNonNull (throws NPE at the same point)",
            );
            return format!("java.util.Objects.requireNonNull({})", a_java);
        }
        // Other postfix/prefix unary forms: map operator -> text directly.
        let mut op = String::new();
        let mut arg_text = None;
        for c in &kids {
            if c.is_named() {
                arg_text = Some(self.transpile(*c));
            } else {
                let t = self.unit.text(*c).trim().to_string();
                match t.as_str() {
                    "-" | "+" | "!" | "++" | "--" => op = t,
                    _ => {}
                }
            }
        }
        let Some(arg_text) = arg_text else {
            return "null".to_string();
        };
        if op.is_empty() {
            arg_text
        } else if op == "++" || op == "--" {
            format!("{}{}", arg_text, op)
        } else {
            format!("{}{}", op, arg_text)
        }
    }
    /// `x in lo..hi` when-condition -> `x >= lo && x <= hi` (`!in` negated).
    /// The subject is the when-expression's subject, not part of the node.
    fn range_test_cond(&mut self, cond: tree_sitter::Node, subject: &str) -> String {
        let mut cursor = cond.walk();
        let kids: Vec<_> = cond.children(&mut cursor).collect();
        let negate = kids.iter().any(|c| self.unit.text(*c).trim() == "!in");
        let range = kids.iter().find(|c| c.is_named()).copied();
        let Some(range) = range else {
            return "true".to_string();
        };
        let mut rc = range.walk();
        let rkids: Vec<_> = range.children(&mut rc).filter(|c| c.is_named()).collect();
        if rkids.len() != 2 {
            // not a plain a..b range — degrade to a true condition
            return "true".to_string();
        }
        let lo = self.transpile(rkids[0]);
        let hi = self.transpile(rkids[1]);
        if subject.is_empty() {
            return "true".to_string();
        }
        let in_range = format!("{} >= {} && {} <= {}", subject, lo, subject, hi);
        if negate {
            format!("!({})", in_range)
        } else {
            in_range
        }
    }

    fn jump(&mut self, node: tree_sitter::Node) -> String {
        let mut cursor = node.walk();
        let kids: Vec<_> = node
            .children(&mut cursor)
            .filter(|c| c.is_named())
            .collect();
        let kind = self.unit.text(node);
        if kind.starts_with("break") {
            "break".to_string()
        } else if kind.starts_with("continue") {
            "continue".to_string()
        } else {
            // return with expression (in expression context)
            kids.first().map(|e| self.transpile(*e)).unwrap_or_default()
        }
    }

    fn narrowed_receiver(&self, source: &str, java: &str) -> String {
        match self.unit.flow_smart_casts.get(source) {
            Some(ty) => format!("(({ty}) {java})"),
            None => java.to_string(),
        }
    }

    fn if_expr(&mut self, node: tree_sitter::Node) -> String {
        // Supported simple `if` expressions lower directly to a Java ternary.
        // Only malformed/complex shapes below emit an approximation warning.
        let mut cursor = node.walk();
        let kids: Vec<tree_sitter::Node> = node.children(&mut cursor).collect();
        // Condition: first named child between the `(` and `)` tokens —
        // the if-condition can be ANY expression kind (call, binary, is,
        // paren). Field-based detection misses call conditions.
        let cond_node: Option<tree_sitter::Node> = {
            let mut c2 = node.walk();
            let all: Vec<_> = node.children(&mut c2).collect();
            let mut inner = Vec::new();
            let mut depth = 0;
            for c in all {
                if c.kind() == "(" {
                    depth = 1;
                    continue;
                }
                if depth == 1 {
                    if c.kind() == ")" {
                        break;
                    }
                    if c.is_named() {
                        inner.push(c);
                    }
                }
            }
            inner.first().copied()
        };
        let cond = cond_node.map(|c| {
            let inner = unwrap_paren_node(c);
            self.transpile(inner)
        });
        // Narrow only the checked stable receiver, and only in the branch
        // where the test proves its type. Restoring the prior environment
        // also keeps nested conditions from leaking into sibling branches.
        let narrowing = cond_node
            .map(|c| unwrap_paren_node(c))
            .filter(|c| c.kind() == "is_expression")
            .and_then(|c| {
                let children = c
                    .children(&mut c.walk())
                    .filter(|k| k.is_named())
                    .collect::<Vec<_>>();
                let target = unwrap_paren_node(*children.first()?);
                if !matches!(target.kind(), "identifier" | "this_expression") {
                    return None;
                }
                let name = self.unit.text(target).trim();
                if target.kind() == "this_expression"
                    && (name != "this" || self.unit.ext_receiver_name.is_some())
                {
                    return None;
                }
                let ty = box_primitive(&kt::java_type(*children.get(1)?, self.unit.source));
                Some((name.to_string(), ty, self.unit.text(c).contains("!is")))
            });
        if let Some((name, ty, _)) = &narrowing
            && self
                .unit
                .flow_smart_casts
                .get(name)
                .is_some_and(|prior| prior != ty)
        {
            self.unit.diag_untranslatable(
                node,
                "overlapping smart casts require a proven intersection type",
            );
        }
        // Branches: named children after the condition. When the condition is
        // a field-named node we skip only that node; else skip all up to `else`.
        let cond_id = cond_node.map(|c| c.id());
        let branches: Vec<_> = kids
            .iter()
            .filter(|c| c.is_named() && c.kind() != "is_expression" && Some(c.id()) != cond_id)
            .copied()
            .collect();
        match (cond.clone(), branches.len()) {
            (Some(c), 2) => {
                let prior = self.unit.flow_smart_casts.clone();
                if let Some((name, ty, false)) = &narrowing {
                    self.unit.flow_smart_casts.insert(name.clone(), ty.clone());
                }
                let a = self.transpile(branches[0]);
                self.unit.flow_smart_casts.clone_from(&prior);
                if let Some((name, ty, true)) = &narrowing {
                    self.unit.flow_smart_casts.insert(name.clone(), ty.clone());
                }
                let b = self.transpile(branches[1]);
                self.unit.flow_smart_casts = prior;
                // Both arms are void calls (println etc.)? A statement-level
                // ternary over void calls is illegal Java — emit real if/else.
                let arms_void =
                    a.contains("System.out.println") || b.contains("System.out.println");
                if arms_void {
                    return format!(
                        "if ({}) {{\n{};\n}} else {{\n{};\n}}",
                        c,
                        a.trim_matches('(').trim_matches(')'),
                        b
                    );
                }
                format!("({} ? {} : {})", c, a, b)
            }
            _ => {
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "if-expression shape could not be represented exactly as a Java ternary",
                );
                // Fallback: emit first/last named children as ternary arms
                if let (Some(c), 1) = (cond, branches.len()) {
                    let a = self.transpile(branches[0]);
                    format!("({} ? {} : null)", c, a)
                } else {
                    "null".to_string()
                }
            }
        }
    }
}

fn unwrap_paren_node(mut node: tree_sitter::Node) -> tree_sitter::Node {
    while matches!(node.kind(), "parenthesized" | "parenthesized_expression") {
        let Some(inner) = node.children(&mut node.walk()).find(|c| c.is_named()) else {
            break;
        };
        node = inner;
    }
    node
}

/// Map primitive Java types to their boxed forms (for instanceof/casts).
fn box_primitive(ty: &str) -> String {
    match ty {
        "int" => "Integer".to_string(),
        "long" => "Long".to_string(),
        "short" => "Short".to_string(),
        "byte" => "Byte".to_string(),
        "double" => "Double".to_string(),
        "float" => "Float".to_string(),
        "boolean" => "Boolean".to_string(),
        "char" => "Character".to_string(),
        other => other.to_string(),
    }
}
