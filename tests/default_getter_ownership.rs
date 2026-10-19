use clap::Parser;
use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
use notlin::semantics::{SymbolId, workspace_symbol};
use notlin::transpiler::fixpoint::{self, WorkspacePlan};
use notlin::workspace::{SourceIndex, SourceLanguage, SourceOverlay};
use notlin::{cli::Cli, migration_pipeline::plan_workspace_migration};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new(layout: Layout, provider_blocker: bool, child_blocker: bool) -> Self {
        Self::with_sources(
            &java_sources(),
            kotlin_sources(layout, provider_blocker, child_blocker),
        )
    }

    fn property_based(layout: Layout, provider_blocker: bool, child_blocker: bool) -> Self {
        Self::with_sources(
            &[],
            property_kotlin_sources(layout, provider_blocker, child_blocker),
        )
    }

    fn with_sources(
        java_sources: &[(&'static str, &'static str)],
        kotlin_sources: Vec<(&'static str, String)>,
    ) -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-temp")
            .join(format!(
                "default-getter-ownership-{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();

        for (name, source) in java_sources {
            fs::write(root.join(name), source).unwrap();
        }
        for (name, source) in kotlin_sources {
            fs::write(root.join(name), source).unwrap();
        }
        Self(root)
    }

    fn root(&self) -> &Path {
        &self.0
    }

    fn index(&self) -> SourceIndex {
        SourceIndex::discover(self.root()).unwrap()
    }

    fn selected_kotlin(&self, index: &SourceIndex) -> Vec<(PathBuf, String)> {
        let mut sources = index
            .kotlin_files()
            .map(|file| (file.path.clone(), file.source_text().to_owned()))
            .collect::<Vec<_>>();
        sources.sort_by(|left, right| left.0.cmp(&right.0));
        sources
    }

    fn cli(&self) -> Cli {
        Cli::parse_from([
            "notlin",
            "--in-place",
            self.root().to_str().expect("fixture path is UTF-8"),
        ])
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Copy, Debug)]
enum Layout {
    OneFile,
    SplitFiles,
}

fn java_sources() -> [(&'static str, &'static str); 5] {
    [
        (
            "RootKey.java",
            "package generic.events;\npublic interface RootKey {}\n",
        ),
        (
            "KeyAware.java",
            "package generic.events;\npublic interface KeyAware<K extends RootKey> { K getId(); }\n",
        ),
        (
            "BaseEntity.java",
            "package generic.events;\npublic interface BaseEntity<K extends RootKey> {}\n",
        ),
        (
            "BaseEvent.java",
            "package generic.events;\npublic interface BaseEvent<T extends BaseEntity<K>, K extends RootKey> extends KeyAware<K> { K getId(); }\n",
        ),
        (
            "BaseCreatedEvent.java",
            "package generic.events;\npublic interface BaseCreatedEvent<T extends BaseEntity<K>, K extends RootKey> extends BaseEvent<T, K> { T getPayload(); }\n",
        ),
    ]
}

fn kotlin_sources(
    layout: Layout,
    provider_blocker: bool,
    child_blocker: bool,
) -> Vec<(&'static str, String)> {
    let common = "package generic.events\n\n\
        class EntityKey : RootKey\n\
        interface EntityInfo {\n\
            fun getId(): EntityKey\n\
        }\n\
        interface DetailInfo : EntityInfo\n\
        class DetailInfoImpl : DetailInfo {\n\
            override fun getId(): EntityKey = EntityKey()\n\
        }\n\
        interface Entity : BaseEntity<EntityKey> {\n\
            fun getInfo(): DetailInfo\n\
            fun getId(): EntityKey\n\
        }\n\
        interface EntityAware {\n\
            fun getEntity(): EntityInfo\n\
        }\n\n";
    let generic_event = format!(
        "interface GenericEvent<T : Entity> : BaseEvent<T, EntityKey>, EntityAware {{\n\
            {}\
            {}\
        }}\n",
        if provider_blocker {
            "    suspend fun independentlyRetainedProvider() {}\n"
        } else {
            ""
        },
        "    override fun getId(): EntityKey = getEntity().getId()"
    );
    let created_event = "interface CreatedEvent<T : Entity> : BaseCreatedEvent<T, EntityKey>, GenericEvent<T> {\n\
        override fun getId(): EntityKey = getEntity().getId()\n\
    }\n\
    interface DetailAware {\n\
        fun getDetail(): DetailInfo\n\
    }\n\
    interface DetailEvent : GenericEvent<DetailEntity>, DetailAware {\n\
        override fun getEntity(): DetailInfo\n\
        override fun getDetail(): DetailInfo = getEntity()\n\
    }\n\
    interface DetailCreatedEvent : CreatedEvent<DetailEntity>, DetailEvent {\n\
        override fun getPayload(): DetailEntity\n\
        override fun getEntity(): DetailInfo = getPayload().getInfo()\n\
    }\n";
    let child_blocker = if child_blocker {
        "        suspend fun independentlyRetainedChild() {}\n"
    } else {
        ""
    };
    let implementation = format!(
        "class DetailEntity : Entity {{\n\
        override fun getInfo(): DetailInfo = DetailInfoImpl()\n\
        override fun getId(): EntityKey = EntityKey()\n\
    }}\n\
    data class CreatedPayload(@JvmField val payload: DetailEntity) : DetailCreatedEvent {{\n\
        override fun getPayload(): DetailEntity = payload\n\
        {child_blocker}\
    }}\n\
    class IndependentIntrinsic {{\n\
        suspend fun keepKotlin() {{}}\n\
    }}\n"
    );

    match layout {
        Layout::OneFile => vec![(
            "Events.kt",
            [common, &generic_event, created_event, &implementation].concat(),
        )],
        Layout::SplitFiles => vec![
            ("GenericEvents.kt", [common, &generic_event].concat()),
            (
                "CreatedEvents.kt",
                ["package generic.events\n\n", created_event].concat(),
            ),
            (
                "ConcreteEvents.kt",
                ["package generic.events\n\n", &implementation].concat(),
            ),
        ],
    }
}

fn property_kotlin_sources(
    layout: Layout,
    provider_blocker: bool,
    child_blocker: bool,
) -> Vec<(&'static str, String)> {
    let common = "package generic.events\n\n\
        interface RootKey\n\
        class EntityKey(val label: String) : RootKey\n\
        interface KeyAware<K : RootKey> {\n\
            val id: K\n\
        }\n\
        interface BaseEntity<K : RootKey> : KeyAware<K>\n\
        interface BaseEvent<T : BaseEntity<K>, K : RootKey> : KeyAware<K>\n\
        interface BaseCreatedEvent<T : BaseEntity<K>, K : RootKey> : BaseEvent<T, K> {\n\
            val payload: T\n\
            override val id\n\
                get() = payload.id\n\
        }\n\
        interface EntityInfo {\n\
            val id: EntityKey\n\
        }\n\
        interface DetailInfo : EntityInfo\n\
        class DetailInfoImpl : DetailInfo {\n\
            override val id: EntityKey = EntityKey(\"info-id\")\n\
        }\n\
        interface Entity : BaseEntity<EntityKey> {\n\
            fun getInfo(): DetailInfo\n\
        }\n\
        interface EntityAware {\n\
            val entity: EntityInfo\n\
        }\n\n";
    let generic_event = format!(
        "interface GenericEvent<T : Entity> : BaseEvent<T, EntityKey>, EntityAware {{\n\
            {}\
            override val id: EntityKey\n\
                get() = entity.id\n\
        }}\n",
        if provider_blocker {
            "    suspend fun independentlyRetainedProvider() {}\n"
        } else {
            ""
        }
    );
    let event_branches = "interface CreatedEvent<T : Entity> : BaseCreatedEvent<T, EntityKey>, GenericEvent<T> {\n\
        override val id: EntityKey\n\
            get() = entity.id\n\
    }\n\
    interface DetailAware {\n\
        val detail: DetailInfo\n\
    }\n\
    interface DetailEvent : GenericEvent<DetailEntity>, DetailAware {\n\
        override val entity: DetailInfo\n\
        override val detail: DetailInfo\n\
            get() = entity\n\
    }\n\
    interface DetailCreatedEvent : CreatedEvent<DetailEntity>, DetailEvent {\n\
        override val payload: DetailEntity\n\
        override val entity: DetailInfo\n\
            get() = payload.getInfo()\n\
    }\n";
    let child_blocker = if child_blocker {
        "        suspend fun independentlyRetainedChild() {}\n"
    } else {
        ""
    };
    let concrete = format!(
        "class DetailEntity : Entity {{\n\
        override val id: EntityKey = EntityKey(\"payload-id\")\n\
        override fun getInfo(): DetailInfo = DetailInfoImpl()\n\
    }}\n\
    data class CreatedPayload(override val payload: DetailEntity) : DetailCreatedEvent {{\n\
        {child_blocker}\
    }}\n\
    class IndependentIntrinsic {{\n\
        suspend fun keepKotlin() {{}}\n\
    }}\n"
    );

    match layout {
        Layout::OneFile => vec![(
            "PropertyEvents.kt",
            [common, &generic_event, event_branches, &concrete].concat(),
        )],
        Layout::SplitFiles => vec![
            (
                "PropertyGenericEvents.kt",
                [common, &generic_event].concat(),
            ),
            (
                "PropertyCreatedEvents.kt",
                ["package generic.events\n\n", event_branches].concat(),
            ),
            (
                "PropertyConcreteEvents.kt",
                ["package generic.events\n\n", &concrete].concat(),
            ),
        ],
    }
}

fn symbol(index: &SourceIndex, name: &str) -> SymbolId {
    let mut declarations = index.declarations_named(name);
    let declaration = declarations
        .next()
        .unwrap_or_else(|| panic!("missing {name}"));
    assert!(
        declarations.next().is_none(),
        "ambiguous fixture symbol {name}"
    );
    workspace_symbol(index, declaration)
}

fn plan_with_sources(
    fixture: &Fixture,
    index: &SourceIndex,
    sources: &[(PathBuf, String)],
    seed: &HashSet<SymbolId>,
) -> WorkspacePlan {
    let roots = [fixture.root().to_path_buf()];
    let cli = fixture.cli();
    if seed.is_empty() {
        fixpoint::plan_workspace_state(sources, &cli, index, &roots, 32, false).unwrap()
    } else {
        fixpoint::plan_workspace_warm(sources, &cli, index, &roots, 32, false, seed).unwrap()
    }
}

fn plan(fixture: &Fixture, index: &SourceIndex, seed: &HashSet<SymbolId>) -> WorkspacePlan {
    let sources = fixture.selected_kotlin(index);
    plan_with_sources(fixture, index, &sources, seed)
}

fn is_retained(plan: &WorkspacePlan, symbol: &SymbolId) -> bool {
    plan.retained.contains(symbol)
}

fn has_suspend_blocker(plan: &WorkspacePlan, symbol: &SymbolId) -> bool {
    plan.plans
        .iter()
        .flat_map(|file| &file.translation.declarations)
        .find(|decision| decision.symbol_id == *symbol)
        .is_some_and(|decision| {
            decision.retention_reasons.iter().any(|reason| {
                matches!(
                    reason,
                    notlin::translation_plan::RetentionReason::SuspendConstruct { .. }
                )
            })
        })
}

fn event_retention_reasons(
    plan: &WorkspacePlan,
    symbols: &[SymbolId],
) -> Vec<(String, Vec<notlin::translation_plan::RetentionReason>)> {
    symbols
        .iter()
        .filter_map(|symbol| {
            plan.plans
                .iter()
                .flat_map(|file| &file.translation.declarations)
                .find(|decision| decision.symbol_id == *symbol)
                .filter(|decision| {
                    decision.final_owner == Some(notlin::translation_plan::BackendOwner::Kotlin)
                })
                .map(|decision| (symbol.name.clone(), decision.retention_reasons.clone()))
        })
        .collect()
}

#[test]
fn generic_event_getter_ownership_is_stable_across_fresh_and_warm_planning() {
    for layout in [Layout::OneFile, Layout::SplitFiles] {
        let fixture = Fixture::new(layout, false, true);
        let old_index = fixture.index();
        let child = symbol(&old_index, "CreatedPayload");
        let old_state = plan(&fixture, &old_index, &HashSet::new());
        assert!(
            old_state.roots.contains(&child),
            "fixture must yield a prior intrinsic child root before its blocker is removed"
        );

        let child_file = old_index.source_file(&child.file).unwrap();
        let updated_source = child_file
            .source_text()
            .replace("        suspend fun independentlyRetainedChild() {}\n", "");
        let new_index = old_index
            .with_overlays(&[SourceOverlay::Replace {
                path: child_file.path.clone(),
                language: SourceLanguage::Kotlin,
                source: updated_source,
            }])
            .unwrap();
        let updated_sources = fixture.selected_kotlin(&new_index);
        let fresh = plan_with_sources(&fixture, &new_index, &updated_sources, &HashSet::new());
        let warm = plan_with_sources(&fixture, &new_index, &updated_sources, &old_state.roots);

        let provider = symbol(&new_index, "GenericEvent");
        let created = symbol(&new_index, "CreatedEvent");
        let detail = symbol(&new_index, "DetailEvent");
        let created_detail = symbol(&new_index, "DetailCreatedEvent");
        let implementation = symbol(&new_index, "CreatedPayload");
        let unrelated = symbol(&new_index, "IndependentIntrinsic");
        assert_eq!(
            warm.retained, fresh.retained,
            "a prior getter-only provider/descendant state must clear when the descendant's former blocker is gone; layout={layout:?}"
        );
        assert!(!is_retained(&fresh, &provider));
        assert!(!is_retained(&fresh, &created));
        assert!(!is_retained(&fresh, &detail));
        assert!(!is_retained(&fresh, &created_detail));
        assert!(!is_retained(&fresh, &implementation));
        assert!(is_retained(&fresh, &unrelated));
    }
}

#[test]
fn generic_event_migration_emits_the_concrete_getter_without_releasing_hard_blockers() {
    for layout in [Layout::OneFile, Layout::SplitFiles] {
        let fixture = Fixture::new(layout, false, false);
        let index = fixture.index();
        let sources = fixture.selected_kotlin(&index);
        let roots = [fixture.root().to_path_buf()];
        let cli = fixture.cli();
        let migration = plan_workspace_migration(sources, &cli, &index, &roots, |_| {}).unwrap();
        let child = migration
            .generated_java
            .iter()
            .find(|output| output.name == "CreatedPayload.java")
            .unwrap_or_else(|| {
                panic!(
                    "generic event implementation retained: {:#?}",
                    migration.translation_plans
                )
            });
        assert!(
            child.source.contains("EntityKey getId()"),
            "generated child must retain the concrete generic getter contract:\n{}",
            child.source
        );
        assert!(
            migration
                .final_kotlin_sources
                .values()
                .any(|source| source.contains("class IndependentIntrinsic")),
            "the unrelated suspended declaration remains Kotlin"
        );
        assert!(migration.rounds >= 1);
    }
}

#[test]
fn genuine_provider_blocker_keeps_generic_child_retained() {
    let fixture = Fixture::new(Layout::SplitFiles, true, false);
    let index = fixture.index();
    let provider = symbol(&index, "GenericEvent");
    let implementation = symbol(&index, "CreatedPayload");
    let fresh = plan(&fixture, &index, &HashSet::new());

    assert!(is_retained(&fresh, &provider));
    assert!(is_retained(&fresh, &implementation));
}

fn copy_kotlin_java_sources(source_root: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source_root).unwrap() {
        let path = entry.unwrap().path();
        if path
            .extension()
            .is_some_and(|extension| extension == "kt" || extension == "java")
        {
            fs::copy(&path, destination.join(path.file_name().unwrap())).unwrap();
        }
    }
}

