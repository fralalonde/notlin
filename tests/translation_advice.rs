use notlin::diagnostics::{RetentionKind, RetentionSite};
use notlin::semantics::{FactStatus, workspace_symbol};
use notlin::translation_advice::{analyze, render};
use notlin::translation_plan::{BackendOwner, SymbolDependency, TranslationPlan};
use notlin::transpiler::fixpoint::FilePlan;
use notlin::workspace::{DeclarationKind, SourceIndex};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "notlin-translation-advice-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn write(&self, name: &str, source: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::write(&path, source).unwrap();
        path
    }

    fn index(&self) -> SourceIndex {
        SourceIndex::discover(&self.root).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn plans_for(index: &SourceIndex) -> (Vec<FilePlan>, HashMap<PathBuf, TranslationPlan>) {
    let mut files = Vec::new();
    let mut plans = HashMap::new();
    for source_file in index.kotlin_files() {
        let source = source_file.source_text().to_string();
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(&source, None).unwrap();
        let mut translation =
            notlin::translation_plan::analyze(&source, tree.root_node(), &source_file.path);
        for decision in &mut translation.declarations {
            decision.final_owner = Some(BackendOwner::Kotlin);
        }
        plans.insert(source_file.path.clone(), translation.clone());
        files.push(FilePlan {
            emission_failed: false,
            file: source_file.path.clone(),
            source,
            java_files: Vec::new(),
            emitted_owners: Vec::new(),
            emitted_file_owners: Vec::new(),
            errors: 0,
            warnings: 0,
            coverage: Default::default(),
            translation,
        });
    }
    (files, plans)
}

/// Read-only analysis benchmark. Use an isolated committed-source snapshot,
/// never an application workspace being migrated concurrently.
#[test]
#[ignore = "set NOTLIN_ADVICE_BENCH_ROOT to an isolated source snapshot"]
fn workspace_advice_performance() {
    let root = PathBuf::from(std::env::var_os("NOTLIN_ADVICE_BENCH_ROOT").expect("benchmark root"));
    let started = std::time::Instant::now();
    let index = SourceIndex::discover(&root).unwrap();
    eprintln!("BENCH index {:.3}s", started.elapsed().as_secs_f64());
    let (files, plans) = plans_for(&index);
    let sites: Vec<_> = files
        .iter()
        .flat_map(|file| {
            file.translation
                .declarations
                .iter()
                .map(|decision| RetentionSite {
                    kind: RetentionKind::RetainedSupertype,
                    params: vec![],
                    file: notlin::paths::display(&file.file),
                    line: file.source[..decision.id.start_byte]
                        .bytes()
                        .filter(|byte| *byte == b'\n')
                        .count()
                        + 1,
                    declaration: decision.symbol_id.name.clone(),
                    name: decision.symbol_id.name.clone(),
                    blockers: vec![],
                    blocker_labels: vec![],
                })
        })
        .collect();
    let lookup_started = std::time::Instant::now();
    let mut comparisons = 0usize;
    for file in &files {
        for decision in &file.translation.declarations {
            let line = file.source[..decision.id.start_byte]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count()
                + 1;
            let found = sites.iter().find(|site| {
                comparisons += 1;
                site.file == notlin::paths::display(&file.file)
                    && site.line == line
                    && site.kind == RetentionKind::MiddleDefaultParameter
                    && site.blockers.is_empty()
            });
            assert!(found.is_none());
        }
    }
    eprintln!(
        "BENCH legacy site lookup {:.3}s, {comparisons} path comparisons",
        lookup_started.elapsed().as_secs_f64()
    );
    let advice_started = std::time::Instant::now();
    let advice = analyze(&files, &plans, &index, std::slice::from_ref(&root), &sites);
    eprintln!(
        "BENCH complete advice {:.3}s, {} recommendations",
        advice_started.elapsed().as_secs_f64(),
        advice.len()
    );
}

fn site(kind: RetentionKind, file: &Path, line: usize, params: Vec<String>) -> RetentionSite {
    RetentionSite {
        kind,
        params,
        file: notlin::paths::display(fs::canonicalize(file).unwrap()),
        line,
        declaration: "class Fixture".into(),
        name: "Fixture".into(),
        blockers: Vec::new(),
        blocker_labels: Vec::new(),
    }
}

#[test]
fn recommends_only_a_known_covariant_default_getter_conflict_on_retained_descendant() {
    let fixture = Fixture::new();
    fixture.write(
        "types.kt",
        "package types\n\
         interface Broad {}\n\
         class Narrow : Broad {}\n",
    );
    fixture.write(
        "api.kt",
        "package sample\n\
         import types.*\n\
         interface BroadContract {\n\
             val ref: Broad\n\
         }\n\
         interface Provider : BroadContract {\n\
             override val ref: Narrow get() = Narrow()\n\
         }\n\
         class RetainedChild : Provider {}\n",
    );
    let index = fixture.index();
    let (files, plans) = plans_for(&index);
    let advice = analyze(
        &files,
        &plans,
        &index,
        std::slice::from_ref(&fixture.root),
        &[],
    );
    let matches: Vec<_> = advice.iter().filter(|item| item.code == "U001").collect();
    assert_eq!(matches.len(), 1, "{advice:#?}");
    // An established-looking inheritance shape must not blame a declaration
    // whose required facts or source parse are explicitly uncertain.
    let broad_contract = workspace_symbol(
        &index,
        index.declarations_named("BroadContract").next().unwrap(),
    );
    for symbol in [
        &matches[0].target,
        &matches[0].retained_roots[0],
        &broad_contract,
    ] {
        let mut uncertain_plans = plans.clone();
        for decision in uncertain_plans
            .values_mut()
            .flat_map(|plan| &mut plan.declarations)
        {
            if &decision.symbol_id == symbol {
                decision.retention_reasons.push(
                    notlin::translation_plan::RetentionReason::UnknownRequiredFact {
                        name: "contract".into(),
                    },
                );
            }
        }
        assert!(
            analyze(
                &files,
                &uncertain_plans,
                &index,
                std::slice::from_ref(&fixture.root),
                &[]
            )
            .is_empty()
        );
    }
    assert_eq!(matches[0].target.name, "RetainedChild");
    assert_eq!(matches[0].title, "competing inherited getters for `ref`");
    assert!(
        matches[0]
            .suggested_change
            .contains("Explicitly override `ref`")
    );
    assert!(matches[0].explanation.contains("not invalid Kotlin"));

    // The concrete class edit removes the precise getter conflict on replanning.
    let edited = Fixture::new();
    edited.write(
        "types.kt",
        "package sample\n\
         interface Broad {}\n\
         class Narrow : Broad {}\n\
         interface BroadContract {\n\
             val ref: Broad\n\
         }\n\
         interface Provider : BroadContract {\n\
             override val ref: Narrow get() = Narrow()\n\
         }\n\
         class RetainedChild : Provider {\n\
             override val ref: Narrow get() = super<Provider>.ref\n\
         }\n",
    );
    let edited_index = edited.index();
    let provider = edited_index
        .declarations_named("Provider")
        .find(|declaration| declaration.kind == DeclarationKind::Interface)
        .unwrap();
    let retained = edited_index
        .declarations()
        .map(|declaration| workspace_symbol(&edited_index, declaration))
        .collect();
    assert!(
        edited_index
            .default_property_getter_conflict(
                provider,
                &retained,
                std::slice::from_ref(&edited.root)
            )
            .is_none()
    );

    // Exact same-package covariance is now proven safe and should not produce
    // advice after the getter conflict check was tightened.
    let exact = Fixture::new();
    exact.write(
        "types.kt",
        "package sample\n\
         interface Broad {}\n\
         class Narrow : Broad {}\n\
         interface BroadContract {\n\
             val ref: Broad\n\
         }\n\
         interface Provider : BroadContract {\n\
             override val ref: Narrow get() = Narrow()\n\
         }\n\
         class RetainedChild : Provider {}\n",
    );
    let exact_index = exact.index();
    let (exact_files, exact_plans) = plans_for(&exact_index);
    let exact_advice = analyze(
        &exact_files,
        &exact_plans,
        &exact_index,
        std::slice::from_ref(&exact.root),
        &[],
    );
    assert!(exact_advice.iter().all(|item| item.code != "U001"));
}

#[test]
fn u001_omits_equal_unknown_generic_and_nonretained_getter_shapes() {
    let fixture = Fixture::new();
    fixture.write(
        "same.kt",
        "package same\ninterface Wide {}\ninterface Contract {\n val ref: Wide\n}\ninterface Provider : Contract {\n override val ref: Wide get() = TODO()\n}\nclass Child : Provider {}\n",
    );
    fixture.write(
        "generic.kt",
        "package generic\ninterface Wide {}\nclass Narrow<T> : Wide {}\ninterface Contract {\n val ref: Wide\n}\ninterface Provider : Contract {\n override val ref: Narrow<String> get() = TODO()\n}\nclass Child : Provider {}\n",
    );
    fixture.write(
        "unknown.kt",
        "package unknown\ninterface Contract {\n val ref: MissingType\n}\ninterface Provider : Contract {\n override val ref get() = TODO()\n}\nclass Child : Provider {}\n",
    );
    fixture.write(
        "unretained.kt",
        "package unretained\ninterface Wide {}\nclass Narrow : Wide {}\ninterface Contract {\n val ref: Wide\n}\ninterface Provider : Contract {\n override val ref: Narrow get() = Narrow()\n}\nclass Child : Provider {}\n",
    );
    let index = fixture.index();
    let (files, mut plans) = plans_for(&index);
    let child = index
        .declarations_named("Child")
        .find(|declaration| declaration.package.as_deref() == Some("unretained"))
        .unwrap();
    for plan in plans.values_mut() {
        for decision in &mut plan.declarations {
            if decision.symbol_id == workspace_symbol(&index, child) {
                decision.final_owner = Some(BackendOwner::Java);
            }
        }
    }
    // `files` is also the final owner snapshot consumed by U002; U001 reads the
    // workspace plans above, so an unretained descendant is not recommended.
    let advice = analyze(
        &files,
        &plans,
        &index,
        std::slice::from_ref(&fixture.root),
        &[],
    );
    assert!(advice.iter().all(|item| item.code != "U001"), "{advice:#?}");
}

#[test]
fn u002_requires_exact_root_collision_site_and_render_calls_it_unverified() {
    const COLLISION: &str = "the delegating overloads its callers need collide after type erasure: generated signatures";
    let fixture = Fixture::new();
    let file = fixture.write(
        "collision.kt",
        "package sample\nclass A {}\nclass B {}\nclass Payload(val a: String, val left: List<A> = TODO(), val right: List<B> = TODO(), val tail: Boolean) {}\n",
    );
    let index = fixture.index();
    let (files, plans) = plans_for(&index);
    let line = 4;
    let advice = analyze(
        &files,
        &plans,
        &index,
        std::slice::from_ref(&fixture.root),
        &[site(
            RetentionKind::MiddleDefaultParameter,
            &file,
            line,
            vec![COLLISION.into()],
        )],
    );
    assert_eq!(advice.len(), 1, "{advice:#?}");
    assert_eq!(advice[0].code, "U002");
    assert_eq!(advice[0].line, line);
    assert!(
        advice[0]
            .suggested_change
            .contains("supply the omitted arguments explicitly")
    );
    let rendered = render(&advice, 10);
    assert!(rendered.contains("Suggestions require manual review"));
    assert!(rendered.contains("not a verified unlock count"));
    assert!(rendered.contains("replan and validate after editing"));

    for excluded in [
        site(
            RetentionKind::MiddleDefaultParameter,
            &file,
            line,
            vec!["it declares another constructor, whose signature a delegating overload could duplicate".into()],
        ),
        site(
            RetentionKind::MiddleDefaultParameter,
            &file,
            line,
            vec!["a caller's argument shape could not be read at consumer.kt:8".into()],
        ),
        site(RetentionKind::MiddleDefaultParameter, &file, line, vec![COLLISION.into()]),
    ] {
        let mut excluded = excluded;
        if excluded.params[0] == COLLISION {
            excluded.blockers.push("AnotherRoot".into());
        }
        let advice = analyze(&files, &plans, &index, std::slice::from_ref(&fixture.root), &[excluded]);
        assert!(advice.iter().all(|item| item.code != "U002"), "{advice:#?}");
    }

    let wrong_line = site(
        RetentionKind::MiddleDefaultParameter,
        &file,
        line + 1,
        vec![COLLISION.into()],
    );
    let advice = analyze(
        &files,
        &plans,
        &index,
        std::slice::from_ref(&fixture.root),
        &[wrong_line],
    );
    assert!(advice.iter().all(|item| item.code != "U002"), "{advice:#?}");
}

#[test]
fn cli_u002_reports_real_erased_signatures_and_exact_omitting_callers() {
    let fixture = Fixture::new();
    fixture.write(
        "types.kt",
        "package sample\nclass A {}\nclass B {}\nclass Payload(val first: String, val left: List<A> = emptyList(), val right: List<B> = emptyList(), val last: Boolean) {}\n",
    );
    fixture.write(
        "callers.kt",
        "package sample\ninline fun <reified T> omitLeft(): Payload = Payload(first = \"a\", right = emptyList(), last = true)\ninline fun <reified T> omitRight(): Payload = Payload(first = \"b\", left = emptyList(), last = false)\n",
    );
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args([
            "--root",
            fixture.root.to_str().unwrap(),
            "--in-place",
            "--lombok",
            "--advice",
        ])
        .arg(&fixture.root)
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "notlin failed:\n{stderr}");
    let report = stderr
        .split("notlin: targeted source changes that may unlock translation")
        .nth(1)
        .unwrap_or("");
    assert!(report.contains("U002"), "missing U002 report:\n{stderr}");
    assert!(
        report.contains("List<A>") && report.contains("List<B>"),
        "the report should show both colliding generic parameter types:\n{report}"
    );
    assert!(
        report.contains("first: String"),
        "the report should show kept constructor parameter names and types:\n{report}"
    );
    assert!(
        report.contains("callers.kt:2") && report.contains("callers.kt:3"),
        "the report should identify both real Kotlin call sites:\n{report}"
    );
    assert!(
        report.contains("left") && report.contains("right"),
        "the report should identify which parameter each caller omits:\n{report}"
    );
}

