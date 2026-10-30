//! Call-expression translation: callee rewriting, stdlib/collection
//! dispatch, trailing lambdas.

use super::Expr;
use crate::semantics::string_case::StringCaseLocale;
use crate::transpiler::kt;
use crate::transpiler::unit::{instance_property_count, primitive_array_factory};

impl<'a, 'src, 'tree> Expr<'a, 'src, 'tree> {
    fn function_return_type_node<'node>(
        &self,
        function: tree_sitter::Node<'node>,
    ) -> Option<tree_sitter::Node<'node>> {
        let mut after_parameters = false;
        for child in function.children(&mut function.walk()) {
            if child.kind() == "function_value_parameters" {
                after_parameters = true;
                continue;
            }
            if after_parameters
                && matches!(
                    child.kind(),
                    "user_type" | "nullable_type" | "parenthesized_type"
                )
            {
                return Some(child);
            }
        }
        None
    }

    fn expected_map_factory_type_arguments(
        &self,
        call: tree_sitter::Node<'_>,
    ) -> Option<(String, String)> {
        let mut value = call;
        let mut context = value.parent()?;
        while context.kind() == "parenthesized_expression" {
            value = context;
            context = value.parent()?;
        }

        let type_node = match context.kind() {
            "function_body" => {
                let function = context
                    .parent()
                    .filter(|node| node.kind() == "function_declaration")?;
                self.function_return_type_node(function)?
            }
            "return_expression" => {
                let mut owner = context.parent()?;
                while owner.kind() != "function_declaration" {
                    if matches!(
                        owner.kind(),
                        "lambda_literal" | "function_literal" | "anonymous_function"
                    ) {
                        return None;
                    }
                    owner = owner.parent()?;
                }
                self.function_return_type_node(owner)?
            }
            "property_declaration" => {
                let declaration = kt::child(context, "variable_declaration")?;
                kt::child(declaration, "user_type")
                    .or_else(|| kt::child(declaration, "nullable_type"))?
            }
            "class_parameter" => {
                kt::child(context, "user_type").or_else(|| kt::child(context, "nullable_type"))?
            }
            _ => return None,
        };

        let (name, args) = if let Some(provider) = self.unit.semantic_provider {
            let candidate = if type_node.kind() == "nullable_type" {
                kt::child(type_node, "user_type")?
            } else {
                type_node
            };
            let raw = self.unit.text(candidate).trim();
            let classifier = raw.split('<').next().unwrap_or(raw).trim();
            if matches!(
                classifier,
                "Map" | "MutableMap" | "kotlin.collections.Map" | "kotlin.collections.MutableMap"
            ) {
                self.map_type_name_and_arguments(type_node)?
            } else {
                if candidate.kind() != "user_type" || raw.contains('<') || raw.contains('.') {
                    return None;
                }
                let mut root = candidate;
                while let Some(parent) = root.parent() {
                    root = parent;
                }
                let package = root
                    .named_children(&mut root.walk())
                    .find(|node| node.kind() == "package_header")
                    .and_then(|header| {
                        header
                            .named_children(&mut header.walk())
                            .find(|node| node.kind() == "qualified_identifier")
                    })
                    .and_then(|node| node.utf8_text(self.unit.source.as_bytes()).ok())
                    .unwrap_or_default()
                    .replace(' ', "");
                let crate::semantics::FactStatus::Established(arguments) =
                    provider.map_alias_type_arguments(self.unit.file, &package, raw)
                else {
                    return None;
                };
                ("Map".to_string(), vec![arguments.key, arguments.value])
            }
        } else {
            self.map_type_name_and_arguments(type_node)?
        };
        if !matches!(
            name.as_str(),
            "Map" | "MutableMap" | "kotlin.collections.Map" | "kotlin.collections.MutableMap"
        ) || args.len() != 2
        {
            return None;
        }
        Some((args[0].clone(), args[1].clone()))
    }

    fn map_type_name_and_arguments(
        &self,
        type_node: tree_sitter::Node<'_>,
    ) -> Option<(String, Vec<String>)> {
        self.map_type_name_and_arguments_depth(type_node, 0)
    }

    fn map_type_name_and_arguments_depth(
        &self,
        type_node: tree_sitter::Node<'_>,
        alias_depth: usize,
    ) -> Option<(String, Vec<String>)> {
        if alias_depth > 16 {
            return None;
        }
        let source = self.unit.source;
        if type_node.kind() == "nullable_type" {
            return self.map_type_name_and_arguments_depth(
                kt::child(type_node, "user_type")?,
                alias_depth,
            );
        }
        if type_node.kind() != "user_type" {
            return None;
        }
        let raw = self.unit.text(type_node).trim();
        let name = raw.split('<').next()?.trim();
        let args_node = kt::child(type_node, "type_arguments");
        if !matches!(
            name,
            "Map" | "MutableMap" | "kotlin.collections.Map" | "kotlin.collections.MutableMap"
        ) {
            if args_node.is_some() {
                return None;
            }
            let tree = crate::transpiler::parse_tree(self.unit.source);
            let mut stack = vec![tree.root_node()];
            while let Some(node) = stack.pop() {
                if node.kind() == "type_alias"
                    && kt::field(node, "type").is_some_and(|alias| self.unit.text(alias) == name)
                {
                    if kt::child(node, "type_parameters").is_some() {
                        return None;
                    }
                    return node
                        .named_children(&mut node.walk())
                        .find(|child| child.kind() == "user_type")
                        .and_then(|target_node| {
                            self.map_type_name_and_arguments_depth(target_node, alias_depth + 1)
                        });
                }
                stack.extend(node.named_children(&mut node.walk()));
            }
            // The current source does not own the alias declaration, so its
            // type-parameter shape is not available to this conservative proof.
            return None;
        }
        let args_node = args_node?;
        let mut args = Vec::new();
        for projection in args_node.named_children(&mut args_node.walk()) {
            if projection.kind() != "type_projection"
                || self.unit.text(projection).trim().starts_with("in ")
                || self.unit.text(projection).trim().starts_with("out ")
                || self.unit.text(projection).trim() == "*"
            {
                return None;
            }
            let mut cursor = projection.walk();
            let mut children = projection.named_children(&mut cursor);
            let ty = children.next()?;
            if children.next().is_some() {
                return None;
            }
            let java = kt::java_type_ann(ty, source, self.unit.annots);
            let java = java.trim_start_matches("@Nullable ").trim();
            let boxed = crate::transpiler::types::boxed_name(java).unwrap_or(java);
            if boxed.contains('?') || boxed.is_empty() {
                return None;
            }
            args.push(boxed.to_string());
        }
        Some((name.to_string(), args))
    }

    fn proven_collection_factory(
        &self,
        call: tree_sitter::Node<'_>,
        callee: Option<tree_sitter::Node<'_>>,
        name: &str,
    ) -> bool {
        let (Some(callee), Some(provider)) = (callee, self.unit.semantic_provider) else {
            return false;
        };
        crate::semantics::known_collection_factory_shape(
            self.unit.source,
            call,
            callee,
            name,
            provider,
        )
    }

    fn map_factory_has_direct_null_component(&self, call: tree_sitter::Node<'_>) -> bool {
        let Some(arguments) = kt::child(call, "value_arguments") else {
            return false;
        };
        arguments
            .named_children(&mut arguments.walk())
            .any(|argument| {
                if argument.kind() != "value_argument" {
                    return false;
                }
                let Some(expression) = argument.named_children(&mut argument.walk()).next() else {
                    return false;
                };
                if expression.kind() != "infix_expression" {
                    return false;
                }
                let children: Vec<_> = expression
                    .children(&mut expression.walk())
                    .filter(|child| child.is_named())
                    .collect();
                if children.len() != 3 || self.unit.text(children[1]).trim() != "to" {
                    return false;
                }
                [children[0], children[2]].into_iter().any(|mut component| {
                    while component.kind() == "parenthesized_expression" {
                        let Some(inner) = component.named_children(&mut component.walk()).next()
                        else {
                            return false;
                        };
                        component = inner;
                    }
                    component.kind() == "identifier" && self.unit.text(component).trim() == "null"
                })
            })
    }

    pub(crate) fn call(&mut self, node: tree_sitter::Node) -> String {
        let mut result = self.call_body(node);
        if let Some(fact) = self
            .unit
            .container_calls
            .get(&(node.start_byte(), node.end_byte()))
        {
            assert_eq!(fact.location.snapshot_hash, self.unit.source_hash);
            if let Some(expected) = &fact.expected_type
                && let crate::semantics::KotlinType::Named { name, .. } = &expected.ty.ty
                && let Some(wrapper) = fact.return_type.as_deref()
                && matches!(
                    (name.rsplit('.').next().unwrap_or(name), wrapper),
                    ("Optional", "java.util.Optional") | ("Stream", "java.util.stream.Stream")
                )
            {
                assert_eq!(expected.location.snapshot_hash, self.unit.source_hash);
                let mut root = node;
                while let Some(parent) = root.parent() {
                    root = parent;
                }
                let ty_node = root
                    .named_descendant_for_byte_range(
                        expected.location.start_byte,
                        expected.location.end_byte,
                    )
                    .expect("planned result type exists in its snapshot");
                let target = kt::java_type(ty_node, self.unit.source);
                result = format!("({target}) ({wrapper}<?>) ({result})");
            }
            if let Some(message) = &fact.result_check {
                return format!(
                    "java.util.Objects.requireNonNull({result}, {})",
                    serde_json::to_string(message)
                        .expect("null-check message is a Java string literal")
                );
            }
        }
        if let Some(fact) = self
            .unit
            .exact_overload_calls
            .get(&(node.start_byte(), node.end_byte()))
            && let Some(message) = &fact.result_check
        {
            assert_eq!(fact.location.snapshot_hash, self.unit.source_hash);
            return format!(
                "java.util.Objects.requireNonNull({result}, {})",
                serde_json::to_string(message)
                    .expect("planned null-check message is a Java string literal")
            );
        }
        result
    }

    fn call_body(&mut self, node: tree_sitter::Node) -> String {
        if let Some(expression) = self.planned_eager_collection_call(node) {
            return expression;
        }
        if let Some(fact) = self
            .unit
            .reified_class_calls
            .get(&(node.start_byte(), node.end_byte()))
            .cloned()
        {
            assert_eq!(fact.location.snapshot_hash, self.unit.source_hash);
            assert_eq!(fact.receiver.snapshot_hash, self.unit.source_hash);
            let mut root = node;
            while let Some(parent) = root.parent() {
                root = parent;
            }
            let receiver = root
                .named_descendant_for_byte_range(fact.receiver.start_byte, fact.receiver.end_byte)
                .expect("planned reified receiver exists in its snapshot");
            let receiver_java = self.transpile(receiver);
            let mut type_parts = Vec::new();
            if !fact.type_argument.package.is_empty() {
                type_parts.push(fact.type_argument.package.clone());
            }
            type_parts.extend(fact.type_argument.owner_path.clone());
            type_parts.push(fact.type_argument.name.clone());
            // Kotlin inlines the wrapper's call against its declaring class.
            // Anchor Java lookup there too, even if surrounding flow casts
            // narrowed the rendered receiver to a subtype or interface.
            let mut owner_parts = Vec::new();
            if !fact.java_target.package.is_empty() {
                owner_parts.push(fact.java_target.package.clone());
            }
            owner_parts.extend(fact.java_target.owner_path.clone());
            return format!(
                "(({}) {receiver_java}).{}({}.class)",
                owner_parts.join("."),
                fact.java_target.name,
                type_parts.join(".")
            );
        }
        if let Some(fact) = self
            .unit
            .reference_string_calls
            .get(&(node.start_byte(), node.end_byte()))
            .cloned()
            && fact.kind == crate::semantics::ReferenceStringCallKind::ObjectToString
            && let crate::semantics::ReferenceStringReceiver::Returned {
                expression,
                nullability,
                ..
            } = fact.receiver
            && nullability == crate::semantics::ReferenceReturnNullability::Nullable
        {
            assert_eq!(fact.location.snapshot_hash, self.unit.source_hash);
            assert_eq!(expression.snapshot_hash, self.unit.source_hash);
            let receiver = node
                .named_descendant_for_byte_range(expression.start_byte, expression.end_byte)
                .expect("planned reference receiver exists in its snapshot");
            assert_eq!(
                (receiver.start_byte(), receiver.end_byte()),
                (expression.start_byte, expression.end_byte)
            );
            // The Object overload preserves Kotlin Any?.toString, including
            // null, without duplicating the producer's evaluation.
            return format!(
                "java.lang.String.valueOf((java.lang.Object) {})",
                self.transpile(receiver)
            );
        }
        if let Some(lowering) = self.unit.todo_lowerings.get(&node.start_byte()).cloned() {
            assert_eq!(lowering.location.snapshot_hash, self.unit.source_hash);
            assert_eq!(
                (lowering.location.start_byte, lowering.location.end_byte),
                (node.start_byte(), node.end_byte())
            );
            let message = if let Some(reason) = lowering.reason {
                assert_eq!(reason.snapshot_hash, self.unit.source_hash);
                let expression = node
                    .named_descendant_for_byte_range(reason.start_byte, reason.end_byte)
                    .expect("planned TODO reason exists in its snapshot");
                assert_eq!(
                    (expression.start_byte(), expression.end_byte()),
                    (reason.start_byte, reason.end_byte)
                );
                format!(
                    "\"An operation is not implemented: \" + {}",
                    self.transpile(expression)
                )
            } else {
                "\"An operation is not implemented.\"".into()
            };
            return format!("throw new kotlin.NotImplementedError({message})");
        }
        let mut cursor = node.walk();
        let kids: Vec<_> = node.children(&mut cursor).collect();

        // callee may be identifier or navigation_expression
        let callee = kids.iter().find(|c| c.is_named()).copied();
        let args_node = kids.iter().find(|c| c.kind() == "value_arguments").copied();
        // trailing lambda: direct lambda_literal or wrapped in annotated_lambda
        let lambda_arg = kids
            .iter()
            .find(|c| c.kind() == "lambda_literal")
            .copied()
            .or_else(|| {
                kids.iter()
                    .find(|c| c.kind() == "annotated_lambda")
                    .and_then(|al| kt::child(*al, "lambda_literal"))
            });
        let lambda_arg = lambda_arg.or_else(|| {
            if self.unit.text(node).contains("anyMatch") {
                let mut stack: Vec<tree_sitter::Node> = kids.to_vec();
                while let Some(current) = stack.pop() {
                    if current.kind() == "lambda_literal" {
                        return Some(current);
                    }
                    stack.extend(current.children(&mut current.walk()));
                }
            }
            None
        });

        let mut args: Vec<String> = Vec::new();
        let mut copy_arg_values: Vec<String> = Vec::new();
        let mut arg_names: Vec<Option<String>> = Vec::new();
        if let Some(an) = args_node {
            let mut inner = an.walk();
            for arg in an.children(&mut inner) {
                if arg.kind() == "value_argument" {
                    let mut ac = arg.walk();
                    let namedkids: Vec<_> = arg
                        .children(&mut ac)
                        .filter(|c| c.is_named())
                        .collect::<Vec<_>>();
                    // `y = 9`: two named kids (identifier + value) and the
                    // raw text carries `=`. Emit `y = 9` so data-class copy
                    // reassembly can substitute by component name.
                    if namedkids.len() == 2
                        && let Some(name) = named_argument_name(self.unit.text(arg))
                    {
                        arg_names.push(Some(name.clone()));
                        let value = self.transpile(namedkids[1]);
                        copy_arg_values.push(value.clone());
                        args.push(format!("{name} = {value}"));
                        continue;
                    }
                    arg_names.push(None);
                    let expr = namedkids
                        .first()
                        .map(|e| {
                            if e.kind() == "spread_expression" {
                                e.children(&mut e.walk())
                                    .find(|c| c.is_named())
                                    .map(|inner| self.transpile(inner))
                                    .unwrap_or_default()
                            } else {
                                self.transpile(*e)
                            }
                        })
                        .unwrap_or_default();
                    copy_arg_values.push(expr.clone());
                    args.push(expr);
                } else if arg.kind() == "named_argument" {
                    // `y = 9` — keep as `y = 9` text so downstream maps
                    // (data-class copy reassembly) see both halves.
                    let mut ac = arg.walk();
                    let pair: Vec<_> = arg.children(&mut ac).collect::<Vec<_>>();
                    if pair.len() >= 2 {
                        // identifier, then value
                        let name = self.unit.text(pair[0]).trim().to_string();
                        let vexpr = pair[pair.len() - 1];
                        arg_names.push(Some(name.clone()));
                        let value = self.transpile(vexpr);
                        copy_arg_values.push(value.clone());
                        args.push(format!("{name} = {value}"));
                    }
                }
            }
        }
        if let Some(l) = lambda_arg {
            arg_names.push(None);
            let value = self.transpile(l);
            copy_arg_values.push(value.clone());
            args.push(value);
        }

        if let Some(fact) = self
            .unit
            .ordinary_source_calls
            .get(&(node.start_byte(), node.end_byte()))
            .cloned()
        {
            assert_eq!(fact.location.snapshot_hash, self.unit.source_hash);
            let callee = callee.expect("planned source call has a callee");
            let java = if callee.kind() == "navigation_expression" {
                let (base, _) = self
                    .unit
                    .nav_base_member(callee)
                    .expect("planned member call has a receiver");
                format!("{}.{}", self.transpile(base), fact.target.name)
            } else {
                fact.target.name
            };
            return format!("{java}({})", args.join(", "));
        }

        if let Some(provider) = self.unit.semantic_provider
            && let Some(classifier) = crate::semantics::established_platform_constructor(
                self.unit.source,
                self.unit
                    .workspace_file
                    .as_deref()
                    .unwrap_or(self.unit.file),
                node,
                provider,
            )
        {
            return format!("new {}({})", classifier, args.join(", "));
        }

        // Navigation callee with type context: array `.size`/`.length` and
        // same-file extension call sites are rewritten BEFORE the generic
        // callee path so private messages don't fire for them.
        if let Some(nav) = kids
            .iter()
            .find(|c| c.kind() == "navigation_expression")
            .copied()
            && let Some((base, member)) = self.unit.nav_base_member(nav)
        {
            if let Some(case) = self.unit.string_case_call(node)
                && case.argument_count() == args.len()
                && case.receiver.start_byte == base.start_byte()
                && case.receiver.end_byte == base.end_byte()
            {
                // Kotlin's no-argument case conversion is invariant; Java's
                // no-argument methods use the process default, so the proven
                // fact pins `Locale.ROOT`. An explicit Locale argument is
                // passed through untouched.
                //
                // `args` keeps the `name = value` spelling so data-class copy
                // reassembly can match components by name; a Java method call
                // takes the value only. `copy_arg_values` holds the same
                // arguments in the same order with that reassembly prefix
                // stripped, so named-Locale call sites read it here instead.
                let locale = match case.locale {
                    StringCaseLocale::Invariant => "java.util.Locale.ROOT".to_string(),
                    StringCaseLocale::Explicit => copy_arg_values
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "java.util.Locale.ROOT".to_string()),
                };
                return format!(
                    "({}).{}({})",
                    self.transpile(base),
                    case.direction.java_method(),
                    locale
                );
            }
            // Kotlin String.split is a literal-delimiter operation which
            // keeps trailing empty fields. Java String.split is regex-based
            // and drops them, so route the exact one-String form through the
            // Kotlin stdlib. The receiver type gate avoids rewriting a
            // user-defined `split` on an unrelated receiver.
            if member == "split"
                && args.len() == 1
                && self.is_known_string_receiver(base)
                && args_node.is_some_and(|arguments| {
                    arguments
                        .children(&mut arguments.walk())
                        .find(|child| child.kind() == "value_argument")
                        .filter(|argument| !self.unit.text(*argument).contains('='))
                        .and_then(|argument| {
                            argument
                                .children(&mut argument.walk())
                                .find(|child| child.is_named())
                        })
                        .is_some_and(|argument| self.is_known_string_expression(argument))
                })
            {
                let receiver = self.transpile(base);
                return format!(
                    "kotlin.text.StringsKt.split((java.lang.CharSequence) ({}), new java.lang.String[]{{{}}}, false, 0)",
                    receiver, args[0]
                );
            }
            // `last()` on a known list, including the List returned above,
            // must throw on an empty list and preserve a nullable last
            // element. Stream.reduce(...).orElse(null) does neither.
            if member == "last" && args.is_empty() {
                let split_receiver = self.is_string_split_result(base);
                if split_receiver || self.is_known_list_receiver(base) {
                    let receiver = self.transpile(base);
                    return format!("kotlin.collections.CollectionsKt.last({})", receiver);
                }
            }
            if self.unit.receiver_is_array(base) && matches!(member.as_str(), "size" | "length") {
                // Kotlin `arr.size()`/`arr.size` -> Java `arr.length`.
                return format!("{}.length", self.transpile(base));
            }
            // Map/collection operator members (Map.plus / Map.minus / error
            // only path) emit broken Java when passed through verbatim.
            // Conservative rule: member calls named plus/minus/times on a
            // receiver whose type resolves to a Map taint the caller. A
            // user-defined operator overload keeps its emission (it nests a
            // real method) — detectability lives in the workspace.
            if matches!(member.as_str(), "plus" | "minus" | "times") {
                let recv_text = self.unit.text(base);
                if ["mapOf(", "mutableMapOf(", "hashMapOf("]
                    .iter()
                    .any(|prefix| recv_text.trim().starts_with(prefix))
                {
                    self.unit.diag_untranslatable(
                        node,
                        "Kotlin Map.plus on a map factory has no Java expression form; declaration retained in Kotlin",
                    );
                    return "null".to_string();
                }
                // Receiver type: local inference first, then the file-scoped
                // property/getter lookup (bare `contexts`, `this.getContexts()`,
                // etc.) — cross-file same-name properties must not win over
                // the declaring file's own member.
                let prop = recv_text
                    .trim()
                    .rsplit('.')
                    .next()
                    .unwrap_or("")
                    .trim_end_matches("()")
                    .trim_start_matches("this.");
                let recv_ty = self
                    .infer_operand_type(base)
                    .or_else(|| self.scope_property_type(prop))
                    .or_else(|| {
                        self.scope_property_type(
                            recv_text
                                .trim()
                                .trim_start_matches("this.")
                                .trim_end_matches("()"),
                        )
                    });
                if recv_ty.is_some_and(|ty| {
                    ty.contains("Map<")
                        || ty.contains("List<")
                        || ty.contains("Set<")
                        || ty.contains("Collection<")
                        || ty.contains("Iterable<")
                }) {
                    self.unit.diag_untranslatable(
                        nav,
                        format!(
                            "Map `{member}` member call: collection algebra has no Java member form; declaration retained in Kotlin"
                        ),
                    );
                    return "null".to_string();
                }
            }
            if matches!(
                member.as_str(),
                "filterValues"
                    | "mapKeys"
                    | "associateBy"
                    | "filterNotNull"
                    | "filterIsInstance"
                    | "minus"
            ) {
                let base_java = if base.kind() == "navigation_expression" {
                    self.navigation_call(base)
                } else {
                    self.transpile(base)
                };
                if member == "filterNotNull" {
                    self.unit.diags.warn_approx(
                        nav,
                        self.unit.file,
                        "Iterable.filterNotNull lowered to stream().filter(Objects::nonNull).collect(toList())",
                    );
                    self.unit.pending_full_call = true;
                    return format!(
                        "{base_java}.stream().filter(Objects::nonNull).collect(java.util.stream.Collectors.toList())"
                    );
                }
                if member == "filterIsInstance" {
                    let ty = self.type_arg_of(node).map(|t| {
                        t.trim_start_matches('<')
                            .trim_end_matches('>')
                            .trim()
                            .to_string()
                    });
                    if let Some(ty) = ty {
                        self.unit.diags.warn_approx(
                            nav,
                            self.unit.file,
                            "Iterable.filterIsInstance<T> lowered to stream().filter(x -> x instanceof T).map(x -> (T) x).collect(toList())",
                        );
                        self.unit.pending_full_call = true;
                        return format!(
                            "{base_java}.stream().filter(x -> x instanceof {ty}).map(x -> ({ty}) x).collect(java.util.stream.Collectors.toList())"
                        );
                    }
                }
                if member == "minus" {
                    let arg = node
                        .children(&mut node.walk())
                        .find(|c| c.kind() == "value_arguments")
                        .and_then(|va| {
                            let mut c = va.walk();
                            va.children(&mut c)
                                .find(|a| a.kind() == "value_argument")
                                .and_then(|a| a.children(&mut a.walk()).find(|cc| cc.is_named()))
                                .map(|e| self.transpile(e))
                        });
                    if let Some(arg) = arg {
                        self.unit.diags.warn_approx(
                            nav,
                            self.unit.file,
                            "Iterable.minus(elem) lowered to stream().filter(x -> !Objects.equals(x, elem)).collect(toList())",
                        );
                        self.unit.pending_full_call = true;
                        return format!(
                            "{base_java}.stream().filter(x -> !Objects.equals(x, {arg})).collect(java.util.stream.Collectors.toList())"
                        );
                    }
                }
                let lowered = self.map_entry_op(nav, &base_java, &member);
                // This branch owns the complete call expression and returns
                // it directly. `map_entry_op` also serves navigation-only
                // callers, where the enclosing call must suppress its args;
                // do not leak that suppression into the next expression.
                self.unit.pending_full_call = false;
                return lowered;
            }
            // Primitive-valued receivers (`.size()`, `.length`, `.count()`)
            // also can't take `.toString()` — same static wrapper path.
            if member == "toString"
                && base.kind() == "navigation_expression"
                && let Some((b, m)) = self.unit.nav_base_member(base)
                && matches!(m.as_str(), "size" | "length" | "count" | "size()")
            {
                let access = if m == "length" {
                    m.to_string()
                } else {
                    format!("{}()", m)
                };
                let recv = format!("{}.{}", self.transpile(b), access);
                self.unit.diags.warn_approx(
                    nav,
                    self.unit.file,
                    "Kotlin primitive `toString(...)` mapped to `String.valueOf(...)`",
                );
                return if args.len() == 1 {
                    format!("Integer.toString({}, {})", recv, args[0])
                } else {
                    format!("String.valueOf({})", recv)
                };
            }
            // Primitive receivers cannot be dereferenced in Java: Kotlin
            // `x.toString(radix)` -> `Integer.toString(x, radix)`,
            // `x.toString()` -> `String.valueOf(x)`, other primitive member
            // calls -> static wrapper when the boxed type has one.
            if base.kind() == "identifier"
                && let Some(bty) = self.unit.var_types.get(self.unit.text(base).trim())
                && matches!(
                    bty.as_str(),
                    "int" | "long" | "short" | "byte" | "double" | "float" | "boolean" | "char"
                )
            {
                let recv = self.transpile(base);
                match member.as_str() {
                    "toString" => {
                        self.unit.diags.warn_approx(
                            nav,
                            self.unit.file,
                            "Kotlin primitive `toString(...)` mapped to the boxed static wrapper (`Integer.toString`/`String.valueOf`)",
                        );
                        return if args.len() == 1 {
                            format!("Integer.toString({}, {})", recv, args[0])
                        } else {
                            format!("String.valueOf({})", recv)
                        };
                    }
                    "toInt" => return recv,
                    "toLong" => return format!("((long) {})", recv),
                    "toDouble" => return format!("((double) {})", recv),
                    "toFloat" => return format!("((float) {})", recv),
                    _ => {}
                }
                // takeIf/takeUnless with a trailing-lambda predicate: no
                // Java deref possible for primitives; lambda-less element
                // read must fall back to a N001 taint instead of a broken
                // `int.takeIf` method reference.
                if let (true, Some(lnode)) = (
                    matches!(member.as_str(), "takeIf" | "takeUnless"),
                    lambda_arg,
                ) {
                    // Kotlin `v.takeIf { it > 0 }` -> `v > 0 ? v : null`
                    // (`takeUnless` -> negation). The subtree beyond the
                    // params/arrow is the predicate expression.
                    let mut body_children: Vec<tree_sitter::Node> = {
                        let mut cur = lnode.walk();
                        lnode
                            .children(&mut cur)
                            .filter(|c| c.is_named() && c.kind() != "lambda_parameters")
                            .collect()
                    };
                    if let Some(pos_arrow) = body_children.iter().position(|c| c.kind() == "->") {
                        body_children = body_children[pos_arrow + 1..].to_vec();
                    }
                    if body_children.len() == 1 {
                        let pred = self.transpile(body_children[0]);
                        let recv_name = self.unit.text(base).trim();
                        let pred = pred.replace("it", recv_name);
                        self.unit.diags.warn_approx(
                            node,
                            self.unit.file,
                            format!(
                                "primitive `.{}` inlined as null-check ternary (no receiver deref in Java)",
                                member
                            ),
                        );
                        return if member == "takeIf" {
                            format!("({} ? {} : null)", pred, recv)
                        } else {
                            format!("(!({}) ? {} : null)", pred, recv)
                        };
                    }
                }
            }
            if member == "toList" && self.unit.receiver_is_array(base) {
                let receiver = self.transpile(base);
                let ty = self
                    .unit
                    .var_types
                    .get(self.unit.text(base).trim())
                    .map(String::as_str)
                    .unwrap_or("");
                return match ty {
                    "int[]" | "long[]" | "double[]" => format!(
                        "java.util.Arrays.stream({receiver}).boxed().collect(java.util.stream.Collectors.toList())"
                    ),
                    "byte[]" | "short[]" | "char[]" | "float[]" | "boolean[]" => format!(
                        "java.util.stream.IntStream.range(0, {receiver}.length).mapToObj(i -> {receiver}[i]).collect(java.util.stream.Collectors.toList())"
                    ),
                    _ => format!("java.util.Arrays.asList({receiver})"),
                };
            }
            if let Some(_recv_ty) = self.unit.extension_fns.get(member.as_str()) {
                // `x.f(...)` for a same-file extension -> static `f(x, ...)`.
                let recv_java = self.transpile(base);
                self.unit.diags.warn_approx(
                        nav,
                        self.unit.file,
                        format!(
                            "extension call site `{base}.{member}(...)` rewritten to static `{member}({base}, ...)` (same-file approximation; caller-visible signature change)",
                            base = self.unit.text(base).trim(),
                        ),
                    );
                return format!(
                    "{}({})",
                    member,
                    std::iter::once(recv_java)
                        .chain(args.into_iter())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }

        let callee_java = callee.map(|c| self.transpile_callee(c)).unwrap_or_default();

        // println -> System.out.println
        if callee_java == "println" {
            return format!("System.out.println({})", args.join(", "));
        }
        if callee_java == "print" {
            return format!("System.out.print({})", args.join(", "));
        }
        // Unplanned TODO forms keep the approximation-only fallback. Proven
        // stdlib calls above preserve kotlin.NotImplementedError and its message.
        if callee_java == "TODO" {
            self.unit.diags.warn_approx(
                node,
                self.unit.file,
                "Kotlin `TODO(msg)` lowered to a NotImplemented-style throw (it is Nothing-typed, never returns)",
            );
            let msg = args.first().cloned().unwrap_or_default();
            return if msg.is_empty() {
                "throw new RuntimeException()".to_string()
            } else {
                format!("throw new RuntimeException({msg})")
            };
        }

        // A Kotlin data-class copy call is lowered only when its receiver's
        // component schema is known. Capture the receiver once, then evaluate
        // supplied arguments in source order before reading omitted components.
        // This preserves Kotlin's receiver/argument evaluation order even when
        // named arguments are reordered or leave gaps.
        let copy_shadowed = has_explicit_copy_declaration(self.unit.current_decl, self.unit.source);
        let is_copy_callee = callee_java == "copy"
            || callee.is_some_and(|callee| {
                callee.kind() == "navigation_expression"
                    && self
                        .unit
                        .nav_base_member(callee)
                        .is_some_and(|(_, member)| member == "copy")
            });
        let copy_receiver = if !copy_shadowed && is_copy_callee && callee_java == "copy" {
            self.unit.current_decl.and_then(|decl| {
                kt::field(decl, "name").map(|name| {
                    let owner = self.unit.text(name).trim().to_string();
                    let type_params = kt::child(decl, "type_parameters")
                        .map(|params| {
                            params
                                .children(&mut params.walk())
                                .filter(|param| param.kind() == "type_parameter")
                                .filter_map(|param| kt::child(param, "identifier"))
                                .map(|param| self.unit.text(param).trim().to_string())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let receiver_type = if type_params.is_empty() {
                        owner
                    } else {
                        format!("{}<{}>", owner, type_params.join(", "))
                    };
                    ("this".to_string(), receiver_type)
                })
            })
        } else if !copy_shadowed
            && is_copy_callee
            && let Some(nav) = callee.filter(|callee| callee.kind() == "navigation_expression")
        {
            self.unit.nav_base_member(nav).and_then(|(base, member)| {
                (member == "copy").then(|| {
                    let receiver = self.transpile(base);
                    let receiver_type = self.unit.infer_type(base);
                    (receiver, receiver_type)
                })
            })
        } else {
            None
        };
        if let Some((receiver, receiver_type)) = copy_receiver {
            let owner = receiver_type
                .split('<')
                .next()
                .unwrap_or(&receiver_type)
                .trim()
                .to_string();
            if let Some(comps) = self.unit.data_components.get(&owner).cloned() {
                let parameters = comps
                    .iter()
                    .map(|(_, name)| name.clone())
                    .collect::<Vec<_>>();
                let supplied = copy_arg_values
                    .iter()
                    .enumerate()
                    .map(|(index, arg)| (arg_names.get(index).cloned().flatten(), arg.clone()))
                    .collect::<Vec<_>>();
                if crate::ctor_defaults::resolve_args(&parameters, &supplied).is_some() {
                    let Some(substitutions) = copy_type_substitutions(
                        self.unit.source,
                        self.unit.current_decl,
                        &owner,
                        &receiver_type,
                    ) else {
                        self.unit.diag_untranslatable(
                            node,
                            "generic data-class `copy(...)` receiver type arguments are unknown",
                        );
                        return "null".to_string();
                    };
                    let mut suffix = node.start_byte().to_string();
                    let mut collision = 0usize;
                    while self
                        .unit
                        .source
                        .contains(&format!("__notlinCopyReceiver{suffix}"))
                        || self
                            .unit
                            .source
                            .contains(&format!("__notlinCopyArg{suffix}_"))
                    {
                        collision += 1;
                        suffix = format!("{}{collision}", node.start_byte());
                    }
                    let temp_receiver = format!("__notlinCopyReceiver{suffix}");
                    let mut slots = vec![None; parameters.len()];
                    let mut captured_args = Vec::new();
                    let mut next_positional = 0usize;
                    for (arg_index, (name, value)) in supplied.iter().enumerate() {
                        let parameter_index = match name {
                            Some(name) => parameters.iter().position(|p| p == name),
                            None => {
                                let index = next_positional;
                                next_positional += 1;
                                Some(index)
                            }
                        };
                        if let Some(index) = parameter_index.filter(|index| *index < slots.len()) {
                            let temp = format!("__notlinCopyArg{suffix}_{arg_index}");
                            let arg_ty =
                                substitute_type_parameters(comps[index].0.trim(), &substitutions);
                            captured_args.push((
                                temp.clone(),
                                java_function_type_arg(&arg_ty),
                                value.clone(),
                            ));
                            slots[index] = Some(temp);
                        }
                    }
                    let has_unproven_omitted_accessor =
                        comps
                            .iter()
                            .enumerate()
                            .any(|(index, (component_type, name))| {
                                if slots.get(index).is_some_and(Option::is_some) {
                                    return false;
                                }
                                let record =
                                    !self.unit.lombok && self.unit.record_types.contains(&owner);
                                let lombok_boolean_is = self.unit.lombok
                                    && component_type.trim() == "boolean"
                                    && name.starts_with("is")
                                    && name.chars().nth(2).is_some_and(|c| c.is_ascii_uppercase());
                                let accessor = if record || lombok_boolean_is {
                                    name.clone()
                                } else {
                                    format!("get{}", capitalize_copy_name(name))
                                };
                                has_direct_copy_accessor(
                                    self.unit.current_decl,
                                    self.unit.source,
                                    &owner,
                                    &accessor,
                                )
                                .unwrap_or(true)
                            });
                    if has_unproven_omitted_accessor {
                        self.unit.diag_untranslatable(
                            node,
                            "data-class `copy(...)` cannot read omitted constructor fields through custom or unresolved accessors",
                        );
                        return "null".to_string();
                    }
                    let values = comps
                        .iter()
                        .enumerate()
                        .map(|(index, (component_type, name))| {
                            slots.get(index).and_then(Clone::clone).unwrap_or_else(|| {
                                let record =
                                    !self.unit.lombok && self.unit.record_types.contains(&owner);
                                let lombok_boolean_is = self.unit.lombok
                                    && component_type.trim() == "boolean"
                                    && name.starts_with("is")
                                    && name.chars().nth(2).is_some_and(|c| c.is_ascii_uppercase());
                                let accessor = if record || lombok_boolean_is {
                                    name.clone()
                                } else {
                                    format!("get{}", capitalize_copy_name(name))
                                };
                                format!("{temp_receiver}.{accessor}()")
                            })
                        })
                        .collect::<Vec<_>>();
                    let typed_receiver = receiver_type.clone();
                    let constructor_type = if typed_receiver.contains('<') {
                        format!("{owner}<>")
                    } else {
                        owner.clone()
                    };
                    let construction = format!("new {}({})", constructor_type, values.join(", "));
                    let mut result_type = typed_receiver.clone();
                    let mut argument_lambda = None;
                    for (arg_name, arg_type, _) in captured_args.iter().rev() {
                        argument_lambda = Some(match argument_lambda {
                            None => format!(
                                "{arg_name} -> {{ java.util.Objects.requireNonNull({temp_receiver}); return {construction}; }}"
                            ),
                            Some(inner) => format!("{arg_name} -> {{ return {inner}; }}"),
                        });
                        result_type =
                            format!("java.util.function.Function<{arg_type}, {result_type}>");
                    }
                    let lambda = match argument_lambda {
                        Some(inner) => format!("{temp_receiver} -> {{ return {inner}; }}"),
                        None => format!(
                            "{temp_receiver} -> {{ java.util.Objects.requireNonNull({temp_receiver}); return {construction}; }}"
                        ),
                    };
                    let function_type =
                        format!("java.util.function.Function<{typed_receiver}, {result_type}>");
                    let mut expression =
                        format!("(({function_type}) ({lambda})).apply({receiver})");
                    for (_, _, value) in &captured_args {
                        expression.push_str(&format!(".apply({value})"));
                    }
                    return expression;
                }
                self.unit.diag_untranslatable(
                    node,
                    "data-class `copy(...)` arguments cannot be mapped to the known component schema",
                );
                return "null".to_string();
            }
            self.unit.diag_untranslatable(
                node,
                "data-class `copy(...)` receiver type has no known component schema",
            );
            return "null".to_string();
        }
        // A qualified Kotlin `copy` whose data-class component schema is not
        // available in this Unit cannot be faithfully reconstructed. Never
        // emit Java's nonexistent `.copy(...)`; retaining the declaration is
        // safer than a compilation failure. (The self-copy form above has a
        // local schema and stays translatable.)
        if callee_java.ends_with(".copy") {
            self.unit.diag_untranslatable(
                node,
                "qualified Kotlin data-class `copy(...)` has no locally available component schema; declaration retained in Kotlin",
            );
            return "null".to_string();
        }
        // Collection slice ops on List receivers (Kotlin -> Java streams):
        // take(n) -> stream().limit(n).collect(toList());
        // drop(n) -> stream().skip(n).collect(toList()); chunked(n) ->
        // no direct API — marked below.
        if args.len() == 1 {
            let tail = callee_java.rsplit('.').next().unwrap_or("");
            if matches!(tail, "take" | "drop") && callee_java.rfind('.').is_some() {
                let recv = callee_java[..callee_java.len() - tail.len() - 1]
                    .trim()
                    .to_string();
                let jop = if tail == "take" { "limit" } else { "skip" };
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    format!("Kotlin collection `.{tail}` -> stream().{jop}().collect(toList())"),
                );
                return format!(
                    "{}.stream().{}({}).collect(java.util.stream.Collectors.toList())",
                    recv, jop, args[0]
                );
            }
            // chunked(n): sliding-free partition via IntStream over chunk
            // indices — logic-equal, emits a List<List<T>>.
            if tail == "chunked" && callee_java.rfind('.').is_some() {
                let recv = callee_java[..callee_java.len() - tail.len() - 1]
                    .trim()
                    .to_string();
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "Kotlin `.chunked(n)` -> IntStream partition (List<List<T>>)",
                );
                return format!(
                    "java.util.stream.IntStream.range(0, (int) (({}.stream().count() + {} - 1) / {})).mapToObj(i -> {}.subList(i * {}, Math.min((i + 1) * {}, {}.size()))).collect(java.util.stream.Collectors.toList())",
                    recv, args[0], args[0], recv, args[0], args[0], recv
                );
            }
            // zip(other): pair up positionally with SimpleImmutableEntry
            // values -> List<SimpleImmutableEntry<A,B>> (matches the Kotlin
            // Pair approximation used by `to`).
            if tail == "zip" && callee_java.rfind('.').is_some() {
                let recv = callee_java[..callee_java.len() - tail.len() - 1]
                    .trim()
                    .to_string();
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "Kotlin `.zip(other)` -> IntStream pairwise zip with SimpleImmutableEntry pairs",
                );
                return format!(
                    "java.util.stream.IntStream.range(0, Math.min({}.size(), {}.size())).mapToObj(i -> new java.util.AbstractMap.SimpleImmutableEntry<>({}.get(i), {}.get(i))).collect(java.util.stream.Collectors.toList())",
                    recv, args[0], recv, args[0]
                );
            }
        }

        // `new Regex(p)` -> `Pattern.compile(p)`; matches(input) ->
        // `Pattern.compile(p).matcher(input).matches()`.
        if callee_java == "Regex" && !args.is_empty() {
            return format!("java.util.regex.Pattern.compile({})", args.join(", "));
        }
        if let Some(rpos) = callee_java.rfind(".matches")
            && callee_java.ends_with(".matches")
        {
            let recv = callee_java[..rpos].trim().to_string();
            if recv.starts_with("new Regex(") {
                let inner = recv.trim_start_matches("new Regex(").trim_end_matches(')');
                return format!(
                    "java.util.regex.Pattern.compile({}).matcher({}).matches()",
                    inner, args[0]
                );
            }
            if let Some(inner) = recv
                .strip_prefix("java.util.regex.Pattern.compile(")
                .filter(|_| recv.ends_with(')'))
                .map(|t| &t[..t.len() - 1])
            {
                return format!(
                    "java.util.regex.Pattern.compile({}).matcher({}).matches()",
                    inner, args[0]
                );
            }
        }

        // Kotlin collection operations with a trailing lambda -> Stream API
        let is_stream_op = callee_java
            .rfind('.')
            .map(|i| &callee_java[i + 1..])
            .map(|m| {
                matches!(
                    m,
                    "map"
                        | "filter"
                        | "filterNot"
                        | "forEach"
                        | "flatMap"
                        | "sorted"
                        | "distinct"
                        | "mapNotNull"
                        | "filterIndexed"
                        | "mapIndexed"
                        | "associate"
                        | "groupBy"
                        | "fold"
                        | "reduce"
                        | "foldIndexed"
                        | "sortedBy"
                        | "sortedByDescending"
                        | "mapValues"
                        | "any"
                        | "all"
                        | "none"
                        | "anyMatch"
                        | "allMatch"
                        | "noneMatch"
                        | "count"
                        | "first"
                        | "firstOrNull"
                        | "last"
                        | "lastOrNull"
                        | "joinToString"
                )
            })
            .unwrap_or(false);
        // Scope function with receiver: `x.apply { ... }` / `x.also { ... }`
        // -> IIFE-style static helper? No: Java 25 has no scope functions;
        // nearest logic-preserving form is a nested block with a typed
        // local. Left as an explicit N001 when the callee is a receiver-scope
        // function to avoid silently emitting a method that does not exist.
        // No-lambda collection terminators (sum) must fire before the
        // lambda-gated block.
        let member0 = callee_java.rsplit('.').next().unwrap_or("");
        if member0 == "sum" && lambda_arg.is_none() {
            self.unit.diags.warn_approx(
                node,
                self.unit.file,
                "`sum()` approximated with mapToInt(...).sum()",
            );
            return format!(
                "{}.stream().mapToInt(Integer::intValue).sum()",
                callee_java[..callee_java.len() - member0.len()].trim_end_matches('.')
            );
        }

        if lambda_arg.is_some() {
            let member = callee_java.rsplit('.').next().unwrap_or("");
            // Stream-terminator predicates: rewrite to filter(...) before
            // the generic stream path (its full-expression mappings drop
            // the predicate).
            if member == "firstOrNull" {
                let base = callee_java[..callee_java.len() - member.len()]
                    .trim_end_matches('.')
                    .to_string();
                let Some(lnode_fo) = lambda_arg else {
                    return callee_java.clone();
                };
                let pred = self.transpile(lnode_fo);
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "firstOrNull { pred } approximated with filter(...).findFirst().orElse(null)",
                );
                return format!(
                    "{}.stream().filter({}).findFirst().orElse(null)",
                    base,
                    it_subst(&pred)
                );
            }
            if member == "sum" {
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "`sum()` approximated with mapToInt(...).sum()",
                );
                return format!(
                    "{}.stream().mapToInt(Integer::intValue).sum()",
                    callee_java[..callee_java.len() - member.len()].trim_end_matches('.')
                );
            }
            if matches!(member, "apply" | "also" | "run" | "with" | "let") {
                self.unit.diag_untranslatable(
                    node,
                    format!(
                        "scope function `.{}` not reproduced: Java has no equivalent; logic must be hand-migrated",
                        member
                    ),
                );
                // Inline comment breaks surrounding expressions; the
                // diagnostic listing carries the message.
                return "null".to_string();
            }
        }
        // A curried fold already assembled its full `stream().reduce(...)`
        // text in navigation_call — take it verbatim and clear it.
        let nav_text = self.unit.pending_nav_text.take();
        let nav_assembled = nav_text.is_some();
        if let (true, Some(lambda)) = (is_stream_op, lambda_arg)
            && !nav_assembled
        {
            let member = callee_java
                .rfind('.')
                .map(|i| &callee_java[i + 1..])
                .unwrap();
            let base = {
                let b = &callee_java[..callee_java.len() - member.len() - 1];
                // Arrays (e.g. `Op.values()`, `chArray`) have no .stream();
                // wrap with Arrays.stream(...).
                if b.ends_with("values()") {
                    format!("java.util.Arrays.stream({})", b)
                } else {
                    b.to_string()
                }
            };
            let planned_container = self
                .unit
                .container_calls
                .get(&(node.start_byte(), node.end_byte()));
            if let Some(fact) = planned_container {
                assert_eq!(fact.location.snapshot_hash, self.unit.source_hash);
            }
            let is_direct_java_optional =
                planned_container.is_some_and(|fact| fact.receiver_type == "java.util.Optional");
            let is_direct_java_stream = planned_container
                .is_some_and(|fact| fact.receiver_type == "java.util.stream.Stream")
                || base.starts_with("java.util.Arrays.stream")
                || base.ends_with(".stream()")
                || (base.contains(".stream()")
                    && !base.contains(".collect(")
                    && !base.ends_with(".toList()"));
            // Arrays.stream(...) and a pre-existing `.stream()` are already
            // Java Streams — no `.stream()` tail and no Kotlin collection-op
            // approximation warning are needed.
            let receiver_is_map = callee
                .and_then(|callee| self.unit.nav_base_member(callee).map(|(base, _)| base))
                .and_then(|base| {
                    let property = self
                        .unit
                        .text(base)
                        .trim()
                        .trim_start_matches("this.")
                        .trim_end_matches("()")
                        .rsplit('.')
                        .next()
                        .unwrap_or("");
                    self.scope_property_type(property)
                        .or_else(|| self.infer_operand_type(base))
                })
                .is_some_and(|ty| ty.starts_with("Map<"));
            let stream_base = if is_direct_java_stream {
                base.clone()
            } else if receiver_is_map
                || self
                    .unit
                    .var_types
                    .get(base.trim())
                    .is_some_and(|t| t.starts_with("Map<"))
            {
                // Kotlin maps stream over their ENTRIES (Map.Entry pairs).
                format!("{}.entrySet().stream()", base)
            } else {
                format!("{}.stream()", base)
            };
            let _ = &stream_base;
            let stream_fn = match member {
                "map" | "mapNotNull" | "mapIndexed" => "map",
                "filter" | "filterNot" | "filterIndexed" => "filter",
                "forEach" => "forEach",
                "sorted" => "sorted",
                other => other,
            };
            if !is_direct_java_stream && !is_direct_java_optional {
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    format!(
                        "collection op `.{member} {{...}}` approximated with Stream",
                        member = member
                    ),
                );
            }
            // joinToString(sep) -> collect(joining(sep)): the separator is
            // the first positional arg; other overloads (prefix/postfix/
            // limit/transform) degrade to joinToString-less collection with
            // a warn via the generic stream path below only when args exist
            if member == "joinToString" && !args.is_empty() {
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    if args.len() == 1 {
                        "Kotlin `joinToString(sep)` mapped to `stream().collect(joining(sep))`"
                    } else {
                        "joinToString with >1 arg (prefix/postfix/limit/transform) approximated as joining(sep); extra args dropped"
                    },
                );
                return format!(
                    "{}.stream().map(Object::toString).collect(java.util.stream.Collectors.joining({}))",
                    base, args[0]
                );
            }
            // forEach terminates the stream: it returns void, so no
            // `.collect(...)` tail (chaining collect after forEach is a
            // compile error and would drop the loop's effect entirely).
            if member == "forEach" {
                return format!("{}.stream().forEach({});", base, self.transpile(lambda));
            }
            // Collectors-backed ops with distinct Java shapes:
            let mapped_lambda = self.transpile(lambda);
            let base_str = callee_java[..callee_java.len() - member.len()]
                .trim_end_matches('.')
                .to_string();
            // Optional receiver (`opt.map { .. }`): Java Optional has its own
            // map/flatMap/filter members — no stream/collect detour; keep the
            // chain on the Optional so terminals like orElse/orElseGet that
            // follow still apply.
            if matches!(
                member,
                "map" | "mapNotNull" | "mapIndexed" | "filter" | "filterIndexed" | "flatMap"
            ) && (is_direct_java_optional || {
                let recv_name = |s: &String| {
                    s.rsplit('.')
                        .next()
                        .unwrap_or("")
                        .trim_end_matches("()")
                        .trim_start_matches("this.")
                        .to_string()
                };
                let mrti = |name: &str| -> Option<String> {
                    let ws = self.unit.workspace?;
                    let declaring = self
                        .unit
                        .workspace_file
                        .as_deref()
                        .unwrap_or(self.unit.file);
                    ws.method_return_type_in_file(declaring, name)
                };
                // Receiver kind matters: `getX()` is a METHOD call — its
                // declared return type wins; getter-name properties are only
                // a fallback for field-style receivers. A cross-file property
                // with the same name must not shadow the local method.
                let direct_method = base_str
                    .split_once('(')
                    .map(|(head, _)| head.rsplit('.').next().unwrap_or(head));
                mrti(base_str.as_str())
                    .or_else(|| direct_method.and_then(mrti))
                    .or_else(|| self.scope_property_type(&recv_name(&base_str)))
                    .or_else(|| {
                        mrti(
                            base_str
                                .trim()
                                .trim_start_matches("this.")
                                .trim_end_matches("()")
                                .rsplit('.')
                                .next()
                                .unwrap_or(""),
                        )
                    })
                    .is_some_and(|t| t.contains("Optional<"))
            }) {
                return format!("{}.{}({})", base_str, stream_fn, mapped_lambda);
            }
            match member {
                // fold(init) { acc, x -> ... } -> reduce(identity, op)
                "fold" => {
                    self.unit.diags.warn_approx(
                        node,
                        self.unit.file,
                        "fold approximated with Stream.reduce(identity, op)",
                    );
                    // The lambda maps to `(acc, x) -> body`; reduce wants a
                    // BiFunction — the transpiled form already is one.
                    return format!(
                        "{}.stream().reduce({}, {})",
                        base_str, args[0], mapped_lambda
                    );
                }
                "reduce" => {
                    self.unit.diags.warn_approx(
                        node,
                        self.unit.file,
                        "reduce approximated with Stream.reduce(op) (Optional; Kotlin throws — .orElseThrow() added)",
                    );
                    return format!(
                        "{}.stream().reduce({}).orElseThrow()",
                        base_str, mapped_lambda
                    );
                }
                "mapValues" => {
                    self.unit.diags.warn_approx(
                        node,
                        self.unit.file,
                        "mapValues approximated with entrySet().stream().collect(toMap(keySet, valueFn))",
                    );
                    // mapped_lambda is `<param> -> body` from the lambda
                    // transpile; toMap wants a bare value fn.
                    let vfn = mapped_lambda
                        .split_once(" -> ")
                        .map(|(_, b)| b.to_string())
                        .unwrap_or_else(|| mapped_lambda.clone())
                        .replace("it.getValue()", "__e.getValue()")
                        .replace("it.value", "__e.getValue()");
                    return format!(
                        "{}.entrySet().stream().collect(java.util.stream.Collectors.toMap(java.util.Map.Entry::getKey, __e -> {}))",
                        base_str, vfn
                    );
                }
                "groupBy" => {
                    // `xs.groupBy { it % 2 }` -> groupingBy(keyFn)
                    self.unit.diags.warn_approx(
                        node,
                        self.unit.file,
                        "groupBy approximated with Collectors.groupingBy (value lists, LinkedHashMap default ordering differs)",
                    );
                    return format!(
                        "{}.collect(java.util.stream.Collectors.groupingBy({}))",
                        base, mapped_lambda
                    );
                }
                "associate" => {
                    // `xs.associate { it to it * 2 }`: the lambda was mapped
                    // to `new SimpleImmutableEntry<>(k, v)` by infix `to` —
                    // convert to toMap(kFn, vFn).
                    self.unit.diags.warn_approx(
                        node,
                        self.unit.file,
                        "associate approximated with Collectors.toMap",
                    );
                    let kv = entry_lambda_to_kv(&mapped_lambda);
                    return format!(
                        "{}.collect(java.util.stream.Collectors.toMap({}, {}))",
                        base, kv.0, kv.1
                    );
                }
                "sortedBy" | "sortedByDescending" => {
                    self.unit.diags.warn_approx(
                        node,
                        self.unit.file,
                        "sortedBy approximated with sorted(Comparator.comparing(key))",
                    );
                    return format!(
                        "{}.sorted(java.util.Comparator.comparing({})).collect(java.util.stream.Collectors.toList())",
                        base_str,
                        it_subst(&mapped_lambda)
                    );
                }
                _ => {}
            }
            let mapped_lambda = if member == "filterNot" {
                negate_predicate_lambda(&mapped_lambda)
            } else {
                mapped_lambda
            };
            if is_direct_java_stream && matches!(member, "map" | "filter" | "flatMap") {
                return format!("{base_str}.{stream_fn}({mapped_lambda})");
            }
            // Kotlin collection operators are eager, so an isolated
            // `filter`/`map` is collected to a List. When another stream
            // operator immediately consumes that generated list, fuse the
            // two operations into one Java stream pipeline and collect only
            // once at the outer boundary.
            const TO_LIST: &str = ".collect(java.util.stream.Collectors.toList())";
            if let Some(pipeline) = base_str.strip_suffix(TO_LIST)
                && pipeline.contains(".stream()")
                && matches!(
                    member,
                    "map"
                        | "mapNotNull"
                        | "mapIndexed"
                        | "filter"
                        | "filterNot"
                        | "filterIndexed"
                        | "sorted"
                        | "flatMap"
                        | "distinct"
                )
            {
                return format!(
                    "{}.{}({}).collect(java.util.stream.Collectors.toList())",
                    pipeline, stream_fn, mapped_lambda
                );
            }
            // Chained stream continuation (`xs.stream().filter {..}.findFirst()`):
            // the base already ends `.stream()` AND the lambda member is a
            // stream-monadic op — the result must stay a Stream so terminal
            // members appended by the compound chain still apply. Emitting a
            // collect here (or double-streaming) breaks the outer chain.
            let continuation = base_str.ends_with(".stream()")
                && matches!(
                    member,
                    "map"
                        | "mapNotNull"
                        | "mapIndexed"
                        | "filter"
                        | "filterNot"
                        | "filterIndexed"
                        | "sorted"
                        | "flatMap"
                        | "distinct"
                );
            if continuation {
                let inner = &base_str[..base_str.len() - ".stream()".len()];
                return format!("{}.stream().{}({})", inner, stream_fn, mapped_lambda);
            }
            if matches!(member, "anyMatch" | "allMatch" | "noneMatch") {
                return format!("{}.{}({})", stream_base, stream_fn, mapped_lambda);
            }
            return if let Some(sep) = self.unit.pending_join_to_string.take() {
                format!(
                    "{}.{}({}).collect(java.util.stream.Collectors.joining({}))",
                    stream_base, stream_fn, mapped_lambda, sep
                )
            } else {
                format!(
                    "{}.{}({}).collect(java.util.stream.Collectors.toList())",
                    stream_base, stream_fn, mapped_lambda
                )
            };
        }

        // mutableListOf<String>(...) etc. -> new ArrayList<>()
        match callee_java.as_str() {
            "mutableListOf" | "arrayListOf" | "listOf" => {
                if !args.is_empty() && !self.proven_collection_factory(node, callee, &callee_java) {
                    self.unit.diag_untranslatable(
                        node,
                        "collection factory argument shape cannot be lowered without changing Kotlin semantics",
                    );
                    return "null".to_string();
                }
                if args.is_empty() {
                    "new ArrayList<>()".to_string()
                } else if args.len() == 1 {
                    format!(
                        "new ArrayList<>(java.util.Collections.singletonList({}))",
                        args[0]
                    )
                } else {
                    format!(
                        "new ArrayList<>(java.util.Arrays.asList({}))",
                        args.join(", ")
                    )
                }
            }
            "listOfNotNull" => {
                // Filters nulls: List.of rejects nulls, so stream the args
                // and keep non-nulls.
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "listOfNotNull approximated with Stream.filter(Objects::nonNull) over List.of",
                );
                format!(
                    "Stream.of({}).filter(java.util.Objects::nonNull).collect(java.util.stream.Collectors.toList())",
                    args.join(", ")
                )
            }
            "mutableMapOf" | "mapOf" | "linkedMapOf" | "hashMapOf" => {
                let map_type = if callee_java == "hashMapOf" {
                    "HashMap"
                } else {
                    "LinkedHashMap"
                };
                if args.is_empty() {
                    format!("new {map_type}<>()")
                } else {
                    if !self.proven_collection_factory(node, callee, &callee_java)
                        || args.iter().any(|arg| {
                            !arg.starts_with("new java.util.AbstractMap.SimpleImmutableEntry<")
                        })
                    {
                        self.unit.diag_untranslatable(
                            node,
                            "map factory requires proven stdlib Pair arguments using infix `to`",
                        );
                        return "null".to_string();
                    }
                    let expected_types = self.expected_map_factory_type_arguments(node);
                    if self.map_factory_has_direct_null_component(node) && expected_types.is_none()
                    {
                        self.unit.diag_untranslatable(
                            node,
                            "map factory with a null Pair component needs a proven expected Map<K, V> type for Java generic inference",
                        );
                        return "null".to_string();
                    }
                    let (map, entry) = collection_temp_names(self.unit.source, node.start_byte());
                    let stream = if let Some((key, value)) = expected_types {
                        format!(
                            "java.util.stream.Stream.<java.util.Map.Entry<{key}, {value}>>of({})",
                            args.join(", ")
                        )
                    } else {
                        format!("java.util.stream.Stream.of({})", args.join(", "))
                    };
                    format!(
                        "{stream}.collect(java.util.{map_type}::new, ({map}, {entry}) -> {map}.put({entry}.getKey(), {entry}.getValue()), java.util.{map_type}::putAll)"
                    )
                }
            }
            "mutableSetOf" | "setOf" | "linkedSetOf" | "hashSetOf" => {
                let set_type = if callee_java == "hashSetOf" {
                    "HashSet"
                } else {
                    "LinkedHashSet"
                };
                if args.is_empty() {
                    format!("new {set_type}<>()")
                } else if !self.proven_collection_factory(node, callee, &callee_java) {
                    self.unit.diag_untranslatable(
                        node,
                        "collection factory argument shape cannot be lowered without changing Kotlin semantics",
                    );
                    "null".to_string()
                } else if args.len() == 1 {
                    format!(
                        "new {set_type}<>(java.util.Collections.singletonList({}))",
                        args[0]
                    )
                } else {
                    format!(
                        "new {set_type}<>(java.util.Arrays.asList({}))",
                        args.join(", ")
                    )
                }
            }
            "emptyList" => "new ArrayList<>()".to_string(),
            "emptyMap" => "new LinkedHashMap<>()".to_string(),
            "emptySet" => "new LinkedHashSet<>()".to_string(),
            k if primitive_array_factory(k).is_some() => {
                // Kotlin array literal factories -> Java array literals:
                // `intArrayOf(1, 2)` -> `new int[]{1, 2}`, `arrayOf(...)` ->
                // `new Object[]{...}` (reuses the inference table).
                // A lone spread arg (`arrayOf(*arr)`) is an array copy:
                // `java.util.Arrays.copyOf(arr, arr.length)` — the literal
                // spread `{*arr}` text is invalid Java.
                let elem_ty = primitive_array_factory(k).unwrap();
                let spread_arg = {
                    // a spread arg was lowered to the bare array text by
                    // transpile()'s spread arm — spot it via the raw subtree
                    let mut found = false;
                    let _cursor = node.walk();
                    if let Some(va) = kt::child(node, "value_arguments") {
                        let mut vcur = va.walk();
                        for argn in va.children(&mut vcur) {
                            if argn.kind() == "value_argument"
                                && argn
                                    .children(&mut argn.walk())
                                    .any(|c| c.kind() == "spread_expression")
                            {
                                found = true;
                            }
                        }
                    }
                    found
                };
                if spread_arg {
                    // A Java array literal cannot spread (`{*a}` is invalid);
                    // the array of the spread parts plus trailing elements is
                    // rebuilt as a concat of flattened element streams. Order
                    // and contents are preserved; shape degrades to Object[]
                    // for `arrayOf` (and is rejected honestly for primitives
                    // via javac on the target).
                    let mut parts: Vec<String> = Vec::new();
                    let _cursor = node.walk();
                    if let Some(va) = kt::child(node, "value_arguments") {
                        let mut vcur = va.walk();
                        for argn in va.children(&mut vcur) {
                            if argn.kind() != "value_argument" {
                                continue;
                            }
                            let mut acur = argn.walk();
                            let ex = match argn.children(&mut acur).find(|c| c.is_named()) {
                                Some(e) => e,
                                None => continue,
                            };
                            if ex.kind() == "spread_expression" {
                                let mut scur = ex.walk();
                                let arr = ex
                                    .children(&mut scur)
                                    .find(|c| c.is_named())
                                    .map(|c| self.transpile(c))
                                    .unwrap_or_else(|| "null".to_string());
                                parts.push(format!("java.util.stream.Stream.of({})", arr));
                            } else {
                                let v = self.transpile(ex);
                                parts.push(format!("java.util.stream.Stream.of({})", v));
                            }
                        }
                    }
                    self.unit.diags.warn_approx(
                        node,
                        self.unit.file,
                        "spread in array factory lowered to a flattened element stream; produces Object[] (primitive element trays need hand migration)",
                    );
                    // Compose flatMap for multi-part spreads:

                    if parts.len() == 1 {
                        let h = parts[0]
                            .strip_prefix("java.util.stream.Stream.of(")
                            .unwrap_or(&parts[0])
                            .trim_end_matches(')');
                        format!("java.util.stream.Stream.of({}).toArray()", h)
                    } else {
                        // Java streams can't concat an element stream with a
                        // bare scalar: wrap each non-spread part in a
                        // one-element Object[] so concat stays homogeneous.
                        // A spread part keeps its own object stream.
                        let strip = |s: &str| {
                            s.strip_prefix("java.util.stream.Stream.of(")
                                .unwrap_or(s)
                                .trim_end_matches(')')
                                .to_string()
                        };
                        let spread_flags = {
                            let mut flags: Vec<bool> = Vec::new();
                            let _cursor = node.walk();
                            if let Some(va) = kt::child(node, "value_arguments") {
                                let mut vcur = va.walk();
                                for argn in va.children(&mut vcur) {
                                    if argn.kind() != "value_argument" {
                                        continue;
                                    }
                                    let mut acur = argn.walk();
                                    let is_spread = argn
                                        .children(&mut acur)
                                        .any(|c| c.kind() == "spread_expression");
                                    flags.push(is_spread);
                                }
                            }
                            flags
                        };
                        let mut chain: String = if spread_flags.first() == Some(&true) {
                            parts[0].clone()
                        } else {
                            format!(
                                "java.util.stream.Stream.of(new Object[]{{{}}})",
                                strip(&parts[0])
                            )
                        };
                        for (i, p) in parts[1..].iter().enumerate() {
                            let tail = if spread_flags.get(i + 1) == Some(&true) {
                                p.clone()
                            } else {
                                format!("java.util.stream.Stream.of(new Object[]{{{}}})", strip(p))
                            };
                            chain = format!("java.util.stream.Stream.concat({}, {})", chain, tail);
                        }
                        format!("{}.toArray()", chain)
                    }
                } else {
                    // `arrayOf(a, b)` style literal: elem_ty ends in `[]`
                    format!(
                        "new {}[]{{{}}}",
                        elem_ty.trim_end_matches("[]"),
                        args.join(", ")
                    )
                }
            }
            _ => {
                // Uppercase callee = constructor call — bare `Foo` or nested
                // `Outer.Inner` both need `new` (data subclasses of a sealed
                // nesting parent are the common case).
                let last_seg = callee_java.rsplit('.').next().unwrap_or("");
                if callee_java == "Triple" {
                    // JDK has no 3-tuple; N001 taint, null emit.
                    self.unit.diag_untranslatable(
                        node,
                        "Triple not translatable: JDK has no 3-tuple type; hand-migrate to a small record",
                    );
                    "null".to_string()
                } else if callee_java
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_uppercase())
                    && last_seg
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_uppercase())
                {
                    // Sealed/abstract Kotlin classes route construction
                    // through their companion `operator fun invoke(...)`:
                    // `Surname(...)` is a factory call
                    // (`Owner.invoke(...)`), NOT a constructor — Java
                    // `new Surname(...)` does not compile.
                    let mut ctor_args = args.clone();
                    if let Some(ws) = self.unit.workspace {
                        let candidates: Vec<_> = ws.declarations_named(&callee_java).collect();
                        // Kotlin named arguments have no Java form
                        // (`new MethodCall(name = "x", params = p)` is not
                        // Java). Lower them to the callee's declared parameter
                        // order. Fill omitted defaults only when all are neutral
                        // literals; otherwise use the planned overload for the
                        // complete original omission pattern (N87CB).
                        let arity = |d: &crate::workspace::Declaration| {
                            if d.constructor_param_count > 0 {
                                d.constructor_param_count
                            } else {
                                instance_property_count(d)
                            }
                        };
                        if ctor_args.iter().any(|arg| arg.contains(" = ")) {
                            let named: Vec<(&str, &str)> = ctor_args
                                .iter()
                                .filter_map(|arg| arg.split_once(" = "))
                                .collect();
                            let matched = candidates.iter().copied().find(|decl| {
                                !decl.constructor_param_names.is_empty()
                                    && named.iter().all(|(name, _)| {
                                        decl.constructor_param_names
                                            .iter()
                                            .any(|param| param == name.trim())
                                    })
                            });
                            if let Some(decl) = matched {
                                let names = decl.constructor_param_names.clone();
                                let mut slots: Vec<Option<String>> = vec![None; names.len()];
                                let mut next = 0usize;
                                for arg in std::mem::take(&mut ctor_args) {
                                    if let Some((name, value)) = arg.split_once(" = ") {
                                        match names.iter().position(|param| param == name.trim()) {
                                            Some(index) => slots[index] = Some(value.to_string()),
                                            None => {
                                                let last = slots.len().saturating_sub(1);
                                                slots[next.min(last)] = Some(value.to_string());
                                            }
                                        }
                                    } else {
                                        while next < slots.len() && slots[next].is_some() {
                                            next += 1;
                                        }
                                        if next < slots.len() {
                                            slots[next] = Some(arg);
                                            next += 1;
                                        }
                                    }
                                }
                                let (lowered, left_to_overload) =
                                    crate::ctor_defaults::lower_call_args(
                                        &slots,
                                        &decl.constructor_param_defaults,
                                    );
                                debug_assert!(
                                    left_to_overload.is_empty() || !slots.is_empty(),
                                    "an omission always leaves a pattern behind"
                                );
                                ctor_args = lowered;
                            }
                        } else if let Some(decl) = candidates.iter().find(|d| {
                            d.has_default_constructor_parameter
                                && !d.constructor_param_defaults.is_empty()
                                && arity(d) > ctor_args.len()
                        }) {
                            // A positional call that stops early: Kotlin fills the
                            // missing trailing parameters from their defaults. The
                            // complete omission pattern must survive when any
                            // default needs the declaration's scope.
                            let mut slots: Vec<Option<String>> =
                                ctor_args.iter().cloned().map(Some).collect();
                            slots.resize(arity(decl), None);
                            let (lowered, _) = crate::ctor_defaults::lower_call_args(
                                &slots,
                                &decl.constructor_param_defaults,
                            );
                            ctor_args = lowered;
                        }
                    }
                    let base_name = callee_java.rsplit('.').next().unwrap_or("").to_string();
                    if let Some(ws) = self.unit.workspace
                        && ws.find_static_member(&base_name, "invoke").is_some()
                    {
                        self.unit.diags.warn_approx(
                                node,
                                self.unit.file,
                                format!(
                                    "companion `operator fun invoke` on `{base_name}`: factory call routed via `.{base_name}.invoke(...)` (not a constructor)"
                                ),
                            );
                        return format!("{}.invoke({})", callee_java, ctor_args.join(", "));
                    }
                    format!("new {}({})", callee_java, ctor_args.join(", "))
                } else if callee_java.ends_with(')')
                    && (args.is_empty() || (nav_assembled && lambda_arg.is_some()))
                {
                    // Mapped member that is already a complete call expression
                    // (`xs.get(0)`, `xs.stream().findFirst().orElse(null)`):
                    // it IS the call — no `()` wrapper to add. A trailing
                    // lambda argument still appends (fold(idx) { ... }).
                    if let Some(la) = lambda_arg {
                        if callee_java.ends_with(".anyMatch")
                            || callee_java.ends_with(".allMatch")
                            || callee_java.ends_with(".noneMatch")
                        {
                            format!("{}({})", callee_java, args.join(", "))
                        } else if let Some(assembled) = nav_text {
                            // navigation_call already merged the lambda and
                            // assembled `stream().reduce(identity, op)`.
                            let _ = la;
                            assembled
                        } else {
                            format!("{} {}", callee_java, self.transpile(la))
                        }
                    } else {
                        callee_java
                    }
                } else if callee_java.rfind(".joinToString(").is_some() {
                    // trailing joinToString(sep) was re-collected with
                    // joining(sep) upstream; drop the tail entirely.
                    self.unit.diags.warn_approx(
                        node,
                        self.unit.file,
                        "trailing joinToString(sep) dropped: already collected with joining(sep)",
                    );
                    callee_java
                } else if std::mem::replace(&mut self.unit.pending_full_call, false) {
                    // joinToString style: callee mapping already emitted the
                    // full call with args — nothing to append.
                    callee_java
                } else if let Some(rewritten) =
                    self.reified_type_argument_rewrite(node, &callee_java, &args)
                {
                    rewritten
                } else {
                    format!("{}({})", callee_java, args.join(", "))
                }
            }
        }
    }

    fn transpile_callee(&mut self, node: tree_sitter::Node) -> String {
        match node.kind() {
            "identifier" => {
                let callee = self.unit.text(node).to_string();
                if let Some(provider) = self.unit.semantic_provider {
                    // Most calls are member or constructor calls and cannot
                    // become a file facade. Avoid the full scoped resolver
                    // unless the spelling has a top-level function candidate.
                    let has_top_level_candidate =
                        provider.symbols_named(&callee).iter().any(|symbol| {
                            symbol.id.kind == "function" && symbol.id.owner_path.is_empty()
                        }) || self
                            .unit
                            .import_alias_targets
                            .get(&callee)
                            .is_some_and(|name| {
                                provider.symbols_named(name).iter().any(|symbol| {
                                    symbol.id.kind == "function" && symbol.id.owner_path.is_empty()
                                })
                            });
                    let call = has_top_level_candidate
                        .then(|| {
                            node.parent()
                                .filter(|parent| parent.kind() == "call_expression")
                        })
                        .flatten();
                    let resolution = call.map(|call| {
                        let file = self
                            .unit
                            .workspace_file
                            .as_deref()
                            .unwrap_or(self.unit.file);
                        crate::semantics::resolve_required_call(
                            self.unit.source,
                            file,
                            call,
                            node,
                            &callee,
                            provider,
                        )
                    });
                    if let Some(crate::semantics::FactStatus::Established(target_id)) =
                        resolution.as_ref()
                    {
                        if target_id.kind == "function"
                            && target_id.owner_path.is_empty()
                            && target_id.receiver.is_none()
                        {
                            if let Some(target) = provider
                                .symbols_named(&target_id.name)
                                .into_iter()
                                .find(|symbol| symbol.id == *target_id)
                            {
                                let retained = self.unit.retained_hint.is_some_and(|retained| {
                                    crate::transpiler::retention_queries::contains(
                                        retained, &target.id,
                                    )
                                }) || ((target.id.file == self.unit.file
                                    || self.unit.workspace_file.as_deref()
                                        == Some(target.id.file.as_path()))
                                    && self.unit.is_untranslated_file_function(&target.id.name));
                                let selected = self.unit.workspace.is_none_or(|workspace| {
                                    workspace
                                        .is_selected(&target.id.file, self.unit.translation_roots)
                                });
                                if retained || !selected {
                                    self.unit.diag_untranslatable(
                                        node,
                                        format!(
                                            "top-level function `{}` remains Kotlin; its callable facade is not accepted for Java ownership",
                                            target.id.name
                                        ),
                                    );
                                } else {
                                    let original = self
                                        .unit
                                        .workspace
                                        .and_then(|workspace| {
                                            workspace.source_file(&target.id.file)
                                        })
                                        .map(|file| file.source_text())
                                        .unwrap_or(self.unit.source);
                                    let facade = crate::function_callsite::facade_name(
                                        original,
                                        &target.id.file,
                                    );
                                    let package = if target.id.package.is_empty() {
                                        String::new()
                                    } else {
                                        format!("{}.", target.id.package)
                                    };
                                    return format!("{package}{facade}.{}", target.id.name);
                                }
                            }
                        } else if target_id.kind == "function"
                            && target_id.owner_path.is_empty()
                            && target_id.receiver.is_some()
                        {
                            self.unit.diag_untranslatable(
                                node,
                                format!(
                                    "extension function `{callee}` requires receiver-aware call lowering"
                                ),
                            );
                        } else if let Some(call) = call {
                            let argument_count = kt::child(call, "value_arguments")
                                .map(|arguments| {
                                    arguments
                                        .named_children(&mut arguments.walk())
                                        .filter(|argument| argument.kind() == "value_argument")
                                        .count()
                                })
                                .unwrap_or(0)
                                + call
                                    .named_children(&mut call.walk())
                                    .filter(|argument| {
                                        matches!(
                                            argument.kind(),
                                            "annotated_lambda" | "lambda_literal"
                                        )
                                    })
                                    .count();
                            if target_id.parameters.len() != argument_count {
                                self.unit.diag_untranslatable(
                                    node,
                                    format!(
                                        "call to `{callee}` matches an enclosing member name but its arity is unresolved"
                                    ),
                                );
                            }
                        }
                    } else if self.unit.is_untranslated_file_function(&callee) {
                        // A bare call to a file-local top-level function that
                        // remained Kotlin cannot resolve in translated Java.
                        self.unit.diag_untranslatable(
                            node,
                            format!(
                                "call to `{callee}` resolves to a top-level function that remains Kotlin"
                            ),
                        );
                    }
                }
                callee
            }
            "navigation_expression" => self.navigation_call(node),
            _ => self.transpile(node),
        }
    }

    /// Callee translation when the type-argument brackets were swallowed into
    /// a binary_expression (`spec<String> { ... }`): translate the callee,
    /// drop the phantom `<Type>` text, and emit a warn explaining the
    /// type-argument loss (Java re-inferable in the common `fun <T>` case).
    pub(crate) fn transpile_callee_generic(
        &mut self,
        callee: tree_sitter::Node,
        type_arg: Option<&str>,
    ) -> String {
        let base = self.transpile_callee(callee);
        if let Some(ty) = type_arg {
            self.unit.diags.warn_approx(
                callee,
                self.unit.file,
                format!(
                    "generic call `{}` with type argument `<{}>` merged from lambda-bracket ambiguity; type argument dropped (Java inference must re-derive it)",
                    base, ty
                ),
            );
        }
        base
    }

    fn type_arg_of(&self, call: tree_sitter::Node) -> Option<String> {
        kt::child(call, "type_arguments").map(|t| self.unit.text(t).to_string())
    }

    /// `recv.get<T>()` where the callee declared a single `reified T` type
    /// parameter with NO other type parameters: Java has no counterpart for
    /// the reified form; the Kotlin ABI that survives migration is the
    /// `Class<T>` overload, so a call site must pass the type literal
    /// explicitly (Kotlin inlined `T::class.java` at compile time).
    fn reified_type_argument_rewrite(
        &mut self,
        call: tree_sitter::Node,
        callee_java: &str,
        args: &[String],
    ) -> Option<String> {
        if args.is_empty() {
            // Only `get<T>()` (no args) maps to `get(T.class)`; with other
            // args the arity mapping is ambiguous, conservatively untouched.
            // The type argument may arrive two ways: a `type_arguments` child
            // of the call node (direct callee) or stashed by navigation_call
            // as `pending_callee_type_arg` (the brackets were swallowed into
            // the callee nav text).
            if let Some(type_args) = kt::child(call, "type_arguments")
                .map(|t| self.unit.text(t).trim().to_string())
                .or_else(|| self.unit.pending_callee_type_arg.take())
            {
                // single type argument name, no bounds/usage text
                let text = type_args;
                let inner = text
                    .strip_prefix('<')
                    .and_then(|t| t.strip_suffix('>'))
                    .map(|t| t.trim().to_string());
                if let Some(ty) = inner
                    && !ty.contains('<')
                    && !ty.contains(',')
                    && !ty.contains('*')
                    && ty
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
                {
                    // Type-literal form: `Ty.class` ( Java class literal).
                    // Receiver text is the callee head before `.member(`.
                    let (head, member) = callee_java
                        .rsplit_once('.')
                        .map(|(h, m)| (h.to_string(), m.to_string()))
                        .unwrap_or_default();
                    if !head.is_empty() && !member.is_empty() {
                        self.unit.diags.warn_approx(
                            call,
                            self.unit.file,
                            format!(
                                "call `{member}<{ty}>()` on a reified inline function lowered to `{member}({ty}.class)` for the Class<T> ABI"
                            ),
                        );
                        return Some(format!("{}.{}({}.class)", head, member, ty));
                    }
                }
            }
        }
        None
    }

    pub(crate) fn lambda(&mut self, node: tree_sitter::Node) -> String {
        if let Some(java) = self.planned_container_lambda(node) {
            return java;
        }
        // lambda_literal: { params -> body }. The body is real AST (named
        // children after `->`) — transpiling it keeps println and other
        // top-level rewrites intact; raw text pass-through loses them.
        let mut cursor = node.walk();
        let kids: Vec<_> = node.children(&mut cursor).collect();
        let prior_smart_casts = self.unit.flow_smart_casts.clone();
        // params
        let mut params_java = String::new();
        // lambda params registered as locals for the body; popped after
        let mut inserted: Vec<String> = Vec::new();
        let mut body_nodes: Vec<tree_sitter::Node> = Vec::new();
        let mut after_arrow = !kids.iter().any(|c| c.kind() == "->");
        for c in kids {
            match c.kind() {
                "lambda_parameters" => {
                    let mut pcur = c.walk();
                    let names: Vec<String> = c
                        .children(&mut pcur)
                        .filter(|vc| vc.kind() == "variable_declaration")
                        .filter_map(|vd| kt::child(vd, "identifier"))
                        .map(|n| self.unit.text(n).to_string())
                        .collect();
                    params_java = names.join(", ");
                    // Lambda params are LOCALS: register them before the body
                    // transpiles, otherwise the body's bare identifier falls
                    // through to workspace property resolution and gets
                    // rewritten to a spurious `this.getX()`.
                    for n in &names {
                        self.unit.flow_smart_casts.remove(n);
                        if !self.unit.var_types.contains_key(n) {
                            self.unit
                                .var_types
                                .insert(n.to_string(), "__lambda_param".to_string());
                            inserted.push(n.to_string());
                        }
                    }
                }
                "->" => after_arrow = true,
                k if after_arrow
                    && c.is_named()
                    && !matches!(k, "line_comment" | "block_comment" | "multiline_comment")
                    && k != "{"
                    && k != "}" =>
                {
                    body_nodes.push(c);
                }
                _ => {}
            }
        }
        if params_java.is_empty() {
            self.unit.flow_smart_casts.remove("it");
        }
        // body statements -> `\n`-joined expressions (Kotlin lambda bodies
        // here are single expressions in practice)
        let mut body_java = String::new();
        for (i, bn) in body_nodes.iter().enumerate() {
            if i > 0 {
                body_java.push_str("; ");
            }
            body_java.push_str(&self.transpile(*bn));
        }
        for n in &inserted {
            self.unit.var_types.remove(n);
        }
        self.unit.flow_smart_casts = prior_smart_casts;
        if body_java.is_empty() {
            body_java = "{}".to_string();
            self.unit.diags.warn_approx(
                node,
                self.unit.file,
                "empty lambda body emitted as a no-op block",
            );
        }
        if params_java.is_empty() {
            // `{ it * 2 }`: implicit `it` parameter — the body references it.
            let uses_it = body_nodes
                .iter()
                .any(|bn| Self::body_uses_it(*bn, self.unit.source));
            if uses_it {
                format!("it -> {}", body_java)
            } else {
                format!("() -> {}", body_java)
            }
        } else if params_java.contains(',') {
            // Multi-param lambda: Java requires the paren list
            format!("({}) -> {}", params_java, body_java)
        } else {
            format!("{} -> {}", params_java, body_java)
        }
    }

    /// Does this lambda-body tree reference the implicit `it` parameter?
    fn body_uses_it<'t>(node: tree_sitter::Node<'t>, source: &'t str) -> bool {
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            if n.kind() == "identifier"
                && n.parent().map(|p| p.kind()) != Some("lambda_parameters")
                && kt::text(n, source) == "it"
            {
                return true;
            }
            let mut cur = n.walk();
            for c in n.children(&mut cur) {
                stack.push(c);
            }
        }
        false
    }

    fn known_expression_type(&self, node: tree_sitter::Node) -> Option<String> {
        if node.kind() == "string_literal" {
            return Some("String".to_string());
        }
        let raw = self.unit.text(node).trim();
        if let Some(ty) = self.unit.var_types.get(raw) {
            return Some(ty.clone());
        }
        if node.kind() == "call_expression"
            && let Some(workspace) = self.unit.workspace
        {
            let declaring = self
                .unit
                .workspace_file
                .as_deref()
                .unwrap_or(self.unit.file);
            if let Some(ty) = workspace.method_return_type_in_file(declaring, raw) {
                return Some(ty);
            }
            if let Some(callee) = node
                .children(&mut node.walk())
                .find(|child| child.is_named())
            {
                let name = self.unit.text(callee);
                if let Some(ty) = workspace.method_return_type_in_file(declaring, name) {
                    return Some(ty);
                }
            }
        }
        self.infer_operand_type(node).or_else(|| {
            let name = raw.trim_start_matches("this.").trim_end_matches("()");
            self.scope_property_type(name)
        })
    }

    fn is_known_string_receiver(&self, node: tree_sitter::Node) -> bool {
        self.known_expression_type(node).is_some_and(|ty| {
            matches!(
                ty.trim().trim_end_matches('?'),
                "String" | "java.lang.String" | "CharSequence" | "kotlin.CharSequence"
            )
        })
    }

    fn is_known_string_expression(&self, node: tree_sitter::Node) -> bool {
        self.is_known_string_receiver(node)
            || (matches!(node.kind(), "identifier" | "simple_identifier")
                && self
                    .inline_same_file_string_const(self.unit.text(node).trim(), node)
                    .is_some())
    }

    fn is_known_list_receiver(&self, node: tree_sitter::Node) -> bool {
        self.known_expression_type(node).is_some_and(|ty| {
            let ty = ty.trim().trim_end_matches('?');
            [
                "List<",
                "MutableList<",
                "java.util.List<",
                "java.util.ArrayList<",
            ]
            .iter()
            .any(|prefix| ty.starts_with(prefix))
        })
    }

    fn is_string_split_result(&self, node: tree_sitter::Node) -> bool {
        if node.kind() != "call_expression" {
            return false;
        }
        let Some(navigation) = node
            .children(&mut node.walk())
            .find(|child| child.kind() == "navigation_expression")
        else {
            return false;
        };
        let Some((receiver, member)) = self.unit.nav_base_member(navigation) else {
            return false;
        };
        if member != "split" || !self.is_known_string_receiver(receiver) {
            return false;
        }
        let Some(arguments) = node
            .children(&mut node.walk())
            .find(|child| child.kind() == "value_arguments")
        else {
            return false;
        };
        let values = arguments
            .children(&mut arguments.walk())
            .filter(|child| child.kind() == "value_argument")
            .collect::<Vec<_>>();
        values.len() == 1
            && !self.unit.text(values[0]).contains('=')
            && values[0]
                .children(&mut values[0].walk())
                .find(|child| child.is_named())
                .is_some_and(|argument| self.is_known_string_expression(argument))
    }
}