fn property_jvm_config(root: &Path, output: PathBuf, lombok_enabled: bool) -> JvmValidationConfig {
    let mut config = JvmValidationConfig::new(
        std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA"),
        output,
    );
    config
        .kotlinc_args
        .extend(["-jvm-target".into(), "17".into()]);
    config.javac_args.extend(["--release".into(), "17".into()]);
    config.classpath.push(PathBuf::from(
        std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"),
    ));
    if lombok_enabled {
        let lombok = PathBuf::from(std::env::var_os("NOTLIN_LOMBOK").expect("NOTLIN_LOMBOK"));
        config.classpath.push(lombok.clone());
        config.javac_args.extend([
            "-processorpath".into(),
            lombok.into_os_string(),
            "-processor".into(),
            "lombok.launch.AnnotationProcessorHider$AnnotationProcessor".into(),
        ]);
        config.kotlinc_args.push(
            format!(
                "-Xplugin={}",
                PathBuf::from(
                    std::env::var_os("NOTLIN_KOTLIN_LOMBOK_PLUGIN")
                        .expect("NOTLIN_KOTLIN_LOMBOK_PLUGIN"),
                )
                .display()
            )
            .into(),
        );
    } else {
        config.javac_args.push("-proc:none".into());
    }
    let mut sources = fs::read_dir(root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "kt" || extension == "java")
        })
        .collect::<Vec<_>>();
    sources.sort();
    for path in sources {
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("kt") => config.kotlin_sources.push(path),
            Some("java") => config.java_sources.push(path),
            _ => unreachable!(),
        }
    }
    config.run_main_class = Some("generic.events.RunnerKt".into());
    config
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin in NOTLIN_* environment variables"]
fn property_event_getter_bridge_preserves_runtime_dispatch_in_both_lombok_modes() {
    let expected = "info-id|info-id|info-id|info-id";
    for layout in [Layout::OneFile, Layout::SplitFiles] {
        let fixture = Fixture::property_based(layout, false, false);
        fs::write(
            fixture.root().join("Runner.kt"),
            r#"package generic.events
fun main() {
    suspend fun keepRunnerKotlin() {}
    val event = CreatedPayload(DetailEntity())
    val keyAware: KeyAware<EntityKey> = event
    val entityInfo: EntityInfo = event.entity
    println("${event.id.label}|${keyAware.id.label}|${event.entity.id.label}|${entityInfo.id.label}")
}
"#,
        )
        .unwrap();

        let before = validate_jvm_sources(&property_jvm_config(
            fixture.root(),
            fixture.root().join("original-classes"),
            false,
        ))
        .expect("original property-based event fixture compiles and runs");
        assert_eq!(
            before.commands.last().unwrap().stdout.trim(),
            expected,
            "original Kotlin behavior must agree across concrete and interface receivers; layout={layout:?}"
        );

        for lombok_enabled in [false, true] {
            let migrated = fixture.root().join(format!("migrated-{lombok_enabled}"));
            copy_kotlin_java_sources(fixture.root(), &migrated);
            let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"));
            command
                .args(["--root"])
                .arg(&migrated)
                .args(["--annotations", "none", "--in-place"]);
            if lombok_enabled {
                command.arg("--lombok");
            }
            let translated = command.arg(&migrated).output().expect("run notlin");
            assert!(
                translated.status.success(),
                "Lombok mode {lombok_enabled}, layout {layout:?}: {}",
                String::from_utf8_lossy(&translated.stderr)
            );
            assert!(migrated.join("Runner.kt").is_file());
            let child_java = fs::read_to_string(migrated.join("CreatedPayload.java"))
                .unwrap_or_else(|_| {
                    panic!("CreatedPayload.java was not emitted; layout={layout:?}")
                });
            assert!(
                child_java.contains("EntityKey getId()"),
                "the generated class needs its explicit narrow default getter bridge:\n{child_java}"
            );
            let after = validate_jvm_sources(&property_jvm_config(
                &migrated,
                fixture
                    .root()
                    .join(format!("migrated-classes-{lombok_enabled}")),
                lombok_enabled,
            ))
            .expect("translated mixed property event fixture compiles and runs");
            assert_eq!(
                after.commands.last().unwrap().stdout.trim(),
                expected,
                "migrated behavior must preserve dispatch; Lombok={lombok_enabled}, layout={layout:?}"
            );
        }
    }
}