#[test]
fn advice_is_opt_in_and_does_not_change_migration_output() {
    fn run(fixture: &Fixture, with_advice: bool) -> (String, Vec<(String, String)>) {
        fixture.write(
            "types.kt",
            "package sample\nclass A {}\nclass B {}\nclass Payload(val first: String, val left: List<A> = emptyList(), val right: List<B> = emptyList(), val last: Boolean) {}\n",
        );
        fixture.write(
            "callers.kt",
            "package sample\ninline fun <reified T> omitLeft(): Payload = Payload(first = \"a\", right = emptyList(), last = true)\ninline fun <reified T> omitRight(): Payload = Payload(first = \"b\", left = emptyList(), last = false)\n",
        );
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"));
        command.args([
            "--root",
            fixture.root.to_str().unwrap(),
            "--in-place",
            "--lombok",
        ]);
        if with_advice {
            command.arg("--advice");
        }
        let output = command.arg(&fixture.root).output().expect("run notlin");
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "notlin failed:\n{stderr}");
        let mut migrated = Vec::new();
        for entry in fs::read_dir(&fixture.root).unwrap() {
            let path = entry.unwrap().path();
            if matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("kt" | "java")
            ) {
                migrated.push((
                    path.file_name().unwrap().to_string_lossy().into_owned(),
                    fs::read_to_string(path).unwrap(),
                ));
            }
        }
        migrated.sort_by(|left, right| left.0.cmp(&right.0));
        (stderr, migrated)
    }

    let default_fixture = Fixture::new();
    let advice_fixture = Fixture::new();
    let (default_stderr, default_migration) = run(&default_fixture, false);
    let (advice_stderr, advice_migration) = run(&advice_fixture, true);
    let heading = "notlin: targeted source changes that may unlock translation";
    let phase = "checking targeted source recommendations";
    assert!(!default_stderr.contains(heading), "{default_stderr}");
    assert!(!default_stderr.contains(phase), "{default_stderr}");
    assert!(advice_stderr.contains(heading), "{advice_stderr}");
    assert!(advice_stderr.contains(phase), "{advice_stderr}");
    assert_eq!(default_migration, advice_migration);
}