fn collection_temp_names(source: &str, start: usize) -> (String, String) {
    let mut suffix = start;
    loop {
        let map = format!("__notlinMap{suffix}");
        let entry = format!("__notlinEntry{suffix}");
        if !source.contains(&map) && !source.contains(&entry) {
            return (map, entry);
        }
        suffix += 1;
    }
}

fn named_argument_name(text: &str) -> Option<String> {
    let (prefix, suffix) = text.split_once('=')?;
    if suffix.trim_start().starts_with('=') || prefix.trim_end().ends_with(['=', '!', '<', '>']) {
        return None;
    }
    let name = prefix.trim();
    let mut chars = name.chars();
    let first = chars.next()?;
    (first == '_' || first.is_ascii_alphabetic()).then_some(())?;
    chars
        .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        .then(|| name.to_string())
}

fn has_explicit_copy_declaration(
    current_decl: Option<tree_sitter::Node<'_>>,
    source: &str,
) -> bool {
    let mut current = current_decl;
    while let Some(node) = current {
        if matches!(node.kind(), "function_declaration" | "class_declaration") {
            let mut stack = node.children(&mut node.walk()).collect::<Vec<_>>();
            while let Some(child) = stack.pop() {
                if child.kind() == "function_declaration"
                    && kt::field(child, "name")
                        .is_some_and(|name| name.utf8_text(source.as_bytes()).ok() == Some("copy"))
                {
                    return true;
                }
                stack.extend(child.children(&mut child.walk()));
            }
        }
        current = node.parent();
    }
    false
}