#[test]
fn property_event_getter_ownership_is_stable_after_a_former_intrinsic_root_is_removed() {
    for layout in [Layout::OneFile, Layout::SplitFiles] {
        let fixture = Fixture::property_based(layout, false, true);
        let old_index = fixture.index();
        let child = symbol(&old_index, "CreatedPayload");
        let old_state = plan(&fixture, &old_index, &HashSet::new());
        assert!(
            has_suspend_blocker(&old_state, &child),
            "the prior child must have an independent suspend blocker"
        );
        assert!(old_state.roots.contains(&child));

        let child_file = old_index.source_file(&child.file).unwrap();
        let updated_source = child_file
            .source_text()
            .replace("        suspend fun independentlyRetainedChild() {}\n", "");
        let new_index = old_index
            .with_overlays(&[SourceOverlay::Replace {
                path: child_file.path.clone(),
                language: SourceLanguage::Kotlin,
                source: updated_source,
            }])
            .unwrap();
        let updated_sources = fixture.selected_kotlin(&new_index);
        let fresh = plan_with_sources(&fixture, &new_index, &updated_sources, &HashSet::new());
        let warm = plan_with_sources(&fixture, &new_index, &updated_sources, &old_state.roots);

        let event_symbols = [
            symbol(&new_index, "GenericEvent"),
            symbol(&new_index, "CreatedEvent"),
            symbol(&new_index, "DetailEvent"),
            symbol(&new_index, "DetailCreatedEvent"),
            symbol(&new_index, "CreatedPayload"),
        ];
        assert_eq!(
            warm.retained,
            fresh.retained,
            "a former child suspend root must not keep a property-only getter cycle warm; layout={layout:?}; warm event reasons={:#?}; fresh event reasons={:#?}",
            event_retention_reasons(&warm, &event_symbols),
            event_retention_reasons(&fresh, &event_symbols)
        );
        for event in &event_symbols {
            assert!(
                !fresh.roots.contains(event),
                "unexpected intrinsic root: {event:?}"
            );
            assert!(
                !is_retained(&fresh, event),
                "getter-only event retained: {event:?}"
            );
        }
        assert!(is_retained(
            &fresh,
            &symbol(&new_index, "IndependentIntrinsic")
        ));
    }
}