#[test]
fn verbose_cli_timestamps_logger_and_progress_output() {
    fn run(verbose: bool) -> String {
        let fixture = Fixture::new();
        fixture.write(
            "Sample.kt",
            "package sample\nclass Sample(val value: String)\n",
        );
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"));
        command.args(["--root", fixture.root.to_str().unwrap()]);
        if verbose {
            command.arg("-v");
        }
        let output = command.arg(&fixture.root).output().expect("run notlin");
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "notlin failed:\n{stderr}");
        stderr
    }

    fn timestamped_line(stderr: &str, needle: &str) -> bool {
        stderr.lines().any(|line| {
            if !line.contains(needle) {
                return false;
            }
            let Some(start) = line.find("[+") else {
                return false;
            };
            let line = &line[start..];
            let bytes = line.as_bytes();
            bytes.len() >= 13
                && bytes[2..4].iter().all(u8::is_ascii_digit)
                && bytes[4] == b':'
                && bytes[5..7].iter().all(u8::is_ascii_digit)
                && bytes[7] == b'.'
                && bytes[8..11].iter().all(u8::is_ascii_digit)
                && bytes[11] == b']'
        })
    }

    let verbose = run(true);
    assert!(
        timestamped_line(&verbose, "cli:"),
        "missing logger timestamp:\n{verbose}"
    );
    assert!(
        timestamped_line(&verbose, "scanning workspace"),
        "missing progress timestamp:\n{verbose}"
    );
    let quiet = run(false);
    assert!(
        !quiet.contains("[+"),
        "quiet output unexpectedly has timestamps:\n{quiet}"
    );
}