fn has_direct_copy_accessor(
    current_decl: Option<tree_sitter::Node<'_>>,
    source: &str,
    owner: &str,
    accessor: &str,
) -> Option<bool> {
    let mut current = current_decl;
    while let Some(node) = current {
        if node.kind() == "class_declaration"
            && kt::field(node, "name")
                .is_some_and(|name| name.utf8_text(source.as_bytes()).ok() == Some(owner))
        {
            let Some(body) = kt::child(node, "class_body") else {
                return Some(false);
            };
            return Some(
                body.named_children(&mut body.walk())
                    .filter(|member| member.kind() == "function_declaration")
                    .any(|function| {
                        kt::field(function, "name").is_some_and(|name| {
                            name.utf8_text(source.as_bytes()).ok() == Some(accessor)
                        }) && kt::child(function, "function_value_parameters").is_some_and(
                            |params| {
                                !params
                                    .named_children(&mut params.walk())
                                    .any(|parameter| parameter.kind() == "parameter")
                            },
                        )
                    }),
            );
        }
        current = node.parent();
    }
    None
}

fn copy_type_substitutions<'tree>(
    source: &str,
    current_decl: Option<tree_sitter::Node<'tree>>,
    owner: &str,
    receiver_type: &str,
) -> Option<std::collections::HashMap<String, String>> {
    let mut root = current_decl?;
    while let Some(parent) = root.parent() {
        root = parent;
    }
    let mut stack = vec![root];
    let mut type_params = None;
    while let Some(node) = stack.pop() {
        if node.kind() == "class_declaration"
            && kt::field(node, "name")
                .is_some_and(|name| name.utf8_text(source.as_bytes()).ok() == Some(owner))
        {
            let params = kt::child(node, "type_parameters")
                .map(|list| {
                    list.children(&mut list.walk())
                        .filter(|param| param.kind() == "type_parameter")
                        .filter_map(|param| kt::child(param, "identifier"))
                        .filter_map(|name| name.utf8_text(source.as_bytes()).ok())
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            type_params = Some(params);
            break;
        }
        stack.extend(node.children(&mut node.walk()));
    }
    let type_params = type_params?;
    if type_params.is_empty() {
        return Some(std::collections::HashMap::new());
    }
    let actual = type_arguments(receiver_type)?;
    if actual.len() != type_params.len() {
        return None;
    }
    Some(type_params.into_iter().zip(actual).collect())
}

fn type_arguments(ty: &str) -> Option<Vec<String>> {
    let open = ty.find('<')?;
    let mut depth = 0usize;
    let mut start = open + 1;
    let mut args = Vec::new();
    for (offset, ch) in ty.char_indices().skip_while(|(offset, _)| *offset <= open) {
        match ch {
            '<' => depth += 1,
            '>' => {
                if depth == 0 {
                    args.push(ty[start..offset].trim().to_string());
                    return Some(args);
                }
                depth -= 1;
            }
            ',' if depth == 0 => {
                args.push(ty[start..offset].trim().to_string());
                start = offset + ch.len_utf8();
            }
            _ => {}
        }
    }
    None
}

fn substitute_type_parameters(
    java_type: &str,
    substitutions: &std::collections::HashMap<String, String>,
) -> String {
    let mut output = String::new();
    let chars = java_type.char_indices().collect::<Vec<_>>();
    let mut cursor = 0usize;
    for (index, (start, ch)) in chars.iter().enumerate() {
        if !(*ch == '_' || ch.is_ascii_alphabetic()) || *start < cursor {
            continue;
        }
        let end_index = chars[index..]
            .iter()
            .take_while(|(_, c)| *c == '_' || c.is_ascii_alphanumeric())
            .count();
        let end = chars
            .get(index + end_index)
            .map_or(java_type.len(), |(at, _)| *at);
        let token = &java_type[*start..end];
        if let Some(replacement) = substitutions.get(token) {
            output.push_str(&java_type[cursor..*start]);
            output.push_str(replacement);
            cursor = end;
        }
    }
    output.push_str(&java_type[cursor..]);
    output
}

fn java_function_type_arg(ty: &str) -> String {
    match ty {
        "boolean" => "Boolean".into(),
        "byte" => "Byte".into(),
        "short" => "Short".into(),
        "int" => "Integer".into(),
        "long" => "Long".into(),
        "char" => "Character".into(),
        "float" => "Float".into(),
        "double" => "Double".into(),
        _ => ty.to_string(),
    }
}

fn capitalize_copy_name(name: &str) -> String {
    let mut chars = name.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}

fn negate_predicate_lambda(mapped: &str) -> String {
    mapped
        .split_once(" -> ")
        .map(|(parameters, body)| format!("{parameters} -> !({body})"))
        .unwrap_or_else(|| format!("it -> !({mapped})"))
}

fn entry_lambda_to_kv(mapped: &str) -> (String, String) {
    if let Some(start) = mapped.find("SimpleImmutableEntry<>(") {
        let inner_start = start + "SimpleImmutableEntry<>(".len();
        if let Some(end) = mapped.rfind(')') {
            let inner = &mapped[inner_start..end];
            if let Some(comma) = split_top_comma(inner) {
                // The `to` lambda's body referenced the element as `it` —
                // toMap's key/value fns also see the element, so keep the
                // original lambda param name.
                return (
                    format!("__e -> {}", inner[..comma].trim().replace("it", "__e")),
                    format!("__e -> {}", inner[comma + 1..].trim().replace("it", "__e")),
                );
            }
        }
    }
    (mapped.trim().to_string(), "__e -> __e".to_string())
}

/// Find the first top-level comma (not inside parens/angles).
fn split_top_comma(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' | '<' => depth += 1,
            ')' | '>' => depth -= 1,
            ',' if depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

/// Replace bare `it` identifier in a transpiled lambda body with the
/// element lambda name (used when hoisting a lambda body into filter()).
fn it_subst(pred: &str) -> String {
    pred.replace("it", "__e")
}