#[test]
fn property_event_getter_ownership_is_stable_after_a_former_provider_root_is_removed() {
    for layout in [Layout::OneFile, Layout::SplitFiles] {
        let fixture = Fixture::property_based(layout, true, false);
        let old_index = fixture.index();
        let provider = symbol(&old_index, "GenericEvent");
        let old_state = plan(&fixture, &old_index, &HashSet::new());
        assert!(
            has_suspend_blocker(&old_state, &provider),
            "the prior provider must have an independent suspend blocker"
        );
        assert!(old_state.roots.contains(&provider));

        let provider_file = old_index.source_file(&provider.file).unwrap();
        let updated_source = provider_file
            .source_text()
            .replace("    suspend fun independentlyRetainedProvider() {}\n", "");
        let new_index = old_index
            .with_overlays(&[SourceOverlay::Replace {
                path: provider_file.path.clone(),
                language: SourceLanguage::Kotlin,
                source: updated_source,
            }])
            .unwrap();
        let updated_sources = fixture.selected_kotlin(&new_index);
        let fresh = plan_with_sources(&fixture, &new_index, &updated_sources, &HashSet::new());
        let warm = plan_with_sources(&fixture, &new_index, &updated_sources, &old_state.roots);

        let event_symbols = [
            symbol(&new_index, "BaseCreatedEvent"),
            symbol(&new_index, "GenericEvent"),
            symbol(&new_index, "CreatedEvent"),
            symbol(&new_index, "DetailEvent"),
            symbol(&new_index, "DetailCreatedEvent"),
            symbol(&new_index, "CreatedPayload"),
        ];
        assert_eq!(
            warm.retained,
            fresh.retained,
            "a former provider suspend root must not keep a property-only getter cycle warm; layout={layout:?}; warm event reasons={:#?}; fresh event reasons={:#?}",
            event_retention_reasons(&warm, &event_symbols),
            event_retention_reasons(&fresh, &event_symbols)
        );
        for event in &event_symbols {
            assert!(
                !fresh.roots.contains(event),
                "unexpected intrinsic root: {event:?}"
            );
            assert!(
                !is_retained(&fresh, event),
                "getter-only event retained: {event:?}"
            );
        }
        assert!(is_retained(
            &fresh,
            &symbol(&new_index, "IndependentIntrinsic")
        ));
    }
}

#[test]
fn property_event_getter_dependencies_do_not_hide_real_suspend_blockers() {
    for layout in [Layout::OneFile, Layout::SplitFiles] {
        for (provider_blocker, child_blocker, blocked_name) in [
            (true, false, "GenericEvent"),
            (false, true, "CreatedPayload"),
        ] {
            let fixture = Fixture::property_based(layout, provider_blocker, child_blocker);
            let index = fixture.index();
            let provider = symbol(&index, "GenericEvent");
            let implementation = symbol(&index, "CreatedPayload");
            let current = plan(&fixture, &index, &HashSet::new());
            let blocked = if blocked_name == "GenericEvent" {
                &provider
            } else {
                &implementation
            };

            assert!(
                has_suspend_blocker(&current, blocked),
                "the selected owner must carry a direct suspend blocker: {blocked_name}"
            );
            assert!(is_retained(&current, blocked));
        }
    }
}
