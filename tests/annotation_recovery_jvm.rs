//! Explicit native gate: preserves original-compiled callers, not just caller source.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Workspace(PathBuf);
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run(command: &mut Command) -> Output {
    let output = command.output().expect("execute required native tool");
    assert!(
        output.status.success(),
        "{command:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn required(name: &str) -> PathBuf {
    let path = PathBuf::from(std::env::var_os(name).expect(name));
    assert!(path.is_file(), "required native input {name}: {path:?}");
    path
}

fn classpath(paths: &[&Path]) -> std::ffi::OsString {
    std::env::join_paths(paths).expect("native classpath")
}

#[test]
#[ignore = "requires NOTLIN_JAVA, NOTLIN_JAVAC, NOTLIN_KOTLINC, NOTLIN_KOTLIN_STDLIB"]
fn bodyless_annotation_recovery_preserves_original_compiled_callers_and_abi() {
    let java = required("NOTLIN_JAVA");
    let javac = required("NOTLIN_JAVAC");
    let kotlinc = required("NOTLIN_KOTLINC");
    let stdlib = required("NOTLIN_KOTLIN_STDLIB");
    let reflect = stdlib.parent().unwrap().join("kotlin-reflect.jar");
    assert!(reflect.is_file(), "kotlin-reflect.jar is required");
    let root =
        std::env::temp_dir().join(format!("notlin-annotation-native-{}", std::process::id()));
    assert!(
        !root.exists(),
        "do not overwrite an existing native workspace"
    );
    fs::create_dir_all(&root).unwrap();
    let _cleanup = Workspace(root.clone());
    let input = root.join("input");
    let original = root.join("original");
    let emitted = root.join("emitted");
    let callers = root.join("callers");
    for path in [&input, &original, &emitted, &callers] {
        fs::create_dir_all(path).unwrap();
    }
    let fixtures = [
        (
            "Meta.kt",
            "package probe\n@Target(AnnotationTarget.CLASS, AnnotationTarget.ANNOTATION_CLASS)\nannotation class Meta\n",
        ),
        (
            "Marker.kt",
            "package probe\n@Meta\n@Target(AnnotationTarget.CLASS)\nannotation class Marker\n",
        ),
        ("Holder.kt", "package probe\n@Marker\nclass Holder\n"),
    ];
    for (name, source) in fixtures {
        fs::write(input.join(name), source).unwrap();
    }
    let caller = root.join("Caller.kt");
    fs::write(
        &caller,
        r#"package probe
import kotlin.reflect.full.primaryConstructor
fun main() {
    check(Marker() is Marker)
    check(Marker::class.primaryConstructor!!.call().annotationClass == Marker::class)
    check(Holder::class.java.isAnnotationPresent(Marker::class.java))
    check(Marker::class.java.isAnnotationPresent(Meta::class.java))
    println("ok")
}
"#,
    )
    .unwrap();
    let java_home = java.parent().unwrap().parent().unwrap();
    run(Command::new(&kotlinc)
        .env("JAVA_HOME", java_home)
        .arg("-cp")
        .arg(classpath(&[&stdlib, &reflect]))
        .arg("-d")
        .arg(&original)
        .args(fixtures.iter().map(|(name, _)| input.join(name)))
        .arg(&caller));
    let driver = root.join("Driver.java");
    fs::write(
        &driver,
        r#"package probe;
import java.lang.annotation.*;
import java.nio.file.Path;
import java.util.Arrays;
public final class Driver {
    static void require(boolean value) { if (!value) throw new AssertionError(); }
    public static void main(String[] args) throws Exception {
        for (Class<?> type : new Class<?>[] {Meta.class, Marker.class}) {
            require(type.getModifiers() == 0x2601);
            require(type.isAnnotation());
            require(Arrays.equals(type.getInterfaces(), new Class<?>[] {Annotation.class}));
            require(type.getDeclaredConstructors().length == 0);
            require(type.getDeclaredMethods().length == 0);
            require(type.getDeclaredFields().length == 0);
            require(type.getAnnotation(Retention.class).value() == RetentionPolicy.RUNTIME);
            Path origin = Path.of(type.getProtectionDomain().getCodeSource().getLocation().toURI());
            require(origin.equals(Path.of(args[0])));
        }
        require(Arrays.equals(Meta.class.getAnnotation(Target.class).value(),
            new ElementType[] {ElementType.TYPE, ElementType.ANNOTATION_TYPE}));
        require(Arrays.equals(Marker.class.getAnnotation(Target.class).value(),
            new ElementType[] {ElementType.TYPE}));
        require(Marker.class.isAnnotationPresent(Meta.class));
        require(Holder.class.isAnnotationPresent(Marker.class));
        System.out.println("ok");
    }
}
"#,
    )
    .unwrap();
    run(Command::new(&javac)
        .arg("-cp")
        .arg(&original)
        .arg("-d")
        .arg(&callers)
        .arg(&driver));
    // Execute the original first; these caller classes are never recompiled below.
    for first in [&original, &emitted] {
        if first == &emitted {
            let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
            for (key, _) in
                std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("NOTLIN_"))
            {
                command.env_remove(key);
            }
            run(command
                .current_dir(&input)
                .args(["--annotations", "none", "--in-place", "."]));
            let files: Vec<_> = ["Meta", "Marker", "Holder"]
                .iter()
                .map(|name| input.join(format!("{name}.java")))
                .collect();
            assert!(
                files.iter().all(|path| path.is_file()),
                "all candidate owners must exist"
            );
            run(Command::new(&javac)
                .arg("-cp")
                .arg("")
                .arg("-d")
                .arg(&emitted)
                .args(files));
            for name in ["Meta", "Marker", "Holder"] {
                assert!(
                    emitted.join(format!("probe/{name}.class")).is_file(),
                    "no original-class fallback"
                );
            }
        }
        let cp = classpath(&[first, &original, &callers, &stdlib, &reflect]);
        let kotlin = run(Command::new(&java)
            .arg("-cp")
            .arg(&cp)
            .arg("probe.CallerKt"));
        let jvm = run(Command::new(&java)
            .arg("-cp")
            .arg(cp)
            .arg("probe.Driver")
            .arg(first));
        assert_eq!(String::from_utf8_lossy(&kotlin.stdout).trim(), "ok");
        assert_eq!(String::from_utf8_lossy(&jvm.stdout).trim(), "ok");
    }
}
