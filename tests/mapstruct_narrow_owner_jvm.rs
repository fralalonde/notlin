use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "notlin-mapstruct-narrow-owner-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

const SOURCES: [(&str, &str); 11] = [
    (
        "Identifier.kt",
        r#"package mapping.narrow
interface Identifier { val code: String }
data class StoredId(override val code: String) : Identifier
"#,
    ),
    (
        "BroadOwner.kt",
        r#"package mapping.narrow
interface BroadOwner { val owner: Identifier }
"#,
    ),
    (
        "NarrowOwner.kt",
        r#"package mapping.narrow
interface NarrowOwner { val owner: StoredId }
"#,
    ),
    (
        "ChildInfo.kt",
        r#"package mapping.narrow
interface ChildInfo : NarrowOwner, BroadOwner {
    val detail: Detail
    override val owner: StoredId get() = detail.owner
}
"#,
    ),
    (
        "OwnerData.kt",
        r#"package mapping.narrow
interface OwnerData : BroadOwner, NarrowOwner
"#,
    ),
    (
        "Step.kt",
        r#"package mapping.narrow
interface Step : OwnerData, ChildInfo
"#,
    ),
    (
        "Output.kt",
        r#"package mapping.narrow
interface Output : BroadOwner
"#,
    ),
    (
        "Detail.kt",
        r#"package mapping.narrow
data class Detail(val owner: StoredId)
"#,
    ),
    (
        "InputRecord.kt",
        r#"package mapping.narrow
data class InputRecord(override val detail: Detail) : Step, Output
"#,
    ),
    (
        "MapStructSupport.java",
        r#"package mapping.narrow;
import org.mapstruct.Mapper;
import org.mapstruct.TargetType;
import org.mapstruct.factory.Mappers;
public final class MapStructSupport {
    public static final class OwnerEntity {
        private String code;
        public String getCode() { return code; }
        public void setCode(String code) { this.code = code; }
    }
    public static final class EntityWithOwner {
        private OwnerEntity owner;
        public OwnerEntity getOwner() { return owner; }
        public void setOwner(OwnerEntity owner) { this.owner = owner; }
    }
    public static final class OwnerLookup {
        private static int calls;
        private static OwnerEntity expected;
        public <T> T fromId(StoredId id, @TargetType Class<T> targetType) {
            calls++;
            return targetType.cast(expected);
        }
        static void reset(OwnerEntity value) { calls = 0; expected = value; }
        static int calls() { return calls; }
    }
    @Mapper(uses = OwnerLookup.class)
    public interface RecordMapper {
        EntityWithOwner map(InputRecord source);
    }
    public static String verify() {
        StoredId id = new StoredId("source-id");
        InputRecord source = new InputRecord(new Detail(id));
        OwnerEntity expected = new OwnerEntity();
        expected.setCode("canonical-owner");
        OwnerLookup.reset(expected);
        EntityWithOwner result = Mappers.getMapper(RecordMapper.class).map(source);
        return (result.getOwner() == expected) + "/" + OwnerLookup.calls() + "/" + result.getOwner().getCode();
    }
}
"#,
    ),
    (
        "Runner.kt",
        "package mapping.narrow\nfun main() {\n    suspend fun keepRunnerKotlin() {}\n    println(MapStructSupport.verify())\n}\n",
    ),
];

fn populate(root: &Path) {
    fs::create_dir_all(root).unwrap();
    for (name, source) in SOURCES {
        fs::write(root.join(name), source).unwrap();
    }
}

fn translate(root: &Path, lombok_enabled: bool) -> std::process::Output {
    let mut kotlin = SOURCES
        .iter()
        .filter(|(name, _)| name.ends_with(".kt"))
        .map(|(name, _)| root.join(name))
        .collect::<Vec<_>>();
    kotlin.sort();
    let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
    command
        .arg("--root")
        .arg(root)
        .args(["--in-place", "--annotations", "none"]);
    if lombok_enabled {
        command.arg("--lombok");
    }
    command.args(kotlin).output().expect("run notlin")
}

fn config(
    root: &Path,
    output: PathBuf,
    classpath: &[PathBuf],
    processor_path: &Path,
    lombok_enabled: bool,
) -> JvmValidationConfig {
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
    let mut processors = "org.mapstruct.ap.MappingProcessor".to_owned();
    let mut processor_paths = vec![processor_path.to_path_buf()];
    if lombok_enabled {
        processor_paths.push(PathBuf::from(
            std::env::var_os("NOTLIN_LOMBOK").expect("NOTLIN_LOMBOK"),
        ));
        processors.push_str(",lombok.launch.AnnotationProcessorHider$AnnotationProcessor");
        let mut plugin = std::ffi::OsString::from("-Xplugin=");
        plugin.push(
            std::env::var_os("NOTLIN_KOTLIN_LOMBOK_PLUGIN").expect("NOTLIN_KOTLIN_LOMBOK_PLUGIN"),
        );
        config.kotlinc_args.push(plugin);
    }
    config.javac_args.extend([
        "-processorpath".into(),
        std::env::join_paths(processor_paths).unwrap(),
    ]);
    config
        .javac_args
        .extend(["-processor".into(), processors.into()]);
    config.classpath.extend_from_slice(classpath);
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("kt") => config.kotlin_sources.push(path),
            Some("java") => config.java_sources.push(path),
            _ => {}
        }
    }
    config.run_main_class = Some("mapping.narrow.RunnerKt".into());
    config
}

#[test]
#[ignore = "requires JVM tools, Lombok, and MapStruct jars in NOTLIN_* environment variables"]
fn mapstruct_uses_the_narrowed_inherited_owner_accessor_in_both_lombok_modes() {
    let classpath = ["NOTLIN_KOTLIN_STDLIB", "NOTLIN_MAPSTRUCT", "NOTLIN_LOMBOK"]
        .map(|name| PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("{name}"))));
    let processor_path = PathBuf::from(
        std::env::var_os("NOTLIN_MAPSTRUCT_PROCESSOR").expect("NOTLIN_MAPSTRUCT_PROCESSOR"),
    );
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    populate(&original);
    let before = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-classes"),
        &classpath,
        &processor_path,
        false,
    ))
    .unwrap_or_else(|error| {
        panic!(
            "original Kotlin/MapStruct fixture failed under {}: {error:?}",
            scratch.0.display()
        )
    });
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .replace("\r\n", "\n");
    assert_eq!(expected, "true/1/canonical-owner");

    for lombok_enabled in [false, true] {
        let migrated = scratch.0.join(format!("migrated-{lombok_enabled}"));
        populate(&migrated);
        let translated = translate(&migrated, lombok_enabled);
        assert!(
            translated.status.success(),
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(migrated.join("InputRecord.java").is_file());
        let after = validate_jvm_sources(&config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{lombok_enabled}")),
            &classpath,
            &processor_path,
            lombok_enabled,
        ))
        .unwrap_or_else(|error| {
            panic!(
                "mixed Kotlin/Java MapStruct fixture failed under {}: {error:?}",
                scratch.0.display()
            )
        });
        assert_eq!(
            after
                .commands
                .last()
                .unwrap()
                .stdout
                .trim()
                .replace("\r\n", "\n"),
            expected,
            "Lombok mode {lombok_enabled} under {} must preserve the canonical narrowed owner mapping",
            scratch.0.display()
        );
    }
}