#[test]
fn impact_ranking_counts_only_exact_established_dependencies_and_terminates_cycles() {
    let fixture = Fixture::new();
    fixture.write(
        "alpha_types.kt",
        "package alphatypes\ninterface Broad {}\nclass Narrow : Broad {}\n",
    );
    fixture.write(
        "alpha.kt",
        "package alpha\nimport alphatypes.*\ninterface Contract {\n val ref: Broad\n}\ninterface Provider : Contract {\n override val ref: Narrow get() = Narrow()\n}\nclass Child : Provider {}\n",
    );
    fixture.write(
        "beta_types.kt",
        "package betatypes\ninterface Broad {}\nclass Narrow : Broad {}\n",
    );
    fixture.write(
        "beta.kt",
        "package beta\nimport betatypes.*\ninterface Contract {\n val ref: Broad\n}\ninterface Provider : Contract {\n override val ref: Narrow get() = Narrow()\n}\nclass Child : Provider {}\n",
    );
    let index = fixture.index();
    let (files, mut plans) = plans_for(&index);
    let provider_alpha = index
        .declarations_named("Provider")
        .find(|declaration| declaration.package.as_deref() == Some("alpha"))
        .unwrap();
    let provider_beta = index
        .declarations_named("Provider")
        .find(|declaration| declaration.package.as_deref() == Some("beta"))
        .unwrap();
    let child_alpha = index
        .declarations_named("Child")
        .find(|declaration| declaration.package.as_deref() == Some("alpha"))
        .unwrap();
    let child_beta = index
        .declarations_named("Child")
        .find(|declaration| declaration.package.as_deref() == Some("beta"))
        .unwrap();
    let provider_alpha = workspace_symbol(&index, provider_alpha);
    let provider_beta = workspace_symbol(&index, provider_beta);
    let child_alpha = workspace_symbol(&index, child_alpha);
    let child_beta = workspace_symbol(&index, child_beta);
    for plan in plans.values_mut() {
        plan.dependencies.clear();
    }
    let source_plan = plans.values_mut().next().unwrap();
    source_plan.dependencies.extend([
        SymbolDependency {
            from: child_alpha.clone(),
            spelling: "Provider".into(),
            resolution: FactStatus::Established(provider_alpha.clone()),
        },
        // A cycle must not make the traversal loop or inflate the count.
        SymbolDependency {
            from: provider_alpha.clone(),
            spelling: "Child".into(),
            resolution: FactStatus::Established(child_alpha.clone()),
        },
        // Same-spelled declarations in another package do not join alpha's
        // count through uncertain workspace facts.
        SymbolDependency {
            from: child_beta.clone(),
            spelling: "Provider".into(),
            resolution: FactStatus::Inferred(provider_alpha.clone()),
        },
        SymbolDependency {
            from: child_beta.clone(),
            spelling: "Provider".into(),
            resolution: FactStatus::Ambiguous(vec![provider_alpha.clone(), provider_beta.clone()]),
        },
    ]);
    let advice = analyze(
        &files,
        &plans,
        &index,
        std::slice::from_ref(&fixture.root),
        &[],
    );
    let alpha = advice
        .iter()
        .find(|item| item.code == "U001" && item.target.package == "alpha")
        .expect("alpha recommendation");
    let beta = advice
        .iter()
        .find(|item| item.code == "U001" && item.target.package == "beta")
        .expect("beta recommendation");
    assert_eq!(alpha.related_retained_declarations, 1);
    assert_eq!(beta.related_retained_declarations, 0);
    assert!(
        advice
            .iter()
            .position(|item| item.target == alpha.target)
            .unwrap()
            < advice
                .iter()
                .position(|item| item.target == beta.target)
                .unwrap()
    );
}
